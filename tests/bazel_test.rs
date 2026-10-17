//! N23g CLI coverage: `bazel-rs test` (TestRunnerAction run by
//! StandaloneTestStrategy with the genuine test-setup.sh, the test
//! environment, testlogs, summaries, exit codes, caching, sharding, flaky
//! attempts, timeouts, test_suite expansion and filters) and `run` of test
//! targets (docs/TEST_COMMAND.md). The fixture constants are shared verbatim
//! with the paired capture (artifacts/bazel_test-capture.py extracts them).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MODULE: &str = r#"module(name = "bt")
"#;

const DEFS: &str = r##"def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [
        DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data)),
        RunEnvironmentInfo(environment = ctx.attr.env, inherited_environment = ctx.attr.inherit),
    ]

_ATTRS = {
    "script": attr.string(),
    "data": attr.label_list(allow_files = True),
    "env": attr.string_dict(),
    "inherit": attr.string_list(),
}

sh_like_test = rule(implementation = _t, test = True, attrs = _ATTRS)
other_lang_test = rule(implementation = _t, test = True, attrs = _ATTRS)
"##;

const PKG_BUILD: &str = r##"load("//:defs.bzl", "other_lang_test", "sh_like_test")

ENV_SCRIPT = """#!/bin/bash
echo "args: $*"
for v in TEST_TARGET TEST_SIZE TEST_TIMEOUT TEST_WORKSPACE TEST_BINARY TZ RUN_UNDER_RUNFILES BAZEL_TEST TESTBRIDGE_TEST_ONLY FOO FOO2 INHERIT_ME TEST_SHARD_INDEX TEST_TOTAL_SHARDS TEST_RANDOM_SEED TEST_RUN_NUMBER RUNFILES_MANIFEST_ONLY; do echo "$v=${!v-<unset>}"; done
for v in TEST_TMPDIR TEST_SRCDIR RUNFILES_DIR JAVA_RUNFILES PYTHON_RUNFILES XML_OUTPUT_FILE TEST_UNDECLARED_OUTPUTS_DIR TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR TEST_PREMATURE_EXIT_FILE TEST_INFRASTRUCTURE_FAILURE_FILE TEST_WARNINGS_OUTPUT_FILE TEST_LOGSPLITTER_OUTPUT_FILE TEST_UNUSED_RUNFILES_LOG_FILE HOME GTEST_TMP_DIR; do echo "$v=${!v-<unset>}"; done
echo "pwd=$PWD"
echo "names: $(env | cut -d= -f1 | grep -v '^_$' | sort | tr '\\n' ' ')"
cat pkg/data.txt
cat ../_repo_mapping
test -d "$TEST_TMPDIR" && echo tmpdir-exists
"""

sh_like_test(
    name = "pass_test",
    size = "small",
    data = ["data.txt"],
    env = {"FOO": "bar"},
    inherit = ["INHERIT_ME"],
    script = ENV_SCRIPT,
    tags = ["unit"],
)

sh_like_test(
    name = "fail_test",
    script = "#!/bin/bash\necho failing\necho to-stderr >&2\nexit 1\n",
)

sh_like_test(
    name = "xml_test",
    script = "#!/bin/bash\necho '<?xml version=\"1.0\"?><testsuites><testsuite name=\"s\"><testcase name=\"a\" classname=\"C\" time=\"0.5\"/><testcase name=\"b\" classname=\"C\" time=\"1.25\"><failure message=\"m\"/></testcase><testcase name=\"c\" classname=\"C\"><skipped/></testcase></testsuite></testsuites>' > \"$XML_OUTPUT_FILE\"\nexit 1\n",
)

sh_like_test(
    name = "sharded_test",
    script = "#!/bin/bash\ntouch \"$TEST_SHARD_STATUS_FILE\"\necho shard $TEST_SHARD_INDEX of $TEST_TOTAL_SHARDS\n",
    shard_count = 3,
)

sh_like_test(
    name = "noshard_test",
    script = "#!/bin/bash\necho no status file\n",
    shard_count = 2,
)

sh_like_test(
    name = "slow_test",
    script = "#!/bin/bash\necho sleeping\nsleep 30\n",
    timeout = "short",
)

sh_like_test(
    name = "flaky_test",
    flaky = True,
    script = "#!/bin/bash\nif [ -e \"$FLAKY_MARKER\" ]; then echo second; exit 0; fi\ntouch \"$FLAKY_MARKER\"\necho first\nexit 1\n",
)

sh_like_test(
    name = "always_flaky_test",
    flaky = True,
    script = "#!/bin/bash\necho never\nexit 2\n",
)

sh_like_test(
    name = "premature_test",
    script = "#!/bin/bash\ntouch \"$TEST_PREMATURE_EXIT_FILE\"\necho premature\n",
)

sh_like_test(
    name = "outputs_test",
    script = "#!/bin/bash\necho hello > \"$TEST_UNDECLARED_OUTPUTS_DIR/out.txt\"\necho annotation > \"$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR/x.part\"\n",
)

sh_like_test(
    name = "signal_test",
    script = "#!/bin/bash\necho crashing\nkill -SEGV $$\n",
)

sh_like_test(
    name = "manual_test",
    script = "#!/bin/bash\necho manual\n",
    tags = ["manual"],
)

sh_like_test(
    name = "exclusive_test",
    script = "#!/bin/bash\necho exclusive\n",
    tags = ["exclusive"],
)

sh_like_test(
    name = "external_test",
    script = "#!/bin/bash\necho external\n",
    tags = ["external"],
)

sh_like_test(
    name = "large_test",
    size = "large",
    script = "#!/bin/bash\necho large\n",
)

other_lang_test(
    name = "lang_test",
    script = "#!/bin/bash\necho lang\n",
)
"##;

const SUITE_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test")

sh_like_test(
    name = "a_test",
    script = "#!/bin/bash\necho a\n",
    tags = ["fast"],
)

sh_like_test(
    name = "b_test",
    size = "small",
    script = "#!/bin/bash\necho b\n",
)

sh_like_test(
    name = "c_test",
    script = "#!/bin/bash\necho c\n",
    tags = ["manual"],
)

test_suite(
    name = "suite",
    tests = [":a_test", ":inner"],
)

test_suite(
    name = "inner",
    tests = [":b_test", ":c_test"],
)

test_suite(
    name = "implicit",
)

test_suite(
    name = "small_only",
    tags = ["small"],
)
"##;

const NT_BUILD: &str = r##"genrule(
    name = "gen",
    outs = ["gen.txt"],
    cmd = "echo gen > $@",
)
"##;

const DATA: &str = "data file\n";

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    /// Published test outputs are read-only (test.outputs is 0555, like
    /// Bazel's); make the tree writable so the temporary directory goes.
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
                .prefix("bazel-test-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", ""),
            ("defs.bzl", DEFS),
            ("pkg/BUILD", PKG_BUILD),
            ("pkg/data.txt", DATA),
            ("suite/BUILD", SUITE_BUILD),
            ("nt/BUILD", NT_BUILD),
        ] {
            ws.file(name, text);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .env("INHERIT_ME", "inherited")
            .env("FLAKY_MARKER", self.path().join("flaky-marker"))
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

/// Durations vary: `in 0.0s` and stats lines become `in T`.
fn timeless(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let mut line = line.to_owned();
        let mut from = 0;
        while let Some(found) = line[from..].find(" in ") {
            let index = from + found;
            let rest = &line[index + 4..];
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            if end > 0 && rest[..end].contains('.') && rest[end..].starts_with('s') {
                line = format!("{} at T{}", &line[..index], &rest[end + 1..]);
            } else {
                from = index + 4;
            }
        }
        // Fast medium tests always get the size note.
        if line.starts_with("There were tests whose specified size is too big.") {
            continue;
        }
        if line.starts_with("  Stats over") {
            line = line.split(':').next().unwrap().to_owned();
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The summary lines (the part of stdout after the test output blocks).
fn summary(output: &Output) -> String {
    let text = stdout(output);
    let start = text
        .rfind("================================================================================\n")
        .map(|index| index + 81)
        .unwrap_or(0);
    timeless(&text[start..])
}

/// The test.log path a summary names for `label`.
fn logs(output: &Output) -> Vec<PathBuf> {
    stdout(output)
        .lines()
        .filter(|line| line.starts_with("  /"))
        .map(|line| PathBuf::from(line.trim()))
        .collect()
}

#[test]
fn passing_test_environment_logs_and_cache() {
    let ws = Workspace::new();
    let args = [
        "test",
        "--test_output=all",
        "--test_arg=x",
        "--test_arg=y z",
        "--test_env=FOO2=v",
        "--test_filter=flt",
        "//pkg:pass_test",
    ];
    let first = ws.run(&args);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let text = stdout(&first);
    for expected in [
        "==================== Test output for //pkg:pass_test:\nargs: x y z\n",
        "TEST_TARGET=//pkg:pass_test\n",
        "TEST_SIZE=small\nTEST_TIMEOUT=60\nTEST_WORKSPACE=_main\nTEST_BINARY=pkg/pass_test.sh\nTZ=UTC\nRUN_UNDER_RUNFILES=1\nBAZEL_TEST=1\nTESTBRIDGE_TEST_ONLY=flt\nFOO=bar\nFOO2=v\nINHERIT_ME=inherited\nTEST_SHARD_INDEX=<unset>\n",
        "data file\n,bt,_main\n",
        "tmpdir-exists\n",
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
    // HOME is the test tmpdir; the runfiles directory and the tmp dir are
    // below the action's execution root.
    let home = text
        .lines()
        .find_map(|line| line.strip_prefix("HOME="))
        .unwrap();
    let tmp = text
        .lines()
        .find_map(|line| line.strip_prefix("TEST_TMPDIR="))
        .unwrap();
    assert_eq!(home, tmp);
    assert!(
        text.contains("pwd=") && text.contains("/bin/pkg/pass_test.sh.runfiles/_main\n"),
        "{text}"
    );
    assert_eq!(
        summary(&first),
        "//pkg:pass_test                                                          PASSED at T\n\nExecuted 1 out of 1 test: 1 test passes.\n"
    );
    assert!(
        stderr(&first).contains("INFO: From Testing //pkg:pass_test:"),
        "{}",
        stderr(&first)
    );
    // The second run is cached and prints the cached log first
    // (TestSummaryPrinter.printCachedOutput).
    let second = ws.run(&args);
    assert_eq!(second.status.code(), Some(0));
    let text = stdout(&second);
    assert!(
        text.starts_with("PASSED: //pkg:pass_test (see /")
            && text.contains("/testlogs/pkg/pass_test/test.log)\nINFO: From Testing //pkg:pass_test\n==================== Test output for //pkg:pass_test:\n"),
        "{text}"
    );
    assert_eq!(
        summary(&second),
        "//pkg:pass_test                                                 (cached) PASSED at T\n\nExecuted 0 out of 1 test: 1 test passes.\n"
    );
    // Another test argument is another action.
    let third = ws.run(&["test", "--test_arg=other", "//pkg:pass_test"]);
    assert_eq!(
        summary(&third),
        "//pkg:pass_test                                                          PASSED at T\n\nExecuted 1 out of 1 test: 1 test passes.\n"
    );
    // A changed data file reruns the test (its runfiles are keyed by content).
    ws.file("pkg/data.txt", "changed\n");
    let fourth = ws.run(&["test", "--test_arg=other", "//pkg:pass_test"]);
    assert!(summary(&fourth).contains("Executed 1 out of 1 test"));
    // --nocache_test_results always runs.
    let fifth = ws.run(&[
        "test",
        "--nocache_test_results",
        "--test_arg=other",
        "//pkg:pass_test",
    ]);
    assert!(summary(&fifth).contains("Executed 1 out of 1 test"));
}

#[test]
fn failing_test_xml_exit_code_and_reruns() {
    let ws = Workspace::new();
    let output = ws.run(&["test", "--test_output=errors", "//pkg:fail_test"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with(
            "==================== Test output for //pkg:fail_test:\nfailing\nto-stderr\n================================================================================\n"
        ),
        "{text}"
    );
    assert!(summary(&output).starts_with(
        "//pkg:fail_test                                                          FAILED at T\n  /"
    ));
    assert!(summary(&output).ends_with("\nExecuted 1 out of 1 test: 1 fails locally.\n"));
    let log = &logs(&output)[0];
    assert!(log.ends_with("bazel-out/k8-fastbuild/testlogs/pkg/fail_test/test.log"));
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        "exec ${PAGER:-/usr/bin/less} \"$0\" || exit 1\nExecuting tests from //pkg:fail_test\n-----------------------------------------------------------------------------\nfailing\nto-stderr\n"
    );
    let xml = fs::read_to_string(log.with_file_name("test.xml")).unwrap();
    // The duration is the test's wall time in seconds (0 on an idle machine;
    // more under heavy load), equal in both attributes.
    let testcase = xml
        .lines()
        .find(|line| line.contains("<testcase "))
        .unwrap_or_else(|| panic!("{xml}"));
    let seconds = testcase
        .split("duration=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("{xml}"));
    assert!(seconds.parse::<u64>().is_ok(), "{xml}");
    assert_eq!(
        testcase.trim(),
        format!(
            "<testcase name=\"pkg/fail_test.sh\" classname=\"\" status=\"run\" duration=\"{seconds}\" time=\"{seconds}\"><error message=\"exited with error code 1\"></error></testcase>"
        ),
        "{xml}"
    );
    assert_eq!(
        fs::metadata(log).unwrap().permissions().mode() & 0o777,
        0o555
    );
    assert!(
        stderr(&output).contains("FAIL: //pkg:fail_test (Exit 1) (see /"),
        "{}",
        stderr(&output)
    );
    // A failed result is not reused under --cache_test_results=auto ...
    let again = ws.run(&["test", "//pkg:fail_test"]);
    assert!(summary(&again).contains("Executed 1 out of 1 test: 1 fails locally."));
    // ... but is under yes.
    let cached = ws.run(&["test", "--cache_test_results=yes", "//pkg:fail_test"]);
    assert_eq!(cached.status.code(), Some(3));
    assert!(
        summary(&cached).starts_with(
            "//pkg:fail_test                                                 (cached) FAILED at T\n"
        ),
        "{}",
        summary(&cached)
    );
    // --flaky_test_attempts reruns any failing test.
    let attempts = ws.run(&["test", "--flaky_test_attempts=2", "//pkg:fail_test"]);
    let text = summary(&attempts);
    assert!(
        text.starts_with(
            "//pkg:fail_test                                                          FAILED in 2 out of 2 at T\n  Stats over 2 runs\n  /"
        ),
        "{text}"
    );
    assert!(text.contains("/test_attempts/attempt_1.log\n"), "{text}");
}

#[test]
fn sharding_flaky_timeout_and_signals() {
    let ws = Workspace::new();
    let sharded = ws.run(&["test", "--test_output=all", "//pkg:sharded_test"]);
    assert_eq!(sharded.status.code(), Some(0), "{}", stderr(&sharded));
    let text = stdout(&sharded);
    for shard in 0..3 {
        assert!(
            text.contains(&format!(
                "==================== Test output for //pkg:sharded_test (shard {} of 3):\nshard {shard} of 3\n",
                shard + 1
            )),
            "{text}"
        );
    }
    assert_eq!(
        summary(&sharded),
        "//pkg:sharded_test                                                       PASSED at T\n  Stats over 3 runs\n\nExecuted 1 out of 1 test: 1 test passes.\n"
    );
    let noshard = ws.run(&["test", "//pkg:noshard_test"]);
    assert_eq!(noshard.status.code(), Some(3));
    assert_eq!(
        summary(&noshard),
        "//pkg:noshard_test                                                    NO STATUS\n\nExecuted 0 out of 1 test: 1 was skipped.\n"
    );
    assert!(stderr(&noshard).contains(": Testing //pkg:noshard_test (shard 1 of 2) failed: Sharding requested, but the test runner did not advertise support for it by touching TEST_SHARD_STATUS_FILE."));
    let flaky = ws.run(&["test", "--test_env=FLAKY_MARKER", "//pkg:flaky_test"]);
    assert_eq!(flaky.status.code(), Some(0), "{}", stderr(&flaky));
    let text = summary(&flaky);
    assert!(
        text.starts_with("//pkg:flaky_test                                                          FLAKY, failed in 1 out of 2 at T\n  Stats over 2 runs\n  /")
            && text.contains("/testlogs/pkg/flaky_test/test_attempts/attempt_1.log\n"),
        "{text}"
    );
    assert!(stderr(&flaky).contains("\nFLAKY: //pkg:flaky_test (Summary)\n      /"));
    let always = ws.run(&["test", "//pkg:always_flaky_test"]);
    assert_eq!(always.status.code(), Some(3));
    assert!(summary(&always).starts_with(
        "//pkg:always_flaky_test                                                  FAILED in 3 out of 3 at T\n"
    ));
    assert_eq!(logs(&always).len(), 3);
    let slow = ws.run(&["test", "--test_timeout=1", "//pkg:slow_test"]);
    assert_eq!(slow.status.code(), Some(3));
    assert!(summary(&slow).starts_with(
        "//pkg:slow_test                                                         TIMEOUT at T\n"
    ));
    let log = &logs(&slow)[0];
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("sleeping\n-- Test timed out at ")
    );
    assert!(
        fs::read_to_string(log.with_file_name("test.xml"))
            .unwrap()
            .contains("<error message=\"exited with error code 142\"></error>")
    );
    let signal = ws.run(&["test", "//pkg:signal_test"]);
    assert_eq!(signal.status.code(), Some(3));
    assert!(
        stderr(&signal).contains("FAIL: //pkg:signal_test (Segmentation fault) (see /"),
        "{}",
        stderr(&signal)
    );
    let premature = ws.run(&["test", "--test_output=errors", "//pkg:premature_test"]);
    assert_eq!(premature.status.code(), Some(3));
    assert!(
        stdout(&premature).contains(
            "premature\n-- Test exited prematurely (TEST_PREMATURE_EXIT_FILE exists) --\n"
        ),
        "{}",
        stdout(&premature)
    );
    let outputs = ws.run(&["test", "//pkg:outputs_test"]);
    assert_eq!(outputs.status.code(), Some(0));
    let entry = fs::read_dir(ws.path().join("private-cache/tests"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.join("bazel-out/k8-fastbuild/testlogs/pkg/outputs_test")
                .exists()
        })
        .unwrap();
    let base = entry.join("bazel-out/k8-fastbuild/testlogs/pkg/outputs_test");
    assert_eq!(
        fs::read_to_string(base.join("test.outputs/out.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(base.join("test.outputs_manifest/ANNOTATIONS")).unwrap(),
        "annotation\n"
    );
    assert!(
        fs::read_to_string(base.join("test.outputs_manifest/MANIFEST"))
            .unwrap()
            .starts_with("out.txt\t6\t")
    );
}

#[test]
fn summaries_filters_suites_and_exit_codes() {
    let ws = Workspace::new();
    let terse = ws.run(&[
        "test",
        "--test_summary=terse",
        "//pkg:pass_test",
        "//pkg:fail_test",
    ]);
    assert_eq!(terse.status.code(), Some(3));
    let text = summary(&terse);
    assert!(
        text.starts_with(
            "//pkg:fail_test                                                          FAILED at T\n  /"
        ) && text.ends_with("\nExecuted 2 out of 2 tests: 1 test passes and 1 fails locally.\n"),
        "{text}"
    );
    let detailed = ws.run(&[
        "test",
        "--test_summary=detailed",
        "//pkg:xml_test",
        "//pkg:fail_test",
    ]);
    assert_eq!(
        summary(&detailed),
        "//pkg:fail_test                                                          FAILED at T\n    ERROR   .pkg/fail_test.sh (0.0s)\n//pkg:xml_test                                                           FAILED at T\n    PASSED  C.a (0.5s)\n    SKIPPED C.c\n    FAILED  C.b (1.3s)\nTest cases: finished with 1 passing, 1 skipped and 2 failing out of 4 test cases\n\nExecuted 2 out of 2 tests: 2 fail locally.\n"
    );
    // A cached result keeps the TestResultData of its run: no test case
    // when that run's summary format did not parse test.xml, so detailed
    // counts it as unknown, like genuine Bazel (TestRunnerAction.actionCacheHit,
    // analysis/test/TestRunnerAction.java:686-700; the stored test.xml is not
    // re-parsed).
    let warm = ws.run(&["test", "//pkg:exclusive_test"]);
    assert_eq!(warm.status.code(), Some(0));
    let cached = ws.run(&[
        "test",
        "--test_summary=detailed",
        "//pkg:exclusive_test",
        "//pkg:fail_test",
    ]);
    assert_eq!(
        summary(&cached),
        "//pkg:exclusive_test                                            (cached) PASSED at T\n//pkg:fail_test                                                          FAILED at T\n    ERROR   .pkg/fail_test.sh (0.0s)\nTest cases: finished with 0 passing, 0 skipped and 1 failing out of 2 test cases (some targets did not have test case information)\n\nExecuted 1 out of 2 tests: 1 test passes and 1 fails locally.\n"
    );
    let none = ws.run(&["test", "--test_summary=none", "//pkg:pass_test"]);
    assert_eq!(
        summary(&none),
        "\nExecuted 0 out of 1 test: 1 test passes.\n"
    );
    // Filters (TestFilter) and manual tests.
    let filtered = ws.run(&[
        "test",
        "--test_tag_filters=unit,lang",
        "--test_lang_filters=sh_like",
        "//pkg:all",
    ]);
    assert!(
        summary(&filtered).ends_with("Executed 0 out of 1 test: 1 test passes.\n"),
        "{}",
        summary(&filtered)
    );
    let sizes = ws.run(&["test", "--test_size_filters=large", "//pkg:all"]);
    assert!(summary(&sizes).starts_with("//pkg:large_test "));
    assert!(summary(&sizes).ends_with("Executed 1 out of 1 test: 1 test passes.\n"));
    let langs = ws.run(&["test", "--test_lang_filters=other_lang", "//pkg:all"]);
    assert!(summary(&langs).starts_with("//pkg:lang_test "));
    let none = ws.run(&["test", "--test_size_filters=enormous", "//pkg:all"]);
    assert_eq!(none.status.code(), Some(4));
    assert!(stderr(&none).contains("WARNING: All specified test targets were excluded by filters"));
    assert!(stderr(&none).contains("ERROR: No test targets were found, yet testing was requested"));
    let no_tests = ws.run(&["test", "//nt:gen"]);
    assert_eq!(no_tests.status.code(), Some(4));
    let manual = ws.run(&["test", "//pkg:manual_test"]);
    assert!(summary(&manual).starts_with("//pkg:manual_test "));
    // Suites: explicit tests (manual included), nested suites, implicit
    // tests (non-manual tests of the package) and tag filters.
    let suite = ws.run(&["test", "//suite:suite"]);
    let text = summary(&suite);
    assert!(
        text.contains("//suite:a_test ")
            && text.contains("//suite:b_test ")
            && text.contains("//suite:c_test ")
            && text.ends_with("Executed 3 out of 3 tests: 3 tests pass.\n"),
        "{text}"
    );
    let implicit = ws.run(&["test", "//suite:implicit"]);
    assert!(summary(&implicit).ends_with("Executed 0 out of 2 tests: 2 tests pass.\n"));
    let small = ws.run(&["test", "//suite:small_only"]);
    assert!(
        summary(&small).starts_with("//suite:b_test ")
            && summary(&small).ends_with("out of 1 test: 1 test passes.\n")
    );
    // --build_tests_only drops non-test targets.
    let tests_only = ws.run(&["test", "--build_tests_only", "//nt:gen", "//suite:b_test"]);
    assert_eq!(tests_only.status.code(), Some(0));
    assert!(!stderr(&tests_only).contains("//nt:gen\t"));
    // Option errors are Bazel's.
    for (option, message) in [
        (
            "--test_output=bogus",
            "While parsing option --test_output=bogus: Not a valid test output: 'bogus' (should be summary, errors, all or streamed)",
        ),
        (
            "--test_timeout=1,2",
            "While parsing option --test_timeout=1,2: Invalid number of comma-separated entries",
        ),
        (
            "--flaky_test_attempts=0",
            "While parsing option --flaky_test_attempts=0: '0' should be >= 1",
        ),
    ] {
        let output = ws.run(&["test", option, "//pkg:pass_test"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains(message), "{}", stderr(&output));
    }
}

#[test]
fn runs_per_test_external_streamed_and_run() {
    let ws = Workspace::new();
    let runs = ws.run(&[
        "test",
        "--runs_per_test=2",
        "--test_output=all",
        "//pkg:pass_test",
    ]);
    assert_eq!(runs.status.code(), Some(0), "{}", stderr(&runs));
    let text = stdout(&runs);
    assert!(text.contains("==================== Test output for //pkg:pass_test (run 2 of 2):\n"));
    assert!(
        text.contains("TEST_RANDOM_SEED=2\nTEST_RUN_NUMBER=2\n"),
        "{text}"
    );
    // Several runs are never cached under auto.
    let again = ws.run(&["test", "--runs_per_test=2", "//pkg:pass_test"]);
    assert!(summary(&again).ends_with("Executed 1 out of 1 test: 1 test passes.\n"));
    let external = ws.run(&["test", "//pkg:external_test"]);
    assert!(summary(&external).contains("Executed 1 out of 1 test"));
    let external = ws.run(&["test", "//pkg:external_test"]);
    assert!(summary(&external).contains("Executed 1 out of 1 test"));
    let streamed = ws.run(&["test", "--test_output=streamed", "//pkg:fail_test"]);
    assert_eq!(streamed.status.code(), Some(3));
    assert!(
        stdout(&streamed).starts_with("failing\nto-stderr\n"),
        "{}",
        stdout(&streamed)
    );
    assert!(stderr(&streamed).contains(
        "WARNING: Streamed test output requested. All tests will be run without sharding, one at a time"
    ));
    // `run` of a test target: test-setup.sh with the test environment.
    let run = ws.run(&["run", "//pkg:pass_test", "--", "r1"]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    let text = stdout(&run);
    assert!(
        text.starts_with("exec ${PAGER:-/usr/bin/less} \"$0\" || exit 1\nExecuting tests from //pkg:pass_test\n-----------------------------------------------------------------------------\nargs: r1\n"),
        "{text}"
    );
    assert!(text.contains("TEST_TARGET=//pkg:pass_test\n") && text.contains("data file\n"));
    assert!(
        stderr(&run).contains(
            "bazel-rs: Running command line: external/bazel_tools/tools/test/test-setup.sh pkg/pass_test.sh <args omitted>"
        ),
        "{}",
        stderr(&run)
    );
    let run = ws.run(&["run", "//pkg:fail_test"]);
    assert_eq!(run.status.code(), Some(1));
    let run = ws.run(&["run", "//pkg:sharded_test"]);
    assert!(
        stderr(&run).contains("Run command does not support tests with more than one shard or run")
    );
}

const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];

/// In the built-in profile the implicit test-runner attributes (`$test_setup_script`,
/// `$collect_coverage_script`, ... of `@bazel_tools//tools/test`) are analyzed, so
/// `@bazel_tools`' mapping reaches the test's `_repo_mapping` like in Bazel
/// (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest).
#[test]
fn builtin_profile_repo_mapping_includes_test_runner_dependencies() {
    let ws = Workspace::new();
    let stubs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_flags/stubs");
    let mut module =
        "module(name = \"bt\")\nbazel_dep(name = \"zlib\", version = \"1.3.1.bcr.5\")\n".to_owned();
    for name in STUBBED {
        let path = match *name {
            // A copy of the stub whose sh_binary (`@bazel_tools//tools/test:collect_coverage`,
            // the implicit `$collect_coverage_script`) depends on a target of
            // its own repository, which sees @zlib.
            "rules_shell" => {
                let copy = ws.path().join("stub_rules_shell");
                fs::create_dir_all(copy.join("shell")).unwrap();
                fs::copy(
                    stubs.join("rules_shell/shell/BUILD.bazel"),
                    copy.join("shell/BUILD.bazel"),
                )
                .unwrap();
                fs::write(
                    copy.join("MODULE.bazel"),
                    "module(name = \"rules_shell\")\nbazel_dep(name = \"zlib\", version = \"1.3.1.bcr.5\")\n",
                )
                .unwrap();
                fs::write(
                    copy.join("shell/sh_binary.bzl"),
                    fs::read_to_string(stubs.join("rules_shell/shell/sh_binary.bzl"))
                        .unwrap()
                        .replace(
                            "attrs = {\"srcs\": attr.label_list(allow_files = True)}",
                            "attrs = {\"srcs\": attr.label_list(allow_files = True), \"_d\": attr.label(default = \"//:d.txt\", allow_single_file = True)}",
                        ),
                )
                .unwrap();
                fs::write(copy.join("BUILD.bazel"), "exports_files([\"d.txt\"])\n").unwrap();
                fs::write(copy.join("d.txt"), "d\n").unwrap();
                copy
            }
            "zlib" => {
                let copy = ws.path().join("stub_zlib");
                fs::create_dir_all(&copy).unwrap();
                fs::write(copy.join("MODULE.bazel"), "module(name = \"zlib\")\n").unwrap();
                fs::write(copy.join("BUILD.bazel"), "exports_files([\"z.txt\"])\n").unwrap();
                fs::write(copy.join("z.txt"), "z\n").unwrap();
                copy
            }
            _ => stubs.join(name),
        };
        module.push_str(&format!(
            "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
            path.display()
        ));
    }
    ws.file("MODULE.bazel", &module);
    ws.file(
        "rm/BUILD",
        "load(\"//:defs.bzl\", \"sh_like_test\")\nsh_like_test(name = \"rm_test\", data = [\"@zlib//:z.txt\"], script = \"#!/bin/bash\\ncat ../_repo_mapping\\n\")\n",
    );
    let registry = format!(
        "--registry=file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let output = ws.run(&[
        "test",
        "--experimental-builtin-modules",
        &registry,
        "--test_output=all",
        "//rm:rm_test",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(
            "==================== Test output for //rm:rm_test:\n,bt,_main\n,zlib,zlib+\nbazel_tools,zlib,zlib+\nrules_shell+,zlib,zlib+\nzlib+,zlib,zlib+\n"
        ),
        "{}",
        stdout(&output)
    );
}

/// A build failure prints FAILED TO BUILD / NO STATUS and exits 1; a test
/// action error stops the remaining tests unless --keep_going; only the
/// last configuration of a test action is cached; attempt log modes.
#[test]
fn build_failures_keep_going_cache_index_and_attempt_modes() {
    let ws = Workspace::new();
    ws.file(
        "bf/BUILD",
        "load(\"//:defs.bzl\", \"sh_like_test\")\nsh_like_test(name = \"broken_test\", data = [\":missing.txt\"], script = \"#!/bin/bash\\n\")\n",
    );
    let failed = ws.run(&["test", "//bf:broken_test", "//suite:b_test"]);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        stdout(&failed),
        "//suite:b_test                                                        NO STATUS\n//bf:broken_test                                                FAILED TO BUILD\n\nExecuted 0 out of 2 tests: 1 fails to build and 1 was skipped.\n"
    );
    let keep_going = ws.run(&["test", "-k", "//bf:broken_test"]);
    assert!(stderr(&keep_going).contains("--keep_going with a build failure is not supported"));
    let build = ws.run(&["build", "-k", "//suite:b_test"]);
    assert!(!build.status.success());
    // With --keep_going every test runs despite the sharding error.
    let all = ws.run(&[
        "test",
        "--keep_going",
        "//pkg:noshard_test",
        "//suite:a_test",
        "//suite:b_test",
    ]);
    // The failed action fails its target like a build error (exit 1;
    // genuine capture M36).
    assert_eq!(all.status.code(), Some(1));
    assert_eq!(
        summary(&all),
        "//pkg:noshard_test                                              FAILED TO BUILD\n//suite:a_test                                                           PASSED at T\n//suite:b_test                                                           PASSED at T\n\nExecuted 3 out of 3 tests: 2 tests pass and 1 fails to build.\n"
    );
    assert!(
        !stderr(&all)
            .lines()
            .any(|line| line.starts_with("ERROR: Sharding requested"))
    );
    let aborted = ws.run(&["test", "//pkg:noshard_test"]);
    assert_eq!(aborted.status.code(), Some(3));
    assert!(
        stderr(&aborted)
            .lines()
            .any(|line| line.starts_with("ERROR: Sharding requested"))
    );
    // Another timeout replaces the cached result of the test action.
    let first = ws.run(&["test", "//suite:b_test"]);
    assert!(summary(&first).contains("(cached) PASSED"));
    let other = ws.run(&["test", "--test_timeout=7", "//suite:b_test"]);
    assert!(summary(&other).contains("Executed 1 out of 1 test"));
    let back = ws.run(&["test", "//suite:b_test"]);
    assert!(
        summary(&back).contains("Executed 1 out of 1 test"),
        "{}",
        summary(&back)
    );
    let attempts = ws.run(&["test", "--flaky_test_attempts=2", "//pkg:fail_test"]);
    let logs = logs(&attempts);
    assert_eq!(
        fs::metadata(&logs[0]).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        fs::metadata(&logs[1]).unwrap().permissions().mode() & 0o777,
        0o555
    );
}
