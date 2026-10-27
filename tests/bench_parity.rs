//! Bugs found by the first like-for-like benchmark against genuine Bazel 9.3
//! (docs/BENCHMARKS.md). Every expected text below was captured from the
//! genuine `bazel-9.3.0-linux-x86_64` ELF (sha256 d302d22e…) on the same
//! workspace, with `--ignore_all_rc_files`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("bench-parity-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for (name, text) in files {
            let path = ws.0.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Unparenthesized tuples wherever Bazel's grammar takes `expr`
/// (Parser.java parseExpr): a `for` iterable (bazel-gazelle
/// internal/env.bzl:223), a subscript of any arity, both sides of an
/// assignment and an expression statement.
#[test]
fn unparenthesized_tuples_parse_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"t\")\n"),
        (
            "defs.bzl",
            r#"def check():
    out = []
    for key in "A", "B":
        out.append(key)
    for k, v in ("a", 1), ("b", 2):
        out.append(k + str(v))
    d = {("x", 1): "tx", ("x", 1, 2): "t3"}
    d["y", 2] = "ty"
    out.append(d["x", 1])
    out.append(d["x", 1, 2])
    out.append(d["y", 2])
    t = 1, 2
    out.append(str(t))
    "a", "b"
    return out
"#,
        ),
        (
            "BUILD.bazel",
            "load(\":defs.bzl\", \"check\")\nprint(check())\n",
        ),
    ]);
    let output = ws.run(&["query", "//..."]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    let debug = format!(
        "DEBUG: {}/BUILD.bazel:2:6: [\"A\", \"B\", \"a1\", \"b2\", \"tx\", \"t3\", \"ty\", \"(1, 2)\"]",
        ws.0.path().display()
    );
    assert!(err.lines().any(|line| line == debug), "{err}");
}

/// `--check_direct_dependencies` (bazel-gazelle's .bazelrc sets `off`) over
/// the mod_command fixture registry: the root requests mc_b@1.0 and
/// mc_leaf@1.0, selection picks 2.0 and 1.1. Genuine 9.3: WARNING events by
/// default, ERROR events and `ERROR: Direct dependency check failed.` (exit
/// 37 for `mod graph`) under `error`, nothing under `off`, and the
/// EnumConverter error for a bad value.
#[test]
fn check_direct_dependencies_modes_match_bazel() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ws = Workspace::new(&[
        (
            "MODULE.bazel",
            "module(name = \"cdd_root\", version = \"0.1\")\n\
             bazel_dep(name = \"mc_a\", version = \"1.0\")\n\
             bazel_dep(name = \"mc_b\", version = \"1.0\", repo_name = \"bee\")\n\
             bazel_dep(name = \"mc_leaf\", version = \"1.0\")\n",
        ),
        ("BUILD.bazel", "filegroup(name = \"x\")\n"),
    ]);
    let registries = [
        format!(
            "--registry=file://{}",
            fixture.join("mod_command/registry").display()
        ),
        format!(
            "--registry=file://{}",
            fixture.join("builtin_modules/registry").display()
        ),
    ];
    let run = |mode: &str| {
        let mut args = vec!["mod", "graph", "--lockfile_mode=off"];
        args.extend(registries.iter().map(String::as_str));
        let flag = format!("--check_direct_dependencies={mode}");
        args.push(&flag);
        let output = ws.run(&args);
        let events: Vec<String> = stderr(&output)
            .lines()
            .filter(|line| line.starts_with("ERROR: ") || line.starts_with("WARNING: "))
            .map(str::to_owned)
            .collect();
        (output.status.code(), events)
    };
    let mismatch = |kind: &str| {
        vec![
            format!(
                "{kind}: For repository 'bee', the root module requires module version mc_b@1.0, but got mc_b@2.0 in the resolved dependency graph. Please update the version in your MODULE.bazel or set --check_direct_dependencies=off"
            ),
            format!(
                "{kind}: For repository 'mc_leaf', the root module requires module version mc_leaf@1.0, but got mc_leaf@1.1 in the resolved dependency graph. Please update the version in your MODULE.bazel or set --check_direct_dependencies=off"
            ),
        ]
    };
    assert_eq!(run("warning"), (Some(0), mismatch("WARNING")));
    let mut errors = mismatch("ERROR");
    errors.push("ERROR: Direct dependency check failed.".to_owned());
    assert_eq!(run("error"), (Some(37), errors));
    assert_eq!(run("off"), (Some(0), vec![]));
    let (code, _) = run("bogus");
    assert_ne!(code, Some(0));
    let mut args = vec!["build", "--check_direct_dependencies=bogus", "//:x"];
    args.extend(registries.iter().map(String::as_str));
    let output = ws.run(&args);
    assert!(
        stderr(&output).contains("While parsing option --check_direct_dependencies=bogus: Not a valid direct deps check mode: 'bogus' (should be off, warning or error)"),
        "{}",
        stderr(&output)
    );
}

/// `--deleted_packages` (bazel-gazelle's .bazelrc): the deleted package's
/// BUILD file is no boundary (the parent's glob crosses into it), `//...`
/// skips it but still finds the packages below it, and loading it fails.
#[test]
fn deleted_packages_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"d\")\n"),
        (
            "BUILD.bazel",
            "filegroup(name = \"all\", srcs = glob([\"sub/**\"], allow_empty = True))\n",
        ),
        ("sub/BUILD.bazel", "filegroup(name = \"s\")\n"),
        ("sub/f.txt", ""),
        ("sub/inner/BUILD.bazel", "filegroup(name = \"i\")\n"),
    ]);
    let query = |expression: &str, deleted: &str| {
        let flag = format!("--deleted_packages={deleted}");
        let output = ws.run(&["query", &flag, expression]);
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr(&output),
        )
    };
    let (code, stdout, _) = query("//...", "sub");
    assert_eq!(
        (code, stdout.as_str()),
        (Some(0), "//:all\n//sub/inner:i\n")
    );
    let (code, stdout, _) = query("labels(srcs, //:all)", "//sub");
    assert_eq!(
        (code, stdout.as_str()),
        (Some(0), "//:sub/BUILD.bazel\n//:sub/f.txt\n")
    );
    let (code, stdout, _) = query("//sub/...", "sub");
    assert_eq!((code, stdout.as_str()), (Some(0), "//sub/inner:i\n"));
    for target in ["//sub:s", "//sub:f.txt"] {
        let (code, _, err) = query(target, "sub");
        assert_eq!(code, Some(7), "{err}");
        assert!(
            err.contains(
                "ERROR: no such package 'sub': Package is considered deleted due to --deleted_packages"
            ),
            "{err}"
        );
    }
    let (code, _, err) = query("//...", "a:b");
    assert_ne!(code, Some(0));
    assert!(
        err.contains(
            "While parsing option --deleted_packages=a:b: invalid package identifier 'a:b': contains ':'"
        ),
        "{err}"
    );
    // Without the option `sub` is a package again.
    let output = ws.run(&["query", "//..."]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "//:all\n//sub:s\n//sub/inner:i\n"
    );
}

/// `--subcommands` / `-s` print each executed spawn as a SUBCOMMAND event in
/// Bazel's copy-pastable form (CommandFailureUtils.describeCommand
/// COMPLETE); `pretty_print` puts one argument per line. The native header
/// differs where actions carry no ActionOwner (docs/BENCHMARKS.md).
#[test]
fn subcommands_print_spawns_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"s\")\n"),
        (
            "BUILD.bazel",
            "genrule(name = \"g\", outs = [\"g.txt\"], cmd = \"echo 'hi there' > $@\")\n\
             genrule(name = \"p\", outs = [\"p.txt\"], cmd = \"echo 'hi there' > $@\")\n",
        ),
    ]);
    let output = ws.run(&["build", "-s", "//:g"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    let start = lines
        .iter()
        .position(|line| {
            line.starts_with("SUBCOMMAND: # //:g [action 'Executing genrule //:g', configuration: ")
        })
        .unwrap_or_else(|| panic!("{err}"));
    assert!(lines[start + 1].starts_with("(cd /"), "{err}");
    assert!(lines[start + 1].ends_with(" && \\"), "{err}");
    // Genuine 9.3, verbatim.
    assert_eq!(
        lines[start + 2..start + 5],
        [
            "  exec env - \\",
            "    PATH=/bin:/usr/bin:/usr/local/bin \\",
            "  /bin/bash -c 'source external/bazel_tools/tools/genrule/genrule-setup.sh; echo '\\''hi there'\\'' > bazel-out/k8-fastbuild/bin/g.txt')",
        ],
        "{err}"
    );
    assert!(lines[start + 5].starts_with("# Configuration: "), "{err}");
    assert!(lines[start + 6].starts_with("# Runner: "), "{err}");
    let output = ws.run(&["build", "--subcommands=pretty_print", "//:p"]);
    let err = stderr(&output);
    let lines: Vec<&str> = err.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("SUBCOMMAND: # //:p "))
        .unwrap_or_else(|| panic!("{err}"));
    assert_eq!(
        lines[start + 4..start + 7],
        [
            "  /bin/bash \\",
            "    -c \\",
            "    'source external/bazel_tools/tools/genrule/genrule-setup.sh; echo '\\''hi there'\\'' > bazel-out/k8-fastbuild/bin/p.txt')",
        ],
        "{err}"
    );
    // A cached action reaches no runner: nothing is printed.
    let output = ws.run(&["build", "-s", "//:g"]);
    assert!(
        !stderr(&output).contains("SUBCOMMAND"),
        "{}",
        stderr(&output)
    );
    let output = ws.run(&["build", "--subcommands=bogus", "//:g"]);
    assert!(
        stderr(&output).contains("While parsing option --subcommands=bogus: Not a valid subcommand option: 'bogus' (should be true, pretty_print or false)"),
        "{}",
        stderr(&output)
    );
}

/// `--build_tag_filters` (TargetPatternPhaseFunction.mergeAll's
/// TargetUtils.tagFilter): a test rule's tags include its size and
/// `noflaky`; `test` still runs the tests it determines.
#[test]
fn build_tag_filters_select_targets_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"b\")\n"),
        (
            "defs.bzl",
            "def _t(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + \".sh\")\n    ctx.actions.write(out, \"#!/bin/sh\\nexit 0\\n\", is_executable = True)\n    return [DefaultInfo(executable = out)]\n\nmy_test = rule(implementation = _t, test = True)\n",
        ),
        (
            "BUILD.bazel",
            "load(\":defs.bzl\", \"my_test\")\n\
             genrule(name = \"a\", outs = [\"a.txt\"], cmd = \"touch $@\", tags = [\"foo\"])\n\
             genrule(name = \"b\", outs = [\"b.txt\"], cmd = \"touch $@\", tags = [\"bar\"])\n\
             genrule(name = \"c\", outs = [\"c.txt\"], cmd = \"touch $@\")\n\
             my_test(name = \"t\", tags = [\"foo\"], size = \"small\")\n",
        ),
    ]);
    let built = |args: &[&str]| {
        let output = ws.run(args);
        let err = stderr(&output);
        assert!(output.status.success(), "{err}");
        let found = err
            .lines()
            .find(|line| line.starts_with("INFO: Found "))
            .unwrap_or_default()
            .to_owned();
        let labels: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split('\t').next())
            .map(str::to_owned)
            .collect();
        (found, labels)
    };
    // Genuine 9.3: the `INFO: Found` lines of the same commands.
    let cases: [(&str, &str, &[&str]); 5] = [
        (
            "--build_tag_filters=foo",
            "INFO: Found 2 targets...",
            &["//:a", "//:t"],
        ),
        (
            "--build_tag_filters=-foo",
            "INFO: Found 2 targets...",
            &["//:b", "//:c"],
        ),
        (
            "--build_tag_filters=small",
            "INFO: Found 1 target...",
            &["//:t"],
        ),
        (
            "--build_tag_filters=foo,-bar",
            "INFO: Found 2 targets...",
            &["//:a", "//:t"],
        ),
        (
            "--build_tag_filters=noflaky",
            "INFO: Found 1 target...",
            &["//:t"],
        ),
    ];
    for (flag, found, labels) in cases {
        assert_eq!(
            built(&["build", flag, "//..."]),
            (
                found.to_owned(),
                labels.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>()
            ),
            "{flag}"
        );
    }
    let output = ws.run(&["test", "--build_tag_filters=bar", "//..."]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(
        err.contains("INFO: Found 1 target and 1 test target..."),
        "{err}"
    );
}

/// Options Bazel 9.3 declares as no-ops or whose effect is on its server
/// state are accepted (genuine: each builds); a command that does not
/// register one rejects it like Bazel, and the boolean converter still
/// checks values.
#[test]
fn no_effect_options_are_accepted_like_bazel() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"n\")\n"),
        ("BUILD.bazel", "filegroup(name = \"a\")\n"),
    ]);
    let output = ws.run(&[
        "build",
        "--nobuild",
        "--discard_analysis_cache",
        "--notrack_incremental_state",
        "--nokeep_state_after_build",
        "--heap_dump_on_oom",
        "--watchfs",
        "--incompatible_python_disallow_native_rules",
        "--incompatible_enable_cc_toolchain_resolution",
        "//:a",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = ws.run(&[
        "query",
        "--enable_bzlmod",
        "--noenable_workspace",
        "--nolegacy_external_runfiles",
        "--incompatible_disallow_empty_glob",
        "--fetch",
        "//:a",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "//:a\n",
        "{}",
        stderr(&output)
    );
    let output = ws.run(&["query", "--discard_analysis_cache", "//:a"]);
    assert!(
        stderr(&output)
            .contains("--discard_analysis_cache :: Unrecognized option: --discard_analysis_cache"),
        "{}",
        stderr(&output)
    );
    let output = ws.run(&["build", "--nobuild", "--enable_bzlmod=maybe", "//:a"]);
    assert!(
        stderr(&output)
            .contains("While parsing option --enable_bzlmod=maybe: 'maybe' is not a boolean"),
        "{}",
        stderr(&output)
    );
}
