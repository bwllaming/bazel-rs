//! Path mapping, `--experimental_output_paths=strip` (docs/PATH_MAPPING.md).
//! Every case of tests/fixtures/path_mapping/reference_receipt.json —
//! genuine Bazel 9.3.1 (tests/fixtures/path_mapping/capture.py) on the
//! fixture workspace, in order, one output base — runs natively here and is
//! compared with the genuine capture: `Target.actions` (argv, inputs,
//! outputs, environment, contents), the outputs of mapped spawns and of
//! mapped parameter-file writes, the collision fallback, the mapped spawn
//! of each configuration (its argv, staged inputs and remote action digest:
//! one disk cache entry for both), the process summaries, aquery's command
//! lines and action keys, and the sandboxing requirement.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/path_mapping")
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
                .prefix("path-mapping-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture().join("workspace"), &ws.ws());
        ws
    }

    fn ws(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    fn run(&self, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new(TIMEOUT)
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("cache").display()
            ))
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(["--noshow_progress", "--color=no", "--curses=no"])
            .args(&argv[1..])
            .current_dir(self.ws())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }

    /// Native stderr with the workspace as `<WS>`.
    fn stderr(&self, output: &Output) -> String {
        let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
        let ws = self.ws();
        for path in [fs::canonicalize(&ws).unwrap(), ws.clone()] {
            text = text.replace(&path.display().to_string(), "<WS>");
        }
        text
    }

    /// The outputs of `patterns` (`CONFIG/bin/...`, `*` a configuration):
    /// exec path to content, a tree as its sorted `relative: content` lines
    /// (capture.py's read_outputs).
    fn outputs(&self, patterns: &[&str]) -> BTreeMap<String, String> {
        let out = fs::canonicalize(self.ws().join("bazel-out")).unwrap();
        let mut result = BTreeMap::new();
        for pattern in patterns {
            let rest = pattern.strip_prefix("*/").unwrap();
            for config in fs::read_dir(&out).unwrap() {
                let config = config.unwrap();
                let path = config.path().join(rest);
                if !path.exists() {
                    continue;
                }
                let exec = format!("bazel-out/{}/{rest}", config.file_name().to_string_lossy());
                if path.is_dir() {
                    let mut files = Vec::new();
                    walk(&path, &path, &mut files);
                    files.sort();
                    result.insert(exec, files.concat());
                } else {
                    result.insert(exec, fs::read_to_string(&path).unwrap());
                }
            }
        }
        result
    }
}

fn walk(root: &Path, directory: &Path, files: &mut Vec<String>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, files);
        } else {
            files.push(format!(
                "{}: {}",
                path.strip_prefix(root).unwrap().display(),
                fs::read_to_string(&path).unwrap()
            ));
        }
    }
}

/// The outputs each build case reads (capture.py's OUTPUTS).
fn output_patterns(case: &str) -> &'static [&'static str] {
    match case {
        "build_a" | "build_b" => &["*/bin/pkg/u.out", "*/bin/pkg/u.outdir"],
        "collisions" => &["*/bin/pkg/coll_same.out", "*/bin/pkg/coll_diff.out"],
        "param_writes" => &[
            "*/bin/pkg/pw.params",
            "*/bin/pkg/pw_tree.params",
            "*/bin/pkg/pw_noreq.params",
        ],
        other => panic!("no outputs for {other}"),
    }
}

/// The MyMnemonic spawns of a JSON execution log (capture.py's
/// mapped_spawns).
fn mapped_spawns(log: &Path) -> Vec<Value> {
    let text = fs::read_to_string(log).unwrap();
    serde_json::Deserializer::from_str(&text)
        .into_iter::<Value>()
        .map(Result::unwrap)
        .filter(|entry| entry["mnemonic"] == "MyMnemonic")
        .map(|entry| {
            let mut record = serde_json::Map::new();
            for field in ["commandArgs", "listedOutputs", "runner", "digest"] {
                record.insert(field.to_owned(), entry[field].clone());
            }
            if let Some(hash) = entry["digest"].get("hash") {
                record.insert("digest".to_owned(), hash.clone());
            }
            record.insert(
                "inputs".to_owned(),
                entry["inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|input| {
                        Value::from(vec![input["path"].clone(), input["digest"]["hash"].clone()])
                    })
                    .collect(),
            );
            Value::Object(record)
        })
        .collect()
}

/// The `N disk cache hit` part of a process summary.
fn disk_cache_hits(summary: Option<&str>) -> Option<&str> {
    let summary = summary?;
    let end = summary.find(" disk cache hit")?;
    let start = summary[..end].rfind(' ').map_or(0, |space| space + 1);
    Some(&summary[start..end])
}

/// aquery's text output as its action blocks, sorted (the order of the
/// actions is not compared, docs/AQUERY.md).
fn action_blocks<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Vec<&'a str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in lines {
        if line.starts_with("action '") || blocks.is_empty() {
            blocks.push(Vec::new());
        }
        blocks.last_mut().unwrap().push(line);
    }
    blocks.sort();
    blocks
}

#[test]
fn every_case_matches_the_genuine_capture() {
    let receipt: Value =
        serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap())
            .unwrap();
    let ws = Workspace::new();
    let disk_cache = ws.0.path().join("disk_cache");
    let mut failures = Vec::new();
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let log = ws.0.path().join(format!("{name}.log.json"));
        let argv: Vec<String> = case["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| {
                arg.as_str()
                    .unwrap()
                    .replace("<DISK_CACHE>", &disk_cache.display().to_string())
                    .replace("<LOG>", &log.display().to_string())
            })
            .collect();
        let output = ws.run(&argv);
        let stderr = ws.stderr(&output);
        let mut problems = Vec::new();
        if output.status.success() != (case["exit_code"] == 0) {
            problems.push(format!("exit code {:?}", output.status.code()));
        }
        match case["kind"].as_str().unwrap() {
            "message" => {
                let message = case["message"].as_str().unwrap();
                if !stderr.contains(message) {
                    problems.push(format!("genuine message {message:?} not in native stderr"));
                }
            }
            "debug" => {
                let actual: Vec<&str> = stderr
                    .lines()
                    .filter_map(|line| line.split_once(": ACT ").map(|(_, rest)| rest))
                    .collect();
                let expected: Vec<&str> = case["debug"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| line.as_str().unwrap())
                    .collect();
                // The analysis tests print in their analysis order.
                let (mut actual, mut expected) = (actual, expected);
                actual.sort_unstable();
                expected.sort_unstable();
                if actual != expected {
                    problems.push(format!(
                        "Target.actions differ:\n{expected:#?}\nvs\n{actual:#?}"
                    ));
                }
            }
            "outputs" => {
                let actual = ws.outputs(output_patterns(name));
                let expected: BTreeMap<String, String> =
                    serde_json::from_value(case["outputs"].clone()).unwrap();
                if actual != expected {
                    problems.push(format!("outputs differ:\n{expected:#?}\nvs\n{actual:#?}"));
                }
                // The disk cache hits of the summary (the other counts
                // follow the native action cache, docs/PATH_MAPPING.md).
                let summary = stderr
                    .lines()
                    .find(|line| line.starts_with("INFO: ") && line.contains(" process"));
                let genuine = case["summary"].as_str();
                if disk_cache_hits(summary) != disk_cache_hits(genuine) {
                    problems.push(format!("summary {summary:?}, genuine {genuine:?}"));
                }
                if let Some(expected) = case.get("spawns") {
                    let actual = Value::from(mapped_spawns(&log));
                    if &actual != expected {
                        problems.push(format!(
                            "mapped spawns differ:\n{expected:#}\nvs\n{actual:#}"
                        ));
                    }
                }
            }
            "aquery" => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let actual = action_blocks(
                    stdout
                        .lines()
                        .filter(|line| !line.starts_with("# Configuration:")),
                );
                let expected = action_blocks(
                    case["aquery"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|line| line.as_str().unwrap()),
                );
                if actual != expected {
                    problems.push(format!("aquery differs:\n{expected:#?}\nvs\n{actual:#?}"));
                }
            }
            other => problems.push(format!("unexpected kind {other}")),
        }
        if !problems.is_empty() {
            failures.push(format!(
                "{name} ({argv:?}): {}\n{stderr}",
                problems.join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
