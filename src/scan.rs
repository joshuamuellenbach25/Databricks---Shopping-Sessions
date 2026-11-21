use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use memchr::memchr;
use memmap2::MmapOptions;

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
    let mut maps = Vec::with_capacity(paths.len());
    for path in paths {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            continue;
        }
        let map = unsafe { MmapOptions::new().map(&file)? };
        maps.push(map);
    }
    let mut tasks = Vec::new();
    for (index, map) in maps.iter().enumerate() {
        let mut start = 0;
        while start < map.len() {
            tasks.push((index, start, (start + chunk).min(map.len())));
            start += chunk;
        }
    }
    let mapped = clock.elapsed();
    let next = AtomicUsize::new(0);
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
                        work(&mut state, &maps[file], start, end);
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<S>>()
    });
    std::mem::forget(maps);
    eprintln!("  scan: {} files, map {:.3} s, parse {:.3} s", paths.len(), mapped.as_secs_f64(), (clock.elapsed() - mapped).as_secs_f64());
    Ok(states)
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

/// Bit i is set when byte `at + i` is a comma, a quote or a newline; bytes
/// past the end of `data` count as none of them.
#[inline(always)]
fn structural(data: &[u8], at: usize) -> u64 {
    if at + 64 <= data.len() {
        unsafe { masks(data.as_ptr().add(at)) }
    } else {
        let mut buf = [0u8; 64];
        let n = data.len() - at;
        buf[..n].copy_from_slice(&data[at..]);
        unsafe { masks(buf.as_ptr()) }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn masks(p: *const u8) -> u64 {
    use std::arch::x86_64::*;
    let comma = _mm_set1_epi8(b',' as i8);
    let quote = _mm_set1_epi8(b'"' as i8);
    let newline = _mm_set1_epi8(b'\n' as i8);
    let mut bits = 0u64;
    for i in 0..4 {
        let v = _mm_loadu_si128(p.add(16 * i) as *const __m128i);
        let hit = _mm_or_si128(_mm_or_si128(_mm_cmpeq_epi8(v, comma), _mm_cmpeq_epi8(v, quote)), _mm_cmpeq_epi8(v, newline));
        bits |= u64::from(_mm_movemask_epi8(hit) as u16) << (16 * i);
    }
    bits
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> u64 {
    let mut bits = 0u64;
    for i in 0..64 {
        let c = *p.add(i);
        if c == b',' || c == b'"' || c == b'\n' {
            bits |= 1 << i;
        }
    }
    bits
}

struct Line<'a, const N: usize> {
    out: [&'a [u8]; N],
    n: usize,
    field_start: usize,
    quoted: bool,
    closed_at: usize,
    bad: bool,
}

impl<'a, const N: usize> Line<'a, N> {
    fn new(start: usize) -> Self {
        Line { out: [&[]; N], n: 0, field_start: start, quoted: false, closed_at: usize::MAX, bad: false }
    }

    #[inline(always)]
    fn end_field(&mut self, data: &'a [u8], p: usize) {
        if self.bad {
            return;
        }
        let value = if self.quoted {
            if self.closed_at != p.wrapping_sub(1) || p < self.field_start + 2 {
                self.bad = true;
                return;
            }
            &data[self.field_start + 1..p - 1]
        } else {
            &data[self.field_start..p]
        };
        if self.n == N {
            self.bad = true;
            return;
        }
        self.out[self.n] = value;
        self.n += 1;
        self.field_start = p + 1;
        self.quoted = false;
        self.closed_at = usize::MAX;
    }

    #[inline(always)]
    fn quote(&mut self, p: usize) {
        if p == self.field_start && !self.quoted {
            self.quoted = true;
        } else if self.quoted && self.closed_at == usize::MAX {
            self.closed_at = p;
        } else {
            self.bad = true;
        }
    }
}

fn fields<'a, const N: usize, S>(data: &'a [u8], start: usize, end: usize, state: &mut S, f: &impl Fn(&mut S, &[&[u8]; N])) {
    let Some(mut line_start) = first_line(data, start) else {
        return;
    };
    if line_start >= end {
        return;
    }
    let mut line: Line<'a, N> = Line::new(line_start);
    let mut block = line_start;
    while block < data.len() {
        let mut m = structural(data, block);
        while m != 0 {
            let p = block + m.trailing_zeros() as usize;
            m &= m - 1;
            match data[p] {
                b'"' => {
                    if !line.bad {
                        line.quote(p);
                    }
                }
                b',' => line.end_field(data, p),
                _ => {
                    line.end_field(data, p);
                    if !line.bad && line.n == N {
                        f(state, &line.out);
                    }
                    line_start = p + 1;
                    if line_start >= end {
                        return;
                    }
                    line = Line::new(line_start);
                }
            }
        }
        block += 64;
    }
    if line_start < data.len() {
        line.end_field(data, data.len());
        if !line.bad && line.n == N {
            f(state, &line.out);
        }
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

    fn check_fields<const N: usize>(dir: &Path, seed: u64) {
        let alphabet: &[u8] = b"ab,\"\r 1";
        let mut x = seed;
        let mut text: Vec<u8> = b"header\n".to_vec();
        let mut lines: Vec<Vec<u8>> = Vec::new();
        for _ in 0..3000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (x >> 58) as usize;
            let mut line = Vec::new();
            let mut y = x;
            for _ in 0..len {
                y = y.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                line.push(alphabet[(y >> 61) as usize % alphabet.len()]);
            }
            if x % 3 == 0 {
                let fields: Vec<String> = (0..N).map(|i| if (x >> i) & 1 == 1 { format!("\"v{i}\"") } else { format!("v{i}") }).collect();
                line = fields.join(",").into_bytes();
            }
            text.extend_from_slice(&line);
            text.push(b'\n');
            lines.push(line);
        }
        let last = b"\"q\",tail".repeat(1);
        text.extend_from_slice(&last);
        lines.push(last);
        let file = dir.join(format!("f-{N}-{seed}.csv"));
        std::fs::write(&file, &text).unwrap();
        let mut expected: Vec<Vec<Vec<u8>>> = Vec::new();
        for line in &lines {
            let mut out: [&[u8]; N] = [&[]; N];
            if crate::dialect::split(line, &mut out) {
                expected.push(out.iter().map(|f| f.to_vec()).collect());
            }
        }
        expected.sort();
        let paths = vec![file];
        for chunk in [1, 13, 64, 100, 4096, 1 << 20] {
            let states = run_fields::<N, _, _, _>(&paths, 3, chunk, Vec::new, |s: &mut Vec<Vec<Vec<u8>>>, f| s.push(f.iter().map(|v| v.to_vec()).collect())).unwrap();
            let mut got: Vec<Vec<Vec<u8>>> = states.into_iter().flatten().collect();
            got.sort();
            assert_eq!(got, expected, "N {N} seed {seed} chunk {chunk}");
        }
    }

    #[test]
    fn fields_agree_with_the_dialect() {
        let dir = temp_dir("fields");
        for seed in 1..6 {
            check_fields::<1>(&dir, seed);
            check_fields::<2>(&dir, seed);
            check_fields::<4>(&dir, seed);
            check_fields::<7>(&dir, seed);
        }
    }
}
