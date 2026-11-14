//! Release baseline for full-file publication.
//!
//! Run with `cargo run --release --example storage_bench`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustdb::{Database, StatementResult};

const SIZES: [usize; 3] = [100, 1_000, 10_000];
const SAMPLES: usize = 5;
const NOTE: &str = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz12";

fn main() {
    assert_eq!(NOTE.len(), 64);
    println!(
        "hardware: {}",
        command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
    );
    println!(
        "memory_bytes: {}",
        command_output("sysctl", &["-n", "hw.memsize"])
    );
    println!("machine: {}", command_output("uname", &["-m"]));
    println!("rustc: {}", command_output("rustc", &["--version"]));
    println!("schema: bench(id INT, email TEXT, note TEXT, active BOOLEAN)");
    println!("note_bytes: 64");
    println!("email: user#####@example.com");
    println!("samples: {SAMPLES}");
    println!("warmup_rows: 20");

    warmup();

    for size in SIZES {
        measure(size);
    }
}

fn measure(size: usize) {
    let path = temp_db();
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE bench (id INT, email TEXT, note TEXT, active BOOLEAN);")
        .expect("create");

    let started = Instant::now();
    for id in 1..=size {
        insert(&mut db, id);
    }
    let load = started.elapsed();
    let file_bytes = fs::metadata(&path).expect("metadata").len();
    let mean_insert_us = load.as_secs_f64() * 1_000_000.0 / size as f64;

    let scans = repeat(SAMPLES, |_| scan(&mut db, size));
    let mut active = false;
    let updates = repeat(SAMPLES, |_| {
        active = !active;
        update(&mut db, active)
    });
    let inserts = repeat(SAMPLES, |sample| timed_insert(&mut db, size + 1 + sample));

    println!("size={size}");
    println!("load_rows={size}");
    println!("load_total_ms={:.3}", load.as_secs_f64() * 1_000.0);
    println!("load_mean_insert_us={mean_insert_us:.1}");
    println!("file_bytes={file_bytes}");
    print_samples("scan_us", &scans);
    print_samples("update_us", &updates);
    print_samples("insert_us", &inserts);

    drop(db);
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

fn warmup() {
    let path = temp_db();
    let mut db = Database::open(&path).expect("warmup open");
    db.execute("CREATE TABLE bench (id INT, email TEXT, note TEXT, active BOOLEAN);")
        .expect("warmup create");
    for id in 1..=20 {
        insert(&mut db, id);
    }
    scan(&mut db, 20);
    drop(db);
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

fn insert(db: &mut Database, id: usize) {
    let email = format!("user{id:05}@example.com");
    let active = if id.is_multiple_of(2) {
        "TRUE"
    } else {
        "FALSE"
    };
    let sql = format!(
        "INSERT INTO bench (id, email, note, active) VALUES ({id}, '{email}', '{NOTE}', {active});"
    );
    match single(db, &sql) {
        StatementResult::Insert { count } => assert_eq!(count, 1),
        other => panic!("insert returned {other:?}"),
    }
}

fn timed_insert(db: &mut Database, id: usize) -> u128 {
    let started = Instant::now();
    insert(db, id);
    started.elapsed().as_micros()
}

fn scan(db: &mut Database, expected: usize) -> u128 {
    let started = Instant::now();
    let result = single(db, "SELECT id, email, note, active FROM bench;");
    let elapsed = started.elapsed().as_micros();
    match result {
        StatementResult::Query(result) => assert_eq!(result.rows.len(), expected),
        other => panic!("scan returned {other:?}"),
    }
    elapsed
}

fn update(db: &mut Database, active: bool) -> u128 {
    let value = if active { "TRUE" } else { "FALSE" };
    let sql = format!("UPDATE bench SET active = {value} WHERE id = 1;");
    let started = Instant::now();
    let result = single(db, &sql);
    let elapsed = started.elapsed().as_micros();
    match result {
        StatementResult::Update { count } => assert_eq!(count, 1),
        other => panic!("update returned {other:?}"),
    }
    elapsed
}

fn single(db: &mut Database, sql: &str) -> StatementResult {
    let mut results = db
        .execute(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(results.len(), 1);
    results.pop().expect("one result")
}

fn repeat(samples: usize, mut operation: impl FnMut(usize) -> u128) -> Vec<u128> {
    (0..samples).map(&mut operation).collect()
}

fn print_samples(label: &str, samples: &[u128]) {
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let median = ordered[ordered.len() / 2];
    println!(
        "{label} min={} median={} max={} samples={samples:?}",
        ordered[0],
        median,
        ordered[ordered.len() - 1]
    );
}

fn temp_db() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rustdb-bench-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    dir.join("bench.db")
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unavailable".to_string())
}
