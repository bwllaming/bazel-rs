//! The command line test-setup.sh gets (B53 investigation,
//! docs/OSS_CORPUS.md): TestStrategy.expandedArgsFromAction passes the test
//! executable's runfiles path as PathFragment.getCallablePathString, so a
//! test of the main repository's root package runs as `./root_test` (like
//! its TEST_BINARY). Replays the genuine Bazel 9.3.1 capture
//! tests/fixtures/test_exec_env/reference_receipt.json (capture.py beside it).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_exec_env")
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &destination)
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn test_setup_gets_the_callable_runfiles_path() {
    let reference: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let workspace = temp.path().join("ws");
    copy(&fixture().join("workspace"), &workspace);
    for directory in ["home", "tmp"] {
        fs::create_dir(workspace.join(directory)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", workspace.join("home"))
        .env("TMPDIR", workspace.join("tmp"))
        .env("BAZEL_RS_CACHE_GC", "off")
        .args(["--ignore_all_rc_files", "--cache-dir=cache", "test"])
        .args(["--lockfile_mode=off", "--memory-limit=2048"])
        .args(strings(&reference["flags"]))
        .args(strings(&reference["targets"]))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<String> = stdout
        .lines()
        .filter(|line| line.starts_with("SETUP_ARGS ") || line.starts_with("TEST_BINARY "))
        .map(str::to_owned)
        .collect();
    lines.sort();
    assert_eq!(
        output.status.code().map(i64::from),
        reference["exit"].as_i64(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(lines, strings(&reference["lines"]), "stdout:\n{stdout}");
}
