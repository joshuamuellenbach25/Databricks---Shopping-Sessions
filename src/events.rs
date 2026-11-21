use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
    catalog: Option<Catalog>,
}

impl<'a> Context<'a> {
    pub fn new(window: Window, products: &'a Products) -> Context<'a> {
        let first = window.start.div_euclid(DAY) - 1;
        let last = (window.end - 1).div_euclid(DAY) + 6;
        Context { window, products, dates: timestamp::Months::new(first, last), catalog: Catalog::new(products) }
    }

    #[inline(always)]
    fn category(&self, product: Key) -> u16 {
        let found = match &self.catalog {
            Some(catalog) => catalog.get(product),
            None => self.products.map.get(&product).copied(),
        };
        found.unwrap_or(self.products.unknown)
    }
}

const VACANT: u64 = u64::MAX;
const MAX_PROBES: usize = 64;

struct Catalog {
    keys: Vec<u64>,
    cats: Vec<u16>,
    bits: u32,
}

impl Catalog {
    fn new(products: &Products) -> Option<Catalog> {
        let slots = (2 * products.map.len()).max(16).next_power_of_two();
        let bits = slots.trailing_zeros();
        let (mut keys, mut cats) = (vec![VACANT; slots], vec![0; slots]);
        for (&key, &cat) in &products.map {
            let home = key.bucket(bits);
            let step = (0..MAX_PROBES).find(|step| keys[(home + step) & (slots - 1)] == VACANT)?;
            let i = (home + step) & (slots - 1);
            keys[i] = key.0;
            cats[i] = cat;
        }
        Some(Catalog { keys, cats, bits })
    }

    #[inline(always)]
    fn get(&self, key: Key) -> Option<u16> {
        let mask = self.keys.len() - 1;
        let mut i = key.bucket(self.bits);
        for _ in 0..MAX_PROBES {
            match self.keys[i] {
                VACANT => return None,
                k if k == key.0 => return Some(self.cats[i]),
                _ => i = (i + 1) & mask,
            }
        }
        None
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
                let cat = ctx.category(Key::from_bytes(product));
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
        #[cfg(target_arch = "x86_64")]
        EXIT_FENCE.with(|_| ());
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
            stream(&mut self.buckets[b], &self.waiting[b]);
        }
    }

    fn finish(mut self) -> Vec<Vec<Raw>> {
        fence();
        for (b, bucket) in self.buckets.iter_mut().enumerate() {
            bucket.extend_from_slice(&self.waiting[b][..usize::from(self.counts[b])]);
        }
        self.buckets
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn fence() {
    unsafe { std::arch::x86_64::_mm_sfence() }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn fence() {}

#[cfg(target_arch = "x86_64")]
struct ExitFence;

#[cfg(target_arch = "x86_64")]
impl Drop for ExitFence {
    fn drop(&mut self) {
        fence();
    }
}

#[cfg(target_arch = "x86_64")]
thread_local! {
    static EXIT_FENCE: ExitFence = const { ExitFence };
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn stream(bucket: &mut Vec<Raw>, waiting: &[Raw; WAITING]) {
    use std::arch::x86_64::*;
    if bucket.capacity() - bucket.len() < WAITING {
        fence();
        bucket.reserve(WAITING);
    }
    unsafe {
        let to = bucket.as_mut_ptr().add(bucket.len()) as *mut __m128i;
        let from = waiting.as_ptr() as *const __m128i;
        for i in 0..WAITING * std::mem::size_of::<Raw>() / 16 {
            _mm_stream_si128(to.add(i), _mm_load_si128(from.add(i)));
        }
        bucket.set_len(bucket.len() + WAITING);
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn stream(bucket: &mut Vec<Raw>, waiting: &[Raw; WAITING]) {
    bucket.extend_from_slice(waiting);
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

const SEEN_MIX: u64 = 0xA076_1D64_78BD_642F;
const COPY_MIX: u64 = 0xE703_7ED1_A0B4_28DB;
const SEEN_BITS_PER_RECORD: usize = 8;

fn grow<T: Clone>(buffer: &mut Vec<T>, len: usize, fill: T) {
    if buffer.len() < len {
        buffer.resize(len, fill);
    }
}

#[inline(always)]
fn seen_bit(event: Key, shift: u32) -> (usize, u64) {
    let h = (event.0.wrapping_mul(SEEN_MIX) >> shift) as usize;
    (h >> 6, 1 << (h & 63))
}

struct Sieve {
    once: Vec<u64>,
    twice: Vec<u64>,
    copies: Vec<(u32, u32)>,
    keys: Vec<u64>,
    kept: Vec<(u32, u32)>,
    survivors: Vec<Event>,
    linked: Vec<u32>,
}

impl Sieve {
    fn new() -> Sieve {
        Sieve { once: Vec::new(), twice: Vec::new(), copies: Vec::new(), keys: Vec::new(), kept: Vec::new(), survivors: Vec::new(), linked: Vec::new() }
    }

    fn survivors(&mut self, parts: &mut [Vec<Raw>], people: &People) -> &[Event] {
        let total: usize = parts.iter().map(Vec::len).sum();
        assert!(u32::try_from(total).is_ok(), "an event bucket holds {total} records; at most 2^32 - 1 fit");
        let bits = (total * SEEN_BITS_PER_RECORD).max(64).next_power_of_two().trailing_zeros();
        let (words, shift) = (1usize << (bits - 6), 64 - bits);
        grow(&mut self.once, words, 0);
        grow(&mut self.twice, words, 0);
        let (once, twice) = (&mut self.once[..words], &mut self.twice[..words]);
        once.fill(0);
        twice.fill(0);
        for raw in parts.iter().flatten() {
            let (w, b) = seen_bit(raw.event, shift);
            twice[w] |= once[w] & b;
            once[w] |= b;
        }
        grow(&mut self.survivors, total, Event { person: NO_USER, ts: 0, sale: Sale::NONE });
        grow(&mut self.linked, total, 0);
        grow(&mut self.copies, total, (0, 0));
        let (mut n, mut m, mut c) = (0, 0, 0);
        for (s, part) in parts.iter().enumerate() {
            for (p, raw) in part.iter().enumerate() {
                let (w, b) = seen_bit(raw.event, shift);
                let copied = twice[w] & b != 0;
                let keep = raw.ok() & !copied;
                self.survivors[n] = Event { person: raw.user, ts: raw.ts, sale: raw.sale };
                self.linked[m] = n as u32;
                self.copies[c] = (s as u32, p as u32);
                m += usize::from(keep & people.may_be_linked(raw.user));
                n += usize::from(keep);
                c += usize::from(copied);
            }
        }
        self.keep_earliest(parts, c);
        for &(s, p) in &self.copies[..c] {
            let raw = &parts[s as usize][p as usize];
            if raw.ok() {
                self.survivors[n] = Event { person: raw.user, ts: raw.ts, sale: raw.sale };
                self.linked[m] = n as u32;
                m += usize::from(people.may_be_linked(raw.user));
                n += 1;
            }
        }
        for t in 0..m {
            if let Some(&ahead) = self.linked[..m].get(t + PREFETCH_AHEAD) {
                people.prefetch_slot(self.survivors[ahead as usize].person);
            }
            let j = self.linked[t] as usize;
            self.survivors[j].person = people.linked_name(self.survivors[j].person);
        }
        &self.survivors[..n]
    }

    fn keep_earliest(&mut self, parts: &mut [Vec<Raw>], count: usize) {
        let bits = (count * 2).max(16).next_power_of_two().trailing_zeros();
        let size = 1usize << bits;
        grow(&mut self.keys, size, 0);
        grow(&mut self.kept, size, (0, 0));
        let keys = &mut self.keys[..size];
        keys.fill(0);
        let (mask, shift) = (size - 1, 64 - bits);
        for &(s, p) in &self.copies[..count] {
            let (s, p) = (s as usize, p as usize);
            let (event, ingest) = (parts[s][p].event, parts[s][p].ingest);
            let mut i = (event.0.wrapping_mul(COPY_MIX) >> shift) as usize;
            loop {
                if keys[i] == 0 {
                    keys[i] = event.0;
                    self.kept[i] = (s as u32, p as u32);
                    break;
                }
                if keys[i] == event.0 {
                    let (hs, hp) = (self.kept[i].0 as usize, self.kept[i].1 as usize);
                    if ingest < parts[hs][hp].ingest {
                        parts[hs][hp].user = NO_USER;
                        self.kept[i] = (s as u32, p as u32);
                    } else {
                        parts[s][p].user = NO_USER;
                    }
                    break;
                }
                i = (i + 1) & mask;
            }
        }
    }
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
                    let mut sieve = Sieve::new();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= EVENT_BUCKETS {
                            break;
                        }
                        let mut parts = std::mem::take(&mut *by_bucket[b].lock().unwrap());
                        for event in sieve.survivors(&mut parts, people) {
                            let bucket = event.person.bucket(PERSON_BITS);
                            let current = &mut open[bucket];
                            if current.len() == current.capacity() {
                                let filled = std::mem::replace(current, Vec::with_capacity(chunk));
                                if !filled.is_empty() {
                                    full[bucket].push(filled);
                                }
                            }
                            current.push(*event);
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
    use hashbrown::HashMap;

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
    fn catalog_agrees_with_the_product_map() {
        let id = |i: usize| match i % 4 {
            0 => format!("p{i}"),
            1 => format!("P_{i}"),
            2 => format!("product_number_{i}"),
            _ => format!("{i}-p"),
        };
        for count in [0, 1, 7, 8, 9, 100, 50_000] {
            let map: HashMap<Key, u16> = (0..count).map(|i| (Key::from_bytes(id(i).as_bytes()), (i % 21) as u16)).collect();
            let products = Products { map, categories: Vec::new(), unknown: 21, rank: Vec::new() };
            let Some(catalog) = Catalog::new(&products) else { panic!("{count} products: no catalog") };
            assert!(catalog.keys.len() >= 2 * count && catalog.keys.len().is_power_of_two());
            let mut found = 0;
            for i in 0..count + 3000 {
                let key = Key::from_bytes(id(i).as_bytes());
                let got = catalog.get(key);
                assert_eq!(got, products.map.get(&key).copied(), "{count} products, {}", id(i));
                found += usize::from(got.is_some());
            }
            assert_eq!(found, count);
            assert_eq!(catalog.get(Key::from_bytes(b"")), None);
        }
    }

    fn crowd(bits: u32, count: usize) -> Vec<String> {
        let last = (1 << bits) - 1;
        let ids: Vec<String> = (0..1_000_000).map(|i| format!("q{i}")).filter(|id| Key::from_bytes(id.as_bytes()).bucket(bits) == last).take(count).collect();
        assert_eq!(ids.len(), count, "{bits} bits");
        ids
    }

    #[test]
    fn crowded_product_ids_fall_back_to_the_product_map() {
        let named = ["01", "03", "0Y", "0_", "13", "15", "1_", "1b", "25"].map(String::from).to_vec();
        for (ids, present) in [(named, 8), (crowd(7, 2 * MAX_PROBES), MAX_PROBES), (crowd(8, 2 * MAX_PROBES + 1), MAX_PROBES + 1)] {
            let key = |id: &String| Key::from_bytes(id.as_bytes());
            let bits = (2 * present).max(16).next_power_of_two().trailing_zeros();
            assert!(ids.iter().all(|id| key(id).bucket(bits) == (1 << bits) - 1), "{present} products");
            let map: HashMap<Key, u16> = ids[..present].iter().enumerate().map(|(i, id)| (key(id), (i % 20) as u16)).collect();
            let products = Products { map, categories: Vec::new(), unknown: 21, rank: Vec::new() };
            let catalog = Catalog::new(&products);
            assert_eq!(catalog.is_some(), present <= MAX_PROBES, "{present} products");
            if let Some(catalog) = &catalog {
                assert_eq!(catalog.bits, bits);
                for id in &ids {
                    assert_eq!(catalog.get(key(id)), products.map.get(&key(id)).copied(), "{present} products, {id}");
                }
            }
            let ctx = Context::new(window(), &products);
            assert_eq!(ctx.catalog.is_some(), present <= MAX_PROBES, "{present} products");
            for (i, id) in ids.iter().enumerate() {
                let want = if i < present { (i % 20) as u16 } else { products.unknown };
                assert_eq!(ctx.category(key(id)), want, "{present} products, {id}");
                let buy = raw(&ctx, ["e1", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:00:05Z", "purchase", id.as_str(), "100"]);
                assert_eq!((buy.sale.price(), buy.sale.cat()), (100, want), "{present} products, {id}");
            }
        }
        let map: HashMap<Key, u16> = (1..=50_000).map(|i| (Key::from_bytes(format!("p{i}").as_bytes()), (i % 20) as u16)).collect();
        assert!(Catalog::new(&Products { map, categories: Vec::new(), unknown: 21, rank: Vec::new() }).is_some());
    }

    #[test]
    fn catalog_get_looks_at_most_max_probes_slots() {
        let wanted = Key::from_bytes(b"p1");
        let bits = 8;
        let mask = (1usize << bits) - 1;
        let home = wanted.bucket(bits);
        for (at, want) in [(MAX_PROBES - 1, Some(7)), (MAX_PROBES, None)] {
            let mut keys = vec![VACANT; mask + 1];
            for step in 0..at {
                keys[(home + step) & mask] = Key::from_bytes(format!("f{step}").as_bytes()).0;
            }
            keys[(home + at) & mask] = wanted.0;
            let catalog = Catalog { keys, cats: vec![7; mask + 1], bits };
            assert_eq!(catalog.get(wanted), want, "p1 {at} slots past its home");
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
    use hashbrown::HashMap;

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

    #[test]
    fn three_copies_keep_the_smallest_ingest_in_any_order() {
        let people = People::build(Vec::new(), 0);
        let copies = [10, 20, 30].map(|ingest| Raw { event: key("e1"), ingest, user: key(&format!("u{ingest}")), ts: ingest as i32, sale: Sale::NONE });
        let first_invalid = [Raw { user: NO_USER, ..copies[0] }, copies[1], copies[2]];
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            for (group, want) in [(copies, vec![10]), (first_invalid, vec![])] {
                let raws: Vec<Raw> = order.iter().map(|&i| group[i]).collect();
                let got: Vec<i32> = dedup(parsed(&raws, 3), 2, &people).iter().flatten().flatten().map(|e| e.ts).collect();
                assert_eq!(got, want, "order {order:?}");
            }
        }
    }
}
