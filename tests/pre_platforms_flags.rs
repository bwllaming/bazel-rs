//! The deprecated pre-platforms flags `--cpu`, `--host_cpu` and
//! `--crosstool_top` (ConfigSetting.DEPRECATED_PRE_PLATFORMS_FLAGS, Bazel 9.3
//! rules/config/ConfigSetting.java:95-97, 420-482): `config_setting(values)`
//! matching through the options' converters, the main-repository-only
//! deprecation warning and its position, conversion errors, and the
//! command-line options (output roots, the exec transition's `--cpu`,
//! `query`). Every case of
//! tests/fixtures/pre_platforms_flags/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) on the fixture workspace — runs natively here and is compared with
//! the genuine capture: success, the WARNING lines (per config_setting in
//! report order: Skyframe evaluates different config_settings in parallel),
//! warnings before the first error, Bazel's first ERROR message, the built
//! outputs and `query`'s stdout.
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
    manifest().join("tests/fixtures/pre_platforms_flags/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/pre_platforms_flags/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "crosstool_top",
        &[
            "crosstool_default",
            "crosstool_canonical",
            "crosstool_custom_unset",
            "crosstool_custom_set",
            "crosstool_default_when_set",
            "crosstool_relative_set",
            "crosstool_colon_root",
            "crosstool_ext_apparent_set",
            "crosstool_ext_canonical_set",
            "crosstool_explicit_default",
            "crosstool_flag_unknown_repo",
        ],
    ),
    (
        "cpu",
        &[
            "cpu_k8",
            "cpu_arm_unset",
            "cpu_arm_set",
            "cpu_k8_when_arm",
            "cpu_empty",
            "cpu_flag_k8",
            "cpu_flag_empty",
            "host_cpu_k8",
            "host_cpu_ppc_unset",
            "host_cpu_ppc_set",
        ],
    ),
    (
        "errors",
        &[
            "crosstool_flag_empty",
            "crosstool_flag_bad",
            "cpu_flag_slash",
            "crosstool_bad",
            "crosstool_empty",
            "error_then_cpu",
            "query_crosstool_top",
        ],
    ),
    (
        "several",
        &[
            "mixed",
            "all_default",
            "all_set",
            "nobuild_mixed",
            "query_cpu",
        ],
    ),
    (
        "external",
        &[
            "ext_conditions",
            "ext_conditions_set",
            "ext_p_all",
            "ext_p_bad",
            "ext_p_main",
        ],
    ),
    (
        "exec",
        &["exec_default", "exec_cpu_arm", "exec_host_cpu_ppc"],
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
                .prefix("pre-platforms-flags-")
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

/// Warnings by the config_setting they name (`""`: any other warning), each
/// group in report order.
fn by_setting<'a>(warnings: &[&'a str]) -> BTreeMap<&'a str, Vec<&'a str>> {
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for line in warnings {
        let setting = line
            .split_once("in config_setting rule ")
            .and_then(|(_, rest)| rest.split_once(": "))
            .map_or("", |(label, _)| label);
        groups.entry(setting).or_default().push(line);
    }
    groups
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
    assert_eq!(
        output.status.success(),
        genuine["exit"].as_i64() == Some(0),
        "{name} {argv:?}: {stderr}"
    );
    let expected = warnings(genuine_stderr);
    let actual = warnings(&stderr);
    assert_eq!(
        by_setting(&actual),
        by_setting(&expected),
        "{name}: {stderr}"
    );
    if let Some(error) = first_error(genuine_stderr) {
        let lines = stderr.lines().collect::<Vec<_>>();
        let position = lines.iter().position(|line| line.ends_with(&error));
        assert!(position.is_some(), "{name}: missing {error:?} in {stderr}");
        // Rule warnings precede the errors (Skyframe replays errors).
        if let Some(last) = lines.iter().rposition(|line| line.starts_with("WARNING: ")) {
            assert!(last < position.unwrap(), "{name}: {stderr}");
        }
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
    for (name, case) in receipt["cases"].as_object().unwrap() {
        assert_eq!(case["verdict"], "MATCH", "{name}");
    }
}

/// `--crosstool_top` (CppOptions.java:102-111, LabelConverter): expected
/// values are converted with `Label.parseCanonical` after the `//` prefix.
#[test]
fn crosstool_top_matches_through_the_canonical_label_converter() {
    check_group("crosstool_top");
}

/// `--cpu`/`--host_cpu` (CoreOptions.java:279-292, 387-394;
/// AutoCpuConverter): the host CPU by default (`""` included), the CPU part
/// of target output roots.
#[test]
fn cpu_and_host_cpu_match_through_the_auto_cpu_converter() {
    check_group("cpu");
}

/// Conversion errors (the setting's warning first) and invalid options.
#[test]
fn conversion_errors_follow_the_warning() {
    check_group("errors");
}

/// Several flags in one config_setting warn in `values` order; several
/// config_settings warn once each; `--nobuild` analyses (and warns) too;
/// `query` accepts the CoreOptions.
#[test]
fn several_flags_and_settings_warn_once_each() {
    check_group("several");
}

/// External repositories' config_settings (like @protobuf//build_defs's)
/// match without a warning; a main-repository setting selected from an
/// external repository warns.
#[test]
fn external_config_settings_never_warn() {
    check_group("external");
}

/// The exec configuration's `--cpu` is the parent's `--host_cpu`
/// (builtin_exec_platforms.bzl:225), naming default-named exec roots.
#[test]
fn exec_configuration_cpu_is_the_host_cpu() {
    check_group("exec");
}
