//! `aquery` of the native Java actions against genuine Bazel 9.3.1
//! (tests/fixtures/java_action_parity, reference_receipt.json): Javac with
//! deps/exports/resources/annotation processors, Turbine and the
//! annotation-processing JavacTurbine header compilation, GenClass,
//! JavaResourceJar, JavaSourceJar and JavaIjar, with their descriptions,
//! command lines, inputs, environment, execution info and ActionKeys.
//! The genuine capture used the real BCR rules_java 9.1.0; this test the
//! same rules_java Starlark offline (tests/support/java_scaffold.rs) with the
//! fixture's main-repository java_toolchain of fail-if-executed tools.
//! Normalized: the configuration checksum (native ConfigIds), block order.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
include!("support/module_archives.rs");
include!("support/java_scaffold.rs");

use std::fs;
use std::path::Path;
use std::process::Command;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java_action_parity")
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// The text output's action blocks with the configuration checksum
/// replaced, sorted.
fn blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = text
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            block
                .lines()
                .map(|line| {
                    if line.starts_with("# Configuration: ") {
                        "# Configuration: <checksum>"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    blocks.sort();
    blocks
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        copy(&fixture().join("ws"), ws.0.path());
        let module = "module(name='java_action_parity')\n".to_owned() + &java_scaffold(ws.0.path());
        fs::write(ws.0.path().join("MODULE.bazel"), module).unwrap();
        for dir in ["home", "tmp"] {
            fs::create_dir_all(ws.0.path().join(dir)).unwrap();
        }
        ws
    }

    fn aquery(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "aquery",
                "--lockfile_mode=update",
                "--repository_disable_download",
                "--extra_toolchains=//:registered",
                "--memory-limit=2048",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

fn assert_parity(native: &str, genuine: &str) {
    let (native, genuine) = (blocks(native), blocks(genuine));
    let mnemonic = |block: &String| {
        block
            .lines()
            .find_map(|line| line.strip_prefix("  Mnemonic: "))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        native.iter().map(mnemonic).collect::<Vec<_>>(),
        genuine.iter().map(mnemonic).collect::<Vec<_>>()
    );
    for (native, genuine) in native.iter().zip(&genuine) {
        assert_eq!(native, genuine);
    }
}

#[test]
fn java_actions_match_genuine_aquery_including_action_keys() {
    let ws = Workspace::new();
    let genuine = |name: &str| fs::read_to_string(fixture().join("genuine").join(name)).unwrap();
    assert_parity(&ws.aquery(&["//pkg:all"]), &genuine("all.txt"));
}
