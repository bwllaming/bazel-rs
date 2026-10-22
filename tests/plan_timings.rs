//! M5 (docs/PARALLEL_ANALYSIS.md "P3 instrumentation") and W0-A
//! (docs/PERF_LAYER1.md): the example-run shape of the plan-stage report —
//! `BAZEL_RS_TIMING=1` adds PERF-STAGE lines (stderr only, stdout
//! untouched), counts identical at --loading_phase_threads=1/32, coverage
//! accounts for the span; the W0-A flush/view counters and the opt-in
//! `BAZEL_RS_TIMING_ALLOC=1` retained-memory breakdown.

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
        self.run_with(threads, &[("BAZEL_RS_TIMING", timing)])
    }

    /// `switches`: each (variable, set) pair is set to `1` or removed.
    fn run_with(&self, threads: &str, switches: &[(&str, bool)]) -> Output {
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
            .env_remove("BAZEL_RS_TIMING_ALLOC")
            .current_dir(&self.0);
        for (name, set) in switches {
            if *set {
                command.env(name, "1");
            }
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
        // W0-A rows: flush batches (none here: no Starlark flag_values
        // key) and the view / Artifact::file counters.
        assert!(
            stderr.contains("PERF-STAGE flush-batch flushes=0 planned=0 "),
            "{stderr}"
        );
        assert!(stderr.contains("PERF-STAGE views edges="), "{stderr}");
        // PERF marks keep the `PERF <t>s rss=<kB>kB <mark>` prefix and
        // append the peak RSS after the mark name.
        let mark = stderr
            .lines()
            .find(|l| l.starts_with("PERF ") && l.contains(" plan.start "))
            .unwrap_or_else(|| panic!("missing plan.start mark:\n{stderr}"));
        let fields: Vec<&str> = mark.split_whitespace().collect();
        assert_eq!(fields.len(), 5, "{mark}");
        assert!(fields[1].ends_with('s'), "{mark}");
        assert!(
            fields[2].starts_with("rss=") && fields[2].ends_with("kB"),
            "{mark}"
        );
        assert_eq!(fields[3], "plan.start", "{mark}");
        let hwm: u64 = fields[4]
            .strip_prefix("hwm=")
            .and_then(|v| v.strip_suffix("kB"))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no hwm in {mark}"));
        assert!(hwm > 0, "{mark}");
        assert!(
            !stderr.contains("PERF-ALLOC"),
            "the allocation report needs BAZEL_RS_TIMING_ALLOC:\n{stderr}"
        );
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

fn counters(line: &str) -> std::collections::BTreeMap<String, u64> {
    line.split_whitespace()
        .filter_map(|field| field.split_once('='))
        .filter_map(|(key, value)| Some((key.to_owned(), value.parse().ok()?)))
        .collect()
}

/// W0-A: a `config_setting` with a Starlark `flag_values` key flushes the
/// pending targets from phase 1 (Engine::plan_pending); the report counts
/// the flush's batch, and the dependency edge of `x` builds one view.
#[test]
fn flush_batches_and_view_counters() {
    let ws = Workspace::new();
    ws.file(
        "defs.bzl",
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, 'x')\n    return [DefaultInfo(files=depset([out]))]\nmy_rule = rule(implementation=_impl, attrs={'deps': attr.label_list()})\ndef _flag_impl(ctx):\n    return []\nbool_flag = rule(implementation=_flag_impl, build_setting=config.bool(flag=True))\n",
    );
    ws.file(
        "BUILD",
        "load(':defs.bzl', 'bool_flag', 'my_rule')\nbool_flag(name='flag', build_setting_default=True)\nconfig_setting(name='on', flag_values={':flag': 'true'})\nmy_rule(name='dep')\nmy_rule(name='x', deps=select({':on': [':dep'], '//conditions:default': []}))\n",
    );
    let plain = ws.run("1", false);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let out = ws.run("1", true);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, plain.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let flush = stderr
        .lines()
        .find(|l| l.starts_with("PERF-STAGE flush-batch "))
        .map(counters)
        .unwrap_or_else(|| panic!("missing flush-batch row:\n{stderr}"));
    assert!(flush["flushes"] >= 1, "{flush:?}\n{stderr}");
    assert!(flush["planned"] >= 1, "{flush:?}\n{stderr}");
    assert!(flush["max"] >= flush["median"], "{flush:?}");
    assert!(flush["phase2"] >= 1, "{flush:?}");
    let views = stderr
        .lines()
        .find(|l| l.starts_with("PERF-STAGE views "))
        .map(counters)
        .unwrap_or_else(|| panic!("missing views row:\n{stderr}"));
    assert!(views["edges"] >= 1, "{views:?}\n{stderr}");
    // Every request builds its view today (no memo).
    assert_eq!(views["constructed"], views["edges"], "{views:?}");
    assert!(views["distinct"] >= 1 && views["distinct"] <= views["edges"]);
    assert!(views["artifact_file"] >= 1, "{views:?}");
}

/// W0-A: `BAZEL_RS_TIMING_ALLOC=1` alone prints the retained-memory
/// breakdown (stderr only), with or without the timing report.
#[test]
fn allocation_report_is_opt_in_and_stderr_only() {
    let ws = Workspace::new();
    let plain = ws.run("1", false);
    assert!(plain.status.success());
    let out = ws.run_with("1", &[("BAZEL_RS_TIMING_ALLOC", true)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, plain.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!stderr.contains("PERF-STAGE"), "{stderr}");
    let rows: Vec<(u64, String)> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("PERF-ALLOC "))
        .map(|rest| {
            let (bytes, name) = rest.split_once(' ').unwrap();
            (bytes.parse().unwrap(), name.to_owned())
        })
        .collect();
    assert!(!rows.is_empty(), "{stderr}");
    // Largest first.
    assert!(rows.windows(2).all(|w| w[0].0 >= w[1].0), "{rows:?}");
    for name in [
        "packages",
        "plan",
        "action_planning.custom_providers",
        "shared:FrozenFrozenHeap",
    ] {
        assert!(
            rows.iter().any(|(bytes, row)| row == name && *bytes > 0),
            "missing {name}: {rows:?}"
        );
    }
    let summary = stderr
        .lines()
        .find(|l| l.starts_with("PERF-ALLOC-SUMMARY "))
        .unwrap_or_else(|| panic!("missing summary:\n{stderr}"));
    let total: u64 = summary
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total="))
        .and_then(|v| v.parse().ok())
        .unwrap();
    assert_eq!(total, rows.iter().map(|(bytes, _)| bytes).sum::<u64>());
    assert!(
        stderr
            .lines()
            .any(|l| l.starts_with("PERF-ALLOC-HEAP ") && l.contains(" custom_providers heaps=")),
        "{stderr}"
    );
    // Both switches: the stage report and the breakdown side by side.
    let both = ws.run_with(
        "1",
        &[("BAZEL_RS_TIMING", true), ("BAZEL_RS_TIMING_ALLOC", true)],
    );
    assert!(both.status.success());
    assert_eq!(both.stdout, plain.stdout);
    let stderr = String::from_utf8_lossy(&both.stderr);
    assert!(stderr.contains("PERF-STAGE total prepare "), "{stderr}");
    assert!(stderr.contains("PERF-ALLOC-SUMMARY "), "{stderr}");
}
