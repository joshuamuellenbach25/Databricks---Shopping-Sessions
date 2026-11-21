//! Faults in the spare capacity of freshly sized record vectors before a scan
//! thread starts parsing, so the kernel zeroes their pages in a few large
//! requests instead of one page fault at a time inside the parse loop. Only
//! Linux does anything; elsewhere `vectors` leaves the vectors as they are.
//! Nothing here changes what a vector holds: it reads no record and writes
//! only spare capacity, which the vector treats as uninitialized.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

const PAGE: usize = 4096;
const HUGE_PAGE: usize = 2 << 20;
/// `MADV_HUGEPAGE` and `MADV_POPULATE_WRITE` (Linux 5.14) from the Linux uapi.
const MADV_HUGEPAGE: i32 = 14;
const MADV_POPULATE_WRITE: i32 = 23;

/// One madvise call on `len` bytes from `addr`: Ok, or the errno it failed with.
type Advise = fn(usize, usize, i32) -> Result<(), i32>;

/// What the prefaults did. After the first failed `MADV_POPULATE_WRITE`
/// (an older kernel answers EINVAL) every later range is touched instead.
#[derive(Default)]
struct Stats {
    populate_off: AtomicBool,
    populate_errno: AtomicI32,
    populated: AtomicUsize,
    touched_pages: AtomicUsize,
    huge_ok: AtomicUsize,
    huge_failed: AtomicUsize,
    huge_errno: AtomicI32,
}

static STATS: Stats = Stats {
    populate_off: AtomicBool::new(false),
    populate_errno: AtomicI32::new(0),
    populated: AtomicUsize::new(0),
    touched_pages: AtomicUsize::new(0),
    huge_ok: AtomicUsize::new(0),
    huge_failed: AtomicUsize::new(0),
    huge_errno: AtomicI32::new(0),
};

/// Prefaults the spare capacity of every vector in `vs`, on the calling thread.
pub fn vectors<T>(vs: &mut [Vec<T>]) {
    #[cfg(target_os = "linux")]
    prefault(vs, linux::madvise, &STATS);
    #[cfg(not(target_os = "linux"))]
    let _ = vs;
}

/// One line for the log: what the prefaults did so far.
pub fn report() -> String {
    let s = &STATS;
    format!(
        "prefault: populated {} MB, touched {} pages (populate errno {}), hugepage hints {} ok {} failed (errno {})",
        s.populated.load(Ordering::Relaxed) >> 20,
        s.touched_pages.load(Ordering::Relaxed),
        s.populate_errno.load(Ordering::Relaxed),
        s.huge_ok.load(Ordering::Relaxed),
        s.huge_failed.load(Ordering::Relaxed),
        s.huge_errno.load(Ordering::Relaxed),
    )
}

/// The byte range of `v`'s spare capacity, or None when it has none.
fn spare<T>(v: &mut Vec<T>) -> Option<(usize, usize)> {
    let spare = v.spare_capacity_mut();
    let bytes = std::mem::size_of_val(spare);
    if bytes == 0 {
        return None;
    }
    let start = spare.as_mut_ptr() as usize;
    Some((start, start + bytes))
}

/// Asks for huge pages over the 2 MiB pages inside the span of all the spare
/// ranges (a hint: a failure leaves the memory as it was), then populates
/// each range for writing, or touches it when populating fails.
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
        let Some((start, end)) = spare(v) else {
            continue;
        };
        if !stats.populate_off.load(Ordering::Relaxed) {
            // The pages holding the range's first and last bytes are mapped,
            // since the vector owns those bytes, so rounding out to whole
            // pages stays inside mapped memory; populating changes no byte.
            let page_start = start / PAGE * PAGE;
            let page_end = end.next_multiple_of(PAGE);
            match advise(page_start, page_end - page_start, MADV_POPULATE_WRITE) {
                Ok(()) => {
                    stats.populated.fetch_add(page_end - page_start, Ordering::Relaxed);
                    continue;
                }
                Err(errno) => {
                    stats.populate_errno.store(errno, Ordering::Relaxed);
                    stats.populate_off.store(true, Ordering::Relaxed);
                }
            }
        }
        let pages = touch(v);
        stats.touched_pages.fetch_add(pages, Ordering::Relaxed);
    }
}

/// Writes a zero byte into `v`'s spare capacity in every 4 KiB page it
/// reaches, and returns how many pages that was.
fn touch<T>(v: &mut Vec<T>) -> usize {
    let spare = v.spare_capacity_mut();
    let bytes = std::mem::size_of_val(spare);
    let base = spare.as_mut_ptr() as *mut u8;
    let mut offset = 0;
    let mut pages = 0;
    while offset < bytes {
        // SAFETY: `offset < bytes`, so the byte is in the spare capacity,
        // which the vector holds as uninitialized memory it owns.
        unsafe { std::ptr::write_volatile(base.add(offset), 0) };
        pages += 1;
        let addr = base as usize + offset;
        offset = (addr / PAGE + 1) * PAGE - base as usize;
    }
    pages
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_int, c_void};

    extern "C" {
        #[link_name = "madvise"]
        fn sys_madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }

    pub fn madvise(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        // SAFETY: the callers pass page-aligned ranges of mapped memory the
        // process owns, and both advices leave every byte's value as it was.
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
    use std::sync::Mutex;

    static CALLS: Mutex<Vec<(usize, usize, i32)>> = Mutex::new(Vec::new());

    fn record(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        CALLS.lock().unwrap().push((addr, len, advice));
        Ok(())
    }

    fn refuse_populate(addr: usize, len: usize, advice: i32) -> Result<(), i32> {
        CALLS.lock().unwrap().push((addr, len, advice));
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

    #[test]
    fn prefault_keeps_contents_and_covers_every_spare_range() {
        for (advise, touched) in [(record as Advise, false), (refuse_populate as Advise, true)] {
            CALLS.lock().unwrap().clear();
            let mut vs = vectors_with_data();
            let before = snapshot(&vs);
            let stats = Stats::default();
            prefault(&mut vs, advise, &stats);
            assert_eq!(snapshot(&vs), before);
            let calls = CALLS.lock().unwrap().clone();
            let populates: Vec<_> = calls.iter().filter(|c| c.2 == MADV_POPULATE_WRITE).collect();
            let with_spare = vs.iter().filter(|v| v.capacity() > v.len()).count();
            if touched {
                assert_eq!(populates.len(), 1, "populate is tried once, then every range is touched");
                assert!(stats.populate_off.load(Ordering::Relaxed));
                assert!(stats.touched_pages.load(Ordering::Relaxed) >= with_spare);
            } else {
                assert_eq!(populates.len(), with_spare);
                assert_eq!(stats.touched_pages.load(Ordering::Relaxed), 0);
                for v in &mut vs {
                    if let Some((start, end)) = spare(v) {
                        assert!(populates.iter().any(|&&(a, l, _)| a % PAGE == 0 && l % PAGE == 0 && a <= start && end <= a + l));
                    }
                }
            }
            for &(a, l, advice) in &calls {
                if advice == MADV_HUGEPAGE {
                    assert!(a % HUGE_PAGE == 0 && l % HUGE_PAGE == 0 && l > 0);
                }
            }
        }
    }

    #[test]
    fn touch_reaches_every_page_of_the_spare_capacity() {
        for (len, cap) in [(0usize, 1usize), (0, 512), (3, 513), (100, 5000), (0, 70_000)] {
            let mut v: Vec<u64> = Vec::with_capacity(cap);
            v.extend(0..len as u64);
            let (start, end) = spare(&mut v).unwrap();
            let want = (end - 1) / PAGE - start / PAGE + 1;
            assert_eq!(touch(&mut v), want, "len {len} cap {cap}");
            assert_eq!(v, (0..len as u64).collect::<Vec<_>>());
        }
        assert_eq!(touch(&mut Vec::<u64>::new()), 0);
    }
}
