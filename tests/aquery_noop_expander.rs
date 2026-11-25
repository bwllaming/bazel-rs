//! aquery of an action whose `add_all(expand_directories = True)` map_each
//! takes a DirectoryExpander (rules_js's `js_image_layer`): without input
//! metadata Bazel maps the tree artifact itself with a NoopExpander
//! (StarlarkCustomCommandLine.java:577-595, 1090-1097). A map_each that
//! fails there (`tree_relative_path` of the directory itself,
//! js_image_layer.bzl:495) does not fail analysis or the build: only an
//! aquery that prints the action fails, with `ERROR: build interrupted` and
//! exit code 8 (the formatter's IOException becomes an interruption,
//! OutputFormatterCallback.processAllTargets); the summary output and
//! queries that do not print it succeed. A map_each that succeeds maps the
//! directory in aquery's command line.
//!
//! Pinned by a genuine capture (tests/fixtures/aquery_noop_expander:
//! reference_receipt.json, genuine 9.3.1).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
include!("support/temp_root.rs");
include!("support/module_archives.rs");
use std::path::Path;
use std::process::{Command, Output};

#[test]
fn a_failing_noop_expander_mapping_interrupts_only_an_aquery_printing_it() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aquery_noop_expander");
    let root = tempfile::Builder::new()
        .prefix("aquery-noop-expander-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let ws = root.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    for name in ["MODULE.bazel", "BUILD.bazel", "defs.bzl"] {
        fs::copy(fixture.join("workspace").join(name), ws.join(name)).unwrap();
    }
    let cache = root.path().join("cache");
    let run = |args: &[&str]| -> Output {
        Command::new(TIMEOUT)
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            // The offline built-in registry and archives (no network).
            .args(with_builtin_registry(args))
            .current_dir(&ws)
            .seeded_archives()
            .output()
            .unwrap()
    };
    let text = |output: &Output| {
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
    };
    let bad = run(&["aquery", "mnemonic(Consume, //:bad)"]);
    assert_eq!(bad.status.code(), Some(8), "{}", text(&bad));
    assert!(bad.stdout.is_empty(), "{}", text(&bad));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("ERROR: build interrupted"),
        "{}",
        text(&bad)
    );
    let commands = run(&["aquery", "--output=commands", "mnemonic(Consume, //:bad)"]);
    assert_eq!(commands.status.code(), Some(8), "{}", text(&commands));
    for args in [
        &["aquery", "mnemonic(Other, //:bad)"][..],
        &["aquery", "--output=summary", "//:bad"][..],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", text(&output));
    }
    let good = run(&["aquery", "mnemonic(Consume, //:good)"]);
    assert_eq!(good.status.code(), Some(0), "{}", text(&good));
    let stdout = String::from_utf8_lossy(&good.stdout);
    assert!(
        stdout.contains("    child:bazel-out/k8-fastbuild/bin/good_tree \\\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "  ActionKey: 321593f2666c70998de5611bcbdeece33a020872f25acbb40be702f2d0c2159d\n"
        ),
        "{stdout}"
    );
    let build = run(&["build", "//:bad", "//:good"]);
    assert_eq!(build.status.code(), Some(0), "{}", text(&build));
    let info = run(&["info", "bazel-bin"]);
    let bin = String::from_utf8(info.stdout).unwrap();
    let bin = Path::new(bin.trim());
    for name in ["bad", "good"] {
        assert_eq!(
            fs::read_to_string(bin.join(format!("{name}.out"))).unwrap(),
            format!(
                "file:bazel-out/k8-fastbuild/bin/{name}_tree/a.txt bazel-out/k8-fastbuild/bin/{name}.out\n"
            )
        );
    }
}
