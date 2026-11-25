//! A build's first failure without `--keep_going` interrupts the actions in
//! flight (crate::interrupt, docs/FAILFAST_INTERRUPT.md), pinned by a genuine
//! capture (tests/fixtures/failfast_interrupt: capture.py,
//! reference_receipt.json). Three 15-second sleepers, a 15-second test or a
//! 15-second persistent-worker request run beside an action that fails: the
//! command ends right after the failure with the failing action's exit code
//! and console, the interrupted actions counted as internal processes; no
//! process of theirs survives, nothing of theirs is cached (the disk cache
//! stays empty, the rerun executes again) and the cache's work directory is
//! empty afterwards. Declared divergence (the receipt's notes): bazel-rs
//! publishes nothing of an interrupted action into the output tree.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
include!("support/explicit_modules.rs");

const SLEEPERS: [&str; 3] = ["//pkg:sleeper0", "//pkg:sleeper1", "//pkg:sleeper2"];

/// The lines of the console a case compares (capture.py's `console`).
const DROPPED: [&str; 12] = [
    "Computing main repo mapping",
    "Loading:",
    "Analyzing:",
    "INFO: Analyzed",
    "INFO: Found",
    "INFO: Invocation ID",
    "INFO: Elapsed time",
    "INFO: Build completed",
    "OpenJDK",
    "Extracting Bazel installation",
    "bazel-rs:",
    "[",
];

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/failfast_interrupt")
}

fn console(text: &str, ws: &Path) -> Value {
    let ws = ws.display().to_string();
    text.replace(&ws, "<WS>")
        .lines()
        .filter(|line| !line.trim().is_empty() && !DROPPED.iter().any(|p| line.starts_with(p)))
        .map(|line| Value::from(line.to_owned()))
        .collect()
}

/// The sleepers', test's and worker's processes still running.
fn survivors() -> Vec<String> {
    let output = Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            (line.contains("sleep 15.073") || line.contains("slow_worker.py"))
                && !line.contains("ps -eo")
        })
        .map(|line| line.trim().to_owned())
        .collect()
}

/// The regular files of bazel-bin/pkg with their contents (None: no such
/// directory).
fn listing(directory: &Path) -> Option<Value> {
    let entries = fs::read_dir(directory).ok()?;
    let mut files = serde_json::Map::new();
    for entry in entries.flatten() {
        if entry.path().is_file() {
            files.insert(
                entry.file_name().to_string_lossy().into_owned(),
                fs::read_to_string(entry.path()).unwrap().into(),
            );
        }
    }
    Some(Value::Object(files))
}

fn disk_cache_files(directory: &Path) -> usize {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                disk_cache_files(&path)
            } else {
                usize::from(!entry.file_name().to_string_lossy().starts_with('.'))
            }
        })
        .sum()
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// Seconds from the first ERROR line on stderr to the exit.
    after_failure: Option<Duration>,
}

/// Runs bazel-rs; stderr is read as it arrives to time the failure.
fn run(ws: &Path, cache: &Path, home: &Path, disk_cache: &Path, command: &[&str]) -> Run {
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        manifest().display()
    );
    let mut child = Command::new(TIMEOUT)
        .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", cache.display()))
        .arg(command[0])
        .args(["--experimental-builtin-modules", &registry])
        .arg(format!("--disk_cache={}", disk_cache.display()))
        .args(&command[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout_pipe = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout_pipe.read_to_string(&mut text).unwrap();
        text
    });
    let mut stderr = String::new();
    let mut failed_at = None;
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line.unwrap();
        if failed_at.is_none() && line.starts_with("ERROR:") {
            failed_at = Some(Instant::now());
        }
        stderr.push_str(&line);
        stderr.push('\n');
    }
    let status = child.wait().unwrap();
    let ended = Instant::now();
    let stdout = reader.join().unwrap();
    assert_ne!(
        status.code(),
        Some(124),
        "CLI timed out: {command:?}\n{stderr}"
    );
    Run {
        code: status.code(),
        stdout,
        stderr,
        after_failure: failed_at.map(|at| ended - at),
    }
}

#[test]
fn the_first_failure_interrupts_the_actions_in_flight() {
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let rows = receipt["rows"].as_array().unwrap();
    let genuine = |case: &str| rows.iter().find(|row| row["case"] == case).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("failfast-interrupt-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let home = scratch.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let build: Vec<&str> = ["build"]
        .into_iter()
        .chain(SLEEPERS)
        .chain(["//pkg:fail", "//pkg:after"])
        .collect();
    let standalone: Vec<&str> = ["build", "--spawn_strategy=standalone"]
        .into_iter()
        .chain(SLEEPERS)
        .chain(["//pkg:fail"])
        .collect();
    let cases: [(&str, Vec<&str>); 5] = [
        ("build", build),
        // Reuses the build case's workspace and caches.
        ("rerun", vec!["build", SLEEPERS[0]]),
        ("standalone", standalone),
        ("test", vec!["test", "//pkg:slow_test", "//pkg:fail_late"]),
        ("worker", vec!["build", "//pkg:slow_work", "//pkg:fail"]),
    ];
    let (mut ws, mut cache, mut disk_cache) = Default::default();
    for (case, command) in cases {
        if case != "rerun" {
            ws = scratch.path().join(format!("ws-{case}"));
            copy(&fixture().join("workspace"), &ws);
            cache = scratch.path().join(format!("cache-{case}"));
            disk_cache = scratch.path().join(format!("disk-cache-{case}"));
        }
        let ran = run(&ws, &cache, &home, &disk_cache, &command);
        let alive = survivors();
        let expected = genuine(case);
        let context = format!("{case}: {}{}", ran.stdout, ran.stderr);
        assert_eq!(
            ran.code,
            expected["exit_code"].as_i64().map(|code| code as i32),
            "{context}"
        );
        assert_eq!(expected["prompt"], json!(true), "{case}");
        if let Some(after) = ran.after_failure {
            assert!(
                after < Duration::from_secs(5),
                "{case}: ended {after:?} after the failure\n{context}"
            );
        }
        assert!(alive.is_empty(), "{case}: survivors {alive:?}");
        assert_eq!(json!(alive), expected["survivors"], "{case}");
        assert_eq!(
            json!(disk_cache_files(&disk_cache)),
            expected["disk_cache_files"],
            "{context}"
        );
        assert_eq!(console(&ran.stdout, &ws), expected["stdout"], "{context}");
        assert_eq!(console(&ran.stderr, &ws), expected["console"], "{context}");
        // The failed (and interrupted) actions are on the critical path
        // with the time they ran (CriticalPathComputer.actionComplete;
        // genuine 9.3.1 scratch capture: `Critical Path: 8.03s` for a
        // failing `sleep 8` genrule, with and without --keep_going): at
        // least the failing action's sleep.
        if case != "rerun" {
            let failing = if case == "test" { 8.0 } else { 1.0 };
            let critical: f64 = ran
                .stderr
                .split("Critical Path: ")
                .nth(1)
                .and_then(|rest| rest.split('s').next())
                .and_then(|seconds| seconds.parse().ok())
                .unwrap_or_else(|| panic!("no critical path: {context}"));
            assert!(critical >= failing, "{case}: critical path {critical}s");
        }
        let bin_pkg = listing(&ws.join("bazel-bin/pkg"));
        match case {
            "rerun" => assert_eq!(bin_pkg.as_ref(), Some(&expected["bin_pkg"]), "{context}"),
            // The completed actions' outputs are published: the test's
            // script, its repository mapping and runfiles manifest (whose
            // paths name each one's own execution root).
            "test" => {
                let files = bin_pkg.as_ref().unwrap().as_object().unwrap();
                let genuine = expected["bin_pkg"].as_object().unwrap();
                let names = |map: &serde_json::Map<String, Value>| {
                    map.keys()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                };
                assert_eq!(names(files), names(genuine), "{context}");
                for name in ["slow_test.sh", "slow_test.sh.repo_mapping"] {
                    assert_eq!(files[name], genuine[name], "{name}");
                }
            }
            // Declared divergence: nothing of an interrupted action is
            // published.
            _ => assert!(
                bin_pkg
                    .as_ref()
                    .is_none_or(|files| files.as_object().unwrap().is_empty()),
                "{case}: {bin_pkg:?}"
            ),
        }
        // The interrupted actions' work directories are gone.
        let work: Vec<_> = fs::read_dir(cache.join("work"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(work.is_empty(), "{case}: work/ keeps {work:?}");
    }
}
