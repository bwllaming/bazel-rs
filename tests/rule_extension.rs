//! B17 (docs/OSS_CORPUS.md): `rule(parent = ...)` rule extension —
//! inherited attribute schemas and overrides, the initializer chain (child
//! to ancestor), `ctx.super()`, the extend-rule allowlist and genuine's
//! error texts. Expected values: genuine Bazel 9.3.1's, captured in
//! tests/fixtures/rule_extension/reference_receipt.json over the fixture
//! workspace beside it.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/module_archives.rs");

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/temp_root.rs");

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rule_extension")
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("rule-extension-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&fixture().join("workspace"), ws.0.path());
        // Offline: the genuine registry fixture, and the @platforms archive
        // that execution platform resolution materializes.
        seed_module_archives(&ws.0.path().join("private-cache"), &["platforms@1.0.0"]);
        ws
    }

    fn build(&self, targets: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("private-cache").display()
            ))
            .arg("build")
            .arg("--experimental-builtin-modules")
            .arg(builtin_registry_flag())
            .arg("--lockfile_mode=off")
            .args(targets)
            .current_dir(self.0.path())
            .env_remove("BAZEL_RS_RESULT_LINES")
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "timed out: {targets:?}");
        output
    }
}

fn receipt() -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(fixture().join("reference_receipt.json")).unwrap())
        .unwrap()
}

#[test]
fn extended_rules_inherit_initialize_and_call_super_like_bazel() {
    let receipt = receipt();
    let ws = Workspace::new();
    let output = ws.build(&["//ok:use", "//ok:c", "//ok:g"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        receipt["build"]["exit"].as_i64().map(|code| code as i32),
        "{stderr}"
    );
    let prefix = format!("{}/", ws.0.path().display());
    let mut prints: Vec<String> = stderr
        .lines()
        .filter(|line| line.contains("RX|"))
        .map(|line| {
            line.split_once(": ")
                .map_or(line, |(_, rest)| rest)
                .replace(&prefix, "")
        })
        .collect();
    prints.sort();
    let expected: Vec<String> = receipt["build"]["prints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(prints, expected, "{stderr}");
}

#[test]
fn rule_extension_errors_match_bazel() {
    let receipt = receipt();
    let ws = Workspace::new();
    for (case, expected) in receipt["errors"]["cases"].as_object().unwrap() {
        let target = format!("//errs/{case}:t");
        let output = ws.build(&[&target]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            expected["exit"].as_i64().map(|code| code as i32),
            "{case}: {stderr}"
        );
        for message in expected["messages"].as_array().unwrap() {
            let message = message.as_str().unwrap();
            assert!(
                stderr.contains(message),
                "{case}: {message:?} not in\n{stderr}"
            );
        }
    }
}
