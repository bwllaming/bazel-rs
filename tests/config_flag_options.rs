//! The configuration flag options on the command line: `--features`/
//! `--host_features` (ctx.features, ctx.disabled_features), the CppOptions
//! flag lists and `--dynamic_mode` (ctx.fragments.cpp), `--javacopt`/
//! `--host_javacopt` (ctx.fragments.java.default_javac_flags[_depset]), in
//! the target configuration, the exec configuration and after a Starlark
//! transition (output path ST- hash included). Every case replays the
//! genuine Bazel 9.3.1 capture tests/fixtures/config_flag_options/reference.json
//! (written by capture.py beside it).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/config_flag_options")
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &destination)
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        copy(&fixture().join("workspace"), workspace.0.path());
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.0.path().join(directory)).unwrap();
        }
        workspace
    }

    fn build(&self, flags: &[String], targets: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("BAZEL_RS_CACHE_GC", "off")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:p",
                "--host_platform=//:p",
                "--memory-limit=2048",
            ])
            .args(flags)
            .args(targets)
            .output()
            .unwrap()
    }
}

fn reference() -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(fixture().join("reference.json")).unwrap()).unwrap()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn configuration_flag_options_match_genuine() {
    let reference = reference();
    for (name, case) in reference["cases"].as_object().unwrap() {
        let workspace = Workspace::new();
        let output = workspace.build(
            &strings(&case["flags"]),
            &["//:top", "//:plain", "//:moved"],
        );
        assert_eq!(
            output.status.code(),
            case["exit"].as_i64().map(|code| code as i32),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // `label\tpath` result lines: each report's path below bazel-out.
        let mut actual = BTreeMap::new();
        for line in String::from_utf8(output.stdout).unwrap().lines() {
            let Some((_, path)) = line.split_once('\t') else {
                continue;
            };
            let relative = path
                .split_once("bazel-out/")
                .map(|(_, relative)| relative.to_owned())
                .unwrap();
            actual.insert(relative, fs::read_to_string(path).unwrap());
        }
        let expected: BTreeMap<String, String> = case["outputs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, content)| (path.clone(), content.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn configuration_flag_option_errors_match_genuine() {
    let reference = reference();
    let workspace = Workspace::new();
    for (name, case) in reference["errors"].as_object().unwrap() {
        let output = workspace.build(&strings(&case["flags"]), &["//:plain"]);
        assert_eq!(output.status.code(), Some(2), "{name}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The first error (genuine's `Build did NOT complete successfully`
        // summary line has no native counterpart on an option error).
        for error in strings(&case["errors"]).into_iter().take(1) {
            let message = error.strip_prefix("ERROR: ").unwrap();
            // The regex engine's own detail differs (docs/CONFIGURATION.md).
            let message = message
                .split_once(": Failed to build valid regular expression")
                .map_or(message, |(prefix, _)| prefix);
            assert!(stderr.contains(message), "{name}: {stderr}");
        }
    }
}

#[test]
fn query_accepts_core_features_but_not_cpp_flags() {
    let workspace = Workspace::new();
    let query = |flag: &str| {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(workspace.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", workspace.0.path().join("home"))
            .env("TMPDIR", workspace.0.path().join("tmp"))
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "query",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                flag,
                "//:plain",
            ])
            .output()
            .unwrap()
    };
    // CoreOptions: registered (and ignored) by query (QueryCommand.java:59-67).
    let features = query("--features=race");
    assert!(features.status.success(), "{features:?}");
    // CppOptions/JavaOptions: `Unrecognized option` (exit 2).
    for flag in ["--copt=-O2", "--javacopt=-g", "--dynamic_mode=off"] {
        let output = query(flag);
        assert_eq!(output.status.code(), Some(2), "{flag}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("{flag} :: Unrecognized option: {flag}")),
            "{flag}: {output:?}"
        );
    }
}
