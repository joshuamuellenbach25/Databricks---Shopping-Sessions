use std::fs::File;
use std::io;
use std::ops::{BitAnd, BitOr, BitXor, Not, Shl, Shr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use memchr::memchr;
use memmap2::{Mmap, MmapOptions};

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

struct Input {
    path: PathBuf,
    len: usize,
    map: OnceLock<io::Result<Mmap>>,
}

fn run_tasks<S, I, W>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, work: W) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    let clock = std::time::Instant::now();
    let mut inputs = Vec::with_capacity(paths.len());
    let mut tasks = Vec::new();
    for path in paths {
        let len = std::fs::metadata(path)?.len() as usize;
        if len == 0 {
            File::open(path)?;
            continue;
        }
        let mut start = 0;
        while start < len {
            tasks.push((inputs.len(), start, (start + chunk).min(len)));
            start += chunk;
        }
        inputs.push(Input { path: path.clone(), len, map: OnceLock::new() });
    }
    let mapped = clock.elapsed();
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<io::Error>> = Mutex::new(None);
    let states = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut state = init();
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(file, start, end)) = tasks.get(t) else {
                            break;
                        };
                        let input = &inputs[file];
                        match input.map.get_or_init(|| File::open(&input.path).and_then(|file| unsafe { MmapOptions::new().map(&file) })) {
                            Ok(map) if map.len() == input.len => work(&mut state, map, start, end),
                            Ok(map) => {
                                let message = format!("{}: {} bytes when the scan started, {} when mapped", input.path.display(), input.len, map.len());
                                failed.lock().unwrap().get_or_insert(io::Error::other(message));
                                break;
                            }
                            Err(error) => {
                                failed.lock().unwrap().get_or_insert(io::Error::new(error.kind(), format!("{}: {error}", input.path.display())));
                                break;
                            }
                        }
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<S>>()
    });
    if let Some(error) = failed.into_inner().unwrap() {
        return Err(error);
    }
    std::mem::forget(inputs);
    eprintln!("  scan: {} files, map {:.3} s, parse {:.3} s", paths.len(), mapped.as_secs_f64(), (clock.elapsed() - mapped).as_secs_f64());
    Ok(states)
}

#[cfg(test)]
pub fn run<S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, line: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[u8]) + Sync,
{
    run_tasks(paths, threads, chunk, init, |state, data, start, end| lines(data, start, end, state, &line))
}

pub fn run_fields<const N: usize, S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, f: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[&[u8]; N]) + Sync,
{
    run_tasks(paths, threads, chunk, init, |state, data, start, end| fields::<N, S>(data, start, end, state, &f))
}

const EVENT_FIELDS: usize = 7;
const EVENT_BATCH: usize = 16;
const SEP_SLOTS: usize = 64 * EVENT_BATCH + 16;
const NEWLINE_SLOTS: usize = 64 * EVENT_BATCH + 4;

pub fn run_events<S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, f: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[&[u8]; EVENT_FIELDS]) + Sync,
{
    run_tasks(paths, threads, chunk, init, |state, data, start, end| events(data, start, end, state, &f))
}

fn events<'a, S>(data: &'a [u8], start: usize, end: usize, state: &mut S, f: &impl Fn(&mut S, &[&[u8]; EVENT_FIELDS])) {
    let Some(mut at) = first_line(data, start) else {
        return;
    };
    if at < end {
        at = event_blocks(data, at, end, state, f);
    }
    let mut out: [&'a [u8]; EVENT_FIELDS] = [&[]; EVENT_FIELDS];
    while at < end {
        let (len, ok) = split_line(data, at, &mut out);
        if ok {
            f(state, &out);
        }
        at += len + 1;
    }
}

#[inline(always)]
fn event_blocks<'a, S>(data: &'a [u8], mut line: usize, end: usize, state: &mut S, f: &impl Fn(&mut S, &[&[u8]; EVENT_FIELDS])) -> usize {
    let mut seps = [0usize; SEP_SLOTS];
    let mut newlines = [0usize; NEWLINE_SLOTS];
    let mut wrong = [0usize; 64 * EVENT_BATCH];
    let mut out: [&'a [u8]; EVENT_FIELDS] = [&[]; EVENT_FIELDS];
    let base = data.as_ptr();
    let mut block = line;
    let (mut sep_carry, mut open_carry, mut close_carry) = (1u64, 0u64, 0u64);
    let mut kept = 0;
    let mut broken = false;
    while block + 64 <= data.len() {
        let (mut count, mut lines, mut faults, mut blocks) = (kept, 0, 0, 0);
        while blocks < EVENT_BATCH && block + 64 <= data.len() {
            let m = unsafe { masks(base.add(block)) };
            let sep = m.comma | m.newline;
            let next = data.get(block + 64).map_or(1, |&c| u64::from(c == b',' || c == b'\n'));
            let after = (sep << 1) | sep_carry;
            let before = (sep >> 1) | (next << 63);
            let (sum, low) = (!sep).overflowing_add(m.quote & after);
            let (sum, high) = sum.overflowing_add(open_carry);
            let close = m.quote & before;
            let mut bad = (m.quote & !(after ^ before)) | ((sum ^ ((close << 1) | close_carry)) & sep);
            open_carry = u64::from(low | high);
            close_carry = close >> 63;
            sep_carry = sep >> 63;
            while bad != 0 {
                wrong[faults] = block + bad.trailing_zeros() as usize;
                faults += 1;
                bad &= bad - 1;
            }
            let mut nl = m.newline;
            let found = nl.count_ones() as usize;
            for k in 0..2 {
                unsafe { *newlines.get_unchecked_mut(lines + k) = block + nl.trailing_zeros() as usize };
                nl &= nl.wrapping_sub(1);
            }
            let mut k = lines + 2;
            while nl != 0 {
                newlines[k] = block + nl.trailing_zeros() as usize;
                k += 1;
                nl &= nl - 1;
            }
            lines += found;
            let mut s = sep;
            let found = sep.count_ones() as usize;
            for k in 0..8 {
                unsafe { *seps.get_unchecked_mut(count + k) = block + s.trailing_zeros() as usize };
                s &= s.wrapping_sub(1);
            }
            let mut k = count + 8;
            while s != 0 {
                seps[k] = block + s.trailing_zeros() as usize;
                k += 1;
                s &= s - 1;
            }
            count += found;
            block += 64;
            blocks += 1;
        }
        let (mut j, mut w) = (0, 0);
        for &nl in &newlines[..lines] {
            let mut clean = !broken;
            while w < faults && wrong[w] <= nl {
                clean = false;
                w += 1;
            }
            if clean && j + EVENT_FIELDS <= count && seps[j + EVENT_FIELDS - 1] == nl {
                let mut from = line;
                for (k, field) in out.iter_mut().enumerate() {
                    unsafe {
                        let to = *seps.get_unchecked(j + k);
                        let quoted = usize::from(*base.add(from) == b'"');
                        *field = std::slice::from_raw_parts(base.add(from + quoted), to - from - 2 * quoted);
                        from = to + 1;
                    }
                }
                f(state, &out);
                j += EVENT_FIELDS;
            } else {
                while seps[j] != nl {
                    j += 1;
                }
                j += 1;
            }
            line = nl + 1;
            broken = false;
            if line >= end {
                return line;
            }
        }
        broken |= w < faults;
        let left = count - j;
        if left >= EVENT_FIELDS {
            broken = true;
            kept = 0;
        } else {
            seps.copy_within(j..count, 0);
            kept = left;
        }
    }
    line
}

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

#[derive(Clone, Copy)]
struct Masks {
    comma: u64,
    quote: u64,
    newline: u64,
}

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
    let base = unsafe { data.as_ptr().add(at) };
    let mut rest = commas;
    let mut start = 0;
    for (i, field) in out.iter_mut().enumerate() {
        let stop = if i + 1 < N { rest.trailing_zeros() as usize } else { len as usize };
        rest = rest & rest.wrapping_add(!M::ZERO);
        let quoted = usize::from(unsafe { *base.add(start) } == b'"');
        debug_assert!(start + 2 * quoted <= stop && stop <= len as usize);
        *field = unsafe { std::slice::from_raw_parts(base.add(start + quoted), stop - start - 2 * quoted) };
        start = stop + 1;
    }
    true
}

#[inline(always)]
fn split_line<'a, const N: usize>(data: &'a [u8], at: usize, out: &mut [&'a [u8]; N]) -> (usize, bool) {
    if at + 128 <= data.len() {
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

    type Rows = Vec<Vec<Vec<u8>>>;

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

    fn push_event(rows: &mut Rows, f: &[&[u8]; EVENT_FIELDS]) {
        rows.push(f.iter().map(|v| v.to_vec()).collect());
    }

    fn events_tokenized(text: &[u8], chunk: usize) -> Rows {
        let mut rows = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let end = text.len().min(start.saturating_add(chunk));
            events(text, start, end, &mut rows, &push_event);
            start = end;
        }
        rows
    }

    fn events_agree(text: &[u8], label: &str) {
        let want = expected::<EVENT_FIELDS>(text);
        for chunk in [1, 2, 7, 63, 64, 65, 127, 128, 129, 1000, 1024, 1025, 4096, usize::MAX] {
            let got = events_tokenized(text, chunk);
            if got != want {
                let row = got.iter().zip(&want).position(|(g, w)| g != w).unwrap_or(got.len().min(want.len()));
                panic!("events chunk {chunk} {label}: {} rows, want {}; row {row} is {:?}, want {:?}", got.len(), want.len(), show(got.get(row)), show(want.get(row)));
            }
        }
    }

    #[test]
    fn event_lines_of_every_length_agree_with_the_dialect() {
        let mut text = b"header\n".to_vec();
        for len in 0..=300 {
            for shape in SHAPES {
                let Some(line) = shaped(EVENT_FIELDS, len, shape) else {
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
                events_agree(&last, &format!("last line of {len} bytes"));
            }
        }
        let mut tail = text.clone();
        tail.extend_from_slice(&[b'x'; 200]);
        events_agree(&tail, "lines of every length");
        for skip in [1, 13, 64, 65, 700] {
            events_agree(&[b"h\n".as_slice(), &text[skip..]].concat(), &format!("lines of every length from byte {skip}"));
        }
    }

    #[test]
    fn event_lines_with_a_quote_at_every_position_agree_with_the_dialect() {
        for len in [0, 1, 2, 3, 5, 9, 12, 30, 61, 62, 63, 64, 65, 66, 100, 125, 126, 127, 128, 129, 130] {
            for shape in SHAPES {
                let Some(line) = shaped(EVENT_FIELDS, len, shape) else {
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
                events_agree(&text, &format!("quotes in lines of {len} bytes"));
                if len <= 30 {
                    for v in &variants {
                        events_agree(&[b"header\n".as_slice(), v].concat(), &format!("quotes in a last line of {len} bytes"));
                    }
                }
            }
        }
    }

    #[test]
    fn random_event_lines_agree_with_the_dialect() {
        for seed in 1..=40 {
            let mut rng = Rng(seed);
            let mut text: Vec<u8> = b"header\n".to_vec();
            for _ in 0..3000 {
                text.extend_from_slice(&random_line(&mut rng, EVENT_FIELDS));
                text.push(b'\n');
            }
            match seed % 5 {
                0 => text.extend_from_slice(b"\"q\",tail"),
                1 => text.extend_from_slice(&shaped(EVENT_FIELDS, 140 + seed as usize, SHAPES[2]).unwrap()),
                2 => text.extend_from_slice(&random_line(&mut rng, EVENT_FIELDS)),
                3 => {}
                _ => {
                    text.extend_from_slice(&random_line(&mut rng, EVENT_FIELDS));
                    text.push(b'"');
                }
            }
            events_agree(&text, &format!("seed {seed}"));
        }
    }

    fn event_like(rng: &mut Rng, i: usize) -> Vec<u8> {
        let ts = match rng.below(3) {
            0 => format!("2026-10-{:02}T{:02}:{:02}:{:02}Z", 1 + rng.below(14), rng.below(24), rng.below(60), rng.below(60)),
            1 => format!("2026-10-{:02}T{:02}:{:02}:{:02}+05:30", 1 + rng.below(14), rng.below(24), rng.below(60), rng.below(60)),
            _ => format!("{}", 1_791_071_890_000u64 + rng.next() % 1_000_000_000),
        };
        let kind = ["view", "cart", "purchase", "Purchase", "cart "][rng.below(5)];
        let fields = [format!("e{i:08x}"), format!("u{:06}", rng.below(5000)), ts, "2026-10-03T23:58:10Z".to_string(), kind.to_string(), format!("p{}", rng.below(50_000)), format!("{}", rng.below(20_000))];
        let mut parts: Vec<Vec<u8>> = fields.iter().map(|f| if rng.below(5) == 0 { format!("\"{f}\"").into_bytes() } else { f.clone().into_bytes() }).collect();
        match rng.below(40) {
            0 => parts[rng.below(EVENT_FIELDS)].insert(0, b'"'),
            1 => parts.push(b"extra".to_vec()),
            2 => {
                parts.pop();
            }
            3 => parts[rng.below(EVENT_FIELDS)].push(b'\r'),
            4 => parts[rng.below(EVENT_FIELDS)].clear(),
            _ => {}
        }
        parts.join(&b',')
    }

    #[test]
    fn long_dense_and_event_like_lines_agree_with_the_dialect() {
        let mut rng = Rng(77);
        let wide = vec![b'w'; 5000];
        let quoted = [b"\"".as_slice(), &wide, b"\""].concat();
        let mut lines: Vec<Vec<u8>> = vec![
            [wide.as_slice(), b",a,b,c,d,e,f"].concat(),
            [b"a,b,c,".as_slice(), &quoted, b",e,f,g"].concat(),
            [b"a,b,c,".as_slice(), &wide, b",e,f,g,h"].concat(),
            [b"a,b,c,".as_slice(), &wide[..2000], b"\"", &wide[..2000], b",e,f,g"].concat(),
            [b"a,b,c,\"".as_slice(), &wide, b",e,f,g"].concat(),
            [b"a,b,c,".as_slice(), &wide, b"\",e,f,g"].concat(),
            [wide.as_slice(), &wide].concat(),
            wide.iter().enumerate().map(|(i, &c)| if i % 9 == 4 { b',' } else { c }).collect(),
        ];
        for _ in 0..60 {
            lines.push(Vec::new());
            lines.push(b",,,,,,".to_vec());
            lines.push(b"\"\",\"\",\"\",\"\",\"\",\"\",\"\"".to_vec());
            lines.push(b",,,,,,,".to_vec());
            lines.push(b"\",,,,,,".to_vec());
        }
        for i in 0..4000 {
            lines.push(event_like(&mut rng, i));
        }
        for round in 0..6 {
            for i in (1..lines.len()).rev() {
                lines.swap(i, rng.below(i + 1));
            }
            let mut text = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_vec();
            for line in &lines {
                text.extend_from_slice(line);
                text.push(b'\n');
            }
            if round % 2 == 1 {
                text.pop();
            }
            let want = expected::<EVENT_FIELDS>(&text);
            assert!(want.len() > 3500, "round {round}: {} rows", want.len());
            events_agree(&text, &format!("long, dense and event-like lines, round {round}"));
        }
    }

    #[test]
    fn events_give_the_rows_of_fields_for_any_piece_start_and_end() {
        let mut rng = Rng(91);
        let mut text = b"h\n".to_vec();
        for i in 0..3000 {
            let line = if rng.below(3) == 0 { random_line(&mut rng, EVENT_FIELDS) } else { event_like(&mut rng, i) };
            text.extend_from_slice(&line);
            text.push(b'\n');
        }
        let mut compared = 0;
        for _ in 0..4000 {
            let a = rng.below(text.len() + 1);
            let b = a + rng.below(text.len() - a + 1);
            let data = &text[a..b];
            let start = rng.below(data.len() + 1);
            let end = start + rng.below(data.len() - start + 1);
            let mut want = Rows::new();
            fields(data, start, end, &mut want, &push_event);
            let mut got = Rows::new();
            events(data, start, end, &mut got, &push_event);
            assert!(got == want, "piece {a}..{b}, start {start}, end {end}: {} rows, want {}", got.len(), want.len());
            compared += want.len();
        }
        assert!(compared > 100_000, "{compared} rows compared");
    }

    fn sorted(states: Vec<Rows>) -> Rows {
        let mut rows: Rows = states.into_iter().flatten().collect();
        rows.sort();
        rows
    }

    fn event_files(dir: &Path) -> Vec<PathBuf> {
        let mut rng = Rng(31);
        let mut random = b"header\n".to_vec();
        for _ in 0..3000 {
            random.extend_from_slice(&random_line(&mut rng, EVENT_FIELDS));
            random.push(b'\n');
        }
        let mut events = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_vec();
        for i in 0..2000 {
            events.extend_from_slice(&event_like(&mut rng, i));
            events.push(b'\n');
        }
        let long = [b"h\n".as_slice(), &[b'x'; 20_000], b",a,b,c,d,e,f\n", &events[..3000], b"\n", &[b'y'; 9000], b",a,b,c,d,e,f"].concat();
        let files: [(&str, &[u8]); 9] = [
            ("a.csv", &random),
            ("b.csv", &events),
            ("c.csv", b""),
            ("d.csv", b"h\n"),
            ("e.csv", b"header"),
            ("f.csv", &long),
            ("g.csv", b"\n\n\n"),
            ("h.csv", &events[..events.len() - 1]),
            ("i.csv", b"h\n\"a\",b,c,d,e,f,g\n\"\",\"\",\"\",\"\",\"\",\"\",\"\""),
        ];
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
        csv_files(dir).unwrap()
    }

    #[test]
    fn run_events_gives_the_rows_of_run_fields() {
        let dir = temp_dir("run-events");
        let paths = event_files(&dir);
        for chunk in [1, 2, 3, 7, 63, 64, 65, 100, 1000, 4095, 4096, 4097, 1 << 20] {
            for threads in [1, 3] {
                let want = sorted(run_fields::<EVENT_FIELDS, _, _, _>(&paths, threads, chunk, Vec::new, push_event).unwrap());
                assert!(want.len() > 3500, "chunk {chunk}: {} rows", want.len());
                let got = sorted(run_events(&paths, threads, chunk, Vec::new, push_event).unwrap());
                assert!(got == want, "chunk {chunk}, threads {threads}: {} rows, want {}", got.len(), want.len());
            }
        }
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
