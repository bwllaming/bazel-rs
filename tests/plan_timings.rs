//! M5 (docs/PARALLEL_ANALYSIS.md "P3 instrumentation"): the example-run
//! shape of the plan-stage report — `BAZEL_RS_TIMING=1` adds PERF-STAGE
//! lines (stderr only, stdout untouched), counts identical at
//! --loading_phase_threads=1/32, and coverage accounts for the span.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-plan-timings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='plan_timings')\n");
        ws.file(
            "defs.bzl",
            "def _impl(ctx):\n    return [DefaultInfo()]\nmy_rule = rule(implementation=_impl)\n",
        );
        ws.file("BUILD", "load(':defs.bzl', 'my_rule')\nmy_rule(name='x')\n");
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, threads: &str, timing: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args([
                "build",
                "--nobuild",
                &format!("--loading_phase_threads={threads}"),
                "//:x",
            ])
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_TIMING_MODULES")
            .current_dir(&self.0);
        if timing {
            command.env("BAZEL_RS_TIMING", "1");
        }
        command.output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stage_total(stderr: &str, stage: &str) -> Option<(f64, u64)> {
    let line = stderr
        .lines()
        .find(|l| l.starts_with(&format!("PERF-STAGE total {stage} ")))?;
    let seconds: f64 = line
        .split_whitespace()
        .nth(3)?
        .trim_end_matches('s')
        .parse()
        .ok()?;
    let count: u64 = line.rsplit("count=").next()?.parse().ok()?;
    Some((seconds, count))
}

#[test]
fn stage_report_shape_and_thread_ab() {
    let ws = Workspace::new();
    let plain = ws.run("1", false);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&plain.stderr).contains("PERF-STAGE"),
        "no stage lines without BAZEL_RS_TIMING"
    );
    let mut counts = Vec::new();
    for threads in ["1", "32"] {
        let out = ws.run(threads, true);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Stage reporting is stderr-only: stdout matches the plain run.
        assert_eq!(out.stdout, plain.stdout, "threads={threads}");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        for stage in ["prepare", "compute", "commit"] {
            let (_, count) = stage_total(&stderr, stage)
                .unwrap_or_else(|| panic!("missing {stage} row:\n{stderr}"));
            counts.push((threads, stage, count));
        }
        let covered: f64 = stderr
            .lines()
            .find(|l| l.starts_with("PERF-STAGE total plan_span"))
            .and_then(|l| l.rsplit("covered=").next())
            .and_then(|v| v.trim_end_matches('%').parse().ok())
            .unwrap_or_else(|| panic!("missing plan_span row:\n{stderr}"));
        // Sub-millisecond span: timer overhead is visible here; the 90%
        // gate is verified on the real workload (docs/PARALLEL_ANALYSIS.md).
        assert!(covered >= 80.0, "coverage {covered}% < 80%:\n{stderr}");
        assert!(
            stderr.contains("PERF-STAGE critical-path compute="),
            "{stderr}"
        );
        assert!(stderr.contains("PERF-STAGE class"), "{stderr}");
    }
    // Same stage counts at both thread counts.
    for stage in ["prepare", "compute", "commit"] {
        let at = |t: &str| {
            counts
                .iter()
                .find(|(th, s, _)| *th == t && *s == stage)
                .map(|(_, _, c)| *c)
        };
        assert_eq!(at("1"), at("32"), "stage {stage} counts differ");
        assert!(at("1").unwrap() >= 1, "stage {stage} never recorded");
    }
}
