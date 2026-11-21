use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::ids::Key;

pub struct People {
    slots: Vec<[u64; 2]>,
    mask: usize,
    shift: u32,
    linked: usize,
    filter: Vec<u64>,
    filter_mask: usize,
}

const SLOT_MIX: u64 = 0xD6E8_FEB8_6659_FD93;
const FILTER_MIX: u64 = 0x9E6C_63D0_676A_9A99;
const FILTER_BITS_PER_ID: usize = 8;

const PART_BITS: u32 = 8;
const PARTS: usize = 1 << PART_BITS;

const FAR: usize = 16;
const NEAR: usize = 8;

const MIN_PARALLEL: usize = 1 << 14;

const LINK: u64 = 1 << 62;

#[inline(always)]
fn mix(id: u64) -> u64 {
    id.wrapping_mul(SLOT_MIX)
}

#[inline(always)]
fn part(id: u64) -> usize {
    (mix(id) >> (64 - PART_BITS)) as usize
}

#[inline(always)]
fn filter_bits(id: u64, filter_mask: usize) -> (usize, u64) {
    let product = u128::from(id) * u128::from(FILTER_MIX);
    let hash = (product >> 64) as u64 ^ product as u64;
    (hash as usize & filter_mask, (1 << (hash >> 58)) | (1 << ((hash >> 52) & 63)))
}

impl People {
    #[cfg(test)]
    pub fn build(pairs: impl IntoIterator<Item = (Key, Key)>, _expected: usize) -> People {
        People::build_in_parts(&[pairs.into_iter().collect()], 2)
    }

    pub fn build_in_parts(links: &[Vec<(Key, Key)>], threads: usize) -> People {
        let total: usize = links.iter().map(Vec::len).sum();
        assert!(total <= u32::MAX as usize / 2, "{total} links: their sides do not fit 32-bit numbers");
        let workers = if total < MIN_PARALLEL { 1 } else { threads.clamp(1, PARTS) };
        let starts: Vec<usize> = links
            .iter()
            .scan(0, |at, list| {
                let start = *at;
                *at += list.len();
                Some(start)
            })
            .collect();
        let link_range = |w: usize| (total * w / workers, total * (w + 1) / workers);
        let counted: Vec<Vec<usize>> = run((0..workers).collect(), |w| {
            let mut counts = vec![0; PARTS];
            each_link(links, &starts, link_range(w), |_, a, b| {
                counts[part(a.0)] += 1;
                counts[part(b.0)] += 1;
            });
            counts
        });
        let mut edges = vec![0; PARTS + 1];
        for p in 0..PARTS {
            edges[p + 1] = edges[p] + counted.iter().map(|counts| counts[p]).sum::<usize>();
        }
        let mut records: Vec<[AtomicU64; 2]> = Vec::with_capacity(edges[PARTS]);
        let mut pieces: Vec<Vec<&mut [MaybeUninit<[AtomicU64; 2]>]>> = (0..workers).map(|_| Vec::with_capacity(PARTS)).collect();
        let mut rest = &mut records.spare_capacity_mut()[..edges[PARTS]];
        for p in 0..PARTS {
            for (mine, counts) in pieces.iter_mut().zip(&counted) {
                let (piece, tail) = rest.split_at_mut(counts[p]);
                mine.push(piece);
                rest = tail;
            }
        }
        run(pieces.into_iter().enumerate().collect(), |(w, mut mine)| {
            let mut filled = vec![0; PARTS];
            each_link(links, &starts, link_range(w), |link, a, b| {
                for (id, side) in [(a.0, 2 * link), (b.0, 2 * link + 1)] {
                    let p = part(id);
                    mine[p][filled[p]].write([AtomicU64::new(id), AtomicU64::new(side as u64)]);
                    filled[p] += 1;
                }
            });
            assert!(mine.iter().zip(&filled).all(|(piece, &n)| piece.len() == n), "a worker filled its pieces partly");
        });
        unsafe { records.set_len(edges[PARTS]) };
        let nodes = &records;
        let node_of: Vec<AtomicU32> = (0..2 * total).map(|_| AtomicU32::new(0)).collect();
        let parts_of = |w: usize| (PARTS * w / workers, PARTS * (w + 1) / workers);
        let numbered: Vec<(Vec<u32>, u64)> = run((0..workers).collect(), |w| {
            let (lo, hi) = parts_of(w);
            let largest = (lo..hi).map(|p| edges[p + 1] - edges[p]).max().unwrap_or(0);
            let mut seen: Vec<(u64, u32)> = vec![(0, 0); (2 * largest).next_power_of_two().max(2)];
            let mut counts = Vec::with_capacity(hi - lo);
            let mut every_bit = 0;
            for p in lo..hi {
                let rows = &nodes[edges[p]..edges[p + 1]];
                let capacity = (2 * rows.len()).next_power_of_two().max(2);
                let (bits, mask) = (capacity.trailing_zeros(), capacity - 1);
                let first = edges[p] as u32;
                let mut count = 0;
                for r in 0..rows.len() {
                    let (id, side) = (rows[r][0].load(Relaxed), rows[r][1].load(Relaxed));
                    let mut i = ((mix(id) << PART_BITS) >> (64 - bits)) as usize;
                    let node = loop {
                        let (slot_id, slot_node) = seen[i];
                        if slot_id == id {
                            break slot_node;
                        }
                        if slot_id == 0 || slot_node < first {
                            let node = first + count as u32;
                            seen[i] = (id, node);
                            rows[count][0].store(id, Relaxed);
                            rows[count][1].store(id, Relaxed);
                            every_bit |= id;
                            count += 1;
                            break node;
                        }
                        i = (i + 1) & mask;
                    };
                    node_of[side as usize].store(node, Relaxed);
                }
                counts.push(count as u32);
            }
            (counts, every_bit)
        });
        let counts: Vec<u32> = numbered.iter().flat_map(|(counts, _)| counts.iter().copied()).collect();
        let every_bit = numbered.iter().fold(0, |bits, &(_, b)| bits | b);
        assert!(every_bit & LINK == 0, "an ID key uses the bit that marks a union-find parent");
        let node = |side: usize| node_of[side].load(Relaxed) as usize;
        let joined: Vec<usize> = run((0..if nodes.is_empty() { 0 } else { workers }).collect(), |w| {
            let (lo, hi) = link_range(w);
            let mut joins = 0;
            for l in lo..hi {
                if l + FAR < hi {
                    touch(nodes, node(2 * (l + FAR)));
                    touch(nodes, node(2 * (l + FAR) + 1));
                }
                if l + NEAR < hi {
                    touch_parent(nodes, node(2 * (l + NEAR)));
                    touch_parent(nodes, node(2 * (l + NEAR) + 1));
                }
                joins += usize::from(join(nodes, node(2 * l), node(2 * l + 1)));
            }
            joins
        });
        let linked: usize = joined.iter().sum();
        drop(node_of);
        let capacity = (linked * 2).next_power_of_two().max(16);
        let (mask, shift) = (capacity - 1, 64 - capacity.trailing_zeros());
        let words = (linked * FILTER_BITS_PER_ID / 64).next_power_of_two();
        let filter_mask = words - 1;
        let filter: Vec<AtomicU64> = (0..words).map(|_| AtomicU64::new(0)).collect();
        let mut slots: Vec<[u64; 2]> = Vec::with_capacity(capacity);
        let fillers = if capacity >= PARTS { workers } else { 1 };
        let mut jobs = Vec::with_capacity(fillers);
        let mut rest = &mut slots.spare_capacity_mut()[..capacity];
        for w in 0..fillers {
            let (lo, hi) = (PARTS * w / fillers, PARTS * (w + 1) / fillers);
            let start = lo * capacity / PARTS;
            let (stretch, tail) = rest.split_at_mut(hi * capacity / PARTS - start);
            rest = tail;
            jobs.push((lo, hi, start, stretch));
        }
        let (edges, counts, bloom) = (&edges, &counts, &filter);
        let spilled: Vec<Vec<[u64; 2]>> = run(jobs, |(lo, hi, start, stretch)| {
            let mut taken = vec![false; stretch.len()];
            let mut spill = Vec::new();
            for p in lo..hi {
                let (from, to) = (edges[p], edges[p] + counts[p] as usize);
                for node in from..to {
                    if node + FAR < to {
                        touch_parent(nodes, node + FAR);
                    }
                    let word = nodes[node][1].load(Relaxed);
                    if word & LINK == 0 {
                        continue;
                    }
                    let id = nodes[node][0].load(Relaxed);
                    #[cfg(test)]
                    tests::count_hops(nodes, node);
                    let entry = [id, find(nodes, node).1];
                    let (at, bits) = filter_bits(id, filter_mask);
                    bloom[at].fetch_or(bits, Relaxed);
                    let mut i = (mix(id) >> shift) as usize - start;
                    loop {
                        match taken.get(i) {
                            None => {
                                spill.push(entry);
                                break;
                            }
                            Some(false) => {
                                stretch[i].write(entry);
                                taken[i] = true;
                                break;
                            }
                            Some(true) => i += 1,
                        }
                    }
                }
            }
            for (slot, &used) in stretch.iter_mut().zip(&taken) {
                if !used {
                    slot.write([0, 0]);
                }
            }
            spill
        });
        unsafe { slots.set_len(capacity) };
        for entry in spilled.into_iter().flatten() {
            let mut i = (mix(entry[0]) >> shift) as usize;
            while slots[i][0] != 0 {
                i = (i + 1) & mask;
            }
            slots[i] = entry;
        }
        People { slots, mask, shift, linked, filter: filter.into_iter().map(AtomicU64::into_inner).collect(), filter_mask }
    }

    #[inline(always)]
    fn slot(&self, key: Key) -> usize {
        (mix(key.0) >> self.shift) as usize
    }

    #[inline(always)]
    pub fn may_be_linked(&self, key: Key) -> bool {
        let (word, bits) = filter_bits(key.0, self.filter_mask);
        self.filter[word] & bits == bits
    }

    #[inline(always)]
    pub fn prefetch_slot(&self, key: Key) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            _mm_prefetch::<_MM_HINT_T0>(self.slots.as_ptr().add(self.slot(key)) as *const i8);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = key;
    }

    #[cfg(test)]
    pub fn name_of(&self, key: Key) -> Key {
        if self.may_be_linked(key) {
            self.linked_name(key)
        } else {
            key
        }
    }

    #[inline(always)]
    pub fn linked_name(&self, key: Key) -> Key {
        let mut i = self.slot(key);
        loop {
            let [id, name] = self.slots[i];
            if id == key.0 {
                return if id == 0 { key } else { Key(name) };
            }
            if id == 0 {
                return key;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn linked(&self) -> usize {
        self.linked
    }
}

fn each_link(links: &[Vec<(Key, Key)>], starts: &[usize], (lo, hi): (usize, usize), mut f: impl FnMut(usize, Key, Key)) {
    for (list, &start) in links.iter().zip(starts) {
        let (from, to) = (lo.max(start), hi.min(start + list.len()));
        if from < to {
            for (link, &(a, b)) in (from..to).zip(&list[from - start..to - start]) {
                if a.0 != 0 && b.0 != 0 {
                    f(link, a, b);
                }
            }
        }
    }
}

#[inline(always)]
fn touch(nodes: &[[AtomicU64; 2]], i: usize) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch::<_MM_HINT_T0>(nodes.as_ptr().wrapping_add(i) as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (nodes, i);
}

#[inline(always)]
fn touch_parent(nodes: &[[AtomicU64; 2]], i: usize) {
    let word = nodes[i][1].load(Relaxed);
    if word & LINK != 0 {
        touch(nodes, (word ^ LINK) as usize);
    }
}

fn run<J: Send, T: Send>(jobs: Vec<J>, work: impl Fn(J) -> T + Sync) -> Vec<T> {
    let mut jobs = jobs.into_iter();
    let Some(first) = jobs.next() else {
        return Vec::new();
    };
    std::thread::scope(|scope| {
        let work = &work;
        let handles: Vec<_> = jobs.map(|job| scope.spawn(move || work(job))).collect();
        let mut results = vec![work(first)];
        results.extend(handles.into_iter().map(|handle| handle.join().unwrap()));
        results
    })
}

#[inline(always)]
fn find(nodes: &[[AtomicU64; 2]], mut i: usize) -> (usize, u64) {
    loop {
        let word = nodes[i][1].load(Relaxed);
        if word & LINK == 0 {
            return (i, word);
        }
        let parent = (word ^ LINK) as usize;
        let up = nodes[parent][1].load(Relaxed);
        if up & LINK == 0 {
            return (parent, up);
        }
        nodes[i][1].store(up, Relaxed);
        i = (up ^ LINK) as usize;
    }
}

#[inline(always)]
fn join(nodes: &[[AtomicU64; 2]], a: usize, b: usize) -> bool {
    loop {
        let (ra, ida) = find(nodes, a);
        let (rb, idb) = find(nodes, b);
        if ra == rb {
            return false;
        }
        let (child, child_id, root) = if Key(ida).cmp_bytes(Key(idb)).is_lt() { (rb, idb, ra) } else { (ra, ida, rb) };
        if nodes[child][1].compare_exchange(child_id, LINK | root as u64, Relaxed, Relaxed).is_ok() {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use hashbrown::HashMap;

    use super::*;

    thread_local! {
        static HOPS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn count_hops(nodes: &[[AtomicU64; 2]], mut node: usize) {
        let mut hops = 0;
        loop {
            let word = nodes[node][1].load(Relaxed);
            if word & LINK == 0 {
                break;
            }
            node = (word ^ LINK) as usize;
            hops += 1;
        }
        HOPS.with(|total| total.set(total.get() + hops));
    }

    fn k(s: &str) -> Key {
        Key::from_bytes(s.as_bytes())
    }

    #[test]
    fn chains_across_order_and_direction() {
        let people = People::build(vec![(k("u3"), k("u9")), (k("u9"), k("u2")), (k("U7"), k("u9")), (k("u10"), k("u10"))], 4);
        for id in ["u3", "u9", "u2", "U7"] {
            assert_eq!(people.name_of(k(id)), k("U7"), "{id}");
        }
        assert_eq!(people.name_of(k("u10")), k("u10"));
        assert_eq!(people.name_of(k("u1")), k("u1"));
    }

    #[test]
    fn long_chain_and_star() {
        let mut pairs = Vec::new();
        for i in 0..400 {
            pairs.push((k(&format!("c{}", i + 1)), k(&format!("c{i}"))));
        }
        for i in 0..1000 {
            pairs.push((k(&format!("p{i}")), k("kiosk")));
        }
        let people = People::build(pairs, 1400);
        assert_eq!(people.name_of(k("c400")), k("c0"));
        assert_eq!(people.name_of(k("p999")), k("kiosk"));
        assert_eq!(people.name_of(k("c0")), k("c0"));
    }

    #[test]
    fn table_matches_a_plain_map_on_many_ids() {
        let mut pairs = Vec::new();
        let mut x: u64 = 7;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = format!("u{}", (x >> 40) % 30_000);
            let b = format!("u{}", (x >> 10) % 30_000);
            pairs.push((k(&a), k(&b)));
        }
        let lists = vec![pairs[..7000].to_vec(), Vec::new(), pairs[7000..].to_vec()];
        let built: Vec<People> = [1, 2, 7, 256].iter().map(|&threads| People::build_in_parts(&lists, threads)).collect();
        let mut parent: HashMap<String, String> = HashMap::new();
        fn root(parent: &mut HashMap<String, String>, x: &str) -> String {
            let mut cur = x.to_string();
            while let Some(next) = parent.get(&cur).cloned() {
                if next == cur {
                    break;
                }
                cur = next;
            }
            cur
        }
        let mut x: u64 = 7;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = format!("u{}", (x >> 40) % 30_000);
            let b = format!("u{}", (x >> 10) % 30_000);
            parent.entry(a.clone()).or_insert_with(|| a.clone());
            parent.entry(b.clone()).or_insert_with(|| b.clone());
            let (ra, rb) = (root(&mut parent, &a), root(&mut parent, &b));
            if ra != rb {
                parent.insert(ra, rb);
            }
        }
        let ids: Vec<String> = parent.keys().cloned().collect();
        let mut smallest: HashMap<String, String> = HashMap::new();
        for id in &ids {
            let r = root(&mut parent, id);
            let entry = smallest.entry(r).or_insert_with(|| id.clone());
            if id.as_bytes() < entry.as_bytes() {
                *entry = id.clone();
            }
        }
        let linked = ids.iter().filter(|id| smallest[&root(&mut parent, id)] != **id).count();
        for people in &built {
            for id in &ids {
                let r = root(&mut parent, id);
                assert_eq!(people.name_of(k(id)), k(&smallest[&r]), "{id}");
            }
            assert_eq!(people.linked(), linked);
            assert_eq!(people.name_of(k("u99999")), k("u99999"));
            assert_eq!(people.name_of(k("")), k(""));
        }
    }

    #[test]
    fn workers_join_a_long_chain_and_a_hub_into_one_person() {
        let mut pairs = Vec::new();
        for j in 0..12_000 {
            let i = j * 7_919 % 12_000;
            pairs.push((k(&format!("c{}", i + 1)), k(&format!("c{i}"))));
        }
        for i in 0..8_000 {
            pairs.push((k(&format!("p{i}")), k("hub")));
        }
        pairs.push((k("hub"), k("c6000")));
        let ids: Vec<String> = (0..=12_000).map(|i| format!("c{i}")).chain((0..8_000).map(|i| format!("p{i}"))).chain(["hub".to_string()]).collect();
        let lists: Vec<Vec<(Key, Key)>> = pairs.chunks(4_321).map(<[_]>::to_vec).collect();
        for threads in [1, 3, 8, 64] {
            let people = People::build_in_parts(&lists, threads);
            assert_eq!(people.linked(), ids.len() - 1, "threads {threads}");
            for id in &ids {
                assert_eq!(people.name_of(k(id)), k("c0"), "{id}, threads {threads}");
            }
        }
    }

    fn race_to_one_root(refused: Option<usize>) -> Result<(), String> {
        const THREADS: usize = 8;
        const EACH: usize = 20_000;
        const WAIT: u32 = 0;
        const GO: u32 = 1;
        const STOP: u32 = 2;
        let top: u64 = 1 << 40;
        let node = |i: usize, t: usize| 1 + i * THREADS + t;
        let nodes: Vec<[AtomicU64; 2]> = (0..(1 + THREADS * EACH) as u64).map(|n| top - n).map(|id| [AtomicU64::new(id), AtomicU64::new(id)]).collect();
        let signal = AtomicU32::new(WAIT);
        let joins: usize = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(THREADS);
            for t in 0..THREADS {
                let (nodes, signal) = (&nodes, &signal);
                let started = if refused == Some(t) {
                    Err(std::io::Error::other("refused by the test"))
                } else {
                    std::thread::Builder::new().spawn_scoped(scope, move || {
                        let mut now = signal.load(Relaxed);
                        while now == WAIT {
                            std::thread::yield_now();
                            now = signal.load(Relaxed);
                        }
                        if now == GO { (0..EACH).filter(|&i| join(nodes, node(i, t), 0)).count() } else { 0 }
                    })
                };
                match started {
                    Ok(handle) => handles.push(handle),
                    Err(error) => {
                        signal.store(STOP, Relaxed);
                        return Err(format!("thread {t} did not start: {error}"));
                    }
                }
            }
            signal.store(GO, Relaxed);
            Ok(handles.into_iter().map(|h| h.join().unwrap()).sum::<usize>())
        })?;
        assert_eq!(joins, THREADS * EACH);
        let name = top - (THREADS * EACH) as u64;
        for n in 0..nodes.len() {
            assert_eq!(find(&nodes, n).1, name, "node {n}");
        }
        Ok(())
    }

    #[test]
    fn threads_racing_to_join_one_root_lose_no_join() {
        assert_eq!(race_to_one_root(None), Ok(()));
    }

    #[test]
    fn a_thread_that_does_not_start_ends_the_race() {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || send.send(race_to_one_root(Some(7))));
        let ended = receive.recv_timeout(std::time::Duration::from_secs(60));
        assert_eq!(ended, Ok(Err("thread 7 did not start: refused by the test".to_string())));
    }

    #[test]
    fn naming_a_descending_chain_reads_few_parents() {
        let n = 16_000;
        let id = |i: usize| k(&format!("u{i:06}"));
        let pairs: Vec<(Key, Key)> = (1..=n).rev().map(|i| (id(i - 1), id(i))).collect();
        HOPS.with(|hops| hops.set(0));
        let people = People::build_in_parts(&[pairs], 1);
        let hops = HOPS.with(Cell::get);
        assert!(hops <= 8 * n, "naming {n} chained IDs read {hops} parents");
        assert_eq!(people.linked(), n);
        for i in 0..=n {
            assert_eq!(people.name_of(id(i)), id(0), "u{i:06}");
        }
    }

    #[test]
    fn empty_sides_are_skipped_and_long_ids_compare_as_bytes() {
        let people = People::build_in_parts(&[vec![(k(""), k("u1")), (k("u2"), k("u1")), (k("zzzzzzzzzzzz"), k("a b")), (k("u5"), k("zzzzzzzzzzzz"))]], 1);
        assert_eq!(people.name_of(k("u2")), k("u1"));
        assert_eq!(people.name_of(k("u1")), k("u1"));
        assert_eq!(people.name_of(k("")), k(""));
        for id in ["u5", "zzzzzzzzzzzz", "a b"] {
            assert_eq!(people.name_of(k(id)), k("a b"), "{id}");
        }
        assert_eq!(people.linked(), 3);
        assert_eq!(People::build_in_parts(&[vec![(k(""), k("u1"))]], 1).linked(), 0);
    }

    #[test]
    fn the_filter_passes_every_linked_id() {
        let mut pairs = Vec::new();
        let mut x: u64 = 11;
        for _ in 0..40_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            pairs.push((k(&format!("v{}", (x >> 40) % 60_000)), k(&format!("v{}", (x >> 12) % 60_000))));
        }
        let lists: Vec<Vec<(Key, Key)>> = pairs.chunks(9_999).map(<[_]>::to_vec).collect();
        for threads in [1, 8] {
            let people = People::build_in_parts(&lists, threads);
            let mut linked = 0;
            for i in 0..60_000 {
                let id = k(&format!("v{i}"));
                let name = people.linked_name(id);
                if name != id {
                    linked += 1;
                    assert!(people.may_be_linked(id), "v{i}, threads {threads}");
                }
            }
            assert_eq!(linked, people.linked(), "threads {threads}");
        }
    }
}
