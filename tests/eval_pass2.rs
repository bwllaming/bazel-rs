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
include!("support/analysis_order.rs");
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
        self.build_keeping_cache(mode, threads, env, args)
    }

    /// [`Self::build`] on the cache as it is (the snapshot store kept).
    fn build_keeping_cache(
        &self,
        mode: &str,
        threads: usize,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            // No clock-timed plain progress lines in the compared stderr.
            .env("BAZEL_RS_PROGRESS", "0")
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
        assert_same(&what, &eval, &walk, || keep_going_text(ws, args));
    }
}

/// The walk's `--keep_going` stderr.
fn keep_going_text(ws: &Workspace, args: &[&str]) -> String {
    let mut with = args.to_vec();
    with.push("--keep_going");
    stable(&ws.build("walk", 1, &[], &with))
}

/// An evaluated build's output against the walk's (the same exit code
/// checked): stdout of a success, stderr byte for byte — canonicalized
/// where this suite's default order is genuine (BAZEL_RS_DEFAULT_ANALYSIS_ORDER:
/// [`assert_genuine_same`]).
fn assert_same(what: &str, eval: &Output, walk: &Output, keep_going: impl FnOnce() -> String) {
    let (got, text) = (stable(eval), stable(walk));
    if eval.status.success() {
        assert_eq!(eval.stdout, walk.stdout, "{what}");
    }
    if genuine_default() {
        assert_genuine_same(what, eval.status.success(), &got, &text, keep_going);
    } else {
        assert_eq!(got, text, "{what}");
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
        assert_eq!(
            eval.status.code(),
            walk.status.code(),
            "{threads} threads, {env:?}"
        );
        assert_same(&format!("{threads} threads, {env:?}"), &eval, &walk, || {
            keep_going_text(&ws, &args)
        });
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

/// Remove `ws`'s cache (the repository store's trees are read-only:
/// `Workspace::build`'s removal stops at them).
fn fresh_cache(ws: &Workspace) {
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
}

/// A config_setting whose `flag_values` key is a rule returning
/// FeatureFlagInfo (not a build setting) needs the key's phase 2 at once:
/// the sequential pass's attempt of a target selecting on it linearizes
/// inside its need, and the tape's pass-2 walk runs there, inside that
/// attempt's observation frame. The walk's results stay whatever becomes
/// of the attempt, so what it observed (the host platform's and the
/// built-in repositories' packages it loads first, a frozen walk's
/// merges) must reach the recording itself: an abandoned attempt
/// (`BAZEL_RS_EVAL_TEST_ABANDON_WALKED`) dropped the walk's
/// observations before (and a frozen walk's merge hit the debug
/// assertion that none is framed), in the default mode too.
#[test]
fn a_walk_inside_an_abandoned_attempt_is_in_the_recording() {
    let ws = Workspace::new(
        1,
        r#"load("//:keys.bzl", "key", "top")
key(name = "k", srcs = ["kdata.txt"])
config_setting(name = "k_opt", flag_values = {":k": "opt"})
[top(
    name = "t%d" % i,
    deps = ["//p0:leaf"],
    mode = select({":k_opt": "saw opt", "//conditions:default": "no opt"}),
) for i in range(8)]
filegroup(name = "keyed", srcs = [":t%d" % i for i in range(8)])
"#,
    );
    for (path, text) in [
        (
            "keys.bzl",
            r#"def _key(ctx):
    return [config_common.FeatureFlagInfo(value = ctx.var["COMPILATION_MODE"])]

key = rule(implementation = _key, attrs = {"srcs": attr.label_list(allow_files = True)})

def _top(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.mode)
    return [DefaultInfo(files = depset([out]))]

top = rule(implementation = _top, attrs = {"deps": attr.label_list(), "mode": attr.string()})
"#,
        ),
        ("kdata.txt", "x\n"),
    ] {
        fs::write(ws.path().join(path), text).unwrap();
    }
    let args = [
        "//:keyed",
        "-c",
        "opt",
        "--experimental-builtin-modules=true",
    ];
    let recorded = |mode: &str, env: &[(&str, &str)]| {
        let record = ws.path().join("tmp/record.txt");
        let _ = fs::remove_file(&record);
        let mut recording: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "record"),
            ("BAZEL_RS_SNAPSHOT_DUMP", record.to_str().unwrap()),
        ];
        recording.extend_from_slice(env);
        fresh_cache(&ws);
        let output = ws.build(mode, 8, &recording, &args);
        assert!(output.status.success(), "{env:?}: {}", stable(&output));
        let record = fs::read_to_string(&record).expect("the record mode's dump");
        // The incremental index of a miss too (the tracker is the walk's,
        // moved into the engine for it, whatever the attempt).
        let index = ws.path().join("tmp/index.txt");
        let _ = fs::remove_file(&index);
        let mut tracking: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "on"),
            ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
        ];
        tracking.extend_from_slice(env);
        fresh_cache(&ws);
        let tracked = ws.build(mode, 8, &tracking, &args);
        assert!(tracked.status.success(), "{env:?}: {}", stable(&tracked));
        let index = fs::read_to_string(&index)
            .unwrap_or_else(|error| panic!("the index dump: {error}: {}", stable(&tracked)));
        (comparable_recording(&record), index, output)
    };
    let (walk, walk_index, walk_output) = recorded("walk", &[]);
    assert!(walk_index.contains("accesses"), "{walk_index}");
    for env in [
        &[][..],
        &[("BAZEL_RS_EVAL_TEST_ABANDON_WALKED", "1")],
        &[
            ("BAZEL_RS_EVAL_TEST_ABANDON_WALKED", "1"),
            ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
        ],
        &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "all")],
    ] {
        let (eval, index, output) = recorded("eval", env);
        if env.iter().any(|(name, _)| name.contains("ABANDON")) {
            // (The hook found an attempt a walk ran in.)
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(
                    "BAZEL_RS_EVAL_TEST_ABANDON_WALKED: an attempt a walk ran in abandoned"
                ),
                "{env:?}: {}",
                stable(&output)
            );
        }
        let missing: Vec<&String> = walk.iter().filter(|line| !eval.contains(line)).collect();
        let extra: Vec<&String> = eval.iter().filter(|line| !walk.contains(line)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{env:?}: the recording: missing {missing:?}, extra {extra:?}"
        );
        assert_eq!(index, walk_index, "{env:?}: the incremental index");
        assert_eq!(output.stdout, walk_output.stdout, "{env:?}: stdout");
    }
}

/// snapshot-miss-speed: with the incremental tracker on (a snapshot miss)
/// a native rule a frozen walk's worker published is merged, the accesses
/// its worker's run made replayed in its planning frame
/// (`buffer::tracked_natives`). `BAZEL_RS_T3_VERIFY_NATIVE_ACCESSES=1`
/// commits each such rule at its turn instead and checks them against the
/// commit's: clean for an output-group filegroup, an alias, and
/// filegroups whose source lookups observe (sources in subdirectories);
/// the index is the one of the natives committed at their turn either way.
#[test]
fn a_published_native_rules_accesses_are_its_commits() {
    let ws = Workspace::new(
        4,
        r#"load("//tools:groups.bzl", "grouped")
grouped(name = "made", deps = ["//p1:top"])
filegroup(name = "picked", srcs = [":made"], output_group = "extra")
filegroup(name = "plain", srcs = [":made", "a.txt", "sub/b.txt", "//p2:top"])
filegroup(name = "nested", srcs = [":plain", "sub/deeper/c.txt"])
alias(name = "named", actual = ":nested")
filegroup(name = "all", srcs = [":everything", ":picked", ":named"])
"#,
    );
    for (path, text) in [
        ("a.txt", "x\n"),
        ("sub/b.txt", "x\n"),
        ("sub/deeper/c.txt", "x\n"),
        ("tools/BUILD.bazel", ""),
        (
            "tools/groups.bzl",
            "def _grouped_impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.extra')\n    ctx.actions.write(out, 'x')\n    return [DefaultInfo(), OutputGroupInfo(extra = depset([out]))]\n\ngrouped = rule(implementation = _grouped_impl, attrs = {'deps': attr.label_list()})\n",
        ),
    ] {
        let path = ws.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let args = ["//:all", "--experimental-builtin-modules=true"];
    let index = ws.path().join("tmp/index.txt");
    let tracked = |mode: &str, env: &[(&str, &str)]| {
        let _ = fs::remove_file(&index);
        let _ = fs::remove_dir_all(ws.path().join("cache/worktrees"));
        let mut all: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "on"),
            ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
            ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
            ("BAZEL_RS_TIMING", "1"),
        ];
        all.extend_from_slice(env);
        let output = ws.build(mode, 8, &all, &args);
        assert!(output.status.success(), "{env:?}: {}", stable(&output));
        let dumped = fs::read_to_string(&index)
            .unwrap_or_else(|error| panic!("the index dump: {error}: {}", stable(&output)));
        (dumped, String::from_utf8_lossy(&output.stderr).into_owned())
    };
    // The walk's index; the natives committed at their turn (as before
    // snapshot-miss-speed) record it too.
    let (walk, _) = tracked("walk", &[]);
    let (committed, _) = tracked("eval", &[("BAZEL_RS_T3_TRACKED_NATIVES", "0")]);
    assert_eq!(committed, walk, "the committed natives' index");
    let (merged, stderr) = tracked("eval", &[]);
    assert_eq!(merged, walk, "the merged natives' index");
    // (The buffer's PERF line: natives merged as published.)
    let fast = |stderr: &str| {
        stderr
            .lines()
            .find(|line| line.starts_with("PERF-PARALLEL buffer starlark="))
            .and_then(|line| {
                line.split(' ')
                    .find(|part| part.starts_with("fast_native="))
            })
            .and_then(|part| part["fast_native=".len()..].parse::<usize>().ok())
            .unwrap_or(0)
    };
    assert!(
        fast(&stderr) > 0,
        "no native rule merged as published: {stderr}"
    );
    let (verified, stderr) = tracked("eval", &[("BAZEL_RS_T3_VERIFY_NATIVE_ACCESSES", "1")]);
    assert_eq!(verified, walk, "the verified natives' index");
    assert!(
        stderr.contains("native_commit:filegroup=") && stderr.contains("native_commit:alias="),
        "the natives were not checked at their turn: {stderr}"
    );
}

/// snapshot-miss2: with the incremental tracker on (a snapshot miss) an
/// aspect job whose prepare observed is placed as without the tracker,
/// the tracker's events of its prepare derived from what its worker
/// recorded with the aspect memo at its turn (`buffer::tracked_aspect_jobs`):
/// applications committed by an earlier entry are met, not planned again;
/// one another job planned is planned where the turn plans it.
/// `BAZEL_RS_T3_VERIFY_ASPECT_ACCESSES=1` prepares each such rule at its
/// turn instead and checks the derived events against the turn's; the
/// index is the walk's either way.
#[test]
fn an_aspect_jobs_tracker_events_are_its_turns() {
    let ws = Workspace::new(
        4,
        r#"load("//tools:asp.bzl", "lib", "user")
lib(name = "a", srcs = ["sub/a.txt"])
lib(name = "b", srcs = ["sub/deeper/b.txt"], deps = [":a"])
lib(name = "c", srcs = ["c.txt"], deps = [":b", "//p2:top"])
lib(name = "d", deps = [":c", ":a"])
user(name = "u1", deps = [":b"])
user(name = "u2", deps = [":d"])
user(name = "u3", deps = [":a", ":c"])
user(name = "u4", deps = [":d", ":u1"])
"#,
    );
    for (path, text) in [
        ("sub/a.txt", "x\n"),
        ("sub/deeper/b.txt", "x\n"),
        ("c.txt", "x\n"),
        ("tools/BUILD.bazel", ""),
        (
            "tools/asp.bzl",
            r#"AInfo = provider(fields = ["files"])

def _asp_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".a")
    ctx.actions.write(out, "x")
    files = [out]
    for dep in getattr(ctx.rule.attr, "deps", []):
        if AInfo in dep:
            files += dep[AInfo].files
    return [AInfo(files = files)]

asp = aspect(implementation = _asp_impl, attr_aspects = ["deps"])

def _lib_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.srcs))]

lib = rule(implementation = _lib_impl, attrs = {
    "deps": attr.label_list(),
    "srcs": attr.label_list(allow_files = True),
})

def _user_impl(ctx):
    files = []
    for dep in ctx.attr.deps:
        if AInfo in dep:
            files += dep[AInfo].files
    return [DefaultInfo(files = depset(files))]

user = rule(implementation = _user_impl, attrs = {"deps": attr.label_list(aspects = [asp])})
"#,
        ),
    ] {
        let path = ws.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let args = [
        "//:u4",
        "//:u3",
        "//:u2",
        "//:u1",
        "--experimental-builtin-modules=true",
    ];
    let index = ws.path().join("tmp/index.txt");
    let tracked = |mode: &str, env: &[(&str, &str)]| {
        let _ = fs::remove_file(&index);
        let _ = fs::remove_dir_all(ws.path().join("cache/worktrees"));
        let mut all: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "on"),
            ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
            ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
            ("BAZEL_RS_TIMING", "1"),
        ];
        all.extend_from_slice(env);
        let output = ws.build(mode, 8, &all, &args);
        assert!(output.status.success(), "{env:?}: {}", stable(&output));
        let dumped = fs::read_to_string(&index)
            .unwrap_or_else(|error| panic!("the index dump: {error}: {}", stable(&output)));
        (dumped, String::from_utf8_lossy(&output.stderr).into_owned())
    };
    // (The aspect jobs' PERF line's counter.)
    let counter = |stderr: &str, name: &str| {
        stderr
            .lines()
            .find(|line| line.starts_with("PERF-PARALLEL aspect_jobs "))
            .and_then(|line| line.split(' ').find(|part| part.starts_with(name)))
            .and_then(|part| part[name.len()..].parse::<usize>().ok())
            .unwrap_or(0)
    };
    let (walk, _) = tracked("walk", &[]);
    let (at_turn, stderr) = tracked("eval", &[("BAZEL_RS_T3_TRACKED_ASPECT_JOBS", "0")]);
    // (Their prepares observe: at their turn without the derivation.)
    assert!(
        counter(&stderr, "fallback_prepare=") > 0,
        "no aspect job at its turn: {stderr}"
    );
    assert_eq!(at_turn, walk, "the index of the aspect jobs at their turn");
    let (placed, stderr) = tracked("eval", &[]);
    assert_eq!(placed, walk, "the index of the placed aspect jobs");
    assert!(
        counter(&stderr, "placed_rules=") > 0 && counter(&stderr, "track_fallbacks=") == 0,
        "no aspect job placed with its derived events: {stderr}"
    );
    // Checked against the turn's, also where an application another job
    // planned is replayed from the walk's table (the workers' order varied
    // by their number until one is; a test hook would keep the snapshot
    // from tracking).
    let mut table_replays = 0;
    for workers in ["2", "3", "4", "6", "8", "12", "16"] {
        let (verified, stderr) = tracked(
            "eval",
            &[
                ("BAZEL_RS_T3_VERIFY_ASPECT_ACCESSES", "1"),
                ("BAZEL_RS_T3_WORKERS", workers),
            ],
        );
        assert_eq!(
            verified, walk,
            "the verified aspect jobs' index ({workers} workers)"
        );
        assert_eq!(counter(&stderr, "track_fallbacks="), 0, "{stderr}");
        eprintln!(
            "DBG workers {workers}: {}",
            counter(&stderr, "track_table_replays=")
        );
        table_replays += counter(&stderr, "track_table_replays=");
    }
    assert!(
        table_replays > 0,
        "no application replayed from the walk's table"
    );
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
    // Every run on a fresh cache.
    let fresh = || fresh_cache(&ws);
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
    let runs: [(usize, &[(&str, &str)]); 11] = [
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
        // Deltas built again at the merge while recording (the sweep is on
        // there by default, super::sweep::enabled).
        (
            8,
            &[
                ("BAZEL_RS_EVAL_FROZEN_PREPARES", "all"),
                ("BAZEL_RS_EVAL_SWEEP_RECORDING", "0"),
            ],
        ),
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
        if genuine_default() {
            // The genuine suite run (a success: canonicalized).
            assert_eq!(canonical(&eval.2), canonical(&walk.2), "{what}: stderr");
        } else {
            assert_eq!(eval.2, walk.2, "{what}: stderr");
        }
        assert_eq!(eval.3, walk.3, "{what}: stdout");
    }
}

/// What docs/PROFILE.md "Parity" requires of a `--profile` file's
/// analysis, read from `path`: the file's structure (otherData, a thread
/// metadata event for every lane used, the phase markers), the
/// `configured target` spans as a multiset of (label, rule) and the
/// `package creation` spans as a multiset of names, and the categories of
/// the events.
struct ProfileContent {
    targets: std::collections::BTreeMap<(String, String), usize>,
    packages: std::collections::BTreeMap<String, usize>,
    categories: std::collections::BTreeSet<String>,
}

fn profile_content(path: &Path) -> ProfileContent {
    use serde_json::Value;
    let text = fs::read_to_string(path).unwrap();
    let trace: Value = serde_json::from_str(&text).unwrap();
    let mut other: Vec<&String> = trace["otherData"].as_object().unwrap().keys().collect();
    other.sort();
    assert_eq!(
        other,
        [
            "bazel_version",
            "build_id",
            "date",
            "output_base",
            "profile_start_ts"
        ]
    );
    let events = trace["traceEvents"].as_array().unwrap();
    let lanes: std::collections::BTreeSet<u64> = events
        .iter()
        .filter(|event| event["ph"] == "M" && event["name"] == "thread_name")
        .map(|event| event["tid"].as_u64().unwrap())
        .collect();
    let mut content = ProfileContent {
        targets: Default::default(),
        packages: Default::default(),
        categories: Default::default(),
    };
    for event in events {
        if event["ph"] == "M" {
            continue;
        }
        assert!(
            lanes.contains(&event["tid"].as_u64().unwrap()),
            "no thread metadata for {event}"
        );
        assert!(event["ts"].is_u64(), "{event}");
        if event["ph"] == "X" {
            assert!(event["dur"].is_u64(), "{event}");
        }
        let category = event["cat"].as_str().unwrap().to_owned();
        let name = event["name"].as_str().unwrap().to_owned();
        match category.as_str() {
            "configured target" => {
                let rule = event["args"]["rule"].as_str().unwrap().to_owned();
                *content.targets.entry((name, rule)).or_default() += 1;
            }
            "package creation" => *content.packages.entry(name).or_default() += 1,
            _ => {}
        }
        content.categories.insert(category);
    }
    content
}

fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// `(packages loaded, targets configured)` of the `Analyzed` line.
fn analyzed_counts(output: &Output) -> (usize, usize) {
    let text = stable(output);
    let line = text
        .lines()
        .find(|line| line.starts_with("INFO: Analyzed "))
        .unwrap_or_else(|| panic!("{text}"));
    let number = |suffix: &str| {
        line.split(suffix)
            .next()
            .unwrap()
            .rsplit(['(', ' '])
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    (number(" packages loaded"), number(" targets configured"))
}

/// `--profile` plans with the evaluator, its flushes deferred and its flag
/// phase 2s speculated as without it (docs/PROFILE.md "Parity"): the
/// output is the walk's, and so is the profile's analysis — one
/// `configured target` span per configured target (the same (label, rule)
/// multiset as the walk's, as many as the `targets configured` count),
/// one `package creation` span per loaded package (genuine's names: `p0`,
/// the root package ``) — at every thread count, under jitter and
/// restarts, with the workers' prepares; the evaluator's own phases are
/// spans of their own.
#[test]
fn the_profile_has_the_walks_targets_and_packages() {
    let ws = Workspace::new(8, "");
    let args = |name: &str| {
        vec![
            "//:everything".to_owned(),
            format!(
                "--profile={}",
                ws.path().join(format!("tmp/{name}.json")).display()
            ),
        ]
    };
    let walk = ws.build("walk", 1, &[], &strs(&args("walk")));
    assert!(walk.status.success(), "{}", stable(&walk));
    let (packages, configured) = analyzed_counts(&walk);
    let expected = profile_content(&ws.path().join("tmp/walk.json"));
    assert_eq!(expected.targets.values().sum::<usize>(), configured);
    assert_eq!(expected.packages.values().sum::<usize>(), packages);
    assert!(expected.packages.values().all(|count| *count == 1));
    assert!(
        expected.packages.contains_key("p0"),
        "{:?}",
        expected.packages
    );
    assert!(
        expected.packages.contains_key(""),
        "{:?}",
        expected.packages
    );
    assert!(
        expected
            .targets
            .contains_key(&("//p0:top".to_owned(), "printing".to_owned())),
        "{:?}",
        expected.targets
    );
    // Deferred and speculated as without --profile.
    let stats = ws.build(
        "eval",
        8,
        &[("BAZEL_RS_EVAL_STATS", "1")],
        &strs(&args("stats")),
    );
    let text = String::from_utf8_lossy(&stats.stderr);
    assert!(stats.status.success(), "{text}");
    let count = |prefix: &str| {
        text.lines()
            .find_map(|line| line.split(prefix).nth(1))
            .and_then(|rest| rest.split(',').next())
            .and_then(|count| count.parse::<usize>().ok())
    };
    assert!(count("flushes deferred ").is_some_and(|n| n > 0), "{text}");
    assert!(
        count("speculative flag phase 2s ").is_some_and(|n| n > 0),
        "{text}"
    );
    let runs: [(usize, &[(&str, &str)]); 5] = [
        (8, &[]),
        (32, &[("BAZEL_RS_EVAL_JITTER", "100")]),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "all")]),
        (1, &[]),
    ];
    for (threads, env) in runs {
        let what = format!("{threads} threads, {env:?}");
        let eval = ws.build("eval", threads, env, &strs(&args("eval")));
        assert_eq!(
            eval.status.code(),
            walk.status.code(),
            "{what}: {}",
            stable(&eval)
        );
        assert_same(&what, &eval, &walk, || unreachable!());
        let got = profile_content(&ws.path().join("tmp/eval.json"));
        assert_eq!(got.targets, expected.targets, "{what}");
        assert_eq!(got.packages, expected.packages, "{what}");
        for category in [
            "build phase marker",
            "general information",
            "configured target",
            "package creation",
        ] {
            assert!(got.categories.contains(category), "{what}: {category}");
        }
        if threads > 1 {
            assert!(got.categories.contains("skyframe evaluator"), "{what}");
        }
    }
}

/// The incremental index of an evaluation whose requested targets end in
/// a deferred flush (a config_setting with Starlark `flag_values`, its
/// phase 2 pending at the requested target's compatibility check) is the
/// walk's: the check's lookup of the requested target is recorded outside
/// every frame, as the walk records it (not as the pending flush's
/// config_setting's lookup: an access the walk does not have).
#[test]
fn the_compatibility_checks_lookups_are_the_walks_with_a_flush_pending() {
    // (An incompatible target the wildcards skip: its check looks up more.)
    let ws = Workspace::new(
        2,
        r#"printing(name = "nope", text = "nope", deps = ["//p1:top"], target_compatible_with = ["@@platforms//:incompatible"])"#,
    );
    let index = ws.path().join("tmp/index.txt");
    let tracked = |mode: &str, threads: usize, env: &[(&str, &str)], target: &str| {
        let _ = fs::remove_file(&index);
        let mut all: Vec<(&str, &str)> = vec![
            ("BAZEL_RS_SNAPSHOT", "on"),
            ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
        ];
        all.extend_from_slice(env);
        let output = ws.build(
            mode,
            threads,
            &all,
            &[target, "--experimental-builtin-modules=true"],
        );
        assert!(
            output.status.success(),
            "{mode} {env:?}: {}",
            stable(&output)
        );
        fs::read_to_string(&index).unwrap_or_else(|error| {
            panic!(
                "{mode} {threads} {env:?} {target}: the index dump: {error}: {}",
                stable(&output)
            )
        })
    };
    for target in [
        "//p0:top",
        "//p1:leaf",
        "//:everything",
        "//...",
        "//p0:all",
    ] {
        let walk = tracked("walk", 1, &[], target);
        for (threads, env) in [
            (8, &[][..]),
            (8, &[("BAZEL_RS_EVAL_FROZEN_PREPARES", "all")][..]),
            (2, &[][..]),
            (32, &[("BAZEL_RS_EVAL_RESTARTS", "5")][..]),
        ] {
            let eval = tracked("eval", threads, env, target);
            assert_eq!(eval, walk, "{target} at {threads} threads {env:?}");
        }
    }
}

/// fix-access-node: with the incremental tracker on, a pass of the
/// linearizer in the middle of phase 1 (a config_setting's Starlark
/// flag_values flush applied at once: `--keep_going`, ordered flushes,
/// config_settings in fetched repositories) keeps the graph's interner,
/// through which the tape's `Access` events name their labels (0.4.23
/// dropped it there: on ~/code, `build --keep_going` on a snapshot miss
/// panicked at the sequential pass's next lookup of a `Tgt` node no
/// attempt had begun); the index is the walk's at 1-8 threads. Then the
/// snapshot's incremental reuse (a package edited) on the same engines.
#[test]
fn the_tracker_names_its_accesses_through_mid_phase_flushes() {
    let ws = Workspace::new(
        4,
        r#"config_setting(name = "root_on", flag_values = {":mode": "on"})
filegroup(name = "sources", srcs = ["a.txt"] + select({":root_on": ["on.txt"], "//conditions:default": []}))
printing(name = "after", text = select({":root_on": "on", "//conditions:default": "off"}), deps = [":sources", "//p2:top", "@ext1//:lib", "@ext2//:lib"])
printing(name = "later", text = "later", deps = ["@ext3//:lib", ":after"] + ["@hub//:s%d" % i for i in range(1, 9)])
load("//:ext.bzl", "tooled")
tooled(name = "with_tool", tool = "@ext1//:lib", deps = ["@hub//:s1", "@spoke2//:lib"])
filegroup(name = "all", srcs = [":with_tool", ":everything", ":after", ":later"])
"#,
    );
    // Repositories a rule fetches (the pool never does: the sequential pass
    // looks their targets up on a cold cache, after the flushes before).
    for (path, text) in [
        ("a.txt", "x\n"),
        ("on.txt", "x\n"),
        (
            "MODULE.bazel",
            "ext = use_repo_rule(\"//:ext.bzl\", \"ext\")\nhub = use_repo_rule(\"//:ext.bzl\", \"hub\")\next(name = \"ext1\")\next(name = \"ext2\")\next(name = \"ext3\")\next(name = \"spoke1\")\next(name = \"spoke2\")\next(name = \"spoke3\")\next(name = \"spoke4\")\next(name = \"spoke5\")\next(name = \"spoke6\")\next(name = \"spoke7\")\next(name = \"spoke8\")\nhub(name = \"hub\")\n",
        ),
        (
            "ext.bzl",
            "def _ext_impl(rctx):\n    rctx.file(\"f.txt\", \"x\")\n    rctx.file(\"BUILD.bazel\", \"config_setting(name = 'on', flag_values = {'@@//:mode': 'on'})\\nfilegroup(name = 'lib', srcs = ['f.txt'] + select({':on': [], '//conditions:default': []}), visibility = ['//visibility:public'])\")\n\next = repository_rule(implementation = _ext_impl)\n\ndef _hub_impl(rctx):\n    rctx.file(\"BUILD.bazel\", \"alias(name = 's1', actual = '@spoke1//:lib', visibility = ['//visibility:public'])\\nalias(name = 's2', actual = '@spoke2//:lib', visibility = ['//visibility:public'])\\nalias(name = 's3', actual = '@spoke3//:lib', visibility = ['//visibility:public'])\\nalias(name = 's4', actual = '@spoke4//:lib', visibility = ['//visibility:public'])\\nalias(name = 's5', actual = '@spoke5//:lib', visibility = ['//visibility:public'])\\nalias(name = 's6', actual = '@spoke6//:lib', visibility = ['//visibility:public'])\\nalias(name = 's7', actual = '@spoke7//:lib', visibility = ['//visibility:public'])\\nalias(name = 's8', actual = '@spoke8//:lib', visibility = ['//visibility:public'])\\n\")\n\nhub = repository_rule(implementation = _hub_impl)\n\ndef _tooled_impl(ctx):\n    return [DefaultInfo(files = depset(ctx.files.tool + ctx.files.deps))]\n\ntooled = rule(implementation = _tooled_impl, attrs = {\"tool\": attr.label(cfg = \"exec\"), \"deps\": attr.label_list(cfg = \"exec\")})\n",
        ),
    ] {
        fs::write(ws.path().join(path), text).unwrap();
    }
    let index = ws.path().join("tmp/index.txt");
    let tracked =
        |mode: &str, threads: usize, env: &[(&str, &str)], extra: &[&str], fresh: bool| {
            let _ = fs::remove_file(&index);
            if fresh {
                // A cold cache: the sequential pass fetches what the pool
                // cannot, looking targets up no attempt has begun yet.
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
            }
            let mut all: Vec<(&str, &str)> = vec![
                ("BAZEL_RS_SNAPSHOT", "on"),
                ("BAZEL_RS_SNAPSHOT_INDEX_DUMP", index.to_str().unwrap()),
            ];
            all.extend_from_slice(env);
            let mut args = vec!["//:all", "--experimental-builtin-modules=true"];
            args.extend_from_slice(extra);
            let output = ws.build_keeping_cache(mode, threads, &all, &args);
            assert!(
                output.status.success(),
                "{mode} {threads} {env:?} {extra:?}: {}",
                stable(&output)
            );
            (
                fs::read_to_string(&index).ok(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
    for extra in [&["--keep_going"][..], &[][..]] {
        let (walk, _) = tracked("walk", 4, &[], extra, true);
        let walk = walk.expect("the walk's index");
        for threads in [1, 2, 4, 8] {
            for env in [
                &[][..],
                &[("BAZEL_RS_EVAL_NO_DEFER", "1")][..],
                // The sequential pass runs what it needs itself (lookups of
                // `Tgt` nodes no attempt has begun after the flush).
                &[("BAZEL_RS_EVAL_NO_ASSIST", "1")][..],
            ] {
                let (eval, _) = tracked("eval", threads, env, extra, true);
                assert_eq!(
                    eval.as_deref(),
                    Some(walk.as_str()),
                    "{threads} threads {env:?} {extra:?}"
                );
            }
        }
    }
    // The incremental reuse: a miss saves, one package is edited, the next
    // run analyzes it again (eval at several thread counts).
    let p2 = ws.path().join("p2/BUILD.bazel");
    let original = fs::read_to_string(&p2).unwrap();
    for threads in [1, 4, 8] {
        for extra in [&["--keep_going"][..], &[][..]] {
            fs::write(&p2, &original).unwrap();
            let _ = tracked("eval", threads, &[], extra, true);
            fs::write(&p2, format!("{original}\n# edited {threads}\n")).unwrap();
            let (_, stderr) = tracked("eval", threads, &[], extra, false);
            assert!(
                !stderr.contains("internal error"),
                "{threads} {extra:?}: {stderr}"
            );
        }
    }
    fs::write(&p2, &original).unwrap();
}
