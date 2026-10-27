//! `--keep_going`/`-k` of `build`, `test`, `run` and `query`
//! (docs/KEEP_GOING.md): a loading error (a package whose BUILD file fails),
//! an analysis error (a rule implementation that fails), an action failure
//! and a test failure in one invocation, with and without `-k`.
//!
//! The fixture constants are shared verbatim with the paired genuine Bazel
//! 9.3.0 capture (/tmp/opencode/perf-research/keep-going-capture.py extracts
//! them); its receipt is tests/fixtures/keep_going/reference_receipt.json.
//! Each scenario's expectation below is the genuine run's: exit code, the
//! key stderr lines (keep-going summary lines, compared as a sorted set:
//! Bazel prints the analysis warnings in its error map's hash order), the
//! test summary lines and which requested targets were built.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const MODULE: &str = r#"module(name = "kg")
"#;

const DEFS: &str = r##"def _fails(ctx):
    fail("intentional analysis failure")

fails_rule = rule(implementation = _fails)

def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

sh_like_test = rule(
    implementation = _t,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)
"##;

/// A package that fails to load before it declares any target.
const LOADERR_BUILD: &str = r##"fail("intentional loading failure")

genrule(name = "x", outs = ["x.txt"], cmd = "echo x > $@")
"##;

/// An analysis failure, a target that depends on it, an independent one.
const ANAERR_BUILD: &str = r##"load("//:defs.bzl", "fails_rule")

fails_rule(name = "bad", visibility = ["//visibility:public"])

genrule(name = "good", outs = ["good.txt"], cmd = "echo good > $@")

genrule(name = "uses_bad", srcs = [":bad"], outs = ["uses_bad.txt"], cmd = "cat $(SRCS) > $@")
"##;

/// An action failure, an action that depends on it, an independent one.
const ACTERR_BUILD: &str = r##"genrule(
    name = "broken",
    outs = ["broken.txt"],
    cmd = "echo intentional-action-failure >&2; exit 1",
    visibility = ["//visibility:public"],
)

genrule(name = "after_broken", srcs = [":broken"], outs = ["after.txt"], cmd = "cat $< > $@")

genrule(name = "independent", outs = ["independent.txt"], cmd = "sleep 1; echo ok > $@")
"##;

/// A passing test, a failing test, a test whose input fails to build and a
/// test whose dependency fails to analyze.
const TESTS_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test")

sh_like_test(name = "passes_test", script = "#!/bin/bash\necho pass\n")

sh_like_test(name = "fails_test", script = "#!/bin/bash\necho fail\nexit 1\n")

sh_like_test(name = "needs_broken_test", script = "#!/bin/bash\necho never\n", data = ["//acterr:broken"])

sh_like_test(name = "bad_dep_test", script = "#!/bin/bash\necho never\n", data = ["//anaerr:bad"])
"##;

/// Missing source files: of a test's runfiles, of a genrule's inputs.
const MI_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test")

sh_like_test(name = "missing_test", script = "#!/bin/bash\necho m\n", data = [":missing.txt"])

genrule(name = "gm", srcs = ["missing2.txt"], outs = ["gm.txt"], cmd = "cat $< > $@")

genrule(name = "ok", outs = ["ok.txt"], cmd = "echo > $@")
"##;

/// The scenarios (name, command and arguments), in the capture's order.
const SCENARIOS: &[(&str, &[&str])] = &[
    ("build_keep_going", &["build", "-k", "//..."]),
    ("build_no_keep_going", &["build", "//..."]),
    (
        "build_keep_going_no_loading_error",
        &[
            "build",
            "--keep_going",
            "//acterr:all",
            "//anaerr:all",
            "//tests:all",
        ],
    ),
    (
        "build_no_keep_going_no_loading_error",
        &["build", "//acterr:all", "//anaerr:all", "//tests:all"],
    ),
    ("test_keep_going", &["test", "-k", "//..."]),
    ("test_no_keep_going", &["test", "//..."]),
    (
        "test_keep_going_no_loading_error",
        &["test", "-k", "//acterr:all", "//tests:all"],
    ),
    (
        "build_keep_going_skipped_patterns",
        &[
            "build",
            "-k",
            "//nope:foo",
            "//acterr:nonexist",
            "//acterr:independent",
        ],
    ),
    (
        "build_keep_going_analysis_only",
        &["build", "-k", "//anaerr:all"],
    ),
    (
        "run_keep_going_analysis",
        &["run", "-k", "//anaerr:uses_bad"],
    ),
    (
        "run_keep_going_action",
        &["run", "-k", "//acterr:after_broken"],
    ),
    ("query_keep_going", &["query", "-k", "//..."]),
    (
        "test_keep_going_missing_inputs",
        &["test", "-k", "//mi:all", "//tests:passes_test"],
    ),
];

/// One scenario's genuine Bazel 9.3.0 result (the receipt's row).
struct Expected {
    exit: i32,
    /// The key stderr lines ([`key_stderr`]), sorted.
    stderr: &'static [&'static str],
    /// The test summary lines ([`summary`]), sorted.
    summary: &'static [&'static str],
    /// The requested targets reported built, sorted.
    built: &'static [&'static str],
}

const BAD: &str =
    "WARNING: errors encountered while analyzing target '//anaerr:bad', it will not be built.";
const USES_BAD: &str = "WARNING: errors encountered while analyzing target '//anaerr:uses_bad', it will not be built. / Analysis failed";
const BAD_DEP_TEST: &str = "WARNING: errors encountered while analyzing target '//tests:bad_dep_test', it will not be built. / Analysis failed";

fn expected(name: &str) -> Expected {
    match name {
        "build_keep_going" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "INFO: Build succeeded for only 5 of 13 top-level targets",
                "WARNING: Target pattern parsing failed.",
                BAD,
                USES_BAD,
                BAD_DEP_TEST,
            ],
            summary: &[],
            built: &[
                "//acterr:independent",
                "//anaerr:good",
                "//mi:ok",
                "//tests:fails_test",
                "//tests:passes_test",
            ],
        },
        "build_keep_going_no_loading_error" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "INFO: Build succeeded for only 4 of 10 top-level targets",
                BAD,
                USES_BAD,
                BAD_DEP_TEST,
            ],
            summary: &[],
            built: &[
                "//acterr:independent",
                "//anaerr:good",
                "//tests:fails_test",
                "//tests:passes_test",
            ],
        },
        "build_no_keep_going" | "build_no_keep_going_no_loading_error" | "test_no_keep_going" => {
            Expected {
                exit: 1,
                stderr: &[],
                summary: &[],
                built: &[],
            }
        }
        "test_keep_going" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "FAIL: //tests:fails_test",
                "INFO: Build succeeded for only 5 of 13 top-level targets",
                "WARNING: Target pattern parsing failed.",
                BAD,
                USES_BAD,
                BAD_DEP_TEST,
            ],
            summary: &[
                "//mi:missing_test FAILED TO BUILD",
                "//tests:fails_test FAILED in T",
                "//tests:needs_broken_test FAILED TO BUILD",
                "//tests:passes_test PASSED in T",
                "Executed 2 out of 4 tests: 1 test passes, 2 fail to build, and 1 fails locally.",
            ],
            built: &[
                "//acterr:independent",
                "//anaerr:good",
                "//mi:ok",
                "//tests:fails_test",
                "//tests:passes_test",
            ],
        },
        "test_keep_going_no_loading_error" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "FAIL: //tests:fails_test",
                "INFO: Build succeeded for only 3 of 7 top-level targets",
                BAD_DEP_TEST,
            ],
            summary: &[
                "//tests:fails_test FAILED in T",
                "//tests:needs_broken_test FAILED TO BUILD",
                "//tests:passes_test PASSED in T",
                "Executed 2 out of 3 tests: 1 test passes, 1 fails to build, and 1 fails locally.",
            ],
            built: &[
                "//acterr:independent",
                "//tests:fails_test",
                "//tests:passes_test",
            ],
        },
        "build_keep_going_skipped_patterns" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "ERROR: Skipping '//acterr:nonexist': no such target '//acterr:nonexist': target 'nonexist' not declared in package 'acterr' defined by <WS>/acterr/BUILD",
                "ERROR: Skipping '//nope:foo': no such package 'nope': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.",
                "ERROR: command succeeded, but there were errors parsing the target pattern",
                "WARNING: Target pattern parsing failed.",
            ],
            summary: &[],
            built: &["//acterr:independent"],
        },
        "build_keep_going_analysis_only" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "ERROR: command succeeded, but not all targets were analyzed",
                "INFO: Build succeeded for only 1 of 3 top-level targets",
                BAD,
                USES_BAD,
            ],
            summary: &[],
            built: &["//anaerr:good"],
        },
        "run_keep_going_analysis" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "ERROR: Build failed. Not running target",
                "ERROR: command succeeded, but not all targets were analyzed",
                "WARNING: Cannot run target //anaerr:uses_bad: Not executable. Will continue anyway",
                USES_BAD,
            ],
            summary: &[],
            built: &[],
        },
        "run_keep_going_action" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "ERROR: Build failed. Not running target",
                "WARNING: Cannot run target //acterr:after_broken: Not executable. Will continue anyway",
            ],
            summary: &[],
            built: &[],
        },
        "query_keep_going" => Expected {
            exit: 3,
            stderr: &[
                "WARNING: --keep_going specified, ignoring errors. Results may be inaccurate",
            ],
            summary: &[],
            built: &[],
        },
        "test_keep_going_missing_inputs" => Expected {
            exit: 1,
            stderr: &[
                "ERROR: Build did NOT complete successfully",
                "INFO: Build succeeded for only 2 of 4 top-level targets",
            ],
            summary: &[
                "//mi:missing_test FAILED TO BUILD",
                "//tests:passes_test PASSED in T",
                "Executed 1 out of 2 tests: 1 test passes and 1 fails to build.",
            ],
            built: &["//mi:ok", "//tests:passes_test"],
        },
        other => panic!("unknown scenario {other}"),
    }
}

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    /// Published test outputs are read-only; make the tree writable so the
    /// temporary directory goes.
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.0.path())
            .status();
    }
}

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("keep-going-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", ""),
            ("defs.bzl", DEFS),
            ("loaderr/BUILD", LOADERR_BUILD),
            ("anaerr/BUILD", ANAERR_BUILD),
            ("acterr/BUILD", ACTERR_BUILD),
            ("tests/BUILD", TESTS_BUILD),
            ("mi/BUILD", MI_BUILD),
        ] {
            let path = ws.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// The scenario's command (the explicit-module profile: the fixture
    /// needs no `@bazel_tools`), with `--show_result=100` like the capture.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = args.to_vec();
        if command[0] != "query" {
            command.insert(1, "--show_result=100");
        }
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(with_explicit_modules(&command))
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

/// The keep-going summary lines of stderr, normalized like the capture:
/// the analysis warnings with their `Analysis failed` continuation joined
/// by ` / `, a `FAIL:` line cut after the label, the workspace path
/// replaced by `<WS>`; the `Skipping` lines' first line only.
fn key_stderr(text: &str, ws: &Path) -> Vec<String> {
    let text = text.replace(&ws.display().to_string(), "<WS>");
    let lines: Vec<&str> = text.lines().collect();
    let mut keys = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("WARNING: errors encountered while analyzing target ") {
            let mut key = (*line).to_owned();
            if lines.get(index + 1) == Some(&"Analysis failed") {
                key.push_str(" / Analysis failed");
            }
            keys.push(key);
        } else if let Some(rest) = line.strip_prefix("FAIL: ") {
            keys.push(format!(
                "FAIL: {}",
                rest.split(' ').next().unwrap_or_default()
            ));
        } else if line.starts_with("INFO: Build succeeded for only ")
            || line.starts_with("INFO: Analysis succeeded for only ")
            || line.starts_with("ERROR: Skipping '")
            || line.starts_with("ERROR: command succeeded, but ")
            || line.ends_with(". Will continue anyway")
            || matches!(
                *line,
                "ERROR: Build did NOT complete successfully"
                    | "ERROR: Build failed. Not running target"
                    | "WARNING: Target pattern parsing failed."
                    | "WARNING: --keep_going specified, ignoring errors. Results may be inaccurate"
            )
        {
            keys.push((*line).to_owned());
        }
    }
    keys.sort();
    keys
}

/// The test summary lines of stdout (status lines with runs of spaces
/// collapsed and durations replaced by `T`, the `Executed` line), sorted.
fn summary(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("Executed ") {
            lines.push(line.to_owned());
        } else if line.starts_with("//") && !line.contains('\t') {
            let mut words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
            if let Some(last) = words.last_mut()
                && last.ends_with('s')
                && last[..last.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.')
            {
                *last = "T".to_owned();
            }
            words.retain(|word| word != "(cached)");
            lines.push(words.join(" "));
        }
    }
    lines.sort();
    lines
}

/// The requested targets reported built: native `build` prints
/// `LABEL\tPATH` on stdout, `test`/`run` on stderr.
fn built(output: &Output) -> Vec<String> {
    let mut labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .filter(|line| line.starts_with("//") && line.contains('\t'))
        .map(|line| line.split('\t').next().unwrap().to_owned())
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

fn check(name: &str) {
    let ws = Workspace::new();
    let (_, args) = SCENARIOS
        .iter()
        .find(|(scenario, _)| *scenario == name)
        .unwrap();
    let output = ws.run(args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = expected(name);
    let context = format!("{name}: {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(output.status.code(), Some(expected.exit), "{context}");
    assert_eq!(key_stderr(&stderr, ws.path()), expected.stderr, "{context}");
    // `query` builds and tests nothing (its output lists targets).
    if args[0] != "query" {
        assert_eq!(summary(&stdout), expected.summary, "{context}");
        assert_eq!(built(&output), expected.built, "{context}");
    }
}

#[test]
fn build_keep_going_builds_everything_not_depending_on_a_failure() {
    check("build_keep_going");
    check("build_keep_going_no_loading_error");
}

#[test]
fn without_keep_going_the_first_failure_fails_the_build() {
    check("build_no_keep_going");
    check("build_no_keep_going_no_loading_error");
    check("test_no_keep_going");
}

#[test]
fn test_keep_going_runs_every_test_whose_inputs_built() {
    check("test_keep_going");
    check("test_keep_going_no_loading_error");
}

#[test]
fn keep_going_skips_failing_patterns_and_reports_analysis_only_failures() {
    check("build_keep_going_skipped_patterns");
    check("build_keep_going_analysis_only");
}

#[test]
fn keep_going_input_failures_fail_only_what_needs_them() {
    check("test_keep_going_missing_inputs");
}

#[test]
fn run_keep_going_builds_then_refuses_to_run() {
    check("run_keep_going_analysis");
    check("run_keep_going_action");
}

#[test]
fn query_keep_going_skips_packages_with_errors() {
    check("query_keep_going");
    let ws = Workspace::new();
    let output = ws.run(&["query", "-k", "//..."]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//anaerr:good\n"), "{stdout}");
    assert!(!stdout.contains("//loaderr:"), "{stdout}");
}

/// A failed keep-going build is never saved as an analysis snapshot: the
/// same command analyzes (and reports) again.
#[test]
fn a_failed_keep_going_build_is_never_a_snapshot_hit() {
    let ws = Workspace::new();
    let args = ["build", "-k", "//anaerr:all"];
    let first = ws.run(&args);
    let second = ws.run(&args);
    for output in [&first, &second] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("ERROR: analysis of //anaerr:bad: "),
            "{stderr}"
        );
    }
}
