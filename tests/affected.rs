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
        self.affected_with(args, &[])
    }

    fn affected_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
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
            .envs(env.iter().copied())
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

/// A `manual` image-like target (a docker macro's push target) beside
/// ordinary ones, and a `manual` test.
fn manual_packages(cmd: &str, size: &str) -> Vec<(&'static str, String)> {
    let mut files: Vec<(&'static str, String)> = PACKAGES
        .iter()
        .map(|(path, content)| (*path, (*content).to_owned()))
        .collect();
    files.push(("img/defs.bzl", DEFS.to_owned()));
    files.push((
        "img/BUILD.bazel",
        format!(
            "load(':defs.bzl', 'my_test')\n\
             genrule(name = 'push', srcs = ['//a'], outs = ['push.out'], cmd = '{cmd}', tags = ['manual'])\n\
             my_test(name = 't', srcs = ['t.sh'], size = '{size}', tags = ['manual'])\n"
        ),
    ));
    files.push(("img/t.sh", "#!/bin/sh\nexit 0\n".to_owned()));
    files
}

#[test]
fn include_manual_reports_manual_targets() {
    let repo = Repo::new();
    let commit = |files: Vec<(&'static str, String)>| {
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (*path, content.as_str()))
            .collect();
        repo.commit(&files)
    };
    let first = commit(manual_packages("cat $< > $@", "small"));
    // Only the manual target's own attribute (a macro argument): nothing
    // without the option, the target with it, scoped or not.
    let second = commit(manual_packages("cat $< $< > $@", "small"));
    let build = "--affected_command=build";
    let (labels, stderr) = repo.run(&first, &second, &[build]);
    assert!(labels.is_empty(), "{labels:?}");
    assert!(stderr.contains("scoped analysis"), "{stderr}");
    let (labels, stderr) = repo.run(&first, &second, &[build, "--include_manual"]);
    assert_eq!(labels, ["//img:push"]);
    assert!(stderr.contains("scoped analysis"), "{stderr}");
    assert_eq!(
        repo.run(
            &first,
            &second,
            &[build, "--include_manual", "--scope=full"]
        )
        .0,
        ["//img:push"]
    );
    // `test` (the default command): a manual test's property too.
    let third = commit(manual_packages("cat $< $< > $@", "medium"));
    assert!(repo.run(&second, &third, &[]).0.is_empty());
    assert_eq!(
        repo.run(&second, &third, &["--include_manual"]).0,
        ["//img:t"]
    );
    assert_eq!(
        repo.run(&second, &third, &["--include_manual", "--scope=full"])
            .0,
        ["//img:t"]
    );
    // A source the manual target consumes: its dependants with it.
    let mut files = manual_packages("cat $< $< > $@", "medium");
    files.push(("a/a.txt", "a2\n".to_owned()));
    let fourth = commit(files);
    assert_eq!(repo.run(&third, &fourth, &[build]).0, ["//a", "//b"]);
    assert_eq!(
        repo.run(&third, &fourth, &[build, "--include_manual"]).0,
        ["//a", "//b", "//img:push"]
    );
    // Stored digests are keyed by the option.
    let file = repo.base().join("manual.digests");
    let write = format!("--write_digests={}", file.display());
    let read = format!("--base_digests={}", file.display());
    repo.run(&second, &third, &[build, "--include_manual", &write]);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("//img:push\t"), "{text}");
    let (labels, stderr) = repo.run(&third, &fourth, &[build, &read]);
    assert!(stderr.contains("does not hold this base"), "{stderr}");
    assert_eq!(labels, ["//a", "//b"]);
    let (labels, stderr) = repo.run(&third, &fourth, &[build, "--include_manual", &read]);
    assert!(stderr.contains("base digests from"), "{stderr}");
    assert_eq!(labels, ["//a", "//b", "//img:push"]);
}

#[test]
fn a_test_nobuild_planning_exits_0_and_a_failed_one_fails_affected() {
    let repo = Repo::new();
    let first = repo.commit(PACKAGES);
    let second = repo.commit(&[("a/a.txt", "a2\n")]);
    // `test --nobuild` (the default command) without test targets in the
    // scope, scoped and entirely: every planning exits 0.
    for extra in [&[][..], &["--scope=full"][..]] {
        let (labels, stderr) = repo.run(&first, &second, extra);
        assert_eq!(labels, ["//a", "//b"], "{stderr}");
        assert_eq!(stderr.matches("bazel-rs affected: planned ").count(), 2);
        assert_eq!(stderr.matches(" (exit 0)").count(), 2, "{stderr}");
        assert!(!stderr.contains("Unable to run tests"), "{stderr}");
    }
    // A head whose analysis fails: `affected` fails, it reports nothing.
    let broken = repo.commit(&[(
        "b/BUILD.bazel",
        "genrule(name = 'b', srcs = ['//a:missing'], outs = ['b.out'], cmd = 'cat $< > $@')\n",
    )]);
    for extra in [&[][..], &["--scope=full"][..], &["--keep_going"][..]] {
        let mut args = vec![format!("--base={second}"), format!("--head={broken}")];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = repo.affected(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{extra:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{extra:?}: {stderr}");
        assert!(stderr.contains("failed (exit 1"), "{extra:?}: {stderr}");
        assert!(!stderr.contains("compared target(s) affected"), "{stderr}");
    }
    // A base that fails: an error too, unless `--base_failure=all` asks for
    // target-determinator's answer (every target of the head).
    let fixed = repo.commit(&[("b/BUILD.bazel", PACKAGES[8].1)]);
    let args = [format!("--base={broken}"), format!("--head={fixed}")];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = repo.affected(&args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(stderr.contains("--base_failure=all"), "{stderr}");
    // (Within the head's scope: //b's package changed.)
    let (labels, stderr) = repo.run(&broken, &fixed, &["--base_failure=all"]);
    assert_eq!(labels, ["//b"], "{stderr}");
    assert!(
        stderr.contains("every target of the head revision"),
        "{stderr}"
    );
}

#[test]
fn files_in_no_target_affect_nothing() {
    // ~/code's first CI run: workflow files, scripts in a package of
    // `exports_files` (one deleted), a Python file in a directory no glob
    // covers. The scope holds the dependants of the package whose file
    // list changed; nothing is affected, and every planning exits 0.
    let repo = Repo::new();
    let mut files: Vec<(&str, &str)> = PACKAGES.to_vec();
    files.extend([
        ("BUILD.bazel", "# the root package\n"),
        (".github/workflows/lint.yaml", "on: push\n"),
        ("bin/BUILD.bazel", "exports_files(['tool.sh'])\n"),
        ("bin/tool.sh", "#!/bin/sh\n"),
        ("bin/bazel", "#!/bin/sh\n"),
        ("bin/old.test.sh", "#!/bin/sh\n"),
        (
            "d/BUILD.bazel",
            "genrule(name = 'd', srcs = ['//bin:tool.sh'], outs = ['d.out'], cmd = 'cat $< > $@')\n",
        ),
        ("a/tools/build/__init__.py", "x = 1\n"),
    ]);
    let first = repo.commit(&files);
    repo.git(&["rm", "-q", "bin/old.test.sh"]);
    let second = repo.commit(&[
        (".github/workflows/lint.yaml", "on: pull_request\n"),
        ("bin/bazel", "#!/bin/sh\nexit 0\n"),
        ("a/tools/build/__init__.py", "x = 2\n"),
    ]);
    let (labels, stderr) = repo.run(&first, &second, &["--explain_affected"]);
    assert!(labels.is_empty(), "{labels:?}: {stderr}");
    assert!(stderr.contains("scope 1 of 5"), "{stderr}");
    assert_eq!(stderr.matches(" (exit 0)").count(), 2, "{stderr}");
    let (labels, _) = repo.run(&first, &second, &["--scope=full"]);
    assert!(labels.is_empty(), "{labels:?}");
}

/// The planning analyses print none of their Starlark DEBUG, WARNING or
/// INFO events (target-determinator's cquery shows none either): a
/// `print()` of a rule implementation, a deprecation warning, the
/// `INFO: Analyzed …` summary stay out of `affected`'s stderr, its own
/// progress lines stay; an analysis error still shows its message.
#[test]
fn planning_events_stay_quiet_but_errors_show() {
    let repo = Repo::new();
    let defs = "\
def _impl(ctx):
    print('noisy impl ' + ctx.label.name)
    if ctx.attr.boom:
        fail('loud failure of ' + ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + '.out')
    ctx.actions.run_shell(
        inputs = [ctx.file.src],
        outputs = [out],
        command = 'cat $1 > $2',
        arguments = [ctx.file.src.path, out.path],
    )
    return [DefaultInfo(files = depset([out]))]

noisy = rule(
    implementation = _impl,
    attrs = {'src': attr.label(allow_single_file = True), 'boom': attr.bool()},
)
";
    let build = "\
load(':defs.bzl', 'noisy')
noisy(name = 'n', src = 'a.txt')
filegroup(name = 'uses_old', srcs = [':n'], data = ['//dep:old'])
";
    let first = repo.commit(&[
        (".gitignore", "bazel-rs-out\nMODULE.bazel.lock\n"),
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("defs.bzl", defs),
        ("BUILD.bazel", build),
        (
            "dep/BUILD.bazel",
            "filegroup(name = 'old', deprecation = 'use new', visibility = ['//visibility:public'])\n",
        ),
        ("a.txt", "a\n"),
    ]);
    let second = repo.commit(&[("a.txt", "a2\n")]);
    for extra in [&[][..], &["--scope=full"][..]] {
        let (labels, stderr) = repo.run(&first, &second, extra);
        assert_eq!(labels, ["//:n", "//:uses_old"], "{stderr}");
        assert!(stderr.contains("bazel-rs affected: planning "), "{stderr}");
        assert_eq!(stderr.matches(" (exit 0)").count(), 2, "{stderr}");
        for noise in ["DEBUG: ", "noisy impl", "WARNING: ", "deprecated", "INFO: "] {
            assert!(!stderr.contains(noise), "{noise:?} in {stderr}");
        }
    }
    // The same analysis outside `affected` prints them.
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "240s"])
        .arg(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            repo.base().join("cache").display()
        ))
        .args(["build", EXPLICIT_MODULES, "--nobuild", "//:all"])
        .current_dir(repo.main())
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", repo.home())
        .env("TMPDIR", repo.home())
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("noisy impl n"), "{stderr}");
    assert!(stderr.contains("depends on deprecated target"), "{stderr}");
    // A head whose analysis fails: the error and its message are shown.
    let broken = repo.commit(&[(
        "BUILD.bazel",
        &build.replace("src = 'a.txt')", "src = 'a.txt', boom = True)"),
    )]);
    let args = [format!("--base={second}"), format!("--head={broken}")];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = repo.affected(&args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
    assert!(stderr.contains("loud failure of n"), "{stderr}");
    assert!(stderr.contains("failed (exit 1"), "{stderr}");
    assert!(!stderr.contains("noisy impl"), "{stderr}");
}

impl Repo {
    /// `bazel-rs ARGS` in the working copy, with the snapshot diagnostics.
    fn bazel(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "240s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.base().join("cache").display()
            ))
            .args(with_explicit_modules(args))
            .current_dir(self.main())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.home())
            .env("LC_ALL", "C")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }
}

/// ~/code's CI after `affected` (docs/AFFECTED_TARGETS.md §8.8): a clean
/// working copy at HEAD is the head revision, planned in place, and its
/// scoped analysis is saved, so the `build` of the affected targets and
/// the `test` of their tests derive their plans from it
/// (crate::snapshot::derive) instead of analyzing them again — and build
/// and test what a full analysis does.
#[test]
fn the_build_and_test_after_affected_derive_their_plans() {
    let repo = Repo::new();
    let tests = "load(':defs.bzl', 'my_test')\nmy_test(name = 't', srcs = ['//a'])\n";
    let mut files: Vec<(&str, &str)> = PACKAGES.to_vec();
    files.push(("t/defs.bzl", DEFS));
    files.push(("t/BUILD.bazel", tests));
    let first = repo.commit(&files);
    repo.commit(&[("a/a.txt", "a2\n")]);
    // Without --head: the clean working copy is HEAD.
    let affected = |base: &str| {
        let output = repo.affected(&[&format!("--base={base}")]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        (labels, stderr)
    };
    let (labels, stderr) = affected(&first);
    assert_eq!(labels, ["//a", "//b", "//t"], "{stderr}");
    assert!(
        stderr.contains(&format!(
            "planning //... (scoped) in {}\n",
            repo.main().display()
        )),
        "the head is planned in place:\n{stderr}"
    );
    let mut build = vec!["build", "--keep_going"];
    build.extend(labels.iter().map(String::as_str));
    let output = repo.bazel(&build);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("bazel-rs-snapshot: derived "), "{stderr}");
    assert!(
        stderr.contains("INFO: Analyzed 3 targets (") && stderr.contains("0 targets configured)."),
        "{stderr}"
    );
    let output = repo.bazel(&["test", "--test_output=errors", "//t"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("bazel-rs-snapshot: derived "), "{stderr}");
    // A head with a change in the working copy is not HEAD: planned in
    // place as before, never saved for derivation from a scope.
    fs::write(repo.main().join("c/c.txt"), "dirty\n").unwrap();
    let (_, stderr) = affected(&first);
    assert!(stderr.contains("the working copy has changes"), "{stderr}");
}

/// A local change plans both revisions at once when the host has the
/// memory for it (`BAZEL_RS_AFFECTED_CONCURRENT_MB`, 20 GB by default), the
/// head still in place (its snapshot saved for the commands after
/// `affected`); otherwise, or with `--noconcurrent_revisions`, one after
/// the other. A global change stays sequential unless asked. The answer is
/// the same every way.
#[test]
fn a_local_change_plans_both_revisions_at_once_when_memory_allows() {
    let repo = Repo::new();
    let first = repo.commit(PACKAGES);
    repo.commit(&[("a/a.txt", "a2\n")]);
    let run = |extra: &[&str], needed: &str| {
        let mut args = vec![
            format!("--base={first}"),
            "--affected_command=build".to_owned(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = repo.affected_with(&args, &[("BAZEL_RS_AFFECTED_CONCURRENT_MB", needed)]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(labels, ["//a", "//b"], "{stderr}");
        assert!(
            stderr.contains(&format!(
                "planning //... (scoped) in {}\n",
                repo.main().display()
            )),
            "the head is planned in place:\n{stderr}"
        );
        stderr
    };
    let stderr = run(&[], "0");
    assert!(
        stderr.contains("planning both revisions concurrently"),
        "{stderr}"
    );
    let stderr = run(&[], "999999999");
    assert!(!stderr.contains("concurrently"), "{stderr}");
    assert!(stderr.contains("one after the other"), "{stderr}");
    let stderr = run(&["--noconcurrent_revisions"], "0");
    assert!(!stderr.contains("concurrently"), "{stderr}");
    let stderr = run(&["--concurrent_revisions"], "999999999");
    assert!(
        stderr.contains("planning both revisions concurrently"),
        "{stderr}"
    );
    // A global change: sequential by default.
    let third = repo.commit(&[("MODULE.bazel", "module(name = 'main')\n# x\n")]);
    let args = [
        format!("--base={third}~1"),
        "--affected_command=build".to_owned(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = repo.affected_with(&args, &[("BAZEL_RS_AFFECTED_CONCURRENT_MB", "0")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("global change: MODULE.bazel"), "{stderr}");
    assert!(!stderr.contains("concurrently"), "{stderr}");
}

/// An action reading a source directory (rules_js's `sharp@…/lc` on
/// ~/code): `affected`'s saved head planning (`test --nobuild`) traverses
/// it as an executing build would, so the CI's executing `build` of the
/// affected targets still derives its plan instead of analyzing again
/// (docs/ANALYSIS_SNAPSHOT.md "Derived hits"); and builds what a full
/// analysis builds, the directory's contents included.
#[test]
fn an_executing_build_derives_source_directory_actions_from_affected() {
    let repo = Repo::new();
    let mut files: Vec<(&str, &str)> = PACKAGES.to_vec();
    files.push((
        "s/BUILD.bazel",
        "genrule(name = 's', srcs = ['d', '//a'], outs = ['s.out'], cmd = 'ls $(location d) > $@; cat $(location //a) >> $@')\n",
    ));
    files.push(("s/d/one.txt", "1\n"));
    let first = repo.commit(&files);
    repo.commit(&[("a/a.txt", "a2\n")]);
    let output = repo.affected(&[&format!("--base={first}"), "--affected_command=build"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(labels, ["//a", "//b", "//s"], "{stderr}");
    let mut build = vec!["build", "--keep_going"];
    build.extend(labels.iter().map(String::as_str));
    let output = repo.bazel(&build);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("bazel-rs-snapshot: derived "), "{stderr}");
    // The output, materialized in the cache's action outputs.
    let mut stack = vec![repo.base().join("cache/actions")];
    let mut out = None;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "s.out") {
                out = Some(fs::read_to_string(&path).unwrap());
            }
        }
    }
    assert_eq!(out.as_deref(), Some("one.txt\na2\n"));
    // A directory an executing build fails to traverse (a package below
    // it): the --nobuild planning still succeeds, the build does not
    // derive from it and fails like a full analysis.
    let second = repo.git(&["rev-parse", "HEAD"]);
    repo.commit(&[("s/d/sub/BUILD.bazel", ""), ("s/d/two.txt", "2\n")]);
    let output = repo.affected(&[&format!("--base={second}"), "--affected_command=build"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//s\n", "{stderr}");
    let output = repo.bazel(&["build", "//s"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(!stderr.contains("bazel-rs-snapshot: derived "), "{stderr}");
    assert!(stderr.contains("error traversing directory"), "{stderr}");
}

/// affected-reuse-cold: the head planning of `affected` on a cold cache, or
/// after an input of a module extension changed, evaluates the extension
/// (which prints, as rules_python's `pip` does) and is still saved for the
/// commands after it — the `build` of the affected targets derives its plan
/// (derived hits never replay what planning printed).
#[test]
fn the_build_after_affected_derives_when_the_head_evaluated_an_extension() {
    let repo = Repo::new();
    let mut files: Vec<(&str, &str)> = PACKAGES
        .iter()
        .copied()
        .filter(|(path, _)| *path != "MODULE.bazel")
        .collect();
    files.push((
        "MODULE.bazel",
        "module(name = 'main')\next = use_extension('//e:ext.bzl', 'ext')\nuse_repo(ext, 'gen')\n",
    ));
    files.push(("e/BUILD.bazel", ""));
    files.push((
        "e/ext.bzl",
        r#"def _repo(ctx):
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", ctx.attr.value)

_repo_rule = repository_rule(implementation = _repo, attrs = {"value": attr.string()})

def _ext(module_ctx):
    print("the extension was evaluated")
    _repo_rule(name = "gen", value = module_ctx.read(Label("//e:value.txt")))
    return module_ctx.extension_metadata(reproducible = True)

ext = module_extension(implementation = _ext)
"#,
    ));
    files.push(("e/value.txt", "one\n"));
    files.push((
        "g/BUILD.bazel",
        "genrule(name = 'g', srcs = ['@gen//:v.txt', '//a'], outs = ['g.out'], cmd = 'cat $(SRCS) > $@')\n",
    ));
    let first = repo.commit(&files);
    let second = repo.commit(&[("a/a.txt", "a2\n")]);
    // A cold cache, then a lukewarm one: the working copy's hidden lockfile
    // (the extension's entry) gone, the head evaluates the extension again.
    for (base, lukewarm) in [(first.as_str(), false), (second.as_str(), true)] {
        if lukewarm {
            repo.commit(&[("a/a.txt", "a3\n")]);
            for entry in fs::read_dir(repo.base().join("cache/worktrees")).unwrap() {
                let lockfile = entry.unwrap().path().join("MODULE.bazel.lock");
                if lockfile.exists() {
                    fs::remove_file(lockfile).unwrap();
                }
            }
        }
        let output = repo.affected_with(
            &[&format!("--base={base}")],
            &[("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")],
        );
        let affected = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{affected}");
        let labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        assert!(labels.contains(&"//g".to_owned()), "{labels:?}\n{affected}");
        // The head evaluated the extension (its print is no output of
        // `affected`'s) and was saved for the commands after it.
        assert!(
            !affected.contains("not saved") && affected.contains("bazel-rs-snapshot: saved "),
            "{affected}"
        );
        let mut build = vec!["build", "--keep_going"];
        build.extend(labels.iter().map(String::as_str));
        let output = repo.bazel(&build);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("bazel-rs-snapshot: derived "),
            "lukewarm {lukewarm}: {stderr}\n--- affected:\n{affected}"
        );
        assert!(
            !stderr.contains("the extension was evaluated"),
            "the derived build evaluates nothing:\n{stderr}"
        );
    }
}

/// A minimal HTTP/1.1 cache for the remote base digests: GET/PUT of
/// `/ac/H` and `/cas/H`, entries inspectable and replaceable.
mod http_cache {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    pub struct Server {
        port: u16,
        pub entries: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    }

    fn handle(mut stream: TcpStream, entries: Arc<Mutex<BTreeMap<String, Vec<u8>>>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let (status, payload) = if method == "PUT" {
                entries.lock().unwrap().insert(path, body);
                ("200 OK", Vec::new())
            } else {
                match entries.lock().unwrap().get(&path) {
                    Some(bytes) => ("200 OK", bytes.clone()),
                    None => ("404 Not Found", Vec::new()),
                }
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            );
            if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&payload).is_err() {
                return;
            }
        }
    }

    impl Server {
        pub fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let entries = Arc::new(Mutex::new(BTreeMap::new()));
            let accept = entries.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let entries = accept.clone();
                    std::thread::spawn(move || handle(stream, entries));
                }
            });
            Self { port, entries }
        }

        pub fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        /// The Action Cache paths (`/ac/H`).
        pub fn ac(&self) -> Vec<String> {
            self.entries
                .lock()
                .unwrap()
                .keys()
                .filter(|path| path.starts_with("/ac/"))
                .cloned()
                .collect()
        }
    }
}

/// `count` of the summary line `base digests: N of M from the remote
/// cache` (`None`: no such line).
fn remote_hits(stderr: &str) -> Option<(usize, usize)> {
    let line = stderr
        .lines()
        .find(|line| line.contains("from the remote cache"))?;
    let rest = line.split("base digests: ").nth(1)?;
    let mut words = rest.split_whitespace();
    let hits = words.next()?.parse().ok()?;
    words.next();
    let total = words.next()?.parse().ok()?;
    Some((hits, total))
}

fn plannings(stderr: &str) -> usize {
    stderr.matches("bazel-rs affected: planning //...").count()
}

/// docs/AFFECTED_TARGETS.md §8.11: the base's digests go through the
/// configured remote cache: a miss plans the base and writes them, a hit
/// skips the base, a partial hit plans only the missing targets; the answer
/// is always the one without the remote cache.
#[test]
fn base_digests_are_shared_through_an_http_remote_cache() {
    let repo = Repo::new();
    let server = http_cache::Server::new();
    let remote = format!("--remote_cache={}", server.url());
    let build = "--affected_command=build";
    let first = repo.commit(PACKAGES);
    let second = repo.commit(&[("a/a.txt", "a2\n")]);
    let reference = |base: &str, head: &str, extra: &[&str]| {
        let mut args = vec![build, "--noaffected_remote_digests", remote.as_str()];
        args.extend(extra);
        let (labels, stderr) = repo.run(base, head, &args);
        assert_eq!(remote_hits(&stderr), None, "{stderr}");
        labels
    };
    let want = reference(&first, &second, &[]);
    assert_eq!(want, ["//a", "//b"]);
    // A miss: the base is planned and its digests are written.
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    assert!(
        stderr.contains("wrote 2 of 2 to the remote cache"),
        "{stderr}"
    );
    assert_eq!(plannings(&stderr), 2, "{stderr}");
    assert_eq!(server.ac().len(), 2);
    // All hit: the base is not planned (nor checked out).
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((2, 2)), "{stderr}");
    assert_eq!(plannings(&stderr), 1, "{stderr}");
    assert!(!stderr.contains("wrote"), "{stderr}");
    // The same tree through another commit: the same entries.
    let other = repo.git(&["commit-tree", &format!("{first}^{{tree}}"), "-m", "other"]);
    let (labels, stderr) = repo.run(&other, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((2, 2)), "{stderr}");
    // Another head on the same base, its scope overlapping: only the
    // missing target is planned at the base (sequentially too).
    let third = repo.commit(&[("c/c.txt", "c2\n")]);
    let want3 = reference(&first, &third, &[]);
    assert_eq!(want3, ["//a", "//b", "//c"]);
    let (labels, stderr) = repo.run(
        &first,
        &third,
        &[build, &remote, "--noconcurrent_revisions"],
    );
    assert_eq!(labels, want3);
    assert_eq!(remote_hits(&stderr), Some((2, 3)), "{stderr}");
    assert!(
        stderr.contains("scope 1 of 4 selected target(s) (the targets asked for)"),
        "{stderr}"
    );
    assert!(stderr.contains("wrote 1 of 1"), "{stderr}");
    // A target new at the head: written as absent at the base, then a hit.
    let fourth = repo.commit(&[(
        "c/BUILD.bazel",
        "genrule(name = 'c', srcs = ['c.txt'], outs = ['c.out'], cmd = 'cat $< > $@')\ngenrule(name = 'd', srcs = ['c.txt'], outs = ['d.out'], cmd = 'cat $< > $@')\n",
    )]);
    let want4 = reference(&first, &fourth, &[]);
    assert!(want4.contains(&"//c:d".to_owned()), "{want4:?}");
    let (labels, stderr) = repo.run(&first, &fourth, &[build, &remote]);
    assert_eq!(labels, want4);
    assert_eq!(remote_hits(&stderr), Some((3, 4)), "{stderr}");
    let (labels, stderr) = repo.run(&first, &fourth, &[build, &remote]);
    assert_eq!(labels, want4);
    assert_eq!(remote_hits(&stderr), Some((4, 4)), "{stderr}");
    assert_eq!(plannings(&stderr), 1, "{stderr}");
    // Other options: another key, nothing shared.
    let define = "--define=k=v";
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote, define]);
    assert_eq!(labels, reference(&first, &second, &[define]));
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    // Read-only (`--noremote_upload_local_results`): looked up, not
    // written; the remote transport options are no part of the key.
    let (_, stderr) = repo.run(
        &first,
        &second,
        &[build, &remote, "--noremote_upload_local_results"],
    );
    assert_eq!(remote_hits(&stderr), Some((2, 2)), "{stderr}");
    let read_only = [
        build,
        &remote,
        "--noremote_upload_local_results",
        "--define=k=w",
    ];
    let (_, stderr) = repo.run(&first, &second, &read_only);
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    assert!(!stderr.contains("wrote"), "{stderr}");
    let (_, stderr) = repo.run(&first, &second, &read_only);
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    // `--noremote_accept_cached`: planned as without, written through.
    let (labels, stderr) = repo.run(
        &first,
        &second,
        &[build, &remote, "--noremote_accept_cached", "--define=k=x"],
    );
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), None, "{stderr}");
    assert!(stderr.contains("wrote 2 of 2"), "{stderr}");
    let (_, stderr) = repo.run(&first, &second, &[build, &remote, "--define=k=x"]);
    assert_eq!(remote_hits(&stderr), Some((2, 2)), "{stderr}");
    // Corrupt entries are misses: analysed (and written again).
    let entries: Vec<String> = server.ac();
    let mut planted = 0;
    {
        let mut map = server.entries.lock().unwrap();
        let originals: Vec<Vec<u8>> = entries.iter().map(|path| map[path].clone()).collect();
        for (at, path) in entries.iter().enumerate() {
            // Every entry: another label's record, or garbage.
            let replacement = if at % 2 == 0 {
                originals[(at + 1) % originals.len()].clone()
            } else {
                b"garbage".to_vec()
            };
            if replacement != originals[at] {
                map.insert(path.clone(), replacement);
                planted += 1;
            }
        }
    }
    assert!(planted > 0);
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    assert!(
        stderr.contains("malformed remote base digest(s) ignored"),
        "{stderr}"
    );
    let (_, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(remote_hits(&stderr), Some((2, 2)), "{stderr}");
    // The cache down: everything is analysed, the same answer.
    let down = "--remote_cache=http://127.0.0.1:1";
    let (labels, stderr) = repo.run(&first, &second, &[build, down]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((0, 2)), "{stderr}");
    assert!(stderr.contains("remote base digests"), "{stderr}");
    // No remote cache: as before.
    let (labels, stderr) = repo.run(&first, &second, &[build]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), None, "{stderr}");
}

include!("support/grpc_cache_server.rs");

/// The same through a gRPC cache, for a global change (both revisions
/// entirely: the head's every target is looked up once it is planned).
#[test]
fn base_digests_are_shared_through_a_grpc_remote_cache() {
    let repo = Repo::new();
    let server = grpc_cache_server::Server::new();
    let remote = format!("--remote_cache={}", server.url());
    let build = "--affected_command=build";
    let first = repo.commit(PACKAGES);
    let second = repo.commit(&[
        ("MODULE.bazel", "module(name = 'main')\n# x\n"),
        ("a/a.txt", "a2\n"),
    ]);
    let (want, stderr) = repo.run(&first, &second, &[build, "--noaffected_remote_digests"]);
    assert!(stderr.contains("global change: MODULE.bazel"), "{stderr}");
    assert_eq!(want, ["//a", "//b"]);
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((0, 4)), "{stderr}");
    assert!(stderr.contains("wrote 4 of 4"), "{stderr}");
    assert_eq!(server.ac_keys("").len(), 4);
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((4, 4)), "{stderr}");
    assert_eq!(plannings(&stderr), 1, "{stderr}");
    // A server that does not keep inlined contents: the records are read
    // from the CAS.
    for key in server.ac_keys("") {
        let mut result =
            bazel_rs::reapi::ActionResult::decode(&server.ac_entry("", &key).unwrap()).unwrap();
        let file = &mut result.output_files[0];
        assert!(server.cas_blob("", &file.digest.hash).is_some());
        file.contents.clear();
        server.put_ac("", &key, result.encode());
    }
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((4, 4)), "{stderr}");
    // ... and one evicted from it is a miss.
    let key = server.ac_keys("").pop().unwrap();
    let result =
        bazel_rs::reapi::ActionResult::decode(&server.ac_entry("", &key).unwrap()).unwrap();
    assert!(server.evict("", &result.output_files[0].digest.hash));
    let (labels, stderr) = repo.run(&first, &second, &[build, &remote]);
    assert_eq!(labels, want);
    assert_eq!(remote_hits(&stderr), Some((3, 4)), "{stderr}");
}
