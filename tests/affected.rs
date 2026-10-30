//! docs/AFFECTED_TARGETS.md: `bazel-rs affected --base=REV --head=REV`
//! plans both revisions in one git worktree and prints the requested
//! targets whose target digest changed (or that are new), one per line —
//! the output contract of target-determinator. A source edit affects its
//! target and the targets consuming it, a comment in a BUILD file nothing,
//! a new target itself; a test's property (`size`) its test.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Repo {
    base: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let base = tempfile::Builder::new()
            .prefix("affected-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        let repo = Self { base };
        fs::create_dir_all(repo.home()).unwrap();
        fs::create_dir_all(repo.main()).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.base.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.base().join("home")
    }

    fn main(&self) -> PathBuf {
        self.base().join("main")
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(self.main())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Writes `files` and commits them; returns the commit.
    fn commit(&self, files: &[(&str, &str)]) -> String {
        for (path, content) in files {
            let path = self.main().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "change"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn affected(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "240s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.base().join("cache").display()
            ))
            .arg("affected")
            .arg(EXPLICIT_MODULES)
            .args(args)
            .current_dir(self.main())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.home())
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// The affected labels between `base` and `head`.
    fn labels(&self, base: &str, head: &str, extra: &[&str]) -> Vec<String> {
        let mut args = vec![format!("--base={base}"), format!("--head={head}")];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.affected(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

const BUILD: &str = "\
genrule(name = 'a', srcs = ['a.txt'], outs = ['a.out'], cmd = 'cat $< > $@')
genrule(name = 'b', srcs = [':a'], outs = ['b.out'], cmd = 'cat $< > $@')
genrule(name = 'c', srcs = ['c.txt'], outs = ['c.out'], cmd = 'cat $< > $@')
";

#[test]
fn affected_targets_between_two_commits() {
    let repo = Repo::new();
    let first = repo.commit(&[
        (".gitignore", "bazel-rs-out\nMODULE.bazel.lock\n"),
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("BUILD.bazel", BUILD),
        ("a.txt", "a\n"),
        ("c.txt", "c\n"),
    ]);
    // A source of `a`: `a` and its consumer `b`.
    let second = repo.commit(&[("a.txt", "a2\n")]);
    let build = ["--affected_command=build"];
    assert_eq!(repo.labels(&first, &second, &build), ["//:a", "//:b"]);
    // Nothing changed: nothing affected (twice: the snapshot hits).
    assert!(repo.labels(&second, &second, &build).is_empty());
    // A comment and a new target: only the new target.
    let third = repo.commit(&[(
        "BUILD.bazel",
        &format!(
            "# a comment\n{BUILD}genrule(name = 'd', srcs = ['c.txt'], outs = ['d.out'], cmd = 'cat $< > $@')\n"
        ),
    )]);
    assert_eq!(repo.labels(&second, &third, &build), ["//:d"]);
    // The reverse direction: a removed target is not reported (as
    // target-determinator), `a`'s source change is.
    assert_eq!(repo.labels(&third, &first, &build), ["//:a", "//:b"]);
    // A pattern restricts the request.
    assert_eq!(
        repo.labels(&first, &second, &["--affected_command=build", "//:b"]),
        ["//:b"]
    );
    // Without --head: the working copy's HEAD.
    assert_eq!(repo.labels(&second, "HEAD", &build), ["//:d"]);
}

const DEFS: &str = "\
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + '_runner.sh')
    ctx.actions.write(out, '#!/bin/sh\\nexit 0\\n', is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.srcs))]

my_test = rule(_impl, test = True, attrs = {'srcs': attr.label_list(allow_files = True)})
";

#[test]
fn a_test_property_affects_its_test() {
    let repo = Repo::new();
    let test = |size: &str| {
        format!(
            "load(':defs.bzl', 'my_test')\nmy_test(name = 't', srcs = ['t.sh'], size = '{size}')\ngenrule(name = 'g', srcs = ['t.sh'], outs = ['g.out'], cmd = 'cat $< > $@')\n"
        )
    };
    let first = repo.commit(&[
        (".gitignore", "bazel-rs-out\nMODULE.bazel.lock\n"),
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("defs.bzl", DEFS),
        ("BUILD.bazel", &test("small")),
        ("t.sh", "#!/bin/sh\nexit 0\n"),
    ]);
    let second = repo.commit(&[("BUILD.bazel", &test("medium"))]);
    // `test --nobuild` (the default): the test's size is a test property.
    assert_eq!(repo.labels(&first, &second, &[]), ["//:t"]);
    // A source both use: both.
    let third = repo.commit(&[("t.sh", "#!/bin/sh\nexit 1\n")]);
    assert_eq!(repo.labels(&second, &third, &[]), ["//:g", "//:t"]);
}

impl Repo {
    /// The affected labels and the stderr.
    fn run(&self, base: &str, head: &str, extra: &[&str]) -> (Vec<String>, String) {
        let mut args = vec![format!("--base={base}"), format!("--head={head}")];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.affected(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let labels = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        (labels, stderr)
    }
}

const PACKAGES: &[(&str, &str)] = &[
    (".gitignore", "bazel-rs-out\nMODULE.bazel.lock\n"),
    ("MODULE.bazel", "module(name = 'main')\n"),
    (".bazelrc", "build --run_under=//tools:runner\n"),
    (
        "tools/BUILD.bazel",
        "filegroup(name = 'runner', srcs = ['run.sh'])\n",
    ),
    ("tools/run.sh", "#!/bin/sh\n"),
    (
        "a/BUILD.bazel",
        "genrule(name = 'a', srcs = ['a.txt'], outs = ['a.out'], cmd = 'cat $< > $@', visibility = ['//visibility:public'])\n",
    ),
    ("a/a.txt", "a\n"),
    ("a/unused.txt", "u\n"),
    (
        "b/BUILD.bazel",
        "genrule(name = 'b', srcs = ['//a'], outs = ['b.out'], cmd = 'cat $< > $@')\n",
    ),
    (
        "c/BUILD.bazel",
        "genrule(name = 'c', srcs = ['c.txt'], outs = ['c.out'], cmd = 'cat $< > $@')\n",
    ),
    ("c/c.txt", "c\n"),
];

#[test]
fn a_local_change_is_analysed_in_its_scope() {
    let repo = Repo::new();
    let build = ["--affected_command=build"];
    let first = repo.commit(PACKAGES);
    // A source: its consumers only are analysed, with the full analysis's
    // answer.
    let second = repo.commit(&[("a/a.txt", "a2\n")]);
    let (labels, stderr) = repo.run(&first, &second, &build);
    assert_eq!(labels, ["//a", "//b"]);
    assert!(stderr.contains("scoped analysis"), "{stderr}");
    assert!(
        stderr.contains("scope 2 of 4 selected target(s)"),
        "{stderr}"
    );
    let full = [build[0], "--scope=full"];
    assert_eq!(repo.run(&first, &second, &full).0, labels);
    // Both revisions at once: the same answer.
    let concurrent = [build[0], "--concurrent_revisions"];
    assert_eq!(repo.run(&first, &second, &concurrent).0, labels);
    // A file no target reads: nothing is analysed, nothing affected.
    let third = repo.commit(&[("a/unused.txt", "u2\n")]);
    let (labels, stderr) = repo.run(&second, &third, &build);
    assert!(labels.is_empty());
    assert!(stderr.contains("scope 0 of 4"), "{stderr}");
    // A configuration root's package (an rc flag's label): everything.
    let fourth = repo.commit(&[(
        "tools/BUILD.bazel",
        "filegroup(name = 'runner', srcs = ['run.sh'])\n# a comment\n",
    )]);
    let (labels, stderr) = repo.run(&third, &fourth, &build);
    assert!(labels.is_empty());
    assert!(stderr.contains("full: //tools:runner"), "{stderr}");
    // A global change: both revisions entirely.
    let fifth = repo.commit(&[("MODULE.bazel", "module(name = 'main')\n# x\n")]);
    let (labels, stderr) = repo.run(&fourth, &fifth, &build);
    assert!(labels.is_empty());
    assert!(stderr.contains("global change: MODULE.bazel"), "{stderr}");
    // An rc line the command does not read is no change.
    let sixth = repo.commit(&[(
        ".bazelrc",
        "build --run_under=//tools:runner\ntest:unused --x\n",
    )]);
    let (_, stderr) = repo.run(&fifth, &sixth, &build);
    assert!(stderr.contains("scoped analysis"), "{stderr}");
}

#[test]
fn stored_base_digests() {
    let repo = Repo::new();
    let build = "--affected_command=build";
    let first = repo.commit(PACKAGES);
    let second = repo.commit(&[("c/c.txt", "c2\n")]);
    let third = repo.commit(&[("a/a.txt", "a2\n")]);
    let file = repo.base().join("second.digests");
    let write = format!("--write_digests={}", file.display());
    // The main-branch run: the head's every digest is written.
    assert_eq!(repo.run(&first, &second, &[build, &write]).0, ["//c"]);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains(&format!("#commit\t{second}")), "{text}");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("//")).count(),
        4
    );
    // A later run on top of it: only the head is planned.
    let read = format!("--base_digests={}", file.display());
    let (labels, stderr) = repo.run(&second, &third, &[build, &read]);
    assert_eq!(labels, ["//a", "//b"]);
    assert!(stderr.contains("base digests from"), "{stderr}");
    assert_eq!(stderr.matches("bazel-rs affected: planning").count(), 1);
    // Another base: the file is not used.
    let (labels, stderr) = repo.run(&first, &third, &[build, &read]);
    assert_eq!(labels, ["//a", "//b", "//c"]);
    assert!(stderr.contains("does not hold this base"), "{stderr}");
}

#[test]
fn a_working_copy_planned_in_place_compares_with_the_worktree() {
    // The worktree and the working copy lie at paths of different lengths:
    // the digests are still alike (the root is normalized per value).
    let repo = Repo::new();
    let first = repo.commit(PACKAGES);
    fs::write(repo.main().join("c/c.txt"), "dirty\n").unwrap();
    let (labels, stderr) = repo.run(&first, "HEAD", &["--affected_command=build"]);
    let _ = stderr;
    assert!(labels.is_empty(), "{labels:?}");
    // Without --head: the dirty working copy itself.
    let args = [
        format!("--base={first}"),
        "--affected_command=build".to_owned(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = repo.affected(&args);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//c\n");
}
