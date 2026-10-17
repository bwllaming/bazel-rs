//! N49: depsets of `struct()` values and Starlark provider instances
//! (docs/DEPSETS.md "Struct and provider elements (N49)"): Bazel's strict
//! element check, StructImpl equality dedup within and across targets in
//! every order, nested depsets/None/Target/runfiles/global-def fields, the
//! element type errors and the explicit gaps. The expected reports are equal
//! to genuine Bazel 9.3.0's after D1 (tests/fixtures/depset_structs/reference_receipt.json).
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn fixture() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n49-")
                .tempdir_in(base)
                .unwrap(),
        );
        let source = manifest().join("tests/fixtures/depset_structs/workspace/ds");
        fs::create_dir_all(ws.0.path().join("ds")).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            let path = entry.unwrap().path();
            fs::copy(
                &path,
                ws.0.path().join("ds").join(path.file_name().unwrap()),
            )
            .unwrap();
        }
        fs::write(
            ws.0.path().join("MODULE.bazel"),
            "module(name = \"depset_structs\")\n",
        )
        .unwrap();
        ws
    }

    fn run(&self, targets: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=private-cache",
                "build",
                "--experimental-builtin-modules",
            ])
            .arg(format!(
                "--registry=file://{}/tests/fixtures/builtin_modules/registry",
                manifest().display()
            ))
            .args(targets)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }
}

#[test]
fn struct_and_provider_elements_follow_bazel() {
    let ws = Workspace::fixture();
    let mut report = String::new();
    for target in ["r_default", "r_post", "r_pre", "r_topo"] {
        let label = format!("//ds:{target}");
        let output = ws.run(&[&label]);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        report.push_str(&format!("{label}\n"));
        for line in fs::read_to_string(path).unwrap().lines() {
            report.push_str(&format!("  {line}\n"));
        }
    }
    assert_eq!(
        report,
        fs::read_to_string(manifest().join("tests/fixtures/depset_structs/expected.txt")).unwrap()
    );
}

#[test]
fn element_checks_and_gaps() {
    let ws = Workspace::fixture();
    for (kind, message) in [
        (
            "mutable_field",
            "depset elements must not be mutable values",
        ),
        ("list", "depset elements must not be mutable values"),
        ("dict", "depset elements must not be mutable values"),
        ("frozen_list", "depsets cannot contain items of type 'list'"),
        ("builtin", "depset elements must not be mutable values"),
        (
            "mix",
            "cannot add an item of type 'string' to a depset of 'struct'",
        ),
        (
            "mix_transitive",
            "cannot add an item of type 'struct' to a depset of 'string'",
        ),
        (
            "mutable_provider_field",
            "depset elements must not be mutable values",
        ),
        (
            "frozen_list_field",
            "a depset element containing a frozen list (immutable in Bazel; native snapshots would be mutable) is not implemented in the native profile",
        ),
        (
            "lambda_field",
            "a depset element containing a function that is not a global def of a loaded .bzl file is not implemented in the native profile",
        ),
    ] {
        let output = ws.run(&[&format!("//ds:bad_{kind}")]);
        assert_eq!(output.status.code(), Some(1), "{kind}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("error: {message}\n")),
            "{kind}: {stderr}"
        );
    }
}
