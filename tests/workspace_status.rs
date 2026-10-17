//! ctx.info_file / ctx.version_file (StarlarkRuleContext.java:994-1004):
//! owner-less `bazel-out/{stable,volatile}-status.txt` Files with Bazel's
//! attributes (tests/fixtures/workspace_status/reference_receipt.json); as
//! action inputs they fail explicitly (the workspace status action is not
//! executed natively).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
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
    let source = manifest().join("tests/fixtures/workspace_status/workspace/ws");
    fs::create_dir_all(ws.path().join("ws")).unwrap();
    for entry in fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, ws.path().join("ws").join(path.file_name().unwrap())).unwrap();
    }
    fs::write(
        ws.path().join("MODULE.bazel"),
        "module(name = \"workspace_status\")\n",
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
fn workspace_status_files_follow_bazel() {
    let ws = fixture();
    assert_eq!(
        outputs(&build(ws.path(), "//ws:ws")),
        "<generated file stable-status.txt> File path=bazel-out/stable-status.txt short_path=stable-status.txt basename=stable-status.txt dirname=bazel-out extension=txt is_source=False is_directory=False owner=None root=bazel-out\n<generated file volatile-status.txt> File path=bazel-out/volatile-status.txt short_path=volatile-status.txt basename=volatile-status.txt dirname=bazel-out extension=txt is_source=False is_directory=False owner=None root=bazel-out\neq=True False\nbazel-out/stable-status.txt\nbazel-out/volatile-status.txt\n"
    );
    let output = build(ws.path(), "//ws:ws_input");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("action input bazel-out/volatile-status.txt: workspace status artifacts (ctx.info_file/ctx.version_file) are not produced by the native profile"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
