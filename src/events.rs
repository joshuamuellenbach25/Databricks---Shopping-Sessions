use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use hashbrown::hash_map::Entry;
use hashbrown::HashMap;

use crate::ids::Key;
use crate::lookups::{Products, Window};
use crate::people::People;
use crate::timestamp::{self, DAY, WEEK};

pub const EVENT_BITS: u32 = 10;
pub const EVENT_BUCKETS: usize = 1 << EVENT_BITS;
pub const PERSON_BITS: u32 = 8;
pub const PERSON_BUCKETS: usize = 1 << PERSON_BITS;
pub const CHUNK: usize = 2 << 20;
pub const PREFETCH_AHEAD: usize = 16;

const NO_USER: Key = Key(0);

#[derive(Clone, Copy)]
#[repr(align(32))]
pub struct Raw {
    pub event: Key,
    pub ingest: i64,
    pub user: Key,
    pub ts: i32,
    pub sale: Sale,
}

impl Raw {
    #[inline]
    pub fn ok(&self) -> bool {
        self.user != NO_USER
    }
}

#[derive(Clone, Copy)]
pub struct Event {
    pub person: Key,
    pub ts: i32,
    pub sale: Sale,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sale(u32);

const PRICE_BITS: u32 = 24;
const PRICE_MASK: u32 = (1 << PRICE_BITS) - 1;
pub const MAX_CATEGORIES: usize = 1 << (32 - PRICE_BITS);

impl Sale {
    pub const NONE: Sale = Sale(0);

    #[inline]
    pub fn new(price: u32, cat: u16) -> Sale {
        debug_assert!((1..=PRICE_MASK).contains(&price) && usize::from(cat) < MAX_CATEGORIES);
        Sale(price | (u32::from(cat) << PRICE_BITS))
    }

    #[inline]
    pub fn is_purchase(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub fn price(self) -> u32 {
        self.0 & PRICE_MASK
    }

    #[inline]
    pub fn cat(self) -> u16 {
        (self.0 >> PRICE_BITS) as u16
    }

    #[inline]
    pub fn to_bits(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn from_bits(bits: u32) -> Sale {
        Sale(bits)
    }
}

const _: () = assert!(std::mem::size_of::<Raw>() == 32);
const _: () = assert!(std::mem::size_of::<Event>() == 16);

pub fn check_fits(window: Window, products: &Products) -> Result<(), String> {
    let span = window.end.saturating_sub(window.start);
    let most = i64::from(i32::MAX) - WEEK;
    if span > most {
        return Err(format!("manifest.json: the window spans {span} ms; times are stored as i32 milliseconds from window_start, which allows at most {most} ms"));
    }
    if products.categories.len() > MAX_CATEGORIES {
        return Err(format!("products.csv: {} categories counting unknown; an event record holds at most {MAX_CATEGORIES}", products.categories.len()));
    }
    Ok(())
}

pub struct Context<'a> {
    pub window: Window,
    pub products: &'a Products,
    dates: timestamp::Months,
}

impl<'a> Context<'a> {
    pub fn new(window: Window, products: &'a Products) -> Context<'a> {
        let first = window.start.div_euclid(DAY) - 1;
        let last = (window.end - 1).div_euclid(DAY) + 6;
        Context { window, products, dates: timestamp::Months::new(first, last) }
    }
}

#[inline(always)]
fn event_type(b: &[u8]) -> (bool, bool) {
    match b.len() {
        4 => ((b == b"view") | (b == b"cart"), false),
        8 => {
            let purchase = b == b"purchase";
            (purchase, purchase)
        }
        _ => (false, false),
    }
}

pub type Parsed = Vec<Vec<Vec<Raw>>>;

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

#[inline(always)]
fn parse_event(fields: &[&[u8]; 7], ctx: &Context) -> Option<Raw> {
    let event = fields[0];
    if event.is_empty() {
        return None;
    }
    let ingest = ctx.dates.parse(fields[3]).unwrap_or(i64::MAX);
    let user = fields[1];
    let product = fields[5];
    let (typed, purchase) = event_type(fields[4]);
    let mut ok = !user.is_empty() & !product.is_empty() & typed;
    let ts = match ctx.dates.parse(fields[2]) {
        Some(t) if t >= ctx.window.start && t < ctx.window.end => {
            ok &= ingest.saturating_sub(t) <= DAY;
            (t - ctx.window.start) as i32
        }
        _ => {
            ok = false;
            0
        }
    };
    let mut sale = Sale::NONE;
    if purchase && ok {
        match parse_price(fields[6]) {
            Some(price) => {
                let cat = ctx.products.map.get(&Key::from_bytes(product)).copied().unwrap_or(ctx.products.unknown);
                sale = Sale::new(price, cat);
            }
            None => ok = false,
        }
    }
    Some(Raw {
        event: Key::from_bytes(event),
        ingest,
        user: if ok { Key::from_bytes(user) } else { NO_USER },
        ts,
        sale,
    })
}

const WAITING: usize = 8;

struct Filer {
    buckets: Vec<Vec<Raw>>,
    waiting: Vec<[Raw; WAITING]>,
    counts: Vec<u8>,
}

impl Filer {
    fn new(per_bucket: usize) -> Filer {
        const EMPTY: Raw = Raw { event: Key(0), ingest: 0, user: NO_USER, ts: 0, sale: Sale::NONE };
        Filer {
            buckets: (0..EVENT_BUCKETS).map(|_| Vec::with_capacity(per_bucket)).collect(),
            waiting: vec![[EMPTY; WAITING]; EVENT_BUCKETS],
            counts: vec![0; EVENT_BUCKETS],
        }
    }

    #[inline(always)]
    fn file(&mut self, raw: Raw) {
        let b = raw.event.bucket(EVENT_BITS);
        let n = usize::from(self.counts[b]);
        self.waiting[b][n] = raw;
        if n + 1 < WAITING {
            self.counts[b] = (n + 1) as u8;
        } else {
            self.counts[b] = 0;
            self.buckets[b].extend_from_slice(&self.waiting[b]);
        }
    }

    fn finish(mut self) -> Vec<Vec<Raw>> {
        for (b, bucket) in self.buckets.iter_mut().enumerate() {
            bucket.extend_from_slice(&self.waiting[b][..usize::from(self.counts[b])]);
        }
        self.buckets
    }
}

pub fn parse(paths: &[PathBuf], threads: usize, ctx: &Context) -> std::io::Result<Parsed> {
    let bytes = crate::scan::total_size(paths)? as usize;
    let per_bucket = bytes / 70 / threads / EVENT_BUCKETS + 16;
    let states = crate::scan::run_fields::<7, _, _, _>(paths, threads, CHUNK, || Filer::new(per_bucket), |state, fields| {
        if let Some(raw) = parse_event(fields, ctx) {
            state.file(raw);
        }
    })?;
    Ok(states.into_iter().map(Filer::finish).collect())
}

pub fn dedup(parsed: Parsed, threads: usize, people: &People) -> Buckets<Event> {
    const CHUNKS_PER_PART: usize = 8;
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
                        survivors.extend(kept.values().map(|&(_, s, p)| (s, p)).filter(|&(s, p)| parts[s as usize][p as usize].ok()));
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
                            current.push(Event { person, ts: raw.ts, sale: raw.sale });
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
mod tests {
    use super::*;
    use crate::timestamp::HOUR;

    fn at(text: &str) -> i64 {
        timestamp::parse(text.as_bytes()).unwrap()
    }

    fn window() -> Window {
        Window { start: at("2026-10-01T00:00:00Z"), end: at("2026-10-15T00:00:00Z") }
    }

    fn products(categories: usize) -> Products {
        let mut map = HashMap::new();
        map.insert(Key::from_bytes(b"p1"), 0);
        Products {
            map,
            categories: (0..categories).map(|i| format!("c{i}").into_bytes()).collect(),
            unknown: (categories - 1) as u16,
            rank: (0..categories as u16).collect(),
        }
    }

    fn raw(ctx: &Context, fields: [&str; 7]) -> Raw {
        parse_event(&fields.map(str::as_bytes), ctx).unwrap()
    }

    #[test]
    fn records_have_their_packed_sizes() {
        assert_eq!(std::mem::size_of::<Raw>(), 32);
        assert_eq!(std::mem::size_of::<Event>(), 16);
    }

    #[test]
    fn context_dates_parse_like_parse_for_any_window() {
        let products = products(3);
        for window in [Window { start: 185_542_587_100_800_000, end: 185_542_587_273_600_000 }, Window { start: at("0000-01-01T00:00:00Z"), end: at("0000-01-15T00:00:00Z") }, window()] {
            let ctx = Context::new(window, &products);
            for text in [&b"0000-01-03T00:00:00Z"[..], b"2026-10-03T23:58:10Z", b"2026-10-03T19:58:10-04:00", b"9999-12-31T23:59:59Z", b"185542587100800000", b"1791071890000"] {
                assert_eq!(ctx.dates.parse(text), timestamp::parse(text), "{:?} in {window:?}", String::from_utf8_lossy(text));
            }
        }
    }

    #[test]
    fn event_types_match_exactly() {
        for (text, typed, purchase) in [
            ("view", true, false),
            ("cart", true, false),
            ("purchase", true, true),
            ("View", false, false),
            ("CART", false, false),
            ("cart ", false, false),
            (" view", false, false),
            ("Purchase", false, false),
            ("purchases", false, false),
            ("purchas", false, false),
            ("viewcart", false, false),
            ("vie", false, false),
            ("", false, false),
        ] {
            assert_eq!(event_type(text.as_bytes()), (typed, purchase), "{text:?}");
        }
    }

    #[test]
    fn filer_keeps_every_record_in_its_bucket_in_order() {
        for count in [0, 1, WAITING - 1, WAITING, WAITING + 1, 3 * WAITING * EVENT_BUCKETS + 5] {
            let mut filer = Filer::new(2);
            let mut want: Vec<Vec<(u64, i64)>> = vec![Vec::new(); EVENT_BUCKETS];
            for i in 0..count {
                let event = Key::from_bytes(format!("e{i}").as_bytes());
                let raw = Raw { event, ingest: i as i64, user: Key(i as u64 + 1), ts: i as i32, sale: Sale::NONE };
                want[event.bucket(EVENT_BITS)].push((event.0, raw.ingest));
                filer.file(raw);
            }
            let got = filer.finish();
            assert_eq!(got.len(), EVENT_BUCKETS);
            for (b, bucket) in got.iter().enumerate() {
                let have: Vec<(u64, i64)> = bucket.iter().map(|r| (r.event.0, r.ingest)).collect();
                assert_eq!(have, want[b], "{count} records, bucket {b}");
                assert!(bucket.iter().all(|r| r.user.0 == r.ingest as u64 + 1 && i64::from(r.ts) == r.ingest), "{count} records, bucket {b}");
            }
            assert_eq!(got.iter().map(Vec::len).sum::<usize>(), count);
        }
    }

    #[test]
    fn sale_holds_the_largest_price_and_category() {
        for (price, cat) in [(1, 0), (250, 7), (10_000_000, (MAX_CATEGORIES - 1) as u16)] {
            let sale = Sale::new(price, cat);
            assert!(sale.is_purchase());
            assert_eq!((sale.price(), sale.cat()), (price, cat));
        }
        assert!(!Sale::NONE.is_purchase());
        assert_eq!(Sale::NONE.price(), 0);
    }

    #[test]
    fn check_fits_refuses_what_the_records_cannot_hold() {
        let start = window().start;
        let most = i64::from(i32::MAX) - WEEK;
        let three = products(3);
        assert!(check_fits(window(), &three).is_ok());
        assert!(check_fits(Window { start, end: start + most }, &three).is_ok());
        assert!(check_fits(Window { start, end: start + most + 1 }, &three).is_err());
        assert!(check_fits(Window { start: at("0000-01-01T00:00:00Z"), end: i64::MAX }, &three).is_err());
        assert!(check_fits(window(), &products(MAX_CATEGORIES)).is_ok());
        assert!(check_fits(window(), &products(MAX_CATEGORIES + 1)).is_err());
    }

    #[test]
    fn parse_event_stores_ts_from_the_window_start() {
        let products = products(3);
        let ctx = Context::new(window(), &products);
        let buy = raw(&ctx, ["e1", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:00:05Z", "purchase", "p1", "0250"]);
        assert!(buy.ok());
        assert_eq!(i64::from(buy.ts), 2 * DAY + 10 * HOUR);
        assert_eq!(buy.ingest, at("2026-10-03T10:00:05Z"));
        assert_eq!((buy.sale.price(), buy.sale.cat()), (250, 0));
        let unknown = raw(&ctx, ["e2", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:00:05Z", "purchase", "p9", "100"]);
        assert_eq!((unknown.sale.price(), unknown.sale.cat()), (100, products.unknown));
        let first = raw(&ctx, ["e3", "u1", "2026-10-01T00:00:00Z", "2026-10-01T00:00:00Z", "view", "p1", "x"]);
        assert!(first.ok());
        assert_eq!((first.ts, first.sale), (0, Sale::NONE));
        let last = raw(&ctx, ["e4", "u1", "1792022399999", "2026-10-14T23:59:59Z", "cart", "p1", ""]);
        assert!(last.ok());
        assert_eq!(i64::from(last.ts), 14 * DAY - 1);
        for bad in [
            ["e5", "u1", "2026-10-15T00:00:00Z", "2026-10-15T00:00:00Z", "view", "p1", ""],
            ["e6", "u1", "2026-10-03T10:00:00Z", "2026-10-04T10:00:01Z", "view", "p1", ""],
            ["e7", "", "2026-10-03T10:00:00Z", "2026-10-03T10:00:00Z", "view", "p1", ""],
            ["e8", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:00:00Z", "purchase", "p1", "0"],
        ] {
            let copy = raw(&ctx, bad);
            assert!(!copy.ok(), "{bad:?}");
            assert_eq!(copy.event, Key::from_bytes(bad[0].as_bytes()), "{bad:?}");
        }
    }

    #[test]
    fn dedup_keeps_the_copy_with_the_exact_smallest_ingest() {
        let products = products(3);
        let ctx = Context::new(window(), &products);
        let copies = [
            raw(&ctx, ["e1", "u2", "2026-10-03T10:00:00Z", "631152000001", "view", "p1", ""]),
            raw(&ctx, ["e1", "u3", "2026-10-03T10:05:00Z", "631152000000", "view", "p1", ""]),
            raw(&ctx, ["e2", "u1", "2026-10-03 10:00:00", "2026-10-03T10:01:00Z", "view", "p1", ""]),
            raw(&ctx, ["e2", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""]),
            raw(&ctx, ["e3", "u2", "2026-10-03T11:00:00Z", "2026-10-03T11:00:00Z", "purchase", "p1", "500"]),
            raw(&ctx, ["e4", "u4", "2026-10-03T12:00:00Z", "2026-10-04T12:00:00Z", "cart", "p1", ""]),
        ];
        let mut state: Vec<Vec<Raw>> = (0..EVENT_BUCKETS).map(|_| Vec::new()).collect();
        for copy in copies {
            state[copy.event.bucket(EVENT_BITS)].push(copy);
        }
        let people = People::build(vec![(Key::from_bytes(b"u2"), Key::from_bytes(b"u1"))], 1);
        let buckets = dedup(vec![state], 2, &people);
        let mut got: Vec<(Vec<u8>, i64, u32)> = Vec::new();
        for (b, parts) in buckets.iter().enumerate() {
            for event in parts.iter().flatten() {
                assert_eq!(event.person.bucket(PERSON_BITS), b);
                got.push((event.person.to_vec(), i64::from(event.ts), event.sale.price()));
            }
        }
        got.sort();
        let start = window().start;
        assert_eq!(
            got,
            vec![
                (b"u1".to_vec(), at("2026-10-03T11:00:00Z") - start, 500),
                (b"u3".to_vec(), at("2026-10-03T10:05:00Z") - start, 0),
                (b"u4".to_vec(), at("2026-10-03T12:00:00Z") - start, 0),
            ]
        );
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    fn key(text: &str) -> Key {
        Key::from_bytes(text.as_bytes())
    }

    fn raws(count: usize) -> Vec<Raw> {
        let mut out = Vec::new();
        let mut x: u64 = 0x5EED;
        for i in 0..count {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let ok = x % 50 != 0;
            let user = if (x >> 20) % 100 == 0 { key("whale") } else { key(&format!("u{}", (x >> 24) % 3000)) };
            let base = Raw {
                event: key(&format!("e{i}")),
                ingest: 1_000_000 + i as i64,
                user: if ok { user } else { NO_USER },
                ts: ((x >> 32) % 1_000_000) as i32,
                sale: if ok && x % 7 == 0 { Sale::new(100 + (x >> 40) as u32 % 50_000, (x >> 50) as u16 % 20) } else { Sale::NONE },
            };
            out.push(base);
            match (x >> 8) % 20 {
                0 => out.push(Raw { ingest: base.ingest + 5, user: key("other"), ts: base.ts + 1, sale: Sale::NONE, ..base }),
                1 => out.push(Raw { ingest: base.ingest - 5, user: NO_USER, sale: Sale::NONE, ..base }),
                2 => out.push(base),
                _ => {}
            }
        }
        out
    }

    fn parsed(raws: &[Raw], scan_threads: usize) -> Parsed {
        let mut parsed: Parsed = (0..scan_threads).map(|_| (0..EVENT_BUCKETS).map(|_| Vec::new()).collect()).collect();
        for (i, raw) in raws.iter().enumerate() {
            parsed[i % scan_threads][raw.event.bucket(EVENT_BITS)].push(*raw);
        }
        parsed
    }

    type Row = (u64, i32, u32, u16, bool);

    fn expected(raws: &[Raw], people: &People) -> Vec<Vec<Row>> {
        let mut kept: HashMap<Key, Raw> = HashMap::new();
        for raw in raws {
            let entry = kept.entry(raw.event).or_insert(*raw);
            if raw.ingest < entry.ingest {
                *entry = *raw;
            }
        }
        let mut out: Vec<Vec<Row>> = vec![Vec::new(); PERSON_BUCKETS];
        for raw in kept.values().filter(|raw| raw.ok()) {
            let person = people.name_of(raw.user);
            out[person.bucket(PERSON_BITS)].push((person.0, raw.ts, raw.sale.price(), raw.sale.cat(), raw.sale.is_purchase()));
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
                let mut events: Vec<Row> = parts.iter().flatten().map(|e| (e.person.0, e.ts, e.sale.price(), e.sale.cat(), e.sale.is_purchase())).collect();
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
