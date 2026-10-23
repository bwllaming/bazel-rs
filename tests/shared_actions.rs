//! Shared actions between configured targets (Bazel 9.3
//! MapBasedActionGraph.registerAction, actions/MapBasedActionGraph.java
//! :46-63, over Actions.canBeShared, actions/Actions.java:85-115): two
//! targets' actions generating one output are one action when their
//! mnemonic, action key and ownerless mandatory inputs and outputs are
//! equal (crate::shared_actions). Every case of
//! tests/fixtures/shared_actions/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) on the fixture workspace — runs natively here and is compared with
//! the genuine capture: success, a conflict's message and its set of
//! labels, the top-level outputs with their contents; a declared divergence
//! is a native failure with its recorded message.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/shared_actions/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/shared_actions/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "shared",
        &[
            "shared_build",
            "shared_reversed",
            "shared_nobuild",
            "shared_plain_tags",
            "single",
            "groups_shared",
            "write_output_first",
            "write_declared_first",
        ],
    ),
    (
        "conflicts",
        &[
            "conflict_args",
            "conflict_nobuild",
            "conflict_tags",
            "conflict_mnemonic",
            "conflict_env",
            "conflict_inputs",
            "conflict_tool",
            "conflict_props",
            "write_conflict",
            "groups_props",
        ],
    ),
    ("declared", &["prefix"]),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("shared-actions-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture(), ws.path());
        extract_platforms(&ws.path().join("platforms"));
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`,
    /// with the artifact and view self-checks on.
    fn run(&self, argv: &[String], env: &[(&str, &str)]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
            // Every build checks the artifact index (one Artifact per File;
            // the shared outputs' second owner adds its own Files) and the
            // memoized dependency views.
            .env("BAZEL_RS_VERIFY_ARTIFACTS", "1")
            .env("BAZEL_RS_VERIFY_VIEWS", "1");
        let output = command.envs(env.iter().copied()).output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The genuine platforms 1.0.0 module archive (tests/exec_transition.rs).
fn extract_platforms(to: &Path) {
    let manifest: Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/label_repo_name/platforms_archive.json"))
            .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = to.join(entry.path().unwrap());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&path).unwrap();
            continue;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// The first action conflict of `stderr`: its first line and the set of
/// labels of the `Label:` line after it (genuine Bazel orders them by
/// analysis timing).
fn conflict(stderr: &str) -> Option<(String, Vec<String>)> {
    let lines = stderr.lines().collect::<Vec<_>>();
    lines.windows(2).find_map(|pair| {
        let line = pair[0]
            .strip_prefix("bazel-rs: ")
            .or_else(|| pair[0].strip_prefix("ERROR: "))
            .unwrap_or(pair[0]);
        let labels = pair[1].strip_prefix("Label: ")?;
        (line.starts_with("file '")
            && line.ends_with("' is generated by these conflicting actions:"))
        .then(|| {
            let mut labels = labels
                .split(',')
                .map(|label| label.trim().to_owned())
                .collect::<Vec<_>>();
            labels.sort();
            (line.to_owned(), labels)
        })
    })
}

/// The native `LABEL\tPATH` result lines as [label, path below the
/// published outputs, contents], sorted like the receipt's.
fn outputs(stdout: &str) -> Vec<Value> {
    let mut found = stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, path)| {
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            serde_json::json!([label, relative, fs::read_to_string(path).ok()])
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|row| row.to_string());
    found
}

fn check(name: &str, case: &Value) {
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let ws = Workspace::new();
    let output = ws.run(&argv, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stderr.contains("panicked"), "{name}: {stderr}");
    if case["verdict"] == "DECLARED_DIVERGENCE" {
        assert!(!output.status.success(), "{name}: {stderr}");
        let message = case["native_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        return;
    }
    assert_eq!(case["verdict"], "MATCH", "{name}");
    let succeeded = case["genuine"]["exit"].as_i64() == Some(0);
    assert_eq!(
        output.status.success(),
        succeeded,
        "{name} {argv:?}: {stderr}"
    );
    if !succeeded {
        let labels = case["expected_labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let expected = (
            case["expected_message"].as_str().unwrap().to_owned(),
            labels,
        );
        assert_eq!(conflict(&stderr), Some(expected), "{name}: {stderr}");
        return;
    }
    if argv.iter().any(|arg| arg == "--nobuild") {
        return;
    }
    let mut expected = case["outputs"].as_array().unwrap().clone();
    expected.sort_by_key(|row| row.to_string());
    assert_eq!(outputs(&stdout), expected, "{name}: {stderr}");
}

fn check_group(group: &str) {
    let receipt = receipt();
    let (_, names) = GROUPS.iter().find(|(name, _)| *name == group).unwrap();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        check(name, case);
    }
}

#[test]
fn receipt_covers_the_fixture_and_every_case_once() {
    let receipt = receipt();
    let mut files = BTreeMap::new();
    let mut stack = vec![fixture()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path.strip_prefix(fixture()).unwrap();
                let digest = Sha256::digest(fs::read(&path).unwrap());
                files.insert(
                    relative.display().to_string(),
                    Value::String(format!("{digest:x}")),
                );
            }
        }
    }
    let recorded = receipt["fixture_files_sha256"].as_object().unwrap();
    assert_eq!(
        files,
        recorded
            .iter()
            .map(|(path, digest)| (path.clone(), digest.clone()))
            .collect::<BTreeMap<_, _>>()
    );
    let mut grouped = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect::<Vec<_>>();
    grouped.sort_unstable();
    let mut cases = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cases.sort_unstable();
    assert_eq!(grouped, cases);
    let declared = GROUPS
        .iter()
        .find(|(group, _)| *group == "declared")
        .unwrap()
        .1;
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let expected = if declared.contains(&name.as_str()) {
            "DECLARED_DIVERGENCE"
        } else {
            "MATCH"
        };
        assert_eq!(case["verdict"], expected, "{name}");
    }
}

/// Identical actions of two targets on one output are shared: the
/// rules_python precompile shape (a tool with runfiles, a param file, env
/// and execution requirements, per-target progress messages), a consumer
/// of both targets' Files, plain tags, exec groups of one platform, and a
/// declared file shared with another rule's output-file target.
#[test]
fn equal_actions_of_two_targets_are_shared() {
    check_group("shared");
}

/// Any compared difference is Bazel's action conflict, both labels named.
#[test]
fn compared_differences_conflict() {
    check_group("conflicts");
}

/// Explicit native errors with a recorded difference.
#[test]
fn declared_divergences_are_explicit_errors() {
    check_group("declared");
}

/// A shared action runs once: the planned graph has one node producing the
/// shared output, owned by the first target in commit order (the plan
/// order, whatever the command line's order), and the consumer of both
/// targets' Files depends on that node once.
#[test]
fn a_shared_action_is_planned_once_by_its_first_owner() {
    let owner = "//pkg:a";
    for targets in [
        ["//pkg:a", "//pkg:b", "//pkg:both"],
        ["//pkg:b", "//pkg:a", "//pkg:both"],
        ["//pkg:both", "//pkg:b", "//pkg:a"],
    ] {
        let ws = Workspace::new();
        let mut argv = vec!["build".to_owned(), "--nobuild".to_owned()];
        argv.extend(targets.iter().map(|target| (*target).to_owned()));
        let output = ws.run(&argv, &[("BAZEL_RS_DUMP_ACTIONS", "1")]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let shared = "bazel-out/k8-fastbuild/bin/pkg/__pycache__/__init__.tag.pyc";
        let producers = stderr
            .split("ACTION ")
            .filter(|node| node.contains(&format!("  output {shared}\n")))
            .map(|node| node.lines().next().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(producers, [format!("{owner}#0")], "{stderr}");
        let consumer = stderr
            .split("ACTION ")
            .find(|node| node.contains("output bazel-out/k8-fastbuild/bin/pkg/both.cat\n"))
            .unwrap();
        assert_eq!(
            consumer.matches(&format!("-> {shared}\n")).count(),
            1,
            "{consumer}"
        );
        assert!(
            consumer.contains(&format!("generated {owner}#0[0] -> {shared}\n")),
            "{consumer}"
        );
    }
}
