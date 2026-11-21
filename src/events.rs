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

/// Surviving events per person bucket, split into parts that together hold the bucket.
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
/// valid, not-late events and returns them bucketed by person. Each thread
/// writes a person bucket's events into chunks sized from the raw count: no
/// output vector is ever reallocated, and chunks are allocated as dedup
/// advances, while the processed raw records are being freed.
pub fn dedup(parsed: Parsed, threads: usize, people: &People) -> Buckets<Event> {
    /// Chunks each thread fills per person bucket when events spread evenly.
    const CHUNKS_PER_PART: usize = 8;
    /// Smallest chunk, in events.
    const MIN_CHUNK: usize = 64;
    let by_bucket: Vec<Mutex<Vec<Vec<Raw>>>> = (0..EVENT_BUCKETS).map(|_| Mutex::new(Vec::with_capacity(parsed.len()))).collect();
    let mut raw_total = 0;
    for state in parsed {
        for (b, part) in state.into_iter().enumerate() {
            raw_total += part.len();
            by_bucket[b].lock().unwrap().push(part);
        }
    }
    let chunk = (raw_total / threads.max(1) / PERSON_BUCKETS / CHUNKS_PER_PART).max(MIN_CHUNK);
    let next = AtomicUsize::new(0);
    let outputs: Vec<Vec<Vec<Vec<Event>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut open: Vec<Vec<Event>> = (0..PERSON_BUCKETS).map(|_| Vec::new()).collect();
                    let mut full: Vec<Vec<Vec<Event>>> = (0..PERSON_BUCKETS).map(|_| Vec::new()).collect();
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
                            let bucket = person.bucket(PERSON_BITS);
                            let current = &mut open[bucket];
                            if current.len() == current.capacity() {
                                let filled = std::mem::replace(current, Vec::with_capacity(chunk));
                                if !filled.is_empty() {
                                    full[bucket].push(filled);
                                }
                            }
                            current.push(Event {
                                person,
                                ts: raw.ts,
                                price: raw.price,
                                cat: raw.cat,
                                purchase: raw.price > 0,
                            });
                        }
                    }
                    for (bucket, current) in open.into_iter().enumerate() {
                        if !current.is_empty() {
                            full[bucket].push(current);
                        }
                    }
                    full
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut buckets: Buckets<Event> = (0..PERSON_BUCKETS).map(|_| Vec::new()).collect();
    for state in outputs {
        for (bucket, parts) in state.into_iter().enumerate() {
            buckets[bucket].extend(parts);
        }
    }
    buckets
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

#[cfg(test)]
mod dedup_tests {
    use super::*;

    fn key(text: &str) -> Key {
        Key::from_bytes(text.as_bytes())
    }

    /// Raw records with copies: later copies naming another user, earlier
    /// invalid copies that drop their event, later valid copies of invalid
    /// events, identical copies, and one user with many more events.
    fn raws(count: usize) -> Vec<Raw> {
        let mut out = Vec::new();
        let mut x: u64 = 0x5EED;
        for i in 0..count {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let ok = x % 50 != 0;
            let user = if (x >> 20) % 100 == 0 { key("whale") } else { key(&format!("u{}", (x >> 24) % 3000)) };
            let base = Raw {
                event: key(&format!("e{i}")),
                user: if ok { user } else { key("") },
                ingest: 1_000_000 + i as i64,
                ts: (x >> 32) as i64 % 1_000_000,
                price: if ok && x % 7 == 0 { 100 + (x >> 40) as u32 % 50_000 } else { 0 },
                cat: (x >> 50) as u16 % 20,
                ok,
            };
            out.push(base);
            match (x >> 8) % 20 {
                0 => out.push(Raw { ingest: base.ingest + 5, user: key("other"), ts: base.ts + 1, price: 0, ok: true, ..base }),
                1 => out.push(Raw { ingest: base.ingest - 5, user: key(""), price: 0, ok: false, ..base }),
                2 => out.push(base),
                _ => {}
            }
        }
        out
    }

    /// The records spread over scan parts and event buckets as `parse` leaves them.
    fn parsed(raws: &[Raw], scan_threads: usize) -> Parsed {
        let mut parsed: Parsed = (0..scan_threads).map(|_| (0..EVENT_BUCKETS).map(|_| Vec::new()).collect()).collect();
        for (i, raw) in raws.iter().enumerate() {
            parsed[i % scan_threads][raw.event.bucket(EVENT_BITS)].push(*raw);
        }
        parsed
    }

    type Row = (u64, i64, u32, u16, bool);

    /// Per person bucket, the sorted surviving events of a plain dedup.
    fn expected(raws: &[Raw], people: &People) -> Vec<Vec<Row>> {
        let mut kept: HashMap<Key, Raw> = HashMap::new();
        for raw in raws {
            let entry = kept.entry(raw.event).or_insert(*raw);
            if raw.ingest < entry.ingest {
                *entry = *raw;
            }
        }
        let mut out: Vec<Vec<Row>> = vec![Vec::new(); PERSON_BUCKETS];
        for raw in kept.values().filter(|raw| raw.ok) {
            let person = people.name_of(raw.user);
            out[person.bucket(PERSON_BITS)].push((person.0, raw.ts, raw.price, raw.cat, raw.price > 0));
        }
        for bucket in &mut out {
            bucket.sort_unstable();
        }
        out
    }

    #[test]
    fn keeps_the_earliest_copy_of_each_valid_event_in_its_person_bucket() {
        let pairs: Vec<(Key, Key)> = (0..600)
            .map(|i| (key(&format!("u{}", 2 * i)), key(&format!("u{}", 2 * i + 1))))
            .chain([(key("whale"), key("u7"))])
            .collect();
        let people = People::build(pairs.iter().copied(), pairs.len());
        let raws = raws(200_000);
        let want = expected(&raws, &people);
        assert!(want.iter().map(Vec::len).sum::<usize>() > 150_000);
        for threads in [1, 4] {
            let got = dedup(parsed(&raws, 3), threads, &people);
            assert_eq!(got.len(), PERSON_BUCKETS);
            let mut chunked = false;
            for (bucket, parts) in got.iter().enumerate() {
                assert!(parts.iter().all(|part| !part.is_empty()), "empty part in bucket {bucket}, threads {threads}");
                chunked |= parts.len() > threads;
                let mut events: Vec<Row> = parts.iter().flatten().map(|e| (e.person.0, e.ts, e.price, e.cat, e.purchase)).collect();
                events.sort_unstable();
                assert_eq!(events, want[bucket], "bucket {bucket}, threads {threads}");
            }
            assert!(chunked, "threads {threads}: no bucket filled more than one chunk per thread");
        }
    }

    #[test]
    fn no_events_give_every_person_bucket_empty() {
        let people = People::build(Vec::new(), 0);
        let got = dedup(Vec::new(), 2, &people);
        assert_eq!(got.len(), PERSON_BUCKETS);
        assert!(got.iter().all(Vec::is_empty));
    }
}
