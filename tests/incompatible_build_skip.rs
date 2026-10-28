//! Original target-pattern provenance, skipped test reporting and binary replay.
//! Genuine 9.3 receipts: fixtures/incompatible_build_skip/REFERENCE.md.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use base64::Engine as _;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

struct Workspace(tempfile::TempDir);
impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        let archive: Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/label_repo_name/platforms_archive.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(archive["bytes_base64"].as_str().unwrap())
            .unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
            .unpack(ws.path().join("platforms"))
            .unwrap();
        let license = fs::File::open(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/module_archives/rules_license-1.0.0.tar.gz"),
        )
        .unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(license))
            .unpack(ws.path().join("rules_license"))
            .unwrap();
        ws.write("MODULE.bazel", "module(name='skip_policy')\nbazel_dep(name='platforms',version='1.0.0')\nlocal_path_override(module_name='platforms',path='platforms')\nbazel_dep(name='rules_license',version='1.0.0')\nlocal_path_override(module_name='rules_license',path='rules_license')\n");
        ws.write("BUILD.bazel", "platform(name='platform',constraint_values=['@platforms//cpu:x86_64','@platforms//os:linux'])\n");
        ws.write("defs.bzl", r##"
def _write(ctx):
    if ctx.attr.explode:
        fail("INCOMPATIBLE-IMPL-RAN")
    ctx.actions.write(ctx.outputs.out, "built\n")
    return [DefaultInfo(files=depset([ctx.outputs.out]))]
writer = rule(implementation=_write, attrs={"deps":attr.label_list(allow_files=True), "explode":attr.bool()}, outputs={"out":"%{name}.out"})
def _test(ctx):
    if ctx.attr.explode:
        fail("INCOMPATIBLE-TEST-IMPL-RAN")
    ctx.actions.write(ctx.outputs.executable, "#!/bin/sh\necho TEST-RAN\nexit 0\n", is_executable=True)
    return [DefaultInfo(executable=ctx.outputs.executable)]
tiny_test = rule(implementation=_test, test=True, attrs={"explode":attr.bool()})
"##);
        ws.write(
            "mix/BUILD.bazel",
            r#"
load("//:defs.bzl", "writer")
package(default_visibility=["//visibility:public"])
writer(name="bad", target_compatible_with=["@platforms//:incompatible"], explode=True)
writer(name="ok")
writer(name="indirect", deps=[":bad"], explode=True)
alias(name="alias", actual=":bad")
label_flag(name="flag", build_setting_default=":bad")
writer(name="missing", deps=["//absent:nope"])
"#,
        );
        ws.write("one/BUILD.bazel", "load('//:defs.bzl','writer')\nwriter(name='bad',target_compatible_with=['@platforms//:incompatible'],explode=True)\n");
        ws.write("tests/BUILD.bazel", "load('//:defs.bzl','tiny_test')\ntiny_test(name='bad',target_compatible_with=['@platforms//:incompatible'],explode=True)\ntiny_test(name='ok')\ntest_suite(name='suite',tests=[':bad',':ok'])\n");
        ws.write("badtests/BUILD.bazel", "load('//:defs.bzl','tiny_test')\ntiny_test(name='bad',target_compatible_with=['@platforms//:incompatible'],explode=True,shard_count=51)\ntest_suite(name='suite',tests=[':bad'])\n");
        ws
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn command(&self, command: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        cmd.current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("BAZEL_RS_CACHE_GC", "off")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
            ])
            .args(args);
        cmd
    }
    fn run(&self, command: &str, args: &[&str], exit: i32) -> Output {
        let out = self.command(command, args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(exit),
            "{command} {args:?}: {}\n{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(!stderr(&out).contains("INCOMPATIBLE-IMPL-RAN"));
        assert!(!stderr(&out).contains("INCOMPATIBLE-TEST-IMPL-RAN"));
        out
    }
    fn bep(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.path().join(name))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn explicitness_comes_from_original_positive_single_target_patterns() {
    let ws = Workspace::new();
    for (args, exit) in [
        (vec!["//one:all"], 0),
        (vec!["one/bad"], 0),
        (vec!["//one:all", "//one:bad"], 1),
        (vec!["--", "//one:bad", "-//one:bad", "//one:all"], 1),
        (vec!["--", "//one:all", "-//one:bad", "//one:all"], 0),
        (vec!["--skip_incompatible_explicit_targets", "//one:bad"], 0),
        (
            vec![
                "--skip_incompatible_explicit_targets",
                "--noskip_incompatible_explicit_targets",
                "//one:bad",
            ],
            1,
        ),
    ] {
        let out = ws.run("build", &args, exit);
        let err = stderr(&out);
        assert!(err.contains(if exit==0 {"Target //one:bad was skipped"} else {"Target //one:bad is incompatible and cannot be built, but was explicitly requested."}),"{err}");
        assert!(!stdout(&out).contains("\t"));
    }
}

#[test]
fn requested_output_alias_and_indirect_cause_chains_keep_their_identity() {
    let ws = Workspace::new();
    for (label, chain) in [
        ("//mix:bad.out", vec!["//mix:bad.out"]),
        ("//mix:alias", vec!["//mix:alias", "//mix:bad"]),
        ("//mix:indirect", vec!["//mix:indirect", "//mix:bad"]),
        ("//mix:flag", vec!["//mix:bad"]),
    ] {
        let out = ws.run("build", &[label], 1);
        let err = stderr(&out);
        assert!(
            err.contains(&format!("Target {label} is incompatible")),
            "{err}"
        );
        let actual: Vec<_> = err
            .lines()
            .filter(|line| line.starts_with("    "))
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(actual, chain);
        assert!(err.contains("didn't satisfy constraint @@platforms//:incompatible"));
    }
}

#[test]
fn skipped_roots_do_not_mask_real_errors_and_keep_going_keeps_compatible_work() {
    let ws = Workspace::new();
    let out = ws.run("build", &["--keep_going", "//mix:indirect", "//mix:ok"], 1);
    let err = stderr(&out);
    assert!(err.contains("errors encountered while analyzing target '//mix:indirect'"));
    assert!(err.contains("Dependency chain:"));
    assert!(err.contains("Build succeeded for only 1 of 2 top-level targets"));
    assert!(stdout(&out).contains("//mix:ok\t"));
    let out = ws.run(
        "build",
        &[
            "--keep_going",
            "--skip_incompatible_explicit_targets",
            "//mix:bad",
            "//mix:missing",
            "//mix:ok",
        ],
        1,
    );
    assert!(stderr(&out).contains("absent"));
    assert!(stderr(&out).contains("Build succeeded for only 2 of 3 top-level targets"));
    assert!(stdout(&out).contains("//mix:ok\t"));
    assert!(!stdout(&out).contains("//mix:bad\t"));
}

#[test]
fn skipped_tests_have_no_run_or_shard_validation_and_genuine_exit_codes() {
    let ws = Workspace::new();
    for args in [
        vec!["//badtests:all"],
        vec!["//badtests:suite"],
        vec!["--skip_incompatible_explicit_targets", "//badtests:bad"],
    ] {
        let out = ws.run("test", &args, 0);
        assert!(stdout(&out).contains("SKIPPED"));
        assert!(stdout(&out).contains("Executed 0 out of 1 test: 1 was skipped."));
        assert!(!stderr(&out).contains("50 shards"));
    }
    ws.run("test", &["//badtests:bad"], 1);
    let nobuild = ws.run("test", &["--nobuild", "//badtests:all"], 1);
    assert!(stderr(&nobuild).contains("Couldn't start the build. Unable to run tests"));
    assert!(!stdout(&nobuild).contains("SKIPPED"));
    ws.run("test", &["//one:all"], 4);
    let rejected = ws.run("test", &["--keep_going", "//badtests:bad"], 1);
    assert!(stderr(&rejected).contains("No test targets were found"));
    let run = ws.run(
        "run",
        &["--skip_incompatible_explicit_targets", "//badtests:bad"],
        2,
    );
    assert!(stderr(&run).contains("No targets found to run"));
    let coverage = ws.run("coverage", &["//badtests:all"], 0);
    assert!(stdout(&coverage).contains("SKIPPED"));
}

#[test]
fn suite_members_skip_but_compatible_tests_run_and_keep_going_rejection_is_not_skip() {
    let ws = Workspace::new();
    let out = ws.run("test", &["//tests:suite"], 0);
    assert!(stdout(&out).contains("Executed 1 out of 2 tests: 1 test passes and 1 was skipped."));
    assert!(stdout(&out).contains("SKIPPED"));
    let out = ws.run("test", &["--keep_going", "//tests:bad", "//tests:ok"], 1);
    assert!(stdout(&out).contains("1 test passes"));
    assert!(!stdout(&out).contains("SKIPPED"));
    assert!(stderr(&out).contains("explicitly requested"));
}

#[test]
fn bep_skipped_events_and_binary_snapshot_replay_are_distinct_from_test_results() {
    let ws = Workspace::new();
    let args = ["--build_event_json_file=events.json", "//badtests:suite"];
    let cold = ws.run("test", &args, 0);
    assert!(
        stderr(&cold).contains("snapshot: saved"),
        "{}",
        stderr(&cold)
    );
    let check = |events: Vec<Value>| {
        let event = events
            .iter()
            .find(|e| e["aborted"]["reason"] == "SKIPPED")
            .unwrap();
        assert_eq!(event["id"]["targetCompleted"]["label"], "//badtests:bad");
        assert_eq!(
            event["aborted"]["description"],
            "Target //badtests:bad build was skipped."
        );
        assert!(
            !events
                .iter()
                .any(|e| e.get("testSummary").is_some() || e.get("testResult").is_some())
        );
        assert!(!events.iter().any(|e| e.get("completed").is_some()));
    };
    check(ws.bep("events.json"));
    let warm = ws.run("test", &args, 0);
    assert!(stderr(&warm).contains("snapshot: hit"));
    assert_eq!(stdout(&cold), stdout(&warm));
    check(ws.bep("events.json"));
    let verified = ws
        .command("test", &args)
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(verified.status.success(), "{}", stderr(&verified));
    check(ws.bep("events.json"));
    let changed = ws.run(
        "test",
        &["--build_event_json_file=explicit.json", "//badtests:bad"],
        1,
    );
    assert!(!stderr(&changed).contains("snapshot: hit"));
    ws.run(
        "build",
        &[
            "--nobuild",
            "--build_event_json_file=nobuild.json",
            "//one:all",
        ],
        0,
    );
    assert!(
        ws.bep("nobuild.json")
            .iter()
            .any(|e| e["aborted"]["reason"] == "SKIPPED")
    );
}

#[test]
fn skip_policy_follows_rc_priority_and_module_alias_reparse_without_config_rekeying() {
    let ws = Workspace::new();
    ws.write(
        ".bazelrc",
        "build:skip --skip_incompatible_explicit_targets\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(ws.path())
        .args([
            "--nosystem_rc",
            "--nohome_rc",
            "--cache-dir=rc-cache",
            "build",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--platforms=//:platform",
            "--host_platform=//:platform",
            "--config=skip",
            "//one:bad",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    ws.run(
        "build",
        &["--skip_incompatible_explicit_targets", "//one:bad"],
        0,
    );
    let warm = ws.run(
        "build",
        &["--skip_incompatible_explicit_targets", "//one:bad"],
        0,
    );
    assert!(stderr(&warm).contains("snapshot: hit"));
    ws.run(
        "build",
        &["--noskip_incompatible_explicit_targets", "//one:bad"],
        1,
    );
    let first = ws.run(
        "cquery",
        &["--output=proto", "--transitions=lite", "deps(//one:bad)"],
        0,
    );
    let second = ws.run(
        "cquery",
        &[
            "--skip_incompatible_explicit_targets",
            "--output=proto",
            "--transitions=lite",
            "deps(//one:bad)",
        ],
        0,
    );
    assert_eq!(first.stdout, second.stdout);
    let module = fs::read_to_string(ws.path().join("MODULE.bazel")).unwrap();
    ws.write(
        "MODULE.bazel",
        &(module
            + "flag_alias(name='skip_incompatible_explicit_targets',starlark_flag='//:switch')\n"),
    );
    ws.write("settings.bzl","def _setting(ctx):\n    return []\nbool_flag=rule(implementation=_setting,build_setting=config.bool(flag=True))\n");
    let build = fs::read_to_string(ws.path().join("BUILD.bazel")).unwrap();
    ws.write("BUILD.bazel",&(build+"load(':settings.bzl','bool_flag')\nbool_flag(name='switch',build_setting_default=False)\n"));
    let alias = ws.run(
        "build",
        &["--skip_incompatible_explicit_targets", "//one:bad"],
        1,
    );
    assert!(
        stderr(&alias).contains("explicitly requested"),
        "{}",
        stderr(&alias)
    );
    let invalid = ws.run(
        "build",
        &["--skip_incompatible_explicit_targets=invalid", "//one:bad"],
        2,
    );
    assert!(stderr(&invalid).contains("not a boolean"));
}

#[test]
fn skipped_tests_survive_real_analysis_and_execution_failures_without_masking_them() {
    let ws = Workspace::new();
    ws.write("errors/defs.bzl", r##"
def _test(ctx):
    if ctx.attr.analysis_fail:
        fail("REAL-ANALYSIS-ERROR")
    ctx.actions.write(ctx.outputs.executable, "#!/bin/sh\nexit %s\n" % ctx.attr.exit_code, is_executable=True)
    return [DefaultInfo(executable=ctx.outputs.executable)]
error_test = rule(implementation=_test, test=True, attrs={"analysis_fail":attr.bool(), "exit_code":attr.int()})
"##);
    ws.write(
        "errors/BUILD.bazel",
        r#"
load(":defs.bzl", "error_test")
error_test(name="bad", target_compatible_with=["@platforms//:incompatible"], analysis_fail=True)
error_test(name="z_analysis", analysis_fail=True)
error_test(name="z_execution", exit_code=1)
test_suite(name="analysis_suite", tests=[":bad", ":z_analysis"])
test_suite(name="execution_suite", tests=[":bad", ":z_execution"])
"#,
    );
    let analysis = ws.run(
        "test",
        &[
            "--build_event_json_file=analysis.json",
            "//errors:analysis_suite",
        ],
        1,
    );
    assert!(
        stdout(&analysis).contains("Executed 0 out of 1 test: 1 was skipped."),
        "{}",
        stdout(&analysis)
    );
    assert!(stderr(&analysis).contains("REAL-ANALYSIS-ERROR"));
    assert!(
        ws.bep("analysis.json")
            .iter()
            .any(|e| e["aborted"]["reason"] == "SKIPPED")
    );
    let execution = ws.run(
        "test",
        &[
            "--build_event_json_file=execution.json",
            "//errors:execution_suite",
        ],
        3,
    );
    assert!(
        stdout(&execution).contains("1 fails locally and 1 was skipped."),
        "{}",
        stdout(&execution)
    );
    assert!(stdout(&execution).contains("SKIPPED"));
    let events = ws.bep("execution.json");
    assert!(events.iter().any(|e| e["aborted"]["reason"] == "SKIPPED"));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.get("testSummary").is_some())
            .count(),
        1
    );
}
