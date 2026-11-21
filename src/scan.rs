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
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return run_tasks_on(&crate::direct::Linux, crate::direct::BUFFER, paths, threads, chunk, init, work);
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    run_maps(paths, threads, chunk, init, work)
}

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
fn run_tasks_on<D, S, I, W>(disk: &D, buffer: usize, paths: &[PathBuf], threads: usize, chunk: usize, init: I, work: W) -> io::Result<Vec<S>>
where
    D: crate::direct::Disk,
    S: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    match crate::direct::try_cold(disk, paths, threads, chunk, buffer, &init, &work) {
        Some(states) => Ok(states),
        None => run_maps(paths, threads, chunk, init, work),
    }
}

fn run_maps<S, I, W>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, work: W) -> io::Result<Vec<S>>
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
    use crate::ids::Key;
    use std::io::Write;
    use std::mem::MaybeUninit;

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

    fn push_fields<const N: usize>(rows: &mut Rows, data: &[u8], start: usize, end: usize) {
        fields::<N, Rows>(data, start, end, rows, &|rows: &mut Rows, f: &[&[u8]; N]| rows.push(f.iter().map(|v| v.to_vec()).collect()));
    }

    fn pieced<const N: usize>(text: &[u8], piece: usize) -> Rows {
        let mut rows = Rows::new();
        let mut edges = Vec::new();
        for data in text.chunks(piece) {
            let (edge, body) = crate::direct::cut(data);
            push_fields::<N>(&mut rows, data, 0, body);
            edges.push(edge);
        }
        crate::direct::seams(&edges, |seam| push_fields::<N>(&mut rows, seam, 1, seam.len()));
        rows.sort();
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
        let mut sorted = want.clone();
        sorted.sort();
        for piece in [1, 2, 3, 7, 63, 64, 65, 127, 128, 129, 4095, 4096, 4097, usize::MAX] {
            let got = pieced::<N>(text, piece);
            assert!(got == sorted, "N {N} piece {piece} {label}: {} rows, want {}", got.len(), sorted.len());
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

    #[test]
    fn pieces_of_small_and_unaligned_files_give_every_line_once() {
        let mut rng = Rng(5);
        let mut long = b"header\n".to_vec();
        while long.len() < 3 * 4096 + 100 {
            long.extend_from_slice(&random_line(&mut rng, 4));
            long.push(b'\n');
        }
        let mut texts: Vec<Vec<u8>> = [b"".as_slice(), b"h", b"h\n", b"h\nx", b"h\n\n", b"\n", b"\n\n", b"h\n,\n\"\"\n"].iter().map(|t| t.to_vec()).collect();
        for size in [1, 2, 100, 4095, 4096, 4097, 8191, 8192, 8193, 3 * 4096 + 17] {
            texts.push(long[..size].to_vec());
        }
        for text in &texts {
            let label = format!("a file of {} bytes", text.len());
            agree::<1>(text, &label);
            agree::<2>(text, &label);
            agree::<4>(text, &label);
            agree::<7>(text, &label);
        }
    }

    struct Fake {
        warm: Vec<PathBuf>,
        masked: bool,
        refuse_probe: bool,
        step: usize,
        fail_at: Option<u64>,
        end_at: Option<u64>,
        refuse_open: bool,
    }

    impl Fake {
        fn cold(step: usize) -> Fake {
            Fake { warm: Vec::new(), masked: false, refuse_probe: false, step, fail_at: None, end_at: None, refuse_open: false }
        }
    }

    unsafe impl crate::direct::Disk for Fake {
        type File = Vec<u8>;

        fn incore(&self, path: &Path, _: &[u8], pages: &mut [u8]) -> io::Result<()> {
            pages.fill(u8::from(self.masked || self.warm.iter().any(|warm| warm == path)));
            Ok(())
        }

        fn cached(&self, path: &Path, _: &File, offset: u64) -> io::Result<bool> {
            if self.refuse_probe {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!("no read probe at offset {offset}")));
            }
            Ok(self.warm.iter().any(|warm| warm == path))
        }

        fn open(&self, path: &Path) -> io::Result<Vec<u8>> {
            if self.refuse_open {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "direct reads refused"));
            }
            std::fs::read(path)
        }

        fn read_at(&self, file: &Vec<u8>, dst: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize> {
            let align = crate::direct::ALIGN;
            if dst.as_ptr() as usize % align != 0 || dst.len() % align != 0 || offset % align as u64 != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unaligned read of {} bytes at offset {offset}", dst.len())));
            }
            if self.fail_at == Some(offset) {
                return Err(io::Error::other(format!("no read at offset {offset}")));
            }
            let end = self.end_at.map_or(file.len(), |end| file.len().min(end as usize));
            let start = end.min(offset as usize);
            let n = dst.len().min(self.step).min(end - start);
            for (slot, &byte) in dst.iter_mut().zip(&file[start..start + n]) {
                slot.write(byte);
            }
            Ok(n)
        }
    }

    fn cold_files(dir: &Path) -> Vec<PathBuf> {
        let mut rng = Rng(23);
        let mut text = b"click_id,user_id,campaign_id,ts\n".to_vec();
        while text.len() < 40_000 {
            text.extend_from_slice(&random_line(&mut rng, 4));
            text.push(b'\n');
        }
        let sizes = [0, 1, 700, 4095, 4096, 4097, 3 * 4096 + 17, text.len()];
        for (i, size) in sizes.into_iter().enumerate() {
            std::fs::write(dir.join(format!("part-{i}.csv")), &text[..size]).unwrap();
        }
        let long = [b"h\n".as_slice(), &[b'x'; 30_000], b",y,z,w\nq,w,e,r\n\"a\",b,c,d"].concat();
        std::fs::write(dir.join("part-long.csv"), long).unwrap();
        csv_files(dir).unwrap()
    }

    fn rows_of(states: Vec<Rows>) -> Rows {
        let mut rows: Rows = states.into_iter().flatten().collect();
        rows.sort();
        rows
    }

    #[test]
    fn cold_reads_give_the_rows_the_maps_give() {
        let dir = temp_dir("cold");
        let paths = cold_files(&dir);
        let want = rows_of(run_maps(&paths, 2, 100, Vec::new, push_fields::<4>).unwrap());
        let mut each: Rows = paths.iter().flat_map(|path| expected::<4>(&std::fs::read(path).unwrap())).collect();
        each.sort();
        assert!(want == each && want.len() > 100, "{} rows from the maps, {} from the dialect", want.len(), each.len());
        assert!(want.iter().any(|row| row[0].len() == 30_000));
        let every_other: Vec<PathBuf> = paths.iter().step_by(2).cloned().collect();
        for buffer in [4096, 8192, 3 * 4096] {
            for step in [4096, 8192, usize::MAX] {
                for threads in [1, 3] {
                    for warm in [Vec::new(), every_other.clone()] {
                        for masked in [false, true] {
                            let label = format!("buffer {buffer}, step {step}, threads {threads}, {} warm, masked {masked}", warm.len());
                            let fake = Fake { warm: warm.clone(), masked, ..Fake::cold(step) };
                            let states = crate::direct::try_cold(&fake, &paths, threads, 100, buffer, &Vec::new, &push_fields::<4>).unwrap_or_else(|| panic!("{label}: no cold read"));
                            assert_eq!(states.len(), threads, "{label}");
                            let got = rows_of(states);
                            assert!(got == want, "{label}: {} rows, want {}", got.len(), want.len());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cold_reads_refuse_every_failure() {
        let dir = temp_dir("refuse");
        let paths = cold_files(&dir);
        let cases = [
            ("every file warm", Fake { warm: paths.clone(), ..Fake::cold(usize::MAX) }),
            ("every file warm behind a masked report", Fake { warm: paths.clone(), masked: true, ..Fake::cold(usize::MAX) }),
            ("a read probe refused", Fake { masked: true, refuse_probe: true, ..Fake::cold(usize::MAX) }),
            ("a read error", Fake { fail_at: Some(4096), ..Fake::cold(usize::MAX) }),
            ("a file shorter than its size", Fake { end_at: Some(4096), ..Fake::cold(usize::MAX) }),
            ("an unaligned short read", Fake::cold(1000)),
            ("an open refused", Fake { refuse_open: true, ..Fake::cold(usize::MAX) }),
        ];
        for (label, fake) in &cases {
            assert!(crate::direct::try_cold(fake, &paths, 2, 100, 4096, &Vec::new, &push_fields::<4>).is_none(), "{label}");
        }
        assert!(crate::direct::try_cold(&Fake::cold(usize::MAX), &paths, 2, 100, 4095, &Vec::new, &push_fields::<4>).is_none(), "a read size off the alignment");
        assert!(crate::direct::try_cold(&Fake::cold(usize::MAX), &paths, 2, 100, 4096, &Vec::new, &push_fields::<4>).is_some(), "no failure");
    }

    #[test]
    fn cold_reads_refuse_edges_past_their_budget() {
        let dir = temp_dir("budget");
        std::fs::write(dir.join("part-wide.csv"), [b"h\n".as_slice(), &[b'x'; 100_000], b",y,z,w\n"].concat()).unwrap();
        let paths = cold_files(&dir);
        let want = rows_of(run_fields::<4, _, _, _>(&paths, 2, 100, Vec::new, |s: &mut Rows, f| s.push(f.iter().map(|v| v.to_vec()).collect())).unwrap());
        assert!(want.iter().any(|row| row[0].len() == 100_000));
        assert!(crate::direct::try_cold(&Fake::cold(usize::MAX), &paths, 2, 100, 4096, &Vec::new, &push_fields::<4>).is_none(), "4096-byte reads");
        let states = crate::direct::try_cold(&Fake::cold(usize::MAX), &paths, 2, 100, 8 * 4096, &Vec::new, &push_fields::<4>).expect("32768-byte reads");
        assert!(rows_of(states) == want);
    }

    struct Late<'a> {
        fail_at: u64,
        rows: &'a AtomicUsize,
        before: &'a AtomicUsize,
    }

    unsafe impl crate::direct::Disk for Late<'_> {
        type File = Vec<u8>;

        fn incore(&self, _: &Path, _: &[u8], pages: &mut [u8]) -> io::Result<()> {
            pages.fill(0);
            Ok(())
        }

        fn cached(&self, path: &Path, _: &File, offset: u64) -> io::Result<bool> {
            Err(io::Error::other(format!("{} is not probed at offset {offset} here", path.display())))
        }

        fn open(&self, path: &Path) -> io::Result<Vec<u8>> {
            std::fs::read(path)
        }

        fn read_at(&self, file: &Vec<u8>, dst: &mut [MaybeUninit<u8>], offset: u64) -> io::Result<usize> {
            if offset == self.fail_at {
                let clock = std::time::Instant::now();
                while self.rows.load(Ordering::SeqCst) == 0 && clock.elapsed() < std::time::Duration::from_secs(30) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                self.before.fetch_max(self.rows.load(Ordering::SeqCst), Ordering::SeqCst);
                return Err(io::Error::other(format!("no read at offset {offset}")));
            }
            let start = file.len().min(offset as usize);
            let n = dst.len().min(file.len() - start);
            for (slot, &byte) in dst.iter_mut().zip(&file[start..start + n]) {
                slot.write(byte);
            }
            Ok(n)
        }
    }

    #[test]
    fn a_late_failure_falls_back_to_the_rows_of_the_maps() {
        let dir = temp_dir("late");
        let mut text = b"user_id_a,user_id_b\n".to_vec();
        for i in 0..2000 {
            text.extend_from_slice(format!("late-left-{i:06},\"late-right-{:06}\"\n", i * 7919 % 2003).as_bytes());
        }
        for (f, cut) in [0, 4097, 9000].into_iter().enumerate() {
            std::fs::write(dir.join(format!("part-{f}.csv")), &text[..text.len() - cut]).unwrap();
        }
        let paths = csv_files(&dir).unwrap();
        let mut want: Vec<(Vec<u8>, Vec<u8>)> = paths.iter().flat_map(|path| expected::<2>(&std::fs::read(path).unwrap())).map(|row| (row[0].clone(), row[1].clone())).collect();
        want.sort();
        let rows = AtomicUsize::new(0);
        let before = AtomicUsize::new(0);
        let work = |state: &mut Vec<(Key, Key)>, data: &[u8], start: usize, end: usize| {
            fields::<2, _>(data, start, end, state, &|state: &mut Vec<(Key, Key)>, f: &[&[u8]; 2]| {
                rows.fetch_add(1, Ordering::SeqCst);
                state.push((Key::from_bytes(f[0]), Key::from_bytes(f[1])));
            })
        };
        let bytes = |states: Vec<Vec<(Key, Key)>>| {
            let mut all: Vec<(Vec<u8>, Vec<u8>)> = states.into_iter().flatten().map(|(a, b)| (a.to_vec(), b.to_vec())).collect();
            all.sort();
            all
        };
        let late = Late { fail_at: 3 * 4096, rows: &rows, before: &before };
        let replayed = bytes(run_tasks_on(&late, 4096, &paths, 3, 100, Vec::new, &work).unwrap());
        assert!(before.load(Ordering::SeqCst) > 0, "no row was parsed before the failure");
        let mapped = bytes(run_maps(&paths, 3, 100, Vec::new, &work).unwrap());
        assert!(want.len() > 5000 && replayed == want && mapped == want, "{} rows after the failure, {} from the maps, want {}", replayed.len(), mapped.len(), want.len());
    }
}
