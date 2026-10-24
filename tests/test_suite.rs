//! test_suite (Bazel 9.3 rules/test/TestSuiteRule.java, TestSuite.java) and
//! its expansion by the target pattern phase of `build`, `test` and `run`
//! (skyframe/TargetPatternPhaseFunction.java, TestsForTargetPatternFunction,
//! TestExpansionFunction): suites replaced by their tests (explicit,
//! implicit, tag-filtered, nested), negative patterns, `--build_tests_only`
//! with the test filters, strict expansion errors, `run` of a suite (its
//! expansion validated by RunCommand.validateTargets), and a suite analyzed
//! as a dependency or with `--noexpand_test_suites` (its runfiles, its
//! `tests` checks). Every case of tests/fixtures/test_suite/reference_receipt.json —
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace — runs natively here (with the
//! artifact and view self-checks) and is compared with the genuine capture:
//! success, the WARNING lines, Bazel's error message, the top-level outputs
//! (mode and contents), a `test` command's summary and the output of the
//! program a `run` command ran.
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
    manifest().join("tests/fixtures/test_suite/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/test_suite/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "expansion",
        &[
            "build_explicit",
            "build_implicit",
            "build_fast",
            "build_not_fast",
            "build_small",
            "build_explicit_fast",
            "build_nested",
            "build_manual_suite",
            "build_all",
            "build_wildcard",
            "build_other",
            "build_suite_and_test",
            "nobuild_explicit",
            "build_sharded",
            "build_noflaky",
        ],
    ),
    (
        "negative",
        &[
            "build_negative",
            "build_negative_suite",
            "tests_only_negative",
            "tests_only_negative_suite",
            "test_negative",
        ],
    ),
    (
        "tests_only",
        &[
            "tests_only_all",
            "tests_only_explicit",
            "tests_only_filter",
            "tests_only_size",
            "tests_only_lib",
            "tests_only_excluded",
            "tests_only_sharded",
            "tests_only_noflaky",
            "tests_only_not_nolocal",
        ],
    ),
    (
        "strict",
        &[
            "with_lib",
            "with_alias",
            "with_file",
            "test_with_lib",
            "with_lib_alias",
        ],
    ),
    (
        "analysis",
        &[
            "consumer",
            "consumer_alias",
            "consumer_lib",
            "consumer_file",
            "consumer_not_testonly",
            "filegroup",
            "noexpand",
            "noexpand_test",
            "consumer_lib_alias",
            "consumer_lib_alias_chain",
            "consumer_sharded",
            "consumer_noflaky",
        ],
    ),
    ("test", &["test_explicit", "test_all"]),
    (
        "run",
        &[
            "run_single",
            "run_other",
            "run_multi",
            "run_strict",
            "run_empty",
            "run_noexpand",
            "run_tests_only_excluded",
            "run_tests_only_lib",
        ],
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
                .prefix("test-suite-")
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
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }

    /// Native stderr with the workspace path as `<WS>`, like the receipt.
    fn stderr(&self, output: &Output) -> String {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut paths = vec![self.path().display().to_string()];
        if let Ok(real) = fs::canonicalize(self.path()) {
            paths.push(real.display().to_string());
        }
        for path in paths {
            stderr = stderr.replace(&path, "<WS>");
        }
        stderr
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

/// A test summary with its ` in N.Ns` run times as ` in <T>s`.
fn normalized_times(stdout: &str) -> String {
    let mut out = String::new();
    let mut rest = stdout;
    while let Some(index) = rest.find(" in ") {
        out.push_str(&rest[..index]);
        let after = &rest[index + 4..];
        let digits = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        if digits > 0 && after[digits..].starts_with('s') {
            out.push_str(" in <T>s");
            rest = &after[digits + 1..];
        } else {
            out.push_str(" in ");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.starts_with("WARNING: "))
        .collect()
}

/// The built top-level outputs (path below `bazel-out` -> [mode, contents])
/// of the native `LABEL\tPATH` result lines.
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    use std::os::unix::fs::PermissionsExt as _;
    stdout
        .lines()
        .map(|line| {
            let (_, path) = line.split_once('\t').unwrap();
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            (
                relative.to_owned(),
                serde_json::json!([format!("0o{mode:o}"), fs::read_to_string(path).unwrap()]),
            )
        })
        .collect()
}

fn argv(case: &Value) -> Vec<String> {
    case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect()
}

fn check(name: &str, case: &Value, ws: &Workspace) {
    let argv = argv(case);
    let genuine = &case["genuine"];
    let genuine_stderr = genuine["stderr"].as_str().unwrap();
    let output = ws.run(&argv);
    let stderr = ws.stderr(&output);
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
    let succeeded = genuine["exit"].as_i64() == Some(0);
    assert_eq!(
        output.status.success(),
        succeeded,
        "{name} {argv:?}: {stderr}"
    );
    assert_eq!(
        warnings(&stderr),
        warnings(genuine_stderr),
        "{name}: {stderr}"
    );
    if !succeeded {
        let message = case["expected_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
    }
    if argv[0] == "test" {
        assert_eq!(
            normalized_times(&stdout),
            case["expected_stdout"].as_str().unwrap(),
            "{name}: {stderr}"
        );
    }
    // The program `run` ran (a test through test-setup.sh).
    if argv[0] == "run" && succeeded {
        assert_eq!(
            stdout,
            case["expected_stdout"].as_str().unwrap(),
            "{name}: {stderr}"
        );
    }
    if argv[0] == "build" && succeeded && !argv.iter().any(|arg| arg == "--nobuild") {
        let expected: BTreeMap<String, Value> = genuine["outputs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect();
        assert_eq!(outputs(&stdout), expected, "{name}: {stderr}");
    }
}

/// The group's builds share one workspace and cache (the repositories of
/// the test rules' implicit dependencies are fetched once); each `test`
/// case gets its own (cached test results would change its summary).
fn check_group(group: &str) {
    let receipt = receipt();
    let (_, names) = GROUPS.iter().find(|(name, _)| *name == group).unwrap();
    let shared = Workspace::new();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        if argv(case)[0] == "test" {
            check(name, case, &Workspace::new());
        } else {
            check(name, case, &shared);
        }
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
    for (name, case) in receipt["cases"].as_object().unwrap() {
        assert_eq!(case["verdict"], "MATCH", "{name}");
    }
}

/// A suite is replaced by its tests: explicit (manual tests too),
/// implicit (the package's non-manual tests), tag filters with the size as
/// a tag, nested suites unfiltered, wildcards skipping manual targets.
#[test]
fn build_expands_test_suites() {
    check_group("expansion");
}

/// Negative patterns apply before the expansion of the targets to build and
/// after the expansion of the tests.
#[test]
fn negative_patterns_follow_the_target_pattern_phase() {
    check_group("negative");
}

/// `--build_tests_only` builds determineTests' tests, after the test
/// filters.
#[test]
fn build_tests_only_builds_the_filtered_tests() {
    check_group("tests_only");
}

/// A member that is no test or test suite fails the command after it ran.
#[test]
fn strict_expansion_errors_fail_after_the_build() {
    check_group("strict");
}

/// A suite analyzed as a dependency (or with --noexpand_test_suites): no
/// files, its tests' data runfiles, its `tests` checks.
#[test]
fn test_suite_analysis_matches() {
    check_group("analysis");
}

/// `test` of suites runs the expansion's tests.
#[test]
fn test_runs_the_expanded_tests() {
    check_group("test");
}

/// `run` of a suite validates its expansion: one test runs, several are
/// Bazel's single-target error, none `No targets found to run`; a strict
/// expansion error fails the build before anything runs.
#[test]
fn run_validates_the_expanded_targets() {
    check_group("run");
}
