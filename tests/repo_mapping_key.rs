//! Repository specifications cover only the repository mapping entries a
//! rule used (docs/REPO_CONTENTS_CACHE.md "Keys"): the mapping lookups of
//! its definition's loads and top-level `Label()` calls are part of the
//! specification, the lookups of `Label()` calls while it runs are recorded
//! inputs (Bazel's REPO_MAPPING recorded inputs). An unrelated dependency
//! or repository added to the root module leaves every repository reused;
//! a changed entry a rule used re-executes exactly that rule; the lockfile,
//! the hidden lockfile and stderr (with the action graph) equal a fresh
//! fetch's.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/sealed_trees.rs");
include!("support/module_archives.rs");

static NEXT: AtomicUsize = AtomicUsize::new(0);

const RULES: &str = r#"def _log(ctx):
    ctx.execute(["sh", "-c", "echo %s >> %s" % (ctx.original_name, ctx.attr.log)])

def _rec(ctx):
    _log(ctx)
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", ctx.attr.value + "\n")

rec = repository_rule(_rec, attrs = {"value": attr.string(), "log": attr.string()})

def _runtime(ctx):
    _log(ctx)
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", "runtime saw " + ctx.read(Label("@bar//:value.txt")))

runtime = repository_rule(_runtime, attrs = {"log": attr.string()})

def _gen(mctx):
    log = mctx.modules[0].tags.log[0].path
    rec(name = "a", value = "A", log = log)
    rec(name = "b", value = "B", log = log)
    runtime(name = "r", log = log)

gen = module_extension(_gen, tag_classes = {"log": tag_class(attrs = {"path": attr.string()})})
"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = crate::test_temp::root().join(format!(
            "native-repo-mapping-key-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = sealed_trees::remove_tree(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        Self { root }
    }

    fn log(&self) -> PathBuf {
        self.root.join("executions.log")
    }

    /// Executions since the last call, sorted.
    fn executions(&self) -> Vec<String> {
        let text = fs::read_to_string(self.log()).unwrap_or_default();
        let _ = fs::remove_file(self.log());
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    }

    /// A workspace `name`; `extra` is appended to MODULE.bazel; `bar` is
    /// what the root's `@bar` names (a use_repo alias of the extension).
    fn workspace(&self, name: &str, extra: &str, bar: &str) -> PathBuf {
        let ws = self.root.join(name);
        let _ = sealed_trees::remove_tree(&ws);
        fs::create_dir_all(&ws).unwrap();
        let log = self.log();
        fs::write(
            ws.join("MODULE.bazel"),
            format!(
                r#"module(name = "mk")
gen = use_extension("//:rules.bzl", "gen")
gen.log(path = "{log}")
use_repo(gen, "a", "b", "r", bar = "{bar}")
rec = use_repo_rule("//:rules.bzl", "rec")
rec(name = "d", value = "D", log = "{log}")
{extra}"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::write(ws.join("rules.bzl"), RULES).unwrap();
        fs::write(
            ws.join("BUILD.bazel"),
            r#"genrule(
    name = "use",
    srcs = ["@a//:value.txt", "@b//:value.txt", "@r//:value.txt", "@d//:value.txt"],
    outs = ["out.txt"],
    cmd = "cat $(SRCS) > $@",
)
"#,
        )
        .unwrap();
        ws
    }

    fn run(&self, ws: &Path, cache: &str) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.root.join(cache).display()))
            .arg("build")
            .arg(builtin_registry_flag())
            .arg("//:use")
            .current_dir(ws)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("BAZEL_RS_DUMP_ACTIONS", "1")
            .seeded_archives()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The worktree state directory of `ws` in `cache`.
    fn worktree(&self, cache: &str, ws: &Path) -> PathBuf {
        let name = ws.file_name().unwrap().to_string_lossy().into_owned();
        fs::read_dir(self.root.join(cache).join("worktrees"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|file| file.to_string_lossy().starts_with(&format!("{name}-")))
            })
            .expect("worktree")
    }

    fn out(&self, cache: &str, ws: &Path) -> String {
        fs::read_to_string(
            self.worktree(cache, ws)
                .join("execroot/_main/bazel-out/k8-fastbuild/bin/out.txt"),
        )
        .unwrap()
    }

    /// The hidden lockfile of the worktree of `ws` in `cache`.
    fn hidden_lockfile(&self, cache: &str, ws: &Path) -> Vec<u8> {
        fs::read(self.worktree(cache, ws).join("MODULE.bazel.lock")).unwrap_or_default()
    }

    fn trees(&self, cache: &str) -> Vec<(String, String)> {
        let slots = self.root.join(cache).join("repositories/external");
        let mut trees: Vec<(String, String)> = fs::read_dir(slots)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let target = fs::read_link(entry.path()).ok()?;
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    target.file_name()?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        trees.sort();
        trees
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.root);
    }
}

/// stderr with the cache and workspace paths normalized, timings and the
/// action execution summary (the same cache's action cache) dropped.
fn normalized(output: &Output, fixture: &Fixture, cache: &str, ws: &Path) -> String {
    let text = String::from_utf8_lossy(&output.stderr)
        .replace(&fixture.root.join(cache).display().to_string(), "<CACHE>")
        .replace(&ws.display().to_string(), "<WS>");
    text.lines()
        .filter(|line| {
            // The reused run's action is an action cache hit (same cache).
            !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("INFO: Elapsed time")
                && !line.contains(" actions executed, ")
                && !line.starts_with("INFO: 1 process")
                && !line.starts_with("INFO: 2 processes")
                && !line.starts_with("INFO: Build completed successfully")
        })
        .map(|line| match line.find("/worktrees/") {
            Some(at) => {
                let end = line[at..].find("/execroot").map_or(line.len(), |e| at + e);
                format!("{}/worktrees/WS{}", &line[..at], &line[end..])
            }
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn only_used_mapping_entries_decide_repository_reuse() {
    let f = Fixture::new();
    let ws1 = f.workspace("ws1", "", "a");
    f.run(&ws1, "c");
    assert_eq!(f.executions(), ["a", "b", "d", "r"]);
    assert_eq!(f.out("c", &ws1), "A\nB\nruntime saw A\nD\n");

    // An unrelated dependency and an unused repository change the root
    // module's mapping; no rule used those entries: nothing runs again.
    let unrelated = format!(
        "bazel_dep(name = \"platforms\", version = \"0.0.4\")\nrec(name = \"unused\", value = \"U\", log = \"{}\")\n",
        f.log().display()
    );
    let ws2 = f.workspace("ws2", &unrelated, "a");
    let reused = f.run(&ws2, "c");
    assert_eq!(f.executions(), Vec::<String>::new());
    // Exactly what a fresh fetch of that workspace publishes and prints.
    let ws2_fresh = f.workspace("ws2_fresh", &unrelated, "a");
    let fresh = f.run(&ws2_fresh, "fresh2");
    assert_eq!(f.executions(), ["a", "b", "d", "r"]);
    assert_eq!(
        normalized(&reused, &f, "c", &ws2),
        normalized(&fresh, &f, "fresh2", &ws2_fresh)
    );
    assert_eq!(
        fs::read(ws2.join("MODULE.bazel.lock")).unwrap(),
        fs::read(ws2_fresh.join("MODULE.bazel.lock")).unwrap()
    );
    assert_eq!(
        f.hidden_lockfile("c", &ws2),
        f.hidden_lockfile("fresh2", &ws2_fresh)
    );
    assert_eq!(f.trees("c"), f.trees("fresh2"));

    // The entry `r` used while it ran (`@bar`) now names another
    // repository: exactly `r` runs again.
    let ws3 = f.workspace("ws3", &unrelated, "b");
    let changed = f.run(&ws3, "c");
    assert_eq!(f.executions(), ["r"]);
    assert!(f.out("c", &ws3).contains("runtime saw B"));
    let ws3_fresh = f.workspace("ws3_fresh", &unrelated, "b");
    let fresh = f.run(&ws3_fresh, "fresh3");
    assert_eq!(f.executions(), ["a", "b", "d", "r"]);
    assert_eq!(
        normalized(&changed, &f, "c", &ws3),
        normalized(&fresh, &f, "fresh3", &ws3_fresh)
    );
    assert_eq!(
        fs::read(ws3.join("MODULE.bazel.lock")).unwrap(),
        fs::read(ws3_fresh.join("MODULE.bazel.lock")).unwrap()
    );
    assert_eq!(
        f.hidden_lockfile("c", &ws3),
        f.hidden_lockfile("fresh3", &ws3_fresh)
    );

    // Back: `r` runs again (the specification's pointer names its latest
    // publication, as for any recorded input that changes back); the
    // others stay reused.
    let ws4 = f.workspace("ws4", &unrelated, "a");
    f.run(&ws4, "c");
    assert_eq!(f.executions(), ["r"]);
    assert!(f.out("c", &ws4).contains("runtime saw A"));
}

/// A mapping entry the definition's loads used (a top-level `Label()` in
/// the rules' `.bzl` file) is part of the specification: changing it runs
/// every rule of that file again.
#[test]
fn load_time_mapping_entries_are_part_of_the_specification() {
    let f = Fixture::new();
    let rules = format!("_PINNED = Label(\"@bar//:value.txt\")\n{RULES}");
    let ws1 = f.workspace("ws1", "", "a");
    fs::write(ws1.join("rules.bzl"), &rules).unwrap();
    f.run(&ws1, "c");
    assert_eq!(f.executions(), ["a", "b", "d", "r"]);
    let ws2 = f.workspace("ws2", "", "b");
    fs::write(ws2.join("rules.bzl"), &rules).unwrap();
    f.run(&ws2, "c");
    assert_eq!(f.executions(), ["a", "b", "d", "r"]);
}
