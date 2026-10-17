//! NOMT idle gate: commit a batch, sit idle, count context switches.
//!
//! `cargo run --release -p vuoxi-store --example nomt_idle -- [buckets] [idle_secs] [keys]`

use nomt::hasher::Blake3Hasher;
use nomt::{KeyReadWrite, Nomt, Options, SessionParams, WitnessMode};
use std::{collections::HashMap, fs, path::Path, time::Duration};

/// Context switches per thread id, plus the thread name.
fn switches() -> HashMap<String, (String, u64)> {
    let mut m = HashMap::new();
    for t in fs::read_dir("/proc/self/task").unwrap().flatten() {
        let Ok(s) = fs::read_to_string(t.path().join("status")) else {
            continue;
        };
        let name = fs::read_to_string(t.path().join("comm")).unwrap_or_default();
        let mut total = 0u64;
        for l in s.lines() {
            if l.starts_with("voluntary_ctxt_switches")
                || l.starts_with("nonvoluntary_ctxt_switches")
            {
                total += l
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
        }
        m.insert(
            t.file_name().to_string_lossy().into_owned(),
            (name.trim().into(), total),
        );
    }
    m
}

fn dir_size(p: &Path) -> (u64, u64) {
    let (mut apparent, mut blocks) = (0, 0);
    for e in fs::read_dir(p).unwrap().flatten() {
        let m = e.metadata().unwrap();
        if m.is_dir() {
            let (a, b) = dir_size(&e.path());
            apparent += a;
            blocks += b;
        } else {
            use std::os::unix::fs::MetadataExt;
            apparent += m.len();
            blocks += m.blocks() * 512;
        }
    }
    (apparent, blocks)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let buckets: u32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(64_000);
    let idle: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(60);
    let keys: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let path = std::env::current_dir().unwrap().join("target/nomt-idle-db");
    let _ = fs::remove_dir_all(&path);

    let mut o = Options::new();
    o.path(&path);
    o.hashtable_buckets(buckets);
    o.io_workers(1);
    o.preallocate_ht(false);
    o.commit_concurrency(1);
    o.page_cache_size(32);
    o.leaf_cache_size(32);
    let db = Nomt::<Blake3Hasher>::open(o).unwrap();
    println!("opened: threads={}", switches().len());

    let sess = db.begin_session(SessionParams::default().witness_mode(WitnessMode::disabled()));
    let mut actuals: Vec<_> = (0..keys)
        .map(|i| {
            let k: [u8; 32] = blake3::hash(&i.to_le_bytes()).into();
            (k, KeyReadWrite::Write(Some(vec![7u8; 64])))
        })
        .collect();
    actuals.sort_by_key(|a| a.0);
    let fin = sess.finish(actuals).unwrap();
    let root = fin.root();
    fin.commit(&db).unwrap();
    println!("committed root {root:?}");

    std::thread::sleep(Duration::from_secs(10)); // let commit threads settle
    let before = switches();
    std::thread::sleep(Duration::from_secs(idle));
    let after = switches();
    let mut delta = 0u64;
    for (tid, (name, n1)) in &after {
        let n0 = before.get(tid).map_or(0, |b| b.1);
        let d = n1 - n0;
        delta += d;
        println!("  tid {tid} {name:<16} +{d}");
    }
    let (t0, t1) = (before.len(), after.len());
    println!("{:?}", db.hash_table_utilization());
    let (a, b) = dir_size(&path);
    println!("buckets={buckets} threads={t0}->{t1} idle={idle}s switches={delta}");
    println!("switches/s={:.3}", delta as f64 / idle as f64);
    println!("disk apparent={} MiB allocated={} MiB", a >> 20, b >> 20);
}
