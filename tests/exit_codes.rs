//! Bazel 9.3's exit codes (crate::exit_code; genuine matrix in
//! tests/fixtures/exit_codes): command-line errors exit 2, remote errors 34,
//! query evaluation errors 7, failed tests 3, no test targets 4, build
//! failures 1, an empty `build` 0.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

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

/// `text` split into shell words (single quotes only, as genuine.txt uses).
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in text.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

#[test]
fn exit_codes_match_bazel() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("exit-codes-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURES).join("build_results/workspace"), &ws);
    let build = ws.join("BUILD");
    let mut text = fs::read_to_string(&build).unwrap();
    text.push_str("my_test(name = \"tf\", code = 1)\n");
    fs::write(&build, text).unwrap();
    let cache = temp.path().join("cache");
    let matrix = fs::read_to_string(Path::new(FIXTURES).join("exit_codes/genuine.txt")).unwrap();
    let mut failures = Vec::new();
    for line in matrix.lines() {
        let (args, code) = line.rsplit_once(" => ").unwrap();
        let expected: i32 = code.parse().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(words(args))
            .args(["--curses=no", "--color=no"])
            .current_dir(&ws)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let code = output.status.code();
        if code != Some(expected) {
            failures.push(format!(
                "{args}: exit {code:?}, genuine {expected}\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
