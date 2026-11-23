//! B13c (docs/OSS_CORPUS.md): one toolchain implementation resolved for two
//! execution platforms in one configuration. Genuine keys the
//! implementation's configured target by its parent's execution platform
//! (ConfiguredTargetKey.executionPlatformLabel), so both copies exist, each
//! with its own exec-configured tools, actions and outputs; cquery lists two
//! results and aquery two owners' actions. Every case of
//! tests/fixtures/toolchain_exec_key/reference_receipt.json (written by
//! tests/fixtures/toolchain_exec_key/capture.py with genuine Bazel 9.3.1)
//! runs natively and is compared: the exit code, the build outputs below
//! bazel-out with their content, the matching stderr lines and the query
//! stdout with the declared divergences normalized (native configuration
//! checksums, docs/CQUERY.md; result order, docs/AQUERY.md).

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toolchain_exec_key")
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

fn run(workspace: &Path, case: &serde_json::Value, analysis: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
    command
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", workspace.join("home"))
        .env("TMPDIR", workspace.join("tmp"))
        .env("BAZEL_RS_RESULT_LINES", "1")
        .env("BAZEL_RS_CACHE_GC", "off");
    if let Some(analysis) = analysis {
        command.env("BAZEL_RS_ANALYSIS", analysis);
    }
    command
        .arg("--ignore_all_rc_files")
        .args([
            "--cache-dir=cache",
            case["command"].as_str().unwrap(),
            "--lockfile_mode=off",
            "--memory-limit=2048",
        ])
        .args(strings(&case["flags"]))
        .args(strings(&case["targets"]))
        .output()
        .unwrap()
}

/// The configuration checksums (native ConfigIds, docs/CQUERY.md) as `C`.
fn without_checksums(text: &str) -> String {
    let short = fancy_regex::Regex::new(r"\([0-9a-f]{7}\)").unwrap();
    let full = fancy_regex::Regex::new(r"(?m)^(# Configuration: )[0-9a-f]{64}$").unwrap();
    let text = short.replace_all(text, "(C)");
    full.replace_all(&text, "${1}C").into_owned()
}

/// A query's stdout as compared: checksums normalized, then aquery's action
/// blocks (separated by empty lines) or the lines sorted (genuine iterates
/// hash sets: two copies of one label may print in either order,
/// docs/AQUERY.md "Divergences").
fn normalized(command: &str, stdout: &str) -> Vec<String> {
    let text = without_checksums(stdout);
    let mut parts: Vec<String> = if command == "aquery" {
        text.split("\n\n")
            .map(|block| block.trim().to_owned())
            .collect()
    } else {
        text.lines().map(str::to_owned).collect()
    };
    parts.retain(|part| !part.is_empty());
    parts.sort();
    parts
}

fn check(analysis: Option<&str>) {
    let reference: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for (name, case) in reference["cases"].as_object().unwrap() {
        let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
        let workspace = temp.path().join("ws");
        copy(&fixture().join("workspace"), &workspace);
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.join(directory)).unwrap();
        }
        let output = run(&workspace, case, analysis);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let command = case["command"].as_str().unwrap();
        let mut problems = Vec::new();
        if output.status.code() != case["exit"].as_i64().map(|code| code as i32) {
            problems.push(format!(
                "exit {:?}, genuine {}",
                output.status.code(),
                case["exit"]
            ));
        }
        for expected in case["stderr"].as_array().unwrap() {
            let line = expected["line"].as_str().expect("genuine printed the line");
            let line = line.strip_prefix("ERROR: ").unwrap_or(line);
            if !stderr.lines().any(|native| native.contains(line)) {
                problems.push(format!("missing {line:?}"));
            }
        }
        if command == "build" {
            // `label\tpath` result lines: each output's path below bazel-out.
            let mut actual = BTreeMap::new();
            for line in stdout.lines() {
                let Some((_, path)) = line.split_once('\t') else {
                    continue;
                };
                let Ok(content) = fs::read_to_string(path) else {
                    continue;
                };
                let Some((_, relative)) = path.split_once("bazel-out/") else {
                    continue;
                };
                actual.insert(relative.to_owned(), content);
            }
            let expected: BTreeMap<String, String> = case["outputs"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(path, content)| (path.clone(), content.as_str().unwrap().to_owned()))
                .collect();
            if actual != expected {
                problems.push(format!("outputs {actual:?}, genuine {expected:?}"));
            }
        } else {
            let genuine = normalized(command, case["stdout"].as_str().unwrap());
            let native = normalized(command, &stdout);
            if native != genuine {
                problems.push(format!("stdout {native:#?}, genuine {genuine:#?}"));
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{name}: {}\n{stderr}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn toolchain_exec_key_matches_genuine() {
    check(None);
}

/// The sequential walk keys the copies alike.
#[test]
fn toolchain_exec_key_matches_genuine_walk() {
    check(Some("walk"));
}
