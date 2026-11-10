//! The analysis engine's selection (docs/EVALUATOR.md "Selecting the
//! engine", src/engine/eval/select.rs): the evaluator plans by default,
//! `BAZEL_RS_ANALYSIS=walk` forces the walk, and the requests the
//! evaluator does not cover (`--keep_going`, top-level aspects — also the
//! validation aspect —, cquery, aquery, print_action) plan with the walk,
//! silently: the output is the forced walk's byte for byte.
//! `BAZEL_RS_ANALYSIS_REPORT=1` names the engine each planning ran on.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

/// The walk-forced suite run builds the binary with
/// `BAZEL_RS_DEFAULT_ANALYSIS=walk`: its default is the walk.
fn default_is_walk() -> bool {
    option_env!("BAZEL_RS_DEFAULT_ANALYSIS") == Some("walk")
}

const REPORT: &str = "bazel-rs: analysis engine: ";

fn workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let root = temp.path();
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write("MODULE.bazel", "module(name = 'm')\n");
    write(
        "defs.bzl",
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, ctx.label.name)\n    print('analyzed', ctx.label.name)\n    return [DefaultInfo(files = depset([out] + [f for d in ctx.attr.deps for f in d[DefaultInfo].files.to_list()]))]\nchain = rule(implementation = _impl, attrs = {'deps': attr.label_list()})\n\
         def _aspect_impl(target, ctx):\n    print('aspect on', target.label.name)\n    return []\nan_aspect = aspect(implementation = _aspect_impl, attr_aspects = ['deps'])\n",
    );
    write(
        "BUILD",
        "load(':defs.bzl', 'chain')\nchain(name = 'leaf')\nchain(name = 'top', deps = [':leaf', '//p:t'])\nchain(name = 'broken', deps = [':nope'])\ngenrule(name = 'g', srcs = [':top'], outs = ['g.out'], cmd = 'cat $(locations :top) > $@')\nplatform(name = 'plat')\n",
    );
    write(
        "p/BUILD",
        "load('//:defs.bzl', 'chain')\nchain(name = 't', visibility = ['//visibility:public'])\n",
    );
    for directory in ["home", "tmp"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    temp
}

/// `bazel-rs <command> <args>` with `BAZEL_RS_ANALYSIS=<mode>` (`None`:
/// unset) and the engine report on.
fn run(root: &Path, mode: Option<&str>, command: &str, args: &[&str]) -> Output {
    let _ = fs::remove_dir_all(root.join("cache"));
    let mut process = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
    process
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("BAZEL_RS_CACHE_GC", "off")
        .env("BAZEL_RS_SNAPSHOT", "off")
        .env("BAZEL_RS_ANALYSIS_REPORT", "1")
        .args(["--ignore_all_rc_files", "--cache-dir=cache", command])
        .args([
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--memory-limit=2048",
            "--loading_phase_threads=8",
        ])
        .args(args);
    if let Some(mode) = mode {
        process.env("BAZEL_RS_ANALYSIS", mode);
    }
    process.output().unwrap()
}

/// The engine reports of a run's stderr.
fn engines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(REPORT).map(str::to_owned))
        .collect()
}

/// stderr without the engine report and the timing lines.
fn stable(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| {
            !line.starts_with(REPORT)
                && !line.starts_with("INFO: Elapsed time")
                && !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("bazel-rs: peak RSS")
                && !line.starts_with("PERF")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default and an explicit `eval` plan on `expected` (the evaluator
/// or a fallback), with the forced walk's exit code, stdout and stderr.
fn same_as_walk(root: &Path, command: &str, args: &[&str], expected: &str) -> Output {
    let walk = run(root, Some("walk"), command, args);
    assert_eq!(engines(&walk), ["walk (forced)"], "{}", stable(&walk));
    let modes: &[Option<&str>] = &[None, Some(""), Some("eval")];
    for &mode in modes {
        let output = run(root, mode, command, args);
        let what = format!("{command} {args:?} with BAZEL_RS_ANALYSIS={mode:?}");
        let wanted = if mode.is_none() && default_is_walk() {
            "walk (forced)"
        } else {
            expected
        };
        assert_eq!(engines(&output), [wanted], "{what}: {}", stable(&output));
        assert_eq!(
            output.status.code(),
            walk.status.code(),
            "{what}: {}",
            stable(&output)
        );
        assert_eq!(stable(&output), stable(&walk), "{what}");
        assert_eq!(output.stdout, walk.stdout, "{what}");
    }
    walk
}

#[test]
fn builds_plan_with_the_evaluator_by_default() {
    let ws = workspace();
    for args in [
        &["--nobuild", "//:top", "//:g"][..],
        &["//:g"],
        &["--platforms=//:plat", "//:g"],
    ] {
        let walk = same_as_walk(ws.path(), "build", args, "eval");
        assert!(walk.status.success(), "{args:?}: {}", stable(&walk));
    }
    let walk = same_as_walk(ws.path(), "build", &["--nobuild", "//:broken"], "eval");
    assert!(!walk.status.success());
}

#[test]
fn uncovered_requests_fall_back_to_the_walk_silently() {
    let ws = workspace();
    for flag in ["--keep_going", "-k"] {
        same_as_walk(
            ws.path(),
            "build",
            &["--nobuild", flag, "//:top", "//:broken"],
            "walk (fallback: --keep_going)",
        );
    }
    same_as_walk(
        ws.path(),
        "build",
        &["--nobuild", "--aspects=//:defs.bzl%an_aspect", "//:top"],
        "walk (fallback: top-level aspects)",
    );
    same_as_walk(
        ws.path(),
        "build",
        &[
            "--nobuild",
            "--experimental_use_validation_aspect",
            "//:top",
        ],
        "walk (fallback: top-level aspects)",
    );
    same_as_walk(
        ws.path(),
        "cquery",
        &["--platforms=//:plat", "deps(//:top)"],
        "walk (fallback: cquery capture)",
    );
    same_as_walk(
        ws.path(),
        "aquery",
        &["--platforms=//:plat", "//:g"],
        "walk (fallback: aquery capture)",
    );
    same_as_walk(
        ws.path(),
        "print_action",
        &["--platforms=//:plat", "//:g"],
        "walk (fallback: aquery capture)",
    );
}

#[test]
fn an_unknown_engine_is_an_error() {
    let ws = workspace();
    let output = run(
        ws.path(),
        Some("parallel"),
        "build",
        &["--nobuild", "//:top"],
    );
    assert!(!output.status.success());
    assert!(
        stable(&output)
            .contains("BAZEL_RS_ANALYSIS=parallel: expected `eval` (the default) or `walk`"),
        "{}",
        stable(&output)
    );
}
