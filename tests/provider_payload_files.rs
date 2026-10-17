//! Files reachable through configured-target values and through struct /
//! provider elements (nested depsets, Target tuples) of depsets in a
//! dependency's provider are known action inputs of its dependents
//! (tests/fixtures/provider_payload_files/reference_receipt.json).
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> tempfile::TempDir {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ws = tempfile::Builder::new()
        .prefix("n54s-")
        .tempdir_in(base)
        .unwrap();
    let source = manifest().join("tests/fixtures/provider_payload_files/workspace/pp");
    fs::create_dir_all(ws.path().join("pp")).unwrap();
    for entry in fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, ws.path().join("pp").join(path.file_name().unwrap())).unwrap();
    }
    fs::write(
        ws.path().join("MODULE.bazel"),
        "module(name = \"provider_payload_files\")\n",
    )
    .unwrap();
    ws
}

fn build(ws: &Path, target: &str) -> Output {
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
        .arg(target)
        .current_dir(ws)
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(124), "CLI timed out");
    output
}

fn outputs(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| fs::read_to_string(line.split_once('\t').unwrap().1).unwrap())
        .collect()
}

#[test]
fn files_of_targets_and_depset_structs_in_providers_are_inputs() {
    let ws = fixture();
    assert_eq!(
        outputs(&build(ws.path(), "//pp:c")),
        "data of g\nitem of p\nitem of p\nnested of p\ndata of g\n"
    );
}
