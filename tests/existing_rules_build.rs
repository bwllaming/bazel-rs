//! `native.existing_rule(name)` and `native.existing_rules()` during BUILD
//! evaluation (docs/EXISTING_RULES.md; StarlarkNativeModule.java:320-521 at
//! Bazel 9.3.1). Every case of
//! tests/fixtures/existing_rules_build/reference_receipt.json — genuine Bazel
//! 9.3.1 `query //<case>:all` (`--batch --ignore_all_rc_files`, a private
//! output user root) on the fixture workspace — runs natively here and is
//! compared with the capture: success, the query's labels, the DEBUG lines
//! (the views' types, reprs, keys and exported attribute values, snapshot
//! sizes) and Bazel's error message.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/existing_rules_build/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The DEBUG lines without their `DEBUG: <file>:L:C: ` prefix.
fn debug_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("DEBUG: "))
        .map(|line| {
            line.split_once(' ')
                .map_or(line, |(_, rest)| rest)
                .to_owned()
        })
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

fn check(case: &str) {
    let receipt = receipt();
    let expected = &receipt["cases"][case]["genuine"];
    let temp = tempfile::Builder::new()
        .prefix("existing-rules-build-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws: PathBuf = temp.path().join("ws");
    copy(
        &manifest().join("tests/fixtures/existing_rules_build/workspace"),
        &ws,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(["query", &format!("//{case}:all")])
        .current_dir(&ws)
        .explicit_modules()
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).replace(ws.to_str().unwrap(), "<WS>");
    let success = expected["exit"].as_i64() == Some(0);
    assert_eq!(output.status.success(), success, "{case}: {stderr}");
    let debug = debug_lines(&stderr);
    let want = strings(&expected["debug"]);
    if success {
        assert_eq!(
            stdout.lines().map(str::to_owned).collect::<Vec<_>>(),
            strings(&expected["stdout"]),
            "{case}"
        );
        assert_eq!(debug, want, "{case}");
    } else {
        // DECLARED_DIVERGENCE: a failing package's DEBUG lines may repeat
        // natively (the failed package is reported twice); the first ones
        // are genuine's.
        assert!(debug.len() >= want.len(), "{case}: {stderr}");
        assert_eq!(debug[..want.len()], want[..], "{case}");
        for error in strings(&expected["errors"]) {
            assert!(
                stderr.contains(&error),
                "{case}: missing {error:?} in {stderr}"
            );
        }
    }
}

/// The receipt's cases, each run by exactly one test below.
const CASES: &[&str] = &[
    "pkg",
    "pv",
    "methods",
    "dict_copy",
    "misc",
    "hashable",
    "sym_macro",
    "sym_legacy",
    "bzl_toplevel",
    "assign",
    "missing_key",
    "frozen_keys",
    "bad_name",
];

#[test]
fn receipt_cases_are_covered() {
    let receipt = receipt();
    let mut cases: Vec<&str> = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    cases.sort_unstable();
    let mut covered = CASES.to_vec();
    covered.sort_unstable();
    assert_eq!(cases, covered);
}

#[test]
fn views_attributes_selects_and_snapshots() {
    check("pkg");
}

#[test]
fn package_defaults_labels_and_select_branches() {
    check("pv");
}

#[test]
fn view_methods_dict_copies_and_hashing() {
    for case in ["methods", "dict_copy", "misc", "hashable"] {
        check(case);
    }
}

#[test]
fn symbolic_macros_and_bzl_initialization_reject() {
    for case in ["sym_macro", "sym_legacy", "bzl_toplevel"] {
        check(case);
    }
}

#[test]
fn view_mutation_and_lookup_errors() {
    for case in ["assign", "missing_key", "frozen_keys", "bad_name"] {
        check(case);
    }
}
