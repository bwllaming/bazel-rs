//! Build without the Bytes (`--remote_download_outputs`, docs/REMOTE_CACHE.md
//! "Build without the Bytes", crate::remote_outputs) against the in-process
//! REAPI server of tests/support (cache and executor on loopback).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/grpc_cache_server.rs");

use grpc_cache_server::{Config, Server, fs_canonical_tmp};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const BUILD: &str = r#"genrule(name = "a", outs = ["a.txt"], cmd = "echo alpha > $@")
genrule(name = "b", srcs = [":a"], outs = ["b.txt"], cmd = "cat $< > $@; echo beta >> $@")
genrule(name = "c", srcs = [":a"], outs = ["c.txt"], cmd = "cat $< > $@; echo gamma >> $@", tags = ["no-remote"])
genrule(name = "d", outs = ["d.txt"], cmd = "echo alpha > $@")
load(":defs.bzl", "tool", "t_test")
tool(name = "tool", data = [":a"])
t_test(name = "t", data = [":a"])
"#;

const DEFS: &str = r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.run_shell(outputs = [out], command = "printf '#!/bin/bash\\ncat pkg/a.txt\\n' > %s && chmod +x %s" % (out.path, out.path))
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]
tool = rule(_impl, executable = True, attrs = {"data": attr.label_list(allow_files = True)})
t_test = rule(_impl, test = True, attrs = {"data": attr.label_list(allow_files = True)})
"#;

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = fs_canonical_tmp().join(format!(
            "native-remote-download-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ws/pkg")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(root.join("ws/MODULE.bazel"), "module(name = \"bwob\")\n").unwrap();
        fs::write(root.join("ws/pkg/BUILD.bazel"), BUILD).unwrap();
        fs::write(root.join("ws/pkg/defs.bzl"), DEFS).unwrap();
        Self(root)
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.0.join(cache).display()))
            .args(args)
            .current_dir(self.0.join("ws"))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"));
        command.explicit_modules().output().unwrap()
    }

    /// `bazel-bin/pkg/NAME` of the output tree of `cache`, if it exists.
    fn bin(&self, cache: &str, name: &str) -> Option<String> {
        let info = self.run(cache, &["info", "bazel-bin"]);
        let bin = String::from_utf8_lossy(&info.stdout).trim().to_owned();
        fs::read_to_string(PathBuf::from(bin).join("pkg").join(name)).ok()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn counts(server: &Server) -> BTreeMap<String, usize> {
    server.method_counts(&server.take_log())
}

fn reads(server: &Server) -> usize {
    counts(server).get("ByteStream/Read").copied().unwrap_or(0)
}

#[test]
fn outputs_stay_remote_until_needed() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        execution: true,
        ..Config::default()
    });
    let executor = format!("--remote_executor={}", server.url());
    // minimal: both spawns execute remotely, nothing is downloaded and
    // nothing appears in the output tree.
    let built = ws.run(
        "c1",
        &["build", &executor, "--remote_download_minimal", "//pkg:b"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("2 executed remotely"),
        "{}",
        stderr(&built)
    );
    assert_eq!(reads(&server), 0);
    assert_eq!(ws.bin("c1", "b.txt"), None);
    assert_eq!(ws.bin("c1", "a.txt"), None);
    // The native action cache knows the outputs are remote: a hit, no call.
    let again = ws.run(
        "c1",
        &["build", &executor, "--remote_download_minimal", "//pkg:b"],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stderr(&again).contains("0 actions executed, 2 cache hits"),
        "{}",
        stderr(&again)
    );
    let calls = counts(&server);
    assert!(
        calls
            .keys()
            .all(|method| method.ends_with("/GetCapabilities")),
        "{calls:?}"
    );
    // toplevel (the default): the requested output must be local, so its
    // action reruns (a remote cache hit) and downloads it; its input stays
    // remote.
    let toplevel = ws.run("c1", &["build", &executor, "//pkg:b"]);
    assert!(toplevel.status.success(), "{}", stderr(&toplevel));
    assert!(
        stderr(&toplevel).contains("1 cache hits"),
        "{}",
        stderr(&toplevel)
    );
    assert_eq!(reads(&server), 1, "{}", stderr(&toplevel));
    assert_eq!(ws.bin("c1", "b.txt").as_deref(), Some("alpha\nbeta\n"));
    assert_eq!(ws.bin("c1", "a.txt"), None);
    // A local consumer (no-remote) fetches its remote-only input first.
    let local = ws.run("c1", &["build", &executor, "//pkg:c"]);
    assert!(local.status.success(), "{}", stderr(&local));
    assert_eq!(reads(&server), 1);
    assert_eq!(ws.bin("c1", "c.txt").as_deref(), Some("alpha\ngamma\n"));
    // --remote_download_regex forces a download; all downloads everything.
    let regex = ws.run(
        "c2",
        &[
            "build",
            &executor,
            "--remote_download_minimal",
            "--remote_download_regex=.*/a\\.txt",
            "//pkg:b",
        ],
    );
    assert!(regex.status.success(), "{}", stderr(&regex));
    assert_eq!(ws.bin("c2", "a.txt").as_deref(), Some("alpha\n"));
    assert_eq!(ws.bin("c2", "b.txt"), None);
    let all = ws.run(
        "c3",
        &["build", &executor, "--remote_download_all", "//pkg:b"],
    );
    assert!(all.status.success(), "{}", stderr(&all));
    assert_eq!(ws.bin("c3", "a.txt").as_deref(), Some("alpha\n"));
    assert_eq!(ws.bin("c3", "b.txt").as_deref(), Some("alpha\nbeta\n"));
    // An expired lease (--experimental_remote_cache_ttl): the action
    // reruns (a remote cache hit) instead of trusting the metadata.
    let built = ws.run(
        "c4",
        &[
            "build",
            &executor,
            "--remote_download_minimal",
            "--experimental_remote_cache_ttl=0",
            "//pkg:a",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let expired = ws.run(
        "c4",
        &["build", &executor, "--remote_download_minimal", "//pkg:a"],
    );
    assert!(
        stderr(&expired).contains("0 cache hits"),
        "{}",
        stderr(&expired)
    );
}

#[test]
fn a_lost_remote_input_reruns_its_producer() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        execution: true,
        ..Config::default()
    });
    let executor = format!("--remote_executor={}", server.url());
    let built = ws.run(
        "c1",
        &["build", &executor, "--remote_download_minimal", "//pkg:a"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    // The CAS evicts a.txt; the local consumer cannot fetch it: the build
    // is retried with the producer invalidated, which executes again
    // (its cached result lacks the blob), and the consumer then fetches it.
    let digest = bazel_rs::reapi::Digest::of(b"alpha\n");
    assert!(server.evict("", &digest.hash));
    let local = ws.run(
        "c1",
        &["build", &executor, "--remote_download_minimal", "//pkg:c"],
    );
    assert!(local.status.success(), "{}", stderr(&local));
    let text = stderr(&local);
    assert!(
        text.contains("Lost inputs no longer available remotely: bazel-out/"),
        "{text}"
    );
    assert!(
        text.contains("Found transient remote cache error, retrying the build..."),
        "{text}"
    );
    assert_eq!(ws.bin("c1", "c.txt").as_deref(), Some("alpha\ngamma\n"));
    // Without retries the command fails with exit code 34.
    let ws = Workspace::new();
    let built = ws.run(
        "c1",
        &["build", &executor, "--remote_download_minimal", "//pkg:a"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(server.evict("", &digest.hash));
    let failed = ws.run(
        "c1",
        &[
            "build",
            &executor,
            "--remote_download_minimal",
            "--experimental_remote_cache_eviction_retries=0",
            "//pkg:c",
        ],
    );
    assert_eq!(failed.status.code(), Some(34), "{}", stderr(&failed));
}

#[test]
fn run_and_local_tests_fetch_what_they_need() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        execution: true,
        ..Config::default()
    });
    let executor = format!("--remote_executor={}", server.url());
    // `run` downloads its target and runfiles even with minimal.
    let ran = ws.run(
        "c1",
        &["run", &executor, "--remote_download_minimal", "//pkg:tool"],
    );
    assert!(ran.status.success(), "{}", stderr(&ran));
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "alpha\n");
    // A local test fetches its remote-only binary and runfiles.
    let tested = ws.run(
        "c2",
        &[
            "test",
            &executor,
            "--remote_download_minimal",
            "--strategy=TestRunner=local",
            "//pkg:t",
        ],
    );
    assert!(tested.status.success(), "{}", stderr(&tested));
    // A remote test needs nothing locally.
    let remote = ws.run(
        "c3",
        &["test", &executor, "--remote_download_minimal", "//pkg:t"],
    );
    assert!(remote.status.success(), "{}", stderr(&remote));
}

#[test]
fn one_blob_is_read_once_per_invocation() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        execution: true,
        ..Config::default()
    });
    let executor = format!("--remote_executor={}", server.url());
    // a.txt and d.txt have one digest: downloaded once, stored, and the
    // second output is served from the native store.
    let built = ws.run("c1", &["build", &executor, "//pkg:a", "//pkg:d"]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(reads(&server), 1, "{}", stderr(&built));
    assert_eq!(ws.bin("c1", "a.txt").as_deref(), Some("alpha\n"));
    assert_eq!(ws.bin("c1", "d.txt").as_deref(), Some("alpha\n"));
}
