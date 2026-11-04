//! The native `starlark_doc_extract` rule (docs/STARLARK_DOC_EXTRACT.md)
//! against the genuine Bazel 9.3.1 captures of
//! tests/fixtures/starlark_doc_extract (reference_receipt.json): the
//! binary and text ModuleInfo protos byte for byte, and the rule's errors.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/starlark_doc_extract")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("doc-extract-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&fixture().join("ws"), ws.0.path());
        seed_module_archives(
            &ws.0.path().join("cache"),
            &[
                "bazel_skylib@1.8.2",
                "platforms@1.0.0",
                "rules_license@1.0.0",
            ],
        );
        ws
    }

    fn build(&self, targets: &[&str]) -> Output {
        let registry = builtin_registry_flag();
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "build"])
            .args(["--experimental-builtin-modules", registry.as_str()])
            .args(targets)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(
            output.status.code(),
            Some(124),
            "build timed out: {targets:?}"
        );
        output
    }

    /// The bytes of a built output (`PKG/FILE` under bazel-bin) from the
    /// build's output listing.
    fn output(&self, listing: &str, path: &str) -> Vec<u8> {
        let built = listing
            .lines()
            .find_map(|line| {
                let candidate = line.split('\t').nth(1).unwrap_or(line).trim();
                candidate
                    .ends_with(&format!("/bin/{path}"))
                    .then(|| candidate.to_owned())
            })
            .unwrap_or_else(|| panic!("no output {path} in {listing}"));
        fs::read(built).unwrap()
    }

    fn shown(&self, text: &str) -> String {
        let workspace = fs::canonicalize(self.0.path()).unwrap();
        text.replace(&workspace.display().to_string(), "<WS>")
            .replace(&self.0.path().display().to_string(), "<WS>")
    }
}

fn receipt() -> serde_json::Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
}

#[test]
fn starlark_doc_extract_outputs_match_genuine() {
    let receipt = receipt();
    let labels: Vec<String> = receipt["outputs"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let mut targets = Vec::new();
    for label in &labels {
        targets.push(label.clone());
        targets.push(format!("{label}.textproto"));
    }
    let ws = Workspace::new();
    let output = ws.build(&targets.iter().map(String::as_str).collect::<Vec<_>>());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let listing = format!("{}\n{stderr}", String::from_utf8_lossy(&output.stdout));
    for label in &labels {
        let (package, name) = label.trim_start_matches("//").split_once(':').unwrap();
        for extension in ["binaryproto", "textproto"] {
            let native = ws.output(&listing, &format!("{package}/{name}.{extension}"));
            let genuine = fs::read(
                fixture()
                    .join("genuine")
                    .join(format!("{package}_{name}.{extension}")),
            )
            .unwrap();
            if extension == "textproto" {
                assert_eq!(
                    String::from_utf8_lossy(&native),
                    String::from_utf8_lossy(&genuine),
                    "{label}.{extension}"
                );
            } else {
                assert!(native == genuine, "{label}.{extension} differs");
            }
        }
    }
}

#[test]
fn starlark_doc_extract_errors_match_genuine() {
    let receipt = receipt();
    let ws = Workspace::new();
    for (label, expected) in receipt["errors"].as_object().unwrap() {
        let output = ws.build(&[label]);
        assert_eq!(output.status.code(), Some(1), "{label}");
        let stderr = ws.shown(&String::from_utf8_lossy(&output.stderr));
        let message = format!(
            "{}: {}",
            expected["location"].as_str().unwrap(),
            expected["message"].as_str().unwrap()
        );
        assert!(
            stderr.contains(&message),
            "{label}:\n{stderr}\nexpected:\n{message}"
        );
    }
}

/// Doc-only symbolic macro features load (their definitions are
/// documented) but an invocation fails explicitly.
#[test]
fn macro_with_attributes_loads_but_invocation_is_unsupported() {
    let ws = Workspace::new();
    fs::write(
        ws.0.path().join("pkg/BUILD"),
        "load(':defs.bzl', 'my_macro')\nmy_macro(name = 'm')\n",
    )
    .unwrap();
    let output = ws.build(&["//pkg:all"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("macro user attributes are unsupported"),
        "{stderr}"
    );
}
