//! The evaluator's lane ahead (`BAZEL_RS_EVAL_LANE_AHEAD`,
//! docs/EVALUATOR.md "Round 10"): a pool package load parked on a local
//! repository rule or an extension group requests its speculation at once,
//! and the registration packages are hinted at the RegToolchains
//! expansion's start. Only the speculations' start moves: the output (a
//! printing local rule's lines in order), the errors, the lockfile and the
//! loads the sequential pass runs are those without it.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
include!("support/explicit_modules.rs");
include!("support/console.rs");
include!("support/sealed_trees.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = crate::test_temp::root().join(format!(
            "bazel-rs-eval-lane-ahead-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A `build` with an empty cache (the lockfile kept: the extension's
    /// entry is reusable, so its group is prepared speculatively) with the
    /// lane ahead on or off: (exit code, stdout, stderr, the lockfile).
    fn cold(
        &self,
        ahead: bool,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> (Option<i32>, Vec<u8>, String, String) {
        let _ = sealed_trees::remove_tree(self.0.join("cache"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env("BAZEL_RS_ANALYSIS", "eval")
            .env("BAZEL_RS_EVAL_LANE_AHEAD", if ahead { "1" } else { "0" })
            .envs(env.iter().copied())
            .current_dir(&self.0);
        let output: Output = command
            // No clock-timed plain progress lines (whatever the runner's env).
            .env("BAZEL_RS_PROGRESS", "0")
            .explicit_modules()
            .output()
            .unwrap();
        let stderr =
            without_print_locations(&without_elapsed(&String::from_utf8_lossy(&output.stderr)))
                .replace(&self.0.display().to_string(), "WS");
        let lockfile = fs::read_to_string(self.0.join("MODULE.bazel.lock")).unwrap_or_default();
        (output.status.code(), output.stdout, stderr, lockfile)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}

/// An extension's local repository rules: `@tc` (a printing toolchain
/// repository), `@loc` (two packages; its rule spins a while so loads park
/// on it together).
fn fixture(registrations: &str) -> Workspace {
    let ws = Workspace::new();
    ws.file(
        "MODULE.bazel",
        &format!(
            "module(name = 'main')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'tc', 'loc')\n{registrations}"
        ),
    );
    ws.file("BUILD", "");
    ws.file(
        "ext.bzl",
        r#"def _local_impl(rctx):
    print("local rule %s starts" % rctx.attr.tag)
    n = 0
    for i in range(rctx.attr.spin):
        n += i
    rctx.file("BUILD", rctx.attr.build)
    for path, content in rctx.attr.files.items():
        rctx.file(path, content)
    print("local rule %s done" % rctx.attr.tag)

_local = repository_rule(
    implementation = _local_impl,
    local = True,
    attrs = {
        "tag": attr.string(),
        "spin": attr.int(),
        "build": attr.string(),
        "files": attr.string_dict(),
    },
)

def _ext_impl(mctx):
    _local(
        name = "tc",
        tag = "tc",
        spin = 1000,
        build = 'load(":defs.bzl", "impl")\ntoolchain_type(name = "type", visibility = ["//visibility:public"])\nimpl(name = "impl")\ntoolchain(name = "tc", toolchain_type = ":type", toolchain = ":impl")\n',
        files = {"defs.bzl": "def _impl(ctx):\n    return [platform_common.ToolchainInfo()]\nimpl = rule(implementation = _impl)\n"},
    )
    _local(
        name = "loc",
        tag = "loc",
        spin = 300000,
        build = "",
        files = {
            "a/BUILD": 'filegroup(name = "a", visibility = ["//visibility:public"])\n',
            "b/BUILD": 'filegroup(name = "b", visibility = ["//visibility:public"])\n',
        },
    )

ext = module_extension(implementation = _ext_impl)
"#,
    );
    ws.file(
        "rules/defs.bzl",
        "def _impl(ctx):\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl, attrs = {'deps': attr.label_list()}, toolchains = ['@tc//:type'])\n",
    );
    ws.file("rules/BUILD", "");
    ws.file("p/BUILD", "platform(name = 'p')\n");
    ws.file(
        "u/BUILD",
        "load('//rules:defs.bzl', 'my_rule')\nmy_rule(name = 'a', deps = ['@loc//a'])\nmy_rule(name = 'b', deps = ['@loc//b'])\n",
    );
    ws
}

/// A printing local rule registered as a toolchain repository, and two
/// packages of one local repository: the same output, lockfile and
/// sequential-pass loads with the lane ahead as without.
#[test]
fn lane_ahead_keeps_output_lockfile_and_sequential_loads() {
    let ws = fixture("register_toolchains('@tc//:all')\n");
    let stats = [("BAZEL_RS_EVAL_STATS", "1")];
    let args = ["build", "--host_platform=//p", "--platforms=//p", "//u:all"];
    // The lockfile's extension entry first.
    let primed = ws.cold(false, &stats, &args);
    assert_eq!(primed.0, Some(0), "{}", primed.2);
    let off = ws.cold(false, &stats, &args);
    assert_eq!(off.0, Some(0), "{}", off.2);
    assert_eq!(off.3, primed.3, "lockfile");
    for tag in ["tc", "loc"] {
        assert_eq!(
            off.2.matches(&format!("local rule {tag} starts")).count(),
            1,
            "{}",
            off.2
        );
    }
    let strip = |stderr: &str| {
        stderr
            .lines()
            .filter(|line| !line.starts_with("bazel-rs: eval"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // The loads left to the sequential pass for a repository sentinel.
    let sentinels = |stderr: &str| -> u64 {
        stderr
            .lines()
            .find_map(|line| {
                line.split_once("repository sentinels ")
                    .map(|(_, rest)| rest)
            })
            .and_then(|rest| rest.split(',').next())
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or_else(|| panic!("no repository sentinels count: {stderr}"))
    };
    for _ in 0..2 {
        let on = ws.cold(true, &stats, &args);
        assert_eq!(on.0, off.0);
        assert_eq!(on.1, off.1);
        assert_eq!(strip(&on.2), strip(&off.2));
        assert_eq!(on.3, off.3, "lockfile");
        assert!(
            sentinels(&on.2) <= sentinels(&off.2),
            "ahead: {} sequential repository sentinels, off: {}",
            sentinels(&on.2),
            sentinels(&off.2)
        );
    }
}

/// An invalid registration before a valid one (no `--keep_going`): the
/// lane ahead may execute the later registration's local rule, but the
/// error, the output and the lockfile are those without it.
#[test]
fn lane_ahead_keeps_an_invalid_registration_before_a_valid_one() {
    let ws = fixture("register_toolchains('//nothere:all', '@tc//:all')\n");
    let args = ["build", "--host_platform=//p", "--platforms=//p", "//u:all"];
    let primed = ws.cold(false, &[], &args);
    assert_ne!(primed.0, Some(0), "{}", primed.2);
    let off = ws.cold(false, &[], &args);
    assert_eq!(off.3, primed.3, "lockfile");
    assert!(
        off.2
            .contains("invalid registered toolchain '//nothere:all'"),
        "{}",
        off.2
    );
    for _ in 0..2 {
        let on = ws.cold(true, &[], &args);
        assert_eq!((on.0, &on.1, &on.2, &on.3), (off.0, &off.1, &off.2, &off.3));
    }
}
