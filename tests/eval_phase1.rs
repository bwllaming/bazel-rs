//! The evaluator's phase-1 nodes (docs/EVALUATOR.md, worker B, S1) against
//! the walk: `BAZEL_RS_EVAL_VERIFY_PHASE1=1` evaluates the requests with the
//! node bodies under the inline driver after the walk's phase 1 and fails
//! the build on the first difference (plan order, configured nodes,
//! contexts, configurations, delegations, errors). Fixtures: select()
//! over a config_setting matching a platform constraint, alias chains,
//! a Starlark attribute transition with print(), toolchains with exec
//! groups and an exec edge, a platform tree, visibility errors, cycles.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const FILES: &[(&str, &str)] = &[
    ("MODULE.bazel", ""),
    (
        "defs.bzl",
        r#"
def _flag_impl(ctx):
    return []

string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _t_impl(settings, attr):
    print("transition " + settings["//:flag"])
    return {"//:flag": "on"}

flip = transition(implementation = _t_impl, inputs = ["//:flag"], outputs = ["//:flag"])

def _use_impl(ctx):
    return []

use = rule(
    implementation = _use_impl,
    attrs = {
        "deps": attr.label_list(cfg = flip),
        "tool": attr.label(cfg = "exec"),
        "grouped": attr.label(cfg = config.exec("g")),
        "_allowlist_function_transition": attr.label(
            default = "//tools/allowlists/function_transition_allowlist",
        ),
    },
    toolchains = ["//:tt"],
    exec_groups = {"g": exec_group(toolchains = ["//:tt"])},
)

def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.label.name)]

impl = rule(implementation = _impl_impl)
"#,
    ),
    (
        "BUILD.bazel",
        r#"
load(":defs.bzl", "impl", "string_flag", "use")

constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "mac", constraint_setting = ":os")
platform(name = "p", constraint_values = [":linux"])

config_setting(name = "is_linux", constraint_values = [":linux"])
config_setting(name = "flag_on", flag_values = {":flag": "on"})

string_flag(name = "flag", build_setting_default = "off")

filegroup(name = "leaf", srcs = ["a.txt"])
filegroup(
    name = "sel",
    srcs = select({":is_linux": [":leaf"], "//conditions:default": []}),
)
filegroup(
    name = "flagged",
    srcs = select({":flag_on": [":leaf"], "//conditions:default": ["b.txt"]}),
)
alias(name = "a1", actual = ":a2")
alias(name = "a2", actual = ":sel")

toolchain_type(name = "tt")
impl(name = "tc_impl")
toolchain(
    name = "tc",
    toolchain = ":tc_impl",
    toolchain_type = ":tt",
    target_compatible_with = [":linux"],
)

genrule(name = "gen", outs = ["gen.txt"], cmd = "touch $@")

use(
    name = "u",
    deps = [":a1", ":flagged"],
    tool = ":gen",
    grouped = ":leaf",
)

filegroup(name = "c1", srcs = [":c2"])
filegroup(name = "c2", srcs = [":c1"])
filegroup(name = "hidden_user", srcs = ["//private:hidden"])
"#,
    ),
    ("a.txt", ""),
    ("b.txt", ""),
    (
        "private/BUILD.bazel",
        r#"filegroup(name = "hidden", srcs = [], visibility = ["//visibility:private"])
"#,
    ),
    (
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        r#"package_group(name = "function_transition_allowlist", packages = ["public"])
"#,
    ),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for (path, text) in FILES {
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

    fn build(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_EVAL_VERIFY_PHASE1", "1")
            .env(
                "BAZEL_RS_EVAL_VERIFY_PHASE1_LOG",
                self.path().join("verify.log"),
            )
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--platforms=//:p",
                "--host_platform=//:p",
                "--extra_toolchains=//:tc",
                "--nobuild",
            ])
            .args(args)
            .output()
            .unwrap()
    }

    fn verified(&self) -> String {
        fs::read_to_string(self.path().join("verify.log")).unwrap_or_default()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_nodes_plan_like_the_walk() {
    let workspace = Workspace::new();
    let built = workspace.build(&["//:u", "//:sel", "//:a1", "//:gen"]);
    assert!(built.status.success(), "{}", stderr(&built));
    // The transition's print() is the walk's (once per application).
    assert!(
        stderr(&built).contains("transition off"),
        "{}",
        stderr(&built)
    );
    assert!(
        workspace.verified().starts_with("identical: "),
        "{}\n{}",
        workspace.verified(),
        stderr(&built)
    );
}

#[test]
fn the_nodes_fail_like_the_walk() {
    for (target, expected) in [
        ("//:c1", "cycle in dependency graph"),
        ("//:hidden_user", "is not visible from"),
        ("//:missing", "no such target"),
    ] {
        let workspace = Workspace::new();
        let failed = workspace.build(&[target]);
        let text = stderr(&failed);
        assert!(!failed.status.success(), "{target}: {text}");
        assert!(text.contains(expected), "{target}: {text}");
        assert!(!text.contains("EVAL_VERIFY_PHASE1"), "{target}: {text}");
    }
}
