//! rule(output_to_genfiles=True) (OSS corpus B20: capnproto's capnp_gen):
//! with --incompatible_merge_genfiles_directory (default true, the only
//! supported value) the genfiles root is the bin root
//! (BuildConfigurationValue.java:430-434), so the rule analyzes and its
//! outputs and `ctx.genfiles_dir` are in `bazel-out/<cfg>/bin`. The case of
//! tests/fixtures/output_to_genfiles/reference_receipt.json — genuine Bazel
//! 9.3.1 (`--batch --ignore_all_rc_files`, a private output user root) — runs
//! natively and is compared: exit code, `cquery --output=files` and the
//! analysis DEBUG line.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/output_to_genfiles/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn check(case: &str) {
    let receipt = receipt();
    let entry = &receipt["cases"][case];
    let expected = &entry["genuine"];
    let argv: Vec<&str> = entry["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    let temp = tempfile::Builder::new()
        .prefix("output-to-genfiles-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws = temp.path().join("ws");
    copy(
        &manifest().join("tests/fixtures/output_to_genfiles/workspace"),
        &ws,
    );
    let mut args = vec![argv[0], EXPLICIT_MODULES];
    args.extend(&argv[1..]);
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(&args)
        .current_dir(&ws)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).replace(ws.to_str().unwrap(), "<WS>");
    assert_eq!(
        output.status.code(),
        expected["exit"].as_i64().map(|code| code as i32),
        "{case}: {stderr}"
    );
    let mut debug: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("DEBUG: "))
        .map(|line| {
            line.split_once(' ')
                .map_or(line, |(_, rest)| rest)
                .to_owned()
        })
        .collect();
    debug.sort();
    let want: Vec<String> = expected["debug_sorted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(debug, want, "{case}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want: Vec<String> = expected["stdout"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        stdout.lines().map(str::to_owned).collect::<Vec<_>>(),
        want,
        "{case}"
    );
}

#[test]
fn output_to_genfiles_outputs_are_in_bin() {
    let receipt = receipt();
    assert_eq!(receipt["cases"].as_object().unwrap().len(), 1);
    check("files");
}
