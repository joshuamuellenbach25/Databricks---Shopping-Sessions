use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use parquet::file::reader::{FileReader, SerializedFileReader};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn case(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("output-folder-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let data = dir.join("data");
    for folder in ["events", "clicks", "links"] {
        fs::create_dir_all(data.join(folder)).unwrap();
    }
    let files = [
        ("manifest.json", r#"{"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}"#),
        ("products.csv", "product_id,category\np1,shoes\n"),
        ("campaigns.csv", "campaign_id,channel\nc_1,search\nc_2,social\n"),
        (
            "events/a.csv",
            "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1,\ne2,u2,2026-10-04T10:00:00Z,2026-10-04T10:00:00Z,view,p1,\n",
        ),
        ("clicks/a.csv", "click_id,user_id,campaign_id,ts\nk1,u1,c_1,2026-10-03T09:00:00Z\n"),
        ("links/a.csv", "user_id_a,user_id_b\n"),
    ];
    for (file, text) in files {
        fs::write(data.join(file), text).unwrap();
    }
    dir
}

fn solve(dir: &Path, ncpu: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_solver"));
    command.env("DATA_DIR", dir.join("data")).env("OUTPUT_DIR", dir.join("out")).env("NCPU", ncpu);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
    command.output().unwrap()
}

fn solved(dir: &Path, ncpu: &str) {
    let run = solve(dir, ncpu);
    assert!(run.status.success(), "NCPU={ncpu}: {}", String::from_utf8_lossy(&run.stderr));
}

fn parts(out: &Path, table: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(out.join(table)).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

fn rows(out: &Path, table: &str) -> i64 {
    parts(out, table)
        .iter()
        .filter(|name| name.ends_with(".parquet"))
        .map(|name| SerializedFileReader::new(fs::File::open(out.join(table).join(name)).unwrap()).unwrap().metadata().file_metadata().num_rows())
        .sum()
}

fn state(out: &Path) -> Vec<(String, i64)> {
    ["user_daily", "campaign_daily"].iter().map(|table| (format!("{table}: {}", parts(out, table).join(" ")), rows(out, table))).collect()
}

#[test]
fn a_rerun_with_fewer_threads_leaves_only_its_own_parts() {
    let dir = case("rerun");
    let out = dir.join("out");
    fs::create_dir_all(out.join("campaign_daily")).unwrap();
    fs::write(out.join("campaign_daily").join("_SUCCESS"), "").unwrap();
    solved(&dir, "2");
    assert_eq!(
        state(&out),
        [("user_daily: part-000.parquet part-001.parquet".to_string(), 2), ("campaign_daily: _SUCCESS part-000.parquet part-001.parquet".to_string(), 2)]
    );
    solved(&dir, "1");
    assert_eq!(state(&out), [("user_daily: part-000.parquet".to_string(), 2), ("campaign_daily: _SUCCESS part-000.parquet".to_string(), 2)]);
}

#[cfg(windows)]
#[test]
fn a_run_that_fails_on_its_input_keeps_the_previous_output() {
    use std::os::windows::fs::OpenOptionsExt;
    let readable = |out: &Path| -> Vec<(String, Vec<u8>)> {
        let mut found = Vec::new();
        for table in ["user_daily", "campaign_daily"] {
            for name in parts(out, table) {
                if name.ends_with(".parquet") && !name.starts_with('.') && !name.starts_with('_') {
                    found.push((format!("{table}/{name}"), fs::read(out.join(table).join(&name)).unwrap()));
                }
            }
        }
        found
    };
    let dir = case("failed");
    let out = dir.join("out");
    solved(&dir, "2");
    let before = readable(&out);
    assert_eq!(before.len(), 4);
    let held = fs::OpenOptions::new().read(true).share_mode(0).open(dir.join("data").join("clicks").join("a.csv")).unwrap();
    let failed = solve(&dir, "2");
    drop(held);
    assert!(!failed.status.success(), "the run with a locked click file passed");
    let after = readable(&out);
    assert!(after == before, "the failed run changed the output: {:?}", after.iter().map(|(name, bytes)| (name, bytes.len())).collect::<Vec<_>>());
    solved(&dir, "1");
    assert_eq!(state(&out), [("user_daily: part-000.parquet".to_string(), 2), ("campaign_daily: part-000.parquet".to_string(), 2)]);
}
