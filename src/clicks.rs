use std::path::PathBuf;

use crate::dialect;
use crate::events::{BUCKETS, BUCKET_BITS, CHUNK};
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
        person: ctx.people.name_of(Key::from_bytes(fields[1])),
        id: Key::from_bytes(fields[0]),
        ts,
        campaign,
    })
}

/// Reads every click file and returns the valid clicks bucketed by person.
/// Copies of a click have identical values, so they need no dedup: a repeated
/// click credits and assists exactly as one copy does.
pub fn collect(paths: &[PathBuf], threads: usize, ctx: &Context) -> std::io::Result<Vec<Vec<Click>>> {
    let bytes = crate::scan::total_size(paths)? as usize;
    let per_bucket = bytes / 45 / threads / BUCKETS + 16;
    let states = crate::scan::run(
        paths,
        threads,
        CHUNK,
        || (0..BUCKETS).map(|_| Vec::<Click>::with_capacity(per_bucket)).collect::<Vec<_>>(),
        |state, line| {
            let mut fields: [&[u8]; 4] = [&[]; 4];
            if dialect::split(line, &mut fields) {
                if let Some(click) = parse_click(&fields, ctx) {
                    state[click.person.bucket(BUCKET_BITS)].push(click);
                }
            }
        },
    )?;
    let mut buckets: Vec<Vec<Click>> = (0..BUCKETS).map(|_| Vec::new()).collect();
    for state in states {
        for (b, part) in state.into_iter().enumerate() {
            if buckets[b].is_empty() {
                buckets[b] = part;
            } else {
                buckets[b].extend_from_slice(&part);
            }
        }
    }
    Ok(buckets)
}
