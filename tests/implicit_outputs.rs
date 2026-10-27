//! N46: `rule(outputs = ...)` implicit outputs through the CLI
//! (docs/IMPLICIT_OUTPUTS.md): `%{attr}` template substitution, the
//! function form, output file targets at loading, `ctx.outputs` entries,
//! default outputs order, rules_cc cc_binary's shape and Bazel's error texts.
//! Expected texts are genuine Bazel 9.3 strings (paired capture
//! tests/fixtures/implicit_outputs/reference_receipt.json); the workspace is
//! tests/fixtures/implicit_outputs/workspace.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/verify_env.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            copy_file(&entry.path(), &target);
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("implicit-outputs-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/implicit_outputs/workspace"),
            f.0.path(),
        );
        f
    }

    fn run(&self, args: &[&str]) -> Output {
        let cache = format!("--cache-dir={}", self.0.path().join(".cache").display());
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path())
            .arg("--ignore_all_rc_files")
            .arg(cache)
            .args(args)
            .explicit_modules()
            .output()
            .unwrap()
    }

    fn build(&self, targets: &[&str]) -> (Vec<(String, String)>, String) {
        let mut args = vec!["build"];
        args.extend_from_slice(targets);
        let output = self.run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{targets:?}: {stderr}");
        let outputs = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let (label, path) = line.split_once('\t').unwrap();
                let relative = path.split_once("/bin/").unwrap().1;
                (label.to_owned(), relative.to_owned())
            })
            .collect();
        (outputs, stderr)
    }

    fn rejected(&self, target: &str, expected: &str) {
        let output = self.run(&["build", target]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{target}: unexpected success");
        assert!(stderr.contains(expected), "{target}: {stderr}");
        assert!(!stderr.contains("panicked"), "{target}: {stderr}");
    }
}

fn printed(stderr: &str) -> &str {
    stderr
        .lines()
        .find_map(|line| line.split_once("OUTPUTS ").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("no OUTPUTS print: {stderr}"))
}

#[test]
fn templates_substitute_attributes_and_order_default_outputs() {
    let f = Fixture::new();
    let (outputs, stderr) = f.build(&["//ok:sub/x"]);
    assert_eq!(
        printed(&stderr),
        "ctx.outputs(a = <generated file ok/sub/x.a>, b = <generated file ok/in-q.b>, c = <generated file ok/sub/x.c>, o = <generated file ok/explicit.o>, os = []) [\"a\", \"b\", \"c\", \"o\", \"os\"]"
    );
    // Implicit outputs precede explicit ones (Rule.java:526-528).
    let files = outputs
        .iter()
        .map(|(_, path)| path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        files,
        ["ok/sub/x.a", "ok/in-q.b", "ok/sub/x.c", "ok/explicit.o"]
    );
    let (_, stderr) = f.build(&["//okdefault:y"]);
    assert!(
        printed(&stderr).contains("b = <generated file okdefault/dir/in.tar-.b>"),
        "{stderr}"
    );
    let (outputs, _) = f.build(&["//outs_t:x"]);
    let files = outputs
        .iter()
        .map(|(_, path)| path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        files,
        [
            "outs_t/out.txt.x",
            "outs_t/one.y",
            "outs_t/out.txt",
            "outs_t/one"
        ]
    );
    let (_, stderr) = f.build(&["//list_t:x"]);
    assert!(
        printed(&stderr).contains("one = <generated file list_t/dir/a.one>"),
        "{stderr}"
    );
    let (_, stderr) = f.build(&["//tags1:x", "//dep_set:x", "//sel_plain:x"]);
    for expected in ["tags1/t1.t", "dep_set/old.t", "sel_plain/v.sel"] {
        assert!(stderr.contains(expected), "{expected}: {stderr}");
    }
    // An executable rule's implicit outputs and its executable.
    let (outputs, _) = f.build(&["//exe_ok:x"]);
    let files = outputs
        .iter()
        .map(|(_, path)| path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(files, ["exe_ok/x.a", "exe_ok/x"]);
}

#[test]
fn output_file_targets_are_buildable_and_queryable() {
    let f = Fixture::new();
    // rules_cc cc_binary's declaration shape: the rule's own files to build
    // are the binary; the implicit outputs are built when requested.
    let (outputs, _) = f.build(&["//cc:bw.so"]);
    assert_eq!(outputs, [("//cc:bw.so".to_owned(), "cc/bw.so".to_owned())]);
    let (outputs, _) = f.build(&["//cc:bw.so.stripped", "//cc:bw.so.dwp"]);
    assert_eq!(
        outputs,
        [
            ("//cc:bw.so.dwp".to_owned(), "cc/bw.so.dwp".to_owned()),
            (
                "//cc:bw.so.stripped".to_owned(),
                "cc/bw.so.stripped".to_owned()
            ),
        ]
    );
    for (pattern, expected) in [
        ("//cc:bw.so.dwp", "//cc:bw.so.dwp"),
        ("//fn_ok:x.tpl", "//fn_ok:x.tpl"),
        ("//cc:all", "//cc:bw.so"),
    ] {
        let output = f.run(&["query", pattern]);
        assert!(output.status.success(), "{pattern}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }
}

#[test]
fn function_form_computes_outputs_from_attributes() {
    let f = Fixture::new();
    let (_, stderr) = f.build(&["//fn_ok:x"]);
    assert_eq!(
        printed(&stderr),
        "ctx.outputs(k = <generated file fn_ok/x.q>, l = <generated file fn_ok/l-in.txt-None-tP>, o = None, os = [], t = <generated file fn_ok/x.tpl>) [\"k\", \"l\", \"o\", \"os\", \"t\"]"
    );
}

#[test]
fn loading_errors_use_bazel_texts() {
    let f = Fixture::new();
    for (target, expected) in [
        (
            "//unknown:x",
            "In rule //unknown:x: For attribute 'u' in outputs: Invalid placeholder(s) in template",
        ),
        (
            "//private:x",
            "In rule //private:x: For attribute 'p' in outputs: Invalid placeholder(s) in template",
        ),
        (
            "//int_t:x",
            "In rule //int_t:x: For attribute 'n' in outputs: Attributes of type int cannot be used in an outputs substitution template",
        ),
        (
            "//bool_t:x",
            "For attribute 'b' in outputs: Attributes of type boolean cannot be used in an outputs substitution template",
        ),
        (
            "//dict_t:x",
            "For attribute 'd' in outputs: Attributes of type dict(string, string) cannot be used in an outputs substitution template",
        ),
        (
            "//vis_t:x",
            "For attribute 'visibility' in outputs: Attributes of type list(label) cannot be used in an outputs substitution template",
        ),
        (
            "//emptylist:x",
            "In rule //emptylist:x: For attribute 'e' in outputs: Invalid placeholder(s) in template",
        ),
        (
            "//sel:x",
            "In rule //sel:x: Attribute s is configurable and cannot be used in outputs",
        ),
        (
            "//collide:x",
            "Implicit output key 'o' collides with output attribute name",
        ),
        (
            "//up:x",
            "illegal output file name '../x' in rule //up:x due to: invalid target name '../x': target names may not contain up-level references '..'",
        ),
        (
            "//empty:x",
            "illegal output file name '' in rule //empty:x due to: invalid target name '': empty target name",
        ),
        (
            "//dup:x",
            "rule 'x' has more than one generated file named 'x.same'",
        ),
        (
            "//input:x",
            "rule 'x' has file 'x.a' as both an input and an output",
        ),
        (
            "//exported:x",
            "generated file 'x.a' in rule 'x' conflicts with existing source file, defined at ",
        ),
        (
            "//filegroup:x",
            "generated file 'x.a' in rule 'x' conflicts with existing filegroup rule, defined at ",
        ),
        (
            "//after:x",
            "filegroup rule 'x.a' conflicts with existing generated file from rule 'x', defined at ",
        ),
        (
            "//other_rule:x",
            "generated file 'x.a' in rule 'x' conflicts with existing generated file from rule 'g', defined at ",
        ),
        (
            "//prefix:x",
            "rule 'x' has conflicting output files 'x/sub' and 'x'",
        ),
        (
            "//fn_unknown:x",
            "In rule //fn_unknown:x: Attribute 'nope' either doesn't exist or uses a select() (i.e. could have multiple values)",
        ),
        (
            "//fn_select:x",
            "In rule //fn_select:x: Attribute 's' either doesn't exist or uses a select() (i.e. could have multiple values)",
        ),
        (
            "//fn_list:x",
            "In rule //fn_list:x: got list for 'implicit outputs function return value', want dict",
        ),
        (
            "//fn_int:x",
            "In rule //fn_int:x: got dict<string, int> for 'implicit outputs function return value', want dict<string, string>",
        ),
        (
            "//fn_multi:x",
            "In rule //fn_multi:x: expected one element but was: <a.m, b.m>",
        ),
        (
            "//fn_badtpl:x",
            "In rule //fn_badtpl:x: For attribute 'bad' in outputs: Invalid placeholder(s) in template",
        ),
        ("//fn_fail:x", "Error in fail: boom x"),
        // Conflicts are thrown at the declaring call with Bazel's frame.
        (
            "//export_after:x",
            "Error in exports_files: source file 'x.a' conflicts with existing generated file from rule 'x', defined at ",
        ),
        (
            "//export_rule:f",
            "Error in exports_files: source file 'f' conflicts with existing filegroup rule, defined at ",
        ),
        // Rule.getLabels includes select() branches.
        (
            "//select_input:x",
            "Error in exe_ok: rule 'x' has file 'x.a' as both an input and an output",
        ),
        (
            "//conflict_stops:x",
            "Error in filegroup: filegroup rule 'x' conflicts with existing filegroup rule, defined at ",
        ),
    ] {
        f.rejected(target, expected);
    }
    let stopped = f.run(&["build", "//conflict_stops:x"]);
    assert!(!String::from_utf8_lossy(&stopped.stderr).contains("later failure"));
    let exported = f.run(&["build", "//exported:x"]);
    assert!(
        String::from_utf8_lossy(&exported.stderr).contains("exported/BUILD.bazel:2:14"),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
}

#[test]
fn analysis_errors_and_explicit_native_failures() {
    let f = Fixture::new();
    f.rejected(
        "//exe:x",
        "in exe rule //exe:x: Multiple outputs with the same key: executable",
    );
    f.rejected(
        "//collide_unset:x",
        "in collide rule //collide_unset:x: Multiple outputs with the same key: o",
    );
    // Genuine Bazel 9.3 crashes on these; the native profile fails explicitly.
    for target in ["//multi:x", "//tags_t:x", "//lbl_t:x", "//dep_t:x"] {
        f.rejected(target, "genuine Bazel 9.3 crashes");
    }
    // Bazel warns and lets the generated file shadow the rule.
    f.rejected("//selfname:x", "which the native profile does not model");
}

#[test]
fn unterminated_placeholder_is_literal_output_name() {
    // `%{` is literal text in an output name; with Bazel's label grammar
    // (N38) native builds it like genuine Bazel (receipt row lit:build,
    // reference_files ["lit/lit%{name"]).
    let f = Fixture::new();
    let (outputs, stderr) = f.build(&["//lit:x"]);
    assert_eq!(
        printed(&stderr),
        "ctx.outputs(e = <generated file lit/lit%{name>, o = None, os = []) [\"e\", \"o\", \"os\"]"
    );
    let files = outputs
        .iter()
        .map(|(_, path)| path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(files, ["lit/lit%{name"]);
}

#[test]
fn implicit_outputs_function_runs_without_package_context() {
    let f = Fixture::new();
    // StarlarkCallbackHelper's transient thread: package functions fail.
    for target in ["//fn_native:x", "//fn_glob:x"] {
        f.rejected(
            target,
            "can only be used while evaluating a BUILD file or a legacy or symbolic macro",
        );
    }
    // Arguments are positional only: a keyword-only parameter is never bound
    // (Bazel's text since N33).
    f.rejected(
        "//fn_kwonly:x",
        "In rule //fn_kwonly:x: _f_kwonly() does not accept positional arguments, but got 1",
    );
}

/// `fs::copy`, except that a file with any execute bit is re-created through
/// `write_executable_script` (bytes and mode kept).
fn copy_file(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(from).unwrap().permissions().mode() & 0o7777;
    if mode & 0o111 != 0 {
        write_executable_script(to, fs::read(from).unwrap(), mode);
    } else {
        fs::copy(from, to).unwrap();
    }
}
