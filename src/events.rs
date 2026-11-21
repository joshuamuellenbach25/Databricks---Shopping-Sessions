use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use hashbrown::hash_map::Entry;
use hashbrown::HashMap;

use crate::ids::Key;
use crate::lookups::{Products, Window};
use crate::people::People;
use crate::timestamp::{self, DAY};

/// Buckets by event_id for dedup: small enough that each bucket's map stays in cache.
pub const EVENT_BITS: u32 = 10;
pub const EVENT_BUCKETS: usize = 1 << EVENT_BITS;
/// Buckets by person for sessions.
pub const PERSON_BITS: u32 = 8;
pub const PERSON_BUCKETS: usize = 1 << PERSON_BITS;
pub const CHUNK: usize = 2 << 20;
/// How many survivors ahead the person table slot is prefetched.
pub const PREFETCH_AHEAD: usize = 16;

/// One parsed line of events, before dedup.
#[derive(Clone, Copy)]
pub struct Raw {
    pub event: Key,
    pub user: Key,
    pub ingest: i64,
    pub ts: i64,
    pub price: u32,
    pub cat: u16,
    /// Passes every validation check and is not late.
    pub ok: bool,
}

/// A surviving event, keyed by its person.
#[derive(Clone, Copy)]
pub struct Event {
    pub person: Key,
    pub ts: i64,
    pub price: u32,
    pub cat: u16,
    pub purchase: bool,
}

pub struct Context<'a> {
    pub window: Window,
    pub products: &'a Products,
}

/// Raw records per scan thread, then per event bucket.
pub type Parsed = Vec<Vec<Vec<Raw>>>;

/// Surviving events per person bucket, in one part per dedup thread.
pub type Buckets<T> = Vec<Vec<Vec<T>>>;

fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u64::from(c - b'0');
        if value > 10_000_000 {
            return None;
        }
    }
    (value >= 1).then_some(value as u32)
}

fn parse_event(fields: &[&[u8]; 7], ctx: &Context) -> Option<Raw> {
    let event = fields[0];
    if event.is_empty() {
        return None;
    }
    let ingest = timestamp::parse(fields[3]).unwrap_or(i64::MAX);
    let user = fields[1];
    let product = fields[5];
    let purchase = fields[4] == b"purchase";
    let mut ok = !user.is_empty() && !product.is_empty() && (purchase || fields[4] == b"view" || fields[4] == b"cart");
    let ts = match timestamp::parse(fields[2]) {
        Some(t) if t >= ctx.window.start && t < ctx.window.end => {
            ok &= ingest.saturating_sub(t) <= DAY;
            t
        }
        _ => {
            ok = false;
            0
        }
    };
    let mut price = 0;
    let mut cat = ctx.products.unknown;
    if purchase && ok {
        match parse_price(fields[6]) {
            Some(p) => price = p,
            None => ok = false,
        }
        if let Some(&c) = ctx.products.map.get(&Key::from_bytes(product)) {
            cat = c;
        }
    }
    Some(Raw {
        event: Key::from_bytes(event),
        user: if ok { Key::from_bytes(user) } else { Key::from_bytes(b"") },
        ingest,
        ts,
        price,
        cat,
        ok,
    })
}

/// Reads every event file into raw records, bucketed by event_id.
pub fn parse(paths: &[PathBuf], threads: usize, ctx: &Context) -> std::io::Result<Parsed> {
    let bytes = crate::scan::total_size(paths)? as usize;
    let per_bucket = bytes / 70 / threads / EVENT_BUCKETS + 16;
    crate::scan::run_fields::<7, _, _, _>(
        paths,
        threads,
        CHUNK,
        || (0..EVENT_BUCKETS).map(|_| Vec::<Raw>::with_capacity(per_bucket)).collect::<Vec<_>>(),
        |state, fields| {
            if let Some(raw) = parse_event(fields, ctx) {
                state[raw.event.bucket(EVENT_BITS)].push(raw);
            }
        },
    )
}

/// Dedups on event_id (smallest ingest wins, before validation), keeps the
/// valid, not-late events and returns them bucketed by person.
pub fn dedup(parsed: Parsed, threads: usize, people: &People) -> Buckets<Event> {
    let by_bucket: Vec<Mutex<Vec<Vec<Raw>>>> = (0..EVENT_BUCKETS).map(|_| Mutex::new(Vec::with_capacity(parsed.len()))).collect();
    for state in parsed {
        for (b, part) in state.into_iter().enumerate() {
            by_bucket[b].lock().unwrap().push(part);
        }
    }
    let next = AtomicUsize::new(0);
    let outputs: Vec<Vec<Vec<Event>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out: Vec<Vec<Event>> = (0..PERSON_BUCKETS).map(|_| Vec::new()).collect();
                    let mut kept: HashMap<Key, (i64, u32, u32)> = HashMap::new();
                    let mut survivors: Vec<(u32, u32)> = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= EVENT_BUCKETS {
                            break;
                        }
                        let parts = std::mem::take(&mut *by_bucket[b].lock().unwrap());
                        let total: usize = parts.iter().map(Vec::len).sum();
                        kept.clear();
                        kept.reserve(total);
                        for (s, part) in parts.iter().enumerate() {
                            for (p, raw) in part.iter().enumerate() {
                                match kept.entry(raw.event) {
                                    Entry::Vacant(v) => {
                                        v.insert((raw.ingest, s as u32, p as u32));
                                    }
                                    Entry::Occupied(mut o) => {
                                        if raw.ingest < o.get().0 {
                                            o.insert((raw.ingest, s as u32, p as u32));
                                        }
                                    }
                                }
                            }
                        }
                        survivors.clear();
                        survivors.extend(kept.values().map(|&(_, s, p)| (s, p)).filter(|&(s, p)| parts[s as usize][p as usize].ok));
                        for (i, &(s, p)) in survivors.iter().enumerate() {
                            if let Some(&(s_ahead, p_ahead)) = survivors.get(i + PREFETCH_AHEAD) {
                                people.prefetch(parts[s_ahead as usize][p_ahead as usize].user);
                            }
                            let raw = &parts[s as usize][p as usize];
                            let person = people.name_of(raw.user);
                            out[person.bucket(PERSON_BITS)].push(Event {
                                person,
                                ts: raw.ts,
                                price: raw.price,
                                cat: raw.cat,
                                purchase: raw.price > 0,
                            });
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    transpose(outputs)
}

/// Turns per-thread lists of buckets into per-bucket lists of parts, moving
/// only the vectors, never their contents.
pub fn transpose<T>(per_thread: Vec<Vec<Vec<T>>>) -> Buckets<T> {
    let buckets = per_thread.first().map_or(0, Vec::len);
    let mut out: Buckets<T> = (0..buckets).map(|_| Vec::with_capacity(per_thread.len())).collect();
    for state in per_thread {
        for (b, part) in state.into_iter().enumerate() {
            if !part.is_empty() {
                out[b].push(part);
            }
        }
    }
    out
}
