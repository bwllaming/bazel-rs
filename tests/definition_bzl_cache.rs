//! W6: definition loads (module extension and repository rule definitions
//! and their `.bzl` graphs) share one evaluation per module and
//! invocation (crate::loading::DefinitionBzlCache). A reused module
//! replays exactly what a fresh definition loader records and prints, so
//! the output, the lockfile and the repository identities are those of
//! the uncached loader (`BAZEL_RS_DEBUG_NO_DEFINITION_CACHE`): two
//! extensions of one `.bzl`, a repository rule shared by both and by a
//! `use_repo_rule`, top-level prints in every module, cold (extensions
//! evaluated) and warm (lockfile entries reused).

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-definition-bzl-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file(
            "MODULE.bazel",
            r#"module(name = "main")
ext = use_extension("//:ext.bzl", "ext")
use_repo(ext, "r1")
ext2 = use_extension("//:ext.bzl", "ext2")
use_repo(ext2, "r2")
make = use_repo_rule("//:lib.bzl", "make")
make(name = "r3")
"#,
        );
        ws.file(
            "BUILD.bazel",
            "filegroup(name = 'all', srcs = ['@r1//:f', '@r2//:f', '@r3//:f'])\n",
        );
        ws.file(
            "ext.bzl",
            r#"load(":lib.bzl", "make")
load(":util.bzl", "name_of")
print("ext.bzl top-level")

def _ext(mctx):
    make(name = name_of("r1"))

ext = module_extension(_ext)

def _ext2(mctx):
    make(name = name_of("r2"))

ext2 = module_extension(_ext2)
"#,
        );
        ws.file(
            "lib.bzl",
            r#"load(":util.bzl", "name_of")
print("lib.bzl top-level", Label("//:x"))

def _impl(rctx):
    rctx.file("BUILD.bazel", "filegroup(name = 'f', srcs = [], visibility = ['//visibility:public'])\n")

make = repository_rule(_impl)
"#,
        );
        ws.file(
            "util.bzl",
            "print('util.bzl top-level')\ndef name_of(name):\n    return name\n",
        );
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, cache: &str, uncached: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={cache}"))
            .args(["build", "--lockfile_mode=update", "//:all"])
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_DEBUG_NO_DEFINITION_CACHE")
            .current_dir(&self.0);
        if uncached {
            command.env("BAZEL_RS_DEBUG_NO_DEFINITION_CACHE", "1");
        }
        command.output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn shared_definition_modules_replay_the_uncached_output() {
    // Two workspaces, one per loader, so each run starts from the same
    // state (no lockfile, empty cache).
    let cached = Workspace::new();
    let uncached = Workspace::new();
    for round in ["cold", "warm"] {
        let a = cached.run("cache", false);
        let b = uncached.run("cache", true);
        assert!(a.status.success(), "{round} cached: {}", stderr(&a));
        assert!(b.status.success(), "{round} uncached: {}", stderr(&b));
        let normalize = |output: &Output, ws: &Workspace| {
            String::from_utf8_lossy(&output.stdout).replace(&ws.0.display().to_string(), "WS")
                + &stderr(output).replace(&ws.0.display().to_string(), "WS")
        };
        assert_eq!(
            normalize(&a, &cached),
            normalize(&b, &uncached),
            "{round}: output"
        );
        assert_eq!(
            fs::read(cached.0.join("MODULE.bazel.lock")).unwrap(),
            fs::read(uncached.0.join("MODULE.bazel.lock")).unwrap(),
            "{round}: lockfile"
        );
        // Every definition load printed its graph's lines, as before.
        let text = stderr(&a);
        assert!(
            text.matches("lib.bzl top-level").count() >= 2,
            "{round}: {text}"
        );
        assert!(
            text.matches("util.bzl top-level").count() >= 2,
            "{round}: {text}"
        );
    }
}
