//! Small genuine-captured build/cquery fixture. The repository callback uses
//! regex_match's bool to create namespace files, not merely to observe a call.
#![allow(clippy::disallowed_methods)]

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

include!("support/explicit_modules.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/py_regex_match")
}

fn receipt() -> Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
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
fn fixture_bytes_are_the_genuine_capture() {
    let root = fixture().join("workspace");
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
        receipt()["fixture_files_sha256"]
    );
}

#[test]
fn build_and_cquery_consume_real_repository_namespace_files() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    copy(&fixture().join("workspace"), ws);
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
    let run = |args: &[&str]| {
        let out = Command::new("timeout")
            .args([
                "--kill-after=5s",
                "120s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .arg(&registry)
            .args([
                "--lockfile_mode=off",
                "--remote_cache=",
                "--remote_executor=",
                "--noremote_upload_local_results",
            ])
            .current_dir(ws)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let result = run(&[
        "build",
        "//tools/build_defs/python:probe",
        "//tools/build_defs/python:repo",
    ]);
    let paths: Vec<_> = result
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, path)| PathBuf::from(path)))
        .collect();
    assert!(!paths.is_empty(), "{result}");
    let expected = receipt();
    let probe = paths
        .iter()
        .find(|path| path.ends_with("probe.txt"))
        .unwrap();
    assert_eq!(
        fs::read_to_string(probe).unwrap(),
        expected["build_output"].as_str().unwrap()
    );
    let accepted = paths
        .iter()
        .find(|path| path.ends_with("accepted.txt"))
        .unwrap();
    assert_eq!(
        fs::read_to_string(accepted).unwrap(),
        expected["repo_output"].as_str().unwrap()
    );
    let inits: Vec<_> = paths
        .iter()
        .filter(|path| path.ends_with("__init__.py"))
        .collect();
    assert_eq!(inits.len(), 3, "{paths:?}");
    for init in inits {
        assert_eq!(fs::read_to_string(init).unwrap(), "# namespace\n");
    }
    let queried = run(&[
        "cquery",
        "deps(//tools/build_defs/python:repo)",
        "--output=label",
    ]);
    let labels = |output: &str| {
        let mut labels: Vec<_> = output
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(|label| {
                label
                    .replace("@@+_repo_rules+namespaces//", "@namespaces//")
                    // Existing use_repo_rule canonical-name difference, not
                    // a regex semantic difference. Compare apparent labels.
                    .replace("@@+namespace_repo+namespaces//", "@namespaces//")
                    .replace("@@namespaces//", "@namespaces//")
                    .replace("@@", "@")
            })
            .collect();
        labels.sort();
        labels
    };
    assert_eq!(
        labels(&queried),
        labels(expected["cases"]["cquery"]["stdout"].as_str().unwrap())
    );
    fs::create_dir_all(ws.join("bad")).unwrap();
    fs::write(
        ws.join("bad/BUILD.bazel"),
        "x = py_internal.regex_match(\"a\", \"a\")\n",
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
