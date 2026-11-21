use std::path::PathBuf;

use crate::events::{transpose, Buckets, CHUNK, PERSON_BITS, PERSON_BUCKETS, PREFETCH_AHEAD};
use crate::ids::Key;
use crate::lookups::{Campaigns, Window};
use crate::people::People;
use crate::timestamp::{self, WEEK};

/// A valid click, keyed by its person.
#[derive(Clone, Copy)]
pub struct Click {
    pub person: Key,
    pub id: Key,
    pub ts: i64,
    pub campaign: u16,
}

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
        ts,
        campaign,
    })
}

/// Reads every click file and returns the valid clicks bucketed by person.
/// Copies of a click have identical values, so they need no dedup: a repeated
/// click credits and assists exactly as one copy does.
pub fn collect(paths: &[PathBuf], threads: usize, ctx: &Context) -> std::io::Result<Buckets<Click>> {
    let bytes = crate::scan::total_size(paths)? as usize;
    let per_thread = bytes / 45 / threads + 16;
    let states = crate::scan::run_fields::<4, _, _, _>(
        paths,
        threads,
        CHUNK,
        || Vec::<Click>::with_capacity(per_thread),
        |state, fields| {
            if let Some(click) = parse_click(fields, ctx) {
                state.push(click);
            }
        },
    )?;
    let people = ctx.people;
    let per_bucket = per_thread / PERSON_BUCKETS + 16;
    let resolved: Vec<Vec<Vec<Click>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = states
            .into_iter()
            .map(|clicks| {
                scope.spawn(move || {
                    let mut out: Vec<Vec<Click>> = (0..PERSON_BUCKETS).map(|_| Vec::with_capacity(per_bucket)).collect();
                    for (i, click) in clicks.iter().enumerate() {
                        if let Some(ahead) = clicks.get(i + PREFETCH_AHEAD) {
                            people.prefetch(ahead.person);
                        }
                        let mut click = *click;
                        click.person = people.name_of(click.person);
                        out[click.person.bucket(PERSON_BITS)].push(click);
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    Ok(transpose(resolved))
}
