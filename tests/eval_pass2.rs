//! BAZEL_RS_ANALYSIS=eval, S3 pass 2 (docs/EVALUATOR.md "S3, pass 2"): a
//! pooled evaluation defers the flushes of config_settings with Starlark
//! `flag_values`; their phase 2 runs in one later walk, the tape's effects
//! applied at their plan positions. The output must be the walk's byte for
//! byte at 1, 2, 8 and 32 threads, under `BAZEL_RS_EVAL_JITTER` and
//! `BAZEL_RS_EVAL_RESTARTS`, with the workers' prepares
//! (`BAZEL_RS_EVAL_FROZEN_PREPARES=all`) and without deferral
//! (`BAZEL_RS_EVAL_NO_DEFER=1`): phase-2 prints between phase-1 prints and
//! warnings around many flushes, a phase-2 failure inside a deferred
//! segment (worded by the flush's chain) and after the last flush, an
//! output conflict between targets of one package that finish out of
//! order, flags whose implementations print, and a genrule reading the
//! deferred target platform (a flush that stays ordered).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const RULES_BZL: &str = r#"
def _flag_impl(ctx):
    print("flag " + str(ctx.label) + " is " + ctx.build_setting_value)
    return [config_common.FeatureFlagInfo(value = ctx.build_setting_value)]

flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _printing_impl(ctx):
    print("implementation of " + str(ctx.label) + ": " + ctx.attr.text)
    if ctx.attr.text == "fail":
        fail("the implementation of " + ctx.label.name + " failed")
    out = ctx.actions.declare_file(ctx.attr.out or ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.text)
    files = [out]
    for dep in ctx.attr.deps:
        files.extend(dep[DefaultInfo].files.to_list())
    return [DefaultInfo(files = depset(files))]

printing = rule(
    implementation = _printing_impl,
    attrs = {"text": attr.string(), "out": attr.string(), "deps": attr.label_list()},
)

def _tag_impl(settings, attr):
    print("transition of " + attr.name + " to " + attr.mode)
    return {"//:mode": attr.mode}

_tag = transition(implementation = _tag_impl, inputs = [], outputs = ["//:mode"])

def _tagged_impl(ctx):
    files = []
    for dep in ctx.attr.deps:
        files.extend(dep[DefaultInfo].files.to_list())
    return [DefaultInfo(files = depset(files))]

tagged = rule(
    implementation = _tagged_impl,
    attrs = {
        "mode": attr.string(),
        "deps": attr.label_list(cfg = _tag),
        "_allowlist_function_transition": attr.label(
            default = "//tools/allowlists/function_transition_allowlist",
        ),
    },
)
"#;

/// A workspace of `packages` packages, each with config_settings on the
/// shared flags (a flush each), printing implementations, a deprecated
/// target (a warning) and a transition printing; `extra` is appended to
/// the root package.
fn files(packages: usize, extra: &str) -> Vec<(String, String)> {
    let mut files = vec![
        ("MODULE.bazel".to_owned(), String::new()),
        ("rules.bzl".to_owned(), RULES_BZL.to_owned()),
        (
            "tools/allowlists/function_transition_allowlist/BUILD.bazel".to_owned(),
            "package_group(name = 'function_transition_allowlist', packages = ['//...'])\n"
                .to_owned(),
        ),
    ];
    let mut all = Vec::new();
    for i in 0..packages {
        let next = if i + 1 < packages {
            format!("'//p{}:top'", i + 1)
        } else {
            String::new()
        };
        files.push((
            format!("p{i}/BUILD.bazel"),
            format!(
                r#"load("//:rules.bzl", "printing", "tagged")
package(default_visibility = ["//visibility:public"])
config_setting(name = "on", flag_values = {{"//:mode": "on", "//:other": "a"}})
config_setting(name = "other", flag_values = {{"//:other": "b"}})
printing(name = "old", text = "old {i}", deprecation = "use //p{i}:top")
printing(
    name = "leaf",
    text = select({{":on": "leaf {i} on", ":other": "leaf {i} other", "//conditions:default": "leaf {i}"}}),
)
printing(name = "mid", text = "mid {i}", deps = [":leaf", ":old"])
tagged(name = "flip", mode = "on", deps = [":mid"])
printing(name = "top", text = "top {i}", deps = [":flip", ":mid"] + [{next}])
"#
            ),
        ));
        all.push(format!("'//p{i}:top'"));
    }
    files.push((
        "BUILD.bazel".to_owned(),
        format!(
            r#"load(":rules.bzl", "flag", "printing")
package(default_visibility = ["//visibility:public"])
flag(name = "mode", build_setting_default = "off")
flag(name = "other", build_setting_default = "a")
filegroup(name = "everything", srcs = [{}])
{extra}
"#,
            all.join(", ")
        ),
    ));
    files
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(packages: usize, extra: &str) -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for (path, text) in files(packages, extra) {
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
        // A fresh cache per run: every package loads (and prints) again.
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
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--nobuild",
            ])
            .arg(format!("--loading_phase_threads={threads}"))
            // The default host platform needs the built-in modules.
            .args(
                (!args
                    .iter()
                    .any(|arg| arg.starts_with("--experimental-builtin-modules")))
                .then_some("--experimental-builtin-modules=false"),
            )
            .args(args)
            .envs(env.iter().copied())
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

fn compare(ws: &Workspace, args: &[&str], check: impl Fn(bool, &str)) {
    let walk = ws.build("walk", 1, &[], args);
    let text = stable(&walk);
    check(walk.status.success(), &text);
    let runs: [(usize, &[(&str, &str)]); 10] = [
        (1, &[]),
        (2, &[]),
        (8, &[]),
        (32, &[]),
        (8, &[("BAZEL_RS_EVAL_JITTER", "300")]),
        (32, &[("BAZEL_RS_EVAL_JITTER", "100")]),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "all")]),
        (
            32,
            &[
                ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
                ("BAZEL_RS_ANALYSIS_JITTER", "5"),
            ],
        ),
        (8, &[("BAZEL_RS_EVAL_NO_DEFER", "1")]),
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
        assert_eq!(stable(&eval), text, "{what}");
        assert_eq!(eval.stdout, walk.stdout, "{what}");
    }
}

#[test]
fn phase_two_prints_and_warnings_keep_the_walks_order() {
    let ws = Workspace::new(10, "");
    // The flushes are deferred, and the workers prepare.
    let stats = ws.build(
        "eval",
        8,
        &[
            ("BAZEL_RS_EVAL_STATS", "1"),
            ("BAZEL_RS_TIMING", "1"),
            ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
        ],
        &["//:everything"],
    );
    let text = String::from_utf8_lossy(&stats.stderr);
    assert!(stats.status.success(), "{text}");
    let deferred = text
        .lines()
        .find_map(|line| line.split("flushes deferred ").nth(1))
        .and_then(|rest| rest.split(',').next())
        .and_then(|count| count.parse::<usize>().ok());
    assert!(deferred.is_some_and(|count| count > 0), "{text}");
    let prepared = text
        .lines()
        .find_map(|line| line.split("PERF-PARALLEL frozen prepared=").nth(1))
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse::<usize>().ok());
    assert!(prepared.is_some_and(|count| count > 0), "{text}");
    for args in [
        &["//:everything"][..],
        &["//:everything", "--//:mode=on"],
        &["//:everything", "--//:other=b"],
        &["//p3:top", "//p0:top", "//p7:mid"],
    ] {
        compare(&ws, args, |success, text| {
            assert!(success, "{args:?}: {text}");
            assert!(text.contains("implementation of @@//p0:top"), "{text}");
            assert!(text.contains("transition of flip to on"), "{text}");
            assert!(text.contains("flag @@//:mode is"), "{text}");
        });
    }
}

#[test]
fn failures_in_deferred_segments_are_the_walks() {
    // A failing implementation reached before later flushes (its segment
    // is deferred), and one after the last flush.
    let ws = Workspace::new(
        6,
        r#"printing(name = "bad", text = "fail")
config_setting(name = "root_on", flag_values = {":mode": "on"})
printing(name = "after", text = select({":root_on": "after on", "//conditions:default": "after"}))
filegroup(name = "bad_first", srcs = [":bad", "//p2:top", ":after"])
filegroup(name = "bad_last", srcs = ["//p2:top", ":after", ":bad"])
"#,
    );
    for args in [
        &["//:bad_first"][..],
        &["//:bad_last"],
        &["//p4:top", "//:bad_first"],
    ] {
        compare(&ws, args, |success, text| {
            assert!(!success, "{args:?}: {text}");
            assert!(text.contains("the implementation of bad failed"), "{text}");
        });
    }
}

#[test]
fn output_conflicts_between_targets_finishing_out_of_order() {
    // Two targets of one package declare one output: the second in plan
    // order is the conflict, whichever finished first; a flush between.
    let ws = Workspace::new(
        3,
        r#"config_setting(name = "root_on", flag_values = {":mode": "on"})
printing(name = "slow", text = select({":root_on": "slow on", "//conditions:default": "slow"}), out = "same.txt", deps = ["//p0:top", "//p1:top", "//p2:top"])
printing(name = "fast", text = "fast", out = "same.txt")
filegroup(name = "conflict", srcs = [":slow", ":fast"])
filegroup(name = "conflict_reversed", srcs = [":fast", ":slow"])
"#,
    );
    for args in [
        &["//:conflict"][..],
        &["//:conflict_reversed"],
        &["//:conflict", "--//:mode=on"],
    ] {
        compare(&ws, args, |success, text| {
            assert!(!success, "{args:?}: {text}");
        });
    }
}

#[test]
fn a_deferred_target_platform_keeps_its_flush_ordered() {
    // No --platforms: a genrule's $(TARGET_CPU) resolves the default host
    // platform in phase 2, inside a flush's segment.
    let ws = Workspace::new(
        3,
        r#"config_setting(name = "root_on", flag_values = {":mode": "on"})
genrule(name = "cpu", outs = ["cpu.txt"], cmd = "echo $(TARGET_CPU) > $@")
printing(name = "after_cpu", text = select({":root_on": "on", "//conditions:default": "off"}), deps = [":cpu"])
filegroup(name = "with_cpu", srcs = [":after_cpu", "//p1:top"])
"#,
    );
    compare(
        &ws,
        &["//:with_cpu", "--experimental-builtin-modules=true"],
        |success, text| {
            assert!(success, "{text}");
        },
    );
}
