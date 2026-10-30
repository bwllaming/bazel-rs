//! `native.bazel_version` flows into MODULE.bazel.lock like genuine Bazel
//! 9.3.1's (the compatibility target): an extension loading a .bzl whose bytes
//! hold the release (bazel_features' `@bazel_features_version//:version.bzl`
//! shape) records the same bzlTransitiveDigest, so a lockfile written by
//! genuine 9.3.1 stays unchanged under bazel-rs. Expected bytes:
//! tests/fixtures/bazel_version_digest/reference_receipt.json (capture.py
//! there; genuine 9.3.1, with 9.3.0's lockfile for contrast). Uses the local
//! builtin-module fixture registry only; never a reference Bazel.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bazel_version_digest"
);
const REGISTRY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/builtin_modules/registry"
);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn receipt() -> serde_json::Value {
    serde_json::from_slice(&fs::read(Path::new(FIXTURE).join("reference_receipt.json")).unwrap())
        .unwrap()
}

fn query(root: &Path, mode: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", root.join("cache").display()))
        .arg("query")
        .arg(format!("--registry=file://{REGISTRY}"))
        .arg(format!("--lockfile_mode={mode}"))
        .arg("--repository_disable_download")
        .arg("@probe//:all")
        .current_dir(root.join("ws"))
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("LC_ALL", "C.UTF-8")
        .explicit_modules()
        .output()
        .unwrap()
}

#[test]
fn lockfile_digest_of_version_dependent_bzl_equals_genuine_931() {
    let receipt = receipt();
    let genuine = &receipt["references"][bazel_rs::COMPATIBILITY_VERSION];
    assert_eq!(genuine["binary"], "bazel-9.3.1-linux-x86_64");
    let expected = genuine["lockfile"].as_str().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("bazel-version-digest-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    copy_tree(
        &Path::new(FIXTURE).join("workspace"),
        &dir.path().join("ws"),
    );
    for name in ["home", "tmp"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let fresh = query(dir.path(), "update");
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&fresh.stdout),
        genuine["stdout"].as_str().unwrap()
    );
    let written = fs::read_to_string(dir.path().join("ws/MODULE.bazel.lock")).unwrap();
    assert_eq!(written, expected);
    // The committed genuine lockfile is up to date for bazel-rs: error mode
    // accepts it and nothing is rewritten.
    let checked = query(dir.path(), "error");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("ws/MODULE.bazel.lock")).unwrap(),
        expected
    );
    // 9.3.0's lockfile differs exactly in that digest and the recorded value.
    let older = receipt["references"]["9.3.0"]["lockfile"].as_str().unwrap();
    let differing: Vec<(&str, &str)> = older
        .lines()
        .zip(expected.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(differing.len(), 2, "{differing:?}");
    assert!(differing[0].0.contains("\"bzlTransitiveDigest\""));
    assert_eq!(
        differing[1],
        (
            "              \"version\": \"9.3.0\"",
            "              \"version\": \"9.3.1\""
        )
    );
}

#[test]
fn genuine_931_lockfile_is_reused_without_rewrite() {
    let receipt = receipt();
    let expected = receipt["references"]["9.3.1"]["lockfile"].as_str().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("bazel-version-digest-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    copy_tree(
        &Path::new(FIXTURE).join("workspace"),
        &dir.path().join("ws"),
    );
    for name in ["home", "tmp"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    fs::write(dir.path().join("ws/MODULE.bazel.lock"), expected).unwrap();
    for mode in ["error", "update"] {
        let output = query(dir.path(), mode);
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("ws/MODULE.bazel.lock")).unwrap(),
            expected,
            "{mode}"
        );
    }
}
