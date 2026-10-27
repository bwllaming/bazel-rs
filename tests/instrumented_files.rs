//! N54: `coverage_common.instrumented_files_info` and `InstrumentedFilesInfo`
//! without `--collect_code_coverage` (docs/COVERAGE.md): the EMPTY instance
//! from Starlark rules, filegroup, genrule and output files (also through
//! aliases), absent on source files and on rules that do not return it;
//! Bazel's parameter-validation and private-API texts; coverage
//! configurations from a Starlark transition stay explicit. Expected values
//! equal genuine Bazel 9.3.0's (tests/fixtures/instrumented_files/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/module_archives.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn fixture() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n54-")
                .tempdir_in(base)
                .unwrap(),
        );
        let source = manifest().join("tests/fixtures/instrumented_files/workspace/inf");
        fs::create_dir_all(ws.0.path().join("inf")).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            let path = entry.unwrap().path();
            fs::copy(
                &path,
                ws.0.path().join("inf").join(path.file_name().unwrap()),
            )
            .unwrap();
        }
        fs::write(
            ws.0.path().join("MODULE.bazel"),
            "module(name = \"instrumented_files\")\n",
        )
        .unwrap();
        ws
    }

    fn run(&self, target: &str) -> Output {
        // Offline: execution platform resolution materializes @platforms.
        seed_module_archives(&self.0.path().join("private-cache"), &["platforms@1.0.0"]);
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=private-cache",
                "build",
                "--experimental-builtin-modules",
            ])
            .arg(format!(
                "--registry=file://{}/tests/fixtures/builtin_modules/registry",
                manifest().display()
            ))
            .arg(target)
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }
}

#[test]
fn empty_instrumented_files_info_everywhere_bazel_has_it() {
    let ws = Workspace::fixture();
    let output = ws.run("//inf:p");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        fs::read_to_string(manifest().join("tests/fixtures/instrumented_files/expected.txt"))
            .unwrap()
    );
}

#[test]
fn parameter_validation_and_private_api_follow_bazel() {
    let ws = Workspace::fixture();
    let private = "file '//inf:defs.bzl' cannot use private API";
    for (kind, message) in [
        (
            "ctx_string",
            "in call to instrumented_files_info(), parameter 'ctx' got value of type 'string', want 'ctx'",
        ),
        (
            "source_string",
            "in call to instrumented_files_info(), parameter 'source_attributes' got value of type 'string', want 'sequence'",
        ),
        (
            "source_int",
            "at index 1 of source_attributes, got element of type int, want string",
        ),
        (
            "deps_none",
            "at index 0 of dependency_attributes, got element of type NoneType, want string",
        ),
        (
            "ext_string",
            "in call to instrumented_files_info(), parameter 'extensions' got value of type 'string', want 'sequence or NoneType'",
        ),
        (
            "ext_int",
            "at index 0 of extensions, got element of type int, want string",
        ),
        (
            "metadata_string",
            "at index 0 of metadata_files, got element of type string, want File",
        ),
        (
            "metadata_depset",
            "in call to instrumented_files_info(), parameter 'metadata_files' got value of type 'depset', want 'sequence'",
        ),
        (
            "baseline_string",
            "at index 0 of baseline_coverage_files, got element of type string, want File",
        ),
        (
            "reported_strings",
            "for 'reported_to_actual_sources', got a depset of 'string', expected a depset of 'tuple'",
        ),
        (
            "reported_list",
            "in call to instrumented_files_info(), parameter 'reported_to_actual_sources' got value of type 'list', want 'depset or NoneType'",
        ),
        ("reported_tuples", private),
        ("env_private", private),
        (
            "env_int",
            "got dict<int, string> for 'coverage_environment', want dict<string, string>",
        ),
        (
            "env_list",
            "in call to instrumented_files_info(), parameter 'coverage_environment' got value of type 'list', want 'dict'",
        ),
        (
            "support_int",
            "at index 0 of coverage_support_files, got element of type int, want one of depset, File or FilesToRunProvider",
        ),
        (
            "support_strings",
            "for 'coverage_support_files', got a depset of 'string', expected a depset of 'File'",
        ),
        ("support_file", private),
        (
            "support_string_arg",
            "in call to instrumented_files_info(), parameter 'coverage_support_files' got value of type 'string', want 'sequence or depset'",
        ),
        (
            "unknown_kw",
            "instrumented_files_info() got unexpected keyword argument 'sources'",
        ),
        (
            "two_positional",
            "instrumented_files_info() accepts no more than 1 positional argument but got 2",
        ),
        (
            "missing_ctx",
            "instrumented_files_info() missing 1 required positional argument: ctx",
        ),
        ("construct", "'Provider' object is not callable"),
        ("get_env", private),
        ("get_support", private),
        (
            "order",
            "at index 0 of extensions, got element of type int, want string",
        ),
    ] {
        let output = ws.run(&format!("//inf:bad_{kind}"));
        assert_eq!(output.status.code(), Some(1), "{kind}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("error: {message}\n")),
            "{kind}: {stderr}"
        );
    }
}

/// RuleConfiguredTargetBuilder.java:179-186: with coverage every rule
/// without InstrumentedFilesInfo or ToolchainInfo carries the forwarded
/// provider (present; its members fail explicitly natively).
#[test]
fn coverage_configurations_forward_instrumented_files_info_on_every_rule() {
    let ws = Workspace::fixture();
    let read = |target: &str| {
        let output = ws.run(target);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        fs::read_to_string(stdout.lines().next().unwrap().split_once('\t').unwrap().1).unwrap()
    };
    assert_eq!(
        read("//inf:presence"),
        "pl False\ntc False\nfg2 True\ngen.go True\na.go False\n"
    );
    assert_eq!(
        read("//inf:cov_presence"),
        "pl True\ntc False\nfg2 True\ngen.go True\na.go False\n"
    );
}

/// Collection in a coverage configuration (a Starlark transition sets
/// `--collect_code_coverage`; the default `--instrumentation_filter`
/// includes everything here): the collected providers of Starlark rules,
/// filegroup, genrule, output files and aliases, `ctx.coverage_instrumented`
/// and forwardAll, byte for byte against genuine Bazel 9.3.0
/// (tests/fixtures/instrumented_files/coverage_*.txt, captured on this
/// fixture with `build //inf:cov //inf:cov_p //inf:cov_presence_read`).
#[test]
fn coverage_configurations_follow_bazel() {
    let ws = Workspace::fixture();
    for (target, expected) in [
        ("//inf:cov_p", "coverage_p.txt"),
        ("//inf:cov_presence_read", "coverage_presence_read.txt"),
    ] {
        let output = ws.run(target);
        assert!(
            output.status.success(),
            "{target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            fs::read_to_string(
                manifest()
                    .join("tests/fixtures/instrumented_files")
                    .join(expected)
            )
            .unwrap(),
            "{target}"
        );
    }
    let output = ws.run("//inf:cov");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
