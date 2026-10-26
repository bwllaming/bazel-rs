//! L2-W4R (PLAN-L2 update 1): local repository rules are recorded for
//! replay instead of tainting the analysis snapshot, their output is
//! collected per execution without changing what stderr shows, and links
//! into published store trees resolve the same in every loading order.
//! Per-tree validation still checks what a verified tree's links reach, the
//! record mode names store trees a link reaches that no repository of the
//! run has as its root, and it records the same inputs in every loading
//! order.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

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
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_SNAPSHOT")
            .env_remove("BAZEL_RS_SNAPSHOT_DUMP")
            .current_dir(self.0.path());
        // Test hooks taint a record-mode run (never saved); a test sets the
        // ones it exercises (the gate may export BAZEL_RS_VERIFY_*).
        for hook in bazel_rs::snapshot::record::TEST_HOOKS {
            command.env_remove(hook);
        }
        for (name, value) in env {
            command.env(name, value);
        }
        command.explicit_modules().output().unwrap()
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

/// A local rule that demands another repository while it runs: the other
/// rule's output appears where the demand happened, between the demanding
/// rule's earlier and later output — printing as it happens, as before the
/// output was collected per execution (and as Bazel's suspended fetch
/// does) — with recording off and on.
#[test]
fn nested_local_rule_output_appears_where_the_demand_happened() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='main')\nouter = use_repo_rule('//:repo.bzl', 'outer')\ninner = use_repo_rule('//:repo.bzl', 'inner')\ninner(name = 'innerrepo')\nouter(name = 'outerrepo')\n",
    );
    f.file(
        "repo.bzl",
        r#"def _inner(ctx):
    print("inner print")
    ctx.execute(["/bin/sh", "-c", "echo inner echo"], quiet = False)
    ctx.file("BUILD.bazel", "exports_files(['v'])\n")
    ctx.file("v", "v")

inner = repository_rule(_inner, local = True)

def _outer(ctx):
    print("outer before")
    ctx.execute(["/bin/sh", "-c", "echo outer echo"], quiet = False)
    ctx.read(Label("@innerrepo//:v"))
    print("outer after")
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("value", "v")

outer = repository_rule(_outer, local = True)
"#,
    );
    f.file(
        "BUILD.bazel",
        "genrule(name = 'copy', srcs = ['@outerrepo//:value'], outs = ['copy.txt'], cmd = 'cp $< $@')\n",
    );
    let expected = [
        "outer before",
        "outer echo",
        "inner print",
        "inner echo",
        "outer after",
    ];
    let dump = f.0.path().join("dump.txt");
    let dump_text = dump.display().to_string();
    let record = [
        ("BAZEL_RS_SNAPSHOT", "record"),
        ("BAZEL_RS_SNAPSHOT_DUMP", dump_text.as_str()),
    ];
    for env in [&[][..], &record[..], &[][..]] {
        let output = f.run_env(env, &["build", "--nobuild", "//:copy"]);
        let text = stderr(&output);
        assert!(output.status.success(), "{text}");
        let printed: Vec<&str> = program_lines(&text)
            .into_iter()
            .filter(|line| line.starts_with("outer ") || line.starts_with("inner "))
            .collect();
        assert_eq!(printed, expected, "{text}");
        if !env.is_empty() {
            // Both rules printed: v1 taints; both replays match and print
            // nothing (each line appears once).
            let lines = snapshot_lines(&text);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("taint local-repository-output")),
                "{text}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("replay dry-run: 2/2 matching")),
                "{text}"
            );
            let dump = fs::read_to_string(&dump).unwrap();
            let outputs: Vec<&str> = dump
                .lines()
                .filter(|line| line.starts_with("REPLAY "))
                .filter_map(|line| line.split(' ').find(|part| part.starts_with("output:")))
                .collect();
            // "outer before\nouter echo\nouter after\n" and
            // "inner print\ninner echo\n", each print() a marked note
            // (crate::ui: 17 + 16 bytes of marks around it).
            assert_eq!(outputs.len(), 2, "{dump}");
            assert!(outputs.contains(&"output:102"), "{dump}");
            assert!(outputs.contains(&"output:56"), "{dump}");
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
    let combinations = ["1", "8", "32"]
        .into_iter()
        .flat_map(|threads| ["", "7", "40", "99"].map(|jitter| (threads, jitter)));
    for (threads, jitter) in combinations {
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
    // The replay printed nothing: "loud" appears once. The replays left
    // the repositories' directories linking their trees, no scratch.
    for entry in Path::new(&f.0.path().join("cache/repositories/external"))
        .read_dir()
        .unwrap()
    {
        let entry = entry.unwrap();
        assert!(!entry.file_name().to_string_lossy().starts_with('.'));
        assert!(
            fs::read_link(entry.path())
                .unwrap()
                .starts_with("../direct/trees")
        );
    }
}

/// The mismatch count of the record mode's `validation dry-run [POLICY]`
/// line.
fn validation_mismatches(lines: &[&str], policy: &str) -> usize {
    let prefix = format!("validation dry-run [{policy}]: ");
    let line = lines
        .iter()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {lines:#?}"));
    let (head, _) = line
        .rsplit_once(" mismatch(es)")
        .unwrap_or_else(|| panic!("{line}"));
    head.rsplit(' ').next().unwrap().parse().unwrap()
}

/// Per-tree validation (the default policy) still validates what a verified
/// tree's symlinks read (review item 1). A non-local generated repository
/// links a host directory (`ctx.symlink("/host/x", "inc")`) and globs
/// through the link; the glob walker records the host listing (READDIR,
/// STAT, CANON, the BUILD lookups) under the logical path inside the
/// verified tree. A local repository with an internal link besides its host
/// link (whose tree a link crossing used to record as a REPO) does the
/// same. A header added to each host directory after the window — by an
/// action of the same build, before the record mode validates — must be a
/// mismatch, i.e. a miss, never a stale hit; with nothing added, the same
/// recording validates cleanly.
#[test]
fn per_tree_validation_sees_host_files_added_behind_tree_symlinks() {
    let f = Fixture::new();
    let host = tempfile::Builder::new()
        .prefix("host-dirs-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let host = fs::canonicalize(host.path()).unwrap();
    for dir in ["x/sub", "y/sub"] {
        fs::create_dir_all(host.join(dir)).unwrap();
    }
    for file in ["x/a.h", "x/sub/b.h", "y/c.h", "y/sub/d.h"] {
        fs::write(host.join(file), file).unwrap();
    }
    let (x, y) = (host.join("x"), host.join("y"));
    f.file(
        "MODULE.bazel",
        &format!(
            "module(name='main')\ng = use_repo_rule('//:repo.bzl', 'g')\nl = use_repo_rule('//:repo.bzl', 'l')\ng(name = 'gen', host = {:?})\nl(name = 'lgen', host = {:?})\n",
            x.display().to_string(),
            y.display().to_string()
        ),
    );
    f.file(
        "repo.bzl",
        r#"def _gen(ctx):
    ctx.symlink(ctx.attr.host, "inc")
    ctx.file("BUILD.bazel", "filegroup(name = 'hdrs', srcs = glob(['inc/**']), visibility = ['//visibility:public'])\n")

g = repository_rule(_gen, attrs = {"host": attr.string()})

def _local(ctx):
    ctx.file("data/v.txt", "v")
    ctx.symlink("data", "alias")
    ctx.symlink(ctx.attr.host, "inc")
    ctx.file("BUILD.bazel", "filegroup(name = 'hdrs', srcs = glob(['alias/**', 'inc/**']), visibility = ['//visibility:public'])\n")

l = repository_rule(_local, local = True, attrs = {"host": attr.string()})
"#,
    );
    f.file(
        "BUILD.bazel",
        &format!(
            "genrule(name = 'list', srcs = ['@gen//:hdrs', '@lgen//:hdrs'], outs = ['list.txt'], cmd = 'ls $(SRCS) > $@')\ngenrule(name = 'add', outs = ['add.txt'], cmd = 'touch {0}/new.h {1}/sub/new.h && echo added > $@')\n",
            x.display(),
            y.display()
        ),
    );
    // Cold: `gen` executes and publishes; the next runs reuse (verify) it.
    let cold = f.run_env(&[], &["build", "--nobuild", "//:list"]);
    assert!(cold.status.success(), "{}", stderr(&cold));
    let record = [
        ("BAZEL_RS_SNAPSHOT", "record"),
        ("BAZEL_RS_SNAPSHOT_COMPARE", "1"),
    ];
    // Planning lists the host directories through the links; `add` then
    // adds a header to each before the record mode validates.
    let added = f.run_env(&record, &["build", "//:list", "//:add"]);
    let text = stderr(&added);
    assert!(added.status.success(), "{text}");
    assert!(x.join("new.h").exists() && y.join("sub/new.h").exists());
    let lines = snapshot_lines(&text);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("replay dry-run: 1/1 matching")),
        "{text}"
    );
    let changed: Vec<&&str> = lines
        .iter()
        .filter(|line| line.starts_with("validation: READDIR") && line.contains("listing changed"))
        .collect();
    for (tree, listed) in [
        ("gen", "/inc: listing changed"),
        ("lgen", "/inc/sub: listing changed"),
    ] {
        assert!(
            changed.iter().any(|line| line.ends_with(listed)),
            "{tree}: {changed:#?}\n{text}"
        );
    }
    assert!(validation_mismatches(&lines, "per-tree") >= 2, "{text}");
    assert!(validation_mismatches(&lines, "full") >= 2, "{text}");
    // Nothing changes during this run (`add` is an action-cache hit): the
    // recording validates cleanly under both policies.
    let unchanged = f.run_env(&record, &["build", "//:list", "//:add"]);
    let text = stderr(&unchanged);
    assert!(unchanged.status.success(), "{text}");
    let lines = snapshot_lines(&text);
    assert_eq!(validation_mismatches(&lines, "per-tree"), 0, "{text}");
    assert_eq!(validation_mismatches(&lines, "full"), 0, "{text}");
}

/// The comparable part of a record-mode dump: every observation, environment
/// digest, repository and integrity entry (without the `racy` flags, which
/// depend on how long ago a file was written), the replay records' names,
/// outcomes and flags, the pinned tables, and every taint but the test
/// hook's.
fn recorded_inputs(dump: &str) -> Vec<String> {
    dump.lines()
        .filter_map(|line| {
            let kind = line.split(' ').next()?;
            match kind {
                "OBS" | "ENV" | "REPO" | "INT" => {
                    Some(line.strip_suffix(" racy").unwrap_or(line).to_owned())
                }
                "ROOT" | "MAPPING" => Some(line.to_owned()),
                "TAINT" if !line.starts_with("TAINT test-hook ") => Some(line.to_owned()),
                "REPLAY" => Some(
                    line.split(' ')
                        .filter(|part| {
                            !part.starts_with("recorded:") && !part.starts_with("replay:")
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            }
        })
        .collect()
}

/// PLAN-L2 §1b determinism (review item 6): the record mode observes the
/// same inputs — every observation, environment digest, verified tree and
/// integrity entry, the replay records and their pinned tables, every taint
/// but the test hook's — at 1, 8 or 32 loading threads under worker jitter,
/// and never an Unstable one. The workspace has globs through subpackage
/// boundaries, a `.bzl` chain, a `.bazelignore`d package, a generated
/// repository globbing a host directory through a link, a local repository
/// with an internal and a host link (replayed), and a repository loading a
/// `.bzl` through a link into another repository's store tree.
#[test]
fn record_mode_observes_the_same_inputs_in_every_loading_order() {
    let f = Fixture::new();
    let host = tempfile::Builder::new()
        .prefix("host-dirs-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let host = fs::canonicalize(host.path()).unwrap();
    fs::create_dir_all(host.join("sub")).unwrap();
    fs::write(host.join("a.h"), "a").unwrap();
    fs::write(host.join("sub/b.h"), "b").unwrap();
    f.file(
        "MODULE.bazel",
        &format!(
            "module(name='main')\ng = use_repo_rule('//:repo.bzl', 'g')\nl = use_repo_rule('//:repo.bzl', 'l')\nd = use_repo_rule('//:repo.bzl', 'd')\nk = use_repo_rule('//:repo.bzl', 'k')\ng(name = 'gen', host = {0:?})\nl(name = 'lgen', host = {0:?})\nd(name = 'dep')\nk(name = 'linker')\n",
            host.display().to_string()
        ),
    );
    f.file(
        "repo.bzl",
        r#"def _gen(ctx):
    ctx.symlink(ctx.attr.host, "inc")
    ctx.file("BUILD.bazel", "filegroup(name = 'hdrs', srcs = glob(['inc/**']), visibility = ['//visibility:public'])\n")

g = repository_rule(_gen, attrs = {"host": attr.string()})

def _local(ctx):
    ctx.file("data/v.txt", "v")
    ctx.symlink("data", "alias")
    ctx.symlink(ctx.attr.host, "inc")
    ctx.file("BUILD.bazel", "filegroup(name = 'files', srcs = glob(['alias/**', 'inc/**']), visibility = ['//visibility:public'])\n")

l = repository_rule(_local, local = True, attrs = {"host": attr.string()})

def _dep(ctx):
    ctx.file("BUILD.bazel", "genrule(name = 'd', outs = ['d.txt'], cmd = 'echo d > $@', visibility = ['//visibility:public'])\n")
    ctx.file("defs.bzl", "VALUE = 'from-dep'\n")

d = repository_rule(_dep)

def _linker(ctx):
    ctx.symlink(Label("@dep//:defs.bzl"), "linked.bzl")
    ctx.file("BUILD.bazel", "load(':linked.bzl', 'VALUE')\ngenrule(name = 'g', outs = ['g.txt'], cmd = 'echo %s > $@' % VALUE, visibility = ['//visibility:public'])\n")

k = repository_rule(_linker)
"#,
    );
    f.file(".bazelignore", "ignored\n");
    f.file("ignored/BUILD.bazel", "fail('ignored')\n");
    f.file("BUILD.bazel", "");
    f.file("tools/BUILD.bazel", "");
    f.file(
        "tools/defs.bzl",
        "load(':helpers.bzl', 'SUFFIX')\n\ndef files(name):\n    native.filegroup(name = name, srcs = native.glob(['**/*' + SUFFIX]))\n",
    );
    f.file("tools/helpers.bzl", "SUFFIX = '.txt'\n");
    for package in 0..6 {
        // p0 loads through the link into `dep`'s tree while p3 demands
        // `dep` itself: which comes first depends on the loading order.
        let load = match package {
            0 => "load('@linker//:linked.bzl', 'VALUE')\n",
            3 => "load('@dep//:defs.bzl', 'VALUE')\n",
            _ => "",
        };
        f.file(
            &format!("p{package}/BUILD.bazel"),
            &format!(
                "load('//tools:defs.bzl', 'files')\n{load}files(name = 'files')\ngenrule(name = 'x', srcs = [':files', '@gen//:hdrs', '@lgen//:files', '@linker//:g', '@dep//:d'], outs = ['x{package}.out'], cmd = 'cat $(SRCS) > $@')\n"
            ),
        );
        f.file(&format!("p{package}/data/a.txt"), "a");
        f.file(&format!("p{package}/data/deeper/b.txt"), "b");
        f.file(&format!("p{package}/data/skip.go"), "package x");
        f.file(
            &format!("p{package}/sub/BUILD.bazel"),
            "exports_files(['c.txt'])\n",
        );
        f.file(&format!("p{package}/sub/c.txt"), "c");
    }
    let build = ["build", "--nobuild", "//..."];
    // Cold, then warm once the publications' stat fingerprints settled (a
    // warm reuse refreshes a manifest whose entries were racy).
    let cold = f.run_env(&[], &build);
    assert!(cold.status.success(), "{}", stderr(&cold));
    std::thread::sleep(std::time::Duration::from_millis(2200));
    let warm = f.run_env(&[], &build);
    assert!(warm.status.success(), "{}", stderr(&warm));
    // Outside the workspace: creating it must not change what is observed.
    let dumps = tempfile::Builder::new()
        .prefix("dumps-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let dump = dumps.path().join("dump.txt");
    let dump_text = dump.display().to_string();
    let record = |threads: &str, jitter: &str| {
        let mut env = vec![
            ("BAZEL_RS_SNAPSHOT", "record"),
            ("BAZEL_RS_SNAPSHOT_DUMP", dump_text.as_str()),
        ];
        if !jitter.is_empty() {
            env.push(("BAZEL_RS_LOADING_JITTER", jitter));
        }
        let threads = format!("--loading_phase_threads={threads}");
        let run = f.run_env(&env, &["build", "--nobuild", &threads, "//..."]);
        let text = stderr(&run);
        assert!(run.status.success(), "{threads} jitter={jitter}: {text}");
        let dump = fs::read_to_string(&dump).unwrap();
        assert!(
            !dump.lines().any(|line| line.starts_with("TAINT unstable ")),
            "{threads} jitter={jitter}: {dump}"
        );
        (recorded_inputs(&dump), text)
    };
    let (reference, text) = record("1", "");
    let lines = snapshot_lines(&text);
    assert!(
        lines.iter().any(|line| line.contains("eligible=yes")),
        "{text}"
    );
    assert!(
        reference
            .iter()
            .any(|line| line.starts_with("REPLAY +l+lgen match ")),
        "{reference:#?}"
    );
    // `dep`'s tree is recorded once, by its owner (whether or not the link
    // crossed into it first), and every tree has a repository.
    let repos: Vec<&String> = reference
        .iter()
        .filter(|line| line.starts_with("REPO "))
        .collect();
    assert_eq!(repos.len(), 3, "{repos:#?}");
    assert!(
        !lines.iter().any(|line| line.starts_with("link anchors:")),
        "{text}"
    );
    for threads in ["1", "8", "32"] {
        for jitter in ["", "7", "40", "99"] {
            let (recorded, _) = record(threads, jitter);
            if recorded != reference {
                let only = |a: &[String], b: &[String]| -> Vec<String> {
                    a.iter()
                        .filter(|line| !b.contains(line))
                        .take(20)
                        .cloned()
                        .collect()
                };
                panic!(
                    "threads={threads} jitter={jitter}: the recording differs\nonly in the reference: {:#?}\nonly here: {:#?}",
                    only(&reference, &recorded),
                    only(&recorded, &reference)
                );
            }
        }
    }
}

/// The stale-tree caveat of the store-tree link anchors (review item 7): a
/// reused repository can hold a link into an older tree of its target
/// repository (here `dep` republished with other BUILD bytes while the
/// linked file kept its digest, so `linker` is reused). The link resolves
/// to that old, verified tree — Bazel would read the current repository —
/// in a hit and a miss alike; the record mode names such trees once the run
/// is otherwise clean.
#[test]
fn record_mode_names_store_trees_a_link_reaches_but_no_repository_roots() {
    let f = Fixture::new();
    let module = |value: &str| {
        format!(
            "module(name='main')\nd = use_repo_rule('//:repo.bzl', 'd')\nk = use_repo_rule('//:repo.bzl', 'k')\nd(name = 'dep', value = '{value}')\nk(name = 'linker')\n"
        )
    };
    f.file("MODULE.bazel", &module("one"));
    f.file(
        "repo.bzl",
        r##"def _dep(ctx):
    ctx.file("BUILD.bazel", "# %s\nexports_files(['defs.bzl'])\n" % ctx.attr.value)
    ctx.file("defs.bzl", "VALUE = 'from-dep'\n")

d = repository_rule(_dep, attrs = {"value": attr.string()})

def _linker(ctx):
    ctx.symlink(Label("@dep//:defs.bzl"), "linked.bzl")
    ctx.file("BUILD.bazel", "load(':linked.bzl', 'VALUE')\ngenrule(name = 'g', outs = ['g.txt'], cmd = 'echo %s > $@' % VALUE, visibility = ['//visibility:public'])\n")

k = repository_rule(_linker)
"##,
    );
    f.file(
        "BUILD.bazel",
        "genrule(name = 'x', srcs = ['@linker//:g', '@dep//:defs.bzl'], outs = ['x.txt'], cmd = 'cat $(SRCS) > $@')\n",
    );
    let record = [("BAZEL_RS_SNAPSHOT", "record")];
    let build = ["build", "--nobuild", "//:x"];
    let anchors = |lines: &[&str]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| line.starts_with("link anchors: "))
            .map(|line| (*line).to_owned())
            .collect()
    };
    let cold = f.run_env(&record, &build);
    let text = stderr(&cold);
    assert!(cold.status.success(), "{text}");
    assert_eq!(
        anchors(&snapshot_lines(&text)),
        Vec::<String>::new(),
        "{text}"
    );
    let trees = |f: &Fixture| -> Vec<String> {
        let mut trees: Vec<String> =
            fs::read_dir(f.0.path().join("cache/repositories/direct/trees"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
        trees.sort();
        trees
    };
    let before = trees(&f);
    // `dep` republishes (a new tree; its warm probe, run again after the
    // publication, reuses it: the execution does not taint), `linker` is
    // reused with its link into the old tree — named at once.
    f.file("MODULE.bazel", &module("two"));
    let changed = f.run_env(&record, &build);
    let changed_text = stderr(&changed);
    assert!(changed.status.success(), "{changed_text}");
    let old = trees(&f)
        .into_iter()
        .find(|tree| {
            before.contains(tree) && {
                let linked =
                    f.0.path()
                        .join("cache/repositories/direct/trees")
                        .join(tree)
                        .join("defs.bzl");
                linked.exists()
            }
        })
        .expect("dep's old tree");
    let named = anchors(&snapshot_lines(&changed_text));
    assert_eq!(named.len(), 1, "{changed_text}");
    assert!(
        named[0].starts_with("link anchors: 1 store tree(s)") && named[0].ends_with(&old),
        "{changed_text}"
    );
    let warm = f.run_env(&record, &build);
    let text = stderr(&warm);
    assert!(warm.status.success(), "{text}");
    let lines = snapshot_lines(&text);
    assert!(
        lines.iter().any(|line| line.contains("eligible=yes")),
        "{text}"
    );
    let named = anchors(&lines);
    assert_eq!(named.len(), 1, "{text}");
    assert!(
        named[0].starts_with("link anchors: 1 store tree(s)") && named[0].ends_with(&old),
        "{text}"
    );
}
