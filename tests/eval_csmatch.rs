//! BAZEL_RS_ANALYSIS=eval and Starlark `flag_values` (docs/EVALUATOR.md,
//! "S2, the CsMatch split"): on the pool a config_setting's flush is a
//! marker the tape applies where the walk flushes, and its flags'
//! FeatureFlagInfo comes from a speculative phase 2 of the flag's subtree.
//! The output must be the walk's byte for byte at 1, 2, 8 and 32 threads,
//! under `BAZEL_RS_EVAL_JITTER` and `BAZEL_RS_EVAL_RESTARTS`: a transition
//! setting the flag, select() on the flag, defaults, a flag returning
//! FeatureFlagInfo (and one failing in its implementation), a flag read by
//! many config_settings, a flag whose implementation depends on another
//! flag through a select() (nested flushes), a toolchain's target_settings
//! on a flag, and a phase-2 failure of an unrelated target planned before a
//! flush (the walk reports it at the flush, worded by the chain above it).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const FLAGS_BZL: &str = r#"
def _plain_impl(ctx):
    print("plain flag " + ctx.build_setting_value)
    return []

plain_flag = rule(implementation = _plain_impl, build_setting = config.string(flag = True))

def _feature_impl(ctx):
    if ctx.build_setting_value == "boom":
        fail("the feature flag's implementation failed")
    return [config_common.FeatureFlagInfo(value = ctx.build_setting_value)]

feature_flag = rule(implementation = _feature_impl, build_setting = config.string(flag = True))

def _derived_impl(ctx):
    base = ctx.attr.base[config_common.FeatureFlagInfo].value
    return [config_common.FeatureFlagInfo(value = base + "-" + ctx.attr.mode)]

derived_flag = rule(
    implementation = _derived_impl,
    attrs = {"base": attr.label(), "mode": attr.string()},
)

def _set_impl(settings, attr):
    return {"//:feature": attr.value, "//:plain": "t-" + attr.value}

_set = transition(implementation = _set_impl, inputs = [], outputs = ["//:feature", "//:plain"])

def _with_flags_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps]))]

with_flags = rule(
    implementation = _with_flags_impl,
    attrs = {
        "value": attr.string(),
        "deps": attr.label_list(cfg = _set),
        "_allowlist_function_transition": attr.label(
            default = "//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _failing_impl(ctx):
    fail("an unrelated implementation failed")

failing = rule(implementation = _failing_impl)

def _printing_impl(ctx):
    print("implementation of " + ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.text)
    return [DefaultInfo(files = depset([out]))]

printing = rule(implementation = _printing_impl, attrs = {"text": attr.string(), "deps": attr.label_list()})

def _tool_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.label.name)]

tool = rule(implementation = _tool_impl)

def _uses_tool_impl(ctx):
    print("toolchain " + ctx.toolchains["//:tool_type"].name)
    return []

uses_tool = rule(implementation = _uses_tool_impl, toolchains = ["//:tool_type"])
"#;

fn files() -> Vec<(String, String)> {
    let mut files = vec![
        (
            "MODULE.bazel".to_owned(),
            "register_toolchains('//:tc_on', '//:tc_default')\n".to_owned(),
        ),
        ("flags.bzl".to_owned(), FLAGS_BZL.to_owned()),
        (
            "tools/allowlists/function_transition_allowlist/BUILD.bazel".to_owned(),
            "package_group(name = 'function_transition_allowlist', packages = ['//...'])\n"
                .to_owned(),
        ),
    ];
    let mut root = String::from(
        r#"package(default_visibility = ["//visibility:public"])
load(":flags.bzl", "plain_flag", "feature_flag", "derived_flag", "with_flags", "failing", "printing", "tool", "uses_tool")
plain_flag(name = "plain", build_setting_default = "off")
feature_flag(name = "feature", build_setting_default = "off")
feature_flag(name = "other", build_setting_default = "a")
config_setting(name = "plain_on", flag_values = {":plain": "on"})
config_setting(name = "feature_on", flag_values = {":feature": "on"})
config_setting(name = "feature_off", flag_values = {":feature": "off"})
config_setting(name = "feature_x", flag_values = {":feature": "x"})
config_setting(name = "both_on", flag_values = {":feature": "on", ":plain": "on"})
config_setting(name = "other_b", flag_values = {":other": "b"})
derived_flag(
    name = "derived",
    base = ":feature",
    mode = select({":other_b": "b", "//conditions:default": "a"}),
)
config_setting(name = "derived_on_a", flag_values = {":derived": "on-a"})
config_setting(name = "derived_off_b", flag_values = {":derived": "off-b"})
printing(
    name = "selecting",
    text = select({
        ":feature_on": "feature on",
        ":feature_x": "feature x",
        "//conditions:default": "feature default",
    }),
)
printing(
    name = "nested",
    text = select({
        ":derived_on_a": "derived on-a",
        ":derived_off_b": "derived off-b",
        "//conditions:default": "derived default",
    }),
)
with_flags(name = "set_on", value = "on", deps = [":selecting", ":nested", "//p0:t"])
with_flags(name = "set_x", value = "x", deps = [":selecting", "//p1:t"])
failing(name = "bad")
filegroup(name = "bad_then_select", srcs = [":bad", ":selecting"])
toolchain_type(name = "tool_type")
tool(name = "tool_on")
tool(name = "tool_default")
toolchain(name = "tc_on", toolchain = ":tool_on", toolchain_type = ":tool_type", target_settings = [":feature_on"])
toolchain(name = "tc_default", toolchain = ":tool_default", toolchain_type = ":tool_type")
uses_tool(name = "uses_tool")
platform(name = "plat")
"#,
    );
    let mut all = vec![
        "':selecting'".to_owned(),
        "':nested'".to_owned(),
        "':set_on'".to_owned(),
        "':set_x'".to_owned(),
        "':uses_tool'".to_owned(),
    ];
    // Many packages, each with config_settings of its own on the shared
    // flags, and a chain between them.
    for i in 0..12 {
        let package = format!("p{i}");
        let next = if i + 1 < 12 {
            format!("'//p{}:t'", i + 1)
        } else {
            String::new()
        };
        files.push((
            format!("{package}/BUILD.bazel"),
            format!(
                r#"load("//:flags.bzl", "printing")
config_setting(name = "mine_on", flag_values = {{"//:feature": "on"}})
config_setting(name = "mine_plain", flag_values = {{"//:plain": "on", "//:feature": "on"}})
printing(
    name = "t",
    text = select({{
        ":mine_on": "{i} on",
        ":mine_plain": "{i} plain",
        "//conditions:default": "{i} default",
    }}),
    deps = [{next}] + select({{"//:both_on": ["//:nested"], "//conditions:default": []}}),
    visibility = ["//visibility:public"],
)
"#
            ),
        ));
        all.push(format!("'//{package}:t'"));
    }
    root.push_str(&format!(
        "filegroup(name = 'everything', srcs = [{}])\n",
        all.join(", ")
    ));
    files.push(("BUILD.bazel".to_owned(), root));
    files
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for (path, text) in files() {
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
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--nobuild",
                "--platforms=//:plat",
                "--host_platform=//:plat",
            ])
            .arg(format!("--loading_phase_threads={threads}"))
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
    let runs: [(usize, &[(&str, &str)]); 7] = [
        (1, &[]),
        (2, &[]),
        (8, &[]),
        (32, &[]),
        (8, &[("BAZEL_RS_EVAL_JITTER", "300")]),
        (32, &[("BAZEL_RS_EVAL_JITTER", "100")]),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
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
fn starlark_flags_on_the_pool_give_the_walks_output() {
    let ws = Workspace::new();
    for args in [
        &["//:everything"][..],
        &["//:everything", "--//:feature=on"],
        &[
            "//:everything",
            "--//:feature=on",
            "--//:plain=on",
            "--//:other=b",
        ],
        &["//:everything", "--//:feature=x"],
    ] {
        compare(&ws, args, |success, text| {
            assert!(success, "{args:?}: {text}");
            assert!(text.contains("implementation of selecting"), "{text}");
        });
    }
}

#[test]
fn flag_and_flush_failures_on_the_pool_give_the_walks_error() {
    let ws = Workspace::new();
    // The flag's own implementation fails: the walk's flush reports it.
    compare(
        &ws,
        &["//:everything", "--//:feature=boom"],
        |success, text| {
            assert!(!success, "{text}");
            assert!(
                text.contains("the feature flag's implementation failed"),
                "{text}"
            );
        },
    );
    // An unrelated target planned before the flush fails there, worded by
    // the select() above the config_setting.
    compare(&ws, &["//:bad_then_select"], |success, text| {
        assert!(!success, "{text}");
        assert!(
            text.contains("an unrelated implementation failed"),
            "{text}"
        );
    });
    compare(
        &ws,
        &["//:bad_then_select", "//:everything", "--//:feature=on"],
        |success, text| {
            assert!(!success, "{text}");
        },
    );
}
