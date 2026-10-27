//! `py_internal.is_singleton_depset` (PyBuiltins.isSingletonDepset,
//! NestedSet.isSingleton) against the paired genuine Bazel 9.3.0 capture of
//! tests/fixtures/py_singleton_depset.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

include!("support/explicit_modules.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/py_singleton_depset")
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
fn is_singleton_depset_follows_bazel() {
    let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
    let ws = temp.path();
    copy(&fixture().join("workspace"), ws);
    let run = |target: &str| -> Output {
        Command::new("timeout")
            .args([
                "--kill-after=5s",
                "120s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
            ])
            .arg(target)
            .current_dir(ws)
            .explicit_modules()
            .output()
            .unwrap()
    };
    let output = run("//tools/build_defs/python:probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let probe = stdout
        .lines()
        .find_map(|line| line.split_once('\t').map(|(_, path)| PathBuf::from(path)))
        .unwrap();
    assert_eq!(
        fs::read_to_string(probe).unwrap(),
        fs::read_to_string(fixture().join("genuine_probe.txt")).unwrap()
    );
    for (target, error) in [
        (
            "wrong_type",
            "in call to is_singleton_depset(), parameter 'value' got value of type 'list', want 'depset'",
        ),
        (
            "named",
            "is_singleton_depset() got named argument for positional-only parameter 'value'",
        ),
    ] {
        let output = run(&format!("//tools/build_defs/python:{target}"));
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(error), "{stderr}");
    }
}
