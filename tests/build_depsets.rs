//! `depset` is a BUILD-file global and a depset converts to a list-typed
//! attribute's value (B27): StarlarkGlobalsImpl.addCommonUtilToplevels puts
//! Depset.DepsetLibrary in BUILD files, and Type.ListType.convert takes a
//! depset's `to_list()` (packages/Type.java:731-738) — directly or as a
//! `select()` branch, for native and Starlark rules (OpenROAD's
//! `@openssl//:BUILD.bazel` uses `select({...: depset([...])})`). Other
//! types keep rejecting a depset, and `list + select of depset` and
//! `depset + list` fail as in Bazel.
//!
//! Pinned by a genuine capture (tests/fixtures/build_depsets:
//! reference_receipt.json, genuine 9.3.1 `query --output=build //:all` and
//! the failing packages' messages).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
include!("support/temp_root.rs");
use std::path::Path;
use std::process::Command;

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn depsets_in_build_files_convert_to_list_attributes() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_depsets");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("reference_receipt.json")).unwrap())
            .unwrap();
    let root = tempfile::Builder::new()
        .prefix("build-depsets-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let ws = root.path().join("ws");
    copy_dir(&fixture.join("workspace"), &ws);
    let cache = root.path().join("cache");
    let query = |pattern: &str, build: bool| {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .arg("query");
        if build {
            command.arg("--output=build");
        }
        command.arg(pattern).current_dir(&ws).output().unwrap()
    };
    let workspace = fs::canonicalize(&ws).unwrap();
    let normalize = |text: &[u8]| {
        String::from_utf8_lossy(text).replace(workspace.to_str().unwrap(), "WORKSPACE")
    };
    let output = query("//:all", true);
    let stderr = normalize(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        normalize(&output.stdout),
        receipt["query_build"].as_str().unwrap()
    );
    assert!(
        stderr.contains(receipt["debug"].as_str().unwrap()),
        "{stderr}"
    );
    for (package, message) in receipt["errors"].as_object().unwrap() {
        let output = query(&format!("//{package}:all"), false);
        let stderr = normalize(&output.stderr);
        assert!(!output.status.success(), "{package}: {stderr}");
        assert!(
            stderr.contains(message.as_str().unwrap()),
            "{package}: {stderr}"
        );
    }
}
