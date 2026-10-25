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

include!("support/snapshot_off.rs");
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        Self::fixture("workspace")
    }

    /// A copy of `tests/fixtures/nobuild/<name>`.
    fn fixture(name: &str) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("nobuild-")
                .tempdir_in(base)
                .unwrap(),
        );
        let source = manifest().join("tests/fixtures/nobuild").join(name);
        copy(&source, ws.path());
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_with(arguments, &[])
    }

    fn run_with(&self, arguments: &[&str], env: &[(&str, &str)]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(arguments)
            .envs(env.iter().copied())
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// Files materialized under the native cache (action outputs live in
    /// `cache/actions/<key>/outputs/...`); empty when nothing executed —
    /// and nothing was saved: the commands whose cache this lists run with
    /// the analysis snapshot off (on by default, a `--nobuild` run saves its
    /// snapshot in the workspace's state directory, `cache/worktrees/`).
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
    // The cache is listed below (Workspace::cache_files).
    let output = ws.run_with(&["build", "--nobuild", "//pkg:gen"], &[SNAPSHOT_OFF]);
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
    // The cache is listed below (Workspace::cache_files).
    let nobuild = ws.run_with(&["build", "--nobuild", "//pkg:bad"], &[SNAPSHOT_OFF]);
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
    // The caches are listed below (Workspace::cache_files).
    let ws = Workspace::new();
    let test = ws.run_with(&["test", "--nobuild", "//pkg:t"], &[SNAPSHOT_OFF]);
    assert_eq!(test.status.code(), Some(1), "{}", stderr(&test));
    assert!(
        stderr(&test).contains("ERROR: Couldn't start the build. Unable to run tests"),
        "{}",
        stderr(&test)
    );
    assert!(stderr(&test).contains("0 actions executed"));
    assert!(ws.cache_files().is_empty());

    let ws = Workspace::new();
    let run = ws.run_with(&["run", "--nobuild", "//pkg:tool"], &[SNAPSHOT_OFF]);
    assert!(!run.status.success(), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("No targets found to run"),
        "{}",
        stderr(&run)
    );
    assert!(ws.cache_files().is_empty());
}

/// The `build` of `//pkg:t` with an aspect adding to the output group its
/// base target already has (tests/fixtures/nobuild/published_workspace).
const SHARED_GROUP_ASPECT: [&str; 5] = [
    "build",
    "--show_result=10",
    "--aspects=//pkg:defs.bzl%extra_aspect",
    "--output_groups=extra",
    "//pkg:t",
];

/// The paths listed under the aspect's `up-to-date:` report.
fn aspect_report(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .skip_while(|line| *line != "Aspect //pkg:defs.bzl%extra_aspect of //pkg:t up-to-date:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(str::trim)
        .collect()
}

#[test]
fn executing_build_builds_every_reported_artifact() {
    // Only --nobuild may report a planned (unexecuted) path. Genuine builds
    // and reports the aspect's own `t.aspect` next to `t.base`
    // (published_receipt.json); natively its action used to be pruned, so
    // the build either panicked on the missing scheduler result or — with a
    // fallback for every missing result — reported an unbuilt path.
    let ws = Workspace::fixture("published_workspace");
    let output = ws.run(&SHARED_GROUP_ASPECT);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(err.contains("2 actions executed, 0 cache hits"), "{err}");
    let reported = aspect_report(&err);
    assert_eq!(reported.len(), 1, "{err}");
    let path = Path::new(reported[0]);
    assert!(path.starts_with(ws.path().join("cache/actions")), "{err}");
    assert!(path.ends_with("bazel-out/k8-fastbuild/bin/pkg/t.aspect"));
    assert_eq!(fs::read_to_string(path).unwrap(), "aspect\n");

    // --nobuild: the same report at the planned exec path, nothing built.
    let ws = Workspace::fixture("published_workspace");
    let mut arguments = SHARED_GROUP_ASPECT.to_vec();
    arguments.insert(1, "--nobuild");
    // The cache is listed below (Workspace::cache_files).
    let output = ws.run_with(&arguments, &[SNAPSHOT_OFF]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(err.contains("0 actions executed, 0 cache hits"), "{err}");
    assert_eq!(
        aspect_report(&err),
        ["bazel-out/k8-fastbuild/bin/pkg/t.aspect"],
        "{err}"
    );
    assert!(ws.cache_files().is_empty(), "{:?}", ws.cache_files());
}

#[test]
fn executing_test_builds_what_it_publishes() {
    // A test publishes its runfiles: the tree only when it is built (not
    // under --nobuild_runfile_links), the runfiles entries always — also
    // without manifests and with an --output_groups override (genuine: the
    // test action consumes the runfiles tree, TestActionBuilder.java:255-257;
    // both commands pass, published_receipt.json). The test cats its
    // generated data runfile, so it passes only if that was built.
    for arguments in [
        &["test", "--nobuild_runfile_links", "//pkg:dt"][..],
        &[
            "test",
            "--nobuild_runfile_manifests",
            "--output_groups=default",
            "//pkg:dt",
        ],
    ] {
        let ws = Workspace::fixture("published_workspace");
        let output = ws.run(arguments);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("Executed 1 out of 1 test: 1 test passes."),
            "{arguments:?}: {}",
            stdout(&output)
        );
    }
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
