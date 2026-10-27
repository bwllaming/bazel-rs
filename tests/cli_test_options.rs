//! Test and target-pattern CLI options of Bazel 9.3 (docs/TEST_COMMAND.md,
//! docs/RUNFILES_TREES.md): `--target_pattern_file` (TargetPatternsHelper),
//! `--notest_keep_going` (TestRunnerAction.executeAllAttempts: `Test failed,
//! aborting`), `--test_strategy` (the TestActionContext registry), per-label
//! `--runs_per_test`, and `--run_under` labels and commands for `test`,
//! `run` and the test's runfiles tree. The fixture constants are shared
//! verbatim with the paired genuine 9.3.0 capture
//! (/home/coder/opencodetmp/w/perf-research/cli-test-options-capture.py
//! extracts them; its receipt is
//! tests/fixtures/cli_test_options/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const MODULE: &str = r#"module(name = "tc")
"#;

const DEFS: &str = r##"def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

sh_like_test = rule(
    implementation = _exe,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)

sh_like_binary = rule(
    implementation = _exe,
    executable = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)
"##;

const BUILD: &str = r##"load("//:defs.bzl", "sh_like_binary", "sh_like_test")

genrule(name = "a", outs = ["a.txt"], cmd = "echo a > $@")
genrule(name = "b", outs = ["b.txt"], cmd = "echo b > $@")
genrule(name = "c", outs = ["c.txt"], cmd = "echo c > $@")

sh_like_test(name = "pass_a_test", script = "#!/bin/bash\necho pass a\n")
sh_like_test(name = "fail_test", script = "#!/bin/bash\necho failing\nexit 1\n")
sh_like_test(name = "pass_b_test", script = "#!/bin/bash\necho pass b\n")
sh_like_test(name = "slow_test", script = "#!/bin/bash\nsleep 3\necho slow\n")
sh_like_test(name = "alone_test", script = "#!/bin/bash\necho alone\n", tags = ["exclusive"])
sh_like_test(name = "args_test", script = "#!/bin/bash\necho \"args=$*\"\n", data = ["data.txt"])

sh_like_binary(
    name = "runner",
    script = "#!/bin/bash\necho \"runner argv0=${0##*/} args=$*\"\nd=\"${RUNFILES_DIR:-$0.runfiles}\"\nif [ -f \"$d/_main/p/runner_data.txt\" ]; then echo \"runner data: $(cat \"$d/_main/p/runner_data.txt\")\"; else echo \"runner data missing\"; fi\nexec \"$@\"\n",
    data = ["runner_data.txt"],
)

sh_like_binary(name = "hello", script = "#!/bin/bash\necho \"hello args=$*\"\n", data = ["data.txt"])
"##;

const PATTERNS: &str = "//p:a\n# comment\n  //p:b   # trailing\n\n-//p:b\n//p:c\n";

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cli-test-options-")
            .tempdir()
            .unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", ""),
            ("defs.bzl", DEFS),
            ("p/BUILD", BUILD),
            ("p/data.txt", "data\n"),
            ("p/runner_data.txt", "runner-data\n"),
            ("pats.txt", PATTERNS),
            ("empty.txt", "# nothing\n\n"),
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
        Workspace { _dir: dir, root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(with_explicit_modules(args))
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The summary lines (`LABEL ... STATUS`) without timings, and the
/// `Executed` line.
fn summary(output: &Output) -> Vec<String> {
    stdout(output)
        .lines()
        .filter(|line| line.starts_with("//") || line.starts_with("Executed "))
        .map(|line| {
            let line = line.split(" in ").next().unwrap_or(line);
            line.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .collect()
}

/// The built `label<TAB>path` lines' labels (stdout of `build`).
fn built(output: &Output) -> Vec<String> {
    stdout(output)
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(label, _)| label.to_owned()))
        .collect()
}

#[test]
fn target_pattern_file_replaces_the_residue() {
    let ws = Workspace::new();
    let output = ws.run(&["build", "--target_pattern_file=pats.txt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(built(&output), ["//p:a", "//p:c"]);
    let output = ws.run(&["build", "--target_pattern_file=pats.txt", "//p:a"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains(
        "ERROR: Command-line target pattern and --target_pattern_file cannot both be specified"
    ));
    let output = ws.run(&["build", "--target_pattern_file=missing.txt"]);
    assert_eq!(output.status.code(), Some(2));
    let missing = ws.path().join("missing.txt");
    assert!(
        stderr(&output).contains(&format!(
            "ERROR: I/O error reading from {}: {} (No such file or directory)",
            missing.display(),
            missing.display()
        )),
        "{}",
        stderr(&output)
    );
    // No patterns: BuildTool's usage warning, nothing built, success.
    let output = ws.run(&["build", "--target_pattern_file=empty.txt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains(
        "Your request is correct, but requested an empty set of targets. Nothing will be built."
    ));
    let output = ws.run(&["test", "--target_pattern_file=empty.txt"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(
        stderr(&output).contains("ERROR: No test targets were found, yet testing was requested")
    );
    // `run` reads the residue only; `query` has no such option.
    let output = ws.run(&["run", "--target_pattern_file=pats.txt"]);
    assert!(stderr(&output).contains("Must specify a target to run"));
    let output = ws.run(&["query", "--target_pattern_file=pats.txt", "//p:a"]);
    assert!(
        stderr(&output).contains(
            "--target_pattern_file=pats.txt :: Unrecognized option: --target_pattern_file=pats.txt"
        ),
        "{}",
        stderr(&output)
    );
}

#[test]
fn test_strategy_must_name_a_test_action_context() {
    let ws = Workspace::new();
    for strategy in ["local", "sandboxed"] {
        let output = ws.run(&[
            "test",
            &format!("--test_strategy={strategy}"),
            "//p:pass_a_test",
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains(&format!(
            "ERROR: No context of type TestActionContext registered for requested value '{strategy}', available identifiers are: [exclusive, standalone]\nunused [interface com.google.devtools.build.lib.analysis.test.TestActionContext]\nERROR: Build did NOT complete successfully\nERROR: Couldn't start the build. Unable to run tests\n"
        )), "{}", stderr(&output));
    }
    let output = ws.run(&["test", "--test_strategy=standalone", "//p:pass_a_test"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn notest_keep_going_stops_at_the_first_failure() {
    let ws = Workspace::new();
    let output = ws.run(&[
        "test",
        "--notest_keep_going",
        "//p:fail_test",
        "//p:slow_test",
    ]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(
        summary(&output),
        [
            "//p:slow_test NO STATUS",
            "//p:fail_test FAILED",
            "Executed 1 out of 2 tests: 1 fails locally and 1 was skipped."
        ]
    );
    let text = stderr(&output);
    assert!(
        text.contains(": Testing //p:fail_test failed: Test failed, aborting\n"),
        "{text}"
    );
    assert!(text.contains("\nERROR: Test failed, aborting\n"), "{text}");
    // Exclusive tests do not run after it either.
    let output = ws.run(&[
        "test",
        "--notest_keep_going",
        "//p:fail_test",
        "//p:alone_test",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        summary(&output),
        [
            "//p:alone_test NO STATUS",
            "//p:fail_test FAILED",
            "Executed 1 out of 2 tests: 1 fails locally and 1 was skipped."
        ]
    );
    // Under --keep_going the other tests still run; the failed one stays
    // FAILED (its result was posted before the exception).
    let output = ws.run(&[
        "test",
        "--notest_keep_going",
        "--keep_going",
        "//p:fail_test",
        "//p:slow_test",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        summary(&output),
        [
            "//p:slow_test PASSED",
            "//p:fail_test FAILED",
            "Executed 2 out of 2 tests: 1 test passes and 1 fails locally."
        ]
    );
    assert!(stderr(&output).contains("Testing //p:fail_test failed: Test failed, aborting"));
    assert!(!stderr(&output).contains("\nERROR: Test failed, aborting\n"));
    // Passing tests are untouched.
    let output = ws.run(&["test", "--notest_keep_going", "//p:pass_a_test"]);
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn per_label_runs_per_test() {
    let ws = Workspace::new();
    let output = ws.run(&[
        "test",
        "--runs_per_test=pass_a,-pass_b@2",
        "//p:pass_a_test",
        "//p:pass_b_test",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert_eq!(text.matches("Stats over 2 runs").count(), 1, "{text}");
    assert!(
        text.contains("Executed 2 out of 2 tests: 2 tests pass."),
        "{text}"
    );
}

#[test]
fn run_under_prefixes_tests_and_run() {
    let ws = Workspace::new();
    // A label: the runner is a (test exec group) prerequisite of the test,
    // its runfiles join the test's tree and its runfiles path prefixes the
    // test command after test-setup.sh.
    let output = ws.run(&[
        "test",
        "--run_under=//p:runner",
        "--test_output=all",
        "//p:args_test",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(
            "runner argv0=runner.sh args=p/args_test.sh\nrunner data: runner-data\nargs=\n"
        ),
        "{text}"
    );
    // A command without `/` runs through the shell; one with `/` directly.
    let output = ws.run(&[
        "test",
        "--run_under=echo pre",
        "--test_output=all",
        "//p:args_test",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("\npre p/args_test.sh\n"),
        "{}",
        stdout(&output)
    );
    let output = ws.run(&[
        "test",
        "--run_under=eval 'echo shell=$0'",
        "--test_output=all",
        "//p:args_test",
    ]);
    assert!(
        stdout(&output).contains("\nshell=/bin/bash p/args_test.sh\n"),
        "{}",
        stdout(&output)
    );
    // `run` of a test: test-setup.sh, the runner, the test.
    let output = ws.run(&["run", "--run_under=//p:runner", "//p:args_test", "--", "r1"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(
            "runner argv0=runner.sh args=p/args_test.sh r1\nrunner data: runner-data\nargs=r1\n"
        ),
        "{}",
        stdout(&output)
    );
    assert!(stderr(&output).contains(
        "Running command line: external/bazel_tools/tools/test/test-setup.sh p/runner.sh p/args_test.sh <args omitted>"
    ));
    // `run` of a binary: the runner's absolute executable path.
    let output = ws.run(&["run", "--run_under=//p:runner", "//p:hello", "--", "x", "y"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("runner argv0=runner.sh args=/")
            && text.ends_with("/bin/p/hello.sh x y\nrunner data: runner-data\nhello args=x y\n"),
        "{text}"
    );
    // A non-executable runner fails the run and the test's analysis.
    let output = ws.run(&["run", "--run_under=//p:a", "//p:hello"]);
    assert!(stderr(&output).contains("Cannot run target //p:a: Not executable"));
    let output = ws.run(&["test", "--run_under=//p:a", "//p:args_test"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("run_under target //p:a is not executable"),
        "{}",
        stderr(&output)
    );
    // The test's runfiles tree (also under `build`) holds the runner's.
    let output = ws.run(&["build", "--run_under=//p:runner", "//p:args_test"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let bin = ws.run(&["info", "bazel-bin"]);
    let bin = PathBuf::from(stdout(&bin).trim());
    let manifest = fs::read_to_string(bin.join("p/args_test.sh.runfiles/MANIFEST")).unwrap();
    let paths: Vec<&str> = manifest
        .lines()
        .filter_map(|line| line.split(' ').next())
        .collect();
    assert_eq!(
        paths,
        [
            "_main/p/args_test.sh",
            "_main/p/data.txt",
            "_main/p/runner.sh",
            "_main/p/runner_data.txt",
            "_repo_mapping"
        ]
    );
}
