#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

const PAGE: usize = 4096;
const HUGE_PAGE: usize = 2 << 20;
const MIN_SPARE: usize = 4 * PAGE;
const MADV_HUGEPAGE: i32 = 14;
const MADV_POPULATE_WRITE: i32 = 23;

type Advise = fn(usize, usize, i32) -> Result<(), i32>;

#[derive(Default)]
struct Stats {
    populate_off: AtomicBool,
    populate_errno: AtomicI32,
    populated: AtomicUsize,
    huge_ok: AtomicUsize,
    huge_failed: AtomicUsize,
    huge_errno: AtomicI32,
}

static STATS: Stats = Stats {
    populate_off: AtomicBool::new(false),
    populate_errno: AtomicI32::new(0),
    populated: AtomicUsize::new(0),
    huge_ok: AtomicUsize::new(0),
    huge_failed: AtomicUsize::new(0),
    huge_errno: AtomicI32::new(0),
};

pub fn vectors<T>(vs: &mut [Vec<T>]) {
    #[cfg(target_os = "linux")]
    prefault(vs, linux::madvise, &STATS);
    #[cfg(not(target_os = "linux"))]
    let _ = vs;
}

pub fn report() -> String {
    line(&STATS)
}

fn line(s: &Stats) -> String {
    format!(
        "prefault: populate requests {} MiB (populate errno {}), hugepage hints {} ok {} failed (errno {})",
        s.populated.load(Ordering::Relaxed) >> 20,
        s.populate_errno.load(Ordering::Relaxed),
        s.huge_ok.load(Ordering::Relaxed),
        s.huge_failed.load(Ordering::Relaxed),
        s.huge_errno.load(Ordering::Relaxed),
    )
}

fn spare<T>(v: &mut Vec<T>) -> Option<(usize, usize)> {
    let spare = v.spare_capacity_mut();
    let bytes = std::mem::size_of_val(spare);
    if bytes < MIN_SPARE {
        return None;
    }
    let start = spare.as_mut_ptr() as usize;
    Some((start, start + bytes))
}

fn prefault<T>(vs: &mut [Vec<T>], advise: Advise, stats: &Stats) {
    let mut low = usize::MAX;
    let mut high = 0;
    for v in vs.iter_mut() {
        if let Some((start, end)) = spare(v) {
            low = low.min(start);
            high = high.max(end);
        }
    }
    if low >= high {
        return;
    }
    let huge_start = low.next_multiple_of(HUGE_PAGE);
    let huge_end = high / HUGE_PAGE * HUGE_PAGE;
    if huge_start < huge_end {
        match advise(huge_start, huge_end - huge_start, MADV_HUGEPAGE) {
            Ok(()) => {
                stats.huge_ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(errno) => {
                stats.huge_failed.fetch_add(1, Ordering::Relaxed);
                stats.huge_errno.store(errno, Ordering::Relaxed);
            }
        }
    }
    for v in vs.iter_mut() {
        if stats.populate_off.load(Ordering::Relaxed) {
            return;
        }
        let Some((start, end)) = spare(v) else {
            continue;
        };
        let page_start = start / PAGE * PAGE;
        let page_end = end.next_multiple_of(PAGE);
        match advise(page_start, page_end - page_start, MADV_POPULATE_WRITE) {
            Ok(()) => {
                stats.populated.fetch_add(page_end - page_start, Ordering::Relaxed);
            }
            Err(errno) => {
                stats.populate_errno.store(errno, Ordering::Relaxed);
                stats.populate_off.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_int, c_void};

    extern "C" {
        #[link_name = "madvise"]
        fn sys_madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }

    pub fn madvise(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        if unsafe { sys_madvise(addr as *mut c_void, len, advice) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static CALLS: RefCell<Vec<(usize, usize, i32)>> = RefCell::new(Vec::new());
    }

    fn take_calls() -> Vec<(usize, usize, i32)> {
        CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }

    fn populates(calls: &[(usize, usize, i32)]) -> Vec<(usize, usize, i32)> {
        calls.iter().copied().filter(|c| c.2 == MADV_POPULATE_WRITE).collect()
    }

    fn record(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        CALLS.with(|c| c.borrow_mut().push((addr, len, advice)));
        Ok(())
    }

    fn refuse_populate(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        record(addr, len, advice)?;
        if advice == MADV_POPULATE_WRITE {
            Err(22)
        } else {
            Ok(())
        }
    }

    fn vectors_with_data() -> Vec<Vec<u64>> {
        (0..5u64).map(|i| {
            let mut v = Vec::with_capacity(1000 + 3000 * i as usize);
            v.extend(0..i * 7);
            v
        })
        .chain([Vec::new(), (0..10).collect::<Vec<u64>>()])
        .collect()
    }

    fn snapshot(vs: &[Vec<u64>]) -> Vec<(Vec<u64>, usize, usize)> {
        vs.iter().map(|v| (v.clone(), v.capacity(), v.as_ptr() as usize)).collect()
    }

    fn marked(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|_| {
                let mut v = Vec::with_capacity(4 * MIN_SPARE);
                for b in v.spare_capacity_mut() {
                    b.write(0xA5);
                }
                v
            })
            .collect()
    }

    fn still_marked(vs: &mut [Vec<u8>]) -> bool {
        vs.iter_mut().all(|v| v.spare_capacity_mut().iter().all(|b| unsafe { b.assume_init_read() } == 0xA5))
    }

    #[test]
    fn prefault_keeps_contents_and_covers_every_large_spare_range() {
        take_calls();
        let mut vs = vectors_with_data();
        let before = snapshot(&vs);
        let stats = Stats::default();
        prefault(&mut vs, record, &stats);
        assert_eq!(snapshot(&vs), before);
        let calls = take_calls();
        let populates = populates(&calls);
        let ranges: Vec<(usize, usize)> = vs.iter_mut().filter_map(spare).collect();
        assert_eq!((ranges.len(), populates.len()), (4, 4));
        for (start, end) in ranges {
            assert!(populates.iter().any(|&(a, l, _)| a % PAGE == 0 && l % PAGE == 0 && a <= start && end <= a + l));
        }
        assert_eq!(stats.populated.load(Ordering::Relaxed), populates.iter().map(|c| c.1).sum::<usize>());
        for &(a, l, advice) in &calls {
            if advice == MADV_HUGEPAGE {
                assert!(a % HUGE_PAGE == 0 && l % HUGE_PAGE == 0 && l > 0);
            }
        }
    }

    #[test]
    fn vectors_with_little_spare_capacity_get_no_advice() {
        take_calls();
        let mut vs: Vec<Vec<u32>> = vec![Vec::with_capacity(16), Vec::with_capacity(MIN_SPARE / 4 - 1), Vec::with_capacity(MIN_SPARE / 4)];
        let stats = Stats::default();
        prefault(&mut vs, record, &stats);
        let populates = populates(&take_calls());
        assert!(spare(&mut vs[0]).is_none() && spare(&mut vs[1]).is_none());
        let (start, end) = spare(&mut vs[2]).unwrap();
        assert_eq!(populates.len(), 1);
        assert!(populates[0].0 <= start && end <= populates[0].0 + populates[0].1);
    }

    #[test]
    fn a_failed_populate_leaves_every_later_vector_to_demand_paging() {
        take_calls();
        let stats = Stats::default();
        let mut first = marked(4);
        prefault(&mut first, refuse_populate, &stats);
        assert_eq!(populates(&take_calls()).len(), 1);
        assert!(stats.populate_off.load(Ordering::Relaxed));
        assert_eq!(stats.populate_errno.load(Ordering::Relaxed), 22);
        let mut second = marked(4);
        prefault(&mut second, record, &stats);
        assert!(populates(&take_calls()).is_empty());
        assert_eq!(stats.populated.load(Ordering::Relaxed), 0);
        assert!(still_marked(&mut first) && still_marked(&mut second));
    }

    #[test]
    fn the_report_counts_requested_mebibytes() {
        let stats = Stats::default();
        stats.populated.store((5 << 20) + PAGE, Ordering::Relaxed);
        stats.populate_errno.store(22, Ordering::Relaxed);
        stats.huge_ok.store(3, Ordering::Relaxed);
        assert_eq!(line(&stats), "prefault: populate requests 5 MiB (populate errno 22), hugepage hints 3 ok 0 failed (errno 0)");
    }
}
