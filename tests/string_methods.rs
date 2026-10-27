//! Bazel 9.3 UTF-8 byte-string semantics of string methods and `repr`,
//! exercised through the real CLI in both BUILD and rule-implementation code.
//! `fixtures/string_methods/expected.txt` is derived from a byte-level model of
//! the pinned `StringModule.java`/`Printer.java` (Bazel 9.3 defaults to UTF-8
//! byte strings), not from bazel-rs output, and was confirmed by a genuine
//! Bazel 9.3 capture (`fixtures/string_methods/reference_receipt.json`). Rows tagged `divergent` are inputs for
//! which Bazel's byte-wise result is invalid UTF-8; they pin the documented
//! bazel-rs behaviour instead (see docs/STRING_METHODS.md).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const DEFS: &str = include_str!("fixtures/string_methods/defs.bzl");
const EXPECTED: &str = include_str!("fixtures/string_methods/expected.txt");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("string-methods-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='string_methods_fixture')\n");
        ws.file("defs.bzl", DEFS);
        ws.file("BUILD.bazel", build);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        fs::write(self.path().join(name), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn outputs(&self, args: &[&str]) -> BTreeMap<String, Vec<u8>> {
        String::from_utf8_lossy(&self.run(args).stdout)
            .lines()
            .map(|line| PathBuf::from(line.split_once('\t').expect("output path").1))
            .map(|p| {
                (
                    p.file_name().unwrap().to_str().unwrap().to_owned(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect()
    }
}

fn assert_rows(context: &str, actual: &[u8]) {
    let actual = std::str::from_utf8(actual).expect("valid UTF-8 report");
    let expected: Vec<&str> = EXPECTED.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    for (e, a) in expected.iter().zip(&actual) {
        assert_eq!(e, a, "{context} row mismatch");
    }
    assert_eq!(expected.len(), actual.len(), "{context} row count");
}

#[test]
fn string_methods_match_bazel_byte_strings_in_build_and_rule_code() {
    let ws = Workspace::new(
        "load(':defs.bzl', 'check', 'report')\ncheck(name = 'check', build_report = report())\n",
    );
    let outputs = ws.outputs(&["build", ":check"]);
    assert_eq!(
        outputs.keys().map(String::as_str).collect::<Vec<_>>(),
        ["check.build.txt", "check.impl.txt"]
    );
    assert_rows("BUILD", &outputs["check.build.txt"]);
    assert_rows("rule implementation", &outputs["check.impl.txt"]);
    assert_eq!(outputs["check.build.txt"], EXPECTED.as_bytes());
    assert_eq!(outputs["check.impl.txt"], EXPECTED.as_bytes());
}

#[test]
fn expectations_cover_every_method_sample_and_declared_divergence() {
    let rows: Vec<&str> = EXPECTED.lines().collect();
    // 15 methods x 41 samples, plus 3 strips x 5 explicit `chars` (incl. None).
    assert_eq!(rows.len(), 15 * 41 + 3 * 5);
    let divergent: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|r| r.starts_with("divergent "))
        .collect();
    assert_eq!(
        divergent,
        [
            "divergent capitalize 30 \"中x\"",
            "divergent capitalize 34 \"\u{3000}\"",
            "divergent capitalize 35 \"\u{3000}a\"",
            "divergent title 23 \"AÉ\"",
            "divergent title 29 \"StraßE\"",
            "divergent title 30 \"中X\"",
            "divergent title 33 \"\u{a0}A\u{a0}\"",
            "divergent title 34 \"\u{3000}\"",
            "divergent title 35 \"\u{3000}A\"",
            "divergent strip_chars 3 \"ê\"",
            "divergent lstrip_chars 3 \"ê\"",
        ]
    );
    // Spot checks of the byte-string model, independent of the generated file.
    for row in [
        "bazel lower 23 \"aÉ\"",
        "bazel upper 26 \"éLAN\"",
        "bazel upper 18 \"ß\"",
        "bazel title 17 \"õa\"",
        "bazel title 20 \"µa\"",
        "bazel title 24 \"ÉA\"",
        "bazel title 26 \"éLan\"",
        "bazel isdigit 38 False",
        "bazel isdigit 39 False",
        "bazel isspace 9 True",
        "bazel isspace 10 False",
        "bazel isspace 32 False",
        "bazel isspace 34 False",
        "bazel isspace 37 False",
        "bazel islower 15 False",
        "bazel islower 23 True",
        "bazel isupper 25 True",
        "bazel istitle 25 True",
        "bazel istitle 27 False",
        "bazel strip 13 \"a\"",
        "bazel strip 14 \"a\u{7f}\"",
        "bazel strip 33 \"\u{a0}a\u{a0}\"",
        "bazel strip 35 \"\u{3000}a\"",
        "bazel lstrip 13 \"a\\x1f\"",
        "bazel rstrip 13 \"\\x1ca\"",
        "bazel repr 12 \"\\\"\u{7f}\\\"\"",
        "bazel repr 13 \"\\\"\\\\x1ca\\\\x1f\\\"\"",
        "bazel repr 32 \"\\\"\u{a0}\\\"\"",
        "bazel strip_chars 4 \"\u{a0}a\"",
        "bazel lstrip_chars 4 \"\u{a0}a\\x1f \"",
    ] {
        assert!(rows.contains(&row), "missing {row:?}");
    }
}
