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

fn main() -> ExitCode {
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
    std::fs::create_dir_all(&user_dir)?;
    std::fs::create_dir_all(&campaign_dir)?;
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
    eprintln!("surviving events: {}", count(&event_buckets));
    phase("events dedup");

    let click_ctx = clicks::Context { window, campaigns: &campaigns, people: &people };
    let click_buckets = clicks::collect(&click_files, threads, &click_ctx)?;
    eprintln!("valid clicks: {}", count(&click_buckets));
    phase("clicks");

    let base_day = timestamp::day_of(window.start);
    let days = (timestamp::day_of(window.end - 1) - base_day + 1) as usize;
    let rules = Rules { base_day, days, products: &products };
    let mut click_buckets = click_buckets.into_iter();
    let buckets: Vec<Mutex<Option<(Vec<Vec<Event>>, Vec<Vec<Click>>)>>> = event_buckets
        .into_iter()
        .map(|parts| Mutex::new(Some((parts, click_buckets.next().unwrap_or_default()))))
        .collect();
    let next = AtomicUsize::new(0);
    let results: Vec<Result<Accum, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let (buckets, next, rules, campaigns, products, user_dir) = (&buckets, &next, &rules, &campaigns, &products, &user_dir);
                scope.spawn(move || -> Result<Accum, String> {
                    let mut accum = Accum::new(campaigns.ids.len(), days);
                    let mut scratch = Scratch::new(days);
                    let mut rows: Vec<UserRow> = Vec::new();
                    let mut bucket_events: Vec<Event> = Vec::new();
                    let mut bucket_clicks: Vec<Click> = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= buckets.len() {
                            break;
                        }
                        let Some((event_parts, click_parts)) = buckets[b].lock().unwrap().take() else {
                            continue;
                        };
                        bucket_events.clear();
                        for part in event_parts {
                            bucket_events.extend_from_slice(&part);
                        }
                        bucket_clicks.clear();
                        for part in click_parts {
                            bucket_clicks.extend_from_slice(&part);
                        }
                        sessions::run_bucket(&mut bucket_events, &mut bucket_clicks, rules, &mut scratch, &mut rows, &mut accum);
                    }
                    if thread == 0 || !rows.is_empty() {
                        let path = user_dir.join(format!("part-{thread:03}.parquet"));
                        output::write_user_daily(&path, &rows, &products.categories).map_err(|e| format!("{}: {e}", path.display()))?;
                    }
                    Ok(accum)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total = Accum::new(campaigns.ids.len(), days);
    for accum in results {
        total.merge(&accum?);
    }
    output::write_campaign_daily(&campaign_dir.join("part-000.parquet"), &total, &campaigns, base_day)?;
    phase("sessions and output");
    let sessions_total: i64 = total.cells.iter().map(|c| c.sessions).sum();
    eprintln!("sessions: {sessions_total}");
    check_output(&out)?;
    Ok(())
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
