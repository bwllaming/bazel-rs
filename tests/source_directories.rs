//! Source directories (a source artifact naming a directory) as action
//! inputs, requested outputs and runfiles, tracked by content like Bazel
//! 9.3's default (TrackSourceDirectoriesFlag, ArtifactFunction
//! .createSourceValue; docs/SOURCE_DIRECTORIES.md): what spawns, runfiles
//! trees, tools and tests see, the traversal errors of an executing build
//! (package boundaries, symlink expansion and cycles) and change tracking
//! across invocations.
//! Every case of tests/fixtures/source_directories/reference_receipt.json —
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace with setup.json applied — runs
//! natively here (with the artifact and view self-checks), step by step in
//! one workspace and cache, and is compared with the genuine capture:
//! success, the WARNING lines, Bazel's error message, the top-level outputs
//! (mode and contents), which actions ran again, test summaries and `run` /
//! `query` output; a declared case fails with its recorded message or
//! reproduces its recorded native outputs and re-executions.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

include!("support/module_archives.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/source_directories")
}

fn receipt() -> Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "inputs",
        &[
            "probe",
            "inputs",
            "use_tool",
            "requested",
            "nobuild_all",
            "query",
        ],
    ),
    ("runfiles", &["run_show", "test"]),
    (
        "errors",
        &[
            "err_cross",
            "err_gen_cross",
            "err_fg_cross",
            "err_runfiles_cross",
            "err_loop",
            "err_cycle",
        ],
    ),
    ("incremental", &["incremental"]),
    (
        "declared",
        &["view", "view_local", "nested", "incremental_chmod"],
    ),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("source-directories-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture().join("workspace"), ws.path());
        let setup: Value =
            serde_json::from_slice(&fs::read(fixture().join("setup.json")).unwrap()).unwrap();
        ws.mutate(&setup);
        extract_platforms(&ws.path().join("platforms"));
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// The capture's workspace operations (sd-capture.py `mutate`).
    fn mutate(&self, operations: &Value) {
        use std::os::unix::fs::PermissionsExt as _;
        for operation in operations.as_array().unwrap() {
            let args = operation
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap())
                .collect::<Vec<_>>();
            let at = |relative: &str| self.path().join(relative);
            match args.as_slice() {
                ["mkdir", path] => fs::create_dir_all(at(path)).unwrap(),
                ["rmdir", path] => fs::remove_dir(at(path)).unwrap(),
                ["symlink", target, path] => {
                    let _ = fs::remove_file(at(path));
                    std::os::unix::fs::symlink(target, at(path)).unwrap();
                }
                ["chmod", mode, path] => fs::set_permissions(
                    at(path),
                    fs::Permissions::from_mode(u32::from_str_radix(mode, 8).unwrap()),
                )
                .unwrap(),
                ["touch", path] => {
                    let file = fs::File::open(at(path)).unwrap();
                    file.set_modified(std::time::SystemTime::now()).unwrap();
                }
                ["write", path, content] => fs::write(at(path), content).unwrap(),
                other => panic!("unknown operation {other:?}"),
            }
        }
    }

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`.
    fn run(&self, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
            // Every build checks the artifact index and the memoized
            // dependency views.
            .env("BAZEL_RS_VERIFY_ARTIFACTS", "1")
            .env("BAZEL_RS_VERIFY_VIEWS", "1")
            .seeded_archives()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }

    /// Native stderr with the workspace path as `<WS>`, like the receipt.
    fn stderr(&self, output: &Output) -> String {
        self.normalized(&String::from_utf8_lossy(&output.stderr))
    }

    fn normalized(&self, text: &str) -> String {
        let mut text = text.to_owned();
        let mut paths = vec![self.path().display().to_string()];
        if let Ok(real) = fs::canonicalize(self.path()) {
            paths.push(real.display().to_string());
        }
        for path in paths {
            text = text.replace(&path, "<WS>");
        }
        text
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

fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.starts_with("WARNING: "))
        .collect()
}

/// The built top-level outputs (path below `bazel-out` -> [mode, contents])
/// of the native `LABEL\tPATH` result lines without the `.stamp` tokens, and
/// the tokens; source files (which Bazel's result does not list) are
/// skipped.
fn outputs(stdout: &str) -> (BTreeMap<String, Value>, BTreeMap<String, String>) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut outputs = BTreeMap::new();
    let mut stamps = BTreeMap::new();
    for line in stdout.lines() {
        let Some((_, path)) = line.split_once('\t') else {
            continue;
        };
        let Some((_, relative)) = path.split_once("/outputs/") else {
            continue;
        };
        let contents = fs::read_to_string(path).unwrap();
        if relative.ends_with(".stamp") {
            stamps.insert(relative.to_owned(), contents);
            continue;
        }
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        outputs.insert(
            relative.to_owned(),
            serde_json::json!([format!("0o{mode:o}"), contents]),
        );
    }
    (outputs, stamps)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect()
}

/// Which actions a step ran again: each stamp of the step against the
/// previous step's.
fn reexecuted(
    stamps: &BTreeMap<String, String>,
    previous: &BTreeMap<String, String>,
) -> BTreeMap<String, Value> {
    stamps
        .iter()
        .map(|(path, token)| (path.clone(), Value::Bool(previous.get(path) != Some(token))))
        .collect()
}

fn recorded_outputs(side: &Value) -> Option<BTreeMap<String, Value>> {
    side["outputs"].as_object().map(|outputs| {
        outputs
            .iter()
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect()
    })
}

fn recorded_reexecuted(side: &Value) -> Option<BTreeMap<String, Value>> {
    side["reexecuted"].as_object().map(|map| {
        map.iter()
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect()
    })
}

fn check(name: &str, case: &Value) {
    let ws = Workspace::new();
    let declared = case["verdict"] == "DECLARED_DIVERGENCE";
    if !declared {
        assert_eq!(case["verdict"], "MATCH", "{name}");
    }
    let mut previous = BTreeMap::new();
    for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        ws.mutate(&step["mutate"]);
        let argv = strings(&step["argv"]);
        let output = ws.run(&argv);
        let stderr = ws.stderr(&output);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let what = format!("{name} step {index} {argv:?}");
        assert!(!stderr.contains("panicked"), "{what}: {stderr}");
        // A declared case: the recorded native message, or (succeeding)
        // the recorded native outputs and re-executions.
        let expected = if declared {
            if let Some(message) = case["native_message"].as_str() {
                assert!(!output.status.success(), "{what}: {stderr}");
                assert!(
                    stderr.contains(message),
                    "{what}: missing {message:?} in {stderr}"
                );
                return;
            }
            &step["native"]
        } else {
            &step["genuine"]
        };
        let succeeded = expected["exit"].as_i64() == Some(0);
        assert_eq!(output.status.success(), succeeded, "{what}: {stderr}");
        assert_eq!(
            warnings(&stderr),
            warnings(step["genuine"]["stderr"].as_str().unwrap()),
            "{what}: {stderr}"
        );
        if let Some(message) = step["expected_message"].as_str() {
            assert!(
                stderr.contains(message),
                "{what}: missing {message:?} in {stderr}"
            );
        }
        if let Some(expected_stdout) = step["expected_stdout"].as_str() {
            let actual = if argv[0] == "test" {
                timing(&stdout)
            } else {
                stdout.clone()
            };
            assert_eq!(actual, expected_stdout, "{what}: {stderr}");
        }
        let built = argv[0] == "build" && succeeded && !argv.iter().any(|arg| arg == "--nobuild");
        if built {
            let (outputs, stamps) = outputs(&stdout);
            assert_eq!(
                Some(outputs),
                recorded_outputs(expected),
                "{what}: {stderr}"
            );
            if let Some(recorded) = recorded_reexecuted(expected) {
                assert_eq!(reexecuted(&stamps, &previous), recorded, "{what}: {stderr}");
            } else if index > 0 {
                // No action ran again.
                assert_eq!(stamps, previous, "{what}: {stderr}");
            }
            previous = stamps;
        }
    }
}

fn timing(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(" in ") {
        let (head, tail) = rest.split_at(at + 4);
        result.push_str(head);
        let digits = tail
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .last()
            .map(|(index, c)| index + c.len_utf8());
        match digits {
            Some(end) if tail[end..].starts_with('s') => {
                result.push_str("<T>");
                rest = &tail[end..];
            }
            _ => rest = tail,
        }
    }
    result.push_str(rest);
    result
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
            } else if path.file_name().unwrap() != "reference_receipt.json" {
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
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let verdict = case["verdict"].as_str().unwrap();
        assert!(
            verdict == "MATCH" || verdict == "DECLARED_DIVERGENCE",
            "{name}: {verdict}"
        );
    }
}

/// Source directories as action inputs (sandboxed, unsandboxed, external,
/// through a filegroup, a genrule, a tool's runfiles), their Files, a
/// requested one, `--nobuild` and `query`.
#[test]
fn source_directories_are_action_inputs() {
    check_group("inputs");
}

/// `run` through the runfiles tree and a test's runfiles.
#[test]
fn source_directories_are_runfiles() {
    check_group("runfiles");
}

/// The traversal errors of an executing build.
#[test]
fn traversal_errors_match() {
    check_group("errors");
}

/// Content tracking across invocations: touch is no change; content, empty
/// directories and dangling links are.
#[test]
fn changes_inside_run_the_consumers_again() {
    check_group("incremental");
}

/// Staging, nested inputs and executable-bit tracking (declared).
#[test]
fn declared_divergences_reproduce() {
    check_group("declared");
}
