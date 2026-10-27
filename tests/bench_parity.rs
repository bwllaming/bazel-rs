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

/// `--check_direct_dependencies` (bazel-gazelle's .bazelrc sets `off`) over
/// the mod_command fixture registry: the root requests mc_b@1.0 and
/// mc_leaf@1.0, selection picks 2.0 and 1.1. Genuine 9.3: WARNING events by
/// default, ERROR events and `ERROR: Direct dependency check failed.` (exit
/// 37 for `mod graph`) under `error`, nothing under `off`, and the
/// EnumConverter error for a bad value.
#[test]
fn check_direct_dependencies_modes_match_bazel() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ws = Workspace::new(&[
        (
            "MODULE.bazel",
            "module(name = \"cdd_root\", version = \"0.1\")\n\
             bazel_dep(name = \"mc_a\", version = \"1.0\")\n\
             bazel_dep(name = \"mc_b\", version = \"1.0\", repo_name = \"bee\")\n\
             bazel_dep(name = \"mc_leaf\", version = \"1.0\")\n",
        ),
        ("BUILD.bazel", "filegroup(name = \"x\")\n"),
    ]);
    let registries = [
        format!(
            "--registry=file://{}",
            fixture.join("mod_command/registry").display()
        ),
        format!(
            "--registry=file://{}",
            fixture.join("builtin_modules/registry").display()
        ),
    ];
    let run = |mode: &str| {
        let mut args = vec!["mod", "graph", "--lockfile_mode=off"];
        args.extend(registries.iter().map(String::as_str));
        let flag = format!("--check_direct_dependencies={mode}");
        args.push(&flag);
        let output = ws.run(&args);
        let events: Vec<String> = stderr(&output)
            .lines()
            .filter(|line| line.starts_with("ERROR: ") || line.starts_with("WARNING: "))
            .map(str::to_owned)
            .collect();
        (output.status.code(), events)
    };
    let mismatch = |kind: &str| {
        vec![
            format!(
                "{kind}: For repository 'bee', the root module requires module version mc_b@1.0, but got mc_b@2.0 in the resolved dependency graph. Please update the version in your MODULE.bazel or set --check_direct_dependencies=off"
            ),
            format!(
                "{kind}: For repository 'mc_leaf', the root module requires module version mc_leaf@1.0, but got mc_leaf@1.1 in the resolved dependency graph. Please update the version in your MODULE.bazel or set --check_direct_dependencies=off"
            ),
        ]
    };
    assert_eq!(run("warning"), (Some(0), mismatch("WARNING")));
    let mut errors = mismatch("ERROR");
    errors.push("ERROR: Direct dependency check failed.".to_owned());
    assert_eq!(run("error"), (Some(37), errors));
    assert_eq!(run("off"), (Some(0), vec![]));
    let (code, _) = run("bogus");
    assert_ne!(code, Some(0));
    let mut args = vec!["build", "--check_direct_dependencies=bogus", "//:x"];
    args.extend(registries.iter().map(String::as_str));
    let output = ws.run(&args);
    assert!(
        stderr(&output).contains("While parsing option --check_direct_dependencies=bogus: Not a valid direct deps check mode: 'bogus' (should be off, warning or error)"),
        "{}",
        stderr(&output)
    );
}
