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
