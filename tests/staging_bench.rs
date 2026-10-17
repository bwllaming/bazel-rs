//! N36 staging benchmark (docs/HARDLINK_STAGING.md), ignored by default:
//! STAGING_BENCH_FILES source files of STAGING_BENCH_KIB KiB (default 2000 x
//! 256 KiB = 500 MiB) shared by STAGING_BENCH_ACTIONS actions (default 50),
//! executed through scheduler::execute_graph with STAGING_BENCH_JOBS workers.
//! Reports cold and warm wall time and peak/final cache disk usage (du,
//! hardlinks counted once) as one JSON line. The harness only uses the
//! Action/PlannedAction API that predates N36, so the same file measures the
//! copy-staging baseline.
//!
//! cargo test --release --test staging_bench -- --ignored --nocapture
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use bazel_rs::action::{Action, ActionExtra};
use bazel_rs::scheduler::{self, PlannedAction};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn du_kib(path: &Path) -> u64 {
    let output = Command::new("du").arg("-sk").arg(path).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn run(cache: &Path, graph: Vec<PlannedAction>, jobs: usize) -> (f64, u64, usize) {
    let peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done, cache) = (peak.clone(), done.clone(), cache.to_path_buf());
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                peak.fetch_max(du_kib(&cache), Ordering::AcqRel);
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let start = Instant::now();
    let results = scheduler::execute_graph(cache, graph, jobs).unwrap();
    let seconds = start.elapsed().as_secs_f64();
    done.store(true, Ordering::Release);
    sampler.join().unwrap();
    let hits = results.values().filter(|result| result.cache_hit).count();
    peak.fetch_max(du_kib(cache), Ordering::AcqRel);
    (seconds, peak.load(Ordering::Acquire), hits)
}

#[test]
#[ignore = "benchmark; run explicitly"]
fn staging_benchmark() {
    let files = setting("STAGING_BENCH_FILES", 2000);
    let kib = setting("STAGING_BENCH_KIB", 256);
    let actions = setting("STAGING_BENCH_ACTIONS", 50);
    let jobs = setting("STAGING_BENCH_JOBS", 8);
    let scratch = tempfile::Builder::new()
        .prefix("staging-bench-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let sources = scratch.path().join("sources");
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut inputs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for index in 0..files {
        let relative = PathBuf::from(format!("sdk/d{:02}/f{index:04}.bin", index % 40));
        let path = sources.join(&relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::with_capacity(kib * 1024);
        while bytes.len() < kib * 1024 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.extend_from_slice(&seed.to_le_bytes());
        }
        fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        inputs.push((path, relative));
    }
    // Let the sources settle past the digest memo's racy guard, as sources
    // in a real workspace have.
    std::thread::sleep(Duration::from_secs(3));
    let graph: Vec<PlannedAction> = (0..actions)
        .map(|index| PlannedAction {
            action: Action {
                id: format!("consumer-{index:02}"),
                command: "find sdk -type f | wc -l > out.txt; head -c 8 sdk/d00/f0000.bin | od -An -tx1 >> out.txt".into(),
                inputs: inputs.clone(),
                outputs: vec!["out.txt".into()],
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
                extra: ActionExtra::default(),
..Default::default()
},
            dependencies: Vec::new(),
            deferred: None,
        })
        .collect();
    let cache = scratch.path().join("cache");
    let (cold, cold_peak, cold_hits) = run(&cache, graph.clone(), jobs);
    let cold_final = du_kib(&cache);
    let (warm, warm_peak, warm_hits) = run(&cache, graph, jobs);
    assert_eq!((cold_hits, warm_hits), (0, actions));
    println!(
        "STAGING_BENCH {{\"files\":{files},\"file_kib\":{kib},\"input_kib\":{},\"actions\":{actions},\"jobs\":{jobs},\"cold_seconds\":{cold:.3},\"cold_peak_cache_kib\":{cold_peak},\"cold_final_cache_kib\":{cold_final},\"warm_seconds\":{warm:.3},\"warm_peak_cache_kib\":{warm_peak},\"unix_time\":{}}}",
        du_kib(&sources),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let _ = Command::new("chmod")
        .args(["-R", "u+w"])
        .arg(scratch.path())
        .status();
}
