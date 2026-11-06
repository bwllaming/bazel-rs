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

def _bin_impl(ctx):
    print("bin %s" % ctx.label)
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "exit 0\n", is_executable = True)
    files = depset(transitive = [dep[Info].files for dep in ctx.attr.deps])
    return [
        DefaultInfo(executable = exe, runfiles = ctx.runfiles(transitive_files = files)),
        Info(files = depset([exe], transitive = [files]), depth = 0, names = [ctx.label.name]),
    ]

bin = rule(
    implementation = _bin_impl,
    executable = True,
    attrs = {"deps": attr.label_list(providers = [Info])},
)
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

/// P4 (deferred action lowering): a lowering that fails — of a Starlark
/// action or of a runfiles support's manifest/tree action, on a lowering
/// worker while the walk goes on — is the first error at its target's turn:
/// the prints before it and none after it, at every thread count and with
/// worker jitter; under --keep_going (lowered at the commit) the failing
/// targets are reported and their dependents skipped, as sequentially.
#[test]
fn lowering_failures_are_reported_at_their_turn() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "z/BUILD",
        "load('//rules:defs.bzl', 'bin', 'lib')\npackage(default_visibility = ['//visibility:public'])\n\
         lib(name = 'ok1', deps = ['//p2:t7'], work = 20000)\n\
         lib(name = 'low1', deps = [':ok1'])\n\
         bin(name = 'tool', deps = ['//p3:t5'])\n\
         lib(name = 'ok2', deps = ['//p4:t7', ':tool'])\n\
         lib(name = 'low2', deps = ['//p5:t3'])\n\
         lib(name = 'above', deps = [':low1', ':ok2'])\n",
    );
    for failing in [
        "//z:low1#1",
        "//z:tool#runfiles_tree",
        "//z:tool#runfiles_manifest,//z:low2#0",
        "//p0:t3#1,//z:low2#1",
    ] {
        let env = [("BAZEL_RS_LOWERING_FAIL", failing)];
        let reference = assert_like_sequential(&ws, &env, &["build", "--nobuild", "//..."]);
        assert!(!reference.status.success(), "{failing}");
        let text = stderr(&reference);
        let first = failing.split(',').next().unwrap();
        assert!(
            text.contains(&format!("lowering of {first} failed")),
            "{failing}: {text}"
        );
        let keep_going =
            assert_like_sequential(&ws, &env, &["build", "--nobuild", "--keep_going", "//..."]);
        assert!(!keep_going.status.success(), "{failing}");
        let text = stderr(&keep_going);
        assert!(
            text.contains(&format!("lowering of {first} failed")),
            "{failing}: {text}"
        );
    }
}

/// Rules of the commit-ahead workspace (P4): writes of a chosen path and
/// content (shared when equal, a conflict otherwise), a rule applying an
/// aspect to its dependencies, and genrules beside them.
const P4_DEFS: &str = r#"
def _w_impl(ctx):
    print("w %s" % ctx.label)
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    out = ctx.actions.declare_file(ctx.attr.out)
    ctx.actions.write(out, ctx.attr.content)
    files = depset([out], transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps])
    summary = ctx.actions.declare_file(ctx.label.name + ".sum")
    ctx.actions.run_shell(
        inputs = files,
        outputs = [summary],
        command = "cat $@ > /dev/null; echo %d > %s" % (total % 7, summary.path),
    )
    return [DefaultInfo(files = depset([summary], transitive = [files]))]

w = rule(
    implementation = _w_impl,
    attrs = {
        "out": attr.string(),
        "content": attr.string(default = "x"),
        "deps": attr.label_list(),
        "work": attr.int(default = 100),
    },
)

def _asp_impl(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".asp")
    ctx.actions.write(out, str(ctx.label))
    return [OutputGroupInfo(asp = depset([out]))]

asp = aspect(implementation = _asp_impl, attr_aspects = ["deps"])

def _with_aspect_impl(ctx):
    print("with_aspect %s" % ctx.label)
    out = ctx.actions.declare_file(ctx.label.name + ".top")
    ctx.actions.write(out, "top")
    return [DefaultInfo(files = depset([out]))]

with_aspect = rule(
    implementation = _with_aspect_impl,
    attrs = {"deps": attr.label_list(aspects = [asp])},
)

def _early_impl(ctx):
    on = ctx.target_platform_has_constraint(ctx.attr.value[platform_common.ConstraintValueInfo])
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, str(on))
    return [DefaultInfo(files = depset([out]))]

# Asks for the target platform: its commit resolves the deferred default
# one, so the targets prepared after it see it resolved (a prepare that saw
# it deferred commits at its turn).
early = rule(implementation = _early_impl, attrs = {"value": attr.label()})
"#;

/// The host (and so the default target) platform of the commit-ahead
/// workspace: `//_early:early` resolves it at its commit, early in the plan.
const P4_HOST: &str = "--host_platform=//_early:host";

/// Independent packages with chains of `w` targets, a genrule and a
/// subpackage each, a pair of equal (shared) actions, aspect
/// applications in the middle of the plan.
fn commit_ahead_workspace(ws: &Workspace) {
    ws.file("rules/BUILD", "");
    ws.file("rules/p4.bzl", P4_DEFS);
    ws.file(
        "_early/BUILD",
        "load('//rules:p4.bzl', 'early')\n\
         constraint_setting(name = 'setting')\n\
         constraint_value(name = 'value', constraint_setting = ':setting')\n\
         platform(name = 'host', constraint_values = [':value'])\n\
         early(name = 'early', value = ':value')\n",
    );
    for package in 0..4 {
        let mut build = String::from(
            "load('//rules:p4.bzl', 'w')\npackage(default_visibility = ['//visibility:public'])\n",
        );
        for index in 0..5 {
            let mut deps = Vec::new();
            if index > 0 {
                deps.push(format!("':x{}'", index - 1));
            }
            if index == 3 {
                deps.push("':g'".to_owned());
            }
            if package > 0 && index == 2 {
                deps.push(format!("'//s{}:x4'", package - 1));
            }
            let work = 200 + ((package * 5 + index * 3) % 7) * 4000;
            build.push_str(&format!(
                "w(name = 'x{index}', out = 'x{index}.txt', deps = [{}], work = {work})\n",
                deps.join(", ")
            ));
        }
        build.push_str(
            "genrule(name = 'g', srcs = [':x1'], outs = ['g.out'], cmd = 'cat $(SRCS) > $@')\n",
        );
        ws.file(&format!("s{package}/BUILD"), &build);
        ws.file(
            &format!("s{package}/sub/BUILD"),
            &format!(
                "load('//rules:p4.bzl', 'w')\npackage(default_visibility = ['//visibility:public'])\n\
                 w(name = 'y', out = 'y.txt', deps = ['//s{package}:x0'], work = 3000)\n"
            ),
        );
    }
    ws.file(
        "shared/BUILD",
        "load('//rules:p4.bzl', 'w')\n\
         w(name = 'a', out = 'same.txt', content = 'same', deps = ['//s1:x3'], work = 9000)\n\
         w(name = 'b', out = 'same.txt', content = 'same', deps = ['//s0:x1'])\n\
         w(name = 'c', out = 'c.txt', deps = [':a', ':b', '//s3:x4'])\n",
    );
    ws.file(
        "asp/BUILD",
        "load('//rules:p4.bzl', 'w', 'with_aspect')\n\
         with_aspect(name = 'top', deps = ['//s2:x4', '//s0/sub:y'])\n\
         w(name = 'after', out = 'after.txt', deps = [':top', '//s3:x2'], work = 5000)\n",
    );
}

/// P4: targets whose computes finish ahead of their turn commit there when
/// their outputs are settled (no uncommitted earlier entry of a related
/// package or aspect barrier); the action graph, the shared action's owner,
/// genrules and aspect applications beside them, prints and the dump are
/// the sequential ones at every thread count and with worker jitter.
#[test]
fn commits_ahead_of_the_turn_keep_the_sequential_graph() {
    let ws = Workspace::new();
    commit_ahead_workspace(&ws);
    let reference = assert_like_sequential(
        &ws,
        &[("BAZEL_RS_DUMP_ACTIONS", "1")],
        &["build", "--nobuild", P4_HOST, "//..."],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
    let timed = ws.run_env(
        &[("BAZEL_RS_TIMING", "1"), ("BAZEL_RS_SNAPSHOT", "off")],
        &[
            "build",
            "--loading_phase_threads=8",
            "--nobuild",
            P4_HOST,
            "//...",
        ],
    );
    let timed = stderr(&timed);
    let committed: u64 = timed
        .lines()
        .filter(|line| line.starts_with("PERF-PARALLEL "))
        .flat_map(str::split_whitespace)
        .find_map(|field| field.strip_prefix("committed_ahead="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no committed_ahead: {timed}"));
    assert!(committed > 0, "{timed}");
}

/// P4: an action conflict between two targets of one package — the second
/// commits after the first (ahead of its turn or at it) — is reported at
/// the second's turn with the sequential message and console, with and
/// without --keep_going.
#[test]
fn conflicts_committed_ahead_are_reported_at_their_turn() {
    let ws = Workspace::new();
    commit_ahead_workspace(&ws);
    ws.file(
        "conflict/BUILD",
        "load('//rules:p4.bzl', 'w')\n\
         w(name = 'a', out = 'clash.txt', content = 'A', deps = ['//s0:x2'], work = 9000)\n\
         w(name = 'b', out = 'clash.txt', content = 'B', deps = ['//s1:x1'])\n\
         w(name = 'later', out = 'later.txt', deps = [':b', '//s3:x4'])\n",
    );
    let reference = assert_like_sequential(&ws, &[], &["build", "--nobuild", P4_HOST, "//..."]);
    assert!(!reference.status.success());
    assert!(
        stderr(&reference).contains("conflicting actions"),
        "{}",
        stderr(&reference)
    );
    let keep_going = assert_like_sequential(
        &ws,
        &[],
        &["build", "--nobuild", "--keep_going", P4_HOST, "//..."],
    );
    assert!(!keep_going.status.success());
}

/// A rule of the allowlisted package `test` declaring a shareable
/// artifact named from another package's path, as a C++ link's
/// `_solib_<cpu>/` symlink is (BuiltinRestriction's `test` entry).
const SHARE_DEFS: &str = r#"
def _link_impl(ctx):
    print("link %s" % ctx.label)
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    so = ctx.actions.declare_shareable_artifact("_solib_k8/libb_Slibfoo.so")
    ctx.actions.write(so, ctx.attr.content)
    out = ctx.actions.declare_file(ctx.label.name + ".bin")
    ctx.actions.run_shell(
        inputs = [so] + [f for dep in ctx.attr.deps for f in dep[DefaultInfo].files.to_list()],
        outputs = [out],
        command = "cat %s > %s; echo %d >> %s" % (so.path, out.path, total % 5, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

link = rule(
    implementation = _link_impl,
    attrs = {
        "content": attr.string(default = "so"),
        "deps": attr.label_list(),
        "work": attr.int(default = 100),
    },
)
"#;

/// `//sa:x` (slow, earlier in plan order) and `//sb:y` (fast, later) both
/// declare the same shareable `_solib_k8/...` output, outside both
/// packages; `content` equal: one shared action, else a conflict.
fn shareable_workspace(ws: &Workspace, other_content: &str) {
    commit_ahead_workspace(ws);
    ws.file("test/BUILD", "");
    ws.file("test/share.bzl", SHARE_DEFS);
    ws.file(
        "sa/BUILD",
        "load('//test:share.bzl', 'link')\n\
         link(name = 'x', deps = ['//s3:x4'], work = 40000)\n",
    );
    ws.file(
        "sb/BUILD",
        &format!(
            "load('//test:share.bzl', 'link')\n\
             link(name = 'y', content = '{other_content}')\n"
        ),
    );
}

/// P4 review item 1: an output outside the target's package (a shareable
/// artifact such as a solib symlink) is not settled by the package rule:
/// such a target commits at its turn, so the first owner in plan order
/// owns the shared action, and a conflict names the same owners, at every
/// thread count and with jitter.
#[test]
fn shareable_outputs_outside_the_package_commit_at_their_turn() {
    for other_content in ["so", "other"] {
        let ws = Workspace::new();
        shareable_workspace(&ws, other_content);
        let reference = assert_like_sequential(
            &ws,
            &[("BAZEL_RS_DUMP_ACTIONS", "1")],
            &["build", "--nobuild", P4_HOST, "//..."],
        );
        let text = stderr(&reference);
        if other_content == "so" {
            assert!(reference.status.success(), "{text}");
            // One action produces it: the first owner's in plan order.
            assert_eq!(
                text.lines()
                    .filter(|line| line.starts_with("  output ")
                        && line.ends_with("_solib_k8/libb_Slibfoo.so"))
                    .count(),
                1,
                "{text}"
            );
            assert!(text.contains("ACTION //sa:x#0"), "{text}");
        } else {
            assert!(!reference.status.success(), "{text}");
            assert!(text.contains("conflicting actions"), "{text}");
        }
    }
}

/// P4 review item 2 (a guard): a spawn's remote platform reads
/// Engine::platforms, filled on first need; a target whose spawns' execution
/// platform is not resolved yet commits at its turn, and one whose prepare
/// saw its target platform deferred commits at its turn too. With a
/// non-default host platform carrying exec properties and a rule resolving
/// the target platform at its commit, the remote platforms in the dump
/// (`BAZEL_RS_DUMP_ACTIONS=remote`) are the sequential ones at every thread
/// count. (Here the explicit host platform is resolved before phase 2, so
/// the run also pins that no spawn changes platform.)
#[test]
fn remote_platforms_resolved_late_commit_at_their_turn() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "plat/BUILD",
        "package(default_visibility = ['//visibility:public'])\n\
         constraint_setting(name = 'os')\n\
         constraint_value(name = 'linux', constraint_setting = ':os')\n\
         platform(name = 'p', constraint_values = [':linux'], exec_properties = {'pool': 'big'})\n",
    );
    ws.file(
        "rules/probe.bzl",
        "def _probe_impl(ctx):\n\
         \x20   linux = ctx.target_platform_has_constraint(ctx.attr._linux[platform_common.ConstraintValueInfo])\n\
         \x20   out = ctx.actions.declare_file(ctx.label.name + '.txt')\n\
         \x20   ctx.actions.run_shell(outputs = [out], command = 'echo %s > %s' % (linux, out.path))\n\
         \x20   return [DefaultInfo(files = depset([out]))]\n\
         probe = rule(implementation = _probe_impl, attrs = {'deps': attr.label_list(), '_linux': attr.label(default = '//plat:linux')})\n",
    );
    ws.file(
        "q/BUILD",
        "load('//rules:probe.bzl', 'probe')\nprobe(name = 'probe', deps = ['//p5:t7'])\n",
    );
    let mut z = String::from("load('//rules:defs.bzl', 'lib')\n");
    for index in 0..8 {
        z.push_str(&format!("lib(name = 'z{index}', work = 200)\n"));
    }
    ws.file("z/BUILD", &z);
    let reference = assert_like_sequential(
        &ws,
        &[("BAZEL_RS_DUMP_ACTIONS", "remote")],
        &["build", "--nobuild", "--host_platform=//plat:p", "//..."],
    );
    let text = stderr(&reference);
    assert!(reference.status.success(), "{text}");
    assert!(text.contains("  remote-"), "{text}");
}

/// P4 review item 3: BAZEL_RS_VERIFY_AHEAD repeats the prepare of every
/// entry committed ahead at its turn (and compares it), besides right
/// before its commit; no entry committed ahead had its target platform
/// resolved before its turn.
#[test]
fn verify_ahead_checks_commits_ahead_at_their_turn() {
    let ws = Workspace::new();
    commit_ahead_workspace(&ws);
    let timed = ws.run_env(
        &[
            ("BAZEL_RS_TIMING", "1"),
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_VERIFY_AHEAD", "1"),
        ],
        &[
            "build",
            "--loading_phase_threads=8",
            "--nobuild",
            P4_HOST,
            "//...",
        ],
    );
    assert!(timed.status.success(), "{}", stderr(&timed));
    let timed = stderr(&timed);
    let field = |name: &str| -> u64 {
        timed
            .lines()
            .filter(|line| line.starts_with("PERF-PARALLEL "))
            .flat_map(str::split_whitespace)
            .find_map(|field| field.strip_prefix(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name}: {timed}"))
    };
    let committed = field("committed_ahead=");
    assert_eq!(field("verified_turn="), committed, "{timed}");
    assert_eq!(field("platform_late="), 0, "{timed}");
}

/// P4 review item 4: a commit ahead of its turn that needed a fetch, an
/// analysis or a package (BAZEL_RS_TEST_ABORT_AHEAD forces it) is an
/// internal error at its turn naming the workaround, which builds.
#[test]
fn an_aborted_commit_ahead_names_the_workaround() {
    let ws = Workspace::new();
    commit_ahead_workspace(&ws);
    let failed = ws.run_env(
        &[
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_TEST_ABORT_AHEAD", "1"),
            ("BAZEL_RS_TIMING", "1"),
        ],
        &[
            "build",
            "--loading_phase_threads=8",
            "--nobuild",
            P4_HOST,
            "//...",
        ],
    );
    let text = stderr(&failed);
    if text.contains("committed_ahead=0 ") {
        return; // Nothing finished ahead of its turn on this run.
    }
    assert!(!failed.status.success(), "{text}");
    assert!(
        text.contains(
            "ahead of its turn needed a fetch, an analysis or a package (BAZEL_RS_NO_COMMIT_AHEAD=1"
        ),
        "{text}"
    );
    let workaround = ws.run_env(
        &[
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_TEST_ABORT_AHEAD", "1"),
            ("BAZEL_RS_NO_COMMIT_AHEAD", "1"),
        ],
        &[
            "build",
            "--loading_phase_threads=8",
            "--nobuild",
            P4_HOST,
            "//...",
        ],
    );
    assert!(workaround.status.success(), "{}", stderr(&workaround));
}

/// P4 review item 5: with the segments of entries committed ahead bounded
/// to one node (BAZEL_RS_COMMITTED_LIMIT), commits ahead wait for turns to
/// consume them; the result is the sequential one.
#[test]
fn committed_segments_are_bounded() {
    let ws = Workspace::new();
    commit_ahead_workspace(&ws);
    let reference = assert_like_sequential(
        &ws,
        &[
            ("BAZEL_RS_DUMP_ACTIONS", "1"),
            ("BAZEL_RS_COMMITTED_LIMIT", "1"),
        ],
        &["build", "--nobuild", P4_HOST, "//..."],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
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

/// Plan B rules: a stamped rule (its commit needs the workspace status, so
/// it commits at its turn: its outputs are known, its commit is not ahead).
const KNOWN_DEFS: &str = r#"
def _s_impl(ctx):
    print("s %s" % ctx.label)
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    out = ctx.actions.declare_file(ctx.attr.out)
    inputs = [ctx.version_file] if ctx.attr.stamp else []
    ctx.actions.run_shell(
        inputs = inputs,
        outputs = [out],
        command = "echo %d > %s" % (total % 7, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

s = rule(
    implementation = _s_impl,
    attrs = {
        "out": attr.string(),
        "stamp": attr.bool(),
        "work": attr.int(default = 100),
        "deps": attr.label_list(),
    },
)
"#;

/// Plan B: entries of one package commit ahead of an earlier uncommitted
/// sibling whose outputs are known and do not meet theirs; one whose
/// outputs meet them (an exact path: a shared action or a conflict; a path
/// below another's output: a prefix conflict) commits after it, and the
/// conflict is the sequential one — at every thread count, with jitter and
/// BAZEL_RS_VERIFY_AHEAD (each commit's registered outputs are checked
/// against its compute's).
#[test]
fn known_outputs_decide_commits_ahead_within_a_package() {
    for (b_out, conflicts) in [("e/f", false), ("d/f", true), ("d", true)] {
        let ws = Workspace::new();
        ws.file("rules/BUILD", "");
        ws.file("rules/known.bzl", KNOWN_DEFS);
        ws.file("rules/p4.bzl", P4_DEFS);
        // Resolves the target platform early (P4_HOST): prepares see it.
        ws.file(
            "_early/BUILD",
            "load('//rules:p4.bzl', 'early')\n\
             constraint_setting(name = 'setting')\n\
             constraint_value(name = 'value', constraint_setting = ':setting')\n\
             platform(name = 'host', constraint_values = [':value'])\n\
             early(name = 'early', value = ':value')\n",
        );
        // Earlier in plan order: a slow implementation computed on a
        // worker holds the turn while `//q` finishes.
        ws.file(
            "a_slow/BUILD",
            "load('//rules:known.bzl', 's')\n\
             s(name = 'x0', out = 'x0.txt')\n\
             s(name = 'z', out = 'z.txt', work = 400000)\n",
        );
        ws.file(
            "q/BUILD",
            &format!(
                "load('//rules:known.bzl', 's')\n\
                 s(name = 'a', out = 'd', stamp = True, work = 2000)\n\
                 s(name = 'b', out = '{b_out}', work = 3000)\n\
                 s(name = 'c', out = 'c.txt', work = 1000)\n\
                 s(name = 'top', out = 'top.txt', deps = [':b', ':c'])\n"
            ),
        );
        let reference = assert_like_sequential(
            &ws,
            &[("BAZEL_RS_DUMP_ACTIONS", "1")],
            &["build", "--nobuild", P4_HOST, "//..."],
        );
        let text = stderr(&reference);
        assert_eq!(reference.status.success(), !conflicts, "{b_out}: {text}");
        if conflicts {
            assert!(
                text.contains("conflicting actions") || text.contains("is a prefix of the other"),
                "{b_out}: {text}"
            );
        }
        // While `//a_slow:z` holds the turn, `//q:b` and `//q:c` commit
        // ahead of the stamped `//q:a` (known outputs, committed at its
        // turn), unless b's outputs meet a's.
        let timed = ws.run_env(
            &[("BAZEL_RS_TIMING", "1"), ("BAZEL_RS_SNAPSHOT", "off")],
            &[
                "build",
                "--loading_phase_threads=8",
                "--nobuild",
                P4_HOST,
                "//...",
            ],
        );
        let timed = stderr(&timed);
        let field = |name: &str| -> f64 {
            timed
                .lines()
                .filter(|line| line.starts_with("PERF-PARALLEL "))
                .flat_map(str::split_whitespace)
                .find_map(|field| field.strip_prefix(name))
                .and_then(|value| value.trim_end_matches('s').parse().ok())
                .unwrap_or_else(|| panic!("no {name}: {timed}"))
        };
        // Only meaningful when z held the turn long enough (a loaded host).
        if field("wait=") > 1.0 {
            if conflicts {
                assert!(field("outputs=") > 0.0, "{b_out}: {timed}");
            } else {
                assert!(field("committed_ahead=") >= 2.0, "{b_out}: {timed}");
            }
        }
    }
}

/// Plan B: the declared outputs' check (no subpackage or source symlink on
/// an output's way) runs on the analysis worker after the implementation;
/// a failing one is the sequential error at the target's turn.
#[test]
fn declared_outputs_checked_on_workers_fail_at_their_turn() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file("rules/known.bzl", KNOWN_DEFS);
    ws.file("q/sub/BUILD", "");
    ws.file(
        "q/BUILD",
        "load('//rules:known.bzl', 's')\n\
         s(name = 'ok', out = 'nested/ok.txt', deps = ['//p2:t7'], work = 3000)\n\
         s(name = 'cross', out = 'sub/f.txt', deps = ['//p3:t5'], work = 2000)\n\
         s(name = 'above', out = 'above.txt', deps = [':ok', ':cross'])\n",
    );
    let reference = assert_like_sequential(&ws, &[], &["build", "--nobuild", "//..."]);
    let text = stderr(&reference);
    assert!(!reference.status.success(), "{text}");
    assert!(text.contains("crosses a package boundary"), "{text}");
}

/// A rule of the allowlisted package `test` declaring a shareable artifact
/// at a chosen path below the output root (any package's directory).
const SHARE_AT_DEFS: &str = r#"
def _share_at_impl(ctx):
    print("share_at %s" % ctx.label)
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    out = ctx.actions.declare_shareable_artifact(ctx.attr.path)
    ctx.actions.write(out, "%d" % (total % 3))
    return [DefaultInfo(files = depset([out]))]

share_at = rule(
    implementation = _share_at_impl,
    attrs = {"path": attr.string(), "work": attr.int(default = 100)},
)
"#;

/// Plan B review item 1: an earlier entry whose outputs are still unknown
/// declares a shareable artifact in another package (`//a:a` writes
/// `b/x`), where a later entry with known outputs (`//b:b`, output `x`)
/// may commit ahead of its turn and register the path first. The commit at
/// the earlier turn then stops the build with the internal error naming
/// BAZEL_RS_NO_COMMIT_AHEAD=1 (the backstop) — never the owners in reverse
/// or the shared action given to the later target — and the workaround is
/// the sequential build.
#[test]
fn a_shareable_output_met_ahead_of_its_turn_stops_with_the_workaround() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/known.bzl", KNOWN_DEFS);
    ws.file("rules/p4.bzl", P4_DEFS);
    ws.file("test/BUILD", "");
    ws.file("test/share_at.bzl", SHARE_AT_DEFS);
    ws.file(
        "_early/BUILD",
        "load('//rules:p4.bzl', 'early')\n\
         constraint_setting(name = 'setting')\n\
         constraint_value(name = 'value', constraint_setting = ':setting')\n\
         platform(name = 'host', constraint_values = [':value'])\n\
         early(name = 'early', value = ':value')\n",
    );
    ws.file(
        "a/BUILD",
        "load('//test:share_at.bzl', 'share_at')\n\
         share_at(name = 'a', path = 'b/x', work = 400000)\n",
    );
    ws.file(
        "b/BUILD",
        "load('//rules:known.bzl', 's')\n\
         s(name = 'b', out = 'x', work = 100)\n",
    );
    let args = ["build", "--nobuild", P4_HOST, "//..."];
    let off = [("BAZEL_RS_SNAPSHOT", "off")];
    let mut sequential_args = vec!["build", "--loading_phase_threads=1"];
    sequential_args.extend(&args[1..]);
    let sequential = ws.run_env(&off, &sequential_args);
    assert!(!sequential.status.success(), "{}", stderr(&sequential));
    let mut parallel_args = vec!["build", "--loading_phase_threads=8"];
    parallel_args.extend(&args[1..]);
    let parallel = ws.run_env(&off, &parallel_args);
    let text = stderr(&parallel);
    if text.contains("internal error: output") {
        assert!(
            text.contains("ahead of its turn") && text.contains("BAZEL_RS_NO_COMMIT_AHEAD=1"),
            "{text}"
        );
    } else {
        assert_eq!(text, stderr(&sequential));
    }
    let workaround = ws.run_env(
        &[
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_NO_COMMIT_AHEAD", "1"),
        ],
        &parallel_args,
    );
    assert_eq!(workaround.status.code(), sequential.status.code());
    assert_eq!(stderr(&workaround), stderr(&sequential));
}

/// Plan B review item 2: a stamped target registers the workspace status
/// files inside its commit (first need); BAZEL_RS_VERIFY_AHEAD's check of
/// the commit's outputs leaves them out (they are no target's), so a
/// stamped executing build verifies.
#[test]
fn verify_ahead_accepts_a_stamped_executing_build() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file("rules/known.bzl", KNOWN_DEFS);
    ws.file(
        "q/BUILD",
        "load('//rules:known.bzl', 's')\n\
         s(name = 'a', out = 'a.txt', stamp = True, deps = ['//p1:t2'])\n\
         s(name = 'b', out = 'b.txt', stamp = True)\n",
    );
    let warm = ws.run_env(&[], &["build", "--loading_phase_threads=1", "//q:all"]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let reference = assert_like_sequential(&ws, &[], &["build", "//q:all"]);
    assert!(reference.status.success(), "{}", stderr(&reference));
}

/// Plan B review item 3: native rules classified as registering no output
/// (known outputs, empty) are checked by BAZEL_RS_VERIFY_AHEAD too: their
/// commits register none (`verified_outputs=` counts them).
#[test]
fn verify_ahead_checks_native_rules_register_no_outputs() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "n/BUILD",
        "filegroup(name = 'fg', srcs = ['//p0:t3', 'f.txt'])\n\
         alias(name = 'al', actual = ':fg')\n\
         filegroup(name = 'fg2', srcs = [':al'])\n",
    );
    ws.file("n/f.txt", "f\n");
    let reference = assert_like_sequential(&ws, &[], &["build", "--nobuild", "//..."]);
    assert!(reference.status.success(), "{}", stderr(&reference));
    let timed = ws.run_env(
        &[
            ("BAZEL_RS_TIMING", "1"),
            ("BAZEL_RS_SNAPSHOT", "off"),
            ("BAZEL_RS_VERIFY_AHEAD", "1"),
        ],
        &["build", "--loading_phase_threads=8", "--nobuild", "//..."],
    );
    let timed = stderr(&timed);
    let field = |name: &str| -> u64 {
        timed
            .lines()
            .filter(|line| line.starts_with("PERF-PARALLEL "))
            .flat_map(str::split_whitespace)
            .find_map(|field| field.strip_prefix(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name}: {timed}"))
    };
    // The 48 Starlark targets and the three native ones.
    assert!(field("verified_outputs=") >= 51, "{timed}");
}

/// Plan B review item 4: BAZEL_RS_NO_ARENA_BINDING=1 keeps the workers in
/// their own jemalloc arenas (to measure arena-lock contention); the build
/// is the sequential one.
#[test]
fn workers_without_the_arena_binding_build_the_same_graph() {
    let ws = Workspace::new();
    library_workspace(&ws);
    let reference = assert_like_sequential(
        &ws,
        &[
            ("BAZEL_RS_DUMP_ACTIONS", "1"),
            ("BAZEL_RS_NO_ARENA_BINDING", "1"),
        ],
        &["build", "--nobuild", "//..."],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
}

/// Plan B review item 5: the hidden top-level runfiles of a requested
/// executable are listed once per requested target; a label requested
/// twice is one target (the same graph as requesting it once).
#[test]
fn a_target_requested_twice_lists_its_runfiles_once() {
    let ws = Workspace::new();
    library_workspace(&ws);
    ws.file(
        "z/BUILD",
        "load('//rules:defs.bzl', 'bin')\n\
         bin(name = 'tool', deps = ['//p3:t5', '//p1:t2'])\n",
    );
    let env = [("BAZEL_RS_DUMP_ACTIONS", "1")];
    let once = assert_like_sequential(&ws, &env, &["build", "--nobuild", "//z:tool"]);
    let twice = assert_like_sequential(&ws, &env, &["build", "--nobuild", "//z:tool", "//z:tool"]);
    assert!(once.status.success(), "{}", stderr(&once));
    assert_eq!(stderr(&once), stderr(&twice));
}
