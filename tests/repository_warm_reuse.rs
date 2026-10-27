//! N61 cross-process verification: repository specification digests (the
//! bytes hashed before any execution) and content identities are identical
//! across separate process runs — for direct (`use_repo_rule`) and
//! extension-generated (`use_extension`) repositories — and a warm second
//! process reuses the publication pointers (nothing re-executes, nothing is
//! republished, the N52 lockfile is reused untouched).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/racy_window.rs");
include!("support/module_archives.rs");

use std::fs;
include!("support/temp_root.rs");
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::UNIX_EPOCH;

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("repo-warm-reuse-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        f.file(
            "MODULE.bazel",
            "module(name='main')\n\
             r = use_repo_rule('//:rules.bzl', 'maker')\n\
             r(name='direct_repo', src='//:source.txt')\n\
             e = use_extension('//:rules.bzl', 'gen')\n\
             use_repo(e, 'generated_repo')\n",
        );
        f.file("BUILD.bazel", "");
        f.file("source.txt", "stable input\n");
        f.file(
            "rules.bzl",
            "def _make(ctx):\n\
             \x20   ctx.read(ctx.attr.src)\n\
             \x20   ctx.file('BUILD.bazel', \"exports_files(['value'])\")\n\
             \x20   ctx.file('value', 'constant')\n\
             maker = repository_rule(implementation=_make, attrs={'src': attr.label()})\n\
             \n\
             def _ext(ctx):\n\
             \x20   maker(name='generated_repo', src='//:source.txt')\n\
             gen = module_extension(implementation=_ext)\n",
        );
        f
    }
    fn file(&self, name: &str, contents: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
    /// A separate bazel-rs PROCESS over `cache`, demanding both repositories
    /// by building the runfiles-consuming target. Default lockfile mode:
    /// the first run writes MODULE.bazel.lock (N52 extension entries), the
    /// warm run reuses them.
    fn run_build(&self, cache: &str) -> Output {
        self.command(cache)
            .args(["build", "--experimental-builtin-modules"])
            .arg(builtin_registry_flag())
            .arg("//:uses_repos")
            .output()
            .unwrap()
    }
    fn command(&self, cache: &str) -> Command {
        let mut command = Command::new("timeout");
        command
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                &format!("--cache-dir={cache}"),
            ])
            .current_dir(self.0.path());
        command
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Every persistent byte of a publication area: path, contents and the exact
/// modification stamp. A warm reuse must not append, rewrite or touch any of
/// them (execution republishes the tree and rewrites the pointer).
fn publication_state(dir: &Path) -> Vec<(PathBuf, Vec<u8>, u128)> {
    let mut state = Vec::new();
    if !dir.is_dir() {
        return state;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let stamp = fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        state.push((path.clone(), fs::read(&path).unwrap(), stamp));
    }
    // By path: a manifest rewritten by rename may change directory order.
    state.sort();
    state
}

fn assert_green(result: &Output) {
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
}

#[test]
fn spec_digests_and_identities_are_identical_across_separate_processes() {
    let f = Fixture::new();
    // A target whose inputs live in both repositories demands them both.
    f.file(
        "BUILD.bazel",
        "genrule(\n\
         \x20   name = 'uses_repos',\n\
         \x20   srcs = ['@direct_repo//:value', '@generated_repo//:value'],\n\
         \x20   outs = ['uses.txt'],\n\
         \x20   cmd = 'cat $(SRCS) > $@',\n\
         )\n",
    );
    for cache in ["cache-cold-a", "cache-cold-b"] {
        let result = f.run_build(cache);
        assert_green(&result);
    }
    let a = f.path("cache-cold-a/repositories/direct");
    let b = f.path("cache-cold-b/repositories/direct");
    // The pre-execution specification digests (direct/specs) — equal listings
    // mean equal spec bytes — for the direct repository, the
    // extension-generated repository and the registry/module entries.
    assert_eq!(listing(&a.join("specs")), listing(&b.join("specs")));
    // Content identities (direct/trees) are stable across processes too.
    assert_eq!(listing(&a.join("trees")), listing(&b.join("trees")));
    // Both repository kinds published exactly one specification each plus the
    // module repositories; nothing per-run about the sets.
    let specs = listing(&a.join("specs"));
    assert!(
        specs.len() >= 2,
        "direct and extension-generated repositories must publish specs: {specs:?}"
    );
}

#[test]
fn warm_second_process_hits_publication_pointers_and_lockfile() {
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "genrule(\n\
         \x20   name = 'uses_repos',\n\
         \x20   srcs = ['@direct_repo//:value', '@generated_repo//:value'],\n\
         \x20   outs = ['uses.txt'],\n\
         \x20   cmd = 'cat $(SRCS) > $@',\n\
         )\n",
    );
    assert_green(&f.run_build("cache-warm"));
    let direct = f.path("cache-warm/repositories/direct");
    let lockfile = f.path("MODULE.bazel.lock");
    let state = || {
        (
            listing(&direct.join("trees")),
            listing(&direct.join("specs")),
            publication_state(&direct.join("specs")),
            publication_state(&direct.join("manifests")),
            fs::read(&lockfile).unwrap_or_default(),
        )
    };
    // The cold publication recorded no stat fingerprints (its files were
    // younger than the store's racy margin). Once they have settled, the
    // first warm process hash-verifies them and persists their
    // fingerprints: its only write (support/racy_window.rs). Wait for that
    // deterministically instead of depending on how soon it starts.
    wait_until_settled(&direct.join("trees"));
    let cold = state();
    assert_green(&f.run_build("cache-warm"));
    let refreshed = state();
    assert_eq!(refreshed.0, cold.0, "no new trees");
    assert_eq!(refreshed.1, cold.1, "no new specs");
    assert_eq!(
        refreshed.2, cold.2,
        "spec pointers must be hit, not rewritten"
    );
    assert_eq!(refreshed.4, cold.4, "the lockfile must be reused untouched");
    assert_eq!(refreshed.3.len(), cold.3.len(), "no new publications");
    for ((path, bytes, _), (cold_path, cold_bytes, _)) in refreshed.3.iter().zip(&cold.3) {
        assert_eq!(path, cold_path);
        assert_eq!(
            without_fingerprints(bytes),
            without_fingerprints(cold_bytes),
            "a warm reuse only adds stat fingerprints to {}",
            path.display()
        );
        assert!(files_fingerprinted(bytes), "{}", path.display());
    }
    let before = refreshed;
    // Second warm process, same cache: the N52 lockfile is reused, the spec
    // pointers are validated and hit — no re-execution, no republication,
    // no lockfile rewrite, and with every fingerprint settled no refresh.
    assert_green(&f.run_build("cache-warm"));
    assert_eq!(listing(&direct.join("trees")), before.0, "no new trees");
    assert_eq!(listing(&direct.join("specs")), before.1, "no new specs");
    assert_eq!(
        publication_state(&direct.join("specs")),
        before.2,
        "spec pointers must be hit, not rewritten"
    );
    assert_eq!(
        publication_state(&direct.join("manifests")),
        before.3,
        "publications must be reused untouched"
    );
    assert_eq!(fs::read(&lockfile).unwrap_or_default(), before.4);
}

/// Step C: with the cache warm, parallel loading at several thread counts
/// (and under worker jitter) serves the repositories through the
/// reuse-only worker probe — byte-identical query output, and still no
/// new trees, no rewritten pointers, no re-execution.
#[test]
fn warm_parallel_query_is_byte_identical_and_reuses() {
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "genrule(\n\
         \x20   name = 'uses_repos',\n\
         \x20   srcs = ['@direct_repo//:value', '@generated_repo//:value'],\n\
         \x20   outs = ['uses.txt'],\n\
         \x20   cmd = 'cat $(SRCS) > $@',\n\
         )\n",
    );
    for i in 0..8 {
        // Packages demand both repositories via srcs labels.
        f.file(
            &format!("p{i}/BUILD.bazel"),
            "genrule(name = 'g', srcs = ['@direct_repo//:value', '@generated_repo//:value'], outs = ['o.txt'], cmd = 'cat $(SRCS) > $@')\n",
        );
    }
    assert_green(&f.run_build("cache-par"));
    let direct = f.path("cache-par/repositories/direct");
    let trees_before = listing(&direct.join("trees"));
    let specs_before = publication_state(&direct.join("specs"));
    let mut outputs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (threads, jitter) in [("1", None), ("8", None), ("32", None), ("8", Some("25"))] {
        let mut cmd = f.command("cache-par");
        cmd.args([
            "query",
            "--experimental-builtin-modules",
            &builtin_registry_flag(),
            &format!("--loading_phase_threads={threads}"),
            "//...",
        ]);
        if let Some(j) = jitter {
            cmd.env("BAZEL_RS_LOADING_JITTER", j);
        }
        let out = cmd.output().unwrap();
        assert_green(&out);
        outputs.push((out.stdout.clone(), out.stderr.clone()));
    }
    for pair in outputs.windows(2) {
        assert_eq!(
            pair[0].0, pair[1].0,
            "query stdout must be byte-identical across thread counts/jitter"
        );
    }
    assert_eq!(
        listing(&direct.join("trees")),
        trees_before,
        "no new trees on warm parallel query"
    );
    assert_eq!(
        publication_state(&direct.join("specs")),
        specs_before,
        "spec pointers hit, not rewritten, under the worker probe"
    );
}
