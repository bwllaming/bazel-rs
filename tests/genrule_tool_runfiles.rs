//! A genrule tool with runfiles support brings its runfiles tree into the
//! action (Bazel 9.3 analysis/CommandHelper.java:156-166 resolvedTools,
//! rules/genrule/GenRuleBase.java:102-106, 171): a js_binary-like launcher
//! finds its runfiles via `$0.runfiles` and via RUNFILES_DIR, sandboxed and
//! standalone, and the staged tree has genuine's entries. Every case of
//! tests/fixtures/genrule_tool_runfiles/reference_receipt.json — genuine
//! Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output
//! user root) on the fixture workspace — runs natively here and is compared
//! with the genuine capture: success and the top-level outputs (mode and
//! contents).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/genrule_tool_runfiles/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/genrule_tool_runfiles/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("genrule-tool-runfiles-")
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

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`.
    fn run(&self, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let cache = format!(
            "--cache-dir={}",
            self.path().join("private-cache").display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &cache])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
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

/// The built top-level outputs (path below `bazel-out` -> [mode, contents])
/// of the native `LABEL\tPATH` result lines.
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    use std::os::unix::fs::PermissionsExt as _;
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(_, path)| {
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            (
                relative.to_owned(),
                serde_json::json!([format!("0o{mode:o}"), fs::read_to_string(path).unwrap()]),
            )
        })
        .collect()
}

fn check(name: &str) {
    let receipt = receipt();
    let case = &receipt["cases"][name];
    assert_eq!(case["verdict"], "MATCH", "{name}");
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let genuine = &case["genuine"];
    let ws = Workspace::new();
    let output = ws.run(&argv);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{name}: {stderr}");
    assert_eq!(
        output.status.success(),
        genuine["exit"].as_i64() == Some(0),
        "{name} {argv:?}: {stderr}"
    );
    let expected: BTreeMap<String, Value> = genuine["outputs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(path, value)| (path.clone(), value.clone()))
        .collect();
    assert_eq!(
        outputs(&String::from_utf8_lossy(&output.stdout)),
        expected,
        "{name}: {stderr}"
    );
}

#[test]
fn receipt_covers_the_fixture_and_every_case() {
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
    let mut cases = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cases.sort_unstable();
    assert_eq!(
        cases,
        ["listing", "listing_standalone", "use", "use_standalone"]
    );
}

/// The launcher reads its runfile through `$0.runfiles` and RUNFILES_DIR.
#[test]
fn genrule_tools_find_their_runfiles_sandboxed() {
    check("use");
}

#[test]
fn genrule_tools_find_their_runfiles_standalone() {
    check("use_standalone");
}

/// The staged tree's entries: the data file, the executable and
/// `_repo_mapping`.
#[test]
fn genrule_tool_runfiles_tree_has_genuine_entries() {
    check("listing");
    check("listing_standalone");
}
