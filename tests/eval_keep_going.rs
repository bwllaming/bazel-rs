//! `--keep_going` on the parallel analysis evaluator (docs/EVALUATOR.md
//! "Selecting the engine", docs/KEEP_GOING.md): the evaluator plans it
//! with the walk's output byte for byte — phase-1 failures shared by
//! several requested targets, a failed requested target whose dependency
//! failed, a dependency cycle reached from several requested targets (the
//! walk words it from where it entered the cycle each time), and
//! config_settings with Starlark `flag_values` keys (one whose flag fails,
//! one whose flush plans another failing target: only its own flag fails
//! it), also inside the registered toolchains' resolution (a toolchain's
//! `target_settings`). On the pool and on the inline driver, with the
//! analysis snapshot off and on (a miss: recording, the tracker).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const REPORT: &str = "bazel-rs: analysis engine: ";

const DEFS: &str = r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    print("analyzed", ctx.label.name)
    return [DefaultInfo(files = depset([out] + [f for d in ctx.attr.deps for f in d[DefaultInfo].files.to_list()]))]

chain = rule(implementation = _impl, attrs = {"deps": attr.label_list(allow_files = True)})

def _fails(ctx):
    fail("intentional analysis failure of " + ctx.label.name)

fails = rule(implementation = _fails, attrs = {"deps": attr.label_list()})

def _flag(ctx):
    return [config_common.FeatureFlagInfo(value = ctx.build_setting_value)]

flag = rule(implementation = _flag, build_setting = config.string(flag = True))

def _badflag(ctx):
    fail("flag impl fails")

badflag = rule(implementation = _badflag, build_setting = config.string(flag = True))

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(v = ctx.attr.v)]

tc_rule = rule(implementation = _tc_impl, attrs = {"v": attr.string()})

def _use_impl(ctx):
    print("toolchain", ctx.toolchains["//t:tt"].v)
    return []

use_tc = rule(implementation = _use_impl, toolchains = ["//t:tt"])
"#;

const BUILD: &str = r#"load(":defs.bzl", "badflag", "chain", "fails", "flag")

chain(name = "leaf")

chain(name = "a", deps = [":leaf", "//p:shared_bad", "//p:ok"])

chain(name = "b", deps = [":leaf", "//p:shared_bad"])

chain(name = "c", deps = [":leaf", "//p:ok", "//p:no_such_target"])

chain(name = "d", deps = ["//q:private"])

chain(name = "e", deps = [":cyc1"])

chain(name = "cyc1", deps = [":cyc2"])

chain(name = "cyc2", deps = [":cyc1"])

fails(name = "f", deps = [":leaf"])

chain(name = "g", deps = [":f", ":leaf"])

chain(name = "i", deps = select({":fx": [":leaf"], "//conditions:default": ["//p:ok"]}))

chain(name = "j", deps = select({":bfx": [":leaf"], "//conditions:default": ["//p:ok"]}))

chain(name = "k", deps = [":g", ":a", ":i"])

flag(name = "fl", build_setting_default = "x")

badflag(name = "bfl", build_setting_default = "x")

config_setting(name = "fx", flag_values = {":fl": "x"}, visibility = ["//visibility:public"])

config_setting(name = "bfx", flag_values = {":bfl": "x"})

genrule(name = "gen", srcs = [":a"], outs = ["gen.out"], cmd = "cat $(SRCS) > $@")

exports_files(["src.txt"])

platform(name = "plat")
"#;

/// Aspects: an attribute aspect whose own attribute dependency is missing
/// (a phase-1 failure of its application), one whose implementation fails
/// (phase 2), one with a toolchain; rules carrying them on `deps`; a target
/// needing a toolchain type nothing provides.
const A_DEFS: &str = r#"def _ok_aspect_impl(target, ctx):
    return []

ok_aspect = aspect(
    implementation = _ok_aspect_impl,
    attr_aspects = ["deps"],
    toolchains = ["//t:tt"],
)

def _bad_attr_impl(target, ctx):
    return []

bad_attr_aspect = aspect(
    implementation = _bad_attr_impl,
    attr_aspects = ["deps"],
    attrs = {"_tool": attr.label(default = "//a:no_such_tool")},
)

def _bad_impl_impl(target, ctx):
    fail("intentional aspect failure on " + str(target.label))

bad_impl_aspect = aspect(implementation = _bad_impl_impl, attr_aspects = ["deps"])

def _carrier_impl(ctx):
    return []

ok_carrier = rule(implementation = _carrier_impl, attrs = {"deps": attr.label_list(aspects = [ok_aspect])})

attr_carrier = rule(implementation = _carrier_impl, attrs = {"deps": attr.label_list(aspects = [bad_attr_aspect])})

impl_carrier = rule(implementation = _carrier_impl, attrs = {"deps": attr.label_list(aspects = [bad_impl_aspect])})

def _needs_impl(ctx):
    return []

needs_missing = rule(implementation = _needs_impl, toolchains = ["//t:nobody"])
"#;

const A_BUILD: &str = r#"load(":defs.bzl", "attr_carrier", "impl_carrier", "needs_missing", "ok_carrier")
load("//:defs.bzl", "chain")

chain(name = "base")

chain(name = "dep", deps = [":base"])

attr_carrier(name = "on_dep_attr", deps = [":dep"])

impl_carrier(name = "on_dep_impl", deps = [":dep"])

ok_carrier(name = "ok", deps = [":dep"])

attr_carrier(name = "above_attr", deps = [":on_dep_attr"])

needs_missing(name = "no_toolchain")

chain(name = "after", deps = [":dep", ":base"])
"#;

const T_BUILD: &str = r#"load("//:defs.bzl", "tc_rule", "use_tc")

toolchain_type(name = "tt", visibility = ["//visibility:public"])

toolchain_type(name = "nobody", visibility = ["//visibility:public"])

tc_rule(name = "impl", v = select({"//:fx": "flagged", "//conditions:default": "plain"}))

toolchain(name = "tc", target_settings = ["//:fx"], toolchain = ":impl", toolchain_type = ":tt")

use_tc(name = "u1")

use_tc(name = "u2")
"#;

const P_BUILD: &str = r#"load("//:defs.bzl", "chain", "fails")

chain(name = "ok", visibility = ["//visibility:public"])

chain(name = "shared_bad", deps = ["//q:private"], visibility = ["//visibility:public"])

fails(name = "pf", visibility = ["//visibility:public"])
"#;

fn workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let root = temp.path();
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write(
        "MODULE.bazel",
        "module(name = 'm')\nregister_toolchains('//t:tc')\n",
    );
    write("defs.bzl", DEFS);
    write("BUILD", BUILD);
    write("src.txt", "source\n");
    write("p/BUILD", P_BUILD);
    write("t/BUILD", T_BUILD);
    write("a/defs.bzl", A_DEFS);
    write("a/BUILD", A_BUILD);
    write(
        "q/BUILD",
        "load('//:defs.bzl', 'chain')\nchain(name = 'private')\n",
    );
    for directory in ["home", "tmp"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    temp
}

/// `bazel-rs <args>` on engine `mode` with `--loading_phase_threads=<threads>`
/// and `BAZEL_RS_SNAPSHOT=<snapshot>` (a fresh cache: a miss).
fn run(root: &Path, mode: &str, snapshot: &str, threads: u32, args: &[&str]) -> Output {
    let _ = fs::remove_dir_all(root.join("cache"));
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("BAZEL_RS_CACHE_GC", "off")
        .env("BAZEL_RS_SNAPSHOT", snapshot)
        .env("BAZEL_RS_ANALYSIS_REPORT", "1")
        .env("BAZEL_RS_ANALYSIS", mode)
        .args(["--ignore_all_rc_files", "--cache-dir=cache", args[0]])
        .args([
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--memory-limit=2048",
            &format!("--loading_phase_threads={threads}"),
        ])
        .args(&args[1..])
        .output()
        .unwrap()
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
                && !line.starts_with("bazel-rs-snapshot:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn engines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(REPORT).map(str::to_owned))
        .collect()
}

/// The evaluator's run of `args` against the forced walk's.
fn same_as_walk(root: &Path, snapshot: &str, threads: u32, args: &[&str]) -> Output {
    let walk = run(root, "walk", snapshot, threads, args);
    let eval = run(root, "eval", snapshot, threads, args);
    let what = format!("{args:?}, {threads} threads, snapshot {snapshot}");
    assert_eq!(engines(&eval), ["eval"], "{what}: {}", stable(&eval));
    assert_eq!(eval.status.code(), walk.status.code(), "{what}");
    assert_eq!(stable(&eval), stable(&walk), "{what}");
    assert_eq!(eval.stdout, walk.stdout, "{what}");
    walk
}

#[test]
fn keep_going_plans_on_the_evaluator_as_the_walk() {
    let ws = workspace();
    for (threads, snapshot) in [(8, "off"), (1, "off"), (8, "on")] {
        for args in [
            &["build", "--nobuild", "-k", "//..."][..],
            &[
                "build",
                "--nobuild",
                "--keep_going",
                "//:k",
                "//:g",
                "//:f",
                "//:src.txt",
                "//:d",
                "//:e",
                "//:i",
                "//:j",
            ],
            &[
                "build",
                "--nobuild",
                "-k",
                "//:cyc2",
                "//:e",
                "//:cyc1",
                "//:j",
                "//:i",
                "//:b",
                "//:c",
            ],
            &[
                "build",
                "--nobuild",
                "-k",
                "--platforms=//:plat",
                "--host_platform=//:plat",
                "//:g",
                "//t:u1",
                "//:k",
                "//t:u2",
            ],
        ] {
            let walk = same_as_walk(ws.path(), snapshot, threads, args);
            assert!(!walk.status.success(), "{args:?}: {}", stable(&walk));
        }
    }
    // A build that executes: what analyzed is built.
    let walk = same_as_walk(
        ws.path(),
        "off",
        8,
        &["build", "-k", "//:gen", "//:leaf", "//:g"],
    );
    let stderr = stable(&walk);
    assert!(
        stderr.contains("errors encountered while analyzing target '//:g'"),
        "{stderr}"
    );
}

const PLATFORM: [&str; 2] = ["--platforms=//:plat", "--host_platform=//:plat"];

/// `--keep_going` with failing attribute aspects (their application's own
/// attribute dependency missing: phase 1; their implementation failing:
/// phase 2) on a dependency and on the requested target, and a target
/// needing a toolchain type nothing provides.
#[test]
fn keep_going_aspect_and_toolchain_failures_plan_as_the_walk() {
    let ws = workspace();
    for (threads, snapshot) in [(8, "off"), (1, "off"), (8, "on")] {
        for targets in [
            &["//a:on_dep_attr", "//a:after", "//a:ok"][..],
            &["//a:on_dep_impl", "//a:after", "//a:above_attr"],
            &["//a:no_toolchain", "//a:after", "//t:u1"],
            &["//a:..."],
        ] {
            let mut args = vec!["build", "--nobuild", "-k"];
            args.extend(PLATFORM);
            args.extend(targets);
            let walk = same_as_walk(ws.path(), snapshot, threads, &args);
            assert!(!walk.status.success(), "{args:?}: {}", stable(&walk));
        }
    }
}

/// `--keep_going --aspects`: a top-level aspect whose own attribute
/// dependency is missing, after the evaluator's requested targets (the
/// walk's aspect phase over the evaluator's published memos), with the
/// snapshot off and on.
#[test]
fn keep_going_top_level_aspect_failures_plan_as_the_walk() {
    let ws = workspace();
    for snapshot in ["off", "on"] {
        for aspect in ["//a:defs.bzl%bad_attr_aspect", "//a:defs.bzl%ok_aspect"] {
            let mut args = vec!["build", "--nobuild", "-k"];
            args.extend(PLATFORM);
            let flag = format!("--aspects={aspect}");
            args.push(&flag);
            args.extend(["//a:dep", "//a:after", "//:k", "//t:u2"]);
            same_as_walk(ws.path(), snapshot, 8, &args);
        }
    }
}

/// cquery over an attribute aspect that resolves toolchains (the capture's
/// aspect-toolchain targets, symbols mapped where replayed), snapshot on.
#[test]
fn cquery_over_an_aspect_with_toolchains_plans_as_the_walk() {
    let ws = workspace();
    for output in ["graph", "jsonproto"] {
        let mut args = vec!["cquery"];
        args.extend(PLATFORM);
        let flag = format!("--output={output}");
        args.push(&flag);
        args.push("deps(//a:ok)");
        let walk = same_as_walk(ws.path(), "on", 8, &args);
        assert!(walk.status.success(), "{}", stable(&walk));
    }
    // With a top-level aspect: the walk's aspect phase writes the capture
    // after the evaluator's replay.
    let mut args = vec!["cquery"];
    args.extend(PLATFORM);
    args.extend([
        "--aspects=//a:defs.bzl%ok_aspect",
        "--output=graph",
        "deps(//a:after)",
    ]);
    let walk = same_as_walk(ws.path(), "on", 8, &args);
    assert!(walk.status.success(), "{}", stable(&walk));
}
