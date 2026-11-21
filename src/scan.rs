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

/// Calls `line` once for every data line of every file (the first line of
/// each file is its header and is skipped; a last line without `\n` counts),
/// on `threads` workers, each with its own state made by `init`. Returns the
/// states. Line order is unspecified.
pub fn run<S, I, F>(paths: &[PathBuf], threads: usize, chunk: usize, init: I, line: F) -> io::Result<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &[u8]) + Sync,
{
    let mut maps = Vec::with_capacity(paths.len());
    for path in paths {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            continue;
        }
        let map = unsafe { MmapOptions::new().populate().map(&file)? };
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
                        lines(&maps[file], start, end, &mut state, &line);
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<S>>()
    });
    Ok(states)
}

fn lines<S>(data: &[u8], start: usize, end: usize, state: &mut S, line: &impl Fn(&mut S, &[u8])) {
    let mut pos = if start == 0 {
        match memchr(b'\n', data) {
            Some(i) => i + 1,
            None => return,
        }
    } else if data[start - 1] == b'\n' {
        start
    } else {
        match memchr(b'\n', &data[start..]) {
            Some(i) => start + i + 1,
            None => return,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn every_line_once_whatever_the_chunking() {
        let dir = std::env::temp_dir().join(format!("scan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
            let states = run(&paths, 3, chunk, Vec::new, |s: &mut Vec<String>, l| {
                s.push(String::from_utf8(l.to_vec()).unwrap())
            })
            .unwrap();
            let mut got: Vec<String> = states.into_iter().flatten().collect();
            got.sort();
            assert_eq!(got, expected, "chunk {chunk}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
