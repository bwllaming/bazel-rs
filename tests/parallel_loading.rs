//! P2 of the parallel-analysis program (docs/PARALLEL_ANALYSIS.md,
//! oracle review §3): package loads of one pattern expansion run on a
//! `--loading_phase_threads` worker pool; the coordinator commits in
//! pattern order. These tests pin the determinism (§5): byte-identical
//! stdout/stderr/exit across thread counts and under worker jitter, the
//! §5.4 stress shapes (a mid-pattern fetch package, one `.bzl` with
//! top-level prints loaded by many packages, two failing siblings, deep
//! Starlark recursion on workers), and the inline fallback for
//! fetch-needing packages.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-parallel-loading-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='parallel_loading')\n");
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// `run_env(env, ["build", "--loading_phase_threads=N", ...])`.
    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .current_dir(&self.0);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A shared `.bzl` graph with top-level prints, loaded by many packages,
/// each building a genrule: the §5 A/B — one warm run, then
/// threads=1/2/8/32 and threads=8 with reproducible worker jitter, all
/// byte-identical (stdout, stderr, exit code).
#[test]
fn loading_is_byte_identical_across_thread_counts_and_jitter() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/helper.bzl",
        "print('helper top-level')\ndef helper(x):\n    return x\n",
    );
    ws.file(
        "rules/lib.bzl",
        "load(':helper.bzl', 'helper')\nprint('lib top-level')\ndef _impl(ctx):\n    print('impl ' + ctx.label.package)\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl)\n",
    );
    for index in 0..12 {
        ws.file(
            &format!("p{index:02}/BUILD"),
            &format!(
                "load('//rules:lib.bzl', 'my_rule')\nmy_rule(name = 'r{index:02}')\ngenrule(name = 'g{index:02}', srcs = ['in.txt'], outs = ['out{index:02}.txt'], cmd = 'cp $(location in.txt) $@')\n"
            ),
        );
        ws.file(
            &format!("p{index:02}/in.txt"),
            &format!("package {index}\n"),
        );
    }
    // Warm every action cache so the summary lines match across runs.
    let warm = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let reference = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(reference.status.success(), "{}", stderr(&reference));
    for (threads, jitter) in [(1usize, 0u64), (2, 0), (8, 0), (32, 0), (8, 40)] {
        let jitter_text = jitter.to_string();
        let env: &[(&str, &str)] = if jitter > 0 {
            &[("BAZEL_RS_LOADING_JITTER", jitter_text.as_str())]
        } else {
            &[]
        };
        let run = ws.run_env(
            env,
            &[
                "build",
                &format!("--loading_phase_threads={threads}"),
                "//...",
            ],
        );
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        assert_eq!(
            run.stdout, reference.stdout,
            "threads={threads}: stdout differs"
        );
        assert_eq!(
            run.stderr,
            reference.stderr,
            "threads={threads}: stderr differs\n--\n{}",
            stderr(&run)
        );
    }
    // The prints: once per module, in load-DFS order, then each
    // implementation's line in plan order — the sequential order.
    let reference_stderr = stderr(&reference);
    let prints = reference_stderr
        .lines()
        .filter(|line| {
            !line.starts_with("WARNING:")
                && !line.starts_with("INFO:")
                && !line.starts_with("bazel-rs: ")
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut expected = vec!["helper top-level".to_owned(), "lib top-level".to_owned()];
    for index in 0..12 {
        expected.push(format!("impl p{index:02}"));
    }
    assert_eq!(prints, expected);
}

/// Two failing packages among healthy siblings (§5.4): the FIRST error in
/// pattern order wins at every thread count, with the same message and
/// exit code as the sequential run. Both fail while their BUILD file
/// evaluates (the stage where parallel loading commits in order).
#[test]
fn first_error_in_pattern_order_wins_across_thread_counts() {
    let ws = Workspace::new();
    ws.file("a/BUILD", "filegroup(name = 'a')\n");
    ws.file(
        "b/BUILD",
        "load('//rules:defs.bzl', 'broken')\nfail('b stops first')\nbroken(name = 'b')\n",
    );
    ws.file("c/BUILD", "genrule(name = 'c', srcs = [], cmd = 'true')\n");
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/defs.bzl",
        "def _broken(ctx):\n    fail('broken implementation')\nbroken = rule(implementation = _broken)\n",
    );
    ws.file("d/BUILD", "filegroup(name = 'd')\n");
    let mut reference = None;
    for threads in [1usize, 2, 8, 32] {
        let run = ws.run(&[
            "build",
            &format!("--loading_phase_threads={threads}"),
            "//...",
        ]);
        assert!(!run.status.success(), "threads={threads} must fail");
        let text = stderr(&run);
        match &reference {
            None => reference = Some((run.stdout.clone(), text, run.status.code())),
            Some((stdout, error, code)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&text, error, "threads={threads}: first error differs");
                assert_eq!(run.status.code(), *code);
            }
        }
    }
    let (_, error, _) = reference.unwrap();
    assert!(error.contains("b stops first"), "{error}");
    assert!(
        !error.contains("missing value for mandatory attribute"),
        "{error}"
    );
}

/// A mid-pattern package whose `.bzl` lives in an unfetched external
/// repository (§5.4): the no-demand guard aborts the worker's unit, the
/// coordinator re-runs it inline — materializing the repository — and
/// the output matches the sequential run. Cold (the fallback runs) and
/// warm, at several thread counts.
#[test]
fn mid_pattern_fetch_needing_package_falls_back_inline() {
    let ws = Workspace::new();
    // A file:// registry whose single module is a local_path source: its
    // root materializes on first demand (the guarded slot a worker never
    // runs).
    ws.file("registry/bazel_registry.json", "{\"mirrors\": []}\n");
    ws.file(
        "registry/modules/ext/1.0/MODULE.bazel",
        "module(name='ext', version='1.0')\n",
    );
    ws.file(
        "registry/modules/ext/1.0/source.json",
        &format!(
            "{{\"type\": \"local_path\", \"path\": {}}}\n",
            json_string(&ws.0.join("ext_src"))
        ),
    );
    ws.file("ext_src/MODULE.bazel", "module(name='ext')\n");
    ws.file("ext_src/BUILD", "");
    ws.file(
        "ext_src/defs.bzl",
        "print('ext module top-level')\ndef _impl(ctx):\n    return DefaultInfo()\next_rule = rule(implementation = _impl)\n",
    );
    ws.file(
        "MODULE.bazel",
        "module(name='main')\nbazel_dep(name = 'ext', version = '1.0')\n",
    );
    for name in ["a", "mid", "z"] {
        ws.file(
            &format!("{name}/BUILD"),
            "load('@ext//:defs.bzl', 'ext_rule')\next_rule(name = 'r')\n",
        );
    }
    let registry = format!("--registry=file://{}", ws.0.join("registry").display());
    // Cold with workers: the mid-pattern demand hits the guard, the
    // coordinator's inline retry materializes the repository.
    let cold = ws.run(&[
        "build",
        &registry,
        "--lockfile_mode=off",
        "--loading_phase_threads=8",
        "//...",
    ]);
    assert!(cold.status.success(), "cold: {}", stderr(&cold));
    // Warm A/B across thread counts (and the sequential default).
    let mut reference = None;
    for threads in [1usize, 2, 8, 32] {
        let run = ws.run(&[
            "build",
            &registry,
            "--lockfile_mode=off",
            &format!("--loading_phase_threads={threads}"),
            "//...",
        ]);
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        match &reference {
            None => reference = Some((run.stdout.clone(), run.stderr.clone())),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&run.stderr, error, "threads={threads}: stderr");
            }
        }
    }
    let (_, error) = reference.unwrap();
    let error = String::from_utf8_lossy(&error);
    // The external module's prints: once, in the first loading package's
    // DFS order.
    assert_eq!(error.matches("ext module top-level").count(), 1);
}

/// Deep Starlark call stacks evaluated on workers (§5.4, §1.6): with the
/// 16 MiB worker stacks a 110-frame call chain (no recursion — this
/// profile rejects recursive functions, tests/no_recursion.rs) evaluates
/// on every worker exactly as on the main thread.
#[test]
fn deep_starlark_recursion_on_workers() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    // f0 calls f1 calls ... calls f109: a 110-frame chain, each frame a
    // distinct function, the call-stack shape of deep Starlark work.
    let mut chain = String::new();
    for index in 0..110 {
        chain.push_str(&format!(
            "def f{index}(x):\n    return f{}(x) + 1\n",
            index + 1
        ));
    }
    chain.push_str("def f110(x):\n    return x\nDEPTH = f0(0)\n");
    ws.file("rules/deep.bzl", &chain);
    for index in 0..8 {
        ws.file(
            &format!("p{index}/BUILD"),
            &format!(
                "load('//rules:deep.bzl', 'DEPTH')\ngenrule(name = 'g{index}', srcs = [], outs = ['o{index}'], cmd = 'echo %d > $@' % DEPTH)\n"
            ),
        );
    }
    let warm = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let sequential = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(sequential.status.success(), "{}", stderr(&sequential));
    for threads in [2usize, 8, 32] {
        let run = ws.run(&[
            "build",
            &format!("--loading_phase_threads={threads}"),
            "//...",
        ]);
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        assert_eq!(run.stdout, sequential.stdout, "threads={threads}");
        assert_eq!(run.stderr, sequential.stderr, "threads={threads}");
    }
    // Every generated output carries the recursion depth.
    for line in String::from_utf8_lossy(&sequential.stdout).lines() {
        let (_, path) = line.split_once('\t').unwrap();
        assert_eq!(fs::read_to_string(path).unwrap().trim(), "110", "{line}");
    }
}

/// The query loader parallelizes through the same engine path
/// (Engine::expand_pattern): identical query output across thread counts.
#[test]
fn query_output_is_identical_across_thread_counts() {
    let ws = Workspace::new();
    for index in 0..10 {
        ws.file(
            &format!("p{index:02}/BUILD"),
            &format!(
                "filegroup(name = 'f{index:02}')\ngenrule(name = 'g{index:02}', srcs = [], outs = ['o{index:02}'], cmd = 'touch $@')\n"
            ),
        );
    }
    let mut reference = None;
    for threads in [1usize, 2, 8, 32] {
        let run = ws.run(&[
            "query",
            &format!("--loading_phase_threads={threads}"),
            "//...",
        ]);
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        match &reference {
            None => reference = Some((run.stdout.clone(), run.stderr.clone())),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&run.stderr, error, "threads={threads}: stderr");
            }
        }
    }
    let (stdout, _) = reference.unwrap();
    assert_eq!(String::from_utf8_lossy(&stdout).lines().count(), 20);
}

/// `--loading_phase_threads` validation mirrors Bazel's converter
/// (LoadingPhaseThreadsOption): positive integers and `auto`.
#[test]
fn loading_phase_threads_option_validation() {
    let ws = Workspace::new();
    ws.file("BUILD.bazel", "filegroup(name = 'a')\n");
    for (value, accepted) in [
        ("1", true),
        ("8", true),
        ("auto", true),
        ("0", false),
        ("-4", false),
        ("many", false),
    ] {
        let run = ws.run(&["build", &format!("--loading_phase_threads={value}"), "//:a"]);
        assert_eq!(
            run.status.success(),
            accepted,
            "--loading_phase_threads={value}: {}",
            stderr(&run)
        );
        if !accepted {
            assert!(
                stderr(&run).contains("not a positive integer or 'auto'"),
                "{}",
                stderr(&run)
            );
        }
    }
    // The separate-value spelling works too.
    let run = ws.run(&["build", "--loading_phase_threads", "4", "//:a"]);
    assert!(run.status.success(), "{}", stderr(&run));
}

/// A JSON string literal (no serde_json dependency in the test).
fn json_string(value: &Path) -> String {
    let mut out = String::from("\"");
    for component in value.components() {
        let text = component.as_os_str().to_string_lossy();
        if text.is_empty() {
            continue;
        }
        if !out.ends_with('"') && !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(&text);
    }
    out.push('"');
    out
}

/// P2 follow-up fix 1: a FAILED load still replays the prints of `.bzl`
/// modules it took from the shared cache — the marker the loader pushes
/// at every cache hit, in DFS position. Package `a` loads the printing
/// `lib.bzl` and then fails; later siblings load the same module. Under
/// jitter the failing unit's module may well be a cache hit another
/// worker evaluated; the output must stay byte-identical to the
/// sequential run either way, with the module's prints exactly once.
#[test]
fn failed_load_replays_cache_hit_bzl_prints() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/lib.bzl",
        "print('lib top-level')\ndef _impl(ctx):\n    return DefaultInfo()\nok_rule = rule(implementation = _impl)\n",
    );
    // a: loads lib.bzl, then its BUILD evaluation fails.
    ws.file(
        "a/BUILD",
        "load('//rules:lib.bzl', 'ok_rule')\nok_rule(name = 'r')\nfail('a fails after the load')\n",
    );
    for index in 1..6 {
        ws.file(
            &format!("p{index:02}/BUILD"),
            "load('//rules:lib.bzl', 'ok_rule')\nok_rule(name = 'r')\n",
        );
    }
    let mut reference = None;
    for (threads, jitter) in [(1u64, 0u64), (2, 0), (8, 0), (32, 0), (8, 40)] {
        let jitter_text = jitter.to_string();
        let env: &[(&str, &str)] = if jitter > 0 {
            &[("BAZEL_RS_LOADING_JITTER", jitter_text.as_str())]
        } else {
            &[]
        };
        let run = ws.run_env(
            env,
            &[
                "build",
                &format!("--loading_phase_threads={threads}"),
                "//...",
            ],
        );
        assert!(!run.status.success(), "threads={threads}");
        let stderr = stderr(&run);
        // The module's prints survive the failed load, exactly once, and
        // precede the package's own error.
        assert_eq!(stderr.matches("lib top-level").count(), 1, "{stderr}");
        assert!(stderr.contains("a fails after the load"), "{stderr}");
        match &reference {
            None => reference = Some((run.stdout.clone(), stderr)),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr, error, "threads={threads}: stderr");
            }
        }
    }
}

/// The failing-module variant: `bad.bzl` prints and then fails; the
/// first loading package emits its partial prints with its error, and a
/// cache-hit of the stored failure never re-prints them.
#[test]
fn failing_bzl_prints_survive_via_marker_on_sibling_hits() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/bad.bzl", "print('bad evaluates')\nfail('boom')\n");
    // a loads a printing good module and the failing one; later siblings
    // load the failing one too (a cache hit of the stored failure).
    ws.file("rules/good.bzl", "print('good top-level')\nvalue = 1\n");
    ws.file(
        "a/BUILD",
        "load('//rules:good.bzl', 'value')\nload('//rules:bad.bzl', 'x')\n",
    );
    for index in 1..4 {
        ws.file(
            &format!("p{index:02}/BUILD"),
            "load('//rules:bad.bzl', 'x')\n",
        );
    }
    let mut reference = None;
    for (threads, jitter) in [(1u64, 0u64), (2, 0), (8, 0), (8, 40)] {
        let jitter_text = jitter.to_string();
        let env: &[(&str, &str)] = if jitter > 0 {
            &[("BAZEL_RS_LOADING_JITTER", jitter_text.as_str())]
        } else {
            &[]
        };
        let run = ws.run_env(
            env,
            &[
                "build",
                &format!("--loading_phase_threads={threads}"),
                "//...",
            ],
        );
        assert!(!run.status.success(), "threads={threads}");
        let stderr = stderr(&run);
        assert_eq!(stderr.matches("good top-level").count(), 1, "{stderr}");
        assert_eq!(stderr.matches("bad evaluates").count(), 1, "{stderr}");
        assert!(stderr.contains("fail: boom"), "{stderr}");
        match &reference {
            None => reference = Some((run.stdout.clone(), stderr)),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr, error, "threads={threads}: stderr");
            }
        }
    }
}
