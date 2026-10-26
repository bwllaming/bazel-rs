//! MODULE.bazel `print()` (Bazel 9.3 ModuleFileFunction.java:244-246,
//! 376-378, 670-674): a no-op for a module from a registry — "We don't want
//! them to be able to spam the console during resolution", e.g. the old
//! gazelle 0.27.0 / rules_go 0.33.0 registry MODULE files' "still highly
//! experimental" WARNING — while the root module and a module with a
//! non-registry override (local_path_override) print through
//! Event.makeDebugPrintHandler: `DEBUG: FILE:LINE:COL: TEXT`, at the call's
//! left parenthesis. tests/fixtures/module_print/reference_receipt.json is
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) running `build --nobuild //:all` twice on one copy of
//! the fixture workspace; both native runs (cold, then warm) print the same
//! DEBUG lines in the same order and nothing of the registry module.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/module_print")
}

fn receipt() -> Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
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

/// The genuine platforms 1.0.0 module (as in tests/py_repo_mapping.rs).
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

fn debug_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.starts_with("DEBUG: "))
        .collect()
}

#[test]
fn registry_module_prints_are_noops_root_and_override_prints_are_debug_events() {
    let receipt = receipt();
    // The receipt was captured on exactly these fixture files.
    for (path, sha256) in receipt["fixture_files_sha256"].as_object().unwrap() {
        let digest = format!(
            "{:x}",
            Sha256::digest(fs::read(fixture().join(path)).unwrap())
        );
        assert_eq!(
            digest,
            sha256.as_str().unwrap(),
            "{path} changed since the capture"
        );
    }
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let scratch = tempfile::Builder::new()
        .prefix("module-print-")
        .tempdir_in(base)
        .unwrap();
    let ws = scratch.path().join("ws");
    copy(&fixture().join("workspace"), &ws);
    extract_platforms(&ws.join("platforms"));
    let mut paths = vec![ws.display().to_string()];
    if let Ok(real) = fs::canonicalize(&ws) {
        paths.push(real.display().to_string());
    }
    let runs = receipt["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    for (index, genuine) in runs.iter().enumerate() {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                scratch.path().join("cache").display()
            ))
            .args(["build", "--nobuild", "--experimental-builtin-modules"])
            .arg(format!(
                "--registry=file://{}/tests/fixtures/builtin_modules/registry",
                manifest().display()
            ))
            .arg(format!(
                "--registry=file://{}/registry",
                fixture().display()
            ))
            .args(["--repository_disable_download", "//:all"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        for path in &paths {
            stderr = stderr.replace(path, "<WS>");
        }
        let run = format!("run {}", index + 1);
        assert_eq!(
            output.status.code(),
            genuine["exit"].as_i64().map(|code| code as i32),
            "{run}: {stderr}"
        );
        let expected = genuine["stderr"].as_str().unwrap();
        assert_eq!(
            debug_lines(&stderr),
            debug_lines(expected),
            "{run}: {stderr}"
        );
        assert_eq!(debug_lines(&stderr).len(), 3, "{run}: {stderr}");
        let registry_print = "The printdep Bazel module is still highly experimental";
        assert!(
            !stderr.contains(registry_print) && !expected.contains(registry_print),
            "{run}: a registry module's print() is a no-op: {stderr}"
        );
    }
}
