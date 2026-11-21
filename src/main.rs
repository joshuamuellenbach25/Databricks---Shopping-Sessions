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

use sessions::{Accum, Rules, Scratch, UserRow};

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

fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    phase("lookups");

    let people = links::collect(&scan::csv_files(&data.join("links"))?, threads)?;
    eprintln!("linked ids: {}", people.linked());
    phase("links");

    let event_ctx = events::Context { window, products: &products, people: &people };
    let event_buckets = events::collect(&scan::csv_files(&data.join("events"))?, threads, &event_ctx)?;
    eprintln!("surviving events: {}", event_buckets.iter().map(Vec::len).sum::<usize>());
    phase("events");

    let click_ctx = clicks::Context { window, campaigns: &campaigns, people: &people };
    let click_buckets = clicks::collect(&scan::csv_files(&data.join("clicks"))?, threads, &click_ctx)?;
    eprintln!("valid clicks: {}", click_buckets.iter().map(Vec::len).sum::<usize>());
    phase("clicks");

    let base_day = timestamp::day_of(window.start);
    let days = (timestamp::day_of(window.end - 1) - base_day + 1) as usize;
    let rules = Rules { base_day, days, products: &products };
    let buckets: Vec<Mutex<Option<(Vec<events::Event>, Vec<clicks::Click>)>>> = event_buckets
        .into_iter()
        .zip(click_buckets)
        .map(|pair| Mutex::new(Some(pair)))
        .collect();
    let next = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    let accums: Vec<Result<Accum, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<Accum, String> {
                    let mut accum = Accum::new(campaigns.ids.len(), days);
                    let mut scratch = Scratch::new(days);
                    let mut rows: Vec<UserRow> = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= buckets.len() {
                            break;
                        }
                        let Some((mut bucket_events, mut bucket_clicks)) = buckets[b].lock().unwrap().take() else {
                            continue;
                        };
                        rows.clear();
                        sessions::run_bucket(&mut bucket_events, &mut bucket_clicks, &rules, &mut scratch, &mut rows, &mut accum);
                        drop(bucket_events);
                        drop(bucket_clicks);
                        if !rows.is_empty() {
                            let path = user_dir.join(format!("part-{b:03}.parquet"));
                            output::write_user_daily(&path, &rows, &products.categories).map_err(|e| format!("{}: {e}", path.display()))?;
                            written.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(accum)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total = Accum::new(campaigns.ids.len(), days);
    for accum in accums {
        total.merge(&accum?);
    }
    if written.load(Ordering::Relaxed) == 0 {
        output::write_user_daily(&user_dir.join("part-000.parquet"), &[], &products.categories)?;
    }
    output::write_campaign_daily(&campaign_dir.join("part-000.parquet"), &total, &campaigns, base_day)?;
    phase("sessions and output");
    let sessions_total: i64 = total.cells.iter().map(|c| c.sessions).sum();
    eprintln!("sessions: {sessions_total}");
    check_output(&out)?;
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
