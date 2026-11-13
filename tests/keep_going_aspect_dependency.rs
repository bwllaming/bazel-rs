//! `--keep_going` and an attribute aspect's own dependency
//! (docs/KEEP_GOING.md): a rule carrying an aspect depends on the aspect's
//! values and so on the aspect's attribute dependencies (genuine Bazel:
//! AspectFunction computes them as the aspect's dependencies, a failure is
//! the application's AspectCreationException `Loading failed`). When such a
//! dependency fails in phase 2, the carrier is skipped like a target whose
//! dependency failed — but without the `Analysis failed` line (the
//! aspect's failure is its cause); a target above the carrier has it.
//!
//! Genuine receipt (Bazel 9.3.1, `bazel --batch --ignore_all_rc_files
//! build --nobuild -k //x:x //x:y //x:z //x:l //x:w //x:r` on [`MODULE`],
//! [`ASP`], [`C_BUILD`], [`X_BUILD`], captured 2026-11-13, exit 1, stderr
//! sha256 53994c3a74ad5f44…): the key lines in [`EXPECTED`] (sorted; Bazel
//! prints the warnings in its error map's order). Genuine also prints one
//! `ERROR: Loading failed` per failed aspect application (here two); the
//! native engines report the failing dependency's own error once, as for
//! any root cause (the divergence noted in docs/KEEP_GOING.md). Both
//! engines are compared with it, the evaluator also with every pass-2 walk
//! frozen (the merge buffer).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

use std::fs;
use std::path::Path;
use std::process::Command;

const MODULE: &str = "module(name = \"m\")\n";

const ASP: &str = r#"CfgInfo = provider(fields = ["v"])

def _cfg_impl(ctx):
    if ctx.attr.fail:
        fail("cfg %s fails" % ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".conf")
    ctx.actions.write(out, ctx.attr.content)
    return [DefaultInfo(files = depset([out])), CfgInfo(v = ctx.attr.content)]

cfg = rule(implementation = _cfg_impl, attrs = {"content": attr.string(), "fail": attr.bool()})

def _asp_impl(target, ctx):
    v = ctx.attr._cfg[CfgInfo].v
    print("aspect on %s sees %s" % (target.label, v))
    out = ctx.actions.declare_file(target.label.name + ".asp")
    ctx.actions.write(out, v)
    return [OutputGroupInfo(asp = depset([out]))]

asp = aspect(implementation = _asp_impl, attr_aspects = ["deps"], attrs = {"_cfg": attr.label(default = "//c:t")})

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".lib")
    ctx.actions.write(out, "lib")
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _user_impl(ctx):
    print("user %s" % ctx.label)
    out = ctx.actions.declare_file(ctx.label.name + ".user")
    ctx.actions.write(out, "user")
    return [DefaultInfo(files = depset([out], transitive = [dep[OutputGroupInfo].asp for dep in ctx.attr.deps]))]

user = rule(implementation = _user_impl, attrs = {"deps": attr.label_list(aspects = [asp])})
"#;

const C_BUILD: &str = r#"load("//rules:asp.bzl", "cfg")
cfg(name = "t", content = "one", fail = True, visibility = ["//visibility:public"])
"#;

const X_BUILD: &str = r#"load("//rules:asp.bzl", "lib", "user")
lib(name = "l")
lib(name = "m", deps = [":l"])
user(name = "x", deps = [":l"])
user(name = "y", deps = [":m"])
lib(name = "z")
lib(name = "w", deps = ["//c:t"])
lib(name = "r", deps = [":x", ":z"])
"#;

const TARGETS: &[&str] = &["//x:x", "//x:y", "//x:z", "//x:l", "//x:w", "//x:r"];

/// The genuine run's key stderr lines, sorted.
const EXPECTED: &[&str] = &[
    "ERROR: Build did NOT complete successfully",
    "ERROR: command succeeded, but not all targets were analyzed",
    "INFO: Analysis succeeded for only 2 of 6 top-level targets",
    "WARNING: errors encountered while analyzing target '//x:r', it will not be built. / Analysis failed",
    "WARNING: errors encountered while analyzing target '//x:w', it will not be built. / Analysis failed",
    "WARNING: errors encountered while analyzing target '//x:x', it will not be built.",
    "WARNING: errors encountered while analyzing target '//x:y', it will not be built.",
];

fn key_lines(stderr: &str) -> Vec<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut keys = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("WARNING: errors encountered while analyzing target ") {
            let mut key = (*line).to_owned();
            if lines.get(index + 1) == Some(&"Analysis failed") {
                key.push_str(" / Analysis failed");
            }
            keys.push(key);
        } else if line.starts_with("INFO: Analysis succeeded for only ")
            || line.starts_with("ERROR: command succeeded, but ")
            || *line == "ERROR: Build did NOT complete successfully"
        {
            keys.push((*line).to_owned());
        }
    }
    keys.sort();
    keys
}

fn run(
    root: &Path,
    mode: &str,
    threads: u32,
    snapshot: &str,
    env: &[(&str, &str)],
) -> (Option<i32>, String) {
    let _ = fs::remove_dir_all(root.join("cache"));
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(root)
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("BAZEL_RS_CACHE_GC", "off")
        .env("BAZEL_RS_SNAPSHOT", snapshot)
        .env("BAZEL_RS_ANALYSIS", mode)
        .envs(env.iter().copied())
        .args(["--ignore_all_rc_files", "--cache-dir=cache", "build"])
        .args([
            "--lockfile_mode=off",
            "--memory-limit=2048",
            &format!("--loading_phase_threads={threads}"),
            "--nobuild",
            "-k",
        ])
        .args(TARGETS)
        .explicit_modules()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Without the console lines whose order is scheduling's alone.
fn stable(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| {
            !line.starts_with("INFO: Elapsed time")
                && !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("bazel-rs: peak RSS")
                && !line.starts_with("PERF")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn keep_going_skips_a_rule_whose_aspects_dependency_failed() {
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let root = temp.path();
    for (path, text) in [
        ("MODULE.bazel", MODULE),
        ("rules/BUILD.bazel", ""),
        ("rules/asp.bzl", ASP),
        ("c/BUILD.bazel", C_BUILD),
        ("x/BUILD.bazel", X_BUILD),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    for directory in ["home", "tmp"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    let (_, walk) = run(root, "walk", 8, "off", &[]);
    const FROZEN: (&str, &str) = ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all");
    for (mode, threads, snapshot, env) in [
        ("walk", 8, "off", &[][..]),
        ("walk", 1, "on", &[]),
        ("eval", 8, "off", &[]),
        ("eval", 1, "off", &[]),
        ("eval", 8, "on", &[]),
        ("eval", 8, "off", &[FROZEN]),
        ("eval", 2, "off", &[FROZEN, ("BAZEL_RS_EVAL_JITTER", "300")]),
        ("eval", 8, "off", &[FROZEN, ("BAZEL_RS_EVAL_RESTARTS", "5")]),
    ] {
        let (code, stderr) = run(root, mode, threads, snapshot, env);
        let what = format!("{mode}, {threads} threads, snapshot {snapshot}, {env:?}: {stderr}");
        assert_eq!(code, Some(1), "{what}");
        assert_eq!(key_lines(&stderr), EXPECTED, "{what}");
        // Skipped, not analyzed: no implementation of the carriers ran, no
        // aspect application.
        assert!(!stderr.contains("user @@//x:"), "{what}");
        assert!(!stderr.contains("aspect on"), "{what}");
        assert!(stderr.contains("cfg t fails"), "{what}");
        assert!(!stderr.contains("internal"), "{what}");
        assert_eq!(stable(&stderr), stable(&walk), "{what}");
    }
}
