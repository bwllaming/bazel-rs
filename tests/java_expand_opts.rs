//! JavaStarlarkCommon.expandJavaOpts: genuine captured attribute expansion,
//! consumed provider/command-line values, diagnostics and snapshot invalidation.
#![allow(clippy::disallowed_methods)]
use std::collections::BTreeMap;
use std::fs;
use std::process::{Command, Output};
include!("support/temp_root.rs");

const BUILD: &str = include_str!("fixtures/java_expand_opts/workspace/BUILD.bazel");
const DEFS: &str =
    include_str!("fixtures/java_expand_opts/workspace/bazel_internal/test_rules/defs.bzl");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        for (name, text) in [
            (
                "MODULE.bazel",
                include_str!("fixtures/java_expand_opts/workspace/MODULE.bazel"),
            ),
            ("BUILD.bazel", BUILD),
            ("bazel_internal/test_rules/BUILD.bazel", ""),
            ("bazel_internal/test_rules/defs.bzl", DEFS),
            ("source.txt", "source\n"),
            ("data.txt", "data\n"),
        ] {
            ws.write(name, text);
        }
        ws
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        let result = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path())
            .args(["--ignore_all_rc_files", "--cache-dir=cache", command])
            .args([
                "--experimental-builtin-modules=false",
                "--host_platform=//:host",
                "--platforms=//:host",
                "--define=CUSTOM=custom words",
                "--define=CHOICE=define",
            ])
            .args(args)
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .output()
            .unwrap();
        assert!(!errors(&result).contains("panicked"), "{}", errors(&result));
        result
    }

    fn build(&self, args: &[&str]) -> (BTreeMap<String, String>, String) {
        let result = self.run("build", args);
        assert!(result.status.success(), "{}", errors(&result));
        let files = String::from_utf8_lossy(&result.stdout)
            .lines()
            .map(|line| {
                let (_, path) = line.split_once('\t').unwrap();
                let name = std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                (name, fs::read_to_string(path).unwrap())
            })
            .collect();
        (files, errors(&result))
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn receipt() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "fixtures/java_expand_opts/reference_receipt.json"
    ))
    .unwrap()
}

#[test]
fn java_options_match_genuine_and_feed_actions_and_providers() {
    let ws = Workspace::new();
    let (files, _) = ws.build(&[
        "//:tokens",
        "//:raw",
        "//:consumed",
        "//:locations",
        "//:exec_locations",
        "//:default_paths",
        "//:resolved",
        "//:generated_runfiles",
        "//:generated_exec",
        "//:builtin_tags",
    ]);
    let reference = receipt();
    assert_eq!(files.len(), 10);
    for (file, expected) in reference["outputs"].as_object().unwrap() {
        let actual: serde_json::Value = serde_json::from_str(&files[file]).unwrap();
        assert_eq!(&actual, expected, "{file}");
    }
    assert_eq!(
        files["consumed.args"],
        reference["consumed.args"].as_str().unwrap()
    );
}

#[test]
fn java_options_validate_binding_attribute_types_and_context_lifetime() {
    let ws = Workspace::new();
    let reference = receipt();
    for group in ["errors", "genuine_crashes_native_errors"] {
        for (case, expected) in reference[group].as_object().unwrap() {
            let result = ws.run("build", &[&format!("//:{case}")]);
            assert_eq!(result.status.code(), Some(1), "{case}: {}", errors(&result));
            assert!(result.stdout.is_empty());
            assert!(
                errors(&result).contains(expected.as_str().unwrap()),
                "{case}: {}",
                errors(&result)
            );
        }
    }
    let escaped = ws.run("build", &["//:escaped"]);
    assert_eq!(escaped.status.code(), Some(1));
    assert!(errors(&escaped).contains("unsupported provider payload type ctx"));
}

#[test]
fn java_options_cquery_does_not_execute_actions_or_workspace_status() {
    let ws = Workspace::new();
    let result = ws.run(
        "cquery",
        &["deps(//:consumed)", "--workspace_status_command=/bin/false"],
    );
    assert!(result.status.success(), "{}", errors(&result));
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("//:consumed"));
    assert!(!ws.0.path().join("cache/actions").exists());
    assert!(!errors(&result).contains("actions executed"));
}

#[test]
fn java_options_snapshots_track_source_configuration_and_toolchain_values() {
    let ws = Workspace::new();
    let args = ["//:tokens", "//:resolved"];
    let (cold, _) = ws.build(&args);
    let (warm, log) = ws.build(&args);
    assert_eq!(cold, warm);
    assert!(log.contains("bazel-rs-snapshot: hit"), "{log}");

    ws.write(
        "BUILD.bazel",
        &BUILD.replace("-A 'two words' plain", "-B 'two words' plain"),
    );
    let (source, log) = ws.build(&args);
    assert!(!log.contains("bazel-rs-snapshot: hit"), "{log}");
    assert!(source["tokens.json"].contains("-B"));

    let (configured, log) = ws.build(&[
        "//:tokens",
        "//:resolved",
        "--define=CUSTOM=changed",
        "-c",
        "opt",
    ]);
    assert!(!log.contains("bazel-rs-snapshot: hit"), "{log}");
    assert!(configured["tokens.json"].contains("changed"));
    assert!(configured["resolved.json"].contains("k8-opt/bin"));

    ws.write(
        "BUILD.bazel",
        &BUILD.replace("tool value", "new toolchain value"),
    );
    let (toolchain, log) = ws.build(&args);
    assert!(!log.contains("bazel-rs-snapshot: hit"), "{log}");
    assert!(toolchain["resolved.json"].starts_with("[\"new\",\"toolchain\",\"value\""));
    assert!(toolchain["tokens.json"].contains("\"new\",\"toolchain\",\"value\""));
}
