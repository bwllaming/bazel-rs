//! Build request, execution and output options (crate::execution_options,
//! crate::output_filter, crate::option_defaults; docs/RC_OPTIONS.md):
//! `--toolchain_resolution_debug` and `--check_up_to_date` against the
//! genuine Bazel 9.3.1 capture tests/fixtures/execution_options/reference.json
//! (capture.py beside it), and the native effects of the output filters,
//! `--experimental_ui_max_stdouterr_bytes`, `--modify_execution_info`,
//! `--local_resources`, the convenience symlink modes, `--jobs=auto`,
//! `version --gnu_format` and options spelled at their genuine default.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/execution_options")
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for entry in fs::read_dir(fixture().join("workspace")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), workspace.0.path().join(entry.file_name())).unwrap();
        }
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.0.path().join(directory)).unwrap();
        }
        workspace
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn command(&self, command: &str, args: &[&str]) -> Output {
        let mut process = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        process
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .args(["--ignore_all_rc_files", "--cache-dir=cache", command]);
        if command != "version" {
            process.args([
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--platforms=//:p",
                "--host_platform=//:p",
            ]);
        }
        process.args(args).output().unwrap()
    }

    fn build(&self, args: &[&str]) -> Output {
        self.command("build", args)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn reference() -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(fixture().join("reference.json")).unwrap()).unwrap()
}

#[test]
fn toolchain_resolution_debug_matches_genuine() {
    let reference = &reference()["toolchain_debug"];
    let workspace = Workspace::new();
    let output = workspace.build(&[
        "--nobuild",
        "--toolchain_resolution_debug=//:t",
        "//:u",
        "//:v",
    ]);
    assert_eq!(
        output.status.code().map(i64::from),
        reference["exit"].as_i64()
    );
    let lines: BTreeSet<String> = stderr(&output)
        .lines()
        .filter(|line| line.contains("ToolchainResolution: "))
        .map(str::to_owned)
        .collect();
    let expected: BTreeSet<String> = reference["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(lines, expected);
    // Without the option (the default `-.*`) nothing is printed.
    let quiet = workspace.build(&["--nobuild", "//:u"]);
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    assert!(!stderr(&quiet).contains("ToolchainResolution"));
}

#[test]
fn check_up_to_date_fails_on_any_action_to_run() {
    let reference = &reference()["check_up_to_date"];
    let workspace = Workspace::new();
    let fresh = workspace.build(&["--check_up_to_date", "//:h"]);
    assert_eq!(
        fresh.status.code().map(i64::from),
        reference["exit"].as_i64()
    );
    // Genuine fails on the first action it reaches (the stable status file).
    let genuine = reference["errors"][0].as_str().unwrap();
    let message = genuine.strip_prefix("ERROR: ").unwrap();
    assert!(stderr(&fresh).contains(message), "{}", stderr(&fresh));
    // Built once, the same check succeeds; a target still to build fails.
    assert!(workspace.build(&["//:g"]).status.success());
    let built = workspace.build(&["--check_up_to_date", "//:g"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let pending = workspace.build(&["--check_up_to_date", "//:h"]);
    assert_eq!(pending.status.code(), Some(1));
    assert!(
        stderr(&pending).contains("action 'Executing genrule //:h' is not up-to-date"),
        "{}",
        stderr(&pending)
    );
}

#[test]
fn output_filters_and_stream_limits_shape_successful_action_output() {
    let workspace = Workspace::new();
    let fresh = |args: &[&str]| {
        let _ = fs::remove_dir_all(workspace.path().join("cache/actions"));
        let mut all = args.to_vec();
        all.push("//:g");
        workspace.build(&all)
    };
    let shown = fresh(&[]);
    assert!(stderr(&shown).contains("warn-out"), "{}", stderr(&shown));
    for hidden in [
        vec!["--output_filter=^//other"],
        vec!["--auto_output_filter=all"],
        vec!["--auto_output_filter=packages", "--output_filter=nothing"],
    ] {
        let output = fresh(&hidden);
        assert!(output.status.success(), "{hidden:?}: {}", stderr(&output));
        assert!(!stderr(&output).contains("warn-out"), "{hidden:?}");
    }
    for kept in [
        vec!["--output_filter=//:g"],
        vec!["--auto_output_filter=packages"],
        vec!["--auto_output_filter=subpackages"],
    ] {
        let output = fresh(&kept);
        assert!(stderr(&output).contains("warn-out"), "{kept:?}");
    }
    let limited = fresh(&["--experimental_ui_max_stdouterr_bytes=3"]);
    let text = stderr(&limited);
    assert!(
        text.contains(
            ") 9 exceeds maximum size of --experimental_ui_max_stdouterr_bytes=3 bytes; skipping"
        ) && !text.contains("warn-out"),
        "{text}"
    );
    let bad = workspace.build(&["--output_filter=[", "//:g"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(
        stderr(&bad)
            .contains("While parsing option --output_filter=[: Not a valid regular expression")
    );
}

#[test]
fn modify_execution_info_is_keyed() {
    let workspace = Workspace::new();
    assert!(workspace.build(&["//:g"]).status.success());
    let cached = workspace.build(&["//:g"]);
    assert!(
        stderr(&cached).contains("action cache hit"),
        "{}",
        stderr(&cached)
    );
    // A requirement added to Genrule spawns changes their key.
    let modified = workspace.build(&["--modify_execution_info=Genrule=+no-remote", "//:g"]);
    assert!(modified.status.success(), "{}", stderr(&modified));
    assert!(
        !stderr(&modified).contains("action cache hit"),
        "{}",
        stderr(&modified)
    );
    // A modifier for another mnemonic does not.
    let other = workspace.build(&["--modify_execution_info=CppCompile=+no-remote", "//:g"]);
    assert!(
        stderr(&other).contains("action cache hit"),
        "{}",
        stderr(&other)
    );
    // Non-additive: only the last value applies.
    let last = workspace.build(&[
        "--noincompatible_modify_execution_info_additive",
        "--modify_execution_info=Genrule=+no-remote",
        "--modify_execution_info=CppCompile=+x",
        "//:g",
    ]);
    assert!(
        stderr(&last).contains("action cache hit"),
        "{}",
        stderr(&last)
    );
    let bad = workspace.build(&["--modify_execution_info=Genrule", "//:g"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("malformed expression 'Genrule'"));
}

#[test]
fn accepted_scheduling_and_symlink_options() {
    let workspace = Workspace::new();
    for args in [
        vec![
            "--local_resources=cpu=2",
            "--local_resources=memory=HOST_RAM*.5",
        ],
        vec!["--jobs=auto"],
        vec!["--jobs=HOST_CPUS*.5"],
        vec!["--symlink_prefix=/"],
        vec!["--experimental_convenience_symlinks=ignore"],
        vec!["--progress_report_interval=60"],
        vec!["--experimental_action_cache_store_output_metadata"],
        // Unmodelled options at their genuine default (crate::option_defaults).
        vec!["--noexperimental_remote_mark_tool_inputs", "--analyze"],
    ] {
        let mut all = args.clone();
        all.push("//:g");
        let output = workspace.build(&all);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
    // `clean` deletes the prefix's symlinks, and only symlinks.
    std::os::unix::fs::symlink("/nonexistent", workspace.path().join("bazel-bin")).unwrap();
    fs::write(workspace.path().join("bazel-out"), "not a link").unwrap();
    let clean = workspace.build(&["--experimental_convenience_symlinks=clean", "//:g"]);
    assert!(clean.status.success(), "{}", stderr(&clean));
    assert!(fs::symlink_metadata(workspace.path().join("bazel-bin")).is_err());
    assert!(workspace.path().join("bazel-out").is_file());
    // What would create symlinks, a non-default unmodelled value: explicit.
    for args in [
        vec!["--symlink_prefix=.b/"],
        vec!["--experimental_convenience_symlinks=normal"],
        vec!["--experimental_remote_mark_tool_inputs"],
        vec!["--local_resources=cpu=lots"],
    ] {
        let mut all = args.clone();
        all.push("//:g");
        let output = workspace.build(&all);
        assert!(!output.status.success(), "{args:?}");
    }
}

#[test]
fn version_gnu_format() {
    let workspace = Workspace::new();
    let output = workspace.command("version", &["--gnu_format"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("bazel 9.3.1+bazel-rs.") && text.ends_with('\n'),
        "{text}"
    );
    let plain = workspace.command("version", &["--nognu_format"]);
    assert!(String::from_utf8_lossy(&plain.stdout).starts_with("bazel-rs "));
    let bad = workspace.command("version", &["--gnu_format=maybe"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn ui_event_filters_drop_note_kinds() {
    let workspace = Workspace::new();
    let shown = workspace.build(&["--nobuild", "//:g"]);
    assert!(stderr(&shown).contains("INFO: Analyzed target //:g"));
    let hidden = workspace.build(&["--ui_event_filters=-info", "--nobuild", "//:g"]);
    assert!(hidden.status.success(), "{}", stderr(&hidden));
    assert!(!stderr(&hidden).contains("INFO: "), "{}", stderr(&hidden));
    // `+info` after `-info` shows them again.
    let again = workspace.build(&[
        "--ui_event_filters=-info",
        "--ui_event_filters=+info",
        "--nobuild",
        "//:g",
    ]);
    assert!(stderr(&again).contains("INFO: Analyzed target //:g"));
    // Errors are no filterable native notes: explicit.
    let errors = workspace.build(&["--ui_event_filters=-error", "--nobuild", "//:g"]);
    assert!(!errors.status.success());
    assert!(stderr(&errors).contains("filtering ERROR events is not modelled"));
}
