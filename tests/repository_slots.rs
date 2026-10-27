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
            .tempdir_in(crate::test_temp::root())
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
            // The native result lines the harness reads (crate::build_results).
            .env("BAZEL_RS_RESULT_LINES", "1")
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

/// gazelle's `go_repository_cache` in miniature: `go.env` names the
/// repository's own directory as GOPATH (self-referencing: its slot stays a
/// real, mutable directory). `tools` (go_repository_tools) reads `go.env`
/// and runs a tool in GOPATH; each `consumer` (go_repository) reads `go.env`,
/// demands `tools`, and writes a module into GOPATH, waits a second and
/// checks it is still there — what `go mod download` and `fetch_repo`'s
/// checksum need. Every subprocess logs its interval.
const GO_BZL: &str = r#"
def _log(rctx, what):
    return "echo " + what + " $(date +%s.%N) >> " + rctx.attr.log + "/" + rctx.name

def _gopath(rctx):
    for line in rctx.read(Label("@gocache//:go.env")).splitlines():
        if line.startswith("GOPATH="):
            return line[len("GOPATH="):]
    fail("no GOPATH")

def _cache(rctx):
    rctx.file("go.env", "GOPATH=%s\nVERSION=%s\n" % (rctx.path("."), rctx.attr.version))
    rctx.file("BUILD.bazel", "exports_files(['go.env'])\n")
    result = rctx.execute(["sh", "-c", "%s; sleep %s; %s" % (
        _log(rctx, "cache-start-" + rctx.attr.version),
        rctx.attr.delay,
        _log(rctx, "cache-end-" + rctx.attr.version),
    )])
    if result.return_code:
        fail(result.stderr)

gocache = repository_rule(_cache, attrs = {
    "version": attr.string(), "delay": attr.string(default = "0"), "log": attr.string(),
})

def _tools(rctx):
    gopath = _gopath(rctx)
    result = rctx.execute(["sh", "-c", "%s; mkdir -p \"$GOPATH/bin\" && echo tool > \"$GOPATH/bin/tool\"; %s" % (
        _log(rctx, "start"), _log(rctx, "end"),
    )], environment = {"GOPATH": gopath})
    if result.return_code:
        fail(result.stderr)
    rctx.file("BUILD.bazel", "exports_files(['tool'])\n")
    rctx.file("tool", "tool\n")

tools = repository_rule(_tools, attrs = {"log": attr.string()})

def _consumer(rctx):
    gopath = _gopath(rctx)
    rctx.path(Label("@tools//:tool"))
    result = rctx.execute(["sh", "-c", """
set -e
%s
d="$GOPATH/pkg/mod/$NAME-$ROUND"
mkdir -p "$d"
echo "$NAME $ROUND" > "$d/file"
sleep 1
test "$(cat "$d/file")" = "$NAME $ROUND"
%s
""" % (_log(rctx, "start"), _log(rctx, "end"))], environment = {
        "GOPATH": gopath, "NAME": rctx.name, "ROUND": rctx.attr.round,
    })
    if result.return_code:
        fail("the module in GOPATH changed: " + result.stderr)
    rctx.file("BUILD.bazel", "exports_files(['ok'])\n")
    rctx.file("ok", rctx.attr.round + "\n")

consumer = repository_rule(_consumer, attrs = {"log": attr.string(), "round": attr.string()})
"#;

/// One workspace of the GOPATH scenario, sharing `cache` with the other.
struct GoWorkspace {
    root: PathBuf,
    cache: PathBuf,
    log: PathBuf,
}

impl GoWorkspace {
    fn new(base: &Path, name: &str, cache: &Path) -> Self {
        let root = base.join(name);
        let log = base.join(format!("{name}-log"));
        for directory in [&root, &log, &base.join("home"), &base.join("tmp")] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(root.join("go.bzl"), GO_BZL).unwrap();
        fs::write(
            root.join("BUILD.bazel"),
            "filegroup(name = 'all', srcs = ['@c1//:ok', '@c2//:ok', '@c3//:ok', '@c4//:ok'])\n",
        )
        .unwrap();
        Self {
            root,
            cache: cache.to_path_buf(),
            log,
        }
    }

    /// Write MODULE.bazel for gocache `version` (sleeping `delay` seconds
    /// in its execution) and consumers of `round`.
    fn module(&self, version: &str, delay: &str, round: &str) {
        let log = self.log.display();
        let mut module = format!(
            "module(name = 'main')\n\
             gocache = use_repo_rule('//:go.bzl', 'gocache')\n\
             gocache(name = 'gocache', version = '{version}', delay = '{delay}', log = '{log}')\n\
             tools = use_repo_rule('//:go.bzl', 'tools')\n\
             tools(name = 'tools', log = '{log}')\n\
             consumer = use_repo_rule('//:go.bzl', 'consumer')\n"
        );
        for name in ["c1", "c2", "c3", "c4"] {
            module.push_str(&format!(
                "consumer(name = '{name}', log = '{log}', round = '{round}')\n"
            ));
        }
        fs::write(self.root.join("MODULE.bazel"), module).unwrap();
    }

    fn spawn(&self) -> std::process::Child {
        let base = self.root.parent().unwrap();
        Command::new("timeout")
            .args([
                "300s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
            ])
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(["build", "//:all"])
            .current_dir(&self.root)
            .env_clear()
            // The native result lines the harness reads (crate::build_results).
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", base.join("home"))
            .env("TMPDIR", base.join("tmp"))
            .explicit_modules()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Wait until the log of `repository` has a line starting with `what`.
    fn wait_for(&self, repository: &str, what: &str) {
        let started = std::time::Instant::now();
        while !fs::read_to_string(self.log.join(repository))
            .is_ok_and(|log| log.lines().any(|line| line.starts_with(what)))
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "{repository} never logged {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Every logged interval: (repository, label, start, end). Lines are
    /// `start T`/`end T` or `cache-start-V T`/`cache-end-V T`.
    fn intervals(&self) -> Vec<(String, String, f64, f64)> {
        let mut intervals = Vec::new();
        for entry in fs::read_dir(&self.log).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let mut open: Option<(String, f64)> = None;
            for line in fs::read_to_string(entry.path()).unwrap().lines() {
                let (what, time) = line.split_once(' ').unwrap();
                let time: f64 = time.parse().unwrap();
                if what.contains("start") {
                    assert!(open.is_none(), "{name}: {what} inside another interval");
                    open = Some((what.replace("start", ""), time));
                } else {
                    let (label, start) = open.take().expect("an end after its start");
                    assert_eq!(label, what.replace("end", ""), "{name}");
                    intervals.push((name.clone(), label, start, time));
                }
            }
        }
        intervals
    }
}

fn finished(child: std::process::Child) -> Output {
    let output = child.wait_with_output().unwrap();
    assert_ne!(
        output.status.code(),
        Some(124),
        "a bazel-rs process timed out"
    );
    output
}

/// No gocache execution overlaps a subprocess that may write into its
/// directory: the logged intervals of both workspaces never interleave.
fn assert_exclusive(workspaces: &[&GoWorkspace]) {
    let intervals: Vec<_> = workspaces.iter().flat_map(|w| w.intervals()).collect();
    let caches: Vec<_> = intervals
        .iter()
        .filter(|i| i.0 == "+gocache+gocache")
        .collect();
    let writers: Vec<_> = intervals
        .iter()
        .filter(|i| i.0 != "+gocache+gocache")
        .collect();
    assert!(!caches.is_empty() && !writers.is_empty(), "{intervals:?}");
    for cache in &caches {
        for writer in &writers {
            assert!(
                cache.3 <= writer.2 || writer.3 <= cache.2,
                "{cache:?} overlaps {writer:?}"
            );
        }
    }
}

/// What every published tree of the store says: no module cache (`pkg/`)
/// was ever published, and each tree verifies against its manifest.
fn assert_trees_clean(store: &Path) {
    for entry in fs::read_dir(store.join("direct/trees")).unwrap() {
        let path = entry.unwrap().path();
        assert!(!path.join("pkg").exists(), "{} holds pkg/", path.display());
        let tree = bazel_rs::repository_store::published_tree(store, &path)
            .unwrap_or_else(|| panic!("{} is no publication", path.display()));
        bazel_rs::repository_store::verify_published_tree(&tree).unwrap();
    }
}

/// Two processes on one cache, the GOPATH hazard of go_repository_cache in
/// both directions: while one workspace's consumers write into GOPATH, the
/// other re-executes the cache (another version); then the re-execution runs
/// first (slowly) and the consumers wait. The re-execution never starts
/// while a consumer's subprocess runs and no consumer starts during it, so
/// every consumer's module survives (no checksum mismatch); the re-executed
/// tree holds only what the rule wrote; the chain consumer -> cache,
/// consumer -> tools -> cache completes in both processes.
#[test]
fn a_re_execution_of_a_self_referencing_repository_waits_for_its_writers() {
    let base = tempfile::Builder::new()
        .prefix("repository-slots-gopath-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let cache = base.path().join("cache");
    let a = GoWorkspace::new(base.path(), "a", &cache);
    let b = GoWorkspace::new(base.path(), "b", &cache);
    // A's consumers are writing into GOPATH when B re-executes the cache.
    a.module("1", "0", "a1");
    b.module("2", "0", "b1");
    let first = a.spawn();
    a.wait_for("+consumer+c1", "start");
    let second = b.spawn();
    for output in [finished(first), finished(second)] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_exclusive(&[&a, &b]);
    // The re-executed cache published only what its rule wrote.
    let store = cache.join("repositories");
    let gopath = store.join("external/+gocache+gocache");
    assert!(fs::symlink_metadata(&gopath).unwrap().is_dir());
    assert_trees_clean(&store);
    for version in ["1", "2"] {
        let trees: Vec<_> = fs::read_dir(store.join("direct/trees"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|tree| {
                fs::read_to_string(tree.join("go.env"))
                    .is_ok_and(|env| env.contains(&format!("VERSION={version}")))
            })
            .collect();
        assert_eq!(trees.len(), 1, "version {version}");
        let mut names: Vec<_> = fs::read_dir(&trees[0])
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["BUILD.bazel", "REPO.bazel", "go.env"]);
    }
    // Reverse: B re-executes the cache (version 3, slowly) while A's
    // consumers (round 2: they run again, the cache is reused) wait.
    b.module("3", "2", "b2");
    a.module("1", "0", "a2");
    let first = b.spawn();
    b.wait_for("+gocache+gocache", "cache-start-3");
    let second = a.spawn();
    for output in [finished(first), finished(second)] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_exclusive(&[&a, &b]);
    assert_trees_clean(&store);
    // Every module of A's second round is in GOPATH, written after the
    // re-execution emptied it (the first round's are gone with it).
    for name in [
        "+consumer+c1",
        "+consumer+c2",
        "+consumer+c3",
        "+consumer+c4",
    ] {
        let module = gopath.join("pkg/mod");
        assert_eq!(
            fs::read_to_string(module.join(format!("{name}-a2/file"))).unwrap(),
            format!("{name} a2\n")
        );
        assert!(!module.join(format!("{name}-a1")).exists());
    }
}

/// Links a rule makes to a sibling repository's directory
/// (`../<sibling>/file`, relative or absolute, or through rctx.symlink) read
/// the sibling's file like genuine's; natively the published tree pins them
/// to the sibling's tree (an absolute path: the tree lives elsewhere than
/// the slot), never through the sibling's slot, whose link may change.
/// Links within the repository stay relative.
#[test]
fn links_to_a_sibling_repository_directory_are_pinned_to_its_tree() {
    let f = Fixture::new();
    let (read, _) = f.build("//:read_links");
    assert_eq!(read, genuine("sibling_links"));
    let sibling = linked_tree(&f.store(), "+sibling+sibling");
    let linker = linked_tree(&f.store(), "+linker+linker");
    for name in ["through_path", "relative", "absolute"] {
        assert_eq!(
            fs::read_link(linker.join(name)).unwrap(),
            sibling.join("file"),
            "{name}"
        );
    }
    for name in ["inside_relative", "inside_absolute"] {
        assert_eq!(
            fs::read_link(linker.join(name)).unwrap(),
            Path::new("inner"),
            "{name}"
        );
    }
}
