use std::path::PathBuf;

use crate::events::{transpose, Buckets, CHUNK, PERSON_BITS, PERSON_BUCKETS, PREFETCH_AHEAD};
use crate::ids::Key;
use crate::lookups::{Campaigns, Window};
use crate::people::People;
use crate::timestamp::{self, WEEK};

/// A valid click, keyed by its person. `ts` is milliseconds from the window
/// start, negative for a click in the 7 days before it.
#[derive(Clone, Copy)]
pub struct Click {
    pub person: Key,
    pub id: Key,
    pub ts: i32,
    pub campaign: u16,
}

const _: () = assert!(std::mem::size_of::<Click>() == 24);

const EMPTY: Click = Click { person: Key(0), id: Key(0), ts: 0, campaign: 0 };

pub struct Context<'a> {
    pub window: Window,
    pub campaigns: &'a Campaigns,
    pub people: &'a People,
}

/// A valid click with its user ID in `person`; the person is resolved later.
fn parse_click(fields: &[&[u8]; 4], ctx: &Context) -> Option<Click> {
    if fields[0].is_empty() || fields[1].is_empty() {
        return None;
    }
    let ts = timestamp::parse(fields[3])?;
    if ts < ctx.window.start - WEEK || ts >= ctx.window.end {
        return None;
    }
    let campaign = *ctx.campaigns.map.get(&Key::from_bytes(fields[2]))?;
    Some(Click {
        person: Key::from_bytes(fields[1]),
        id: Key::from_bytes(fields[0]),
        ts: (ts - ctx.window.start) as i32,
        campaign,
    })
}

/// One scan thread's clicks by person bucket. A parsed click waits in `ring`
/// while its person table slot is prefetched, then takes its person's name
/// and goes straight into its bucket, so each click is stored once.
struct Filed {
    buckets: Vec<Vec<Click>>,
    ring: [Click; PREFETCH_AHEAD],
    parsed: usize,
}

impl Filed {
    fn new(per_bucket: usize) -> Filed {
        let mut buckets: Vec<Vec<Click>> = (0..PERSON_BUCKETS).map(|_| Vec::with_capacity(per_bucket)).collect();
        crate::prefault::vectors(&mut buckets);
        Filed { buckets, ring: [EMPTY; PREFETCH_AHEAD], parsed: 0 }
    }

    #[inline(always)]
    fn file(&mut self, mut click: Click, people: &People) {
        click.person = people.name_of(click.person);
        self.buckets[click.person.bucket(PERSON_BITS)].push(click);
    }

    /// Starts the person lookup of a parsed click and files the click parsed
    /// `PREFETCH_AHEAD` clicks before it.
    #[inline(always)]
    fn take(&mut self, click: Click, people: &People) {
        people.prefetch(click.person);
        let slot = self.parsed % PREFETCH_AHEAD;
        if self.parsed >= PREFETCH_AHEAD {
            let ready = self.ring[slot];
            self.file(ready, people);
        }
        self.ring[slot] = click;
        self.parsed += 1;
    }

    /// Files the clicks still waiting in the ring and returns the buckets.
    fn finish(mut self, people: &People) -> Vec<Vec<Click>> {
        for n in self.parsed.saturating_sub(PREFETCH_AHEAD)..self.parsed {
            let click = self.ring[n % PREFETCH_AHEAD];
            self.file(click, people);
        }
        self.buckets
    }
}

/// Reads every click file and returns the valid clicks bucketed by person.
/// Copies of a click have identical values, so they need no dedup: a repeated
/// click credits and assists exactly as one copy does.
pub fn collect(paths: &[PathBuf], threads: usize, ctx: &Context) -> std::io::Result<Buckets<Click>> {
    let bytes = crate::scan::total_size(paths)? as usize;
    let per_bucket = bytes / 45 / threads / PERSON_BUCKETS + 16;
    let people = ctx.people;
    let states = crate::scan::run_fields::<4, _, _, _>(
        paths,
        threads,
        CHUNK,
        || Filed::new(per_bucket),
        |state, fields| {
            if let Some(click) = parse_click(fields, ctx) {
                state.take(click, people);
            }
        },
    )?;
    Ok(transpose(states.into_iter().map(|state| state.finish(people)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashMap;

    type Filing = (Vec<u8>, Vec<u8>, i64);

    /// Writes a click file with `count` valid clicks and one with an unknown
    /// campaign, and returns each valid click as (click_id, name, stored ts).
    fn write_clicks(dir: &std::path::Path, file: usize, count: usize, window: Window) -> Vec<Filing> {
        let mut text = String::from("click_id,user_id,campaign_id,ts\n");
        let mut expected = Vec::new();
        for i in 0..count {
            let id = format!("k{file}x{i}");
            let user = i % 60;
            let ts = window.start - WEEK + i as i64 * 6_000_000 + file as i64;
            text.push_str(&format!("{id},b{user},c_1,{ts}\n"));
            let name = if user < 50 { format!("a{user}") } else { format!("b{user}") };
            expected.push((id.into_bytes(), name.into_bytes(), ts - window.start));
        }
        text.push_str(&format!("k{file}bad,b1,c_9,{}\n", window.start));
        std::fs::write(dir.join(format!("part-{file}.csv")), text).unwrap();
        expected
    }

    fn filed(paths: &[PathBuf], threads: usize, ctx: &Context) -> Vec<Filing> {
        let mut got = Vec::new();
        for (b, parts) in collect(paths, threads, ctx).unwrap().iter().enumerate() {
            for click in parts.iter().flatten() {
                assert_eq!(click.person.bucket(PERSON_BITS), b);
                assert_eq!(click.campaign, 1);
                got.push((click.id.to_vec(), click.person.to_vec(), i64::from(click.ts)));
            }
        }
        got.sort();
        got
    }

    #[test]
    fn records_have_their_packed_size() {
        assert_eq!(std::mem::size_of::<Click>(), 24);
    }

    #[test]
    fn every_click_is_filed_once_under_its_persons_name() {
        let dir = std::env::temp_dir().join(format!("clicks-filed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let window = Window { start: timestamp::parse(b"2026-10-01T00:00:00Z").unwrap(), end: timestamp::parse(b"2026-10-15T00:00:00Z").unwrap() };
        let mut map = HashMap::new();
        map.insert(Key::from_bytes(b"c_1"), 1);
        let campaigns = Campaigns { map, ids: vec![b"organic".to_vec(), b"c_1".to_vec()], channels: vec![b"organic".to_vec(), b"search".to_vec()] };
        let pairs = (0..50).map(|i| (Key::from_bytes(format!("b{i}").as_bytes()), Key::from_bytes(format!("a{i}").as_bytes())));
        let people = People::build(pairs, 50);
        let ctx = Context { window, campaigns: &campaigns, people: &people };
        let mut everything = Vec::new();
        for (file, count) in [0, 1, PREFETCH_AHEAD - 1, PREFETCH_AHEAD, PREFETCH_AHEAD + 1, 300].into_iter().enumerate() {
            let mut expected = write_clicks(&dir, file, count, window);
            expected.sort();
            assert_eq!(filed(&[dir.join(format!("part-{file}.csv"))], 1, &ctx), expected, "{count} clicks");
            everything.extend(expected);
        }
        everything.sort();
        assert!(everything.iter().any(|f| f.2 < 0) && everything.iter().any(|f| f.2 > 0));
        assert_eq!(filed(&crate::scan::csv_files(&dir).unwrap(), 3, &ctx), everything);
    }
}
