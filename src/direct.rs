use std::alloc::Layout;
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::Builder;
use std::time::Instant;

use memchr::{memchr, memrchr};
use memmap2::MmapOptions;

pub const ALIGN: usize = 4096;
pub const PAGE: usize = 4096;
pub const BUFFER: usize = 4 << 20;
const READERS: usize = 16;
const WINDOW: usize = 16;
const SAMPLES: usize = 16;
const EDGE_BUFFERS: usize = 16;

const _: () = assert!(BUFFER % ALIGN == 0 && BUFFER >= ALIGN);

pub unsafe trait Disk: Sync {
    type File: Sync;

    fn incore(&self, path: &Path, window: &[u8], pages: &mut [u8]) -> io::Result<()>;

    fn cached(&self, path: &Path, file: &File, offset: u64) -> io::Result<bool>;

    fn open(&self, path: &Path) -> io::Result<Self::File>;

    fn read_at(&self, file: &Self::File, dst: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize>;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const O_DIRECT: i32 = 0o40000;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_PREADV2: std::ffi::c_long = 327;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RWF_NOWAIT: std::ffi::c_long = 8;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const EAGAIN: i32 = 11;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub struct Linux;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe impl Disk for Linux {
    type File = File;

    fn incore(&self, _: &Path, window: &[u8], pages: &mut [u8]) -> io::Result<()> {
        extern "C" {
            fn mincore(addr: *mut std::ffi::c_void, length: usize, vec: *mut u8) -> std::ffi::c_int;
        }
        if window.as_ptr() as usize % PAGE != 0 || pages.len() < window.len().div_ceil(PAGE) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("mincore of {} bytes at {:p} into {} page flags", window.len(), window.as_ptr(), pages.len())));
        }
        if unsafe { mincore(window.as_ptr() as *mut std::ffi::c_void, window.len(), pages.as_mut_ptr()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn cached(&self, _: &Path, file: &File, offset: u64) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
        }
        let at = std::ffi::c_long::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("offset {offset} does not fit in off_t")))?;
        let mut byte = [0u8; 1];
        let mut buffers = [io::IoSliceMut::new(&mut byte)];
        let (count, high): (std::ffi::c_long, std::ffi::c_long) = (1, 0);
        match unsafe { syscall(SYS_PREADV2, std::ffi::c_long::from(file.as_raw_fd()), buffers.as_mut_ptr(), count, at, high, RWF_NOWAIT) } {
            1 => Ok(true),
            0 => Ok(false),
            -1 => match io::Error::last_os_error() {
                error if error.raw_os_error() == Some(EAGAIN) => Ok(false),
                error => Err(error),
            },
            other => Err(io::Error::other(format!("a 1-byte read probe at offset {offset} reported {other}"))),
        }
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).custom_flags(O_DIRECT).open(path)
    }

    fn read_at(&self, file: &File, dst: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize> {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn pread(fd: std::ffi::c_int, buf: *mut std::ffi::c_void, count: usize, offset: i64) -> isize;
        }
        let at = i64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("offset {offset} does not fit in off_t")))?;
        let read = unsafe { pread(file.as_raw_fd(), dst.as_mut_ptr().cast(), dst.len(), at) };
        usize::try_from(read).map_err(|_| io::Error::last_os_error())
    }
}

struct Buffer {
    ptr: NonNull<u8>,
    cap: usize,
    len: usize,
}

unsafe impl Send for Buffer {}

impl Buffer {
    fn new(cap: usize) -> io::Result<Buffer> {
        let layout = Layout::from_size_align(cap, ALIGN).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("a buffer of {cap} bytes: {e}")))?;
        if cap == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a buffer of 0 bytes"));
        }
        let ptr = NonNull::new(unsafe { std::alloc::alloc(layout) }).ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, format!("no memory for a buffer of {cap} bytes")))?;
        Ok(Buffer { ptr, cap, len: 0 })
    }

    fn read_to(&mut self, end: usize, read: impl FnOnce(&mut [MaybeUninit<u8>]) -> io::Result<usize>) -> io::Result<usize> {
        assert!(self.len <= end && end <= self.cap);
        let spare = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.len).cast::<MaybeUninit<u8>>(), end - self.len) };
        let n = read(spare)?;
        if n > end - self.len {
            return Err(io::Error::other(format!("a read into {} bytes reported {n}", end - self.len)));
        }
        self.len += n;
        Ok(n)
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.cap, ALIGN)) };
    }
}

struct Queue<T> {
    items: Mutex<(VecDeque<T>, bool)>,
    ready: Condvar,
}

impl<T> Queue<T> {
    fn new() -> Queue<T> {
        Queue { items: Mutex::new((VecDeque::new(), false)), ready: Condvar::new() }
    }

    fn push(&self, item: T) {
        let mut items = self.items.lock().unwrap();
        if !items.1 {
            items.0.push_back(item);
            self.ready.notify_one();
        }
    }

    fn try_pop(&self) -> Option<T> {
        self.items.lock().unwrap().0.pop_front()
    }

    fn pop(&self) -> Option<T> {
        let mut items = self.items.lock().unwrap();
        loop {
            if let Some(item) = items.0.pop_front() {
                return Some(item);
            }
            if items.1 {
                return None;
            }
            items = self.ready.wait(items).unwrap();
        }
    }

    fn close(&self) {
        self.items.lock().unwrap().1 = true;
        self.ready.notify_all();
    }
}

struct Shutdown<'a> {
    stop: &'a AtomicBool,
    free: &'a Queue<Buffer>,
    full: &'a Queue<(usize, Buffer)>,
}

impl Drop for Shutdown<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.stop.store(true, Ordering::Relaxed);
            self.free.close();
            self.full.close();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    file: usize,
    offset: u64,
    want: usize,
}

fn plan(sizes: &[u64], buffer: usize) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for (file, &size) in sizes.iter().enumerate() {
        let mut offset = 0;
        while offset < size {
            let want = (size - offset).min(buffer as u64) as usize;
            pieces.push(Piece { file, offset, want });
            offset += want as u64;
        }
    }
    pieces
}

fn fill<D: Disk>(disk: &D, file: &D::File, buf: &mut Buffer, piece: Piece) -> io::Result<()> {
    let request = piece.want.next_multiple_of(ALIGN);
    if request > buf.cap {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a read of {request} bytes into a buffer of {} bytes", buf.cap)));
    }
    buf.len = 0;
    while buf.len < piece.want {
        let at = piece.offset + buf.len as u64;
        match buf.read_to(request, |dst| disk.read_at(file, dst, at)) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("the file ended at offset {at}, {} bytes short", piece.want - buf.len))),
            Ok(_) if buf.len < piece.want && buf.len % ALIGN != 0 => return Err(io::Error::other(format!("a short read ended at unaligned offset {}", piece.offset + buf.len as u64))),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub struct Edge {
    head: Vec<u8>,
    newline: bool,
    tail: Vec<u8>,
}

pub fn cut(data: &[u8]) -> (Edge, usize) {
    match (memchr(b'\n', data), memrchr(b'\n', data)) {
        (Some(first), Some(last)) => (Edge { head: data[..=first].to_vec(), newline: true, tail: data[last + 1..].to_vec() }, last + 1),
        _ => (Edge { head: data.to_vec(), newline: false, tail: Vec::new() }, 0),
    }
}

pub fn seams<'a>(edges: impl IntoIterator<Item = &'a Edge>, mut line: impl FnMut(&[u8])) {
    let mut seam = vec![b'\n'];
    let mut header = true;
    for edge in edges {
        seam.extend_from_slice(&edge.head);
        if edge.newline {
            if !header {
                line(&seam);
            }
            header = false;
            seam.truncate(1);
            seam.extend_from_slice(&edge.tail);
        }
    }
    if !header && seam.len() > 1 {
        line(&seam);
    }
}

fn samples(pages: usize) -> impl Iterator<Item = usize> {
    let count = pages.div_ceil(WINDOW).min(SAMPLES);
    let last = pages.saturating_sub(WINDOW);
    (0..count).map(move |i| if count > 1 { i * last / (count - 1) } else { 0 })
}

fn mostly_resident<D: Disk>(disk: &D, path: &Path, map: &[u8], mut probe: impl FnMut(u64) -> io::Result<bool>) -> io::Result<bool> {
    let total = map.len().div_ceil(PAGE);
    let mut pages = [0u8; WINDOW];
    let (mut seen, mut resident, mut next) = (0, 0, 0);
    let mut starts = Vec::with_capacity(SAMPLES);
    for first in samples(total) {
        let from = first.max(next);
        let to = total.min(from + WINDOW);
        if from >= to {
            continue;
        }
        let window = &map[from * PAGE..map.len().min(to * PAGE)];
        disk.incore(path, window, &mut pages[..to - from])?;
        seen += to - from;
        resident += pages[..to - from].iter().filter(|&&page| page & 1 == 1).count();
        starts.push(from);
        next = to;
    }
    if resident < seen {
        return Ok(2 * resident > seen);
    }
    let mut cached = 0;
    for (probed, &from) in starts.iter().enumerate() {
        if 2 * cached > starts.len() || 2 * (probed - cached) >= starts.len() {
            break;
        }
        cached += usize::from(probe((from * PAGE) as u64)?);
    }
    Ok(2 * cached > starts.len())
}

struct Cold<'a, F> {
    path: &'a Path,
    file: F,
    size: u64,
}

pub fn try_cold<S, I, W, D>(disk: &D, paths: &[PathBuf], threads: usize, chunk: usize, buffer: usize, init: &I, work: &W) -> Option<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
    D: Disk,
{
    match read_cold(disk, paths, threads, chunk, buffer, init, work) {
        Ok(states) => states,
        Err(error) => {
            eprintln!("  scan: direct reads failed ({error}); reading {} files through their maps instead", paths.len());
            None
        }
    }
}

fn read_cold<S, I, W, D>(disk: &D, paths: &[PathBuf], threads: usize, chunk: usize, buffer: usize, init: &I, work: &W) -> io::Result<Option<Vec<S>>>
where
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
    D: Disk,
{
    if threads == 0 {
        return Ok(None);
    }
    if buffer == 0 || buffer % ALIGN != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a read size of {buffer} bytes is not a positive multiple of {ALIGN}")));
    }
    let clock = Instant::now();
    let named = |path: &Path, e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let mut warm = Vec::new();
    let mut cold = Vec::new();
    for path in paths {
        let file = File::open(path).map_err(|e| named(path, e))?;
        let size = file.metadata().map_err(|e| named(path, e))?.len();
        if size == 0 {
            continue;
        }
        let map = unsafe { MmapOptions::new().map(&file) }.map_err(|e| named(path, e))?;
        if mostly_resident(disk, path, &map, |offset| disk.cached(path, &file, offset)).map_err(|e| named(path, e))? {
            warm.push(map);
        } else {
            let file = disk.open(path).map_err(|e| named(path, e))?;
            cold.push(Cold { path: path.as_path(), file, size });
        }
    }
    if cold.is_empty() {
        return Ok(None);
    }
    let mapped = clock.elapsed();
    let maps: Vec<&[u8]> = warm.iter().map(|map| &map[..]).collect();
    let states = read_all(disk, &cold, &maps, chunk, buffer, threads, init, work)?;
    drop(maps);
    std::mem::forget(warm);
    eprintln!("  scan: {} files, {} read with O_DIRECT, map {:.3} s, parse {:.3} s", paths.len(), cold.len(), mapped.as_secs_f64(), (clock.elapsed() - mapped).as_secs_f64());
    Ok(Some(states))
}

#[allow(clippy::too_many_arguments)]
fn read_all<S, I, W, D>(disk: &D, cold: &[Cold<D::File>], maps: &[&[u8]], chunk: usize, buffer: usize, threads: usize, init: &I, work: &W) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
    D: Disk,
{
    let sizes: Vec<u64> = cold.iter().map(|c| c.size).collect();
    let pieces = plan(&sizes, buffer);
    let mut tasks = Vec::new();
    for (index, map) in maps.iter().enumerate() {
        let mut start = 0;
        while start < map.len() {
            tasks.push((index, start, (start + chunk).min(map.len())));
            start += chunk;
        }
    }
    let free = Queue::new();
    for _ in 0..(READERS + 2 * threads).min(pieces.len()) {
        free.push(Buffer::new(buffer)?);
    }
    let full: Queue<(usize, Buffer)> = Queue::new();
    let next_task = AtomicUsize::new(0);
    let next_piece = AtomicUsize::new(0);
    let kept = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let error = Mutex::new(None);
    let fail = |e: io::Error| {
        let mut first = error.lock().unwrap();
        if first.is_none() {
            *first = Some(e);
        }
        drop(first);
        stop.store(true, Ordering::Relaxed);
        free.close();
    };
    let parse = |state: &mut S, edges: &mut Vec<(usize, Edge)>, (p, buf): (usize, Buffer)| {
        if !stop.load(Ordering::Relaxed) {
            let data = &buf.bytes()[..pieces[p].want];
            let (edge, body) = cut(data);
            if body > 0 {
                work(state, data, 0, body);
            }
            let size = edge.head.len() + edge.tail.len();
            if kept.fetch_add(size, Ordering::Relaxed) + size > EDGE_BUFFERS * buffer {
                fail(io::Error::other(format!("lines across buffer edges hold more than {} bytes", EDGE_BUFFERS * buffer)));
            }
            edges.push((p, edge));
        }
        free.push(buf);
    };
    let worker = || {
        let _shutdown = Shutdown { stop: &stop, free: &free, full: &full };
        let mut state = init();
        let mut edges = Vec::new();
        loop {
            if let Some(item) = full.try_pop() {
                parse(&mut state, &mut edges, item);
                continue;
            }
            if !stop.load(Ordering::Relaxed) {
                if let Some(&(index, start, end)) = tasks.get(next_task.fetch_add(1, Ordering::Relaxed)) {
                    work(&mut state, maps[index], start, end);
                    continue;
                }
            }
            match full.pop() {
                Some(item) => parse(&mut state, &mut edges, item),
                None => break,
            }
        }
        (state, edges)
    };
    let reader = || {
        let _shutdown = Shutdown { stop: &stop, free: &free, full: &full };
        while !stop.load(Ordering::Relaxed) {
            let p = next_piece.fetch_add(1, Ordering::Relaxed);
            let Some(&piece) = pieces.get(p) else {
                break;
            };
            let Some(mut buf) = free.pop() else {
                break;
            };
            let source = &cold[piece.file];
            match fill(disk, &source.file, &mut buf, piece) {
                Ok(()) => full.push((p, buf)),
                Err(e) => {
                    fail(io::Error::new(e.kind(), format!("{} at offset {}: {e}", source.path.display(), piece.offset)));
                    break;
                }
            }
        }
    };
    let results: Vec<(S, Vec<(usize, Edge)>)> = std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            match Builder::new().spawn_scoped(scope, &worker) {
                Ok(handle) => workers.push(handle),
                Err(e) => {
                    fail(e);
                    break;
                }
            }
        }
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS.min(pieces.len()) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match Builder::new().spawn_scoped(scope, &reader) {
                Ok(handle) => readers.push(handle),
                Err(e) => {
                    fail(e);
                    break;
                }
            }
        }
        for handle in readers {
            handle.join().unwrap();
        }
        full.close();
        workers.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
    if stop.load(Ordering::Relaxed) {
        return Err(error.lock().unwrap().take().unwrap_or_else(|| io::Error::other("direct reads stopped with no error")));
    }
    let mut states = Vec::with_capacity(results.len());
    let mut edges = Vec::with_capacity(pieces.len());
    for (state, mut part) in results {
        states.push(state);
        edges.append(&mut part);
    }
    edges.sort_unstable_by_key(|&(p, _)| p);
    if edges.len() != pieces.len() || edges.iter().enumerate().any(|(i, &(p, _))| p != i) {
        return Err(io::Error::other(format!("{} edges came back for {} pieces", edges.len(), pieces.len())));
    }
    let Some(first) = states.first_mut() else {
        return Err(io::Error::other("no worker thread ran"));
    };
    for group in edges.chunk_by(|a, b| pieces[a.0].file == pieces[b.0].file) {
        seams(group.iter().map(|(_, edge)| edge), |seam| work(&mut *first, seam, 1, seam.len()));
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_reads_every_byte_once_in_aligned_pieces() {
        let sizes = [0, 1, 700, 4095, 4096, 4097, 3 * 4096 + 17, 2 * BUFFER as u64 + 1];
        for buffer in [ALIGN, 2 * ALIGN, 3 * ALIGN, BUFFER] {
            let pieces = plan(&sizes, buffer);
            for (file, &size) in sizes.iter().enumerate() {
                let mut at = 0;
                for piece in pieces.iter().filter(|piece| piece.file == file) {
                    assert_eq!(piece.offset, at, "{piece:?}");
                    assert!(piece.offset % ALIGN as u64 == 0 && piece.want > 0 && piece.want <= buffer, "{piece:?}");
                    at += piece.want as u64;
                }
                assert_eq!(at, size, "file {file}, buffer {buffer}");
            }
            assert!(pieces.windows(2).all(|pair| (pair[0].file, pair[0].offset) < (pair[1].file, pair[1].offset)));
        }
    }

    #[test]
    fn samples_spread_over_every_page() {
        assert_eq!(samples(1).collect::<Vec<_>>(), vec![0]);
        assert_eq!(samples(WINDOW).collect::<Vec<_>>(), vec![0]);
        assert_eq!(samples(WINDOW + 1).collect::<Vec<_>>(), vec![0, 1]);
        for pages in [1, 5, WINDOW, WINDOW + 1, 2 * WINDOW, 100, WINDOW * SAMPLES - 1, WINDOW * SAMPLES, WINDOW * SAMPLES + 1, 1000, 400_000] {
            let starts: Vec<usize> = samples(pages).collect();
            assert!(!starts.is_empty() && starts.len() <= SAMPLES, "{pages} pages");
            assert_eq!(starts[0], 0);
            assert_eq!(starts[starts.len() - 1] + WINDOW.min(pages), pages, "{pages} pages");
            let gaps: Vec<usize> = starts.windows(2).map(|pair| pair[1] - pair[0]).collect();
            assert!(gaps.iter().all(|&gap| gap > 0), "{pages} pages: {starts:?}");
            if let (Some(&low), Some(&high)) = (gaps.iter().min(), gaps.iter().max()) {
                assert!(high - low <= 1, "{pages} pages: {starts:?}");
                assert!(pages > WINDOW * SAMPLES || high <= WINDOW, "{pages} pages: {starts:?}");
            }
        }
    }

    struct Marked;

    unsafe impl Disk for Marked {
        type File = ();

        fn incore(&self, _: &Path, window: &[u8], pages: &mut [u8]) -> io::Result<()> {
            for (i, page) in pages.iter_mut().enumerate() {
                *page = u8::from(window[i * PAGE] == b'r');
            }
            Ok(())
        }

        fn cached(&self, path: &Path, _: &File, offset: u64) -> io::Result<bool> {
            Err(io::Error::other(format!("{} is not probed at offset {offset} here", path.display())))
        }

        fn open(&self, path: &Path) -> io::Result<()> {
            Err(io::Error::other(format!("{} is not opened here", path.display())))
        }

        fn read_at(&self, _: &(), _: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize> {
            Err(io::Error::other(format!("no read at offset {offset} here")))
        }
    }

    struct Masked;

    unsafe impl Disk for Masked {
        type File = ();

        fn incore(&self, _: &Path, _: &[u8], pages: &mut [u8]) -> io::Result<()> {
            pages.fill(1);
            Ok(())
        }

        fn cached(&self, path: &Path, file: &File, offset: u64) -> io::Result<bool> {
            Marked.cached(path, file, offset)
        }

        fn open(&self, path: &Path) -> io::Result<()> {
            Marked.open(path)
        }

        fn read_at(&self, file: &(), dst: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize> {
            Marked.read_at(file, dst, offset)
        }
    }

    fn marked(resident: &[bool]) -> Vec<u8> {
        resident.iter().flat_map(|&page| std::iter::repeat(if page { b'r' } else { b'c' }).take(PAGE)).collect()
    }

    fn unprobed(offset: u64) -> io::Result<bool> {
        Err(io::Error::other(format!("a read probe at offset {offset}")))
    }

    #[test]
    fn residency_counts_each_sampled_page_once() {
        let mut ends = vec![false; WINDOW + 1];
        ends[..8].fill(true);
        assert!(!mostly_resident(&Marked, Path::new("ends.csv"), &marked(&ends), unprobed).unwrap());
        ends[WINDOW] = true;
        assert!(mostly_resident(&Marked, Path::new("ends.csv"), &marked(&ends), unprobed).unwrap());
        let pages = 1000;
        for (resident, want) in [(0, false), (400, false), (600, true), (pages, true)] {
            let map = marked(&(0..pages).map(|page| page < resident).collect::<Vec<_>>());
            let probe = |offset: u64| if resident < pages { unprobed(offset) } else { Ok(map[offset as usize] == b'r') };
            assert_eq!(mostly_resident(&Marked, Path::new("big.csv"), &map, probe).unwrap(), want, "{resident} of {pages} pages resident");
        }
    }

    #[test]
    fn a_masked_residency_report_is_settled_by_read_probes() {
        let pages = 1000;
        for (cached, want, expect) in [(0, false, 8), (400, false, 15), (600, true, 9), (pages, true, 9)] {
            let map = marked(&(0..pages).map(|page| page < cached).collect::<Vec<_>>());
            let mut probes = 0;
            let probe = |offset: u64| {
                probes += 1;
                Ok(map[offset as usize] == b'r')
            };
            assert_eq!(mostly_resident(&Masked, Path::new("masked.csv"), &map, probe).unwrap(), want, "{cached} of {pages} pages cached");
            assert_eq!(probes, expect, "{cached} of {pages} pages cached");
        }
        let map = marked(&[true; 3]);
        assert!(mostly_resident(&Masked, Path::new("small.csv"), &map, unprobed).is_err());
        assert!(!mostly_resident(&Masked, Path::new("small.csv"), &map, |_| Ok(false)).unwrap());
    }

    #[test]
    fn queue_gives_back_every_item_then_ends() {
        let queue = Queue::new();
        std::thread::scope(|scope| {
            let takers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        let mut got = Vec::new();
                        while let Some(item) = queue.pop() {
                            got.push(item);
                        }
                        got
                    })
                })
                .collect();
            for item in 0..1000 {
                queue.push(item);
            }
            queue.close();
            queue.push(5000);
            let mut all: Vec<i32> = takers.into_iter().flat_map(|taker| taker.join().unwrap()).collect();
            all.sort_unstable();
            assert_eq!(all, (0..1000).collect::<Vec<_>>());
        });
    }
}
