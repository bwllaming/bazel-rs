//! docs/CACHE_GC.md: the native cache's garbage collection through the CLI.
//! A scratch workspace (explicit-module profile, offline) with genrules and
//! a repository rule, and an explicit `--cache-dir`: the size budget removes
//! the least recently used entries and a later build re-executes exactly
//! those; builds and collections running at the same time never fail and
//! never see a partial entry; the state of a deleted worktree is swept; a
//! collection killed at any removal step leaves a cache every later build
//! uses correctly; the automatic collection at the end of a command follows
//! the idle delay.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DAY: i64 = 24 * 60 * 60;

const MODULE: &str = "module(name = 'main')\n\
    gen = use_repo_rule('//:repo.bzl', 'gen')\n\
    gen(name = 'gen')\n";

const REPO_RULE: &str = r#"
def _impl(ctx):
    ctx.file("value.txt", "from the repository\n")
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

gen = repository_rule(_impl)
"#;

const BUILD: &str = r#"
genrule(name = "a", outs = ["a.txt"], cmd = "echo alpha > $@")
genrule(name = "b", outs = ["b.txt"], cmd = "echo beta > $@")
genrule(name = "c", srcs = ["@gen//:value.txt"], outs = ["c.txt"], cmd = "cat $< > $@; echo gamma >> $@")
genrule(name = "d", srcs = [":a", ":b"], outs = ["d.txt"], cmd = "cat $(SRCS) > $@")
"#;

struct Fixture {
    base: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::Builder::new()
            .prefix("cache-gc-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let fixture = Self { base };
        fs::create_dir_all(fixture.home()).unwrap();
        fixture.workspace("ws");
        fixture
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.base.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.base().join("home")
    }

    fn cache(&self) -> PathBuf {
        self.base().join("cache")
    }

    /// A workspace `<base>/<name>` with the fixture's files.
    fn workspace(&self, name: &str) -> PathBuf {
        let root = self.base().join(name);
        fs::create_dir_all(&root).unwrap();
        for (path, content) in [
            ("MODULE.bazel", MODULE),
            ("repo.bzl", REPO_RULE),
            ("BUILD", BUILD),
        ] {
            fs::write(root.join(path), content).unwrap();
        }
        root
    }

    fn command(&self, workspace: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "240s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache().display()))
            .args(args)
            .current_dir(self.base().join(workspace))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.home())
            .env("LC_ALL", "C")
            .envs(env.iter().copied());
        command
    }

    fn run(&self, workspace: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let output = self.command(workspace, args, env).output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// A successful build of `targets` in `workspace`: its stderr; every
    /// output has its expected content.
    fn build(&self, workspace: &str, targets: &[&str], extra: &[&str]) -> String {
        let mut args = vec!["build", "--experimental-builtin-modules=false"];
        args.extend_from_slice(extra);
        args.extend_from_slice(targets);
        let output = self.run(workspace, &args, &[]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        check_outputs(&output);
        stderr
    }

    fn gc(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut all = vec!["gc"];
        all.extend_from_slice(args);
        self.run("ws", &all, env)
    }

    /// Make everything the cache remembers as used look last used `days`
    /// ago: action manifests, test statuses, tree manifests and stamps.
    fn backdate(&self, days: i64) {
        let time = now() - days * DAY;
        let cache = self.cache();
        let mut paths = Vec::new();
        for entry in list(&cache.join("actions")) {
            paths.push(entry.join("manifest.json"));
        }
        paths.extend(list(&cache.join("gc/used")));
        paths.extend(list(&cache.join("repositories/direct/manifests")));
        paths.extend(list(&cache.join("repositories/downloads/sha256")));
        for path in paths {
            set_times(&path, time);
        }
    }
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn set_times(path: &Path, secs: i64) {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps};
    let time = Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    let _ = rustix::fs::utimensat(
        CWD,
        path,
        &Timestamps {
            last_access: time,
            last_modification: time,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    );
}

fn list(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

/// Every built output (`label<TAB>path` lines) has the content its genrule
/// writes.
fn check_outputs(output: &Output) {
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((label, path)) = line.split_once('\t') else {
            continue;
        };
        let content = fs::read_to_string(path).unwrap_or_else(|e| panic!("{label} {path}: {e}"));
        let expected = match label {
            "//:a" => "alpha\n",
            "//:b" => "beta\n",
            "//:c" => "from the repository\ngamma\n",
            "//:d" => "alpha\nbeta\n",
            _ => continue,
        };
        assert_eq!(content, expected, "{label}");
    }
}

fn executed(stderr: &str) -> usize {
    let line = stderr
        .lines()
        .find(|line| line.contains(" actions executed, "))
        .unwrap_or_else(|| panic!("no summary in {stderr}"));
    line.trim_start_matches("bazel-rs: ")
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// Whether the action cache holds the entry of genrule output `name`.
fn cached(fixture: &Fixture, name: &str) -> bool {
    list(&fixture.cache().join("actions")).iter().any(|entry| {
        entry
            .join("outputs/bazel-out/k8-fastbuild/bin")
            .join(name)
            .exists()
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The size budget removes the least recently used entries; what was used
/// in the last day stays even under a budget of one byte; a later build
/// re-executes exactly the removed actions and gets the same outputs.
#[test]
fn the_budget_removes_the_least_recently_used_and_builds_execute_them_again() {
    let fixture = Fixture::new();
    fixture.build("ws", &["//:a"], &[]);
    fixture.backdate(3);
    let warm = fixture.build("ws", &["//:b", "//:c"], &[]);
    assert_eq!(executed(&warm), 2, "{warm}");
    let before = list(&fixture.cache().join("actions")).len();
    let output = fixture.gc(&["--experimental_disk_cache_gc_max_size=1"], &[]);
    assert!(output.status.success());
    let report = stdout(&output);
    assert!(report.contains("Deleted "), "{report}");
    assert!(report.contains("size budget  1 B"), "{report}");
    let after = list(&fixture.cache().join("actions")).len();
    assert!(after < before, "{before} -> {after}: {report}");
    assert!(!cached(&fixture, "a.txt"), "{report}");
    assert!(cached(&fixture, "b.txt") && cached(&fixture, "c.txt"));
    // The repository tree of //:c was used now: it stays.
    assert_eq!(
        list(&fixture.cache().join("repositories/direct/trees")).len(),
        1
    );
    // //:a's action went and runs again; //:b and //:c are hits.
    let again = fixture.build("ws", &["//:a", "//:b", "//:c"], &[]);
    assert_eq!(executed(&again), 1, "{again}");
    // A dry run reports and removes nothing.
    let after = list(&fixture.cache().join("actions")).len();
    fixture.backdate(3);
    let output = fixture.gc(
        &["--dry_run", "--experimental_disk_cache_gc_max_size=1"],
        &[],
    );
    assert!(stdout(&output).contains("Would delete "));
    assert_eq!(list(&fixture.cache().join("actions")).len(), after);
    assert!(list(&fixture.cache().join("gc/trash")).is_empty());
}

/// Builds of the same and of other targets and collections that remove
/// everything they may, all at once: every build succeeds with the right
/// outputs (a collection waits until no build uses the cache; a build
/// waits while a collection removes).
#[test]
fn builds_and_collections_at_the_same_time_never_fail() {
    let fixture = Fixture::new();
    fixture.build("ws", &["//:a", "//:b", "//:c", "//:d"], &[]);
    for _ in 0..3 {
        fixture.backdate(90);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for targets in [
                &["//:a", "//:d"][..],
                &["//:b", "//:c"],
                &["//:d"],
                &["//:a", "//:b", "//:c", "//:d"],
            ] {
                let fixture = &fixture;
                handles.push(scope.spawn(move || {
                    fixture.build(
                        "ws",
                        targets,
                        &[
                            "--experimental_disk_cache_gc_idle_delay=0",
                            "--experimental_disk_cache_gc_max_size=1",
                            "--experimental_disk_cache_gc_max_age=1m",
                            "--repo_contents_cache_gc_max_age=1m",
                        ],
                    );
                }));
            }
            for _ in 0..3 {
                let fixture = &fixture;
                handles.push(scope.spawn(move || {
                    let output = fixture.gc(
                        &[
                            "--experimental_disk_cache_gc_max_size=1",
                            "--experimental_disk_cache_gc_max_age=1m",
                            "--repo_contents_cache_gc_max_age=1m",
                        ],
                        &[],
                    );
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
    }
    // A last collection after everything: the cache is still whole.
    fixture.backdate(90);
    assert!(
        fixture
            .gc(&["--experimental_disk_cache_gc_max_age=1d"], &[])
            .status
            .success()
    );
    let last = fixture.build("ws", &["//:a", "//:b", "//:c", "//:d"], &[]);
    assert_eq!(executed(&last), 4, "{last}");
    assert!(list(&fixture.cache().join("gc/trash")).is_empty());
}

/// The state directory of a worktree whose checkout is gone is removed; a
/// live worktree's stays.
#[test]
fn the_state_of_a_deleted_worktree_is_swept() {
    let fixture = Fixture::new();
    fixture.workspace("other");
    fixture.build("ws", &["//:a"], &[]);
    fixture.build("other", &["//:a"], &[]);
    let states = list(&fixture.cache().join("worktrees"));
    assert_eq!(states.len(), 2, "{states:?}");
    let records = list(&fixture.cache().join("gc/worktrees"));
    assert_eq!(records.len(), 2, "{records:?}");
    fs::remove_dir_all(fixture.base().join("other")).unwrap();
    let output = fixture.gc(&[], &[]);
    assert!(output.status.success());
    let report = stdout(&output);
    assert!(report.contains("worktrees    1 of 1 entries"), "{report}");
    let states = list(&fixture.cache().join("worktrees"));
    assert_eq!(states.len(), 1, "{states:?}");
    assert!(
        states[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ws-")
    );
    assert_eq!(list(&fixture.cache().join("gc/worktrees")).len(), 1);
    fixture.build("ws", &["//:a"], &[]);
}

/// A collection killed at its first, second, ... removal step leaves a
/// cache from which every build gets the right outputs (re-executing what
/// is gone, never trusting a partial entry), and the next collection
/// removes what the killed one left.
#[test]
fn a_killed_collection_leaves_a_cache_that_builds_correctly() {
    let fixture = Fixture::new();
    let all = ["//:a", "//:b", "//:c", "//:d"];
    fixture.build("ws", &all, &[]);
    for step in 1..=8 {
        fixture.backdate(90);
        let step = step.to_string();
        let output = fixture.gc(
            &[
                "--experimental_disk_cache_gc_max_age=1d",
                "--repo_contents_cache_gc_max_age=1d",
            ],
            &[("BAZEL_RS_CACHE_GC_ABORT_AFTER", &step)],
        );
        if output.status.success() {
            // Fewer steps than that: the collection completed.
            break;
        }
        fixture.build("ws", &all, &[]);
    }
    fixture.backdate(90);
    let output = fixture.gc(&["--experimental_disk_cache_gc_max_age=1d"], &[]);
    assert!(output.status.success());
    assert!(list(&fixture.cache().join("gc/trash")).is_empty());
    fixture.build("ws", &all, &[]);
}

/// At the end of a command the collection runs when the last one is at
/// least the idle delay old, and says what it removed.
#[test]
fn commands_collect_at_their_end_after_the_idle_delay() {
    let fixture = Fixture::new();
    let flags = [
        "--experimental_disk_cache_gc_idle_delay=1h",
        "--experimental_disk_cache_gc_max_age=2d",
    ];
    // The first command collects (no collection before): nothing to remove.
    let first = fixture.build("ws", &["//:a"], &flags);
    assert!(!first.contains("cache garbage collection"), "{first}");
    assert!(fixture.cache().join("gc/stamp").is_file());
    fixture.backdate(3);
    // Within the idle delay: no collection.
    let second = fixture.build("ws", &["//:b"], &flags);
    assert!(!second.contains("cache garbage collection"), "{second}");
    assert!(cached(&fixture, "a.txt") && cached(&fixture, "b.txt"));
    // An idle delay of zero: //:a's entry (unused for 3 days) goes.
    let third = fixture.build(
        "ws",
        &["//:b"],
        &[
            "--experimental_disk_cache_gc_idle_delay=0",
            "--experimental_disk_cache_gc_max_age=2d",
        ],
    );
    assert!(
        third.contains("bazel-rs: cache garbage collection: deleted "),
        "{third}"
    );
    assert!(!cached(&fixture, "a.txt") && cached(&fixture, "b.txt"));
    // BAZEL_RS_CACHE_GC=off: never automatically.
    fixture.backdate(3);
    let mut args = vec![
        "build",
        "--experimental-builtin-modules=false",
        "--experimental_disk_cache_gc_idle_delay=0",
        "--experimental_disk_cache_gc_max_age=2d",
        "//:a",
    ];
    let output = fixture.run("ws", &args, &[("BAZEL_RS_CACHE_GC", "off")]);
    assert!(output.status.success());
    assert!(cached(&fixture, "a.txt") && cached(&fixture, "b.txt"));
    // Invalid values fail like genuine's converters.
    args[3] = "--experimental_disk_cache_gc_max_age=2x";
    let output = fixture.run("ws", &args, &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "While parsing option --experimental_disk_cache_gc_max_age=2x: Illegal duration '2x'."
        ),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
