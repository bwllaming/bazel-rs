//! PERF(TEST): `bazel-rs test` runs each parallel test action as soon as its
//! test's inputs are built, inside the build's execution graph, like Bazel
//! 9.3's TestRunnerActions (skyframe/BuildDriverFunction.java:589-642,
//! TestCompletionFunction); exclusive tests run one at a time after the
//! build (SkyframeBuildView.java:759-794); the summary is printed after
//! everything, in its fixed order (docs/TEST_COMMAND.md, "Scheduling").
//!
//! Every scenario is deterministic: the tests and build actions synchronize
//! through marker files below `$MARKER_DIR` (`--action_env`, which reaches
//! genrules and tests alike), not through timing. The fixture constants are
//! shared verbatim with the paired genuine Bazel 9.3.0 capture
//! (/tmp/opencode/perf-research/test-overlap-capture.py extracts them; its
//! receipt is tests/fixtures/test_overlap/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
include!("support/explicit_modules.rs");

const MODULE: &str = r#"module(name = "ov")
"#;

const DEFS: &str = r##"def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

sh_like_test = rule(
    implementation = _t,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)

def _validated(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/bash\necho ran\n", is_executable = True)
    check = ctx.actions.declare_file(ctx.label.name + ".validation")
    ctx.actions.run_shell(
        outputs = [check],
        command = "sleep 2; echo validation-failed >&2; exit 1",
        mnemonic = "Validate",
        progress_message = "Validating %{label}",
    )
    return [
        DefaultInfo(executable = out),
        OutputGroupInfo(_validation = depset([check])),
    ]

validated_test = rule(implementation = _validated, test = True)
"##;

const OV_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test", "validated_test")

# O1: a build action that can only finish after a test of the same build ran.
genrule(
    name = "waits_for_test",
    outs = ["waited.txt"],
    cmd = "for i in $$(seq 1 600); do test -e \"$$MARKER_DIR/tested\" && break; sleep 0.1; done; test -e \"$$MARKER_DIR/tested\" && echo waited > $@",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "marks_test",
    script = "#!/bin/bash\ntouch \"$MARKER_DIR/tested\"\necho marked\n",
    tags = ["no-sandbox"],
)

# O2: a build action fails once one test passed and another one runs.
genrule(
    name = "fails_after_test",
    outs = ["never.txt"],
    cmd = "for i in $$(seq 1 600); do test -e \"$$MARKER_DIR/passed\" && test -e \"$$MARKER_DIR/sleeping\" && break; sleep 0.1; done; sleep 2; echo intentional-failure >&2; exit 1",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "passes_test",
    script = "#!/bin/bash\ntouch \"$MARKER_DIR/passed\"\necho passing\n",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "sleeps_test",
    script = "#!/bin/bash\ntest -e \"$MARKER_DIR/fast\" && exit 0\ntouch \"$MARKER_DIR/sleeping\"\nsleep 120\n",
    tags = ["no-sandbox"],
)

# O2b: every test passed before an unrelated build action failed.
genrule(
    name = "fails_after_pass",
    outs = ["never_either.txt"],
    cmd = "for i in $$(seq 1 600); do test -e \"$$MARKER_DIR/passed\" && break; sleep 0.1; done; sleep 2; echo intentional-failure >&2; exit 1",
    tags = ["no-sandbox"],
)

# O2c: a build action fails while one shard of a test passed and another
# one runs.
genrule(
    name = "fails_after_shards",
    outs = ["never_sharded.txt"],
    cmd = "for i in $$(seq 1 600); do test -e \"$$MARKER_DIR/shard0\" && test -e \"$$MARKER_DIR/shard1\" && break; sleep 0.1; done; sleep 2; echo intentional-failure >&2; exit 1",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "sharded_sleeps_test",
    script = "#!/bin/bash\ntouch \"$TEST_SHARD_STATUS_FILE\"\ntouch \"$MARKER_DIR/shard$TEST_SHARD_INDEX\"\nif [ \"$TEST_SHARD_INDEX\" = 1 ]; then sleep 120; fi\n",
    shard_count = 2,
    tags = ["no-sandbox"],
)

# O3: exclusive tests run after the parallel ones, one at a time.
sh_like_test(
    name = "first_test",
    script = "#!/bin/bash\nsleep 1\ntouch \"$MARKER_DIR/first\"\necho first\n",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "second_test",
    script = "#!/bin/bash\nsleep 1\ntouch \"$MARKER_DIR/second\"\necho second\n",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "alone_test",
    script = "#!/bin/bash\ntest -e \"$MARKER_DIR/first\" && test -e \"$MARKER_DIR/second\" && mkdir \"$MARKER_DIR/alone\" && sleep 1 && rmdir \"$MARKER_DIR/alone\" && echo alone\n",
    tags = ["exclusive", "no-sandbox"],
)

sh_like_test(
    name = "alone_if_local_test",
    script = "#!/bin/bash\ntest -e \"$MARKER_DIR/first\" && test -e \"$MARKER_DIR/second\" && mkdir \"$MARKER_DIR/alone\" && sleep 1 && rmdir \"$MARKER_DIR/alone\" && echo alone if local\n",
    tags = ["exclusive-if-local", "no-sandbox"],
)

# O4: --local_test_jobs bounds the tests running at once.
sh_like_test(
    name = "lock_a_test",
    script = "#!/bin/bash\nmkdir \"$MARKER_DIR/lock\" || exit 1\nsleep 1\nrmdir \"$MARKER_DIR/lock\"\n",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "lock_b_test",
    script = "#!/bin/bash\nmkdir \"$MARKER_DIR/lock\" || exit 1\nsleep 1\nrmdir \"$MARKER_DIR/lock\"\n",
    tags = ["no-sandbox"],
)

# O5: streamed output runs the tests one at a time after the build.
sh_like_test(
    name = "stream_a_test",
    script = "#!/bin/bash\necho stream a\nmkdir \"$MARKER_DIR/streaming\" || exit 1\nsleep 1\nrmdir \"$MARKER_DIR/streaming\"\necho stream a done\n",
    tags = ["no-sandbox"],
)

sh_like_test(
    name = "stream_b_test",
    script = "#!/bin/bash\necho stream b\nmkdir \"$MARKER_DIR/streaming\" || exit 1\nsleep 1\nrmdir \"$MARKER_DIR/streaming\"\necho stream b done\n",
    tags = ["no-sandbox"],
)

# O6: a test runs only once its target is complete (validation included).
validated_test(name = "validated_test")

# A sandboxed test (the default strategy).
sh_like_test(
    name = "plain_test",
    script = "#!/bin/bash\necho plain\n",
)
"##;

/// The scenarios (name, `test` arguments), in the capture's order.
const SCENARIOS: &[(&str, &[&str])] = &[
    ("overlap", &["//ov:waits_for_test", "//ov:marks_test"]),
    (
        "build_failure_while_testing",
        &[
            "//ov:fails_after_test",
            "//ov:passes_test",
            "//ov:sleeps_test",
        ],
    ),
    (
        "build_failure_after_all_passed",
        &["//ov:fails_after_pass", "//ov:passes_test"],
    ),
    (
        "build_failure_while_a_shard_runs",
        &["//ov:fails_after_shards", "//ov:sharded_sleeps_test"],
    ),
    (
        "exclusive_after_parallel",
        &[
            "--test_output=all",
            "//ov:first_test",
            "//ov:second_test",
            "//ov:alone_test",
            "//ov:alone_if_local_test",
        ],
    ),
    (
        "local_test_jobs",
        &[
            "--local_test_jobs=1",
            "//ov:lock_a_test",
            "//ov:lock_b_test",
        ],
    ),
    (
        "streamed",
        &[
            "--test_output=streamed",
            "//ov:stream_a_test",
            "//ov:stream_b_test",
        ],
    ),
    ("validation_failure", &["//ov:validated_test"]),
];

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
                .prefix("test-overlap-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", ""),
            ("defs.bzl", DEFS),
            ("ov/BUILD", OV_BUILD),
        ] {
            let path = ws.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::create_dir_all(ws.markers()).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn markers(&self) -> std::path::PathBuf {
        self.path().join("markers")
    }

    /// `bazel-rs test` with `$MARKER_DIR` set for every action (the
    /// explicit-module profile: the fixture needs no `@bazel_tools`).
    fn test(&self, args: &[&str]) -> Output {
        let marker_dir = format!("--action_env=MARKER_DIR={}", self.markers().display());
        let mut command = vec!["test", marker_dir.as_str()];
        command.extend_from_slice(args);
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

    fn scenario(&self, name: &str) -> Output {
        let (_, args) = SCENARIOS
            .iter()
            .find(|(scenario, _)| *scenario == name)
            .unwrap();
        self.test(args)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Durations vary: `in 0.1s` becomes `in T`; the size note is dropped.
fn timeless(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("There were tests whose specified size is too big.") {
            continue;
        }
        let line = match line.rfind(" in ") {
            Some(index)
                if line[index + 4..].ends_with('s')
                    && line[index + 4..line.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.') =>
            {
                format!("{} in T", &line[..index])
            }
            _ => line.to_owned(),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The summary: stdout after the last test output block.
fn summary(output: &Output) -> String {
    let text = stdout(output);
    let start = text
        .rfind("================================================================================\n")
        .map(|index| index + 81)
        .unwrap_or(0);
    timeless(&text[start..])
}

/// A build action that waits for a test of the same build finishes: the
/// test ran while the build was still running (the old schedule ran tests
/// only after the whole build, so this build would fail after 60 s).
#[test]
fn a_test_runs_while_the_rest_of_the_build_is_still_running() {
    let ws = Workspace::new();
    let output = ws.scenario("overlap");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        summary(&output),
        "//ov:marks_test                                                          PASSED in T\n\nExecuted 1 out of 1 test: 1 test passes.\n"
    );
    // The build summary still comes after the tests that ran inside it.
    let text = stderr(&output);
    assert!(
        text.contains("//ov:waits_for_test\t") && text.contains(" actions executed, 0 cache hits"),
        "{text}"
    );
    // Cached on the next run, like any test result.
    let again = ws.scenario("overlap");
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        summary(&again).contains("(cached) PASSED"),
        "{}",
        summary(&again)
    );
}

/// --nokeep_going: a build action fails while one test has passed and
/// another one runs. The passed test keeps its result, the running one is
/// interrupted (no result, NO STATUS, nothing cached), exclusive tests do not
/// run, and the command fails with exit 1 without waiting for the
/// interrupted test (AbstractQueueVisitor interrupts in-flight actions;
/// AggregatingTestListener keeps posted summaries).
#[test]
fn a_build_failure_interrupts_running_tests_and_keeps_finished_results() {
    let ws = Workspace::new();
    let start = Instant::now();
    let output = ws.scenario("build_failure_while_testing");
    let elapsed = start.elapsed();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(elapsed < Duration::from_secs(100), "{elapsed:?}");
    assert_eq!(
        summary(&output),
        "//ov:sleeps_test                                                      NO STATUS\n//ov:passes_test                                                         PASSED in T\n\nExecuted 1 out of 2 tests: 1 test passes and 1 was skipped.\n"
    );
    assert!(
        stderr(&output).contains("intentional-failure"),
        "{}",
        stderr(&output)
    );
    // The interrupted test left nothing behind: no process, no result.
    let leftover = Command::new("pgrep")
        .args(["-f", "^sleep 120$"])
        .output()
        .unwrap();
    let pids = String::from_utf8_lossy(&leftover.stdout).into_owned();
    for pid in pids.split_whitespace() {
        let environ = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&environ).contains(&ws.markers().display().to_string()),
            "the interrupted test's sleep {pid} is still running"
        );
    }
    // Without the failing action the interrupted test runs again (nothing
    // was cached; the `fast` marker makes it pass at once) and the passed
    // one is cached.
    fs::write(ws.markers().join("fast"), "").unwrap();
    let rerun = ws.test(&["//ov:passes_test", "//ov:sleeps_test"]);
    assert_eq!(rerun.status.code(), Some(0), "{}", stderr(&rerun));
    assert_eq!(
        summary(&rerun),
        "//ov:passes_test                                                (cached) PASSED in T\n//ov:sleeps_test                                                         PASSED in T\n\nExecuted 1 out of 2 tests: 2 tests pass.\n"
    );
}

/// Every test passed before an unrelated build action failed: the summary
/// says so (TestCommand.java:216-223) and the command fails with exit 1.
#[test]
fn passed_tests_of_a_failed_build_say_so() {
    let ws = Workspace::new();
    let output = ws.scenario("build_failure_after_all_passed");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        timeless(&stdout(&output)),
        "//ov:passes_test                                                         PASSED in T\n\nExecuted 1 out of 1 test: 1 test passes.\nAll tests passed but there were other errors during the build.\n\n"
    );
}

/// A test with a finished shard and an interrupted one has NO STATUS (its
/// summary was never posted: AggregatingTestListener marks it unbuilt), and
/// counts as executed (a shard ran).
#[test]
fn a_partly_run_sharded_test_of_a_failed_build_has_no_status() {
    let ws = Workspace::new();
    let output = ws.scenario("build_failure_while_a_shard_runs");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        summary(&output),
        "//ov:sharded_sleeps_test                                              NO STATUS\n\nExecuted 1 out of 1 test: 1 was skipped.\n"
    );
}

/// Exclusive tests (`exclusive`, `exclusive-if-local`) run after every
/// parallel test, one at a time; the output blocks come as the tests end,
/// the summary after them in its fixed order.
#[test]
fn exclusive_tests_run_after_the_parallel_ones_one_at_a_time() {
    let ws = Workspace::new();
    let output = ws.scenario("exclusive_after_parallel");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let position = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {text}"))
    };
    let alone = position("==================== Test output for //ov:alone_test:\nalone\n");
    let alone_if_local = position(
        "==================== Test output for //ov:alone_if_local_test:\nalone if local\n",
    );
    for parallel in [
        "==================== Test output for //ov:first_test:\nfirst\n",
        "==================== Test output for //ov:second_test:\nsecond\n",
    ] {
        assert!(position(parallel) < alone.min(alone_if_local), "{text}");
    }
    // Exclusive tests run in label order.
    assert!(alone_if_local < alone, "{text}");
    assert_eq!(
        summary(&output),
        "//ov:alone_if_local_test                                                 PASSED in T\n//ov:alone_test                                                          PASSED in T\n//ov:first_test                                                          PASSED in T\n//ov:second_test                                                         PASSED in T\n\nExecuted 4 out of 4 tests: 4 tests pass.\n"
    );
}

/// --local_test_jobs=1: the two tests never run at once (each holds a lock
/// directory while it runs).
#[test]
fn local_test_jobs_bounds_the_tests_running_at_once() {
    let ws = Workspace::new();
    let output = ws.scenario("local_test_jobs");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        summary(&output),
        "//ov:lock_a_test                                                         PASSED in T\n//ov:lock_b_test                                                         PASSED in T\n\nExecuted 2 out of 2 tests: 2 tests pass.\n"
    );
}

/// --test_output=streamed: the tests run one at a time after the build, in
/// label order, their logs streamed to stdout.
#[test]
fn streamed_output_runs_the_tests_one_at_a_time() {
    let ws = Workspace::new();
    let output = ws.scenario("streamed");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        timeless(&stdout(&output)),
        "stream a\nstream a done\nstream b\nstream b done\n//ov:stream_a_test                                                       PASSED in T\n//ov:stream_b_test                                                       PASSED in T\n\nExecuted 2 out of 2 tests: 2 tests pass.\n"
    );
    assert!(stderr(&output).contains(
        "WARNING: Streamed test output requested. All tests will be run without sharding, one at a time"
    ));
}

/// The build summary's execution note describes the build's actions only,
/// as when the tests ran after it was printed: a sandboxed test spawn of a
/// fully cached build leaves it "not sandboxed".
#[test]
fn the_build_summary_note_counts_build_actions_only() {
    let ws = Workspace::new();
    let first = ws.test(&["//ov:plain_test"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let again = ws.test(&["--nocache_test_results", "//ov:plain_test"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        summary(&again).ends_with("Executed 1 out of 1 test: 1 test passes.\n"),
        "{}",
        summary(&again)
    );
    assert!(
        stderr(&again).contains(
            "bazel-rs: 0 actions executed, 4 cache hits (experimental local execution; not sandboxed)"
        ),
        "{}",
        stderr(&again)
    );
}

/// A test runs only once its target is complete: its failing validation
/// action (`_validation`, built with the target) fails the build first, so
/// the cheap test never runs (TestCompletionFunction.java:55).
#[test]
fn a_test_waits_for_its_target_to_complete() {
    let ws = Workspace::new();
    let output = ws.scenario("validation_failure");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        summary(&output),
        "//ov:validated_test                                             FAILED TO BUILD\n\nExecuted 0 out of 1 test: 1 fails to build.\n"
    );
    assert!(!stdout(&output).contains("ran\n"));
}
