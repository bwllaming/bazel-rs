//! The native `genquery` rule (docs/AQUERY.md) against the genuine Bazel
//! 9.3.1 captures of tests/fixtures/genquery (reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/genquery")
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
                .prefix("genquery-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&fixture().join("ws"), ws.0.path());
        seed_module_archives(&ws.0.path().join("cache"), &["platforms@1.0.0"]);
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

    /// The bytes of a built genquery target's output file.
    fn raw(&self, stdout: &str, name: &str) -> Vec<u8> {
        let path = stdout
            .lines()
            .find_map(|line| {
                let path = line.split('\t').nth(1).unwrap_or(line).trim();
                path.ends_with(&format!("/bin/pkg/{name}"))
                    .then(|| path.to_owned())
            })
            .unwrap_or_else(|| panic!("no output of {name} in {stdout}"));
        fs::read(path).unwrap()
    }

    /// The output file of a built genquery target, the workspace path as `<WS>`.
    fn output(&self, stdout: &str, name: &str) -> Vec<u8> {
        let bytes = self.raw(stdout, name);
        let workspace = fs::canonicalize(self.0.path()).unwrap();
        String::from_utf8_lossy(&bytes)
            .replace(&workspace.display().to_string(), "<WS>")
            .replace(&self.0.path().display().to_string(), "<WS>")
            .into_bytes()
    }
}

fn genuine(name: &str) -> Vec<u8> {
    fs::read(fixture().join("genuine").join(name)).unwrap()
}

#[test]
fn genquery_outputs_match_genuine() {
    let ws = Workspace::new();
    let output = ws.build(&["//pkg:qk", "//pkg:qb", "//pkg:qp", "//pkg:qz"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // `build` prints the output paths (one per line).
    let stdout = format!("{}\n{stderr}", String::from_utf8_lossy(&output.stdout));
    for name in ["qk", "qb", "qp"] {
        assert_eq!(
            String::from_utf8_lossy(&ws.output(&stdout, name)),
            String::from_utf8_lossy(&genuine(name)),
            "{name}"
        );
    }
    // compressed_output: a gzip stream of the same output.
    let compressed = ws.raw(&stdout, "qz");
    assert_eq!(&compressed[..2], b"\x1f\x8b");
    let mut plain = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(&compressed[..]),
        &mut plain,
    )
    .unwrap();
    assert_eq!(plain, genuine("qz.uncompressed"));
}

#[test]
fn genquery_scope_is_enforced() {
    let ws = Workspace::new();
    for (target, message) in [
        (
            "//pkg:qx",
            "in genquery rule //pkg:qx: query failed: Evaluation failed: target '//pkg:a' is not within the scope of the query",
        ),
        (
            "//pkg:qw",
            "in genquery rule //pkg:qw: query failed: Evaluation failed: target '//pkg:BUILD' is not within the scope of the query",
        ),
    ] {
        let output = ws.build(&[target]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{target}: {stderr}");
        assert!(stderr.contains(message), "{target}: {stderr}");
    }
    // strict = False: a warning, and the out-of-scope target is skipped.
    let output = ws.build(&["//pkg:qns"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("WARNING: target '//pkg:a' is not within the scope of the query. Skipping"),
        "{stderr}"
    );
}
