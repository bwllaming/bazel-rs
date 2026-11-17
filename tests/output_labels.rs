//! Output attributes are labels (BuildType.OutputType.convert,
//! packages/BuildType.java:561-569): `attr.output`, `attr.output_list` and
//! genrule `outs` accept `name`, `:name`, `//pkg:name` and Label values in
//! the current package, and reject other packages with `label 'X' is not in
//! the current package`. Every case of
//! tests/fixtures/output_labels/reference_receipt.json — genuine Bazel 9.3.1
//! (`--batch --ignore_all_rc_files`, a private output user root) on the
//! fixture workspace — runs natively and is compared with the capture: the
//! exit code, the query output and the first error's message.
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
        &fs::read(manifest().join("tests/fixtures/output_labels/reference_receipt.json")).unwrap(),
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
        .prefix("output-labels-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws = temp.path().join("ws");
    copy(
        &manifest().join("tests/fixtures/output_labels/workspace"),
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
    // DECLARED_DIVERGENCE: a failed query exits 7 in genuine Bazel; the
    // native exit code of a loading error differs, so only success is compared.
    assert_eq!(
        output.status.success(),
        expected["exit"].as_i64() == Some(0),
        "{case}: {stderr}"
    );
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
    // The first error's message after its `<location>: <target>: ` prefix
    // (bazel-rs reports the conversion error of the call).
    if let Some(error) = expected["errors"].as_array().unwrap().first() {
        let message = error.as_str().unwrap().rsplit_once(": ").unwrap().1;
        assert!(
            stderr.contains(message),
            "{case}: missing {message:?} in {stderr}"
        );
    }
}

#[test]
fn receipt_cases_are_covered() {
    let receipt = receipt();
    let mut cases: Vec<&String> = receipt["cases"].as_object().unwrap().keys().collect();
    cases.sort();
    assert_eq!(
        cases,
        [
            "genrule_other_package",
            "invalid_label",
            "other_package",
            "outputs"
        ]
    );
}

#[test]
fn output_labels_name_the_package_outputs() {
    check("outputs");
}

#[test]
fn outputs_outside_the_package_or_invalid_fail() {
    for case in ["other_package", "invalid_label", "genrule_other_package"] {
        check(case);
    }
}
