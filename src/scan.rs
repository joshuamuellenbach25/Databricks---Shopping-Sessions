use std::fs::File;
use std::io;
use std::ops::{BitAnd, BitOr, BitXor, Not, Shl, Shr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use memchr::{memchr, memrchr};
use memmap2::{Mmap, MmapOptions};

/// Every `*.csv` file directly in `dir`, sorted by name.
pub fn csv_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "csv") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub fn total_size(paths: &[PathBuf]) -> io::Result<u64> {
    let mut total = 0;
    for path in paths {
        total += std::fs::metadata(path)?.len();
    }
    Ok(total)
}

/// Splits every file into chunks and runs `work(state, data, start, end)` on
/// each, on `threads` workers with their own state made by `init`. The maps
/// are never unmapped: the process exits soon after, and unmapping 1.6 GB
/// costs a quarter second on some systems.
fn run_tasks<S, I, W>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, work: W) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    let clock = std::time::Instant::now();
    // On Linux each chunk is read with pread into a worker's reused buffer,
    // so the parse takes no page faults on the map; a chunk the read cannot
    // serve is parsed from the map instead.
    #[cfg(target_os = "linux")]
    let (maps, files) = map_files(paths, true, |path| File::open(path))?;
    #[cfg(target_os = "linux")]
    let pread: &ReadAt<'_> = &|file: usize, offset: usize, buf: &mut [u8]| match &files[file] {
        Some(handle) => std::os::unix::fs::FileExt::read_exact_at(handle, buf, offset as u64),
        None => Err(io::Error::from(io::ErrorKind::Unsupported)),
    };
    #[cfg(target_os = "linux")]
    let read = Some(pread);
    #[cfg(not(target_os = "linux"))]
    let (maps, _) = map_files(paths, false, |path| File::open(path))?;
    #[cfg(not(target_os = "linux"))]
    let read = None;
    let mapped = clock.elapsed();
    let (states, fallbacks) = dispatch(&maps, threads, chunk, init, work, read);
    std::mem::forget(maps);
    eprintln!(
        "  scan: {} files, map {:.3} s, parse {:.3} s, read fallbacks {}",
        paths.len(),
        mapped.as_secs_f64(),
        (clock.elapsed() - mapped).as_secs_f64(),
        fallbacks
    );
    Ok(states)
}

/// Opens and maps every nonempty file of `paths` with `open`. With `keep`, it
/// also keeps each file's handle, for positional reads, until an open fails:
/// the kept handles may hold the last free descriptors, so it then closes
/// them all, keeps no more and opens that file again. Workers parse a file
/// that kept no handle from its map.
fn map_files(paths: &[PathBuf], mut keep: bool, mut open: impl FnMut(&Path) -> io::Result<File>) -> io::Result<(Vec<Mmap>, Vec<Option<File>>)> {
    let mut maps = Vec::with_capacity(paths.len());
    let mut files: Vec<Option<File>> = Vec::with_capacity(paths.len());
    for path in paths {
        let file = match open(path) {
            Ok(file) => file,
            Err(_) if keep && !files.is_empty() => {
                keep = false;
                files.iter_mut().for_each(|kept| *kept = None);
                open(path)?
            }
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() == 0 {
            continue;
        }
        maps.push(unsafe { MmapOptions::new().map(&file)? });
        files.push(keep.then_some(file));
    }
    Ok((maps, files))
}

/// Fills all of `buf` from byte `offset` of file number `file`.
type ReadAt<'a> = dyn Fn(usize, usize, &mut [u8]) -> io::Result<()> + Sync + 'a;

/// Runs `work` on every chunk of every map, as `run_tasks` describes, and
/// returns the states and how many chunks fell back to the map although
/// `read` was given. With `read`, each worker reads the bytes a chunk needs
/// into its own buffer (see `Window::fill`) and parses those; it parses the
/// chunk from the map when that read fails or cannot hold the chunk's lines.
fn dispatch<M, S, I, W>(maps: &[M], threads: usize, chunk: usize, init: I, work: W, read: Option<&ReadAt<'_>>) -> (Vec<S>, usize)
where
    M: std::ops::Deref<Target = [u8]> + Sync,
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    let mut tasks = Vec::new();
    for (index, map) in maps.iter().enumerate() {
        let mut start = 0;
        while start < map.len() {
            tasks.push((index, start, (start + chunk).min(map.len())));
            start += chunk;
        }
    }
    let next = AtomicUsize::new(0);
    let fallbacks = AtomicUsize::new(0);
    let states = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut state = init();
                    let mut window = Window::new(WINDOW_TAIL);
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(file, start, end)) = tasks.get(t) else {
                            break;
                        };
                        if let Some(read) = read {
                            if let Some((data, from, to)) = window.fill(maps[file].len(), start, end, |offset, buf| read(file, offset, buf)) {
                                work(&mut state, data, from, to);
                                continue;
                            }
                            fallbacks.fetch_add(1, Ordering::Relaxed);
                        }
                        work(&mut state, &maps[file], start, end);
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<S>>()
    });
    (states, fallbacks.into_inner())
}

/// Bytes read past a chunk's end, to reach the `\n` of the last line the
/// chunk owns; a chunk whose last line runs further is parsed from the map.
const WINDOW_TAIL: usize = 4096;

/// A reused buffer holding the bytes one chunk's lines need.
struct Window {
    buf: Vec<u8>,
    tail: usize,
}

impl Window {
    /// A window that allocates its buffer on its first fill.
    fn new(tail: usize) -> Self {
        Window { buf: Vec::new(), tail }
    }

    /// Reads, with one `read(offset, buf)` that fills all of `buf` from that
    /// file offset, the bytes of a `len`-byte file from the byte before
    /// `start` (so `first_line` sees the same `\n`) to `tail` bytes past
    /// `end`, or to the end of the file. Returns those bytes with the chunk
    /// bounds in them when they hold every line the chunk owns, through its
    /// `\n` or the end of the file; None when the last of those lines runs
    /// further or the read fails. So the buffer never holds more than the
    /// chunk and `tail + 1` bytes, and a chunk that owns no line, such as one
    /// inside a long line, costs one read.
    fn fill<R>(&mut self, len: usize, start: usize, end: usize, read: R) -> Option<(&[u8], usize, usize)>
    where
        R: FnOnce(usize, &mut [u8]) -> io::Result<()>,
    {
        debug_assert!(start < end && end <= len);
        let from = start.saturating_sub(1);
        let to = len.min(end.saturating_add(self.tail));
        let need = to - from;
        if self.buf.len() < need {
            // Room for the longest fill of a chunk this size, so one
            // allocation serves every full chunk.
            self.buf = vec![0; end - start + self.tail + 1];
        }
        read(from, &mut self.buf[..need]).ok()?;
        let data = &self.buf[..need];
        // The chunk owns the lines that start right after a `\n` in
        // data[..owned]; the last of them ends at the first `\n` from owned on.
        let owned = end - 1 - from;
        let whole = to == len || memrchr(b'\n', &data[..owned]).is_none() || memchr(b'\n', &data[owned..]).is_some();
        whole.then_some((data, start - from, end - from))
    }
}

/// Calls `line` once for every data line of every file (the first line of
/// each file is its header and is skipped; a last line without `\n` counts).
/// Line order is unspecified.
#[cfg(test)]
pub fn run<S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, line: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[u8]) + Sync,
{
    run_tasks(paths, threads, chunk, init, |state, data, start, end| lines(data, start, end, state, &line))
}

/// Calls `f` once for every well-formed data line of every file, with its `N`
/// fields unquoted, exactly as `dialect::split` would give them. Malformed
/// lines are skipped. Line order is unspecified.
pub fn run_fields<const N: usize, S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, f: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[&[u8]; N]) + Sync,
{
    run_tasks(paths, threads, chunk, init, |state, data, start, end| fields::<N, S>(data, start, end, state, &f))
}

/// The start of the first line a chunk owns: after the header for the first
/// chunk, else the first line starting at or after `start`.
fn first_line(data: &[u8], start: usize) -> Option<usize> {
    if start == 0 {
        memchr(b'\n', data).map(|i| i + 1)
    } else if data[start - 1] == b'\n' {
        Some(start)
    } else {
        memchr(b'\n', &data[start..]).map(|i| start + i + 1)
    }
}

#[cfg(test)]
fn lines<S>(data: &[u8], start: usize, end: usize, state: &mut S, line: &impl Fn(&mut S, &[u8])) {
    let Some(mut pos) = first_line(data, start) else {
        return;
    };
    while pos < end {
        let line_end = match memchr(b'\n', &data[pos..]) {
            Some(i) => pos + i,
            None => data.len(),
        };
        line(state, &data[pos..line_end]);
        pos = line_end + 1;
    }
}

/// Bit i of each mask is set when byte i of a 64-byte block is a comma, a
/// quote or a newline.
#[derive(Clone, Copy)]
struct Masks {
    comma: u64,
    quote: u64,
    newline: u64,
}

/// The masks of the 64 bytes from `p`, which must all be readable.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> Masks {
    use std::arch::x86_64::*;
    let lo = _mm256_loadu_si256(p as *const __m256i);
    let hi = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let comma = _mm256_set1_epi8(b',' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let join = |low: i32, high: i32| u64::from(low as u32) | (u64::from(high as u32) << 32);
    Masks {
        comma: join(_mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, comma)), _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, comma))),
        quote: join(_mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, quote)), _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, quote))),
        newline: join(_mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, newline)), _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, newline))),
    }
}

/// The masks of the 64 bytes from `p`, which must all be readable.
#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> Masks {
    use std::arch::x86_64::*;
    let comma = _mm_set1_epi8(b',' as i8);
    let quote = _mm_set1_epi8(b'"' as i8);
    let newline = _mm_set1_epi8(b'\n' as i8);
    let mut m = Masks { comma: 0, quote: 0, newline: 0 };
    for i in 0..4 {
        let v = _mm_loadu_si128(p.add(16 * i) as *const __m128i);
        m.comma |= u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(v, comma)) as u16) << (16 * i);
        m.quote |= u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(v, quote)) as u16) << (16 * i);
        m.newline |= u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(v, newline)) as u16) << (16 * i);
    }
    m
}

/// The masks of the 64 bytes from `p`, which must all be readable.
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> Masks {
    let mut m = Masks { comma: 0, quote: 0, newline: 0 };
    for i in 0..64 {
        let c = *p.add(i);
        m.comma |= u64::from(c == b',') << i;
        m.quote |= u64::from(c == b'"') << i;
        m.newline |= u64::from(c == b'\n') << i;
    }
    m
}

/// The masks of one line, bit i standing for byte i of the line: `u64` for a
/// line shorter than 64 bytes, `u128` for one shorter than 128.
trait Bits: Copy + Eq + BitAnd<Output = Self> + BitOr<Output = Self> + BitXor<Output = Self> + Not<Output = Self> + Shl<u32, Output = Self> + Shr<u32, Output = Self> {
    const ZERO: Self;
    const ONE: Self;
    fn wrapping_add(self, other: Self) -> Self;
    fn count_ones(self) -> u32;
    fn trailing_zeros(self) -> u32;
}

macro_rules! bits {
    ($t:ty) => {
        impl Bits for $t {
            const ZERO: Self = 0;
            const ONE: Self = 1;

            #[inline(always)]
            fn wrapping_add(self, other: Self) -> Self {
                <$t>::wrapping_add(self, other)
            }

            #[inline(always)]
            fn count_ones(self) -> u32 {
                <$t>::count_ones(self)
            }

            #[inline(always)]
            fn trailing_zeros(self) -> u32 {
                <$t>::trailing_zeros(self)
            }
        }
    };
}

bits!(u64);
bits!(u128);

/// Splits the `len`-byte line at `data[at..]`, whose `\n` is the byte at
/// `at + len`, into `out` exactly as `dialect::split` does, from the comma
/// and quote masks of the bytes from `at` (bits past the line are ignored).
/// Returns false for a malformed line.
///
/// A quote is legal only as the first or the last byte of a field of two or
/// more bytes, and a field opens with a quote exactly when it closes with
/// one. So every quote has a separator on exactly one side (a comma, or the
/// newline before or after the line). And adding the bit of each opening
/// quote to the mask of non-separators carries through the rest of its
/// field into the separator that ends it: those separators must be exactly
/// the ones that follow a quote.
#[inline(always)]
fn split_masked<'a, M: Bits, const N: usize>(data: &'a [u8], at: usize, len: u32, comma: M, quote: M, out: &mut [&'a [u8]; N]) -> bool {
    debug_assert!(data[at + len as usize] == b'\n');
    let newline = M::ONE << len;
    let line = newline.wrapping_add(!M::ZERO);
    let commas = comma & line;
    let quotes = quote & line;
    let sep = commas | newline;
    let after_sep = (sep << 1) | M::ONE;
    let before_sep = sep >> 1;
    let stray = quotes & !(after_sep ^ before_sep);
    let opened = (!sep).wrapping_add(quotes & after_sep) & sep;
    let closed = (quotes << 1) & sep;
    if commas.count_ones() != N as u32 - 1 || (stray | (opened ^ closed)) != M::ZERO {
        return false;
    }
    // SAFETY: every byte from `at` to the line's `\n` is in `data`, and
    // each field runs from `start` to `stop`, with start <= stop <= len.
    let base = unsafe { data.as_ptr().add(at) };
    let mut rest = commas;
    let mut start = 0;
    for (i, field) in out.iter_mut().enumerate() {
        let stop = if i + 1 < N { rest.trailing_zeros() as usize } else { len as usize };
        rest = rest & rest.wrapping_add(!M::ZERO);
        // A field that opens with a quote is two or more bytes long and
        // closes with one; its value is the bytes between them.
        let quoted = usize::from(unsafe { *base.add(start) } == b'"');
        debug_assert!(start + 2 * quoted <= stop && stop <= len as usize);
        *field = unsafe { std::slice::from_raw_parts(base.add(start + quoted), stop - start - 2 * quoted) };
        start = stop + 1;
    }
    true
}

/// Splits the line that starts at `at` into `out`, returning its length
/// without the `\n` and whether it is well-formed. A line shorter than 128
/// bytes that has 128 bytes of data from its start is split from its masks;
/// longer lines and the lines near the end of the data (the last one perhaps
/// without a `\n`) go to `dialect::split`.
#[inline(always)]
fn split_line<'a, const N: usize>(data: &'a [u8], at: usize, out: &mut [&'a [u8]; N]) -> (usize, bool) {
    if at + 128 <= data.len() {
        // SAFETY: the 128 bytes from `at` are in `data`.
        let lo = unsafe { masks(data.as_ptr().add(at)) };
        if lo.newline != 0 {
            let len = lo.newline.trailing_zeros();
            return (len as usize, split_masked::<u64, N>(data, at, len, lo.comma, lo.quote, out));
        }
        let hi = unsafe { masks(data.as_ptr().add(at + 64)) };
        if hi.newline != 0 {
            let len = 64 + hi.newline.trailing_zeros();
            let comma = u128::from(lo.comma) | (u128::from(hi.comma) << 64);
            let quote = u128::from(lo.quote) | (u128::from(hi.quote) << 64);
            return (len as usize, split_masked::<u128, N>(data, at, len, comma, quote, out));
        }
    }
    let len = memchr(b'\n', &data[at..]).unwrap_or(data.len() - at);
    (len, crate::dialect::split(&data[at..at + len], out))
}

fn fields<'a, const N: usize, S>(data: &'a [u8], start: usize, end: usize, state: &mut S, f: &impl Fn(&mut S, &[&[u8]; N])) {
    let Some(mut at) = first_line(data, start) else {
        return;
    };
    let mut out: [&'a [u8]; N] = [&[]; N];
    while at < end {
        let (len, ok) = split_line(data, at, &mut out);
        if ok {
            f(state, &out);
        }
        at += len + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_line_once_whatever_the_chunking() {
        let dir = temp_dir("lines");
        let mut expected = Vec::new();
        for (name, count, trailing) in [("a.csv", 1000usize, true), ("b.csv", 0, true), ("c.csv", 7, false), ("d.csv", 0, false)] {
            let mut file = File::create(dir.join(name)).unwrap();
            if count > 0 || trailing {
                file.write_all(b"h1,h2\n").unwrap();
            }
            for i in 0..count {
                let text = format!("{name},{i}");
                expected.push(text.clone());
                let nl = if i + 1 == count && !trailing { "" } else { "\n" };
                file.write_all(format!("{text}{nl}").as_bytes()).unwrap();
            }
        }
        std::fs::write(dir.join("e.csv"), b"h1,h2\n\n\nx,y\n").unwrap();
        expected.extend(["".to_string(), "".to_string(), "x,y".to_string()]);
        expected.sort();
        let paths = csv_files(&dir).unwrap();
        for chunk in [1, 7, 100, 1 << 20] {
            let states = run(&paths, 3, chunk, Vec::new, |s: &mut Vec<String>, l| s.push(String::from_utf8(l.to_vec()).unwrap())).unwrap();
            let mut got: Vec<String> = states.into_iter().flatten().collect();
            got.sort();
            assert_eq!(got, expected, "chunk {chunk}");
        }
    }

    /// The field values of well-formed lines, one list per line.
    type Rows = Vec<Vec<Vec<u8>>>;

    /// A linear congruential generator, for repeatable test data.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// What `dialect::split` gives for every data line of `text`, in order.
    fn expected<const N: usize>(text: &[u8]) -> Rows {
        let Some(header) = memchr(b'\n', text) else {
            return Vec::new();
        };
        let body = &text[header + 1..];
        let mut lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
        if body.is_empty() || body.ends_with(b"\n") {
            lines.pop();
        }
        let mut rows = Vec::new();
        for line in lines {
            let mut out: [&[u8]; N] = [&[]; N];
            if crate::dialect::split(line, &mut out) {
                rows.push(out.iter().map(|f| f.to_vec()).collect());
            }
        }
        rows
    }

    /// What `fields` gives for `text` read in chunks of `chunk` bytes, in order.
    fn tokenized<const N: usize>(text: &[u8], chunk: usize) -> Rows {
        let mut rows = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let end = text.len().min(start.saturating_add(chunk));
            fields(text, start, end, &mut rows, &|rows: &mut Rows, f: &[&[u8]; N]| rows.push(f.iter().map(|v| v.to_vec()).collect()));
            start = end;
        }
        rows
    }

    fn show(row: Option<&Vec<Vec<u8>>>) -> Option<Vec<String>> {
        row.map(|r| r.iter().map(|v| String::from_utf8_lossy(v).into_owned()).collect())
    }

    /// Checks `fields` against `dialect::split` on `text`, with chunk ends at
    /// every byte, around 64 and 128 bytes, and nowhere.
    fn agree<const N: usize>(text: &[u8], label: &str) {
        let want = expected::<N>(text);
        for chunk in [1, 2, 7, 63, 64, 65, 127, 128, 129, usize::MAX] {
            let got = tokenized::<N>(text, chunk);
            if got != want {
                let row = got.iter().zip(&want).position(|(g, w)| g != w).unwrap_or(got.len().min(want.len()));
                panic!("N {N} chunk {chunk} {label}: {} rows, want {}; row {row} is {:?}, want {:?}", got.len(), want.len(), show(got.get(row)), show(want.get(row)));
            }
        }
    }

    const PLAIN: u8 = 0;
    const QUOTED: u8 = 1;
    const EMPTY: u8 = 2;
    const QUOTED_EMPTY: u8 = 3;
    const SHAPES: [[u8; 4]; 4] = [[PLAIN; 4], [QUOTED; 4], [QUOTED, PLAIN, EMPTY, QUOTED_EMPTY], [QUOTED_EMPTY, QUOTED, PLAIN, EMPTY]];

    /// A well-formed line of `n` fields and exactly `len` bytes, field i
    /// shaped by `shape[i % 4]`, its spare bytes spread over the plain and
    /// quoted fields. None when no such line exists.
    fn shaped(n: usize, len: usize, shape: [u8; 4]) -> Option<Vec<u8>> {
        let shapes: Vec<u8> = (0..n).map(|i| shape[i % 4]).collect();
        let quoted = |s: u8| s == QUOTED || s == QUOTED_EMPTY;
        let fixed = n - 1 + 2 * shapes.iter().filter(|&&s| quoted(s)).count();
        let holders: Vec<usize> = (0..n).filter(|&i| shapes[i] == PLAIN || shapes[i] == QUOTED).collect();
        let spare = len.checked_sub(fixed)?;
        if spare > 0 && holders.is_empty() {
            return None;
        }
        let mut sizes = vec![0; n];
        for k in 0..spare {
            sizes[holders[k % holders.len()]] += 1;
        }
        let mut line = Vec::with_capacity(len);
        for (i, &s) in shapes.iter().enumerate() {
            if i > 0 {
                line.push(b',');
            }
            if quoted(s) {
                line.push(b'"');
            }
            line.extend((0..sizes[i]).map(|k| b"ab 1\rz"[(i + k) % 6]));
            if quoted(s) {
                line.push(b'"');
            }
        }
        Some(line)
    }

    fn every_length<const N: usize>() {
        let mut text = b"header\n".to_vec();
        for len in 0..=300 {
            for shape in SHAPES {
                let Some(line) = shaped(N, len, shape) else {
                    continue;
                };
                let variants = [
                    line.clone(),
                    [b"\"".as_slice(), &line].concat(),
                    [line.as_slice(), b"\""].concat(),
                    [line.as_slice(), b","].concat(),
                    line[line.len().min(1)..].to_vec(),
                ];
                for variant in variants {
                    text.extend_from_slice(&variant);
                    text.push(b'\n');
                }
                let last = [b"header\n".as_slice(), &[b'y'; 150], b"\n", &line].concat();
                agree::<N>(&last, &format!("last line of {len} bytes"));
            }
        }
        text.extend_from_slice(&[b'x'; 200]);
        agree::<N>(&text, "lines of every length");
    }

    #[test]
    fn lines_of_every_length() {
        every_length::<1>();
        every_length::<2>();
        every_length::<4>();
        every_length::<7>();
    }

    /// Lines made from well-formed ones by putting a quote between two bytes
    /// and in place of a byte, at every position, by taking out each quote,
    /// and, for short lines, by putting in two quotes at every pair of
    /// positions.
    fn quotes_everywhere<const N: usize>() {
        for len in [0, 1, 2, 3, 5, 9, 12, 30, 61, 62, 63, 64, 65, 66, 100, 125, 126, 127, 128, 129, 130] {
            for shape in SHAPES {
                let Some(line) = shaped(N, len, shape) else {
                    continue;
                };
                let mut variants = vec![line.clone()];
                for p in 0..=line.len() {
                    let mut v = line.clone();
                    v.insert(p, b'"');
                    variants.push(v);
                    if p < line.len() {
                        let mut v = line.clone();
                        if line[p] == b'"' {
                            v.remove(p);
                        } else {
                            v[p] = b'"';
                        }
                        variants.push(v);
                    }
                    if len <= 12 {
                        for q in p..=line.len() {
                            let mut v = line.clone();
                            v.insert(q, b'"');
                            v.insert(p, b'"');
                            variants.push(v);
                        }
                    }
                }
                let mut text = b"header\n".to_vec();
                for v in &variants {
                    text.extend_from_slice(v);
                    text.push(b'\n');
                }
                text.extend_from_slice(&[b'x'; 200]);
                agree::<N>(&text, &format!("quotes in lines of {len} bytes"));
                if len <= 30 {
                    for v in &variants {
                        agree::<N>(&[b"header\n".as_slice(), v].concat(), &format!("quotes in a last line of {len} bytes"));
                    }
                }
            }
        }
    }

    #[test]
    fn a_quote_at_every_position() {
        quotes_everywhere::<1>();
        quotes_everywhere::<2>();
        quotes_everywhere::<4>();
        quotes_everywhere::<7>();
    }

    #[test]
    fn masks_mark_every_comma_quote_and_newline() {
        let mut blocks: Vec<[u8; 64]> = Vec::new();
        for value in 0..=255u8 {
            for p in 0..64 {
                let mut block = [b'x'; 64];
                block[p] = value;
                blocks.push(block);
            }
        }
        let mut rng = Rng(11);
        let alphabet = b",\"\n\r a\x80\xff";
        for _ in 0..2000 {
            let mut block = [0u8; 64];
            for b in block.iter_mut() {
                *b = alphabet[rng.below(alphabet.len())];
            }
            blocks.push(block);
        }
        for block in &blocks {
            let m = unsafe { masks(block.as_ptr()) };
            for (i, &b) in block.iter().enumerate() {
                let got = ((m.comma >> i) & 1, (m.quote >> i) & 1, (m.newline >> i) & 1);
                assert_eq!(got, (u64::from(b == b','), u64::from(b == b'"'), u64::from(b == b'\n')), "byte {b} at {i}");
            }
        }
    }

    /// A random line: noise from a small alphabet, up to 300 bytes long; a
    /// well-formed line of `n` fields with empty, quoted, short and long
    /// values; such a line with a quote or a comma put in, a byte made a
    /// quote or a byte taken out; or a line of `n` short values, some quoted.
    fn random_line(rng: &mut Rng, n: usize) -> Vec<u8> {
        let kind = rng.below(4);
        if kind == 0 {
            let len = if rng.below(4) == 0 { rng.below(300) } else { rng.below(64) };
            return (0..len).map(|_| b"ab,\"\r 1"[rng.below(7)]).collect();
        }
        if kind == 3 {
            let values: Vec<String> = (0..n).map(|i| if rng.below(2) == 1 { format!("\"v{i}\"") } else { format!("v{i}") }).collect();
            return values.join(",").into_bytes();
        }
        let mut values: Vec<Vec<u8>> = Vec::with_capacity(n);
        for _ in 0..n {
            let most = [1, 4, 16, 91][rng.below(4)];
            let size = rng.below(most);
            let value: Vec<u8> = (0..size).map(|_| b"ab 1\rz"[rng.below(6)]).collect();
            values.push(if rng.below(5) < 2 { [b"\"".as_slice(), &value, b"\""].concat() } else { value });
        }
        let mut line = values.join(&b',');
        if kind == 2 {
            let p = rng.below(line.len() + 1);
            match rng.below(4) {
                0 => line.insert(p, b'"'),
                1 => line.insert(p, b','),
                2 if p < line.len() => line[p] = b'"',
                _ if p < line.len() => {
                    line.remove(p);
                }
                _ => line.push(b'"'),
            }
        }
        line
    }

    fn check_fields<const N: usize>(dir: &Path, seed: u64) {
        let mut rng = Rng(seed);
        let mut text: Vec<u8> = b"header\n".to_vec();
        for _ in 0..3000 {
            text.extend_from_slice(&random_line(&mut rng, N));
            text.push(b'\n');
        }
        match seed % 5 {
            0 => text.extend_from_slice(b"\"q\",tail"),
            1 => text.extend_from_slice(&shaped(N, 140 + seed as usize, SHAPES[2]).unwrap()),
            2 => text.extend_from_slice(&random_line(&mut rng, N)),
            3 => {}
            _ => {
                text.extend_from_slice(&random_line(&mut rng, N));
                text.push(b'"');
            }
        }
        let file = dir.join(format!("f-{N}-{seed}.csv"));
        std::fs::write(&file, &text).unwrap();
        let mut want = expected::<N>(&text);
        want.sort();
        let paths = vec![file];
        for chunk in [1, 13, 64, 100, 127, 128, 129, 4096, 1 << 20] {
            let states = run_fields::<N, _, _, _>(&paths, 3, chunk, Vec::new, |s: &mut Rows, f| s.push(f.iter().map(|v| v.to_vec()).collect())).unwrap();
            let mut got: Rows = states.into_iter().flatten().collect();
            got.sort();
            assert!(got == want, "N {N} seed {seed} chunk {chunk}: {} rows, want {}", got.len(), want.len());
        }
        agree::<N>(&text, &format!("seed {seed}"));
    }

    /// What `fields` gives for `text` cut into chunks of `chunk` bytes, each
    /// read through a `Window` with the given tail, or taken from the whole
    /// text when the window cannot hold its lines, in order.
    fn windowed<const N: usize>(text: &[u8], chunk: usize, tail: usize) -> Rows {
        let mut window = Window::new(tail);
        let mut rows = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let end = text.len().min(start.saturating_add(chunk));
            let read = |offset: usize, buf: &mut [u8]| {
                buf.copy_from_slice(&text[offset..offset + buf.len()]);
                Ok(())
            };
            let (data, from, to) = window.fill(text.len(), start, end, read).unwrap_or((text, start, end));
            fields(data, from, to, &mut rows, &|rows: &mut Rows, f: &[&[u8]; N]| rows.push(f.iter().map(|v| v.to_vec()).collect()));
            start = end;
        }
        rows
    }

    fn check_window<const N: usize>(seed: u64) {
        let mut rng = Rng(seed);
        let mut text: Vec<u8> = b"header\n".to_vec();
        for _ in 0..1500 {
            text.extend_from_slice(&random_line(&mut rng, N));
            text.push(b'\n');
        }
        if seed % 2 == 0 {
            text.extend_from_slice(&random_line(&mut rng, N));
        }
        let want = expected::<N>(&text);
        for chunk in [1, 7, 64, 100, 128, 129, 4096, usize::MAX >> 1] {
            for tail in [0, 1, 3, 64, 4096] {
                let got = windowed::<N>(&text, chunk.min(text.len()), tail);
                assert!(got == want, "N {N} seed {seed} chunk {chunk} tail {tail}: {} rows, want {}", got.len(), want.len());
            }
        }
    }

    #[test]
    fn windows_give_the_same_fields_as_the_whole_file() {
        for seed in 1..=6 {
            check_window::<1>(seed);
            check_window::<2>(seed);
            check_window::<4>(seed);
            check_window::<7>(seed);
        }
        let mut window = Window::new(1);
        let failed = window.fill(10, 0, 4, |_, _| Err(io::Error::other("read failed")));
        assert!(failed.is_none());
    }

    #[test]
    fn a_long_line_costs_one_bounded_read_per_chunk() {
        let long = [b"a,".as_slice(), &[b'x'; 1 << 16]].concat();
        let short: Vec<u8> = (0..100).flat_map(|i| format!("s{i},t{i}\n").into_bytes()).collect();
        let (chunk, tail) = (2048, 64);
        // The long line last without its `\n`, last with it, and between short lines.
        let texts = [
            [b"h1,h2\n".as_slice(), &long].concat(),
            [b"h1,h2\n".as_slice(), &long, b"\n"].concat(),
            [b"h1,h2\n".as_slice(), &short, &long, b"\n", &short].concat(),
        ];
        for text in texts {
            let mut window = Window::new(tail);
            let (mut rows, mut reads, mut bytes, mut from_map) = (Vec::new(), 0, 0, 0);
            let mut start = 0;
            while start < text.len() {
                let end = text.len().min(start + chunk);
                let read = |offset: usize, buf: &mut [u8]| {
                    reads += 1;
                    bytes += buf.len();
                    buf.copy_from_slice(&text[offset..offset + buf.len()]);
                    Ok(())
                };
                let (data, from, to) = match window.fill(text.len(), start, end, read) {
                    Some(got) => got,
                    None => {
                        from_map += 1;
                        (text.as_slice(), start, end)
                    }
                };
                fields(data, from, to, &mut rows, &|rows: &mut Rows, f: &[&[u8]; 2]| rows.push(f.iter().map(|v| v.to_vec()).collect()));
                assert!(window.buf.len() <= chunk + tail + 1, "a window of {} bytes", window.buf.len());
                start = end;
            }
            let chunks = text.len().div_ceil(chunk);
            assert_eq!(rows, expected::<2>(&text));
            // One read per chunk; only the chunk that owns the long line is
            // parsed from the map.
            assert_eq!((reads, from_map), (chunks, 1));
            assert!(bytes <= text.len() + chunks * (tail + 1), "{bytes} bytes read for {} bytes in {chunks} chunks", text.len());
        }
    }

    #[test]
    fn dispatch_parses_every_chunk_whose_read_fails_from_the_map() {
        let mut rng = Rng(5);
        let texts: Vec<Vec<u8>> = (0..3)
            .map(|_| {
                let mut text = b"header\n".to_vec();
                for _ in 0..300 {
                    text.extend_from_slice(&random_line(&mut rng, 4));
                    text.push(b'\n');
                }
                text
            })
            .collect();
        let mut want: Rows = texts.iter().flat_map(|text| expected::<4>(text)).collect();
        want.sort();
        let chunk = 100;
        let chunks: usize = texts.iter().map(|text| text.len().div_ceil(chunk)).sum();
        let work = |rows: &mut Rows, data: &[u8], start: usize, end: usize| fields(data, start, end, rows, &|rows: &mut Rows, f: &[&[u8]; 4]| rows.push(f.iter().map(|v| v.to_vec()).collect()));
        let sorted = |states: Vec<Rows>| {
            let mut rows: Rows = states.into_iter().flatten().collect();
            rows.sort();
            rows
        };
        let (states, fallbacks) = dispatch(&texts, 3, chunk, Vec::new, &work, None);
        assert!(sorted(states) == want && fallbacks == 0);
        // Reads that all succeed, every third read failing, every read failing.
        for every in [0, 3, 1] {
            let calls = AtomicUsize::new(0);
            let failed = AtomicUsize::new(0);
            let read: &ReadAt<'_> = &|file: usize, offset: usize, buf: &mut [u8]| {
                let call = calls.fetch_add(1, Ordering::Relaxed);
                if every > 0 && call % every == 0 {
                    failed.fetch_add(1, Ordering::Relaxed);
                    return Err(io::Error::other("read failed"));
                }
                buf.copy_from_slice(&texts[file][offset..offset + buf.len()]);
                Ok(())
            };
            let (states, fallbacks) = dispatch(&texts, 3, chunk, Vec::new, &work, Some(read));
            let failed = failed.load(Ordering::Relaxed);
            assert!(sorted(states) == want, "every {every}");
            assert_eq!(calls.load(Ordering::Relaxed), chunks, "every {every}");
            assert_eq!((fallbacks, failed > 0), (failed, every > 0), "every {every}");
        }
    }

    #[test]
    fn a_failed_open_closes_the_kept_handles_and_maps_every_file() {
        let dir = temp_dir("open");
        let mut paths = Vec::new();
        for i in 0..6 {
            let path = dir.join(format!("{i}.csv"));
            std::fs::write(&path, if i == 2 { String::new() } else { format!("h\n{i}\n") }).unwrap();
            paths.push(path);
        }
        let want: Vec<String> = [0, 1, 3, 4, 5].iter().map(|i| format!("h\n{i}\n")).collect();
        let texts = |maps: &[Mmap]| -> Vec<String> { maps.iter().map(|map| String::from_utf8(map.to_vec()).unwrap()).collect() };
        let (maps, files) = map_files(&paths, true, |path| File::open(path)).unwrap();
        assert_eq!(texts(&maps), want);
        assert!(files.len() == 5 && files.iter().all(Option::is_some));
        // The fourth open fails, as it would at the descriptor limit: the two
        // kept handles close, the open runs again and every file is mapped.
        let mut opens = 0;
        let (maps, files) = map_files(&paths, true, |path| {
            opens += 1;
            if opens == 4 {
                return Err(io::Error::other("too many open files"));
            }
            File::open(path)
        })
        .unwrap();
        assert_eq!(opens, 7);
        assert_eq!(texts(&maps), want);
        assert!(files.len() == 5 && files.iter().all(Option::is_none));
        // An open that fails again after the handles close is an error.
        let failed = map_files(&paths, true, |path| if path.ends_with("3.csv") { Err(io::Error::other("unreadable")) } else { File::open(path) });
        assert!(failed.is_err());
    }

    #[test]
    fn fields_agree_with_the_dialect() {
        let dir = temp_dir("fields");
        for seed in 1..=20 {
            check_fields::<1>(&dir, seed);
            check_fields::<2>(&dir, seed);
            check_fields::<4>(&dir, seed);
            check_fields::<7>(&dir, seed);
        }
    }
}
