//! `print()` locations like Bazel 9.3 (crate::debug_print; genuine capture
//! in tests/fixtures/print_locations): another repository's `.bzl` file by
//! its path below `<output_base>/external`, and nothing from an `Args.map_each` callback.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

include!("support/module_archives.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/print_locations"
);

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

#[test]
fn prints_name_repository_files_by_path_and_map_each_prints_nothing() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("print-locations-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURE).join("workspace"), &ws);
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(with_builtin_registry(&[
            "build",
            "//:m",
            "//:x",
            "--curses=no",
            "--color=no",
        ]))
        .current_dir(&ws)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .stdin(Stdio::null())
        .seeded_archives()
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let info = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(with_builtin_registry(&["info", "output_base"]))
        .current_dir(&ws)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .stdin(Stdio::null())
        .seeded_archives()
        .output()
        .unwrap();
    let output_base = String::from_utf8_lossy(&info.stdout).trim().to_owned();
    assert!(!output_base.is_empty());
    let mut observed = vec![format!("rc={}", output.status.code().unwrap())];
    observed.extend(
        stderr
            .lines()
            .filter(|line| line.starts_with("DEBUG: "))
            .map(|line| line.replace(&output_base, "<OUTPUT_BASE>")),
    );
    let genuine: Vec<String> = fs::read_to_string(Path::new(FIXTURE).join("genuine.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(observed, genuine, "{stderr}");
    // The callbacks ran: the actions' command lines hold their results.
    assert!(!stderr.contains("map_each"), "{stderr}");
}
