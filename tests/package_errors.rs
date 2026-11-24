//! A package with errors (Package.containsErrors): a BUILD file that fails
//! part-way keeps the targets it declared before the failure. `query`
//! patterns and visitations see them — the failure an error event, a
//! pattern over the package's targets reporting `package contains errors`,
//! an edge to one of them `Target 'T' contains an error and its package is
//! in error` (the target visited all the same) — and the query fails at
//! its end (exit 7) or, with `--keep_going`, prints its partial result
//! (exit 3). `build` and `cquery` fail such targets as before. gRPC's
//! examples/objective-c/BUILD (a removed `objc_library` after ten targets)
//! was the OSS corpus case. Every case replays the genuine Bazel 9.3.1
//! capture tests/fixtures/package_errors/reference_receipt.json (written by
//! capture.py beside it): the exit code, stdout, and for `query` every
//! error message genuine prints but its Starlark traceback.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/package_errors")
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

/// A path ending in `/ws` (the capture's scratch workspace) as `<WS>`.
fn without_workspace(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("/ws/") {
        let start = rest[..at]
            .rfind(['"', ' ', '\''])
            .map_or(0, |quote| quote + 1);
        out.push_str(&rest[..start]);
        out.push_str("<WS>/");
        rest = &rest[at + 4..];
    }
    out.push_str(rest);
    out
}

/// The message of a genuine stderr line both tools print: without the
/// event prefix, a `Skipping 'P': ` or `error loading package 'P': `
/// context and the Starlark traceback that follows `: Traceback` (native
/// formats it differently). `None`: a traceback line only.
fn core(line: &str) -> Option<String> {
    let mut rest = line.strip_prefix("ERROR: ")?;
    for prefix in ["Skipping '", "error loading package '"] {
        if let Some(tail) = rest.strip_prefix(prefix)
            && let Some(end) = tail.find("': ")
        {
            rest = &tail[end + 3..];
        }
    }
    let rest = rest.split(": Traceback").next().unwrap_or(rest);
    (!rest.starts_with("Traceback")).then(|| rest.to_owned())
}

/// Cases whose difference is not about packages with errors: a package
/// whose `load()` fails is NoSuchPackage in Bazel (skipped under
/// `--keep_going`, exit 3/7); native's query fails such a single-package
/// pattern (exit 1).
const LOAD_FAILURES: [&str; 2] = ["query_all", "query_loadfail_keep_going"];

#[test]
fn packages_with_errors_match_genuine() {
    let reference: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for (name, case) in reference["cases"].as_object().unwrap() {
        if LOAD_FAILURES.contains(&name.as_str()) {
            continue;
        }
        let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
        let workspace = temp.path().join("ws");
        copy(&fixture().join("workspace"), &workspace);
        fs::create_dir(temp.path().join("home")).unwrap();
        let command = case["command"].as_str().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", temp.path().join("home"))
            .env("TMPDIR", temp.path().join("home"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .args(["--ignore_all_rc_files", "--cache-dir=cache", command])
            .args(["--lockfile_mode=off", "--memory-limit=2048"])
            .args(strings(&case["flags"]))
            .args(strings(&case["targets"]))
            .output()
            .unwrap();
        let stderr =
            String::from_utf8_lossy(&output.stderr).replace(workspace.to_str().unwrap(), "<WS>");
        let mut problems = Vec::new();
        if output.status.code() != case["exit"].as_i64().map(|code| code as i32) {
            problems.push(format!(
                "exit {:?}, genuine {}",
                output.status.code(),
                case["exit"]
            ));
        }
        let stdout = without_workspace(
            &String::from_utf8_lossy(&output.stdout).replace(workspace.to_str().unwrap(), "<WS>"),
        );
        let expected = without_workspace(case["stdout"].as_str().unwrap());
        if stdout != expected {
            problems.push(format!("stdout {stdout:?}, genuine {expected:?}"));
        }
        if command == "query" {
            for line in strings(&case["messages"]) {
                if let Some(message) = core(&line)
                    && !stderr.contains(&message)
                {
                    problems.push(format!("missing {message:?}"));
                }
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{name}: {}\n{stderr}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
