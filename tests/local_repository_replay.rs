//! L2-W4R (PLAN-L2 update 1): local repository rules are recorded for
//! replay instead of tainting the analysis snapshot, their output is
//! collected per execution without changing what stderr shows, and links
//! into published store trees resolve the same in every loading order.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("local-replay-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        )
    }

    fn file(&self, name: &str, contents: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new("timeout");
        command
            .args([
                "120s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_SNAPSHOT")
            .env_remove("BAZEL_RS_SNAPSHOT_DUMP")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .current_dir(self.0.path());
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Lines of `text` that are neither diagnostics nor the build summary.
fn program_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| {
            !line.starts_with("INFO:")
                && !line.starts_with("bazel-rs")
                && !line.starts_with("PERF")
                && !line.is_empty()
        })
        .collect()
}

/// A local rule's print(), `execute(quiet=False)` echo and reproducibility
/// DEBUG lines keep their bytes and program order, before the loading
/// output that follows the repository's demand — on every run (a local
/// rule executes every time), at any thread count.
#[test]
fn local_rule_output_keeps_its_bytes_and_position() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr = use_repo_rule('//:repo.bzl', 'r')\nr(name = 'printing')\n",
    );
    f.file(
        "repo.bzl",
        r#"def _impl(ctx):
    print("rule print one")
    ctx.execute(["/bin/sh", "-c", "echo echoed stdout; echo echoed stderr >&2"], quiet = False)
    print("rule print two")
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("defs.bzl", "print('defs top-level')\nX = 1\n")
    ctx.file("value", "v")
    return {"value": "changed"}

r = repository_rule(_impl, local = True, attrs = {"value": attr.string(default = "original")})
"#,
    );
    f.file("main.bzl", "print('main bzl')\nY = 2\n");
    f.file(
        "BUILD.bazel",
        "load('@printing//:defs.bzl', 'X')\nload(':main.bzl', 'Y')\ngenrule(name = 'copy', srcs = ['@printing//:value'], outs = ['copy.txt'], cmd = 'cp $< $@')\n",
    );
    let mut reference: Option<Vec<String>> = None;
    for threads in ["1", "8", "1"] {
        let output = f.run_env(
            &[],
            &[
                "build",
                "--nobuild",
                &format!("--loading_phase_threads={threads}"),
                "//:copy",
            ],
        );
        let text = stderr(&output);
        assert!(output.status.success(), "{text}");
        let lines: Vec<String> = program_lines(&text)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let position = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from\n{text}"))
        };
        let order = [
            position("rule print one"),
            position("echoed stdout"),
            position("echoed stderr"),
            position("rule print two"),
            position("indicated that a canonical reproducible form"),
            position("defined by rule r in"),
            position("defs top-level"),
            position("main bzl"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert_eq!(lines[order[0]], "rule print one");
        assert_eq!(lines[order[1]], "echoed stdout");
        assert_eq!(lines[order[2]], "echoed stderr");
        match &reference {
            Some(reference) => assert_eq!(&lines, reference, "{text}"),
            None => reference = Some(lines),
        }
    }
}

/// PLAN-L2 update 1 §(c): a repository whose BUILD loads a `.bzl` through a
/// symlink into another repository's published store tree resolves it the
/// same way whatever was materialized before — here the warm runs never
/// demand the target repository before the crossing — under worker jitter
/// and 1, 8 or 32 loading threads.
#[test]
fn store_tree_links_resolve_identically_in_every_loading_order() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr = use_repo_rule('//:repo.bzl', 'r')\nd = use_repo_rule('//:repo.bzl', 'd')\nd(name = 'dep')\nr(name = 'gen')\n",
    );
    f.file(
        "repo.bzl",
        r#"def _dep(ctx):
    ctx.file("BUILD.bazel", "genrule(name = 'd', outs = ['d.txt'], cmd = 'echo d > $@', visibility = ['//visibility:public'])\n")
    ctx.file("defs.bzl", "VALUE = 'from-dep'\n")

d = repository_rule(_dep)

def _gen(ctx):
    ctx.symlink(Label("@dep//:defs.bzl"), "linked.bzl")
    ctx.file("BUILD.bazel", "load(':linked.bzl', 'VALUE')\ngenrule(name = 'g', outs = ['g.txt'], cmd = 'echo %s > $@' % VALUE, visibility = ['//visibility:public'])\n")

r = repository_rule(_gen)
"#,
    );
    f.file("BUILD.bazel", "");
    for package in 0..6 {
        f.file(
            &format!("p{package}/BUILD.bazel"),
            &format!(
                "genrule(name = 'x', srcs = ['@gen//:g', '@dep//:d'], outs = ['x{package}.txt'], cmd = 'cat $(SRCS) > $@')\n"
            ),
        );
    }
    // Cold: both rules execute (gen demands dep through its Label).
    let cold = f.run_env(&[], &["build", "--nobuild", "//..."]);
    assert!(cold.status.success(), "{}", stderr(&cold));
    let reference = f.run_env(
        &[],
        &[
            "build",
            "--nobuild",
            "--loading_phase_threads=1",
            "@gen//:g",
        ],
    );
    assert!(reference.status.success(), "{}", stderr(&reference));
    let broad = f.run_env(
        &[],
        &["build", "--nobuild", "--loading_phase_threads=1", "//..."],
    );
    assert!(broad.status.success(), "{}", stderr(&broad));
    for (threads, jitter) in [("8", ""), ("32", ""), ("8", "7"), ("32", "40"), ("1", "99")] {
        let env: Vec<(&str, &str)> = if jitter.is_empty() {
            Vec::new()
        } else {
            vec![("BAZEL_RS_LOADING_JITTER", jitter)]
        };
        for (args, expected) in [
            (vec!["@gen//:g"], &reference),
            (vec!["//..."], &broad),
            (vec!["@dep//:d", "@gen//:g"], &reference),
        ] {
            let mut argv = vec![
                "build".to_owned(),
                "--nobuild".to_owned(),
                format!("--loading_phase_threads={threads}"),
            ];
            argv.extend(args.iter().map(|arg| (*arg).to_owned()));
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let run = f.run_env(&env, &argv);
            assert!(
                run.status.success(),
                "threads={threads} jitter={jitter} {args:?}: {}",
                stderr(&run)
            );
            if args != ["@dep//:d", "@gen//:g"] {
                assert_eq!(
                    run.stderr, expected.stderr,
                    "threads={threads} jitter={jitter} {args:?}"
                );
                assert_eq!(run.stdout, expected.stdout);
            }
        }
    }
}

fn snapshot_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("bazel-rs-snapshot: "))
        .collect()
}

/// `BAZEL_RS_SNAPSHOT=record`: local executions are replay records (no
/// `repository-rule-executed` taint), the dry run replays them all and
/// matches, a warm run is eligible, and a printing local rule taints
/// (`local-repository-output`, v1 of the output protocol).
#[test]
fn record_mode_replays_local_rules_and_is_eligible_without_output() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr = use_repo_rule('//:repo.bzl', 'r')\nr(name = 'one')\nr(name = 'two', value = 'second')\n",
    );
    f.file(
        "repo.bzl",
        r#"def _impl(ctx):
    if ctx.attr.loud:
        print("loud")
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("value", ctx.attr.value + ctx.os.name)

r = repository_rule(_impl, local = True, attrs = {"value": attr.string(default = "first"), "loud": attr.bool()})
"#,
    );
    f.file(
        "BUILD.bazel",
        "genrule(name = 'copy', srcs = ['@one//:value', '@two//:value'], outs = ['copy.txt'], cmd = 'cat $(SRCS) > $@')\n",
    );
    let dump = f.0.path().join("dump.txt");
    let dump_text = dump.display().to_string();
    let record = [
        ("BAZEL_RS_SNAPSHOT", "record"),
        ("BAZEL_RS_SNAPSHOT_DUMP", dump_text.as_str()),
    ];
    let off = f.run_env(&[], &["build", "--nobuild", "//:copy"]);
    assert!(off.status.success(), "{}", stderr(&off));
    for _ in 0..2 {
        let on = f.run_env(&record, &["build", "--nobuild", "//:copy"]);
        let text = stderr(&on);
        assert!(on.status.success(), "{text}");
        // Byte-identical apart from the diagnostics.
        assert_eq!(on.stdout, off.stdout);
        let rest: Vec<&str> = text
            .lines()
            .filter(|line| !line.starts_with("bazel-rs-snapshot:"))
            .collect();
        assert_eq!(rest, stderr(&off).lines().collect::<Vec<_>>());
        let lines = snapshot_lines(&text);
        assert!(
            lines.iter().any(|line| line.contains("eligible=yes")),
            "{text}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("replay dry-run: 2/2 matching")),
            "{text}"
        );
        assert!(
            !text.contains("repository-rule-executed"),
            "local rules are no taint: {text}"
        );
        let dump = fs::read_to_string(&dump).unwrap();
        assert_eq!(
            dump.lines()
                .filter(|line| line.starts_with("REPLAY ") && line.contains(" match "))
                .count(),
            2,
            "{dump}"
        );
        assert!(dump.lines().any(|line| line.starts_with("MAPPING ")));
    }
    // A printing local rule taints the run (and still prints, once).
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr = use_repo_rule('//:repo.bzl', 'r')\nr(name = 'one', loud = True)\nr(name = 'two', value = 'second')\n",
    );
    let loud = f.run_env(&record, &["build", "--nobuild", "//:copy"]);
    let text = stderr(&loud);
    assert!(loud.status.success(), "{text}");
    assert_eq!(
        text.lines().filter(|line| *line == "loud").count(),
        1,
        "{text}"
    );
    let lines = snapshot_lines(&text);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("taint local-repository-output")),
        "{text}"
    );
    assert!(
        lines.iter().any(|line| line.contains("eligible=no")),
        "{text}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("replay dry-run: 2/2 matching")),
        "{text}"
    );
    // The replay printed nothing: "loud" appears once.
    assert!(
        !Path::new(&f.0.path().join("cache/repositories/direct/stages"))
            .read_dir()
            .unwrap()
            .any(|_| true)
    );
}
