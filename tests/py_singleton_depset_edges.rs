//! Real loading, repository, rule and provider consumers, compared with the
//! pinned genuine ELF's output (not Rust-computed cardinality expectations).
#![allow(clippy::disallowed_methods)]

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/py_singleton_depset_edges")
}

fn copy(source: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

#[test]
fn genuine_singleton_results_drive_rule_provider_and_repository_outputs() {
    let root = fixture().join("workspace");
    let receipt: Value =
        serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap())
            .unwrap();
    let mut stack = vec![root.clone()];
    let mut files = BTreeMap::new();
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(
                    path.strip_prefix(&root).unwrap().display().to_string(),
                    format!("{:x}", Sha256::digest(fs::read(&path).unwrap())),
                );
            }
        }
    }
    assert_eq!(
        serde_json::to_value(files).unwrap(),
        receipt["fixture_files_sha256"]
    );
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    copy(&root, ws);
    // Existing native BUILD-global gap: depset is only installed in .bzl.
    // Import that same constructor, without changing the captured calls,
    // inputs, outputs or helper's BUILD availability. Do not expand globals
    // as part of this Python-helper change.
    let defs = ws.join("tools/build_defs/python/defs.bzl");
    fs::write(
        &defs,
        format!(
            "{}\nbuild_depset = depset\n",
            fs::read_to_string(&defs).unwrap()
        ),
    )
    .unwrap();
    let build = ws.join("tools/build_defs/python/BUILD.bazel");
    fs::write(
        &build,
        format!(
            "load(\":defs.bzl\", depset = \"build_depset\")\n{}",
            fs::read_to_string(&build).unwrap()
        ),
    )
    .unwrap();
    let platforms: Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/label_repo_name/platforms_archive.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(platforms["bytes_base64"].as_str().unwrap())
        .unwrap();
    tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
        .unpack(ws.join("platforms"))
        .unwrap();
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let out = Command::new("timeout")
        .args([
            "--kill-after=5s",
            "120s",
            env!("CARGO_BIN_EXE_bazel-rs"),
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            "build",
            "//tools/build_defs/python:probe",
            "--lockfile_mode=off",
            "--remote_cache=",
            "--remote_executor=",
            "--noremote_upload_local_results",
        ])
        .arg(&registry)
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let paths: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, p)| PathBuf::from(p)))
        .collect();
    for (suffix, expected) in [("probe.txt", "build_output"), ("result.txt", "repo_output")] {
        let path = paths.iter().find(|p| p.ends_with(suffix)).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            receipt[expected].as_str().unwrap(),
            "{suffix}"
        );
    }
    // The pure helper is not a new BUILD global: existing availability and
    // namespace guards remain in force.
    fs::create_dir(ws.join("bad")).unwrap();
    fs::write(
        ws.join("bad/BUILD.bazel"),
        "x = py_internal.is_singleton_depset(depset())\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args([
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            "query",
            "//bad:all",
            "--lockfile_mode=off",
        ])
        .arg(&registry)
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("py_internal"));
}
