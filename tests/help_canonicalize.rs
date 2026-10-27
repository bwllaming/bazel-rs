//! `help` and `canonicalize-flags` against genuine Bazel 9.3.0 captures
//! (tests/fixtures/help, tests/fixtures/canonicalize; capture.py there).
//! `help`: every topic's text and option listing, line for line except the
//! release header (blank lines not compared); `canonicalize-flags`: stdout,
//! exit code and ERROR events.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
include!("support/temp_root.rs");
use std::path::Path;
use std::process::Command;

fn receipt(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .join("reference_receipt.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

#[test]
fn help_topics_match_genuine_bazel() {
    let receipt = receipt("help");
    let lines = strings(&receipt["lines"]);
    let directory = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
    let mut failures = Vec::new();
    for case in receipt["cases"].as_array().unwrap() {
        let args = strings(&case["args"]);
        let info_keys = args == ["info-keys"];
        let mut command = vec!["help"];
        command.extend(&args);
        let output = run(&command, directory.path());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let skip = usize::from(!info_keys);
        let native: Vec<&str> = stdout
            .split('\n')
            .skip(skip)
            .filter(|l| !l.is_empty())
            .collect();
        let expected: Vec<&str> = match case.get("template") {
            Some(template) => template
                .as_str()
                .unwrap()
                .split('\n')
                .skip(1)
                .filter(|line| !line.is_empty())
                .chain(
                    case["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|index| lines[index.as_u64().unwrap() as usize]),
                )
                .collect(),
            None => case["stdout"]
                .as_str()
                .unwrap()
                .split('\n')
                .skip(skip)
                .filter(|line| !line.is_empty())
                .collect(),
        };
        let native = if args.is_empty() {
            native[..expected.len().min(native.len())].to_vec()
        } else {
            native
        };
        let errors: Vec<String> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| line.starts_with("ERROR: "))
            .map(str::to_owned)
            .collect();
        if native != expected
            || output.status.code() != case["exit_code"].as_i64().map(|code| code as i32)
            || errors != strings(&case["errors"])
        {
            failures.push(format!("{args:?}: {errors:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn canonicalize_flags_matches_genuine_bazel() {
    let receipt = receipt("canonicalize");
    let directory = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
    std::fs::write(directory.path().join("MODULE.bazel"), "").unwrap();
    for case in receipt["cases"].as_array().unwrap() {
        let mut command = vec!["canonicalize-flags", "--lockfile_mode=off"];
        command.extend(strings(&case["args"]));
        let output = run(&command, directory.path());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let errors: Vec<&str> = stderr
            .lines()
            .filter(|l| l.starts_with("ERROR: "))
            .collect();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            case["stdout"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(
            output.status.code(),
            case["exit_code"].as_i64().map(|code| code as i32),
            "{}: {stderr}",
            case["name"]
        );
        assert_eq!(errors, strings(&case["errors"]), "{}", case["name"]);
    }
}
