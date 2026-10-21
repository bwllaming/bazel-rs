//! `--nobuild` (BuildRequestOptions.performExecutionPhase, Bazel 9.3
//! BuildRequestOptions.java:146-158): loading and analysis run normally,
//! the execution phase is skipped — zero actions execute, zero outputs
//! are written, and a failing action therefore cannot fail the command.
//! Genuine 9.3.0 behaviour is recorded in
//! tests/fixtures/nobuild/reference_receipt.json (scratch capture with
//! `--batch --ignore_all_rc_files` and a private output base).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("nobuild-")
                .tempdir_in(base)
                .unwrap(),
        );
        let source = manifest().join("tests/fixtures/nobuild/workspace");
        copy(&source, ws.path());
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(arguments)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// Files materialized under the native cache (action outputs live in
    /// `cache/actions/<key>/outputs/...`); empty when nothing executed.
    fn cache_files(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect(&self.path().join("cache"), &mut names);
        names.sort();
        names
    }
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn collect(dir: &Path, names: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, names);
        } else {
            names.push(path.to_string_lossy().into_owned());
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn nobuild_analyses_but_never_executes() {
    let ws = Workspace::new();
    let output = ws.run(&["build", "--nobuild", "//pkg:gen"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // The planned artifact is reported at its exec path (genuine reports
    // artifactsToBuild under --nobuild), with 0 actions executed.
    assert_eq!(
        stdout(&output),
        "//pkg:gen\tbazel-out/k8-fastbuild/bin/pkg/out.txt\n"
    );
    assert!(
        stderr(&output).contains("0 actions executed, 0 cache hits"),
        "{}",
        stderr(&output)
    );
    // Zero outputs written: the fresh cache holds no action output.
    assert!(
        ws.cache_files().is_empty(),
        "cache files: {:?}",
        ws.cache_files()
    );
    assert!(!ws.path().join("bazel-out").exists());
}

#[test]
fn nobuild_failing_genrule_still_succeeds() {
    // Action-execution regression: without --nobuild this genrule fails
    // (exit 42); under --nobuild no action runs, so the command succeeds.
    let ws = Workspace::new();
    let normal = ws.run(&["build", "//pkg:bad"]);
    assert!(!normal.status.success(), "{}", stderr(&normal));

    let ws = Workspace::new();
    let nobuild = ws.run(&["build", "--nobuild", "//pkg:bad"]);
    assert!(nobuild.status.success(), "{}", stderr(&nobuild));
    assert!(
        stderr(&nobuild).contains("0 actions executed, 0 cache hits"),
        "{}",
        stderr(&nobuild)
    );
    assert!(ws.cache_files().is_empty());
}

#[test]
fn nobuild_analysis_errors_still_surface() {
    let ws = Workspace::new();
    let output = ws.run(&["build", "--nobuild", "//pkg:missing"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no such target '//pkg:missing'"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn nobuild_boolean_forms_match_bazel() {
    // `--build=false` is the same negation; `--nobuild=false` is rejected
    // (the no-prefixed boolean form takes no value, genuine capture).
    let ws = Workspace::new();
    let output = ws.run(&["build", "--build=false", "//pkg:gen"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("0 actions executed"));

    let ws = Workspace::new();
    let rejected = ws.run(&["build", "--nobuild=false", "//pkg:gen"]);
    assert!(!rejected.status.success());
    assert!(
        stderr(&rejected)
            .contains("--nobuild=false :: Unexpected value after boolean option: --nobuild=false"),
        "{}",
        stderr(&rejected)
    );

    // `--build=true` executes normally.
    let ws = Workspace::new();
    let built = ws.run(&["build", "--build=true", "//pkg:gen"]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("1 actions executed"),
        "{}",
        stderr(&built)
    );
    assert!(!ws.cache_files().is_empty());
}

#[test]
fn nobuild_test_and_run_do_not_execute() {
    // Genuine: `test --nobuild` builds 0 actions then fails with
    // `Couldn't start the build. Unable to run tests` (exit 1);
    // `run --nobuild` fails with `No targets found to run` (exit 2 in
    // genuine; native reports the same text through its exit-1 channel).
    let ws = Workspace::new();
    let test = ws.run(&["test", "--nobuild", "//pkg:t"]);
    assert_eq!(test.status.code(), Some(1), "{}", stderr(&test));
    assert!(
        stderr(&test).contains("ERROR: Couldn't start the build. Unable to run tests"),
        "{}",
        stderr(&test)
    );
    assert!(stderr(&test).contains("0 actions executed"));
    assert!(ws.cache_files().is_empty());

    let ws = Workspace::new();
    let run = ws.run(&["run", "--nobuild", "//pkg:tool"]);
    assert!(!run.status.success(), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("No targets found to run"),
        "{}",
        stderr(&run)
    );
    assert!(ws.cache_files().is_empty());
}

#[test]
fn nobuild_is_rejected_for_query() {
    // QueryCommand does not inherit BuildCommand options (genuine:
    // `--nobuild :: Unrecognized option: --nobuild`); native keeps its
    // explicit unsupported-option error — never silently ignored.
    let ws = Workspace::new();
    let output = ws.run(&["query", "--nobuild", "//pkg:gen"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--nobuild"), "{}", stderr(&output));
}
