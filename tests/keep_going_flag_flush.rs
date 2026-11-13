//! `--keep_going` and config_settings with Starlark `flag_values`
//! (docs/KEEP_GOING.md): a config_setting fails only when one of its own
//! flag keys fails analysis (its implementation, or a dependency of the
//! flag) — ConfigSetting depends on the flag's ConfiguredTargetValue — and
//! a requested target whose own select() keys fail is that target's own
//! failure (no `Analysis failed` line). Another target's failure in the
//! same analysis fails only what is above it.
//!
//! Genuine receipt (Bazel 9.3.1, `bazel --batch --ignore_all_rc_files
//! build --nobuild -k //:k //:i //:j //:j2 //:d //:g //:e //:ke //:kj` on
//! [`MODULE`], [`DEFS`], [`BUILD`], captured 2026-11-13, exit 1, stderr
//! sha256 630b74aae6767611…): the key lines in [`EXPECTED`] (sorted; Bazel
//! prints the warnings in its error map's order). Both engines are
//! compared with it.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

use std::fs;
use std::path::Path;
use std::process::Command;

const MODULE: &str = "module(name = \"m\")\n";

const DEFS: &str = r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]
chain = rule(implementation = _impl, attrs = {"deps": attr.label_list()})
def _fails(ctx):
    fail("intentional analysis failure of " + ctx.label.name)
fails = rule(implementation = _fails, attrs = {"deps": attr.label_list()})
def _flag(ctx):
    return [config_common.FeatureFlagInfo(value = ctx.build_setting_value)]
flag = rule(implementation = _flag, build_setting = config.string(flag = True))
def _badflag(ctx):
    fail("flag impl fails")
badflag = rule(implementation = _badflag, build_setting = config.string(flag = True))
def _depflag(ctx):
    return [config_common.FeatureFlagInfo(value = ctx.build_setting_value)]
depflag = rule(implementation = _depflag, build_setting = config.string(flag = True), attrs = {"deps": attr.label_list()})
"#;

const BUILD: &str = r#"load(":defs.bzl", "badflag", "chain", "depflag", "fails", "flag")
chain(name = "leaf")
fails(name = "f", deps = [":leaf"])
chain(name = "g", deps = [":f", ":leaf"])
flag(name = "fl", build_setting_default = "x")
badflag(name = "bfl", build_setting_default = "x")
depflag(name = "dfl", build_setting_default = "x", deps = [":f"])
config_setting(name = "fx", flag_values = {":fl": "x"})
config_setting(name = "bfx", flag_values = {":bfl": "x"})
config_setting(name = "dfx", flag_values = {":dfl": "x"})
chain(name = "i", deps = select({":fx": [":leaf"], "//conditions:default": []}))
chain(name = "j", deps = select({":bfx": [":leaf"], "//conditions:default": []}))
chain(name = "j2", deps = select({":bfx": [":leaf"], "//conditions:default": []}))
chain(name = "d", deps = select({":dfx": [":leaf"], "//conditions:default": []}))
chain(name = "k", deps = [":g", ":i"])
config_setting(name = "mfx", flag_values = {":missing_flag": "x"})
chain(name = "e", deps = select({":mfx": [":leaf"], "//conditions:default": []}))
chain(name = "ke", deps = [":e"])
chain(name = "kj", deps = [":j"])
"#;

const TARGETS: &[&str] = &[
    "//:k", "//:i", "//:j", "//:j2", "//:d", "//:g", "//:e", "//:ke", "//:kj",
];

/// The genuine run's key stderr lines, sorted.
const EXPECTED: &[&str] = &[
    "ERROR: Build did NOT complete successfully",
    "ERROR: command succeeded, but not all targets were analyzed",
    "INFO: Analysis succeeded for only 1 of 9 top-level targets",
    "WARNING: errors encountered while analyzing target '//:d', it will not be built.",
    "WARNING: errors encountered while analyzing target '//:e', it will not be built.",
    "WARNING: errors encountered while analyzing target '//:g', it will not be built. / Analysis failed",
    "WARNING: errors encountered while analyzing target '//:j', it will not be built.",
    "WARNING: errors encountered while analyzing target '//:j2', it will not be built.",
    "WARNING: errors encountered while analyzing target '//:k', it will not be built. / Analysis failed",
    "WARNING: errors encountered while analyzing target '//:ke', it will not be built. / Analysis failed",
    "WARNING: errors encountered while analyzing target '//:kj', it will not be built. / Analysis failed",
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

fn run(root: &Path, mode: &str, threads: u32, snapshot: &str) -> (Option<i32>, String) {
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

#[test]
fn keep_going_fails_a_config_setting_only_for_its_own_flags() {
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let root = temp.path();
    for (path, text) in [
        ("MODULE.bazel", MODULE),
        ("defs.bzl", DEFS),
        ("BUILD", BUILD),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    for directory in ["home", "tmp"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    for (mode, threads, snapshot) in [
        ("walk", 8, "off"),
        ("eval", 8, "off"),
        ("eval", 1, "off"),
        ("eval", 8, "on"),
    ] {
        let (code, stderr) = run(root, mode, threads, snapshot);
        let what = format!("{mode}, {threads} threads, snapshot {snapshot}: {stderr}");
        assert_eq!(code, Some(1), "{what}");
        assert_eq!(key_lines(&stderr), EXPECTED, "{what}");
        // The failed flag's root cause, reported once per select() whose key
        // it fails (genuine: `errors encountered resolving select() keys`).
        for target in ["//:j", "//:j2", "//:d", "//:e"] {
            assert!(
                stderr.contains(&format!(
                    "errors encountered resolving select() keys for {target}"
                )),
                "{what}"
            );
        }
        assert!(
            !stderr.contains("errors encountered resolving select() keys for //:i"),
            "{what}"
        );
    }
}
