//! Unresolved symlink artifacts (Bazel 9.3 StarlarkActionFactory.java:194-354,
//! analysis/actions/UnresolvedSymlinkAction.java): ctx.actions.declare_symlink
//! declares a symlink File (`is_symlink`), distinct from a file at the same
//! path; ctx.actions.symlink(target_path = ...) registers an
//! UnresolvedSymlinkAction with Bazel's argument errors; `--nobuild` analyzes
//! producers, consumers and runfiles. Executed, the symlink has the literal
//! (normalized) target; consumers, runfiles trees, the runfiles manifest,
//! `run`, tests and a rules_python-like venv see it (docs/UNRESOLVED_SYMLINKS.md).
//! Every case of tests/fixtures/unresolved_symlinks/reference_receipt.json —
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace — runs natively here (with the
//! artifact and view self-checks) and is compared with the genuine capture:
//! success, the WARNING lines, Bazel's error message, the top-level outputs
//! (mode and contents, or a symlink's text), `run` output and test summaries.
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
include!("support/module_archives.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/unresolved_symlinks/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/unresolved_symlinks/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    ("analysis", &["probe", "nobuild_all", "nobuild_show"]),
    (
        "errors",
        &[
            "err_file_then_symlink",
            "err_symlink_then_file",
            "err_target_file_on_symlink",
            "err_target_path_on_file",
            "err_is_executable",
            "err_bad_target_type",
            "err_both_targets",
            "err_no_action",
            "err_target_path_int",
        ],
    ),
    (
        "execution",
        &[
            "build_rel",
            "build_consumer",
            "build_spawned",
            "run_show",
            "build_dangling",
            "build_consumer_standalone",
            "build_not_a_symlink",
        ],
    ),
    (
        "venv",
        &[
            "run_venv",
            "build_venv_use",
            "build_venv_use_standalone",
            "test_venv",
            "build_through",
            "build_through_standalone",
            "build_manifest",
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
                .prefix("unresolved-symlinks-")
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
            .seeded_archives()
            .explicit_modules()
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

/// The built top-level outputs (path below `bazel-out` -> [mode, contents],
/// or ["symlink", text] for an unresolved symlink) of the native
/// `LABEL\tPATH` result lines; source files (which Bazel's result does not
/// list) are skipped.
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    use std::os::unix::fs::PermissionsExt as _;
    stdout
        .lines()
        .filter_map(|line| {
            let (_, path) = line.split_once('\t').unwrap();
            let (_, relative) = path.split_once("/outputs/")?;
            if fs::symlink_metadata(path).unwrap().file_type().is_symlink() {
                let text = fs::read_link(path).unwrap();
                return Some((
                    relative.to_owned(),
                    serde_json::json!(["symlink", text.to_str().unwrap()]),
                ));
            }
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
    // A test summary, run times normalized.
    if argv[0] == "test" {
        let timing = |text: &str| {
            let mut normalized = String::new();
            for line in text.lines() {
                match line.rfind(" in ") {
                    Some(at) if line.ends_with('s') => {
                        normalized.push_str(&line[..at]);
                        normalized.push_str(" in <T>s");
                    }
                    _ => normalized.push_str(line),
                }
                normalized.push('\n');
            }
            normalized
        };
        assert_eq!(
            timing(&stdout),
            timing(case["expected_stdout"].as_str().unwrap()),
            "{name}: {stderr}"
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

/// Symlink Files as Starlark sees them, and `--nobuild` of their producers,
/// consumers and runfiles.
#[test]
fn declared_symlinks_analyze() {
    check_group("analysis");
}

/// StarlarkActionFactory.symlink's argument errors and the orphaned
/// declarations.
#[test]
fn symlink_argument_errors_match() {
    check_group("errors");
}

/// Created symlinks, their consumers (sandboxed and standalone), `run`'s
/// runfiles tree, and a spawn's output that is no symlink.
#[test]
fn unresolved_symlinks_execute() {
    check_group("execution");
}

/// A rules_python-like venv: a relative link into an external repository
/// resolved in runfiles trees (`run`, a tool's spawn, a test) and named by
/// the runfiles manifest.
#[test]
fn venv_links_resolve_in_runfiles() {
    check_group("venv");
}
