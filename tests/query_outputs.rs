//! `query --output=xml|proto|streamed_proto|streamed_jsonproto` and SkyQuery
//! (`--universe_scope`, `--infer_universe_scope`, `allrdeps`, `rbuildfiles`)
//! against genuine Bazel 9.3.0 captures (docs/QUERY.md "Structured outputs"
//! and "SkyQuery"). Every case of
//! tests/fixtures/query_outputs/reference_receipt.json (captured by
//! tests/fixtures/query_outputs/capture.py over tests/fixtures/query_outputs/ws)
//! must reproduce the genuine stdout byte for byte (the workspace path
//! normalized to `${WORKSPACE}`), exit code and ERROR/WARNING/INFO events.
//! Protobuf outputs are deterministic in Bazel (attributes sorted, targets in
//! label order), so they are compared byte-exact; `--order_output=no`
//! SkyQuery output is compared as a set (Bazel's parallel visitation order).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/module_archives.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/query_outputs")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "bazel-rs-query-outputs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(&fixture().join("ws"), &path);
        Self(fs::canonicalize(path).unwrap())
    }

    fn query(&self, args: &[String]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.join("bazel-rs-out").display()
            ))
            .arg("query")
            .arg(builtin_registry_flag())
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn events(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| {
            (line.starts_with("ERROR:")
                || line.starts_with("WARNING:")
                || line.starts_with("INFO:"))
                && !line.contains("Invocation ID")
        })
        .map(str::to_owned)
        .collect()
}

fn replace(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if !needle.is_empty() && haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn sorted_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = bytes
        .split(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    lines.sort();
    lines
}

/// Every genuine capture, replayed natively.
#[test]
fn genuine_captures_match() {
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap())
            .unwrap();
    let ws = Workspace::new();
    let mut failures = Vec::new();
    let cases = receipt["cases"].as_object().unwrap();
    assert!(cases.len() >= 40, "{} cases", cases.len());
    for (id, case) in cases {
        let args = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let genuine = fs::read(fixture().join("genuine").join(format!("{id}.out"))).unwrap();
        let output = ws.query(&args);
        let stdout = replace(
            &output.stdout,
            ws.0.display().to_string().as_bytes(),
            b"${WORKSPACE}",
        );
        let stderr = String::from_utf8_lossy(&output.stderr)
            .replace(&ws.0.display().to_string(), "${WORKSPACE}");
        let expected_events = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let unordered = args.iter().any(|arg| arg == "--order_output=no");
        let same_stdout = if unordered {
            sorted_lines(&stdout) == sorted_lines(&genuine)
        } else {
            stdout == genuine
        };
        let code = output.status.code().unwrap_or(-1);
        if !same_stdout
            || i64::from(code) != case["exit_code"].as_i64().unwrap()
            || events(&stderr) != expected_events
        {
            failures.push(format!(
                "{id} {args:?}: exit {code} (genuine {}), events {:?} (genuine {expected_events:?}), stdout equal: {same_stdout}\n--- native stdout ---\n{}\n--- stderr ---\n{stderr}",
                case["exit_code"],
                events(&stderr),
                String::from_utf8_lossy(&stdout).chars().take(3000).collect::<String>()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The receipt pins the fixture it was captured from.
#[test]
fn receipt_matches_fixture() {
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap())
            .unwrap();
    for (path, digest) in receipt["fixture_sha256"].as_object().unwrap() {
        use sha2::Digest;
        let bytes = fs::read(fixture().join("ws").join(path)).unwrap();
        let actual = format!("{:x}", sha2::Sha256::digest(&bytes));
        assert_eq!(&actual, digest.as_str().unwrap(), "{path}");
    }
    for (id, case) in receipt["cases"].as_object().unwrap() {
        use sha2::Digest;
        let bytes = fs::read(fixture().join("genuine").join(format!("{id}.out"))).unwrap();
        let actual = format!("{:x}", sha2::Sha256::digest(&bytes));
        assert_eq!(&actual, case["stdout_sha256"].as_str().unwrap(), "{id}");
    }
}
