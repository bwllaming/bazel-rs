//! Execution roots left by a killed command (docs/HARDLINK_STAGING.md,
//! "Execution-root reuse"): the next command on the cache removes the work
//! directories of processes that are gone, never those of a live command
//! sharing the cache (its owner file stays locked while it runs).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
include!("support/explicit_modules.rs");

const BUILD: &str = r#"
genrule(name = "slow", outs = ["slow.txt"], cmd = "sleep 4; echo slow > $@")
genrule(name = "quick", outs = ["quick.txt"], cmd = "echo quick > $@")
"#;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("work-sweep-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        fs::write(
            ws.path().join("MODULE.bazel"),
            "module(name = 'work_sweep')\n",
        )
        .unwrap();
        fs::create_dir_all(ws.path().join("pkg")).unwrap();
        fs::write(ws.path().join("pkg/BUILD.bazel"), BUILD).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn command(&self, cache: &Path, target: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .args(["--ignore_all_rc_files"])
            .arg(format!("--cache-dir={}", cache.display()))
            .args(["build", target])
            .current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command.explicit_modules();
        command
    }

    fn build(&self, cache: &Path, target: &str) -> Output {
        self.command(cache, target).output().unwrap()
    }

    fn start(&self, cache: &Path, target: &str) -> Child {
        self.command(cache, target).spawn().unwrap()
    }
}

/// The work directories of process `pid` in `cache` (`{key}-{pid}-{n}`).
fn work_of(cache: &Path, pid: u32) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(cache.join("work")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let parts: Vec<&str> = name.split('-').collect();
            parts.len() == 3 && parts[0].len() == 64 && parts[1] == pid.to_string()
        })
        .map(|entry| entry.path())
        .collect()
}

/// Wait until `child` has a work directory in `cache`.
fn wait_for_work(cache: &Path, child: &Child) -> Vec<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let work = work_of(cache, child.id());
        if !work.is_empty() {
            return work;
        }
        assert!(Instant::now() < deadline, "no work directory appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_killed_commands_work_directories_are_swept_by_the_next() {
    let ws = Workspace::new();
    let cache = ws.path().join("cache");
    let mut killed = ws.start(&cache, "//pkg:slow");
    let left = wait_for_work(&cache, &killed);
    killed.kill().unwrap();
    killed.wait().unwrap();
    // The orphaned genrule finishes (its root is gone or not: either way).
    std::thread::sleep(Duration::from_secs(5));
    assert!(left.iter().all(|path| path.exists()), "{left:?}");
    let next = ws.build(&cache, "//pkg:quick");
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert!(left.iter().all(|path| !path.exists()), "{left:?}");
    assert!(!cache.join(format!("work/.owner-{}", killed.id())).exists());
}

#[test]
fn a_live_commands_work_directories_survive_a_concurrent_command() {
    let (running, other) = (Workspace::new(), Workspace::new());
    let cache = running.path().join("shared-cache");
    let live = running.start(&cache, "//pkg:slow");
    let work = wait_for_work(&cache, &live);
    let next = other.build(&cache, "//pkg:quick");
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert!(work.iter().all(|path| path.exists()), "{work:?}");
    let done = live.wait_with_output().unwrap();
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
}
