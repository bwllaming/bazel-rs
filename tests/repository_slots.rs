//! Repository rules run in their slot `<cache>/repositories/external/<canonical
//! name>`, genuine Bazel 9.3's `<output_base>/external/<name>`
//! (RepositoryFetchFunction.java:158-160; crate::repository_slots): the
//! directory a rule sees and every path a tool derives from it are genuine's
//! (captures: tests/fixtures/repository_slots/genuine_capture.json, made by
//! capture.py from tests/fixtures/repository_slots/workspace).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/explicit_modules.rs");
include!("support/snapshot_off.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// A copy of the captured workspace.
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("repository-slots-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let source = manifest().join("tests/fixtures/repository_slots/workspace");
        for entry in fs::read_dir(&source).unwrap() {
            let entry = entry.unwrap();
            fs::create_dir_all(root.path().join("ws")).unwrap();
            fs::copy(entry.path(), root.path().join("ws").join(entry.file_name())).unwrap();
        }
        Self { root }
    }

    fn ws(&self) -> PathBuf {
        self.root.path().join("ws")
    }

    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// The repository store below the cache directory.
    fn store(&self) -> PathBuf {
        self.cache().join("repositories")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
            ])
            .arg(format!("--cache-dir={}", self.cache().display()))
            .args(args)
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path().join("home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("BAZEL_RS_TIMING", "1")
            .explicit_modules();
        command
    }

    /// Build `target`: its output's bytes, and stderr.
    fn build(&self, target: &str) -> (String, String) {
        self.build_with(target, &[])
    }

    /// [`Self::build`] with extra environment variables.
    fn build_with(&self, target: &str, environment: &[(&str, &str)]) -> (String, String) {
        fs::create_dir_all(self.root.path().join("home")).unwrap();
        fs::create_dir_all(self.root.path().join("tmp")).unwrap();
        let output = self
            .command(&["build", target])
            .envs(environment.iter().copied())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let path = outputs(&output)[0].1.clone();
        (fs::read_to_string(path).unwrap(), stderr)
    }
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').expect("native build output path");
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

/// The genuine capture's output of `case`.
fn genuine(case: &str) -> String {
    let capture: serde_json::Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/repository_slots/genuine_capture.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(capture["cases"][case]["exit"], 0, "{case}");
    capture["cases"][case]["output"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The publication a slot links (`../direct/trees/<identity>`).
fn linked_tree(store: &Path, canonical: &str) -> PathBuf {
    let slot = store.join("external").join(canonical);
    let target = fs::read_link(&slot)
        .unwrap_or_else(|error| panic!("{} is no link: {error}", slot.display()));
    let identity = target.strip_prefix("../direct/trees").unwrap();
    assert_eq!(identity.as_os_str().len(), 64, "{}", target.display());
    store.join("direct/trees").join(identity)
}

/// The PERF-REPO line's counts (BAZEL_RS_TIMING): (reused, executed).
fn repository_counts(stderr: &str) -> (usize, usize) {
    let line = stderr
        .lines()
        .find(|line| line.starts_with("PERF-REPO reused="))
        .unwrap_or_else(|| panic!("no PERF-REPO line: {stderr}"));
    let count = |key: &str| -> usize {
        line.split(' ')
            .find_map(|part| part.strip_prefix(key))
            .unwrap()
            .parse()
            .unwrap()
    };
    (count("reused="), count("executed="))
}

/// A rule's `repository_ctx.path(".")` is `external/<canonical name>` below
/// the output base, as genuine's — natively the output base of
/// repositories is `<cache>/repositories`. This repository writes that path
/// into its BUILD file, so it is self-referencing: its slot stays a real
/// directory (with its marker) and the published tree is a copy; a warm run
/// reuses it without executing.
#[test]
fn a_rule_runs_in_external_canonical_name_like_bazel() {
    let f = Fixture::new();
    let (show, stderr) = f.build("@probed//:show");
    assert_eq!(show, genuine("repopath"));
    assert_eq!(repository_counts(&stderr).1, 1, "{stderr}");
    let (full, _) = f.build("@probed//:full");
    assert_eq!(
        full.replace(&f.store().display().to_string(), "<OUTPUT_BASE>"),
        genuine("repopath_full")
    );
    let slot = f.store().join("external/+probe+probed");
    assert!(fs::symlink_metadata(&slot).unwrap().is_dir());
    let markers: Vec<_> = fs::read_dir(f.store().join("direct/slots"))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(markers.len(), 1);
    let identity = markers[0].lines().nth(1).unwrap().to_owned();
    assert_eq!(markers[0].lines().nth(2), Some("+probe+probed"));
    let tree = f.store().join("direct/trees").join(&identity);
    assert_eq!(
        fs::read(tree.join("BUILD.bazel")).unwrap(),
        fs::read(slot.join("BUILD.bazel")).unwrap()
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(f.store().join(format!("direct/manifests/{identity}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["self_ref"], true);
    // Warm: an analysis snapshot hit (nothing is looked at); with the
    // snapshot off (the subject is the warm probe), reused without
    // executing. Either way the slot stays.
    let (again, stderr) = f.build("@probed//:show");
    assert_eq!(again, show);
    assert_eq!(repository_counts(&stderr).1, 0, "{stderr}");
    let (again, stderr) = f.build_with("@probed//:show", &[SNAPSHOT_OFF]);
    assert_eq!(again, show);
    assert_eq!(repository_counts(&stderr), (1, 0), "{stderr}");
    assert!(fs::symlink_metadata(&slot).unwrap().is_dir());
}

/// gazelle's go_repository mode spells `${SRCDIR}` from the repository
/// root's parent directory (language/go/generate.go:130-144): genuine's
/// `external/<name>` is valid from the execution root, so the duckdb Go
/// bindings' `-L${SRCDIR}/` finds its archives. In the former private stage
/// it was `../<32-hex stage>` and the link failed (TEST-SURVEY L2). The
/// repository is ordinary: its slot links the published tree after the
/// build, and a warm run binds a slot another worktree pointed elsewhere.
#[test]
fn gazelle_srcdir_resolves_from_the_execution_root_like_bazel() {
    let f = Fixture::new();
    let (flags, stderr) = f.build("@cgo//:link_flags");
    assert_eq!(flags, genuine("cgo_srcdir"));
    assert_eq!(repository_counts(&stderr).1, 1, "{stderr}");
    let tree = linked_tree(&f.store(), "+cgo_like+cgo");
    let build = fs::read_to_string(tree.join("BUILD.bazel")).unwrap();
    assert!(
        build.contains("test -f external/+cgo_like+cgo/libfoo.a"),
        "{build}"
    );
    assert!(!build.contains("../"), "{build}");
    // Warm: an analysis snapshot hit, which names trees only: a slot that
    // another worktree pointed elsewhere does not disturb it.
    let slot = f.store().join("external/+cgo_like+cgo");
    fs::remove_file(&slot).unwrap();
    std::os::unix::fs::symlink("../direct/trees/elsewhere", &slot).unwrap();
    let (again, stderr) = f.build("@cgo//:link_flags");
    assert_eq!(again, flags);
    assert!(stderr.contains("snapshot.hit"), "{stderr}");
    // The warm probe (snapshot off: its subject) reuses the publication and
    // binds the slot again, also when it is gone.
    for gone in [false, true] {
        if gone {
            fs::remove_file(&slot).unwrap();
        }
        let (again, stderr) = f.build_with("@cgo//:link_flags", &[SNAPSHOT_OFF]);
        assert_eq!(again, flags);
        assert_eq!(repository_counts(&stderr), (1, 0), "{stderr}");
        assert_eq!(linked_tree(&f.store(), "+cgo_like+cgo"), tree);
    }
}
