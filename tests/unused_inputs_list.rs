//! `ctx.actions.run(unused_inputs_list = ...)` (docs/UNUSED_INPUTS_LIST.md).
//! Every case of tests/fixtures/unused_inputs_list/reference_receipt.json —
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace, in order, with the recorded
//! source edits — runs natively here and is compared with the genuine
//! capture: the same outputs (also after an unused and a used input change)
//! and Bazel's Starlark binding error. The process counts differ by design
//! after an unused input changes (no input pruning natively).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/unused_inputs_list")
}

fn receipt() -> Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
}

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

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("unused-inputs-list-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture().join("workspace"), &ws.ws());
        ws
    }

    fn ws(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    fn run(&self, command: &str) -> Output {
        let mut words = command.split(' ');
        let verb = words.next().unwrap();
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("cache").display()
            ))
            .arg(verb)
            .args(["--experimental-builtin-modules", &registry])
            .args(words)
            .current_dir(self.ws())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {command}");
        output
    }

    /// Native stderr with the workspace as `<WS>`.
    fn stderr(&self, output: &Output) -> String {
        let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
        let ws = self.ws();
        for path in [ws.clone(), fs::canonicalize(&ws).unwrap()] {
            text = text.replace(&path.display().to_string(), "<WS>");
        }
        text
    }

    /// The native `LABEL\tPATH` result lines as sorted [label, contents].
    fn outputs(&self, output: &Output) -> Vec<(String, String)> {
        let mut outputs: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(label, path)| {
                let path = self.ws().join(path);
                (
                    label.replace("@@//", "//"),
                    fs::read_to_string(path).unwrap(),
                )
            })
            .collect();
        outputs.sort();
        outputs
    }
}

#[test]
fn every_case_matches_the_genuine_capture() {
    let receipt = receipt();
    let ws = Workspace::new();
    let mut failures = Vec::new();
    for (name, case) in receipt["cases"].as_object().unwrap() {
        // The capture's source edits, before the case (cases run in order).
        for (file, text) in case["writes"].as_object().unwrap() {
            fs::write(ws.ws().join(file), text.as_str().unwrap()).unwrap();
        }
        let command = case["command"].as_str().unwrap();
        let genuine = &case["genuine"];
        let verdict = case["verdict"].as_str().unwrap();
        let output = ws.run(command);
        let stderr = ws.stderr(&output);
        let ok = output.status.success();
        let problem = match verdict {
            "EXACT" if !ok => Some("native failed".to_owned()),
            "EXACT" => {
                let expected: Vec<(String, String)> = genuine["outputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pair| {
                        (
                            pair[0].as_str().unwrap().to_owned(),
                            pair[1].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect();
                let actual = if command.contains("--nobuild") {
                    Vec::new()
                } else {
                    ws.outputs(&output)
                };
                (expected != actual).then(|| format!("outputs differ: {expected:?} vs {actual:?}"))
            }
            "EXACT_MESSAGE" if ok => Some("native succeeded".to_owned()),
            "EXACT_MESSAGE" => {
                let message = genuine["message"].as_str().unwrap();
                (!stderr.contains(message))
                    .then(|| format!("genuine message {message:?} not in native stderr"))
            }
            other => Some(format!("unexpected verdict {other}")),
        };
        if let Some(problem) = problem {
            failures.push(format!("{name} ({command}): {problem}\n{stderr}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
