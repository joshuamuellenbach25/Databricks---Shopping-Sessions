mod clicks;
mod dialect;
mod events;
mod ids;
mod links;
mod lookups;
mod output;
mod people;
mod scan;
mod sessions;
mod timestamp;
mod wide;

use std::ffi::{c_int, c_long};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use clicks::Click;
use events::Event;
use people::People;
use sessions::{Accum, Rules, Scratch, UserRow};

type Fail = Box<dyn std::error::Error + Send + Sync>;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_purge_delay` in the `mi_option_t` enum of the linked mimalloc (3.3).
const MI_OPTION_PURGE_DELAY: c_int = 15;

extern "C" {
    fn mi_option_set(option: c_int, value: c_long);
}

/// Stops mimalloc from returning freed memory to the kernel during the run:
/// memory one phase frees stays mapped and is reused by later phases instead
/// of being faulted in and zeroed again.
fn keep_freed_memory() {
    unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, -1) };
}

fn main() -> ExitCode {
    keep_freed_memory();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn env_path(name: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| default.into()))
}

fn run() -> Result<(), Fail> {
    let clock = Instant::now();
    let mut last = clock.elapsed();
    let mut phase = |name: &str| {
        let now = clock.elapsed();
        eprintln!("{name}: {:.3} s (total {:.3} s)", (now - last).as_secs_f64(), now.as_secs_f64());
        last = now;
    };
    let data = env_path("DATA_DIR", "/data");
    let out = env_path("OUTPUT_DIR", "/output");
    let threads = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from));

    let window = lookups::read_manifest(&data)?;
    let products = lookups::read_products(&data)?;
    let campaigns = lookups::read_campaigns(&data)?;
    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    table_dir(&user_dir)?;
    table_dir(&campaign_dir)?;
    let link_pairs = links::collect(&scan::csv_files(&data.join("links"))?, threads)?;
    let event_files = scan::csv_files(&data.join("events"))?;
    let click_files = scan::csv_files(&data.join("clicks"))?;
    phase("lookups and links");

    if !events::fits(window, &products) {
        let expected = link_pairs.iter().map(Vec::len).sum();
        let people = People::build(link_pairs.into_iter().flatten(), expected);
        let tables = wide::run(&event_files, &click_files, threads, window, &products, &campaigns, &people)?;
        output::write_user_daily(&user_dir.join("part-000.parquet"), &tables.rows, &products.categories)?;
        output::write_campaign_daily(&campaign_dir.join("part-000.parquet"), &tables.accum, &campaigns, tables.base_day)?;
        phase("wide records");
        check_output(&out)?;
        std::process::exit(0);
    }

    let event_ctx = events::Context { window, products: &products };
    let (people, parsed) = std::thread::scope(|scope| {
        let builder = scope.spawn(|| {
            let started = Instant::now();
            let expected = link_pairs.iter().map(Vec::len).sum();
            let people = People::build(link_pairs.into_iter().flatten(), expected);
            eprintln!("  people: {expected} links in {:.3} s", started.elapsed().as_secs_f64());
            people
        });
        let parsed = events::parse(&event_files, threads, &event_ctx);
        (builder.join().unwrap(), parsed)
    });
    let parsed = parsed?;
    eprintln!("linked ids: {}", people.linked());
    phase("events parse and people");

    let event_buckets = events::dedup(parsed, threads, &people);
    eprintln!("surviving events: {}", count(&event_buckets));
    phase("events dedup");

    let click_ctx = clicks::Context { window, campaigns: &campaigns, people: &people };
    let click_buckets = clicks::collect(&click_files, threads, &click_ctx)?;
    eprintln!("valid clicks: {}", count(&click_buckets));
    phase("clicks");

    let base_day = timestamp::day_of(window.start);
    let days = (timestamp::day_of(window.end - 1) - base_day + 1) as usize;
    let rules = Rules { origin: window.start, base_day, days, products: &products };
    let mut click_buckets = click_buckets.into_iter();
    let buckets: Vec<Mutex<Option<(Vec<Vec<Event>>, Vec<Vec<Click>>)>>> = event_buckets
        .into_iter()
        .map(|parts| Mutex::new(Some((parts, click_buckets.next().unwrap_or_default()))))
        .collect();
    let next = AtomicUsize::new(0);
    let results: Vec<Result<(Accum, f64, f64, f64), String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let (buckets, next, rules, campaigns, products, user_dir) = (&buckets, &next, &rules, &campaigns, &products, &user_dir);
                scope.spawn(move || -> Result<(Accum, f64, f64, f64), String> {
                    let started = Instant::now();
                    let mut accum = Accum::new(campaigns.ids.len(), days);
                    let mut scratch = Scratch::new(days);
                    let mut rows: Vec<UserRow> = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= buckets.len() {
                            break;
                        }
                        let Some((event_parts, click_parts)) = buckets[b].lock().unwrap().take() else {
                            continue;
                        };
                        sessions::run_bucket(&event_parts, &click_parts, rules, &mut scratch, &mut rows, &mut accum);
                    }
                    let computed = started.elapsed().as_secs_f64();
                    if thread == 0 || !rows.is_empty() {
                        let path = user_dir.join(format!("part-{thread:03}.parquet"));
                        output::write_user_daily(&path, &rows, &products.categories).map_err(|e| format!("{}: {e}", path.display()))?;
                    }
                    Ok((accum, computed, started.elapsed().as_secs_f64() - computed, scratch.sort_seconds))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total = Accum::new(campaigns.ids.len(), days);
    let (mut compute_max, mut write_max, mut sort_max) = (0f64, 0f64, 0f64);
    for result in results {
        let (accum, computed, written, sorted) = result?;
        compute_max = compute_max.max(computed);
        write_max = write_max.max(written);
        sort_max = sort_max.max(sorted);
        total.merge(&accum);
    }
    eprintln!("  sessions compute max {compute_max:.3} s (sort max {sort_max:.3} s), user_daily write max {write_max:.3} s");
    output::write_campaign_daily(&campaign_dir.join("part-000.parquet"), &total, &campaigns, base_day)?;
    phase("sessions and output");
    let sessions_total: i64 = total.cells.iter().map(|c| c.sessions).sum();
    eprintln!("sessions: {sessions_total}");
    check_output(&out)?;
    // Every output file is written and closed, so exit here instead of
    // returning: returning would free every record buffer one by one first.
    std::process::exit(0)
}

fn count<T>(buckets: &[Vec<Vec<T>>]) -> usize {
    buckets.iter().flatten().map(Vec::len).sum()
}

fn table_dir(dir: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(fail)?;
    for entry in std::fs::read_dir(dir).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        let table_file = name.ends_with(b".parquet") && !name.starts_with(b".") && !name.starts_with(b"_");
        if table_file && entry.file_type().map_err(fail)?.is_file() {
            std::fs::remove_file(entry.path()).map_err(|e| format!("{}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn check_output(out: &Path) -> Result<(), String> {
    for table in ["user_daily", "campaign_daily"] {
        let dir = out.join(table);
        let any = std::fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "parquet"));
        if !any {
            return Err(format!("{}: no parquet file written", dir.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
    }

    #[test]
    fn freed_memory_stays_mapped() {
        // 1000 ms is the linked mimalloc's default purge delay, so the index names that option.
        assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, 1000);
        keep_freed_memory();
        assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, -1);
    }

    #[test]
    fn table_dir_removes_only_the_parquet_files_a_reader_takes() {
        let dir = std::env::temp_dir().join(format!("table-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fresh = dir.join("campaign_daily");
        table_dir(&fresh).unwrap();
        assert!(fresh.is_dir());
        let table = dir.join("user_daily");
        std::fs::create_dir_all(table.join("nested.parquet")).unwrap();
        for name in ["part-000.parquet", "part-001.parquet", "_SUCCESS", ".part-0.parquet.crc", "_part-9.parquet", ".part-8.parquet", "notes.txt"] {
            std::fs::write(table.join(name), b"stale").unwrap();
        }
        table_dir(&table).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&table).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, [".part-0.parquet.crc", ".part-8.parquet", "_SUCCESS", "_part-9.parquet", "nested.parquet", "notes.txt"]);
    }
}
