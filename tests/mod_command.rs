//! `mod` against the genuine Bazel 9.3.0 capture
//! tests/fixtures/mod_command/reference_receipt.json (capture.py there):
//! stdout byte for byte, the exit code and the ERROR events, for every
//! subcommand and display option on one module graph with a selection
//! upgrade, overrides, a cycle, an extension and a `use_repo_rule`. The
//! genuine capture resolved the built-in closure from the public BCR; this
//! test resolves the same closure from the offline built-in registry
//! (tests/fixtures/builtin_modules), so it never touches the network.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mod_command")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Workspace {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("mod-command-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for part in ["registry", "sources", "ws"] {
            copy_tree(&fixture().join(part), &root.join(part));
        }
        Self {
            _directory: directory,
            root,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new("timeout")
            .arg("600s")
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.root.join("cache").display()))
            .arg("mod")
            .arg(format!(
                "--registry=file://{}",
                self.root.join("registry").display()
            ))
            .arg(format!(
                "--registry=file://{}",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/builtin_modules/registry")
                    .display()
            ))
            .arg("--lockfile_mode=off")
            .args(args)
            .current_dir(self.root.join("ws"))
            .env_remove("BAZEL_RS_SNAPSHOT")
            .output()
            .unwrap()
    }
}

/// Cases whose genuine output depends on the genuine installation: the
/// embedded `@bazel_tools` directory (compared up to the path), the
/// `streamed_jsonproto` repository proto (not implemented natively,
/// docs/MOD_COMMAND.md) and a resolver error wording (exit code only).
fn comparison(name: &str) -> &'static str {
    match name {
        "show_repo_builtin" => "builtin_path",
        "show_repo_jsonproto" => "unsupported",
        "graph_ignore_dev" => "exit_code",
        _ => "exact",
    }
}

#[test]
fn mod_subcommands_match_genuine_bazel() {
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let workspace = Workspace::new();
    let fixture_text = workspace.root.display().to_string();
    let mut failures = Vec::new();
    for case in receipt["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let output = workspace.run(&args);
        let stdout = String::from_utf8_lossy(&output.stdout).replace(&fixture_text, "${FIXTURE}");
        let stderr = String::from_utf8_lossy(&output.stderr).replace(&fixture_text, "${FIXTURE}");
        let errors: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with("ERROR: "))
            .collect();
        let expected_errors: Vec<&str> = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|event| event.starts_with("ERROR: "))
            .collect();
        let code = output.status.code().unwrap_or(-1);
        let expected_code = case["exit_code"].as_i64().unwrap() as i32;
        let expected_stdout = case["stdout"].as_str().unwrap();
        let ok = match comparison(name) {
            "exact" => {
                code == expected_code && stdout == expected_stdout && errors == expected_errors
            }
            "builtin_path" => {
                let prefix = "## @bazel_tools:\nBuiltin or overridden repo located at: ";
                code == expected_code
                    && stdout.starts_with(prefix)
                    && expected_stdout.starts_with(prefix)
                    && stdout.ends_with("\n\n")
            }
            "exit_code" => code == expected_code,
            _ => code == 2 && stdout.is_empty(),
        };
        if !ok {
            failures.push(format!(
                "{name} {args:?}: exit {code} (genuine {expected_code})\n--- native stdout\n{stdout}--- genuine stdout\n{expected_stdout}--- native errors {errors:?}\n--- genuine errors {expected_errors:?}\n--- stderr\n{stderr}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn mod_option_errors_fail_before_running() {
    let workspace = Workspace::new();
    let output = workspace.run(&["graph", "--depth=deep"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ERROR: While parsing option --depth=deep: 'deep' is not an int"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
}

/// `mod tidy` twice on tidy_ws/ (receipt `tidy`): the rewritten MODULE.bazel,
/// the INFO events and exit codes. Needs the network: buildozer's module
/// source and its prebuilt binary (`@buildozer_binary` of `@bazel_tools`)
/// are downloaded like genuine does.
#[test]
#[ignore = "downloads buildozer from the public BCR and GitHub"]
fn mod_tidy_matches_genuine_bazel() {
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let directory = tempfile::Builder::new()
        .prefix("mod-tidy-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let workspace = fs::canonicalize(directory.path()).unwrap().join("ws");
    copy_tree(&fixture().join("tidy_ws"), &workspace);
    for (index, expected) in receipt["tidy"].as_array().unwrap().iter().enumerate() {
        let output = Command::new("timeout")
            .arg("1200s")
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                workspace.parent().unwrap().join("cache").display()
            ))
            .args(["mod", "tidy", "--lockfile_mode=off"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let events: Vec<&str> = stderr
            .lines()
            .filter(|line| {
                ["ERROR: ", "WARNING: ", "INFO: "]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .collect();
        let expected_events: Vec<&str> = expected["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            output.status.code(),
            expected["exit_code"].as_i64().map(|code| code as i32),
            "run {index}: {stderr}"
        );
        assert_eq!(events, expected_events, "run {index}: {stderr}");
        assert_eq!(
            fs::read_to_string(workspace.join("MODULE.bazel")).unwrap(),
            expected["module"].as_str().unwrap(),
            "run {index}"
        );
    }
}
