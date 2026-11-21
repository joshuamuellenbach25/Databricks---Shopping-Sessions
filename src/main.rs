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
use std::sync::{mpsc, Mutex};
use std::time::Instant;

use clicks::Click;
use events::Event;
use lookups::{Campaigns, Products};
use output::{Parts, UserDaily};
use people::People;
use sessions::{Accum, Counts, Rules, Scratch, UserRows};

type Fail = Box<dyn std::error::Error + Send + Sync>;

const ROW_GROUP_ROWS: usize = 1 << 14;

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: tests::Counted = tests::Counted;

const MI_OPTION_PURGE_DELAY: c_int = 15;

extern "C" {
    fn mi_option_set(option: c_int, value: c_long);
}

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
    let products: &'static Products = Box::leak(Box::new(lookups::read_products(&data)?));
    events::check_fits(window, products)?;
    let campaigns: &'static Campaigns = Box::leak(Box::new(lookups::read_campaigns(&data)?));
    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&user_dir)?;
    std::fs::create_dir_all(&campaign_dir)?;
    let (user_parts, campaign_parts) = (Parts::new(user_dir)?, Parts::new(campaign_dir)?);
    let link_pairs = links::collect(&scan::csv_files(&data.join("links"))?, threads)?;
    let event_files = scan::csv_files(&data.join("events"))?;
    let click_files = scan::csv_files(&data.join("clicks"))?;
    phase("lookups and links");

    let event_ctx = events::Context::new(window, products);
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
    phase("events dedup");

    let click_ctx = clicks::Context { window, campaigns, people: &people };
    let click_buckets = clicks::collect(&click_files, threads, &click_ctx)?;
    eprintln!("valid clicks: {}", count(&click_buckets));
    phase("clicks");

    let base_day = timestamp::day_of(window.start);
    let days = (timestamp::day_of(window.end - 1) - base_day + 1) as usize;
    let rules = Rules { origin: window.start, base_day, products };
    let mut order: Vec<(usize, usize)> = event_buckets.iter().enumerate().map(|(b, parts)| (parts.iter().map(Vec::len).sum(), b)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let mut click_buckets = click_buckets.into_iter();
    let buckets: Vec<Mutex<Option<(Vec<Vec<Event>>, Vec<Vec<Click>>)>>> = event_buckets
        .into_iter()
        .map(|parts| Mutex::new(Some((parts, click_buckets.next().unwrap_or_default()))))
        .collect();
    let next = AtomicUsize::new(0);
    let (sender, arrivals) = mpsc::channel::<Counts>();
    let finished = std::thread::scope(|scope| -> Result<(Accum, f64, f64, f64), String> {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let (buckets, order, next, rules, user_parts, sender) = (&buckets, &order, &next, &rules, &user_parts, sender.clone());
                scope.spawn(move || -> Result<(bool, f64, f64, f64), String> {
                    let started = Instant::now();
                    let fail = |e: Fail| format!("{}: {e}", user_parts.path(thread).display());
                    let mut counts = Counts::new(campaigns.ids.len(), days);
                    let mut scratch = Scratch::new(campaigns.ids.len(), days);
                    let mut rows = UserRows::with_capacity(2 * ROW_GROUP_ROWS);
                    let mut file: Option<UserDaily> = None;
                    let mut written = 0f64;
                    let mut taken = 0;
                    let mut flush = |rows: &mut UserRows, file: &mut Option<UserDaily>| -> Result<(), String> {
                        let begun = Instant::now();
                        if file.is_none() {
                            *file = Some(UserDaily::create(user_parts.create(thread)?, &products.categories).map_err(fail)?);
                        }
                        if let Some(writer) = file {
                            writer.write(rows).map_err(fail)?;
                        }
                        rows.clear();
                        written += begun.elapsed().as_secs_f64();
                        Ok(())
                    };
                    loop {
                        let Some(&(events, b)) = order.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        let Some((event_parts, click_parts)) = buckets[b].lock().unwrap().take() else {
                            continue;
                        };
                        taken += events;
                        sessions::check_events(taken)?;
                        sessions::run_bucket(&event_parts, &click_parts, rules, &mut scratch, &mut rows, &mut counts)?;
                        if rows.len() >= ROW_GROUP_ROWS {
                            flush(&mut rows, &mut file)?;
                        }
                    }
                    sender.send(counts).map_err(|_| format!("sessions thread {thread}: the campaign totals were no longer being collected"))?;
                    drop(sender);
                    if !rows.is_empty() {
                        flush(&mut rows, &mut file)?;
                    }
                    let wrote = file.is_some();
                    if let Some(file) = file {
                        let begun = Instant::now();
                        file.close().map_err(fail)?;
                        written += begun.elapsed().as_secs_f64();
                    }
                    Ok((wrote, started.elapsed().as_secs_f64() - written, written, scratch.sort_seconds))
                })
            })
            .collect();
        drop(sender);
        let shares = threads.min(campaigns.ids.len());
        let campaign_files: Vec<Result<File, String>> = (0..shares).map(|part| campaign_parts.create(part)).collect();
        let mut total = Accum::new(campaigns.ids.len(), days);
        let mut merged = 0;
        for counts in &arrivals {
            total.merge(&counts);
            merged += 1;
        }
        let campaign_rows: Result<Vec<usize>, String> = if merged == threads {
            std::thread::scope(|inner| {
                let (total, campaign_parts) = (&total, &campaign_parts);
                let writers: Vec<_> = campaign_files
                    .into_iter()
                    .enumerate()
                    .map(|(part, file)| {
                        let share = part * campaigns.ids.len() / shares..(part + 1) * campaigns.ids.len() / shares;
                        inner.spawn(move || output::write_campaign_daily(file?, total, campaigns, share, base_day).map_err(|e| format!("{}: {e}", campaign_parts.path(part).display())))
                    })
                    .collect();
                writers.into_iter().map(|writer| writer.join().unwrap()).collect()
            })
        } else {
            Err(format!("{merged} of {threads} sessions threads delivered campaign totals"))
        };
        let mut any_user_part = false;
        let (mut compute_max, mut write_max, mut sort_max) = (0f64, 0f64, 0f64);
        for handle in handles {
            let (wrote, computed, written, sorted) = handle.join().unwrap()?;
            any_user_part |= wrote;
            compute_max = compute_max.max(computed);
            write_max = write_max.max(written);
            sort_max = sort_max.max(sorted);
        }
        let campaign_rows = campaign_rows?;
        let any_campaign_rows = campaign_rows.iter().any(|&rows| rows > 0);
        for (part, &rows) in campaign_rows.iter().enumerate() {
            if rows == 0 && (any_campaign_rows || part > 0) {
                campaign_parts.remove(part)?;
            }
        }
        if !any_user_part {
            UserDaily::create(user_parts.create(0)?, &products.categories).and_then(UserDaily::close).map_err(|e| format!("{}: {e}", user_parts.path(0).display()))?;
        }
        output::publish(&[&user_parts, &campaign_parts])?;
        Ok((total, compute_max, write_max, sort_max))
    });
    let (total, compute_max, write_max, sort_max) = match finished {
        Ok(finished) => finished,
        Err(error) => {
            let message = match user_parts.discard(threads).and(campaign_parts.discard(threads)) {
                Ok(()) => error,
                Err(problem) => format!("{error}; {problem}"),
            };
            return Err(message.into());
        }
    };
    eprintln!("  sessions compute max {compute_max:.3} s (sort max {sort_max:.3} s), user_daily write max {write_max:.3} s");
    phase("sessions and output");
    let sessions_total: i64 = total.cells.iter().map(|c| c.sessions).sum();
    eprintln!("sessions: {sessions_total}");
    check_output(&out)?;
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
    use std::alloc::{GlobalAlloc, Layout};
    use std::cell::Cell;

    use super::*;

    extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
    }

    pub struct Counted;

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    fn track(bytes: isize) {
        let _ = LIVE.try_with(|live| live.set(live.get() + bytes));
    }

    pub fn live_bytes() -> isize {
        LIVE.with(Cell::get)
    }

    unsafe impl GlobalAlloc for Counted {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            track(layout.size() as isize);
            mimalloc::MiMalloc.alloc(layout)
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            track(layout.size() as isize);
            mimalloc::MiMalloc.alloc_zeroed(layout)
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            track(-(layout.size() as isize));
            mimalloc::MiMalloc.dealloc(ptr, layout)
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            track(new_size as isize - layout.size() as isize);
            mimalloc::MiMalloc.realloc(ptr, layout, new_size)
        }
    }

    #[test]
    fn freed_memory_stays_mapped() {
        assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, 1000);
        keep_freed_memory();
        assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, -1);
    }
}
