//! P3 of the parallel-analysis program (docs/PARALLEL_ANALYSIS.md, "P3 —
//! rule implementations on analysis workers"): Starlark rule
//! implementations run on worker threads while the coordinator prepares and
//! commits in plan order. Everything a build shows must be the sequential
//! engine's (`--loading_phase_threads=1`): the action graph
//! (`BAZEL_RS_DUMP_ACTIONS`), `print()` lines in plan order, the first
//! error (and every `--keep_going` error, in order), cycles, the exit code —
//! at every thread count and under worker jitter
//! (`BAZEL_RS_ANALYSIS_JITTER`), which reorders when computes finish.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
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
            "bazel-rs-parallel-impls-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='parallel_impls')\n");
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command.arg("--ignore_all_rc_files");
        if !args.iter().any(|arg| arg.starts_with("--cache-dir=")) {
            command.arg("--cache-dir=cache");
        }
        command
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_ANALYSIS_JITTER")
            .env_remove("BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS")
            .current_dir(&self.0);
        for (key, value) in env {
            command.env(key, value);
        }
        command.explicit_modules().output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    without_elapsed(&String::from_utf8_lossy(&output.stderr))
}

/// The rules: a library whose implementation prints, burns a little
/// (varying) CPU, writes a file, runs an action over its dependencies'
/// files and returns a provider its dependents read; a failing rule.
const DEFS: &str = r#"
Info = provider(fields = ["files", "depth", "names"])

def _lib_impl(ctx):
    depth = 0
    names = []
    for dep in ctx.attr.deps:
        depth = max(depth, dep[Info].depth + 1)
        names.extend(dep[Info].names)
    # A compute whose length varies by target.
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    print("impl %s depth=%d total=%d" % (ctx.label, depth, total))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(sorted(names + [str(ctx.label)])))
    transitive = depset([out], transitive = [dep[Info].files for dep in ctx.attr.deps])
    summary = ctx.actions.declare_file(ctx.label.name + ".sum")
    ctx.actions.run_shell(
        inputs = transitive,
        outputs = [summary],
        command = "cat $@ > /dev/null; echo %d > %s" % (depth, summary.path),
        mnemonic = "Summarize",
    )
    return [
        DefaultInfo(files = depset([out, summary])),
        Info(files = transitive, depth = depth, names = names + [ctx.label.name]),
    ]

lib = rule(
    implementation = _lib_impl,
    attrs = {
        "deps": attr.label_list(providers = [Info]),
        "work": attr.int(default = 100),
    },
)

def _bad_impl(ctx):
    print("bad %s runs" % ctx.label)
    fail("%s fails" % ctx.label.name)

bad = rule(implementation = _bad_impl, attrs = {"deps": attr.label_list()})
"#;

/// A DAG of `lib` targets over several packages: chains, diamonds and
/// independent subtrees (what the pipeline can run concurrently).
fn library_workspace(ws: &Workspace) {
    ws.file("rules/BUILD", "");
    ws.file("rules/defs.bzl", DEFS);
    for package in 0..6 {
        let mut build = String::from(
            "load('//rules:defs.bzl', 'lib')\npackage(default_visibility = ['//visibility:public'])\n",
        );
        for index in 0..8 {
            let mut deps = Vec::new();
            if index > 0 {
                deps.push(format!("':t{}'", index - 1));
            }
            if index > 2 {
                deps.push(format!("':t{}'", index / 2));
            }
            if package > 0 && index % 3 == 0 {
                deps.push(format!("'//p{}:t{}'", package - 1, (index + 5) % 8));
            }
            let work = 500 + ((package * 7 + index * 13) % 11) * 3000;
            build.push_str(&format!(
                "lib(name = 't{index}', deps = [{}], work = {work})\n",
                deps.join(", ")
            ));
        }
        ws.file(&format!("p{package}/BUILD"), &build);
    }
}

/// The runs to compare with the sequential engine: thread counts, jitter.
/// The third element: BAZEL_RS_VERIFY_AHEAD=1 (every prepare ahead of its
/// turn is repeated at the turn and compared).
const RUNS: [(usize, u64, bool); 6] = [
    (2, 0, false),
    (8, 0, false),
    (32, 0, false),
    (8, 25, false),
    (32, 40, false),
    (8, 10, true),
];

fn assert_like_sequential(ws: &Workspace, env: &[(&str, &str)], args: &[&str]) -> Output {
    // Every run analyzes (no analysis snapshot serves it).
    let mut env = env.to_vec();
    env.push(("BAZEL_RS_SNAPSHOT", "off"));
    let env = env.as_slice();
    let mut sequential_args = vec![args[0], "--loading_phase_threads=1"];
    sequential_args.extend(&args[1..]);
    let reference = ws.run_env(env, &sequential_args);
    // Pipelining off at any thread count is the sequential engine too.
    let mut off_env = env.to_vec();
    off_env.push(("BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS", "1"));
    let mut off_args = vec![args[0], "--loading_phase_threads=8"];
    off_args.extend(&args[1..]);
    let off = ws.run_env(&off_env, &off_args);
    assert_eq!(off.status.code(), reference.status.code());
    assert_eq!(off.stdout, reference.stdout);
    assert_eq!(stderr(&off), stderr(&reference));
    for (threads, jitter, verify) in RUNS {
        let jitter_text = jitter.to_string();
        let mut run_env = env.to_vec();
        if jitter > 0 {
            run_env.push(("BAZEL_RS_ANALYSIS_JITTER", jitter_text.as_str()));
        }
        if verify {
            run_env.push(("BAZEL_RS_VERIFY_AHEAD", "1"));
        }
        let threads_arg = format!("--loading_phase_threads={threads}");
        let mut run_args = vec![args[0], threads_arg.as_str()];
        run_args.extend(&args[1..]);
        let run = ws.run_env(&run_env, &run_args);
        let label = format!("threads={threads} jitter={jitter} verify={verify}");
        assert_eq!(
            run.status.code(),
            reference.status.code(),
            "{label}: exit code\n{}",
            stderr(&run)
        );
        assert_eq!(run.stdout, reference.stdout, "{label}: stdout differs");
        assert_eq!(
            stderr(&run),
            stderr(&reference),
            "{label}: stderr differs\n-- run --\n{}\n-- sequential --\n{}",
            stderr(&run),
            stderr(&reference)
        );
    }
    reference
}

/// The action graph and the implementations' prints of a DAG of Starlark
/// targets: byte-identical to the sequential engine's at every thread
/// count and under jitter (`build --nobuild` with the full action dump).
#[test]
fn action_graph_and_prints_are_the_sequential_ones() {
    let ws = Workspace::new();
    library_workspace(&ws);
    let reference = assert_like_sequential(
        &ws,
        &[("BAZEL_RS_DUMP_ACTIONS", "1")],
        &["build", "--nobuild", "//..."],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
    let text = stderr(&reference);
    // Every implementation printed once, in plan order (dependencies
    // first): //p0:t0 before //p0:t1, and before //p1:t0 that needs it.
    assert_eq!(text.matches("impl @@//p0:t0 depth=0").count(), 1, "{text}");
    let position = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert!(position("impl @@//p0:t0 ") < position("impl @@//p0:t1 "));
    assert!(position("impl @@//p0:t5 ") < position("impl @@//p1:t0 "));
    assert!(text.contains("ACTION "), "the dump is on: {text}");
    // The pipeline ran: targets were prepared ahead of their turn and
    // computed on workers.
    let timed = ws.run_env(
        &[
            ("BAZEL_RS_TIMING", "1"),
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_VERIFY_AHEAD", "1"),
        ],
        &["build", "--loading_phase_threads=8", "--nobuild", "//..."],
    );
    let timed = stderr(&timed);
    let line = timed
        .lines()
        .find(|line| line.starts_with("PERF-PARALLEL "))
        .unwrap_or_else(|| panic!("no PERF-PARALLEL line: {timed}"));
    let ahead: u64 = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("ahead="))
        .and_then(|value| value.parse().ok())
        .unwrap();
    assert!(ahead > 0, "{line}");
    // BAZEL_RS_VERIFY_AHEAD prepared each of them again at its turn.
    let verified: u64 = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("verified="))
        .and_then(|value| value.parse().ok())
        .unwrap();
    assert_eq!(verified, ahead, "{line}");
}

/// Executing the same graph: outputs and the build's console are the
/// sequential ones.
#[test]
fn builds_execute_the_same_actions() {
    let ws = Workspace::new();
    library_workspace(&ws);
    let warm = ws.run_env(&[], &["build", "--loading_phase_threads=1", "//..."]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let reference = assert_like_sequential(&ws, &[], &["build", "//..."]);
    assert!(reference.status.success(), "{}", stderr(&reference));
}

/// Two independent failing implementations among succeeding ones: the
/// first error in plan order is the one reported (the commit order), its
/// print before it, nothing of the later failure; with `--keep_going`
/// every error, in plan order.
#[test]
fn errors_are_reported_in_the_sequential_order() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "z/BUILD",
        "load('//rules:defs.bzl', 'bad', 'lib')\npackage(default_visibility = ['//visibility:public'])\n\
         lib(name = 'ok1', deps = ['//p2:t7'], work = 20000)\n\
         bad(name = 'bad1', deps = [':ok1'])\n\
         lib(name = 'ok2', deps = ['//p4:t7'])\n\
         bad(name = 'bad2', deps = ['//p5:t3'])\n\
         lib(name = 'above', deps = [':bad1'])\n",
    );
    let reference = assert_like_sequential(&ws, &[], &["build", "--nobuild", "//..."]);
    assert!(!reference.status.success());
    let text = stderr(&reference);
    assert!(text.contains("bad1 fails"), "{text}");
    let keep_going =
        assert_like_sequential(&ws, &[], &["build", "--nobuild", "--keep_going", "//..."]);
    assert!(!keep_going.status.success());
    let text = stderr(&keep_going);
    assert!(
        text.contains("bad1 fails") && text.contains("bad2 fails"),
        "{text}"
    );
}

/// A dependency cycle among Starlark targets: phase 1 reports it, the
/// same at every thread count.
#[test]
fn cycles_are_reported_like_sequentially() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "c/BUILD",
        "load('//rules:defs.bzl', 'lib')\n\
         lib(name = 'a', deps = [':b', '//p3:t6'])\n\
         lib(name = 'b', deps = [':c'])\n\
         lib(name = 'c', deps = [':a'])\n",
    );
    let reference = assert_like_sequential(&ws, &[], &["build", "--nobuild", "//..."]);
    assert!(!reference.status.success());
    assert!(
        stderr(&reference).contains("cycle"),
        "{}",
        stderr(&reference)
    );
}

/// The analysis snapshot a pipelined analysis records serves the next run
/// exactly like the sequential one's: the hit replays the same console.
#[test]
fn snapshots_recorded_in_parallel_replay_the_sequential_output() {
    let ws = Workspace::new();
    library_workspace(&ws);
    let sequential_cache = "--cache-dir=cache-sequential";
    let parallel_cache = "--cache-dir=cache-parallel";
    let mut outputs = Vec::new();
    for (cache, threads) in [
        (sequential_cache, "--loading_phase_threads=1"),
        (parallel_cache, "--loading_phase_threads=8"),
    ] {
        let first = ws.run_env(
            &[("BAZEL_RS_ANALYSIS_JITTER", "20")],
            &[cache, "build", threads, "--nobuild", "//..."],
        );
        assert!(first.status.success(), "{}", stderr(&first));
        // Served by the snapshot (any thread count).
        let second = ws.run_env(
            &[],
            &[
                cache,
                "build",
                "--loading_phase_threads=1",
                "--nobuild",
                "//...",
            ],
        );
        assert!(second.status.success(), "{}", stderr(&second));
        outputs.push((first, second));
    }
    assert_eq!(stderr(&outputs[0].0), stderr(&outputs[1].0));
    assert_eq!(stderr(&outputs[0].1), stderr(&outputs[1].1));
    assert_eq!(outputs[0].1.stdout, outputs[1].1.stdout);
}

/// P3 review item 1: the incremental-analysis index of a build in which a
/// later target (`//t:j`) is prepared ahead of an earlier one (`//t:i`)
/// and so builds the shared dependency view of an alias first: the lookups
/// of the view's miss path count for `//t:i` too (every view-memo hit
/// records them), so the index is the sequential engine's.
#[test]
fn incremental_index_does_not_depend_on_who_built_a_view() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/defs.bzl", DEFS);
    ws.file(
        "r/BUILD",
        "load('//rules:defs.bzl', 'lib')\npackage(default_visibility = ['//visibility:public'])\nlib(name = 'real')\n",
    );
    ws.file(
        "t/BUILD",
        "load('//rules:defs.bzl', 'lib')\n\
         alias(name = 'd', actual = '//r:real')\n\
         lib(name = 'x', work = 300000)\n\
         lib(name = 'i', deps = [':d', ':x'])\n\
         lib(name = 'j', deps = [':d'])\n",
    );
    let mut indexes = Vec::new();
    for (run, threads) in ["1", "8", "8"].into_iter().enumerate() {
        let dump = ws.0.join(format!("index-{run}.txt"));
        let cache = format!("--cache-dir=cache-{run}");
        let threads = format!("--loading_phase_threads={threads}");
        let output = ws.run_env(
            &[("BAZEL_RS_SNAPSHOT_INDEX_DUMP", dump.to_str().unwrap())],
            &[&cache, "build", &threads, "//t:i", "//t:j"],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        let index = fs::read_to_string(&dump)
            .unwrap_or_else(|error| panic!("no index dump: {error}\n{}", stderr(&output)))
            .replace(&format!("cache-{run}/"), "cache-N/");
        indexes.push(index);
    }
    assert!(indexes[0].contains("label_accesses"), "{}", indexes[0]);
    assert_eq!(indexes[1], indexes[0], "threads 8: the index differs");
    assert_eq!(indexes[2], indexes[0], "threads 8 again: the index differs");
}

/// P3 review item 3: a build that fails at an early turn stops the
/// workers — computes queued for turns that never come are skipped and a
/// worker's (long) jitter sleep is interrupted — so it ends promptly, with
/// the sequential engine's output.
#[test]
fn a_failing_build_does_not_wait_for_speculative_computes() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "a/BUILD",
        "load('//rules:defs.bzl', 'bad')\nbad(name = 'first')\n",
    );
    let sequential = ws.run_env(
        &[("BAZEL_RS_SNAPSHOT", "off")],
        &[
            "build",
            "--loading_phase_threads=1",
            "--nobuild",
            "//a:first",
            "//p0:all",
            "//p1:all",
            "//p2:all",
        ],
    );
    assert!(!sequential.status.success());
    let started = std::time::Instant::now();
    let parallel = ws.run_env(
        &[
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_ANALYSIS_JITTER", "20000"),
        ],
        &[
            "build",
            "--loading_phase_threads=8",
            "--nobuild",
            "//a:first",
            "//p0:all",
            "//p1:all",
            "//p2:all",
        ],
    );
    let elapsed = started.elapsed();
    assert_eq!(parallel.status.code(), sequential.status.code());
    assert_eq!(stderr(&parallel), stderr(&sequential));
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "the failing build waited {elapsed:?} for speculative computes"
    );
}

/// P3 review item 2 (found on ~/code): a later target prepared ahead of its
/// turn builds the dependency view of a source file first (and registers
/// its artifact, stamped with its own turn); an earlier target that reaches
/// the same view at its turn through the view memo still resolves the file
/// as an action input, as sequentially.
#[test]
fn a_source_view_built_ahead_resolves_for_an_earlier_target() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/defs.bzl", DEFS);
    ws.file(
        "rules/src.bzl",
        r#"
load(":defs.bzl", "Info")

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        inputs = ctx.files.srcs + [f for dep in ctx.attr.deps for f in dep[Info].files.to_list()],
        outputs = [out],
        command = "cat $@ > /dev/null; touch " + out.path,
    )
    return [DefaultInfo(files = depset([out]))]

use_src = rule(
    implementation = _impl,
    attrs = {"srcs": attr.label_list(allow_files = True), "deps": attr.label_list()},
)
"#,
    );
    ws.file("t/src.txt", "source\n");
    ws.file(
        "t/BUILD",
        "load('//rules:defs.bzl', 'lib')\n\
         load('//rules:src.bzl', 'use_src')\n\
         lib(name = 'x', work = 300000)\n\
         use_src(name = 'i', srcs = ['src.txt'], deps = [':x'])\n\
         use_src(name = 'j', srcs = ['src.txt'])\n",
    );
    let reference = assert_like_sequential(
        &ws,
        &[("BAZEL_RS_DUMP_ACTIONS", "1")],
        &["build", "--nobuild", "//t:i", "//t:j"],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
}
