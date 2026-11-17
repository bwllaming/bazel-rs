//! config_setting(values = {...}) on repeatable String options: `--features`
//! (normalized: disables win, sorted), `--host_features`, `--copt`
//! (ConfigSetting.optionMatches' multi-value list case,
//! rules/config/ConfigSetting.java:572-579: the expected value is one
//! element, matched by containment). Every case of
//! tests/fixtures/config_setting_features/reference_receipt.json — genuine
//! Bazel 9.3.1 (`--batch --ignore_all_rc_files`, a private output user root)
//! on the fixture workspace — runs natively and is compared with the
//! capture: the exit code, the analysis DEBUG lines (sorted) and the error.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/config_setting_features/reference_receipt.json"))
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

fn check(case: &str) {
    let receipt = receipt();
    let entry = &receipt["cases"][case];
    let expected = &entry["genuine"];
    let argv: Vec<&str> = entry["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    let temp = tempfile::Builder::new()
        .prefix("config-setting-features-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws = temp.path().join("ws");
    copy(
        &manifest().join("tests/fixtures/config_setting_features/workspace"),
        &ws,
    );
    let mut args = vec![argv[0], EXPLICIT_MODULES];
    args.extend(&argv[1..]);
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(&args)
        .current_dir(&ws)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).replace(ws.to_str().unwrap(), "<WS>");
    assert_eq!(
        output.status.code(),
        expected["exit"].as_i64().map(|code| code as i32),
        "{case}: {stderr}"
    );
    let mut debug: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("DEBUG: "))
        .map(|line| {
            line.split_once(' ')
                .map_or(line, |(_, rest)| rest)
                .to_owned()
        })
        .collect();
    debug.sort();
    let want: Vec<String> = expected["debug_sorted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(debug, want, "{case}");
    for error in expected["errors"].as_array().unwrap() {
        // Genuine's `ERROR: ` prefix is bazel-rs's `bazel-rs: `.
        let message = error.as_str().unwrap().strip_prefix("ERROR: ").unwrap();
        assert!(
            stderr.contains(message),
            "{case}: missing {message:?} in {stderr}"
        );
    }
}

#[test]
fn receipt_cases_are_covered() {
    let receipt = receipt();
    let mut cases: Vec<&String> = receipt["cases"].as_object().unwrap().keys().collect();
    cases.sort();
    assert_eq!(cases, ["defaults", "flags", "no_match", "normalized"]);
}

#[test]
fn features_copt_and_host_features_match_by_containment() {
    for case in ["defaults", "flags", "normalized"] {
        check(case);
    }
}

#[test]
fn no_matching_condition_fails_like_bazel() {
    check("no_match");
}
