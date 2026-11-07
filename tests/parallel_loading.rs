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
include!("support/explicit_modules.rs");
include!("support/console.rs");
include!("support/sealed_trees.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = crate::test_temp::root().join(format!(
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
        command.explicit_modules().output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    without_print_locations(&without_elapsed(&String::from_utf8_lossy(&output.stderr)))
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
            stderr(&run),
            stderr(&reference),
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
            None => reference = Some((run.stdout.clone(), stderr(&run))),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr(&run), error, "threads={threads}: stderr");
            }
        }
    }
    let (_, error) = reference.unwrap();
    let error = error.as_str();
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
        assert_eq!(stderr(&run), stderr(&sequential), "threads={threads}");
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
            None => reference = Some((run.stdout.clone(), stderr(&run))),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr(&run), error, "threads={threads}: stderr");
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

/// P2 follow-up 2, fix 1: a cached FAILURE hit replays the failing
/// module's dependencies' prints too — `F` loads the printing `D` and
/// then fails at evaluation; both `p1` and `p2` load `F`. Whichever
/// worker evaluates D and F first, the first package in order replays
/// D's lines, F's partial lines and the error, byte-identical to the
/// sequential DFS (jitter shuffles who evaluates).
#[test]
fn cached_failure_replays_dependency_prints_in_postorder() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/d.bzl", "print('D top-level')\nv = 1\n");
    ws.file(
        "rules/f.bzl",
        "load(':d.bzl', 'v')\nprint('F partial')\nfail('F boom')\n",
    );
    for package in ["p1", "p2", "p3"] {
        ws.file(&format!("{package}/BUILD"), "load('//rules:f.bzl', 'x')\n");
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
        assert_eq!(stderr.matches("D top-level").count(), 1, "{stderr}");
        assert_eq!(stderr.matches("F partial").count(), 1, "{stderr}");
        assert!(stderr.contains("F boom"), "{stderr}");
        match &reference {
            None => reference = Some((run.stdout.clone(), stderr)),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr, error, "threads={threads}: stderr");
            }
        }
    }
}

/// P2 follow-up 2, fix 2: a module aborted mid-evaluation by the
/// no-demand sentinel suppresses its partial prints — the inline re-run
/// emits the module's FULL lines. The module prints, parses a label of
/// an unfetched repository (the mid-evaluation demand), prints again.
#[test]
fn sentinel_aborted_module_prints_fully_on_the_inline_rerun() {
    let ws = Workspace::new();
    // The local file registry from the mid-pattern fixture: the ext
    // repository materializes on first demand.
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
        "MODULE.bazel",
        "module(name='main')\nbazel_dep(name = 'ext', version = '1.0')\n",
    );
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/m.bzl",
        "print('M before the demand')\nl = Label('@ext//:x')\nprint('M after the demand')\ndef _impl(ctx):\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl)\n",
    );
    for name in ["a", "z"] {
        ws.file(
            &format!("{name}/BUILD"),
            "load('//rules:m.bzl', 'my_rule')\nmy_rule(name = 'r')\n",
        );
    }
    let registry = format!("--registry=file://{}", ws.0.join("registry").display());
    // Cold with workers: the mid-evaluation demand hits the guard, the
    // partial line is suppressed, the inline re-run prints both.
    let cold = ws.run(&[
        "build",
        &registry,
        "--lockfile_mode=off",
        "--loading_phase_threads=8",
        "//...",
    ]);
    assert!(cold.status.success(), "cold: {}", stderr(&cold));
    let cold_stderr = stderr(&cold);
    assert_eq!(cold_stderr.matches("M before the demand").count(), 1);
    assert_eq!(cold_stderr.matches("M after the demand").count(), 1);
    // Warm A/B across thread counts (and jitter) stays byte-identical.
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
                &registry,
                "--lockfile_mode=off",
                &format!("--loading_phase_threads={threads}"),
                "//...",
            ],
        );
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        match &reference {
            None => reference = Some((run.stdout.clone(), stderr(&run))),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&stderr(&run), error, "threads={threads}: stderr");
            }
        }
    }
    let (_, error) = reference.unwrap();
    let error = error.as_str();
    assert_eq!(error.matches("M before the demand").count(), 1);
    assert_eq!(error.matches("M after the demand").count(), 1);
}

/// N62 per-label single-flight: a parallel-`.bzl` graph shape — a diamond
/// (`left` and `right` both load the printing `base`) and a wide fan-in
/// (ten modules all loading the printing `fan`), reached from many
/// packages in shuffled roots, so two workers concurrently load a third
/// printing module: one evaluation, one print record, the other
/// traversal a cache hit. Output stays byte-identical across thread
/// counts and under jitter.
#[test]
fn parallel_bzl_graph_shapes_stay_identical_across_thread_counts() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    // The diamond: two modules load the same printing module.
    ws.file("rules/base.bzl", "print('base top-level')\nv = 1\n");
    ws.file(
        "rules/left.bzl",
        "load('//rules:base.bzl', 'v')\nprint('left top-level')\nl = v\n",
    );
    ws.file(
        "rules/right.bzl",
        "load('//rules:base.bzl', 'v')\nprint('right top-level')\nr = v\n",
    );
    // The wide fan-in: ten modules all load the printing fan module.
    ws.file("rules/fan.bzl", "print('fan top-level')\nf = 1\n");
    for index in 0..10 {
        ws.file(
            &format!("rules/wide{index}.bzl"),
            "load('//rules:fan.bzl', 'f')\nw = f\n",
        );
    }
    // Packages reach the shared modules from different roots in different
    // orders, so concurrent workers converge on `base` and `fan` from
    // both sides (p01/p02 the diamond, p03.. the fan-in).
    ws.file(
        "p01/BUILD",
        "load('//rules:left.bzl', 'l')\nload('//rules:right.bzl', 'r')\n",
    );
    ws.file(
        "p02/BUILD",
        "load('//rules:right.bzl', 'r')\nload('//rules:left.bzl', 'l')\n",
    );
    for index in 3..12 {
        let mut build = String::new();
        for wide in (0..10).rev() {
            build.push_str(&format!("load('//rules:wide{wide}.bzl', 'w')\n"));
        }
        ws.file(&format!("p{index:02}/BUILD"), &build);
    }
    let warm = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let mut reference = None;
    for (threads, jitter) in [(1usize, 0u64), (2, 0), (8, 0), (32, 0), (8, 40), (32, 25)] {
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
        let text = stderr(&run);
        for line in [
            "base top-level",
            "left top-level",
            "right top-level",
            "fan top-level",
        ] {
            assert_eq!(text.matches(line).count(), 1, "threads={threads}: {text}");
        }
        match &reference {
            None => reference = Some((run.stdout.clone(), text)),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&text, error, "threads={threads}: stderr");
            }
        }
    }
}

/// N62, the reachable cycle shape: a load CYCLE (a loads b, b loads a)
/// reached from two packages with different roots — whichever worker
/// meets it first, the first package in pattern order reports the cycle
/// with the sequential traversal's message, byte-identical at every
/// thread count (the cross-thread wait-cycle abort re-runs inline and
/// cannot change it).
#[test]
fn cross_package_load_cycle_message_is_stable_across_thread_counts() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/a.bzl", "load('//rules:b.bzl', 'b')\na = b\n");
    ws.file("rules/b.bzl", "load('//rules:a.bzl', 'a')\nb = a\n");
    ws.file("p01/BUILD", "load('//rules:a.bzl', 'a')\n");
    ws.file("p02/BUILD", "load('//rules:b.bzl', 'b')\n");
    let mut reference = None;
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
        assert!(!run.status.success(), "threads={threads} must fail");
        let text = stderr(&run);
        // The cycle is reported from p01's root (pattern order), naming
        // both modules of the cycle, exactly once.
        assert!(text.contains("load cycle"), "threads={threads}: {text}");
        assert!(text.contains("a.bzl"), "threads={threads}: {text}");
        assert!(text.contains("b.bzl"), "threads={threads}: {text}");
        assert_eq!(text.matches("load cycle").count(), 1, "threads={threads}");
        match &reference {
            None => reference = Some((run.stdout.clone(), text)),
            Some((stdout, error)) => {
                assert_eq!(&run.stdout, stdout, "threads={threads}: stdout");
                assert_eq!(&text, error, "threads={threads}: stderr");
            }
        }
    }
}

/// N62 action-graph A/B: the same warm fixture built at threads
/// 1/8/32 and under jitter produces a byte-identical
/// `BAZEL_RS_DUMP_ACTIONS` dump (ids, argv, inputs, outputs, env,
/// execution info — review §5) after normalizing the per-run repository
/// staging paths, and identical stdout/stderr/exit. The dump is on
/// stderr, gated on the variable's presence (any value).
#[test]
fn action_dump_is_identical_across_thread_counts_and_jitter() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/lib.bzl",
        "load('//rules:base.bzl', 'v')\nprint('lib top-level')\ndef _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, 'value=' + str(v))\n    return DefaultInfo(files = depset([out]))\nw_rule = rule(implementation = _impl)\n",
    );
    ws.file("rules/base.bzl", "print('base top-level')\nv = 41\n");
    for index in 0..8 {
        ws.file(
            &format!("p{index:02}/BUILD"),
            &format!(
                "load('//rules:lib.bzl', 'w_rule')\nw_rule(name = 'w{index:02}')\ngenrule(name = 'g{index:02}', srcs = [':w{index:02}'], outs = ['o{index:02}.txt'], cmd = 'cp $(location :w{index:02}) $@')\n"
            ),
        );
    }
    let normalize = |text: &str| -> String {
        // Per-run repository staging paths (pre-existing variance, only
        // ever visible in this debug dump): trees/<64-hex> directories.
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find("trees/") {
            out.push_str(&rest[..at]);
            out.push_str("trees/HASH");
            let after = &rest[at + "trees/".len()..];
            let skip = after
                .char_indices()
                .take(64)
                .take_while(|(_, c)| c.is_ascii_hexdigit())
                .count();
            rest = &after[skip.min(after.len())..];
            if skip == 0 {
                break;
            }
        }
        out.push_str(rest);
        out
    };
    let dump = |threads: usize, jitter: u64| -> (std::process::Output, String) {
        let jitter_text = jitter.to_string();
        let mut env: Vec<(&str, &str)> = vec![("BAZEL_RS_DUMP_ACTIONS", "1")];
        if jitter > 0 {
            env.push(("BAZEL_RS_LOADING_JITTER", jitter_text.as_str()));
        }
        let run = ws.run_env(
            &env,
            &[
                "build",
                &format!("--loading_phase_threads={threads}"),
                "//...",
            ],
        );
        let text = stderr(&run);
        let actions = text
            .lines()
            .filter(|line| line.starts_with("ACTION ") || line.starts_with("  "))
            .collect::<Vec<_>>()
            .join("\n");
        (run, normalize(&actions))
    };
    // Warm the action cache so executed/cached summaries cannot differ.
    let warm = ws.run(&["build", "--loading_phase_threads=1", "//..."]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let (reference, reference_dump) = dump(1, 0);
    assert!(reference.status.success(), "{}", stderr(&reference));
    assert!(
        reference_dump.contains("ACTION "),
        "dump must contain actions: {reference_dump}"
    );
    for (threads, jitter) in [(8usize, 0u64), (32, 0), (8, 40), (32, 25)] {
        let (run, text) = dump(threads, jitter);
        assert!(run.status.success(), "threads={threads}: {}", stderr(&run));
        assert_eq!(run.stdout, reference.stdout, "threads={threads}: stdout");
        assert_eq!(text, reference_dump, "threads={threads}: action dump");
        // stderr outside the dump (prints, summaries) is identical too.
        let clean = |output: &std::process::Output| -> String {
            stderr(output)
                .lines()
                .filter(|line| !(line.starts_with("ACTION ") || line.starts_with("  ")))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(clean(&run), clean(&reference), "threads={threads}: stderr");
    }
}

/// A file:// registry of local_path modules `names` (sources under
/// `<name>_src`, each with a printing `defs.bzl`): their roots materialize
/// on the coordinator's first demand, so every worker unit loading from
/// them aborts with the sentinel and re-runs once the root is there.
fn local_registry(ws: &Workspace, names: &[&str]) -> String {
    ws.file("registry/bazel_registry.json", "{\"mirrors\": []}\n");
    let mut module = String::from("module(name='main')\n");
    for name in names {
        ws.file(
            &format!("registry/modules/{name}/1.0/MODULE.bazel"),
            &format!("module(name='{name}', version='1.0')\n"),
        );
        ws.file(
            &format!("registry/modules/{name}/1.0/source.json"),
            &format!(
                "{{\"type\": \"local_path\", \"path\": {}}}\n",
                json_string(&ws.0.join(format!("{name}_src")))
            ),
        );
        ws.file(
            &format!("{name}_src/MODULE.bazel"),
            &format!("module(name='{name}')\n"),
        );
        ws.file(&format!("{name}_src/BUILD"), "");
        ws.file(
            &format!("{name}_src/defs.bzl"),
            &format!(
                "print('{name} top-level')\ndef _impl(ctx):\n    return DefaultInfo()\n{name}_rule = rule(implementation = _impl)\n"
            ),
        );
        module.push_str(&format!("bazel_dep(name = '{name}', version = '1.0')\n"));
    }
    ws.file("MODULE.bazel", &module);
    format!("--registry=file://{}", ws.0.join("registry").display())
}

/// Runs `args` at threads 1/2/8/32 and 8 with jitter: stdout, stderr and
/// the exit code are the sequential run's. Returns the reference stderr.
fn assert_identical_across_threads(ws: &Workspace, args: &[&str]) -> String {
    let mut reference: Option<(Vec<u8>, String, Option<i32>)> = None;
    for (threads, jitter) in [(1u64, 0u64), (2, 0), (8, 0), (32, 0), (8, 40)] {
        let jitter_text = jitter.to_string();
        let env: &[(&str, &str)] = if jitter > 0 {
            &[("BAZEL_RS_LOADING_JITTER", jitter_text.as_str())]
        } else {
            &[]
        };
        let threads_flag = format!("--loading_phase_threads={threads}");
        let mut all = args.to_vec();
        all.insert(1, &threads_flag);
        let run = ws.run_env(env, &all);
        let current = (run.stdout.clone(), stderr(&run), run.status.code());
        match &reference {
            None => reference = Some(current),
            Some(expected) => {
                assert_eq!(current.0, expected.0, "threads={threads}: stdout");
                assert_eq!(
                    current.1, expected.1,
                    "threads={threads} jitter={jitter}: stderr\n--\n{}",
                    current.1
                );
                assert_eq!(current.2, expected.2, "threads={threads}: exit");
            }
        }
    }
    reference.unwrap().1
}

/// How many blocked units the pattern's coordinator queued again on the
/// workers (`PERF-PATTERN retried=`, summed over the pattern's loads).
fn pattern_retries(output: &Output) -> u64 {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("PERF-PATTERN "))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("retried="))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .sum()
}

/// Pattern loading re-runs blocked units on the workers (docs/COLD_ANALYSIS.md,
/// "Pattern loading: blocked units re-run on the workers"): packages that
/// need several repositories, repositories needed by many packages, `.bzl`
/// and BUILD prints in every package — the first package of a demand
/// re-runs inline, the later ones on the workers, in waves. Cold and warm,
/// the output is the sequential run's at every thread count and under
/// jitter, prints once per module in the sequential order.
#[test]
fn blocked_units_rerun_on_workers_with_the_sequential_output() {
    let ws = Workspace::new();
    let registry = local_registry(&ws, &["ext1", "ext2", "ext3"]);
    for index in 0..10 {
        // One repository, needed by many packages.
        ws.file(
            &format!("one/p{index:02}/BUILD"),
            &format!("load('@ext1//:defs.bzl', 'ext1_rule')\nprint('one p{index:02}')\next1_rule(name = 'r')\n"),
        );
    }
    for index in 0..6 {
        // Several repositories per package, in two load orders.
        let (first, second) = if index % 2 == 0 {
            ("ext2", "ext3")
        } else {
            ("ext3", "ext2")
        };
        ws.file(
            &format!("several/q{index:02}/BUILD"),
            &format!(
                "load('@{first}//:defs.bzl', '{first}_rule')\nload('@{second}//:defs.bzl', '{second}_rule')\nload('@ext1//:defs.bzl', 'ext1_rule')\nprint('several q{index:02}')\n{first}_rule(name = 'a')\n{second}_rule(name = 'b')\next1_rule(name = 'c')\n"
            ),
        );
    }
    ws.file("plain/BUILD", "print('plain')\nfilegroup(name = 'p')\n");
    let args = [
        "build",
        registry.as_str(),
        "--lockfile_mode=off",
        "--nobuild",
        "//...",
    ];
    // Cold at 8 threads: the roots materialize at their sequential demand
    // and the later units re-run on the workers.
    let mut cold_args = args.to_vec();
    cold_args.insert(1, "--loading_phase_threads=8");
    let cold = ws.run_env(&[("BAZEL_RS_TIMING", "1")], &cold_args);
    assert!(cold.status.success(), "cold: {}", stderr(&cold));
    assert!(
        pattern_retries(&cold) > 0,
        "no unit re-ran on the workers: {}",
        stderr(&cold)
    );
    let error = assert_identical_across_threads(&ws, &args);
    let prints: Vec<&str> = error
        .lines()
        .filter(|line| line.starts_with("DEBUG") || !line.contains(':'))
        .collect();
    for name in ["ext1", "ext2", "ext3"] {
        assert_eq!(
            error.matches(&format!("{name} top-level")).count(),
            1,
            "{name}: {error}"
        );
    }
    for index in 0..10 {
        assert_eq!(error.matches(&format!("one p{index:02}")).count(), 1);
    }
    // BUILD prints in pattern order.
    let order: Vec<usize> = (0..10)
        .map(|index| error.find(&format!("one p{index:02}")).unwrap())
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{prints:?}");
}

/// Units blocked on a module extension re-run on the workers once the
/// coordinator evaluated it; an extension that fails fails the first
/// package (in pattern order) that needs it, with the sequential message,
/// at every thread count — with and without --keep_going.
#[test]
fn units_blocked_on_extensions_rerun_and_a_failing_extension_fails_identically() {
    let ws = Workspace::new();
    ws.file(
        "MODULE.bazel",
        "module(name='main')\ngood = use_extension('//:ext.bzl', 'good')\nuse_repo(good, 'gen')\nbad = use_extension('//:ext.bzl', 'bad')\nuse_repo(bad, 'badgen')\n",
    );
    ws.file("BUILD", "");
    ws.file(
        "ext.bzl",
        "print('ext.bzl top-level')\n\
def _repo_impl(rctx):\n    rctx.file('BUILD', '')\n    rctx.file('defs.bzl', \"print('gen defs top-level')\\ndef _impl(ctx):\\n    return DefaultInfo()\\ngen_rule = rule(implementation = _impl)\\n\")\n\
_repo = repository_rule(implementation = _repo_impl)\n\
def _good_impl(mctx):\n    _repo(name = 'gen')\n\
good = module_extension(implementation = _good_impl)\n\
def _bad_impl(mctx):\n    fail('the bad extension fails')\n\
bad = module_extension(implementation = _bad_impl)\n",
    );
    for index in 0..8 {
        ws.file(
            &format!("good/a{index}/BUILD"),
            &format!(
                "load('@gen//:defs.bzl', 'gen_rule')\nprint('a{index}')\ngen_rule(name = 'r')\n"
            ),
        );
    }
    ws.file("m/BUILD", "load('@badgen//:defs.bzl', 'x')\nprint('m')\n");
    for index in 0..4 {
        ws.file(
            &format!("z{index}/BUILD"),
            &format!(
                "load('@gen//:defs.bzl', 'gen_rule')\nprint('z{index}')\ngen_rule(name = 'r')\n"
            ),
        );
    }
    // The good packages alone: blocked on the extension, re-run on the
    // workers, identical output.
    let good_args = ["build", "--lockfile_mode=off", "--nobuild", "//good/..."];
    let good = assert_identical_across_threads(&ws, &good_args);
    assert_eq!(good.matches("gen defs top-level").count(), 1, "{good}");
    let mut timed = good_args.to_vec();
    timed.insert(1, "--loading_phase_threads=8");
    let timed = ws.run_env(&[("BAZEL_RS_TIMING", "1")], &timed);
    assert!(timed.status.success(), "{}", stderr(&timed));
    assert!(pattern_retries(&timed) > 0, "{}", stderr(&timed));
    // The failing extension: the first error in pattern order.
    let failing = assert_identical_across_threads(
        &ws,
        &["build", "--lockfile_mode=off", "--nobuild", "//..."],
    );
    assert!(failing.contains("the bad extension fails"), "{failing}");
    let keep_going = assert_identical_across_threads(
        &ws,
        &[
            "build",
            "--lockfile_mode=off",
            "--nobuild",
            "--keep_going",
            "//...",
        ],
    );
    assert!(
        keep_going.contains("the bad extension fails"),
        "{keep_going}"
    );
    for index in 0..4 {
        assert!(keep_going.contains(&format!("z{index}")), "{keep_going}");
    }
}
