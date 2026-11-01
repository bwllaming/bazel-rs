//! Derived hits (crate::snapshot::derive, docs/ANALYSIS_SNAPSHOT.md
//! "Derived hits"): a `build` or `test` whose targets are a subset of a
//! saved snapshot's, with options that differ only in what selects targets
//! and runs tests, executes that snapshot's plan restricted to them.
//!
//! Every scenario runs on two copies of one workspace that take turns at
//! one path (absolute paths, keys and outputs are the same): the derive
//! side with the snapshot on, the reference side with
//! `BAZEL_RS_SNAPSHOT=off` (every command a full analysis). Exit codes,
//! stdout and the action dumps (`BAZEL_RS_DUMP_ACTIONS`) must be equal; so
//! must stderr, but for what a derived hit intentionally prints otherwise:
//! like a warm Bazel server it replays none of the analysis' prints and
//! reports `0 targets configured`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/console.rs");

const DERIVE: &str = "derive";
const REFERENCE: &str = "reference";

struct Twin {
    base: tempfile::TempDir,
}

struct Pair {
    derive: Output,
    reference: Output,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl Twin {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = tempfile::Builder::new()
            .prefix("snapshot-derive-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        let twin = Self { base };
        for side in [DERIVE, REFERENCE] {
            fs::create_dir_all(twin.side(side).join("ws")).unwrap();
            fs::create_dir_all(twin.side(side).join("home")).unwrap();
        }
        for (path, content) in files {
            twin.write(path, content);
        }
        twin
    }

    fn side(&self, side: &str) -> PathBuf {
        self.base.path().join(side)
    }

    fn run_root(&self) -> PathBuf {
        self.base.path().join("run")
    }

    fn write(&self, path: &str, content: &str) {
        for side in [DERIVE, REFERENCE] {
            let path = self.side(side).join("ws").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn run_side(&self, side: &str, args: &[&str]) -> Output {
        fs::rename(self.side(side), self.run_root()).unwrap();
        let home = self.run_root().join("home");
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "300s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.run_root().join("cache").display()
            ))
            .args(with_explicit_modules(args))
            .arg("--lockfile_mode=off")
            .current_dir(self.run_root().join("ws"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("LC_ALL", "C")
            .env("BAZEL_RS_DUMP_ACTIONS", "1");
        if side == DERIVE {
            command
                .env("BAZEL_RS_SNAPSHOT", "on")
                .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1");
        } else {
            command.env("BAZEL_RS_SNAPSHOT", "off");
        }
        let output = command.output().expect("bazel-rs runs");
        fs::rename(self.run_root(), self.side(side)).unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// `args` on both sides; exit, stdout and the comparable stderr equal.
    fn same(&self, args: &[&str]) -> Pair {
        let pair = Pair {
            derive: self.run_side(DERIVE, args),
            reference: self.run_side(REFERENCE, args),
        };
        assert_eq!(
            pair.derive.status.code(),
            pair.reference.status.code(),
            "{args:?}: exit\n{}",
            text(&pair.derive.stderr)
        );
        assert_eq!(
            without_test_durations(&text(&pair.derive.stdout)),
            without_test_durations(&text(&pair.reference.stdout)),
            "{args:?}: stdout"
        );
        assert_eq!(
            comparable(&pair.derive),
            comparable(&pair.reference),
            "{args:?}: stderr\n{}",
            text(&pair.derive.stderr)
        );
        pair
    }
}

impl Pair {
    fn diagnostics(&self) -> String {
        text(&self.derive.stderr)
            .lines()
            .filter(|line| line.starts_with("bazel-rs-snapshot: "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn derived(&self) -> bool {
        self.diagnostics()
            .lines()
            .any(|line| line.starts_with("bazel-rs-snapshot: derived "))
    }

    fn assert_derived(&self) -> &Self {
        assert!(self.derived(), "not derived:\n{}", self.diagnostics());
        let stderr = text(&self.derive.stderr);
        assert!(
            stderr
                .lines()
                .any(|line| line.starts_with("INFO: Analyzed ")
                    && line.ends_with(" packages loaded, 0 targets configured).")),
            "{stderr}"
        );
        self
    }

    fn assert_full(&self) -> &Self {
        assert!(
            !self.derived(),
            "derived unexpectedly:\n{}",
            self.diagnostics()
        );
        self
    }

    fn assert_success(&self) -> &Self {
        assert!(
            self.derive.status.success(),
            "{}",
            text(&self.derive.stderr)
        );
        self
    }

    fn assert_diagnostic(&self, needle: &str) -> &Self {
        assert!(
            self.diagnostics().contains(needle),
            "no {needle:?} in:\n{}",
            self.diagnostics()
        );
        self
    }
}

/// stderr without the snapshot diagnostics, durations normalized, and
/// without what analysis prints (its `print()` lines and the `INFO:
/// Analyzed` counts): a derived hit prints none of it. The action dump's
/// blocks are compared as a set: a derived plan keeps the saved plan's
/// order.
fn comparable(output: &Output) -> (String, Vec<String>) {
    let mut kept = String::new();
    let mut actions = Vec::new();
    for line in without_elapsed(&text(&output.stderr)).split_inclusive('\n') {
        if line.starts_with("ACTION ") {
            actions.push(line.to_owned());
            continue;
        }
        if line.starts_with("  ") && !actions.is_empty() {
            actions.last_mut().unwrap().push_str(line);
            continue;
        }
        if line.starts_with("bazel-rs-snapshot: ")
            || line.starts_with("PERF")
            || line.starts_with("DEBUG: ")
            || line.starts_with("INFO: Analyzed ")
        {
            continue;
        }
        kept.push_str(&without_test_durations(line));
    }
    actions.sort();
    (kept, actions)
}

/// A test summary's `in 0.1s` durations as `in <T>s`.
fn without_test_durations(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(" in ") {
        out.push_str(&rest[..at + 4]);
        rest = &rest[at + 4..];
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if digits > 0 && rest[digits..].starts_with('s') {
            out.push_str("<T>");
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

const RULES: &str = r#"print("rules.bzl loaded")

def _exe(ctx):
    print("analyzing " + ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

_ATTRS = {
    "script": attr.string(),
    "data": attr.label_list(allow_files = True),
}

script_test = rule(implementation = _exe, test = True, attrs = _ATTRS)

def _stamped(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.info_file],
        command = "grep -c . {} > {}".format(ctx.info_file.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

stamped = rule(implementation = _stamped)
"#;

const ROOT_BUILD: &str = r##"load("//:rules.bzl", "script_test", "stamped")

genrule(name = "a", srcs = ["in.txt"], outs = ["a.txt"], cmd = "cat $< > $@")

genrule(name = "b", srcs = [":a"], outs = ["b.txt"], cmd = "cat $< > $@; echo b >> $@")

script_test(name = "pass_test", data = [":b"], script = "#!/bin/bash\necho test-ran\ncat b.txt\n")

script_test(name = "other_test", data = [":a"], script = "#!/bin/bash\necho other-ran\n")

script_test(name = "nightly_test", tags = ["nightly"], script = "#!/bin/bash\nexit 0\n")

stamped(name = "stamped")
"##;

fn fixture() -> Twin {
    Twin::new(&[
        ("MODULE.bazel", "module(name = \"derive\")\n"),
        ("rules.bzl", RULES),
        ("BUILD.bazel", ROOT_BUILD),
        ("in.txt", "input one\n"),
    ])
}

/// The labels of `name` (a target pattern file) in the workspace copies.
fn pattern_file(twin: &Twin, name: &str, labels: &[&str]) -> String {
    twin.write(name, &format!("{}\n", labels.join("\n")));
    format!("--target_pattern_file={name}")
}

/// ~/code's CI sequence: a `build --keep_going` of everything, then a
/// `test` of the test rules among them with test-only options and a tag
/// filter: the test derives its plan from the build's snapshot, runs the
/// same actions and tests, and prints the same results.
#[test]
fn a_test_of_a_built_subset_derives_its_plan() {
    let twin = fixture();
    let build = pattern_file(
        &twin,
        "build.txt",
        &[
            "//:a",
            "//:b",
            "//:pass_test",
            "//:other_test",
            "//:nightly_test",
        ],
    );
    twin.same(&["build", "--keep_going", &build])
        .assert_success()
        .assert_full()
        .assert_diagnostic("saved ");
    let tests = pattern_file(
        &twin,
        "test.txt",
        &["//:pass_test", "//:other_test", "//:nightly_test"],
    );
    let test = [
        "test",
        "--keep_going",
        "--test_output=errors",
        "--test_summary=terse",
        "--local_test_jobs=2",
        "--test_tag_filters=-nightly",
        tests.as_str(),
    ];
    let pair = twin.same(&test);
    pair.assert_success().assert_derived();
    let stdout = text(&pair.derive.stdout);
    let stderr = text(&pair.derive.stderr);
    assert!(
        stdout.contains("Executed 2 out of 2 tests: 2 tests pass."),
        "{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("INFO: Found 1 target and 2 test targets..."),
        "{stderr}"
    );
    // Nothing analyzed, nothing printed by the analysis.
    assert!(!stderr.contains("analyzing pass_test"), "{stderr}");
    // A build of a subset after it derives too (from the build's).
    twin.same(&["build", "//:b"])
        .assert_success()
        .assert_derived();
}

/// Invalidation: a source edit keeps the plan (execution sees the new
/// content); a BUILD edit fails the covering snapshot's validation, so the
/// test is analyzed in full and saved, and the stale snapshot leaves its
/// family (a build of its other targets is analyzed too).
#[test]
fn edits_are_seen_and_a_stale_snapshot_leaves_its_family() {
    let twin = fixture();
    let build = pattern_file(&twin, "build.txt", &["//:a", "//:b", "//:pass_test"]);
    twin.same(&["build", &build]).assert_success();
    twin.write("in.txt", "input two\n");
    let test = ["test", "--test_output=all", "//:pass_test"];
    let pair = twin.same(&test);
    pair.assert_success().assert_derived();
    assert!(text(&pair.derive.stdout).contains("input two"));
    twin.write(
        "BUILD.bazel",
        &ROOT_BUILD.replace("echo b >> $@", "echo changed >> $@"),
    );
    let pair = twin.same(&test);
    pair.assert_success()
        .assert_full()
        .assert_diagnostic("the full analysis runs)")
        .assert_diagnostic("saved ");
    assert!(text(&pair.derive.stdout).contains("changed"));
    // Its own snapshot from now on; the stale one left the family.
    twin.same(&test).assert_success().assert_diagnostic("hit ");
    twin.same(&["build", "//:a"]).assert_success().assert_full();
}

/// Only options that select targets or run tests may differ: another
/// `--define` is another analysis, and a label the snapshot did not
/// analyze is not covered.
#[test]
fn other_options_and_other_targets_are_analyzed() {
    let twin = fixture();
    twin.same(&["build", "//:a", "//:pass_test"])
        .assert_success();
    twin.same(&["test", "--define=x=1", "//:pass_test"])
        .assert_success()
        .assert_full();
    twin.same(&["test", "//:pass_test", "//:other_test"])
        .assert_success()
        .assert_full();
}

/// A `--nobuild` snapshot serves an executing build of its targets (the
/// workspace status actions are added), but not one whose actions read the
/// workspace status (the `--nobuild` plan has no producer for it).
#[test]
fn a_nobuild_snapshot_serves_an_executing_build_without_status_readers() {
    let twin = fixture();
    twin.same(&["build", "--nobuild", "//:b", "//:stamped"])
        .assert_success();
    twin.same(&["build", "//:b"])
        .assert_success()
        .assert_derived();
    twin.same(&["build", "//:stamped"])
        .assert_success()
        .assert_full()
        .assert_diagnostic("reads the workspace status");
}

/// `BAZEL_RS_SNAPSHOT_DERIVE=0` keeps every miss a full analysis.
#[test]
fn derivation_can_be_switched_off() {
    let twin = fixture();
    twin.same(&["build", "//:a", "//:pass_test"])
        .assert_success();
    fs::rename(twin.side(DERIVE), twin.run_root()).unwrap();
    let home = twin.run_root().join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            twin.run_root().join("cache").display()
        ))
        .args(with_explicit_modules(&["test", "//:pass_test"]))
        .arg("--lockfile_mode=off")
        .current_dir(twin.run_root().join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TMPDIR", &home)
        .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
        .env("BAZEL_RS_SNAPSHOT_DERIVE", "0")
        .output()
        .unwrap();
    fs::rename(twin.run_root(), twin.side(DERIVE)).unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        !text(&output.stderr).contains("bazel-rs-snapshot: derived "),
        "{}",
        text(&output.stderr)
    );
}
