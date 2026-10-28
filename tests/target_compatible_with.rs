//! `target_compatible_with`, step 1 (Bazel 9.3 IncompatibleTargetChecker
//! .IncompatibleTargetProducer, analysis/constraints/
//! IncompatibleTargetChecker.java:126-227, run after the target platform and
//! the select() keys and before toolchain resolution by
//! DependencyContextProducerWithCompatibilityCheck.java:159-189): the
//! configured labels' ConstraintValueInfo and config_setting results decide
//! compatibility; a compatible target is analyzed and built as usual, an
//! incompatible one is an actionless result whose build/test root policy still
//! fails explicitly (skipping is not modelled). The 11 original divergences here
//! are BUILD-policy evidence, not cquery failures; target_compatibility_cquery.rs
//! verifies configured query's inclusion of these results independently.
//! Every case of tests/fixtures/target_compatible_with/reference_receipt.json
//! — genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace — runs natively here and is
//! compared with the genuine capture: success, Bazel's error message, the
//! top-level outputs with their contents; a declared divergence is a native
//! failure with its recorded message.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/target_compatible_with/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/target_compatible_with/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "compatible",
        &[
            "compatible",
            "compatible_select",
            "compatible_config_setting",
            "compatible_consumer",
            "compatible_nobuild",
            "exec_tool",
            "setting",
            "arm_platform",
            "compatible_default",
        ],
    ),
    (
        "errors",
        &[
            "duplicate",
            "no_match",
            "not_a_constraint",
            "config_setting_error",
        ],
    ),
    (
        "declared",
        &[
            "incompatible",
            "incompatible_nobuild",
            "incompatible_two",
            "incompatible_config_setting",
            "incompatible_consumer",
            "wildcard",
            "incompatible_marker",
            "incompatible_default",
            "incompatible_mixed",
            "incompatible_sorted",
            "incompatible_missing_deps",
        ],
    ),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("target-compatible-with-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture(), ws.path());
        extract_platforms(&ws.path().join("platforms"));
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`.
    fn run(&self, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }

    /// Native stderr with the workspace path as `<WS>`, like the receipt.
    fn stderr(&self, output: &Output) -> String {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut paths = vec![self.path().display().to_string()];
        if let Ok(real) = fs::canonicalize(self.path()) {
            paths.push(real.display().to_string());
        }
        for path in paths {
            stderr = stderr.replace(&path, "<WS>");
        }
        stderr
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

/// The genuine platforms 1.0.0 module archive (tests/exec_transition.rs).
fn extract_platforms(to: &Path) {
    let manifest: Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/label_repo_name/platforms_archive.json"))
            .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = to.join(entry.path().unwrap());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&path).unwrap();
            continue;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// The native `LABEL\tPATH` result lines as [label, path below the
/// published outputs, contents], sorted like the receipt's.
fn outputs(stdout: &str) -> Vec<Value> {
    let mut found = stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, path)| {
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            serde_json::json!([label, relative, fs::read_to_string(path).ok()])
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|row| row.to_string());
    found
}

fn check(name: &str, case: &Value) {
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let ws = Workspace::new();
    let output = ws.run(&argv);
    let stderr = ws.stderr(&output);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stderr.contains("panicked"), "{name}: {stderr}");
    if case["verdict"] == "DECLARED_DIVERGENCE" {
        assert!(!output.status.success(), "{name}: {stderr}");
        let message = case["native_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        // Genuine's incompatibility reason (the text after `<--` of its
        // dependency chain, TopLevelConstraintSemantics
        // .formatIncompatibilityReasons) is the native message's, verbatim.
        if let Some(reason) = case["genuine_reason"].as_str() {
            let expected = format!("is incompatible with its target platform:{reason};");
            assert!(
                stderr.contains(&expected),
                "{name}: missing {expected:?} in {stderr}"
            );
        }
        return;
    }
    assert_eq!(case["verdict"], "MATCH", "{name}");
    let succeeded = case["genuine"]["exit"].as_i64() == Some(0);
    assert_eq!(
        output.status.success(),
        succeeded,
        "{name} {argv:?}: {stderr}"
    );
    if !succeeded {
        let message = case["expected_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        return;
    }
    if argv.iter().any(|arg| arg == "--nobuild") {
        return;
    }
    let mut expected = case["outputs"].as_array().unwrap().clone();
    expected.sort_by_key(|row| row.to_string());
    assert_eq!(outputs(&stdout), expected, "{name}: {stderr}");
}

fn check_group(group: &str) {
    let receipt = receipt();
    let (_, names) = GROUPS.iter().find(|(name, _)| *name == group).unwrap();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        check(name, case);
    }
}

#[test]
fn receipt_covers_the_fixture_and_every_case_once() {
    let receipt = receipt();
    let mut files = BTreeMap::new();
    let mut stack = vec![fixture()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path.strip_prefix(fixture()).unwrap();
                let digest = Sha256::digest(fs::read(&path).unwrap());
                files.insert(
                    relative.display().to_string(),
                    Value::String(format!("{digest:x}")),
                );
            }
        }
    }
    let recorded = receipt["fixture_files_sha256"].as_object().unwrap();
    assert_eq!(
        files,
        recorded
            .iter()
            .map(|(path, digest)| (path.clone(), digest.clone()))
            .collect::<BTreeMap<_, _>>()
    );
    let mut grouped = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect::<Vec<_>>();
    grouped.sort_unstable();
    let mut cases = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cases.sort_unstable();
    assert_eq!(grouped, cases);
    let declared = GROUPS
        .iter()
        .find(|(group, _)| *group == "declared")
        .unwrap()
        .1;
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let expected = if declared.contains(&name.as_str()) {
            "DECLARED_DIVERGENCE"
        } else {
            "MATCH"
        };
        assert_eq!(case["verdict"], expected, "{name}");
    }
}

/// Compatible targets build as usual: constraint lists, ~/code's select()
/// shape, config_settings and an alias of a constraint value, a genrule, a
/// filegroup, an alias, an exec-configuration tool, a non-host target
/// platform, a build setting (never checked: no toolchain resolution), and
/// a select() whose unmatched default is @platforms//:incompatible.
#[test]
fn compatible_targets_are_analyzed_and_built() {
    check_group("compatible");
}

/// Duplicate constraint settings, a select() without a match, a label
/// without a ConstraintValueInfo or ConfigMatchingProvider and a
/// config_setting whose match is an error (`For config_setting L: ...`)
/// fail with Bazel's messages.
#[test]
fn validation_errors_match_bazel() {
    check_group("errors");
}

/// An incompatible target is an explicit error with Bazel's reason text
/// (genuine skips it in wildcards and rejects it when requested):
/// @platforms//:incompatible directly and as a select() default, an unmet
/// constraint with a non-matching config_setting, both lists sorted and
/// deduplicated, and dependencies on a missing package and an unknown
/// repository that are never analyzed (only the incompatibility is
/// reported).
#[test]
fn incompatible_build_roots_retain_the_documented_skip_policy_boundary() {
    check_group("declared");
}
