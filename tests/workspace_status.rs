//! ctx.info_file / ctx.version_file (StarlarkRuleContext.java:994-1004):
//! the owner-less `bazel-out/{stable,volatile}-status.txt` Files with
//! Bazel's attributes (tests/fixtures/workspace_status/reference_receipt.json),
//! and N63: the workspace status action producing them — the files an
//! action sees (Bazel's built-in keys, the format and the trailing space of
//! an empty BUILD_EMBED_LABEL), a --workspace_status_command script's keys
//! (stable/volatile split, built-in overrides), script failures
//! (infrastructure failure with Bazel's text, exit statuses included), the
//! ./x PathFragment normalization (127), and the constant-metadata rebuild
//! semantics (a volatile change never rebuilds a consumer; a stable change
//! does) — docs/WORKSPACE_STATUS.md.
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
        .prefix("n63s-")
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

fn build(ws: &Path, target: &str, extra: &[&str], cache: &str, epoch: Option<&str>) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
        .args([
            "--ignore_all_rc_files",
            &format!("--cache-dir={cache}"),
            "build",
            "--experimental-builtin-modules",
        ])
        .arg(format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        ))
        .args(extra)
        .arg(target)
        .current_dir(ws)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws)
        .env("LC_ALL", "C");
    if let Some(epoch) = epoch {
        command.env("SOURCE_DATE_EPOCH", epoch);
    }
    command.output().unwrap()
}

fn outputs(output: &Output) -> Vec<(String, String)> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let path = line.split_once('\t').unwrap().1;
            (
                path.rsplit('/').next().unwrap().to_owned(),
                fs::read_to_string(path).unwrap(),
            )
        })
        .collect()
}

fn output_of(output: &Output, name: &str) -> String {
    outputs(output)
        .into_iter()
        .find(|(seen, _)| seen == name)
        .unwrap_or_else(|| panic!("no output {name}"))
        .1
}

fn script(ws: &Path, body: &str) -> String {
    let path = ws.join("status.sh");
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path.to_string_lossy().into_owned()
}

fn summary(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| line.contains("actions executed"))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn workspace_status_files_follow_bazel() {
    let ws = fixture();
    let built = build(ws.path(), "//ws:ws", &[], "cache", None);
    assert_eq!(
        output_of(&built, "ws.args"),
        "bazel-out/stable-status.txt\nbazel-out/volatile-status.txt\n"
    );
    assert_eq!(
        output_of(&built, "ws.txt"),
        "<generated file stable-status.txt> File path=bazel-out/stable-status.txt short_path=stable-status.txt basename=stable-status.txt dirname=bazel-out extension=txt is_source=False is_directory=False owner=None root=bazel-out\n<generated file volatile-status.txt> File path=bazel-out/volatile-status.txt short_path=volatile-status.txt basename=volatile-status.txt dirname=bazel-out extension=txt is_source=False is_directory=False owner=None root=bazel-out\neq=True False\n"
    );
    // N63: the status files exist as real generated artifacts; the grep
    // counts BUILD_TIMESTAMP, always a built-in volatile key.
    assert_eq!(
        output_of(
            &build(ws.path(), "//ws:ws_input", &[], "cache", None),
            "ws_input.txt"
        ),
        "1\n"
    );
}

#[test]
fn status_files_an_action_sees() {
    let ws = fixture();
    let flag = format!(
        "--workspace_status_command={}",
        script(
            ws.path(),
            "echo 'STABLE_COMMIT deadbeef'\necho 'BUILD_USER stampuser'\necho 'VOLATILE_ID r7'\n"
        )
    );
    let text = output_of(
        &build(
            ws.path(),
            "//ws:ws_cat",
            &[&flag, "--embed_label=n63"],
            "cache",
            Some("1792511811"),
        ),
        "ws_cat.txt",
    );
    let lines: Vec<&str> = text.lines().collect();
    // The stable file: built-in keys with the script's overrides, sorted;
    // an empty BUILD_EMBED_LABEL would keep its trailing space (the built-in
    // here is overridden by --embed_label).
    assert_eq!(lines[0], "BUILD_EMBED_LABEL n63");
    assert!(lines[1].starts_with("BUILD_HOST "));
    assert_eq!(lines[2], "BUILD_USER stampuser");
    assert_eq!(lines[3], "STABLE_COMMIT deadbeef");
    // The volatile file: the built-in timestamp pair (SOURCE_DATE_EPOCH
    // pinned) plus the script's volatile key, sorted.
    assert_eq!(lines[4], "BUILD_TIMESTAMP 1792511811");
    assert_eq!(lines[5], "FORMATTED_DATE 2026 Oct 20 15 56 51 Tue");
    assert_eq!(lines[6], "VOLATILE_ID r7");
    assert_eq!(lines.len(), 7);
}

#[test]
fn a_failing_script_is_an_infrastructure_error_with_bazel_text() {
    let ws = fixture();
    let flag = format!(
        "--workspace_status_command={}",
        script(ws.path(), "echo STABLE_X 1\nexit 7\n")
    );
    let output = build(ws.path(), "//ws:ws_input", &[&flag], "cache", None);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        ["stable-status.txt", "volatile-status.txt"].iter().any(|file| {
            stderr.contains(&format!(
                "BazelWorkspaceStatusAction {file} failed: Failed to determine workspace status: Process exited with status 7"
            ))
        }),
        "{stderr}"
    );
    // The ./x normalization of the PathFragment converter: `./status.sh`
    // becomes `status.sh`, the shell cannot find it and exits 127 — the
    // same exit status Bazel 9.3 reports for a relative script.
    let flag = "--workspace_status_command=./status.sh";
    let output = build(ws.path(), "//ws:ws_input", &[flag], "cache2", None);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to determine workspace status: Process exited with status 127"),
        "{stderr}"
    );
}

#[test]
fn volatile_changes_never_rebuild_a_consumer_stable_changes_do() {
    let ws = fixture();
    let script = script(
        ws.path(),
        "echo 'STABLE_COMMIT deadbeef'\necho 'BUILD_USER stampuser'\n",
    );
    let flag = format!("--workspace_status_command={script}");
    let first = build(
        ws.path(),
        "//ws:ws_cat",
        &[&flag],
        "cache",
        Some("1792511811"),
    );
    assert!(first.status.success());
    assert!(
        summary(&first).starts_with("bazel-rs: 3 actions executed"),
        "{}",
        summary(&first)
    );
    // The same status: everything is a cache hit.
    let warm = build(
        ws.path(),
        "//ws:ws_cat",
        &[&flag],
        "cache",
        Some("1792511811"),
    );
    assert!(
        summary(&warm).starts_with("bazel-rs: 0 actions executed, 3 cache hits"),
        "{}",
        summary(&warm)
    );
    // A new timestamp only rewrites the volatile file
    // (executeUnconditionally); the consumer stays cached (constant
    // metadata).
    let volatile = build(
        ws.path(),
        "//ws:ws_cat",
        &[&flag],
        "cache",
        Some("1792519999"),
    );
    assert!(
        summary(&volatile).starts_with("bazel-rs: 1 actions executed, 2 cache hits"),
        "{}",
        summary(&volatile)
    );
    // A changed stable key rebuilds the consumer.
    fs::write(
        ws.path().join("status.sh"),
        "#!/bin/sh\necho 'STABLE_COMMIT feedface'\necho 'BUILD_USER stampuser'\n",
    )
    .unwrap();
    let stable = build(
        ws.path(),
        "//ws:ws_cat",
        &[&flag],
        "cache",
        Some("1792511811"),
    );
    // The stable write and the consumer re-execute; the volatile write is
    // still cached at this timestamp.
    assert!(
        summary(&stable).starts_with("bazel-rs: 2 actions executed, 1 cache hits"),
        "{}",
        summary(&stable)
    );
}
