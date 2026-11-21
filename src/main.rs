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

use std::ffi::{c_int, c_long};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use clicks::Click;
use events::Event;
use output::UserDaily;
use people::People;
use sessions::{Accum, Counts, Rules, Scratch, UserRows};

type Fail = Box<dyn std::error::Error + Send + Sync>;

const ROW_GROUP_ROWS: usize = 1 << 14;

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
    events::check_fits(window, &products)?;
    let campaigns = lookups::read_campaigns(&data)?;
    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&user_dir)?;
    std::fs::create_dir_all(&campaign_dir)?;
    let user_paths: Vec<PathBuf> = (0..threads).map(|part| user_dir.join(output::staged_name(part))).collect();
    let campaign_paths: Vec<PathBuf> = (0..threads).map(|part| campaign_dir.join(output::staged_name(part))).collect();
    let creating = {
        let paths: Vec<PathBuf> = user_paths.iter().chain(&campaign_paths).cloned().collect();
        std::thread::spawn(move || -> Result<Vec<File>, String> { paths.iter().map(|path| File::create(path).map_err(|e| format!("{}: {e}", path.display()))).collect() })
    };
    let link_pairs = links::collect(&scan::csv_files(&data.join("links"))?, threads)?;
    let event_files = scan::csv_files(&data.join("events"))?;
    let click_files = scan::csv_files(&data.join("clicks"))?;
    phase("lookups and links");

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
    let surviving = count(&event_buckets);
    eprintln!("surviving events: {surviving}");
    sessions::check_counts(surviving)?;
    phase("events dedup");

    let click_ctx = clicks::Context { window, campaigns: &campaigns, people: &people };
    let click_buckets = clicks::collect(&click_files, threads, &click_ctx)?;
    eprintln!("valid clicks: {}", count(&click_buckets));
    phase("clicks");

    let base_day = timestamp::day_of(window.start);
    let days = (timestamp::day_of(window.end - 1) - base_day + 1) as usize;
    let rules = Rules { origin: window.start, base_day, products: &products };
    let mut order: Vec<(usize, usize)> = event_buckets.iter().enumerate().map(|(b, parts)| (parts.iter().map(Vec::len).sum(), b)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let mut click_buckets = click_buckets.into_iter();
    let buckets: Vec<Mutex<Option<(Vec<Vec<Event>>, Vec<Vec<Click>>)>>> = event_buckets
        .into_iter()
        .map(|parts| Mutex::new(Some((parts, click_buckets.next().unwrap_or_default()))))
        .collect();
    let next = AtomicUsize::new(0);
    let mut user_files = creating.join().unwrap()?;
    let campaign_files = user_files.split_off(threads);
    let mut user_files = user_files.into_iter();
    let (sender, receiver) = mpsc::channel::<Counts>();
    let (results, total, campaign_written) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let (buckets, order, next, rules, campaigns, products, path) = (&buckets, &order, &next, &rules, &campaigns, &products, &user_paths[thread]);
                let file = user_files.next();
                let sender = sender.clone();
                scope.spawn(move || -> Result<(f64, f64, f64), String> {
                    let started = Instant::now();
                    let fail = |e: Fail| format!("{}: {e}", path.display());
                    let mut counts = Counts::new(campaigns.ids.len(), days);
                    let mut scratch = Scratch::new(campaigns.ids.len(), days);
                    let mut rows = UserRows::with_capacity(2 * ROW_GROUP_ROWS);
                    let file = file.ok_or_else(|| format!("{}: not created", path.display()))?;
                    let mut file = UserDaily::create(file, &products.categories).map_err(fail)?;
                    let mut written = 0f64;
                    let mut flush = |rows: &mut UserRows, file: &mut UserDaily| -> Result<(), String> {
                        let begun = Instant::now();
                        file.write(rows).map_err(fail)?;
                        rows.clear();
                        written += begun.elapsed().as_secs_f64();
                        Ok(())
                    };
                    loop {
                        let Some(&(_, b)) = order.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        let Some((event_parts, click_parts)) = buckets[b].lock().unwrap().take() else {
                            continue;
                        };
                        sessions::run_bucket(&event_parts, &click_parts, rules, &mut scratch, &mut rows, &mut counts);
                        if rows.len() >= ROW_GROUP_ROWS {
                            flush(&mut rows, &mut file)?;
                        }
                    }
                    sender.send(counts).map_err(|_| "the campaign_daily writer stopped before taking a thread's counts".to_string())?;
                    if !rows.is_empty() {
                        flush(&mut rows, &mut file)?;
                    }
                    let begun = Instant::now();
                    file.close().map_err(fail)?;
                    let written = written + begun.elapsed().as_secs_f64();
                    Ok((started.elapsed().as_secs_f64() - written, written, scratch.sort_seconds))
                })
            })
            .collect();
        drop(sender);
        let mut total = Accum::new(campaigns.ids.len(), days);
        let mut merged = 0;
        while merged < threads {
            let Ok(counts) = receiver.recv() else {
                break;
            };
            total.merge(&counts);
            merged += 1;
        }
        let total = Arc::new(total);
        let campaign_written: Result<(), String> = if merged == threads {
            let parts = campaign_files.len();
            let writers: Vec<_> = campaign_files
                .into_iter()
                .zip(&campaign_paths)
                .enumerate()
                .map(|(part, (file, path))| {
                    let (total, campaigns) = (Arc::clone(&total), &campaigns);
                    let share = part * campaigns.ids.len() / parts..(part + 1) * campaigns.ids.len() / parts;
                    scope.spawn(move || output::write_campaign_daily(file, &total, campaigns, share, base_day).map_err(|e| format!("{}: {e}", path.display())))
                })
                .collect();
            writers.into_iter().map(|h| h.join().unwrap()).collect()
        } else {
            Err(format!("only {merged} of {threads} sessions threads passed their counts"))
        };
        let results: Vec<Result<(f64, f64, f64), String>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        (results, total, campaign_written)
    });
    let (mut compute_max, mut write_max, mut sort_max) = (0f64, 0f64, 0f64);
    for result in results {
        let (computed, written, sorted) = result?;
        compute_max = compute_max.max(computed);
        write_max = write_max.max(written);
        sort_max = sort_max.max(sorted);
    }
    campaign_written?;
    output::publish(&user_dir, threads)?;
    output::publish(&campaign_dir, threads)?;
    eprintln!("  sessions compute max {compute_max:.3} s (sort max {sort_max:.3} s), user_daily write max {write_max:.3} s");
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
}
