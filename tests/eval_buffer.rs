//! BAZEL_RS_ANALYSIS=eval, T3 (docs/EVALUATOR.md "T3 as built"): a frozen
//! walk computes phase 2 in dependency order on the workers and merges in
//! plan order. The output must be the walk's byte for byte (stderr with the
//! action dump, stdout, exit code) whatever finishes first: an output
//! shared by two targets of a package (and a conflicting one) where the
//! loser — later in plan order — finishes long before the winner, with a
//! reader of the loser's output; a failure in the middle of the plan while
//! the targets after it have finished (their prints and actions dropped,
//! fail fast); prints of many targets across buffered merges.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
include!("support/module_archives.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const RULES_BZL: &str = r#"
def _w_impl(ctx):
    print("w %s" % ctx.label)
    total = 0
    for i in range(ctx.attr.work):
        total += i * i
    if ctx.attr.fail:
        fail("the implementation of %s failed" % ctx.label.name)
    out = ctx.actions.declare_file(ctx.attr.out or ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.content)
    files = depset([out], transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps])
    summary = ctx.actions.declare_file(ctx.label.name + ".sum")
    ctx.actions.run_shell(
        inputs = files,
        outputs = [summary],
        command = "cat $@ > /dev/null; echo %d > %s" % (total % 7, summary.path),
    )
    return [DefaultInfo(files = depset([summary], transitive = [files]))]

def _writer_impl(ctx):
    out = ctx.outputs.out if ctx.attr.out else ctx.actions.declare_file(ctx.attr.path)
    ctx.actions.write(out, "shared write\n")
    return [DefaultInfo(files = depset([out]))]

writer = rule(
    implementation = _writer_impl,
    attrs = {"out": attr.output(), "path": attr.string()},
)

def _owners_impl(ctx):
    print("owners %s" % [(file.path, file.owner) for file in ctx.files.srcs])
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, str([(file.path, file.owner) for file in ctx.files.srcs]))
    return [DefaultInfo(files = depset([report]))]

owners = rule(
    implementation = _owners_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)

w = rule(
    implementation = _w_impl,
    attrs = {
        "out": attr.string(),
        "content": attr.string(default = "x"),
        "deps": attr.label_list(),
        "work": attr.int(default = 10),
        "fail": attr.bool(),
    },
)
"#;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, String)]) -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        let mut all = vec![
            ("MODULE.bazel", String::new()),
            ("rules/BUILD.bazel", String::new()),
            ("rules/defs.bzl", RULES_BZL.to_owned()),
        ];
        all.extend(files.iter().cloned());
        for (path, text) in all {
            let path = workspace.0.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.0.path().join(directory)).unwrap();
        }
        workspace
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn build(&self, mode: &str, threads: usize, env: &[(&str, &str)], args: &[&str]) -> Output {
        let _ = fs::remove_dir_all(self.path().join("cache"));
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_SNAPSHOT", "off")
            .env("BAZEL_RS_ANALYSIS", mode)
            .env("BAZEL_RS_DUMP_ACTIONS", "1")
            .args(with_builtin_registry(
                &[
                    "--ignore_all_rc_files",
                    "--cache-dir=cache",
                    "build",
                    "--experimental-builtin-modules=false",
                    "--lockfile_mode=off",
                    "--memory-limit=2048",
                    "--nobuild",
                ]
                .into_iter()
                .map(str::to_owned)
                .chain([format!("--loading_phase_threads={threads}")])
                .chain(args.iter().map(|arg| (*arg).to_owned()))
                .collect::<Vec<_>>(),
            ))
            .envs(env.iter().copied())
            .seeded_archives()
            .output()
            .unwrap()
    }
}

/// stderr without the timing and memory lines.
fn stable(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| {
            !line.starts_with("INFO: Elapsed time")
                && !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("PERF")
                && !line.starts_with("bazel-rs: peak RSS")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const FROZEN: (&str, &str) = ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all");
const ASPECT_JOBS: (&str, &str) = ("BAZEL_RS_T3_ASPECT_JOBS", "1");
/// The aspect prefetch at the rule's turn instead (parallel::aspects).
const NO_ASPECT_JOBS: (&str, &str) = ("BAZEL_RS_T3_ASPECT_JOBS", "0");
const PARITY_VERIFY: (&str, &str) = ("BAZEL_RS_PARITY_VERIFY", "1");
const REVERSE: (&str, &str) = ("BAZEL_RS_PARITY_REVERSE", "1");
/// Task D: the frozen walk's jobs start during phase 1, from the first
/// flushed entry on.
const EARLY: (&str, &str) = ("BAZEL_RS_EVAL_EARLY_CT2", "1");
const EARLY_MIN: (&str, &str) = ("BAZEL_RS_EVAL_EARLY_CT2_MIN", "1");
const EARLY_GROWTH: (&str, &str) = ("BAZEL_RS_EVAL_EARLY_CT2_GROWTH", "0");

/// The walk's output, then the buffered evaluation's at several thread
/// counts, with jitter, restarts and a tiny window, compared byte for byte;
/// one timed run checks the merge buffer walked.
fn compare(ws: &Workspace, args: &[&str], check: impl Fn(bool, &str)) {
    let walk = ws.build("walk", 1, &[], args);
    let text = stable(&walk);
    check(walk.status.success(), &text);
    let runs: [(usize, &[(&str, &str)]); 26] = [
        (2, &[FROZEN]),
        (8, &[FROZEN]),
        (32, &[FROZEN]),
        (32, &[FROZEN, ("BAZEL_RS_ANALYSIS_JITTER", "5")]),
        (8, &[FROZEN, ("BAZEL_RS_EVAL_JITTER", "300")]),
        (8, &[FROZEN, ("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[FROZEN, ("BAZEL_RS_T3_WINDOW", "3")]),
        // The cursor meets entries still being classified.
        (32, &[FROZEN, ("BAZEL_RS_T3_CLASSIFY_JITTER", "20000")]),
        (8, &[FROZEN, ("BAZEL_RS_VERIFY_DELTA", "1")]),
        // The sweep (task A): rewrites checked against the T3 cursor's
        // rebuilds; the latest ready job first; the rebuilds themselves.
        (8, &[FROZEN, PARITY_VERIFY]),
        (1, &[FROZEN, REVERSE]),
        (2, &[FROZEN, REVERSE, PARITY_VERIFY]),
        (8, &[FROZEN, REVERSE, PARITY_VERIFY]),
        (32, &[FROZEN, REVERSE, ("BAZEL_RS_ANALYSIS_JITTER", "5")]),
        (8, &[FROZEN, REVERSE, ("BAZEL_RS_EVAL_JITTER", "300")]),
        (8, &[FROZEN, REVERSE, ("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[FROZEN, ("BAZEL_RS_EVAL_SWEEP", "0")]),
        // Task D: early boards (one per flushed segment), the walk taking
        // their jobs; checked against the T3 cursor's rebuilds and the
        // coordinator's deltas.
        (8, &[FROZEN, EARLY, EARLY_MIN, EARLY_GROWTH]),
        (
            32,
            &[
                FROZEN,
                EARLY,
                EARLY_MIN,
                EARLY_GROWTH,
                REVERSE,
                PARITY_VERIFY,
            ],
        ),
        (
            8,
            &[
                FROZEN,
                EARLY,
                EARLY_MIN,
                ("BAZEL_RS_EVAL_JITTER", "300"),
                ("BAZEL_RS_EVAL_RESTARTS", "5"),
            ],
        ),
        (
            2,
            &[FROZEN, EARLY, EARLY_MIN, ("BAZEL_RS_VERIFY_DELTA", "1")],
        ),
        // The T2 pipeline (no buffer: no sweep, losers publish nothing).
        (
            8,
            &[FROZEN, ("BAZEL_RS_EVAL_NO_BUFFER", "1"), PARITY_VERIFY],
        ),
        // Task B: attribute-aspect rules as worker jobs; checked against
        // the coordinator's inputs; the latest ready job first.
        (8, &[FROZEN, NO_ASPECT_JOBS]),
        (
            2,
            &[
                FROZEN,
                ASPECT_JOBS,
                ("BAZEL_RS_T3_ASPECT_DELTAS", "1"),
                ("BAZEL_RS_VERIFY_DELTA", "1"),
            ],
        ),
        (
            8,
            &[
                FROZEN,
                ("BAZEL_RS_T3_ASPECT_JOBS", "verify"),
                ("BAZEL_RS_VERIFY_DELTA", "1"),
            ],
        ),
        (
            32,
            &[
                FROZEN,
                ASPECT_JOBS,
                REVERSE,
                PARITY_VERIFY,
                ("BAZEL_RS_ANALYSIS_JITTER", "5"),
            ],
        ),
    ];
    for (threads, env) in runs {
        let eval = ws.build("eval", threads, env, args);
        let what = format!("{args:?} at {threads} threads, {env:?}");
        assert_eq!(
            eval.status.code(),
            walk.status.code(),
            "{what}: {}",
            stable(&eval)
        );
        let got = stable(&eval);
        if got != text {
            // The first lines that differ (the whole texts are long).
            let first = got
                .lines()
                .zip(text.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let window = |all: &str| {
                all.lines()
                    .skip(first.saturating_sub(2))
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "{what}: differs at line {first}\neval:\n{}\nwalk:\n{}",
                window(&got),
                window(&text)
            );
        }
        assert_eq!(eval.stdout, walk.stdout, "{what}");
    }
    let timed = ws.build("eval", 8, &[FROZEN, ("BAZEL_RS_TIMING", "1")], args);
    let succeeded = timed.status.success();
    let timed = String::from_utf8_lossy(&timed.stderr);
    let line = timed
        .lines()
        .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
        .unwrap_or_else(|| panic!("the merge buffer walked: {timed}"));
    let count = |name: &str| -> u64 {
        line.split_whitespace()
            .find_map(|field| field.strip_prefix(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name} in {line}"))
    };
    // Every entry of the buffered walks is classified at most once (the
    // cursor may take it first), and either dispatched to a job or taken
    // at its turn by the cursor.
    let entries: u64 = timed
        .lines()
        .filter_map(|line| line.strip_prefix("PERF-PARALLEL buffer cursor entries="))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .sum();
    assert!(count("walks=") > 0 && entries > 0, "{timed}");
    assert!(
        count("starlark=") + count("native=") + count("turn=") <= entries,
        "{line}"
    );
    // (A failed walk stops at its error: the rest is never claimed.)
    let handled = count("dispatched=") + count("at_turn=");
    if succeeded {
        assert_eq!(handled, entries, "{line}");
    } else {
        assert!(handled <= entries, "{line}");
    }
    // Task D: early boards ran during phase 1 (a workspace with a flushed
    // entry before its last requested target) and the walk took their jobs.
    let early = ws.build(
        "eval",
        8,
        &[
            FROZEN,
            EARLY,
            EARLY_MIN,
            EARLY_GROWTH,
            ("BAZEL_RS_TIMING", "1"),
        ],
        args,
    );
    let early = String::from_utf8_lossy(&early.stderr);
    if let Some(line) = early
        .lines()
        .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
    {
        let field = |name: &str| -> u64 {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(name))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {line}"))
        };
        assert!(field("early_taken=") <= field("early_entries="), "{line}");
        eprintln!(
            "early boards {} entries {} taken {}",
            field("early_boards="),
            field("early_entries="),
            field("early_taken=")
        );
    }
}

/// The winner of a shared output (first in plan order) computes for long,
/// the loser and its reader finish first; with different contents the two
/// conflict. The action dump names the winner's action for the shared
/// output, and the reader's inputs name it too.
#[test]
fn shared_outputs_and_conflicts_where_the_loser_finishes_first() {
    for content in ["same", "other"] {
        let ws = Workspace::new(&[(
            "sh/BUILD.bazel",
            format!(
                r#"load("//rules:defs.bzl", "w")
w(name = "a", out = "shared.txt", content = "same", work = 400000)
w(name = "b", out = "shared.txt", content = "{content}")
w(name = "r", deps = [":b"])
w(name = "z", deps = [":r", ":a"])
"#
            ),
        )]);
        compare(&ws, &["//sh:z", "//sh:b", "//sh:a"], |success, text| {
            if content == "same" {
                assert!(success, "{text}");
                assert_eq!(
                    text.lines()
                        .filter(
                            |line| line.starts_with("  output ") && line.ends_with("sh/shared.txt")
                        )
                        .count(),
                    1,
                    "one action produces the shared output: {text}"
                );
            } else {
                assert!(!success, "{text}");
                assert!(text.contains("conflicting actions"), "{text}");
            }
        });
    }
}

/// The loser's shared action is never lowered by the walk, so a failure
/// only its lowering finds (the `BAZEL_RS_LOWERING_FAIL` hook) never fails
/// the build: the sweep drops that lowering with the action.
#[test]
fn a_shared_actions_dropped_lowering_never_fails_the_build() {
    let ws = Workspace::new(&[(
        "sh/BUILD.bazel",
        r#"load("//rules:defs.bzl", "w")
w(name = "a", out = "shared.txt", content = "same", work = 400000)
w(name = "b", out = "shared.txt", content = "same")
w(name = "r", deps = [":b"])
w(name = "z", deps = [":r", ":a"])
"#
        .to_owned(),
    )]);
    let fail = ("BAZEL_RS_LOWERING_FAIL", "//sh:b#0");
    let args = ["//sh:z", "//sh:b", "//sh:a"];
    let walk = ws.build("walk", 1, &[fail], &args);
    assert!(walk.status.success(), "{}", stable(&walk));
    for (threads, env) in [
        (8, &[FROZEN, fail][..]),
        (2, &[FROZEN, fail, PARITY_VERIFY][..]),
        (8, &[FROZEN, fail, REVERSE][..]),
    ] {
        let eval = ws.build("eval", threads, env, &args);
        assert_eq!(stable(&eval), stable(&walk), "{env:?}");
        assert_eq!(eval.status.code(), walk.status.code(), "{env:?}");
    }
}

/// Task B: package groups are worker natives (their resolved node read
/// from phase 1), a reader of one ready on its published results.
#[test]
fn package_groups_publish_from_the_workers() {
    let ws = Workspace::new(&[
        (
            "pg/defs.bzl",
            r#"def _impl(ctx):
    info = ctx.attr.group[PackageSpecificationInfo]
    print("contains %s %s" % (info.contains(Label("//pg/sub:x")), info.contains(Label("//elsewhere:x"))))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

reader = rule(implementation = _impl, attrs = {"group": attr.label(providers = [PackageSpecificationInfo])})
"#
            .to_owned(),
        ),
        (
            "pg/BUILD.bazel",
            r#"load(":defs.bzl", "reader")
package_group(name = "g", packages = ["//pg/..."], includes = [":h"])
package_group(name = "h", packages = ["//other"])
reader(name = "r", group = ":g")
"#
            .to_owned(),
        ),
    ]);
    compare(&ws, &["//pg:r", "//pg:h"], |success, text| {
        assert!(success, "{text}");
        assert!(text.contains("contains True False"), "{text}");
    });
}

/// A symlinked source file's lookup is never memoized (its path may
/// change), so no frozen view knows it: a job of the merge buffer resolves
/// it itself (applied at its merge) instead of leaving the target to its
/// turn, whose dependents would wait for the cursor.
#[test]
fn symlinked_sources_resolve_on_the_workers() {
    let ws = Workspace::new(&[
        ("s/a.txt", "a\n".to_owned()),
        ("s/sub/dir/f", "f\n".to_owned()),
        (
            "s/BUILD.bazel",
            r#"load("//rules:defs.bzl", "owners")
owners(name = "o1", srcs = ["link.txt", "a.txt", "sub/dir", "missing.txt"])
owners(name = "o2", srcs = ["link.txt", ":o1"])
"#
            .to_owned(),
        ),
    ]);
    std::os::unix::fs::symlink("a.txt", ws.path().join("s/link.txt")).unwrap();
    compare(&ws, &["//s:all"], |success, text| {
        assert!(success, "{text}");
        assert!(
            text.contains("(\"s/link.txt\", Label(\"//s:link.txt\"))"),
            "{text}"
        );
    });
    let timed = ws.build("eval", 8, &[FROZEN, ("BAZEL_RS_TIMING", "1")], &["//s:all"]);
    let timed = String::from_utf8_lossy(&timed.stderr);
    let line = timed
        .lines()
        .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
        .unwrap_or_else(|| panic!("the merge buffer walked: {timed}"));
    assert!(line.contains(" aborted=0 "), "{line}");
}

/// An alias merged from its published results whose actual committed new
/// provider values at its merge (computed again there: the test hook) is
/// published again: its later readers see the actual's committed values,
/// checked against what its earlier readers read.
#[test]
fn an_alias_whose_actual_commits_new_values_is_published_again() {
    let ws = Workspace::new(&[
        (
            "t/defs.bzl",
            r#"P = provider(fields = ["v"])

def _act(ctx):
    return [P(v = ctx.label.name), DefaultInfo()]

act = rule(implementation = _act)

def _reader(ctx):
    print("reader %s %s" % (ctx.label.name, ctx.attr.dep[P].v))
    return []

reader = rule(implementation = _reader, attrs = {"dep": attr.label()})
"#
            .to_owned(),
        ),
        (
            "t/BUILD.bazel",
            r#"load("//rules:defs.bzl", "w")
load(":defs.bzl", "act", "reader")
w(name = "slow", work = 600000)
act(name = "actual")
alias(name = "al", actual = ":actual")
reader(name = "r1", dep = ":al")
reader(name = "r2", dep = ":al")
"#
            .to_owned(),
        ),
    ]);
    let args = ["//t:slow", "//t:r1", "//t:r2"];
    let walk = ws.build("walk", 1, &[], &args);
    let text = stable(&walk);
    assert!(walk.status.success(), "{text}");
    assert!(text.contains("reader r2 actual"), "{text}");
    const HOOK: (&str, &str) = ("BAZEL_RS_TEST_RECOMPUTE_AT_MERGE", "actual");
    // (Timed, each: whether the alias's job ran before the actual's merge
    // depends on the scheduling; one republish in the runs is the check.)
    const TIMED: (&str, &str) = ("BAZEL_RS_TIMING", "1");
    let runs: [&[(&str, &str)]; 4] = [
        &[FROZEN, HOOK, TIMED],
        &[FROZEN, HOOK, REVERSE, PARITY_VERIFY, TIMED],
        &[FROZEN, HOOK, TIMED],
        &[FROZEN, HOOK, TIMED, ("BAZEL_RS_ANALYSIS_JITTER", "5")],
    ];
    let mut republished = 0;
    for env in runs {
        let eval = ws.build("eval", 8, env, &args);
        assert_eq!(stable(&eval), text, "{env:?}");
        assert_eq!(eval.stdout, walk.stdout, "{env:?}");
        let stderr = String::from_utf8_lossy(&eval.stderr);
        if let Some(line) = stderr
            .lines()
            .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
        {
            republished += line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("forwarded_republished="))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("{line}"));
        }
    }
    assert!(republished > 0, "the alias was merged from stale values");
}

/// A genrule is committed at its turn; its files to build and runfiles,
/// the same as its worker published, keep the published values, so an
/// alias of it (and an alias of that alias) merges from its published
/// results without being published again.
#[test]
fn aliases_of_a_genrule_merge_fast() {
    let ws = Workspace::new(&[(
        "g/BUILD.bazel",
        r#"load("//rules:defs.bzl", "owners", "w")
w(name = "slow", work = 300000)
genrule(name = "gen", outs = ["gen.txt"], cmd = "echo x > $@")
alias(name = "al", actual = ":gen")
alias(name = "al2", actual = ":al")
owners(name = "o", srcs = [":al2", ":al"])
"#
        .to_owned(),
    )]);
    let args = ["//g:slow", "//g:o"];
    compare(&ws, &args, |success, text| {
        assert!(success, "{text}");
        assert!(text.contains("g/gen.txt"), "{text}");
    });
    let timed = ws.build("eval", 8, &[FROZEN, ("BAZEL_RS_TIMING", "1")], &args);
    let timed = String::from_utf8_lossy(&timed.stderr);
    let line = timed
        .lines()
        .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
        .unwrap_or_else(|| panic!("the merge buffer walked: {timed}"));
    // (Both aliases merged from their published results.)
    assert!(line.contains(" forwarded_republished=0"), "{line}");
    assert!(!line.contains(" fast_native=0 "), "{line}");
}

/// A tool's runfiles hold a shared output whose loser (later in plan order)
/// published first: its users, built ahead on the workers, hold a tool
/// runfiles group naming the loser's action, which the sweep rewrites. The
/// planned actions share one group per tree, as the walk's (PERF-GROUPS
/// `distinct`; a snapshot saves and verifies the plan's shared groups).
#[test]
fn a_rewritten_tool_runfiles_group_stays_shared() {
    let users = (0..6)
        .map(|index| format!("user(name = \"u{index}\", tool = \":t\")\n"))
        .collect::<String>();
    let ws = Workspace::new(&[
        (
            "g/defs.bzl",
            r##"def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(implementation = _tool, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _user(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(executable = ctx.executable.tool, outputs = [out], arguments = [out.path])
    return [DefaultInfo(files = depset([out]))]

user = rule(implementation = _user, attrs = {"tool": attr.label(executable = True, cfg = "target")})
"##
            .to_owned(),
        ),
        (
            "g/BUILD.bazel",
            format!(
                r#"load("//rules:defs.bzl", "w")
load(":defs.bzl", "tool", "user")
w(name = "a", out = "shared.txt", content = "same", work = 400000)
w(name = "b", out = "shared.txt", content = "same")
tool(name = "t", data = [":b"])
{users}"#
            ),
        ),
    ]);
    let args = ["//g:a", "//g:all"];
    compare(&ws, &args, |success, text| assert!(success, "{text}"));
    const TIMED: (&str, &str) = ("BAZEL_RS_TIMING", "1");
    let field = |stderr: &str, prefix: &str, name: &str| -> u64 {
        let line = stderr
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix}: {stderr}"));
        line.split_whitespace()
            .find_map(|field| field.strip_prefix(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name} in {line}"))
    };
    let walk = ws.build("walk", 1, &[TIMED], &args);
    let walk = String::from_utf8_lossy(&walk.stderr);
    let distinct = field(&walk, "PERF-GROUPS ", "distinct=");
    assert_eq!(distinct, 1, "{walk}");
    let runs: [(usize, &[(&str, &str)]); 4] = [
        (8, &[FROZEN, TIMED]),
        (8, &[FROZEN, TIMED, PARITY_VERIFY]),
        (32, &[FROZEN, TIMED, ("BAZEL_RS_ANALYSIS_JITTER", "5")]),
        (2, &[FROZEN, TIMED, REVERSE]),
    ];
    let mut rewritten = 0;
    for (threads, env) in runs {
        let eval = ws.build("eval", threads, env, &args);
        let eval = String::from_utf8_lossy(&eval.stderr);
        assert_eq!(
            field(&eval, "PERF-GROUPS ", "distinct="),
            distinct,
            "{threads} threads, {env:?}: {eval}"
        );
        if eval.contains("PERF-PARALLEL sweep ") {
            rewritten += field(&eval, "PERF-PARALLEL sweep ", "groups_rewritten=");
        }
    }
    assert!(rewritten > 0, "a worker's group was rewritten");
}

/// An output-file target's File names its generating rule even when
/// another target of the package declared the same path (a shared action)
/// earlier in plan order and merged before the generating rule published:
/// a reader of the label gets the generating rule's declaration.
#[test]
fn an_output_file_label_resolves_to_its_generating_rules_declaration() {
    let ws = Workspace::new(&[(
        "o/BUILD.bazel",
        r#"load("//rules:defs.bzl", "owners", "writer")
writer(name = "w_declared", path = "w.txt")
writer(name = "w_output", out = "w.txt")
owners(name = "w_owners", srcs = [":w.txt"])
"#
        .to_owned(),
    )]);
    compare(&ws, &["//o:w_declared", "//o:w_owners"], |success, text| {
        assert!(success, "{text}");
        assert!(text.contains("Label(\"//o:w_output\")"), "{text}");
    });
}

/// A target in the middle of the plan fails; the targets after it (not
/// depending on it) finish on the workers first. Fail fast: the error is
/// the walk's, and nothing of the later targets — prints, actions — shows.
#[test]
fn a_failure_in_the_middle_of_the_plan_drops_the_later_buffered_results() {
    let mut build = String::from("load(\"//rules:defs.bzl\", \"w\")\n");
    let mut roots = Vec::new();
    for index in 0..24 {
        let (work, fail) = if index == 8 {
            (300000, "True")
        } else {
            (10, "False")
        };
        build.push_str(&format!(
            "w(name = \"t{index:02}\", work = {work}, fail = {fail})\n"
        ));
        roots.push(format!("//f:t{index:02}"));
    }
    let ws = Workspace::new(&[("f/BUILD.bazel", build)]);
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    compare(&ws, &roots, |success, text| {
        assert!(!success, "{text}");
        assert!(text.contains("the implementation of t08 failed"), "{text}");
    });
}

/// Many printing targets over several packages, their implementations
/// taking varying time: the prints keep the plan's order across the
/// buffered merges.
#[test]
fn prints_keep_the_plan_order_across_buffered_merges() {
    let mut files = Vec::new();
    for (package, path) in [
        "p0/BUILD.bazel",
        "p1/BUILD.bazel",
        "p2/BUILD.bazel",
        "p3/BUILD.bazel",
    ]
    .into_iter()
    .enumerate()
    {
        let mut build = String::from(
            "load(\"//rules:defs.bzl\", \"w\")\npackage(default_visibility = [\"//visibility:public\"])\n",
        );
        for index in 0..16 {
            let work = 10 + (package * 16 + index) * 7919 % 40000;
            let deps = if index > 0 {
                format!("[\":t{}\"]", index - 1)
            } else if package > 0 {
                format!("[\"//p{}:t15\"]", package - 1)
            } else {
                "[]".to_owned()
            };
            build.push_str(&format!(
                "w(name = \"t{index}\", work = {work}, deps = {deps})\n"
            ));
            build.push_str(&format!(
                "w(name = \"s{index}\", work = {})\n",
                40000 - work.min(39990)
            ));
        }
        files.push((path, build));
    }
    let ws = Workspace::new(&files);
    compare(
        &ws,
        &["//p0:all", "//p1:all", "//p2:all", "//p3:all"],
        |success, text| {
            assert!(success, "{text}");
            assert!(text.contains("w @@//p3:t15"), "{text}");
        },
    );
}

const ASPECTS_BZL: &str = r#"
HeavyInfo = provider(fields = ["files", "count"])
LightInfo = provider(fields = ["files"])

def _heavy_impl(target, ctx):
    print("heavy %s" % target.label)
    if target.label.name == ctx.attr._boom:
        fail("the heavy aspect failed on %s" % target.label.name)
    total = 0
    for i in range(ctx.attr._work):
        total += i * i
    deps = [dep[HeavyInfo] for dep in ctx.rule.attr.deps if HeavyInfo in dep]
    out = ctx.actions.declare_file(target.label.name + ".heavy")
    ctx.actions.write(out, "%d %d" % (total % 7, len(deps)))
    count = 1
    for info in deps:
        count += info.count
    return [HeavyInfo(
        files = depset([out], transitive = [info.files for info in deps]),
        count = count,
    )]

heavy = aspect(
    implementation = _heavy_impl,
    attr_aspects = ["deps"],
    attrs = {"_work": attr.int(default = 60000), "_boom": attr.string(default = "BOOM")},
)

def _light_impl(target, ctx):
    deps = [dep[LightInfo] for dep in ctx.rule.attr.deps if LightInfo in dep]
    out = ctx.actions.declare_file(target.label.name + ".light")
    ctx.actions.write(out, "%s %d" % (target.label, target[HeavyInfo].count))
    return [LightInfo(files = depset([out], transitive = [info.files for info in deps]))]

light = aspect(implementation = _light_impl, attr_aspects = ["deps"], requires = [heavy])

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".lib")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _lint_impl(ctx):
    print("lint %s" % ctx.label)
    files = depset(transitive = [dep[LightInfo].files for dep in ctx.attr.deps] +
                                [dep[HeavyInfo].files for dep in ctx.attr.deps])
    out = ctx.actions.declare_file(ctx.label.name + ".lint")
    ctx.actions.run_shell(inputs = files, outputs = [out], command = "touch " + out.path)
    return [DefaultInfo(files = depset([out]))]

lint = rule(implementation = _lint_impl, attrs = {"deps": attr.label_list(aspects = [light])})
"#;

/// Lint targets apply attribute aspects (one requiring the other) over a
/// shared dependency graph, like rules_rust's clippy and rustfmt: each lint
/// target is a worker job whose applications are analyzed once and
/// committed at the first lint target's merge (parallel::aspect_jobs), or
/// (`BAZEL_RS_T3_ASPECT_JOBS=0`) analyzed on the workers at its turn and
/// replayed by its prepare (parallel::aspects). Prints, actions and an
/// aspect's failure come out as the walk's.
#[test]
fn attribute_aspect_applications_analyzed_ahead_replay_in_order() {
    for boom in ["BOOM", "l7"] {
        let mut build = String::from("load(\"//rules:aspects.bzl\", \"lib\", \"lint\")\n");
        for index in 0..12 {
            let deps = (0..index)
                .filter(|dep| (index + dep) % 3 == 0 || dep + 1 == index)
                .map(|dep| format!("\":l{dep}\""))
                .collect::<Vec<_>>()
                .join(", ");
            build.push_str(&format!("lib(name = \"l{index}\", deps = [{deps}])\n"));
        }
        build.push_str("lint(name = \"lint_a\", deps = [\":l5\", \":l11\"])\n");
        build.push_str("lint(name = \"lint_b\", deps = [\":l9\"])\n");
        build.push_str("lint(name = \"lint_c\", deps = [\":l11\", \":l2\"])\n");
        let ws = Workspace::new(&[
            (
                "rules/aspects.bzl",
                ASPECTS_BZL.replace("default = \"BOOM\"", &format!("default = \"{boom}\"")),
            ),
            ("a/BUILD.bazel", build),
        ]);
        if boom == "BOOM" {
            // Task B: the lint rules are worker jobs, their applications
            // committed at their merges.
            let timed = ws.build(
                "eval",
                8,
                &[FROZEN, ASPECT_JOBS, ("BAZEL_RS_TIMING", "1")],
                &["//a:lint_a", "//a:lint_b", "//a:lint_c"],
            );
            let timed = String::from_utf8_lossy(&timed.stderr);
            let line = timed
                .lines()
                .find(|line| line.starts_with("PERF-PARALLEL aspect_jobs "))
                .unwrap_or_else(|| panic!("the aspect jobs ran: {timed}"));
            let count = |name: &str| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix(name))
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or_default()
            };
            assert!(count("placed_rules=") > 0, "{line}");
            assert!(count("placed=") > 0, "{line}");
        }
        compare(
            &ws,
            &["//a:lint_a", "//a:lint_b", "//a:lint_c"],
            |success, text| {
                if boom == "BOOM" {
                    assert!(success, "{text}");
                    assert!(text.contains("lint @@//a:lint_c"), "{text}");
                } else {
                    assert!(!success, "{text}");
                    assert!(text.contains("the heavy aspect failed on l7"), "{text}");
                }
            },
        );
        if boom == "BOOM" {
            let timed = ws.build(
                "eval",
                8,
                &[FROZEN, NO_ASPECT_JOBS, ("BAZEL_RS_TIMING", "1")],
                &["//a:lint_a", "//a:lint_b", "//a:lint_c"],
            );
            let timed = String::from_utf8_lossy(&timed.stderr);
            let line = timed
                .lines()
                .find(|line| line.starts_with("PERF-PARALLEL aspects "))
                .unwrap_or_else(|| panic!("the aspects were prefetched: {timed}"));
            let used = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("used="))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();
            assert!(used > 0, "{line}");
        }
    }
}

const CONTEXT_BZL: &str = r#"
A1Info = provider(fields = ["seen"])
A2Info = provider(fields = ["files"])

def _a1_impl(target, ctx):
    deps = getattr(ctx.rule.attr, "deps", [])
    seen = [str(dep.label) + ":" + str(A2Info in dep) for dep in deps]
    print("a1 on %s (%s) target %s" % (target.label, ", ".join(seen), target))
    out = ctx.actions.declare_file(target.label.name + ".a1")
    ctx.actions.write(out, ",".join(seen))
    return [A1Info(seen = seen), OutputGroupInfo(a1 = depset([out]))]

a1 = aspect(implementation = _a1_impl)

def _a2_impl(target, ctx):
    print("a2 on %s" % target.label)
    out = ctx.actions.declare_file(target.label.name + ".a2")
    ctx.actions.write(out, "a2")
    return [A2Info(files = depset([out]))]

a2 = aspect(implementation = _a2_impl, attr_aspects = ["deps"])

def _lib_impl(ctx):
    print("lib %s" % ctx.label)
    return [DefaultInfo()]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _user_impl(ctx):
    print("%s sees %s" % (ctx.label, ", ".join([str(dep) + "=" + str(dep[A1Info].seen) for dep in ctx.attr.deps])))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out], transitive = [dep[OutputGroupInfo].a1 for dep in ctx.attr.deps]))]

one = rule(implementation = _user_impl, attrs = {"deps": attr.label_list(aspects = [a1])})
both = rule(implementation = _user_impl, attrs = {"deps": attr.label_list(aspects = [a1, a2])})
"#;

/// Task B: one aspect application reached in two contexts. An earlier rule
/// applies [a1] to :p, a later one [a1, a2] (a2 along `deps`): the
/// sequential walk plans (p, a1) once, in the earlier rule's context, and
/// the later rule's a2 applications at its own turn — whichever job
/// analyzes first. Under the latest-ready-first and classification jitter
/// runs of the matrix the later rule's job fills its context first.
#[test]
fn an_aspect_application_reached_in_two_contexts_is_the_first_in_plan_orders() {
    let mut build = String::from(
        "load(\"//rules:context.bzl\", \"both\", \"lib\", \"one\")\n\
         lib(name = \"q\")\nlib(name = \"r\", deps = [\":q\"])\n\
         lib(name = \"p\", deps = [\":r\"])\n\
         one(name = \"early\", deps = [\":p\"])\n",
    );
    for index in 0..6 {
        build.push_str(&format!("lib(name = \"z{index}\")\n"));
    }
    build.push_str(
        "both(name = \"late\", deps = [\":p\", \":z0\", \":z1\", \":z2\", \":z3\", \":z4\", \":z5\"])\n",
    );
    let ws = Workspace::new(&[
        ("rules/context.bzl", CONTEXT_BZL.to_owned()),
        ("a/BUILD.bazel", build),
    ]);
    compare(&ws, &["//a:early", "//a:late"], |success, text| {
        assert!(success, "{text}");
        assert!(text.contains("a2 on @@//a:q"), "{text}");
    });
}

/// Task B: an aspect-configured target reached through an alias first in
/// plan order and through its actual label first in time: its one shared
/// `Target` value (and the application's base view) is the alias's, as the
/// sequential walk builds it.
#[test]
fn an_alias_first_in_plan_order_keeps_its_aspect_configured_target() {
    let mut build = String::from(
        "load(\"//rules:context.bzl\", \"lib\", \"one\")\n\
         lib(name = \"q\")\nlib(name = \"p\", deps = [\":q\"])\n\
         alias(name = \"p_alias\", actual = \":p\")\n\
         one(name = \"through_alias\", deps = [\":p_alias\"])\n",
    );
    for index in 0..6 {
        build.push_str(&format!("lib(name = \"z{index}\")\n"));
    }
    build.push_str(
        "one(name = \"direct\", deps = [\":p\", \":z0\", \":z1\", \":z2\", \":z3\", \":z4\", \":z5\"])\n",
    );
    let ws = Workspace::new(&[
        ("rules/context.bzl", CONTEXT_BZL.to_owned()),
        ("a/BUILD.bazel", build),
    ]);
    compare(
        &ws,
        &["//a:through_alias", "//a:direct"],
        |success, text| {
            assert!(success, "{text}");
            assert!(text.contains("a1 on @@//a:p"), "{text}");
        },
    );
}

/// Task D: a flush (a config_setting on a Starlark flag not planned yet)
/// in each requested target's segment, so the entries flushed before the
/// later requested targets run on early boards during phase 1 and the walk
/// takes their jobs: the output is the walk's.
#[test]
fn early_boards_run_the_flushed_entries_during_phase_one() {
    let flags = r#"load(":defs.bzl", "flag", "uses")
package(default_visibility = ["//visibility:public"])
[flag(name = "f%d" % i, build_setting_default = True) for i in range(1, 4)]
[config_setting(name = "on%d" % i, flag_values = {":f%d" % i: "True"}) for i in range(1, 4)]
toolchain_type(name = "tt")
platform(name = "p")
uses(name = "u")
"#;
    let defs = r#"def _flag_impl(ctx):
    return []

flag = rule(implementation = _flag_impl, build_setting = config.bool(flag = True))

# Resolves the target platform first: the later flushes are deferred.
uses = rule(
    implementation = _flag_impl,
    toolchains = [config_common.toolchain_type("//flags:tt", mandatory = False)],
)
"#;
    let package = |index: usize, previous: Option<usize>| {
        format!(
            r#"load("//rules:defs.bzl", "w")
package(default_visibility = ["//visibility:public"])
[w(name = "l%d" % i, deps = [":l%d" % (i - 1)] if i else {deps}) for i in range(12)]
w(
    name = "top",
    content = select({{"//flags:on{index}": "on", "//conditions:default": "off"}}),
    deps = [":l11"],
)
"#,
            deps = previous.map_or("[]".to_owned(), |previous| format!(
                "[\"//p{previous}:top\"]"
            )),
        )
    };
    let ws = Workspace::new(&[
        ("flags/BUILD.bazel", flags.to_owned()),
        ("flags/defs.bzl", defs.to_owned()),
        ("p1/BUILD.bazel", package(1, None)),
        ("p2/BUILD.bazel", package(2, Some(1))),
        ("p3/BUILD.bazel", package(3, Some(2))),
    ]);
    let args = &[
        "--platforms=//flags:p",
        "--host_platform=//flags:p",
        "//flags:u",
        "//p1:top",
        "//p2:top",
        "//p3:top",
    ];
    compare(&ws, args, |success, text| {
        assert!(success, "{text}");
    });
    // The early boards are deterministic (one per flushed segment after
    // the platform's resolution); how many jobs they finish is timing.
    let timed = ws.build(
        "eval",
        8,
        &[
            FROZEN,
            EARLY,
            EARLY_MIN,
            EARLY_GROWTH,
            ("BAZEL_RS_TIMING", "1"),
        ],
        args,
    );
    let timed = String::from_utf8_lossy(&timed.stderr);
    assert!(
        timed
            .lines()
            .any(|line| line.starts_with("PERF-PARALLEL buffer starlark=")
                && !line.contains(" early_boards=0 ")),
        "{timed}"
    );
}
