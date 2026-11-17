//! A test's runfiles are symlinks, as in Bazel (B15, docs/RUNFILES_TREES.md
//! "Test runfiles"): a published output's runfile links to the artifact at
//! its exec path in the test's execution root (`.../bazel-out/<cfg>/bin/...`,
//! where it is staged like any input), a source's to the live workspace
//! file; `bazel run` uses the output tree's runfiles tree, whose links name
//! the published outputs and the workspace sources.
//!
//! Pinned by a genuine capture (tests/fixtures/runfiles_symlinks:
//! capture.py, reference_receipt.json): for the probe's executable and each
//! runfile, whether it is a symlink, its `readlink` and its `realpath`
//! (execution roots and the workspace normalized), in a sandboxed test, a
//! `no-sandbox` test and `bazel run`. Declared differences (asserted below):
//! a source's runfile in a sandboxed test links straight to the workspace
//! file (genuine: to `<sandbox execroot>/_main/<path>`, itself a link to
//! the workspace file — same `realpath`), and the `no-sandbox` test's tree
//! artifact is staged file by file (genuine runs it in the output tree,
//! where the tree's runfile is one directory link).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
include!("support/temp_root.rs");
use std::path::Path;
use std::process::{Command, Output};

include!("support/module_archives.rs");

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    fn drop(&mut self) {
        // Published outputs may be read-only.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.0.path())
            .status();
    }
}

impl Workspace {
    fn run(&self, cache: &Path, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(args)
            .arg("--repository_disable_download")
            .current_dir(self.0.path().join("ws"))
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

/// capture.py's `normalize` for native paths.
fn normalize(text: &str, workspace: &str, cache: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    // `<cache>/tests/work/<key>-<pid>/execroot` and
    // `<cache>/actions/<key>/outputs`.
    while let Some(at) = rest.find(cache) {
        out.push_str(&rest[..at]);
        let after = &rest[at + cache.len()..];
        let skip = |after: &str, prefix: &str, suffix: &str| -> Option<usize> {
            let tail = after.strip_prefix(prefix)?;
            let end = tail.find('/')?;
            tail[end..]
                .starts_with(suffix)
                .then_some(prefix.len() + end + suffix.len())
        };
        match skip(after, "/tests/work/", "/execroot")
            .or_else(|| skip(after, "/actions/", "/outputs"))
        {
            Some(length) => {
                out.push_str("EXECROOT");
                rest = &after[length..];
            }
            None => {
                out.push_str(cache);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace(workspace, "WORKSPACE")
}

fn probe_lines(text: &str, prefix: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with(prefix))
        .map(str::to_owned)
        .collect()
}

#[test]
fn test_and_run_runfiles_are_symlinks_like_bazel() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runfiles_symlinks");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("reference_receipt.json")).unwrap())
            .unwrap();
    let root = Workspace(
        tempfile::Builder::new()
            .prefix("runfiles-symlinks-cli-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
    );
    let ws = root.0.path().join("ws");
    for name in [
        "MODULE.bazel",
        "BUILD.bazel",
        "defs.bzl",
        "src.txt",
        "sub/nested.txt",
    ] {
        let target = ws.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(fixture.join("workspace").join(name), target).unwrap();
    }
    let cache = root.0.path().join("cache");
    seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
    let registry = builtin_registry_flag();
    let test = root.run(&cache, &["test", &registry, "--test_output=all", "//:all"]);
    let test_text = format!(
        "{}{}",
        String::from_utf8_lossy(&test.stdout),
        String::from_utf8_lossy(&test.stderr)
    );
    assert!(test.status.success(), "{test_text}");
    let run = root.run(&cache, &["run", &registry, "//:probe"]);
    assert!(run.status.success(), "{run:?}");
    let workspace = fs::canonicalize(&ws).unwrap();
    let cache = fs::canonicalize(&cache).unwrap();
    let normalized =
        |text: &str| normalize(text, workspace.to_str().unwrap(), cache.to_str().unwrap());
    let expected = |row: &str| -> Vec<String> {
        receipt["rows"][row]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect()
    };
    // `bazel run`: the output tree's runfiles tree, exactly genuine's.
    assert_eq!(
        probe_lines(&normalized(&String::from_utf8_lossy(&run.stdout)), "run_"),
        expected("run")
    );
    // The tests, with the declared differences.
    let mut genuine = expected("test");
    let declared = [
        (
            "test_dir/f.txt kind=file readlink= realpath=EXECROOT/bazel-out/k8-fastbuild/bin/dir/f.txt",
            "test_dir/f.txt kind=symlink readlink=EXECROOT/bazel-out/k8-fastbuild/bin/dir/f.txt realpath=EXECROOT/bazel-out/k8-fastbuild/bin/dir/f.txt",
        ),
        (
            "test_src.txt kind=symlink readlink=EXECROOT/src.txt realpath=WORKSPACE/src.txt",
            "test_src.txt kind=symlink readlink=WORKSPACE/src.txt realpath=WORKSPACE/src.txt",
        ),
        (
            "test_sub/nested.txt kind=symlink readlink=EXECROOT/sub/nested.txt realpath=WORKSPACE/sub/nested.txt",
            "test_sub/nested.txt kind=symlink readlink=WORKSPACE/sub/nested.txt realpath=WORKSPACE/sub/nested.txt",
        ),
    ];
    for (reference, native) in declared {
        let at = genuine
            .iter()
            .position(|line| line == reference)
            .unwrap_or_else(|| panic!("receipt lacks {reference}"));
        genuine[at] = native.to_owned();
    }
    // One block per test (from its `test_self` line), in either order.
    let blocks = |lines: Vec<String>| -> Vec<Vec<String>> {
        let mut blocks: Vec<Vec<String>> = Vec::new();
        for line in lines {
            if line.starts_with("test_self ") || blocks.is_empty() {
                blocks.push(Vec::new());
            }
            blocks.last_mut().unwrap().push(line);
        }
        blocks.sort();
        blocks
    };
    assert_eq!(
        blocks(probe_lines(&normalized(&test_text), "test_")),
        blocks(genuine)
    );
}

#[test]
fn a_source_changed_while_its_test_ran_is_not_cached() {
    let root = Workspace(
        tempfile::Builder::new()
            .prefix("runfiles-symlinks-live-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
    );
    let ws = root.0.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("MODULE.bazel"), "module(name = \"live\")\n").unwrap();
    fs::write(ws.join("data.txt"), "before\n").unwrap();
    fs::write(
        ws.join("defs.bzl"),
        r##"def _impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, """#!/bin/bash
cat "$TEST_SRCDIR/_main/data.txt"
echo during > "$(realpath "$TEST_SRCDIR/_main/data.txt")"
""", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

edit_test = rule(implementation = _impl, test = True, attrs = {"data": attr.label_list(allow_files = True)})
"##,
    )
    .unwrap();
    // `no-sandbox`: the test may write the workspace file (genuine's local
    // runs may too); the change must not leave a cached result behind.
    fs::write(
        ws.join("BUILD.bazel"),
        "load(\":defs.bzl\", \"edit_test\")\nedit_test(name = \"edit_test\", data = [\"data.txt\"], tags = [\"no-sandbox\"])\n",
    )
    .unwrap();
    let cache = root.0.path().join("cache");
    seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
    let registry = builtin_registry_flag();
    let first = root.run(
        &cache,
        &["test", &registry, "--test_output=all", "//:edit_test"],
    );
    let text = String::from_utf8_lossy(&first.stdout).into_owned()
        + &String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "{text}");
    assert!(text.contains("before"), "{text}");
    assert_eq!(fs::read_to_string(ws.join("data.txt")).unwrap(), "during\n");
    // Restore the keyed content: the first result was not stored, so the
    // test runs again (a stored result would be reported `(cached)`).
    fs::write(ws.join("data.txt"), "before\n").unwrap();
    let second = root.run(&cache, &["test", &registry, "//:edit_test"]);
    let text = String::from_utf8_lossy(&second.stdout).into_owned()
        + &String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "{text}");
    assert!(!text.contains("(cached)"), "{text}");
}
