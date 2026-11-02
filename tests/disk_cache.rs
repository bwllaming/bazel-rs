//! `--disk_cache` shared with genuine Bazel 9.3.1 (docs/REPOSITORY_CACHE.md):
//! genuine's `ac/` and `cas/` layout, hits on the entries genuine wrote
//! (tests/fixtures/disk_cache/genuine, captured by capture.py), and genuine's
//! read and write policies.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/disk_cache");
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn receipt() -> Value {
    serde_json::from_str(&fs::read_to_string(format!("{FIXTURE}/reference_receipt.json")).unwrap())
        .unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::create_dir_all(to).unwrap();
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), out);
            } else {
                out.push(
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = crate::test_temp::root().join(format!(
            "native-disk-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        copy_tree(&Path::new(FIXTURE).join("workspace"), &root.join("ws"));
        fs::create_dir_all(root.join("home")).unwrap();
        Self(root)
    }

    fn build(&self, cache: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.0.join(cache).display()))
            .arg("build")
            .arg("//pkg:all")
            .args(args)
            .current_dir(self.0.join("ws"))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"))
            .explicit_modules()
            .output()
            .unwrap()
    }

    fn disk(&self, name: &str) -> (PathBuf, String) {
        let path = self.0.join(name);
        let flag = format!("--disk_cache={}", path.display());
        (path, flag)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The `INFO: N processes: …` line and the outputs (`LABEL<TAB>PATH`).
fn result(output: &Output) -> (Vec<String>, Value) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let processes = stderr
        .lines()
        .filter(|line| line.contains(" processes: "))
        .map(str::to_owned)
        .collect();
    let mut outputs = serde_json::Map::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (_, path) = line.split_once('\t').unwrap();
        outputs.insert(
            Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            Value::String(fs::read_to_string(path).unwrap()),
        );
    }
    (processes, Value::Object(outputs))
}

fn expected(step: &str) -> (Vec<String>, Value) {
    let step = &receipt()["steps"][step];
    (
        step["processes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect(),
        step["outputs"].clone(),
    )
}

#[test]
fn entries_genuine_wrote_are_hits() {
    let scratch = Scratch::new();
    let (disk, flag) = scratch.disk("genuine");
    copy_tree(&Path::new(FIXTURE).join("genuine"), &disk);
    assert_eq!(
        result(&scratch.build("c1", &[&flag])),
        expected("2_native_fresh_cache_on_genuine_written")
    );
}

#[test]
fn entries_have_genuines_layout_and_a_fresh_cache_hits_them() {
    let scratch = Scratch::new();
    let (disk, flag) = scratch.disk("native");
    let (processes, outputs) = result(&scratch.build("c1", &[&flag]));
    assert_eq!(outputs, expected("3_native_cold").1);
    assert_eq!(processes.len(), 1);
    let receipt = receipt();
    let mut written = files(&disk);
    // DiskCacheClient's temporary directory stays (empty).
    assert!(disk.join("tmp").is_dir());
    written.retain(|name| !name.starts_with("tmp/"));
    assert_eq!(
        serde_json::to_value(&written).unwrap(),
        receipt["genuine_files"]
    );
    assert_eq!(
        result(&scratch.build("c2", &[&flag])),
        expected("2_native_fresh_cache_on_genuine_written")
    );
}

#[test]
fn the_disk_cache_ignores_the_remote_cache_policies_like_genuine() {
    let scratch = Scratch::new();
    let (disk, flag) = scratch.disk("disk");
    // getWriteCachePolicy: --noremote_upload_local_results is the remote
    // cache's; the disk cache is written.
    result(&scratch.build("c1", &[&flag, "--noremote_upload_local_results"]));
    assert_eq!(
        files(&disk).iter().filter(|f| f.starts_with("ac/")).count(),
        2
    );
    // getReadCachePolicy: --noremote_accept_cached is the remote cache's.
    assert_eq!(
        result(&scratch.build("c2", &[&flag, "--noremote_accept_cached"])),
        expected("2_native_fresh_cache_on_genuine_written")
    );
    // Without the option nothing is read from it.
    let (processes, _) = result(&scratch.build("c3", &[]));
    assert!(!processes[0].contains("disk cache hit"), "{processes:?}");
}

#[test]
fn an_output_base_inside_the_disk_cache_is_an_error() {
    let scratch = Scratch::new();
    let flag = format!("--disk_cache={}", scratch.0.display());
    let output = scratch.build("c1", &[&flag]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be a subdirectory of the --disk_cache directory"),
        "{stderr}"
    );
}
