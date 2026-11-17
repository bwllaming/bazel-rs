//! Symbolic macros with user attributes and `inherit_attrs`
//! (StarlarkRuleClassFunctions.macro, packages/MacroClass.java at Bazel
//! 9.3.1). Every case of tests/fixtures/symbolic_macro_attrs/reference_receipt.json
//! — genuine Bazel 9.3.1 (`--batch --ignore_all_rc_files`, a private output
//! user root) on the fixture workspace — runs natively here and is compared
//! with the capture: success, the query output (without its `#` comment
//! lines), the DEBUG lines (the implementation's kwargs) and Bazel's error
//! messages; the `build` case also builds and compares the outputs.
//! `naming` is an analysis failure (a macro-declared target violating the
//! macro naming rules).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/symbolic_macro_attrs/reference_receipt.json"))
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

/// The DEBUG lines without their `DEBUG: <file>:L:C: ` prefix.
fn debug_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("DEBUG: "))
        .map(|line| {
            line.split_once(' ')
                .map_or(line, |(_, rest)| rest)
                .to_owned()
        })
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

fn bazel_rs(ws: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", cache.display()))
        .args(args)
        .current_dir(ws)
        .explicit_modules()
        .output()
        .unwrap()
}

fn check(case: &str) {
    let receipt = receipt();
    let entry = &receipt["cases"][case];
    let expected = &entry["genuine"];
    let temp = tempfile::Builder::new()
        .prefix("symbolic-macro-attrs-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws: PathBuf = temp.path().join("ws");
    let cache = temp.path().join("cache");
    copy(
        &manifest().join("tests/fixtures/symbolic_macro_attrs/workspace"),
        &ws,
    );
    let argv = strings(&entry["argv"]);
    let args = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let output = bazel_rs(&ws, &cache, &args);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).replace(ws.to_str().unwrap(), "<WS>");
    let success = expected["exit"].as_i64() == Some(0);
    assert_eq!(output.status.success(), success, "{case}: {stderr}");
    let debug = debug_lines(&stderr);
    let want = strings(&expected["debug"]);
    if success {
        // DECLARED_DIVERGENCE: the `#` comment lines of --output=build (the
        // rule's call stack shows the macro callables in the native
        // traceback layout) are not compared.
        assert_eq!(
            stdout
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            strings(&expected["stdout"]),
            "{case}"
        );
        assert_eq!(debug, want, "{case}");
    } else {
        // DECLARED_DIVERGENCE: a failing package's DEBUG lines may repeat
        // natively (the failed package is reported twice); the first ones
        // are genuine's.
        assert!(debug.len() >= want.len(), "{case}: {stderr}");
        assert_eq!(debug[..want.len()], want[..], "{case}");
        for error in strings(&expected["errors"]) {
            assert!(
                stderr.contains(&error),
                "{case}: missing {error:?} in {stderr}"
            );
        }
    }
    if let Some(build) = expected.get("build") {
        let argv = strings(&build["argv"]);
        let args = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let output = bazel_rs(&ws, &cache, &args);
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info = bazel_rs(&ws, &cache, &["info", "bazel-bin"]);
        let bin = PathBuf::from(String::from_utf8_lossy(&info.stdout).trim());
        for (name, text) in build["outputs"].as_object().unwrap() {
            assert_eq!(
                fs::read_to_string(bin.join(case).join(name)).unwrap(),
                text.as_str().unwrap(),
                "{case}: {name}"
            );
        }
    }
}

macro_rules! cases {
    ($($case:ident),* $(,)?) => {
        /// The receipt's cases, each run by exactly one test below.
        const CASES: &[&str] = &[$(stringify!($case)),*];
        $(
            #[test]
            fn $case() {
                check(stringify!($case));
            }
        )*
    };
}

cases!(
    e_kwargs,
    e_inh_int,
    e_inh_str,
    e_inh_list,
    e_reserved,
    e_computed,
    e_late,
    e_badname,
    e_attrs_val,
    e_attrs_key,
    e_unexported,
    e_unknown,
    e_mand,
    e_type,
    e_type_label,
    e_type_branch,
    e_noncfg,
    e_implicit,
    e_noname,
    e_tags,
    e_tags_select,
    e_return,
    e_submacro_name,
    e_recursion,
    e_indirect,
    e_dup,
    e_dup_main,
    e_target_vs_macro,
    e_macro_vs_target,
    ok_nested,
    ok_nested_dv,
    inherit,
    select,
    build,
    naming,
);

#[test]
fn receipt_cases_are_covered() {
    let receipt = receipt();
    let mut cases: Vec<&str> = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    cases.sort_unstable();
    let mut covered = CASES.to_vec();
    covered.sort_unstable();
    assert_eq!(cases, covered);
}

#[test]
fn fixture_files_match_the_receipt() {
    let receipt = receipt();
    let root = manifest().join("tests/fixtures/symbolic_macro_attrs/workspace");
    for (path, sha) in receipt["fixture_files_sha256"].as_object().unwrap() {
        let bytes = fs::read(root.join(path)).unwrap();
        let digest = sha256_hex(&bytes);
        assert_eq!(digest, sha.as_str().unwrap(), "{path}");
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}
