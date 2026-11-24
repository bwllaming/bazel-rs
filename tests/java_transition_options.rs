//! JavaOptions as configuration options (B67 of docs/OSS_CORPUS.md;
//! genuine captures in tests/fixtures/java_transition_options): Starlark
//! transitions and `analysis_test_transition` write them, config_setting
//! and the ST- hash see them, `ctx.fragments.java` and
//! `configuration_field("java", "launcher")` read them, and the exec
//! transition propagates them.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

include!("support/module_archives.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/java_transition_options"
);

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

fn run(ws: &Path, cache: &Path, command: &str, target: &str) -> Output {
    let args = [
        command,
        "--platforms=//:p",
        "--host_platform=//:p",
        "--host_java_launcher=//:host_launcher",
        target,
    ];
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", cache.display()))
        .args(with_builtin_registry(&args))
        .current_dir(ws)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .stdin(Stdio::null())
        .seeded_archives()
        .output()
        .unwrap()
}

/// The one file named `name` below `root` (following symlinks).
fn find(root: &Path, name: &str) -> PathBuf {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                found.push(path);
            }
        }
    }
    assert_eq!(found.len(), 1, "{name}: {found:?}");
    found.remove(0)
}

#[test]
fn java_options_are_configuration_options() {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("java-transition-options-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURE).join("workspace"), &ws);
    let cache = temp.path().join("cache");
    let expected = |name: &str| fs::read_to_string(Path::new(FIXTURE).join(name)).unwrap();

    let output = run(&ws, &cache, "build", "//:plain");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(ws.join("bazel-bin/plain.txt")).unwrap(),
        expected("plain.txt")
    );

    let output = run(&ws, &cache, "build", "//:java");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(find(&ws.join("bazel-out"), "transitioned.txt")).unwrap(),
        expected("transitioned.txt")
    );

    // The analysis test fails with the probe's lines as its message.
    let output = run(&ws, &cache, "test", "//:shown");
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(ws.join("bazel-testlogs/shown/test.log")).unwrap();
    let (_, message) = log
        .split_once(
            "-----------------------------------------------------------------------------\n",
        )
        .unwrap();
    assert_eq!(message, expected("shown.txt"));
}
