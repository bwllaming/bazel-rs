//! Bugs found by the first like-for-like benchmark against genuine Bazel 9.3
//! (docs/BENCHMARKS.md). Every expected text below was captured from the
//! genuine `bazel-9.3.0-linux-x86_64` ELF (sha256 d302d22e…) on the same
//! workspace, with `--ignore_all_rc_files`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("bench-parity-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for (name, text) in files {
            let path = ws.0.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Unparenthesized tuples wherever Bazel's grammar takes `expr`
/// (Parser.java parseExpr): a `for` iterable (bazel-gazelle
/// internal/env.bzl:223), a subscript of any arity, both sides of an
/// assignment and an expression statement.
#[test]
fn unparenthesized_tuples_parse_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"t\")\n"),
        (
            "defs.bzl",
            r#"def check():
    out = []
    for key in "A", "B":
        out.append(key)
    for k, v in ("a", 1), ("b", 2):
        out.append(k + str(v))
    d = {("x", 1): "tx", ("x", 1, 2): "t3"}
    d["y", 2] = "ty"
    out.append(d["x", 1])
    out.append(d["x", 1, 2])
    out.append(d["y", 2])
    t = 1, 2
    out.append(str(t))
    "a", "b"
    return out
"#,
        ),
        (
            "BUILD.bazel",
            "load(\":defs.bzl\", \"check\")\nprint(check())\n",
        ),
    ]);
    let output = ws.run(&["query", "//..."]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    let debug = format!(
        "DEBUG: {}/BUILD.bazel:2:6: [\"A\", \"B\", \"a1\", \"b2\", \"tx\", \"t3\", \"ty\", \"(1, 2)\"]",
        ws.0.path().display()
    );
    assert!(err.lines().any(|line| line == debug), "{err}");
}
