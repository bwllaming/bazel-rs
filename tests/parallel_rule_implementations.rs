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
const RUNS: [(usize, u64); 5] = [(2, 0), (8, 0), (32, 0), (8, 25), (32, 40)];

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
    for (threads, jitter) in RUNS {
        let jitter_text = jitter.to_string();
        let mut run_env = env.to_vec();
        if jitter > 0 {
            run_env.push(("BAZEL_RS_ANALYSIS_JITTER", jitter_text.as_str()));
        }
        let threads_arg = format!("--loading_phase_threads={threads}");
        let mut run_args = vec![args[0], threads_arg.as_str()];
        run_args.extend(&args[1..]);
        let run = ws.run_env(&run_env, &run_args);
        let label = format!("threads={threads} jitter={jitter}");
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
        &[("BAZEL_RS_TIMING", "1"), ("BAZEL_RS_SNAPSHOT", "off")],
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
