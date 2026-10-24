//! `--enable_runfiles` as a configuration value (Bazel 9.3
//! analysis/config/CoreOptions.java:954-961, a TriState defaulting to `auto`):
//! config_setting(values) converts the expected value with TriStateConverter
//! and compares it with the configured value, so aspect_bazel_lib's
//! `//lib:enable_runfiles` (`true`) does not match by default; the boolean
//! command-line forms, the last occurrence winning, the exec propagation,
//! `query` and the converter errors.
//! Every case of tests/fixtures/enable_runfiles_option/reference_receipt.json
//! — genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace — runs natively here (with the
//! artifact and view self-checks) and is compared with the genuine capture:
//! success, the WARNING lines, Bazel's error message, the top-level outputs
//! (mode and contents) and a `query`'s output.
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
    manifest().join("tests/fixtures/enable_runfiles_option/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/enable_runfiles_option/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "matching",
        &[
            "defaults",
            "set",
            "set_yes",
            "set_upper",
            "set_no",
            "set_false",
            "set_auto",
            "last_wins",
            "nobuild",
        ],
    ),
    (
        "errors",
        &["bad_value", "no_prefix_value", "setting_bad", "query_bad"],
    ),
    ("query", &["query"]),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("enable-runfiles-option-")
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

    /// Whether an action of the private cache produced `relative` (a path
    /// below the execution root, `bazel-out/...`).
    fn built(&self, relative: &str) -> bool {
        let Ok(actions) = fs::read_dir(self.path().join("private-cache/actions")) else {
            return false;
        };
        actions.filter_map(Result::ok).any(|action| {
            action
                .path()
                .join("outputs")
                .join(relative)
                .symlink_metadata()
                .is_ok()
        })
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
/// of the native `LABEL\tPATH` result lines; source files (which Bazel's
/// result does not list) are skipped.
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    use std::os::unix::fs::PermissionsExt as _;
    stdout
        .lines()
        .filter_map(|line| {
            let (_, path) = line.split_once('\t').unwrap();
            let (_, relative) = path.split_once("/outputs/")?;
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            Some((
                relative.to_owned(),
                serde_json::json!([format!("0o{mode:o}"), fs::read_to_string(path).unwrap()]),
            ))
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
    // The program `run` ran, the `query` result.
    if succeeded && (argv[0] == "run" || argv[0] == "query") {
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
    if let Some(exists) = genuine["exists"].as_object() {
        for (relative, built) in exists {
            assert_eq!(
                ws.built(relative),
                built.as_bool().unwrap(),
                "{name}: {relative} built: {stderr}"
            );
        }
    }
}

/// The group's commands share one workspace and cache, except a case that
/// records built outputs (an earlier case could have built them).
fn check_group(group: &str) {
    let receipt = receipt();
    let (_, names) = GROUPS.iter().find(|(name, _)| *name == group).unwrap();
    let shared = Workspace::new();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        if case["genuine"]["exists"].is_object() {
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
        let verdict = case["verdict"].as_str().unwrap();
        assert!(
            verdict == "MATCH" || verdict == "DECLARED_DIVERGENCE",
            "{name}: {verdict}"
        );
    }
}

/// config_setting(values) on enable_runfiles at the default, with each
/// command-line form, in the exec configuration and under `--nobuild`.
#[test]
fn enable_runfiles_settings_match_the_configured_tristate() {
    check_group("matching");
}

/// TriStateConverter errors on the command line and in a config_setting,
/// and the `no` prefix with a value.
#[test]
fn enable_runfiles_conversion_errors_match() {
    check_group("errors");
}

/// `query` accepts the CoreOptions flag.
#[test]
fn query_accepts_enable_runfiles() {
    check_group("query");
}
