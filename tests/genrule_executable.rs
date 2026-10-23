//! `genrule(executable = True)` (Bazel 9.3 rules/genrule/GenRuleBase.java:78-86,
//! 215-235, 288-298): the single output is the target's executable
//! (FilesToRunProvider without runfiles support) for
//! `dep[DefaultInfo].files_to_run`, `ctx.executable`, genrule tools,
//! `$(location)`/`$(execpath)`/`$(rootpath)`, Starlark `run_shell` tools and
//! `bazel run`; several outputs are an attribute error. Every case of
//! tests/fixtures/genrule_executable/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) on the fixture workspace — runs natively here and is compared with
//! the genuine capture: success, the WARNING lines, Bazel's rule and
//! attribute ERROR messages, the top-level outputs (mode and contents) and
//! `run`'s stdout; a declared divergence is a native failure with its
//! recorded message.
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
    manifest().join("tests/fixtures/genrule_executable/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/genrule_executable/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "consumers",
        &[
            "exe",
            "plain",
            "selected",
            "use",
            "use_execpath",
            "probe",
            "probe_output_file",
            "run_tool_rule",
            "nobuild",
        ],
    ),
    ("errors", &["two", "two_without_cmd"]),
    (
        "run",
        &["run_exe", "run_selected", "run_plain", "run_output_file"],
    ),
    (
        "single_file",
        &[
            "single_file_output",
            "single_file_rule",
            "single_file_plain_output",
        ],
    ),
    ("declared", &["probe_plain_tool"]),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("genrule-executable-")
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
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
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

fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.starts_with("WARNING: "))
        .collect()
}

/// Bazel's ERROR messages other than its build summaries (`Analysis of
/// target ... failed`, `Build did NOT complete successfully`, ...).
fn errors(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("ERROR: "))
        .filter(|message| {
            !message.contains("Analysis of target")
                && !message.starts_with("Build did NOT complete")
                && !message.starts_with("Build failed.")
        })
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

fn check(name: &str, case: &Value) {
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let genuine = &case["genuine"];
    let genuine_stderr = genuine["stderr"].as_str().unwrap();
    let ws = Workspace::new();
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
    for error in errors(genuine_stderr) {
        assert!(
            stderr.lines().any(|line| line.ends_with(error)),
            "{name}: missing {error:?} in {stderr}"
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
    // `run`: the program's own output.
    if argv[0] == "run" && succeeded {
        assert_eq!(stdout, genuine["stdout"].as_str().unwrap(), "{name}");
    }
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

/// The executable output in every consumer, the select()ed source of
/// build_stack_rules_proto's protoc_exe, a non-executable genrule, an
/// output file target in an `executable` attribute, `--nobuild`.
#[test]
fn executable_genrules_provide_their_output_as_executable() {
    check_group("consumers");
}

/// Several outputs (with and without `cmd`).
#[test]
fn executable_genrules_have_one_output() {
    check_group("errors");
}

/// `bazel run` of executable genrules, output files and a non-executable
/// genrule.
#[test]
fn run_executes_the_genrule_output() {
    check_group("run");
}

/// build_stack_rules_proto's `protoc(tool = ":protoc.exe")`: an
/// `allow_single_file` executable attribute naming an executable genrule's
/// output file, the genrule, and a non-executable genrule's output file.
#[test]
fn single_file_executable_attributes_take_genrule_outputs() {
    check_group("single_file");
}

/// Explicit native errors with a recorded difference.
#[test]
fn declared_divergences_are_explicit_errors() {
    check_group("declared");
}
