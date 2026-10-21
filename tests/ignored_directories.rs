//! CLI behavior of Bazel 9.3 ignored subdirectories: `.bazelignore` prefixes
//! and REPO.bazel `ignore_directories()` patterns (docs/IGNORED_DIRECTORIES.md).
//! The fixture mirrors the shape of a real monorepo: `**/node_modules` in
//! REPO.bazel plus `.bazelignore` prefixes (with duplicates and a comment)
//! for `node_modules` trees, a Python `.venv`, a nested `.venv/lib`, and
//! top-level tool directories.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

const GARBAGE: &str = "this is not a BUILD file !!!\n";

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-ignored-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='code_fixture')\n");
        ws
    }

    /// A monorepo-shaped tree. Every BUILD file below an ignored directory is
    /// unparsable, so loading one would fail loudly.
    fn monorepo() -> Self {
        let ws = Self::new();
        ws.file("REPO.bazel", "ignore_directories([\"**/node_modules\"])\n");
        ws.file(
            ".bazelignore",
            "# tool and dependency trees\n.bwbin\npkg\nsrc/python/.venv\nsrc/beyondwork.ai/modelproxy/.venv/lib\nsrc/ts/bui/node_modules\nsrc/ts/node_modules\nsrc/ts/neui/node_modules\nsrc/ts/neui/node_modules\n.claude\n",
        );
        ws.file(
            "BUILD.bazel",
            "filegroup(name='root', srcs=glob(['**/*.txt']))\nfilegroup(name='through', srcs=['node_modules/b.txt'])\n",
        );
        for (path, content) in [
            ("notes.txt", "notes"),
            ("docs/a.txt", "a"),
            ("src/top.txt", "top"),
            ("node_modules/BUILD", GARBAGE),
            ("node_modules/b.txt", "b"),
            ("pkg/BUILD", GARBAGE),
            ("pkg/sub/BUILD.bazel", GARBAGE),
            ("pkg/c.txt", "c"),
            (".claude/BUILD", GARBAGE),
            (".claude/d.txt", "d"),
            (".bwbin/BUILD", GARBAGE),
            ("pkgs/BUILD.bazel", "filegroup(name='pkgs')\n"),
            (
                "src/ts/BUILD.bazel",
                "filegroup(name='ts', srcs=glob(['**/*.js']))\n",
            ),
            ("src/ts/x.txt", "x"),
            ("src/ts/node_modules/left-pad/BUILD.bazel", GARBAGE),
            ("src/ts/node_modules/left-pad/index.js", "js"),
            ("src/ts/index.js", "js"),
            ("src/ts/deep/a/node_modules/BUILD", GARBAGE),
            ("src/ts/deep/a/node_modules/x.js", "js"),
            ("src/ts/deep/a/y.js", "js"),
            ("src/ts/neui/node_modules/BUILD", GARBAGE),
            (
                "src/ts/bui/BUILD.bazel",
                "filegroup(name='bui', srcs=glob(['**/*.js']))\n",
            ),
            ("src/ts/bui/src/app.js", "js"),
            ("src/ts/bui/node_modules/dep/index.js", "js"),
            ("src/ts/bui/node_modules/dep/BUILD", GARBAGE),
            (
                "src/python/BUILD.bazel",
                "filegroup(name='py', srcs=glob(['**/*.py']))\n",
            ),
            ("src/python/main.py", "py"),
            ("src/python/.venv/z.py", "py"),
            ("src/python/.venv/lib/site/BUILD", GARBAGE),
            ("src/python/.venv/lib/site/x.py", "py"),
            (
                "src/beyondwork.ai/modelproxy/BUILD.bazel",
                "filegroup(name='mp', srcs=glob(['**/*.py']))\n",
            ),
            ("src/beyondwork.ai/modelproxy/server.py", "py"),
            ("src/beyondwork.ai/modelproxy/.venv/top.py", "py"),
            ("src/beyondwork.ai/modelproxy/.venv/lib/BUILD", GARBAGE),
            ("src/beyondwork.ai/modelproxy/.venv/lib/x.py", "py"),
            (
                "src/beyondwork.ai/modelproxy/.venv/bin/BUILD.bazel",
                "filegroup(name='venv_bin', srcs=glob(['*.py']))\n",
            ),
            ("src/beyondwork.ai/modelproxy/.venv/bin/tool.py", "py"),
        ] {
            ws.file(path, content);
        }
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }

    fn query(&self, pattern: &str) -> (Vec<String>, String) {
        let output = self.run(&["query", pattern]);
        assert!(output.status.success(), "{pattern}: {}", stderr(&output));
        let errors = stderr(&output);
        (
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect(),
            errors,
        )
    }

    fn built(&self, target: &str) -> Vec<String> {
        let output = self.run(&["build", target]);
        assert!(output.status.success(), "{target}: {}", stderr(&output));
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let path = Path::new(line.split_once('\t').unwrap().1);
                path.strip_prefix(&self.0)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn rejected(&self, args: &[&str], expected: &str) {
        let output = self.run(args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr(&output).contains(expected),
            "{args:?}: expected {expected:?} in {}",
            stderr(&output)
        );
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Startup notice, not a diagnostic (crate::memory_limit).
    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr + "\n"
    }
}

#[test]
fn recursive_query_skips_bazelignore_prefixes_and_repo_patterns() {
    let ws = Workspace::monorepo();
    let (labels, errors) = ws.query("//...");
    assert_eq!(
        labels,
        [
            "//:root",
            "//:through",
            "//pkgs:pkgs",
            "//src/beyondwork.ai/modelproxy:mp",
            "//src/beyondwork.ai/modelproxy/.venv/bin:venv_bin",
            "//src/python:py",
            "//src/ts:ts",
            "//src/ts/bui:bui",
        ]
    );
    assert!(!errors.contains("WARNING"), "{errors}");
    assert_eq!(
        ws.query("//src/ts/...").0,
        ["//src/ts:ts", "//src/ts/bui:bui"]
    );
    // Without the ignore files, the unparsable BUILD files are loaded.
    fs::remove_file(ws.0.join(".bazelignore")).unwrap();
    ws.rejected(&["query", "//pkg/..."], "pkg/BUILD");
    fs::write(ws.0.join("REPO.bazel"), "").unwrap();
    ws.rejected(&["query", "//src/ts/..."], "node_modules");
}

#[test]
fn patterns_below_ignored_directories_warn_and_select_nothing() {
    let ws = Workspace::monorepo();
    for (pattern, entry) in [
        ("//pkg/...", "pkg"),
        ("//pkg/sub/...", "pkg"),
        ("//.claude/...", ".claude"),
        ("//src/ts/node_modules/...", "src/ts/node_modules"),
        ("//src/ts/deep/a/node_modules/...", "**/node_modules"),
        ("//node_modules/...", "**/node_modules"),
        ("//src/python/.venv/lib/...", "src/python/.venv"),
    ] {
        let (labels, errors) = ws.query(pattern);
        assert!(labels.is_empty(), "{pattern}: {labels:?}");
        assert!(
            errors.contains(&format!(
                "WARNING: Pattern '{pattern}' was filtered out by ignored directory '{entry}'"
            )),
            "{pattern}: {errors}"
        );
    }
    // A nested .venv/lib prefix leaves its siblings alone.
    assert_eq!(
        ws.query("//src/beyondwork.ai/modelproxy/.venv/...").0,
        ["//src/beyondwork.ai/modelproxy/.venv/bin:venv_bin"]
    );
}

#[test]
fn packages_in_ignored_directories_are_deleted_packages() {
    let ws = Workspace::monorepo();
    for (args, package) in [
        (vec!["query", "//pkg:all"], "pkg"),
        (vec!["query", "//pkg/sub:x"], "pkg/sub"),
        (
            vec!["build", "//src/ts/node_modules/left-pad:all"],
            "src/ts/node_modules/left-pad",
        ),
        (
            vec!["query", "//src/ts/deep/a/node_modules:all"],
            "src/ts/deep/a/node_modules",
        ),
    ] {
        ws.rejected(
            &args,
            &format!(
                "no such package '{package}': Package is considered deleted due to --deleted_packages"
            ),
        );
    }
    ws.file(
        "pkgs/BUILD.bazel",
        "filegroup(name='pkgs', srcs=['//pkg:c.txt'])\n",
    );
    ws.rejected(
        &["build", "//pkgs"],
        "no such package 'pkg': Package is considered deleted due to --deleted_packages",
    );
    ws.file(
        "pkgs/BUILD.bazel",
        "load('//src/ts/node_modules/left-pad:defs.bzl', 'x')\n",
    );
    ws.file("src/ts/node_modules/left-pad/defs.bzl", "x = 1\n");
    ws.rejected(
        &["query", "//pkgs:all"],
        "Label '//src/ts/node_modules/left-pad:defs.bzl' is invalid because 'src/ts/node_modules/left-pad' is not a package; perhaps you meant to put the colon here: '//src/ts:node_modules/left-pad/defs.bzl'?",
    );
}

#[test]
fn globs_exclude_ignored_directories_deterministically() {
    let ws = Workspace::monorepo();
    assert_eq!(
        ws.built("//:root"),
        ["docs/a.txt", "notes.txt", "src/top.txt"]
    );
    assert_eq!(
        ws.built("//src/ts:ts"),
        ["src/ts/deep/a/y.js", "src/ts/index.js"]
    );
    assert_eq!(ws.built("//src/ts/bui"), ["src/ts/bui/src/app.js"]);
    assert_eq!(ws.built("//src/python:py"), ["src/python/main.py"]);
    assert_eq!(
        ws.built("//src/beyondwork.ai/modelproxy:mp"),
        [
            "src/beyondwork.ai/modelproxy/.venv/top.py",
            "src/beyondwork.ai/modelproxy/server.py"
        ]
    );
    // An ignored directory with a BUILD file is not a subpackage boundary.
    assert_eq!(ws.built("//:through"), ["node_modules/b.txt"]);
    // Rebuilding yields the identical, sorted selection.
    assert_eq!(
        ws.built("//:root"),
        ["docs/a.txt", "notes.txt", "src/top.txt"]
    );
}

#[test]
fn ignore_file_errors_are_bazels() {
    let ws = Workspace::monorepo();
    ws.file(".bazelignore", "ok\n/abs/dir/\n");
    ws.rejected(
        &["query", "//..."],
        &format!(
            "Invalid path in {}: '/abs/dir': cannot be an absolute path",
            fs::canonicalize(&ws.0)
                .unwrap()
                .join(".bazelignore")
                .display()
        ),
    );
    ws.file(".bazelignore", "");
    ws.file(
        "REPO.bazel",
        "ignore_directories(['a'])\nignore_directories(['b'])\n",
    );
    ws.rejected(
        &["query", "//..."],
        "'ignored_directories()' can only be called once",
    );
    ws.file("REPO.bazel", "ignore_directories('node_modules')\n");
    ws.rejected(
        &["query", "//:all"],
        "in call to ignore_directories(), parameter 'dirs' got value of type 'string', want 'sequence'",
    );
    for (repo, expected) in [
        (
            "ignore_directories()\n",
            "ignore_directories() missing 1 required positional argument: dirs",
        ),
        (
            "ignore_directories(dirs = ['b'])\n",
            "ignore_directories() got named argument for positional-only parameter 'dirs'",
        ),
        (
            "ignore_directories(['b'], ['c'])\n",
            "ignore_directories() accepts no more than 1 positional argument but got 2",
        ),
    ] {
        ws.file("REPO.bazel", repo);
        ws.rejected(&["query", "//:all"], expected);
    }
    // RepoFileFunction: the IOException message is exactly this, no cause.
    fs::remove_file(ws.0.join("REPO.bazel")).unwrap();
    fs::create_dir(ws.0.join("REPO.bazel")).unwrap();
    ws.rejected(
        &["query", "//:all"],
        &format!(
            "error reading REPO.bazel file at {}\n",
            fs::canonicalize(&ws.0)
                .unwrap()
                .join("REPO.bazel")
                .display()
        ),
    );
}

/// Bazel's execroot symlink forest omits top-level entries that are ignored
/// (SymlinkForest.symlinkShouldBePlanted), so a declared source input below
/// one is missing when the action runs; nested ignored directories are
/// reachable through their planted top-level ancestor.
#[test]
fn action_inputs_below_ignored_top_level_entries_are_not_planted() {
    let ws = Workspace::monorepo();
    ws.file(
        "BUILD.bazel",
        "filegroup(name='through', srcs=['node_modules/b.txt'])\ngenrule(name='cat', srcs=['node_modules/b.txt'], outs=['cat.txt'], cmd='cat $(SRCS) > $@')\ngenrule(name='prefix', srcs=['pkg/c.txt'], outs=['prefix.txt'], cmd='cat $(SRCS) > $@')\ngenrule(name='ok', srcs=['docs/a.txt'], outs=['ok.txt'], cmd='cat $(SRCS) > $@')\n",
    );
    ws.file(
        "src/ts/BUILD.bazel",
        "genrule(name='cat', srcs=['node_modules/left-pad/index.js', 'deep/a/node_modules/x.js'], outs=['cat.txt'], cmd='cat $(SRCS) > $@')\n",
    );
    for target in ["//:cat", "//:prefix"] {
        let output = ws.run(&["build", target]);
        assert!(!output.status.success(), "{target}");
        assert!(
            stderr(&output).contains(": No such file or directory"),
            "{target}: {}",
            stderr(&output)
        );
    }
    // Source targets themselves (no action) still build, like Bazel.
    assert_eq!(ws.built("//:through"), ["node_modules/b.txt"]);
    let built = ws.built("//src/ts:cat");
    assert_eq!(fs::read_to_string(ws.0.join(&built[0])).unwrap(), "jsjs");
    let built = ws.built("//:ok");
    assert_eq!(fs::read_to_string(ws.0.join(&built[0])).unwrap(), "a");
    // Without the REPO.bazel pattern (and its BUILD file, which would then
    // be a subpackage boundary), node_modules is planted again.
    ws.file("REPO.bazel", "");
    fs::remove_file(ws.0.join("node_modules/BUILD")).unwrap();
    let built = ws.built("//:cat");
    assert_eq!(fs::read_to_string(ws.0.join(&built[0])).unwrap(), "b");
}

/// A declared source input that does not exist is reported by Bazel's
/// ArtifactFunction ("missing input file") whether or not the execroot would
/// plant it: inputs below ignored top-level entries are checked too
/// (skyframe/ArtifactFunction.java:79-90, ActionExecutionFunction.java:1543-1583).
#[test]
fn missing_inputs_below_ignored_top_level_entries_are_reported() {
    let ws = Workspace::monorepo();
    ws.file(
        "BUILD.bazel",
        "genrule(name='gone', srcs=['node_modules/gone.txt', 'docs/a.txt'], outs=['gone.out'], cmd='cat $(SRCS) > $@')
genrule(name='gone_pkg', srcs=['pkg/gone.txt'], outs=['gone_pkg.out'], cmd='cat $(SRCS) > $@')
",
    );
    let root = ws.0.canonicalize().unwrap();
    for (target, line, missing) in [
        ("//:gone", 1, "//:node_modules/gone.txt"),
        ("//:gone_pkg", 2, "//:pkg/gone.txt"),
    ] {
        let output = ws.run(&["build", target]);
        assert!(!output.status.success(), "{target}");
        let location = format!("{}/BUILD.bazel:{line}:8", root.display());
        assert_eq!(
            stderr(&output),
            format!(
                "bazel-rs: {location}: Executing genrule {target} failed: missing input file '{missing}'\n{location}: Executing genrule {target} failed: 1 input file(s) do not exist\n"
            ),
            "{target}"
        );
    }
}

/// TargetPattern warnings are reported before a later pattern fails.
#[test]
fn filtered_pattern_warnings_precede_later_errors() {
    let ws = Workspace::monorepo();
    ws.rejected(
        &["build", "//pkg/...", "//missing:x"],
        "WARNING: Pattern '//pkg/...' was filtered out by ignored directory 'pkg'\n",
    );
}

/// The N13 genuine-capture scenario `B/repo_ignore_directories_recursive`
/// (tests/fixtures/native_rules/reference_receipt.json): Bazel 9.3 exits 0;
/// the ignored subpackage is simply not discovered.
#[test]
fn n13_receipt_scenario_now_succeeds() {
    let ws = Workspace::new();
    ws.file("REPO.bazel", "ignore_directories(['probe_repo/ignored'])\n");
    ws.file("probe_repo/BUILD.bazel", "filegroup(name='f')\n");
    ws.file("probe_repo/ignored/BUILD.bazel", "filegroup(name='g')\n");
    let (labels, errors) = ws.query("//probe_repo/...");
    assert_eq!(labels, ["//probe_repo:f"]);
    assert!(!errors.contains("WARNING"), "{errors}");
}
