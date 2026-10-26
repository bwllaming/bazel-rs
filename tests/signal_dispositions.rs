//! The signals a started process begins with ignored when bazel-rs itself
//! was started with SIGHUP, SIGINT, SIGQUIT, SIGPIPE and SIGTERM ignored (a
//! background job of a non-interactive shell, nohup), pinned by a genuine
//! capture (tests/fixtures/signal_dispositions: capture.py,
//! reference_receipt.json; src/signals.rs). Repository `execute`, actions
//! and tests start with every signal at its default action (the
//! process-wrapper's ClearSignalMask); the --workspace_status_command keeps
//! SIGHUP, SIGINT and SIGTERM ignored like a direct start of genuine's JVM
//! (its --batch rows: bazel-rs is not daemonized, genuine's server always
//! ignores SIGHUP); a `run` target keeps only SIGHUP ignored like the
//! client's exec (genuine's server-mode rows; --batch runs the target from
//! the JVM instead).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::Command;
include!("support/explicit_modules.rs");

/// The capture's signals, by number.
const SIGNALS: [(&str, u32); 5] = [
    ("SIGHUP", 1),
    ("SIGINT", 2),
    ("SIGQUIT", 3),
    ("SIGPIPE", 13),
    ("SIGTERM", 15),
];

/// The names of [`SIGNALS`] set in a `SigIgn:\t<hex>` line.
fn ignored(line: &str) -> Vec<&'static str> {
    let mask = u64::from_str_radix(line.split_whitespace().last().unwrap(), 16).unwrap();
    SIGNALS
        .iter()
        .filter(|(_, number)| mask >> (number - 1) & 1 == 1)
        .map(|(name, _)| *name)
        .collect()
}

fn probes(text: &str, prefix: &str) -> serde_json::Value {
    let prefix = format!("{prefix} SigIgn:");
    text.lines()
        .filter(|line| line.starts_with(&prefix))
        .map(ignored)
        .collect::<Vec<_>>()
        .into()
}

fn find_output(cache: &Path, name: &str) -> String {
    fn visit(dir: &Path, name: &str) -> Option<String> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = visit(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|file| file == name) {
                return fs::read_to_string(path).ok();
            }
        }
        None
    }
    visit(&cache.join("actions"), name).unwrap_or_else(|| panic!("no output {name}"))
}

/// The genuine row of (`mode`, `client`, `strategy`).
fn genuine<'a>(
    rows: &'a [serde_json::Value],
    mode: &str,
    client: &str,
    strategy: &str,
) -> &'a serde_json::Value {
    rows.iter()
        .find(|row| row["mode"] == mode && row["client"] == client && row["strategy"] == strategy)
        .unwrap()
}

#[test]
fn spawns_start_with_the_signal_dispositions_genuine_gives_them() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signal_dispositions");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("reference_receipt.json")).unwrap())
            .unwrap();
    let rows = receipt["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    let scratch = tempfile::Builder::new()
        .prefix("signal-dispositions-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let ws = scratch.path().join("ws");
    for name in [
        "MODULE.bazel",
        "pkg/BUILD.bazel",
        "pkg/defs.bzl",
        "pkg/probe.sh",
    ] {
        let target = ws.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(fixture.join("workspace").join(name), &target).unwrap();
    }
    let home = scratch.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let status_out = scratch.path().join("status.txt");
    let status_script = scratch.path().join("status.sh");
    fs::write(
        &status_script,
        format!(
            "#!/bin/sh\ngrep SigIgn /proc/self/status > {}\n",
            status_out.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&status_script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The default-dispositions cases need a harness that ignores none of the
    // signals itself (SIGPIPE aside: std resets it): a shell cannot un-ignore
    // what it inherited ignored.
    let harness = fs::read_to_string("/proc/self/status").unwrap();
    let harness = harness
        .lines()
        .find(|line| line.starts_with("SigIgn:"))
        .unwrap();
    let clients: &[&str] = if ignored(harness).iter().all(|name| *name == "SIGPIPE") {
        &["default", "ignored"]
    } else {
        eprintln!("this harness ignores {harness}: only the ignored-client cases run");
        &["ignored"]
    };
    for &client in clients {
        for strategy in ["standalone", "sandboxed"] {
            let cache = scratch.path().join(format!("cache-{client}-{strategy}"));
            let bazel = |args: &[&str]| {
                let mut command = Command::new("/bin/sh");
                let trap = if client == "ignored" {
                    "trap '' HUP INT QUIT PIPE TERM; "
                } else {
                    ""
                };
                command
                    .arg("-c")
                    .arg(format!("{trap}exec \"$0\" \"$@\""))
                    .arg(env!("CARGO_BIN_EXE_bazel-rs"))
                    .args([
                        "--ignore_all_rc_files",
                        &format!("--cache-dir={}", cache.display()),
                    ])
                    .args(with_explicit_modules(args))
                    .arg(format!("--spawn_strategy={strategy}"))
                    .arg(format!(
                        "--workspace_status_command={}",
                        status_script.display()
                    ))
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", &home)
                    .current_dir(&ws);
                let output = command.output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                assert!(
                    output.status.success(),
                    "{client}/{strategy} {args:?}: {stdout}{stderr}"
                );
                (stdout, stderr)
            };
            let _ = fs::remove_file(&status_out);
            let (stdout, stderr) = bazel(&[
                "test",
                "--test_output=all",
                "//pkg:probe",
                "//pkg:probe_test",
            ]);
            let status = fs::read_to_string(&status_out).unwrap();
            let (run, _) = bazel(&["run", "//pkg:probe_bin"]);
            let genrule = find_output(&cache, "probe.txt");
            let batch = genuine(rows, "batch", client, strategy);
            let server = genuine(rows, "server", client, strategy);
            let case = format!("{client}/{strategy}");
            for (kind, observed) in [
                ("repository_execute", probes(&genrule, "repository")),
                ("action", probes(&genrule, "action")),
                ("test", probes(&format!("{stdout}{stderr}"), "probe")),
            ] {
                assert_eq!(batch[kind], server[kind], "{case} {kind}");
                assert_eq!(observed, batch[kind], "{case} {kind}");
            }
            assert_eq!(
                serde_json::json!([ignored(&status)]),
                batch["workspace_status"],
                "{case} workspace_status"
            );
            assert_eq!(probes(&run, "probe"), server["run"], "{case} run");
        }
    }
}
