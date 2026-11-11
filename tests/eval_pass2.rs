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
include!("support/module_archives.rs");
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

def _tool_aspect_impl(target, ctx):
    return []

tool_aspect = aspect(
    implementation = _tool_aspect_impl,
    attr_aspects = ["deps"],
    attrs = {"_tool": attr.label(default = "//tools/aspect:tool")},
)

def _aspected_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps]))]

aspected = rule(
    implementation = _aspected_impl,
    attrs = {"deps": attr.label_list(aspects = [tool_aspect])},
)

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
            r#"load(":rules.bzl", "aspected", "flag", "printing")
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
            // The built-in modules resolve through the offline registry,
            // their archives seeded (no test reaches the network).
            .args(with_builtin_registry(
                &[
                    "--ignore_all_rc_files",
                    "--cache-dir=cache",
                    "build",
                    "--lockfile_mode=off",
                    "--memory-limit=2048",
                    "--nobuild",
                ]
                .into_iter()
                .map(str::to_owned)
                .chain([format!("--loading_phase_threads={threads}")])
                // The default host platform needs the built-in modules.
                .chain(
                    (!args
                        .iter()
                        .any(|arg| arg.starts_with("--experimental-builtin-modules")))
                    .then(|| "--experimental-builtin-modules=false".to_owned()),
                )
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

fn compare(ws: &Workspace, args: &[&str], check: impl Fn(bool, &str)) {
    let walk = ws.build("walk", 1, &[], args);
    let text = stable(&walk);
    check(walk.status.success(), &text);
    let runs: [(usize, &[(&str, &str)]); 11] = [
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
        // A new platform's flush walked at once, as before it was an op.
        (8, &[("BAZEL_RS_EVAL_PLATFORM_WALKS", "1")]),
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

const PLATFORMS_BZL: &str = r#"
def _uses_impl(ctx):
    print("toolchain of " + str(ctx.label) + ": " + str(ctx.toolchains["//plat:tt"] != None))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

uses = rule(
    implementation = _uses_impl,
    toolchains = [config_common.toolchain_type("//plat:tt", mandatory = False)],
)

def _to_impl(settings, attr):
    return {"//command_line_option:platforms": [attr.platform]}

_to = transition(implementation = _to_impl, inputs = [], outputs = ["//command_line_option:platforms"])

def _on_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps]))]

on_platform = rule(
    implementation = _on_impl,
    attrs = {
        "platform": attr.string(),
        "deps": attr.label_list(cfg = _to),
        "_allowlist_function_transition": attr.label(
            default = "//tools/allowlists/function_transition_allowlist",
        ),
    },
)
"#;

/// Where each platform is published (the flush after its memo's first use
/// on the tape) does not depend on when the pool computed it: the same
/// sequence at every thread count, under jitter and restarts; and the
/// output is the walk's.
#[test]
fn platforms_are_published_at_their_first_use_whatever_the_timing() {
    let ws = Workspace::new(
        4,
        r#"filegroup(name = "multi", srcs = ["//p3:top", "//plat:on_a", "//p1:top", "//plat:on_b", "//p2:top", "//plat:on_c"])
"#,
    );
    for (path, text) in [
        ("plat/defs.bzl", PLATFORMS_BZL),
        (
            "plat/BUILD.bazel",
            r#"load(":defs.bzl", "on_platform", "uses")
package(default_visibility = ["//visibility:public"])
toolchain_type(name = "tt")
platform(name = "a")
platform(name = "b")
platform(name = "c")
uses(name = "u")
on_platform(name = "on_a", platform = "//plat:a", deps = [":u", "//p0:top"])
on_platform(name = "on_b", platform = "//plat:b", deps = [":u", "//p2:top"])
on_platform(name = "on_c", platform = "//plat:c", deps = ["//p1:top", ":u"])
"#,
        ),
    ] {
        fs::create_dir_all(ws.path().join("plat")).unwrap();
        fs::write(ws.path().join(path), text).unwrap();
    }
    let args = ["//:multi", "--experimental-builtin-modules=true"];
    compare(&ws, &args, |success, text| {
        assert!(success, "{text}");
    });
    let log = [("BAZEL_RS_EVAL_PLATFORM_LOG", "1")];
    // (Sorted: the notes reach stderr through the held console, not
    // necessarily in flush order; the flush of each is what must agree.)
    let sequence = |output: &Output| -> Vec<String> {
        let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| line.starts_with("bazel-rs: eval platform published"))
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    };
    // A new platform's flush is deferred: its platform is an op of the
    // deferred walk.
    let stats = ws.build("eval", 8, &[("BAZEL_RS_EVAL_STATS", "1")], &args);
    let stats = String::from_utf8_lossy(&stats.stderr);
    assert!(stats.contains("walked at once: new platform 0,"), "{stats}");
    let first = ws.build("eval", 1, &log, &args);
    assert!(first.status.success(), "{}", stable(&first));
    let expected = sequence(&first);
    assert!(expected.len() >= 3, "{}", stable(&first));
    let runs: [(usize, &[(&str, &str)]); 6] = [
        (2, &[]),
        (8, &[]),
        (32, &[]),
        (8, &[("BAZEL_RS_EVAL_JITTER", "300")]),
        (32, &[("BAZEL_RS_EVAL_JITTER", "100")]),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
    ];
    for (threads, env) in runs {
        let env: Vec<(&str, &str)> = log.iter().chain(env).copied().collect();
        let eval = ws.build("eval", threads, &env, &args);
        assert!(eval.status.success(), "{}", stable(&eval));
        assert_eq!(sequence(&eval), expected, "{threads} threads, {env:?}");
    }
}

const PLATFORM_ASPECT_BZL: &str = r#"
def _seen_impl(target, ctx):
    has = ctx.target_platform_has_constraint(ctx.attr._red[platform_common.ConstraintValueInfo])
    print("aspect on %s: red=%s" % (target.label, has))
    return []

seen = aspect(
    implementation = _seen_impl,
    attr_aspects = ["deps"],
    attrs = {"_red": attr.label(default = "//plat:red")},
)

def _lib_impl(ctx):
    return [DefaultInfo()]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _lint_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

lint = rule(implementation = _lint_impl, attrs = {"deps": attr.label_list(aspects = [seen])})
"#;

/// Task B: aspect applications whose implementation reads the target
/// platform, under platforms the deferred walk publishes at their first
/// use: an application analyzed by a later rule's job (the platform
/// resolved at its position) and committed at an earlier one's merge is
/// the earlier position's, as the walk analyzes it (the jobs' reads hold
/// only where the walk's would; else the rule is prepared at its turn).
#[test]
fn aspect_applications_under_deferred_platforms_read_them_at_the_placing_rules_position() {
    let ws = Workspace::new(
        3,
        r#"filegroup(name = "multi", srcs = ["//plat:on_a", "//p1:top", "//plat:on_b", "//p2:top", "//plat:on_a2"])
"#,
    );
    for (path, text) in [
        ("plat/defs.bzl", PLATFORMS_BZL.to_owned()),
        ("plat/aspects.bzl", PLATFORM_ASPECT_BZL.to_owned()),
        (
            "plat/BUILD.bazel",
            r#"load(":defs.bzl", "on_platform")
load(":aspects.bzl", "lib", "lint")
package(default_visibility = ["//visibility:public"])
constraint_setting(name = "color")
constraint_value(name = "red", constraint_setting = ":color")
platform(name = "a", constraint_values = [":red"])
platform(name = "b")
lib(name = "base")
lib(name = "mid", deps = [":base"])
lint(name = "early", deps = [":mid"])
lint(name = "late", deps = [":mid", ":base"])
on_platform(name = "on_a", platform = "//plat:a", deps = [":early", "//p0:top"])
on_platform(name = "on_b", platform = "//plat:b", deps = [":late", ":early"])
on_platform(name = "on_a2", platform = "//plat:a", deps = ["//p1:top", ":late"])
"#
            .to_owned(),
        ),
    ] {
        fs::create_dir_all(ws.path().join("plat")).unwrap();
        fs::write(ws.path().join(path), text).unwrap();
    }
    let args = ["//:multi", "--experimental-builtin-modules=true"];
    compare(&ws, &args, |success, text| {
        assert!(success, "{text}");
        assert!(text.contains("red=True"), "{text}");
    });
    let walk = ws.build("walk", 1, &[], &args);
    let frozen = ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all");
    let runs: [(usize, &[(&str, &str)]); 4] = [
        (8, &[frozen, ("BAZEL_RS_PARITY_REVERSE", "1")]),
        (32, &[frozen, ("BAZEL_RS_T3_CLASSIFY_JITTER", "20000")]),
        (
            2,
            &[
                frozen,
                ("BAZEL_RS_PARITY_REVERSE", "1"),
                ("BAZEL_RS_VERIFY_DELTA", "1"),
            ],
        ),
        (8, &[frozen, ("BAZEL_RS_T3_ASPECT_JOBS", "0")]),
    ];
    for (threads, env) in runs {
        let eval = ws.build("eval", threads, env, &args);
        assert_eq!(stable(&eval), stable(&walk), "{threads} threads, {env:?}");
    }
}

/// The record mode's dump without what depends on the time of the run or
/// on the run's fresh cache: observations (without their stat keys and
/// `racy` flags), environment digests, repositories (not their integrity
/// entries' stat keys), pinned tables, taints (the test hook's aside),
/// the plan's registrations and replay records without their execution
/// times.
fn comparable_recording(dump: &str) -> Vec<String> {
    dump.lines()
        .filter_map(|line| {
            let kind = line.split(' ').next().unwrap_or_default();
            match kind {
                "OBS" | "ENV" | "REPO" => Some(
                    line.split(' ')
                        .filter(|part| !part.starts_with("key:") && *part != "racy")
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                "ROOT" | "MAPPING" | "REGISTERED" => Some(line.to_owned()),
                "TAINT" if !line.starts_with("TAINT test-hook ") => Some(line.to_owned()),
                "REPLAY" => Some(
                    line.split(' ')
                        .filter(|part| {
                            !part.starts_with("recorded:") && !part.starts_with("replay:")
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            }
        })
        .collect()
}

/// S3 with the analysis snapshot's recording and the incremental tracker
/// on (a snapshot miss, the default mode; docs/EVALUATOR.md "Selecting the
/// engine"): the flushes are deferred there too, and the recording (the
/// record mode's dump: the same observations, also of source files a
/// restarted attempt looked up first), the incremental index and the
/// output are the walk's at every thread count, under jitter and
/// restarts, with and without the workers' prepares.
#[test]
fn the_recording_and_the_incremental_index_are_the_walks() {
    let ws = Workspace::new(
        6,
        r#"config_setting(name = "root_on", flag_values = {":mode": "on"})
filegroup(name = "sources", srcs = ["a.txt", "sub/b.txt"] + select({":root_on": ["on.txt"], "//conditions:default": []}))
genrule(name = "cpu", srcs = [":sources"], outs = ["cpu.txt"], cmd = "echo $(TARGET_CPU) > $@")
printing(name = "after_cpu", text = select({":root_on": "on", "//conditions:default": "off"}), deps = [":cpu", "//p2:top"])
alias(name = "d", actual = "//p1:top")
printing(name = "via_d", text = "via", deps = [":d", "//p3:mid"])
aspected(name = "asp", deps = [":d", "//p3:mid"])
filegroup(name = "all", srcs = [":everything", ":after_cpu", ":sources", ":via_d", ":asp"])
"#,
    );
    for (path, text) in [
        ("a.txt", "x\n"),
        ("sub/b.txt", "x\n"),
        ("on.txt", "x\n"),
        (
            "tools/aspect/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nfilegroup(name = 'tool', srcs = ['tool.sh'])\n",
        ),
        ("tools/aspect/tool.sh", "x\n"),
    ] {
        let path = ws.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let args = ["//:all", "--experimental-builtin-modules=true"];
    // Every run on a fresh cache (the repository store's trees are
    // read-only: `Workspace::build`'s removal stops at them).
    let fresh = || {
        fn writable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::symlink_metadata(path)
                && metadata.is_dir()
            {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
                for entry in fs::read_dir(path).into_iter().flatten().flatten() {
                    writable(&entry.path());
                }
            }
        }
        let cache = ws.path().join("cache");
        writable(&cache);
        let _ = fs::remove_dir_all(&cache);
        assert!(!cache.exists());
    };
    let planned = |mode: &str, threads: usize, env: &[(&str, &str)]| {
        let record = ws.path().join("tmp/record.txt");
        let index = ws.path().join("tmp/index.txt");
        let _ = fs::remove_file(&record);
        let _ = fs::remove_file(&index);
        let mut recording: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "record"),
            ("BAZEL_RS_SNAPSHOT_DUMP", record.to_str().unwrap()),
            // The dropped frames' audit: what the evaluator's memo nodes the
            // tape never replayed observed is in the recording anyway.
            ("BAZEL_RS_EVAL_AUDIT", "1"),
        ];
        recording.extend_from_slice(env);
        fresh();
        let recorded = ws.build(mode, threads, &recording, &args);
        assert!(recorded.status.success(), "{}", stable(&recorded));
        for line in String::from_utf8_lossy(&recorded.stderr).lines() {
            if line.contains("eval audit:") {
                assert!(
                    line.contains("of memos 0"),
                    "{threads} threads, {env:?}: {line}"
                );
            }
        }
        let record = fs::read_to_string(&record).expect("the record mode's dump");
        // (A test hook such as the jitter keeps the snapshot, and so the
        // tracker, off: the recording only.)
        if env.iter().any(|(name, _)| {
            matches!(
                *name,
                "BAZEL_RS_EVAL_JITTER" | "BAZEL_RS_EVAL_TEST_SOURCE_SLEEP_MS"
            )
        }) {
            // (The record mode's report: timings.)
            let stderr = stable(&recorded)
                .lines()
                .filter(|line| !line.starts_with("bazel-rs-snapshot:"))
                .collect::<Vec<_>>()
                .join("\n");
            return (comparable_recording(&record), None, stderr, recorded.stdout);
        }
        let mut tracking: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "on"),
            ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
        ];
        tracking.extend_from_slice(env);
        fresh();
        let tracked = ws.build(mode, threads, &tracking, &args);
        assert!(tracked.status.success(), "{}", stable(&tracked));
        let index = fs::read_to_string(&index)
            .unwrap_or_else(|error| panic!("the index dump: {error}: {}", stable(&tracked)));
        (
            comparable_recording(&record),
            Some(index),
            stable(&tracked),
            tracked.stdout,
        )
    };
    let walk = planned("walk", 1, &[]);
    assert!(
        walk.0.iter().any(|line| line.contains("sub/b.txt")),
        "{:?}",
        walk.0
    );
    assert!(
        walk.1
            .as_ref()
            .is_some_and(|index| index.contains("accesses")),
        "{:?}",
        walk.1
    );
    let walk_recorded = planned("walk", 1, &[("BAZEL_RS_EVAL_JITTER", "300")]);
    // Deferred.
    let stats = ws.build(
        "eval",
        8,
        &[("BAZEL_RS_EVAL_STATS", "1"), ("BAZEL_RS_SNAPSHOT", "on")],
        &args,
    );
    let stats = String::from_utf8_lossy(&stats.stderr);
    assert!(!stats.contains("flushes deferred 0,"), "{stats}");
    let runs: [(usize, &[(&str, &str)]); 10] = [
        (1, &[]),
        (2, &[]),
        (8, &[]),
        (32, &[]),
        (8, &[("BAZEL_RS_EVAL_JITTER", "300")]),
        // A source lookup's observations kept with it late: its source is
        // not memoized for phase 2 before (review of eval-record).
        (8, &[("BAZEL_RS_EVAL_TEST_SOURCE_SLEEP_MS", "30")]),
        (
            32,
            &[
                ("BAZEL_RS_EVAL_TEST_SOURCE_SLEEP_MS", "10"),
                ("BAZEL_RS_EVAL_RESTARTS", "3"),
            ],
        ),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "all")]),
        (8, &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "0")]),
    ];
    for (threads, env) in runs {
        let eval = planned("eval", threads, env);
        let what = format!("{threads} threads, {env:?}");
        let missing: Vec<&String> = walk
            .0
            .iter()
            .filter(|line| !eval.0.contains(line))
            .collect();
        let extra: Vec<&String> = eval
            .0
            .iter()
            .filter(|line| !walk.0.contains(line))
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{what}: the recording: missing {missing:?}, extra {extra:?}"
        );
        let walk = if eval.1.is_some() {
            &walk
        } else {
            &walk_recorded
        };
        assert_eq!(eval.1, walk.1, "{what}: the incremental index");
        assert_eq!(eval.2, walk.2, "{what}: stderr");
        assert_eq!(eval.3, walk.3, "{what}: stdout");
    }
}
