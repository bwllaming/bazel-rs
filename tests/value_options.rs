//! Table-driven native options (crate::configuration::VALUE_OPTIONS):
//! `config_setting(values)` matching through each option's converter, the
//! command-line forms of ProtoConfiguration's
//! `--experimental_proto_descriptor_sets_include_source_info`,
//! `--strict_proto_deps` and `--strict_public_imports`, the exec transition
//! (propagated / default / set), the identity of explicit defaults, a
//! transitioned `collect_code_coverage` root, `apple_platform_type`, and
//! `query`'s `Unrecognized option` for other fragments' options. Every case
//! of tests/fixtures/value_options/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) on the fixture workspace — runs natively here and is compared with
//! the genuine capture: success, the WARNING lines, Bazel's first ERROR
//! message, the built outputs (so the output roots) and `query`'s stdout;
//! a declared divergence is a native failure with its recorded message.
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
    manifest().join("tests/fixtures/value_options/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/value_options/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "proto_command_line",
        &[
            "proto_defaults",
            "proto_src_info",
            "proto_src_info_yes",
            "proto_src_info_neg",
            "proto_strict_off",
            "proto_strict_upper",
            "proto_strict_separate",
            "proto_strict_explicit_default",
            "proto_strict_default_constant",
            "proto_public_warn",
            "proto_last_wins",
            "proto_nobuild",
        ],
    ),
    (
        "proto_errors",
        &[
            "proto_src_info_bad",
            "proto_src_info_neg_value",
            "proto_strict_bad",
            "proto_strict_no_prefix",
            "proto_setting_bad_enum",
            "proto_setting_bad_bool",
        ],
    ),
    (
        "proto_exec_and_external",
        &[
            "proto_exec",
            "proto_ext_defaults",
            "proto_ext_set",
            "proto_ext_warn",
        ],
    ),
    (
        "query",
        &[
            "proto_query",
            "proto_query_bool",
            "proto_query_neg",
            "proto_query_public",
            "coverage_query",
            "apple_query",
        ],
    ),
    (
        "coverage",
        &[
            "coverage_defaults",
            "coverage_transition",
            "coverage_transition_off",
            "coverage_setting_bad",
            // A command-line option since `bazel coverage` (docs/COVERAGE.md).
            "coverage_cmdline",
        ],
    ),
    ("apple", &["apple_defaults"]),
    // CoreOptions.enable_runfiles (tests/fixtures/enable_runfiles_option);
    // the case keeps the name it had before the option was modelled.
    ("enable_runfiles", &["unmodelled_enable_runfiles"]),
    ("combined", &["all_defaults", "all_set"]),
    ("declared", &["apple_cmdline", "proto_transition"]),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("value-options-")
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
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
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

/// Bazel's first `ERROR: ` message; an external repository's location (the
/// genuine output base) is reduced to its BUILD file name.
fn first_error(stderr: &str) -> Option<String> {
    let message = stderr
        .lines()
        .find_map(|line| line.strip_prefix("ERROR: "))?;
    Some(match message.strip_prefix("<OB>/external/") {
        Some(rest) => rest
            .split_once('/')
            .map_or(rest, |(_, rest)| rest)
            .to_owned(),
        None => message.to_owned(),
    })
}

/// The built outputs (path below `bazel-out` -> contents) of the native
/// `LABEL\tPATH` result lines.
fn outputs(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .map(|line| {
            let (_, path) = line.split_once('\t').unwrap();
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            (relative.to_owned(), fs::read_to_string(path).unwrap())
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
        // Every declared divergence is a native failure with its own text
        // where genuine Bazel succeeds.
        assert_eq!(genuine["exit"].as_i64(), Some(0), "{name}");
        assert!(!output.status.success(), "{name}: {stderr}");
        let message = case["native_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        return;
    }
    assert_eq!(case["verdict"], "MATCH", "{name}");
    assert_eq!(
        output.status.success(),
        genuine["exit"].as_i64() == Some(0),
        "{name} {argv:?}: {stderr}"
    );
    assert_eq!(
        warnings(&stderr),
        warnings(genuine_stderr),
        "{name}: {stderr}"
    );
    if let Some(error) = first_error(genuine_stderr) {
        assert!(
            stderr.lines().any(|line| line.ends_with(&error)),
            "{name}: missing {error:?} in {stderr}"
        );
    }
    let executed =
        argv[0] == "build" && output.status.success() && !argv.iter().any(|arg| arg == "--nobuild");
    if executed {
        let expected: BTreeMap<String, String> = genuine["outputs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, content)| (path.clone(), content.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(outputs(&stdout), expected, "{name}: {stderr}");
    }
    if argv[0] == "query" && output.status.success() {
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

/// `--experimental_proto_descriptor_sets_include_source_info`
/// (BooleanConverter), `--strict_proto_deps`/`--strict_public_imports`
/// (StrictDepsMode, case-insensitive; `default` is a constant of its own);
/// explicit defaults are the default configuration; the last value wins.
#[test]
fn proto_options_on_the_command_line_match_genuine() {
    check_group("proto_command_line");
}

/// Converter errors of the command line and of config_setting values.
#[test]
fn proto_conversion_errors_match() {
    check_group("proto_errors");
}

/// The proto options propagate to exec configurations; protobuf's
/// native_bool_flag pattern in an external module.
#[test]
fn proto_options_reach_exec_tools_and_external_settings() {
    check_group("proto_exec_and_external");
}

/// `query` registers only CoreOptions: the other fragments' options are
/// unknown, `--collect_code_coverage` is accepted.
#[test]
fn query_rejects_other_fragments_options() {
    check_group("query");
}

/// `collect_code_coverage` in the default, a transitioned (ST- root) and an
/// exec configuration.
#[test]
fn collect_code_coverage_matches_its_configured_value() {
    check_group("coverage");
}

/// `apple_platform_type` is `macos`, also in exec configurations.
#[test]
fn apple_platform_type_is_macos() {
    check_group("apple");
}

/// config_setting on CoreOptions.enable_runfiles (aspect_bazel_lib's
/// //lib:enable_runfiles) does not match `true` at the default `auto`.
#[test]
fn enable_runfiles_true_does_not_match_the_default() {
    check_group("enable_runfiles");
}

/// Every setting at once, with and without the proto options set.
#[test]
fn all_settings_at_once() {
    check_group("combined");
}

/// Explicit native errors where genuine Bazel succeeds.
#[test]
fn declared_divergences_are_explicit_errors() {
    check_group("declared");
}
