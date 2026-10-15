//! Real CLI scenarios for Bazel 9.3 lexer and string semantics (N26 review
//! follow-up): eager escape errors in BUILD/MODULE/.bzl with Bazel columns,
//! raw strings, byte escapes in BUILD actions, Bazel's split()/isspace/title/
//! elems in repository evaluation, .bzl load failure texts and the DEBUG
//! reproducibility lines of repository rules.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("n26-strings-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name = 'main')\n");
        f
    }

    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("/usr/bin/timeout")
            .args([
                "60s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }

    fn built(&self, target: &str) -> Vec<u8> {
        let output = self.run(&["build", target]);
        assert!(output.status.success(), "{}", stderr(&output));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let path = PathBuf::from(stdout.lines().next().unwrap().split_once('\t').unwrap().1);
        fs::read(path).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, marker: &str) {
    assert!(!output.status.success(), "{}", stderr(output));
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
    assert!(stderr(output).contains(marker), "{}", stderr(output));
}

/// A repository rule whose callback returns the data file's text.
fn probe(f: &Fixture, body: &str) {
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nmake = use_repo_rule('//:defs.bzl', 'make')\nmake(name = 'made')\n",
    );
    let body: String = body.lines().map(|line| format!("    {line}\n")).collect();
    f.file(
        "defs.bzl",
        format!(
            "def _value(ctx):\n{body}\ndef _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data'])\\n\")\n    ctx.file('data', _value(ctx), executable = False)\n\nmake = repository_rule(_impl)\n"
        ),
    );
    f.file("BUILD.bazel", "");
}

#[test]
fn invalid_escapes_fail_build_bzl_and_module_files_at_the_escape_column() {
    let f = Fixture::new();
    for (build, marker) in [
        (
            "V = \"\\x41\"\nfilegroup(name = 'x')\n",
            "BUILD.bazel:1:7-8: invalid escape sequence: \\x. Use '\\\\' to insert '\\'.",
        ),
        (
            "V = \"\\400\"\nfilegroup(name = 'x')\n",
            "BUILD.bazel:1:9-10: octal escape sequence out of range (maximum is \\377)",
        ),
        (
            "filegroup(name = 'x', tags = ['\\q'])\n",
            "invalid escape sequence: \\q.",
        ),
    ] {
        f.file("BUILD.bazel", build);
        rejected(&f.run(&["query", "//:x"]), marker);
    }
    // A .bzl that is only loaded (never executed in a repository) too.
    f.file(
        "BUILD.bazel",
        "load(':lib.bzl', 'v')\nfilegroup(name = 'x')\n",
    );
    f.file("lib.bzl", "v = 1\ndef unused():\n    return '\\N'\n");
    rejected(
        &f.run(&["query", "//:x"]),
        "lib.bzl:3:14-15: invalid escape sequence: \\N.",
    );
    // MODULE.bazel lexer errors fail every command, even for an unused value.
    f.file("BUILD.bazel", "filegroup(name = 'x')\n");
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nunused = ['ok', \"\\u0041\"]\n",
    );
    rejected(
        &f.run(&["query", "//:x"]),
        "MODULE.bazel:2:19-20: invalid escape sequence: \\u.",
    );
}

#[test]
fn raw_strings_keep_backslashes_and_build_byte_escapes_decode_at_actions() {
    let f = Fixture::new();
    probe(
        &f,
        "return r\"\\\"\" + \"|\" + str(len(r\"\\\"\")) + r'\\''",
    );
    // Bazel keeps the backslash before a quote (Lexer.java:305-320).
    assert_eq!(f.built("@made//:data"), b"\\\"|2\\'");
    // BUILD strings are bytes; process arguments decode them as UTF-8 with
    // replacement (StringEncoding.internalToPlatform): '\377' -> U+FFFD.
    f.file(
        "BUILD.bazel",
        "genrule(name = 'g', outs = ['g.txt'], cmd = 'printf %s > $@' % '\\101\\377')\ngenrule(name = 'e', outs = ['e.txt'], cmd = 'printf %s > $@' % '\\303\\251')\n",
    );
    assert_eq!(f.built("//:g"), b"A\xef\xbf\xbd");
    assert_eq!(f.built("//:e"), "é".as_bytes());
    // Lone bytes stay distinct values (bazel_lib strings.bzl byte tables) and
    // FileWriteAction writes the raw internal bytes.
    f.file(
        "write.bzl",
        "TABLE = {'\\200': 128, '\\201': 129, '\\377': 255}\ndef _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, '\\377A' + str(TABLE['\\201']) + str(len('\\377')))\n    return [DefaultInfo(files = depset([out]))]\nwrite = rule(_impl)\n",
    );
    f.file(
        "BUILD.bazel",
        "load(':write.bzl', 'write')\nwrite(name = 'w')\n",
    );
    assert_eq!(f.built("//:w"), b"\xffA1291");
    // Args written as a parameter file: ParameterFile.java:97 writes internal
    // bytes too, after ShellEscaper quoting (non-ASCII is quoted).
    f.file(
        "args.bzl",
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    args = ctx.actions.args()\n    args.add('\\377A')\n    args.add('plain')\n    args.add('\\303\\251')\n    ctx.actions.write(out, args)\n    return [DefaultInfo(files = depset([out]))]\nargs_write = rule(_impl)\n",
    );
    f.file(
        "BUILD.bazel",
        "load(':args.bzl', 'args_write')\nargs_write(name = 'a')\n",
    );
    assert_eq!(f.built("//:a"), b"'\xffA'\nplain\n'\xc3\xa9'\n");
}

#[test]
fn repository_strings_follow_bazel_string_module() {
    let f = Fixture::new();
    probe(
        &f,
        "return '|'.join([\n    str(['\\240'.isspace(), '\\205'.isspace(), ' '.isspace(), 'a'.isspace()]),\n    '\\351'.upper(), 'a\\351b'.title(), '\\351x'.capitalize(),\n    str([len('\\303\\251'.elems()), '\\303\\251'.elems()[0] == '\\303', type('ab'.elems())]),\n    str('a,b,,c'.split(',')), str('a,b,c'.split(',', 1)), str('a,b,c'.rsplit(',', 1)),\n    str('a,b'.split(sep = ',', maxsplit = 0)),\n]) + '\\n'",
    );
    assert_eq!(
        f.built("@made//:data"),
        b"[True, True, True, False]|\xe9|A\xe9b|\xc9x|[2, True, \"list\"]|[\"a\", \"b\", \"\", \"c\"]|[\"a\", \"b,c\"]|[\"a,b\", \"c\"]|[\"a,b\"]\n"
    );
    for (expression, message) in [
        (
            "'a b'.split()",
            "split() missing 1 required positional argument: sep",
        ),
        (
            "'a b'.rsplit()",
            "rsplit() missing 1 required positional argument: sep",
        ),
        (
            "'a b'.split(None, 1)",
            "in call to split(), parameter 'sep' got value of type 'NoneType', want 'string'",
        ),
        ("'a b'.split('')", "Empty separator"),
    ] {
        probe(&f, &format!("return {expression}"));
        rejected(&f.run(&["build", "@made//:data"]), message);
    }
}

#[test]
fn bzl_load_failures_use_bzl_compile_function_texts() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.file("lib/BUILD.bazel", "");
    f.file("lib/real/x.bzl", "v = 1\n");
    symlink("missing.bzl", f.0.path().join("lib/dangling.bzl")).unwrap();
    symlink("real", f.0.path().join("lib/dirlink.bzl")).unwrap();
    symlink("cyc2.bzl", f.0.path().join("lib/cyc1.bzl")).unwrap();
    symlink("cyc1.bzl", f.0.path().join("lib/cyc2.bzl")).unwrap();
    let root = fs::canonicalize(f.0.path()).unwrap();
    for (name, message) in [
        (
            "dangling.bzl",
            "cannot load '//lib:dangling.bzl': no such file".to_owned(),
        ),
        (
            "absent.bzl",
            "cannot load '//lib:absent.bzl': no such file".to_owned(),
        ),
        (
            "dirlink.bzl",
            "cannot load '//lib:dirlink.bzl': is a directory".to_owned(),
        ),
        (
            "cyc1.bzl",
            format!(
                "circular symlinks detected\n[start of symlink cycle]\n{0}/lib/cyc1.bzl\n{0}/lib/cyc2.bzl\n[end of symlink cycle]",
                root.display()
            ),
        ),
    ] {
        f.file(
            "BUILD.bazel",
            format!("load('//lib:{name}', 'v')\nfilegroup(name = 'x')\n"),
        );
        rejected(&f.run(&["query", "//:x"]), &message);
    }
}

#[test]
fn repository_rules_print_bazel_debug_reproducibility_lines() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nmake = use_repo_rule('//:defs.bzl', 'make')\nmake(name = 'same', probe = 'p', mode = 'same')\nmake(name = 'changed', probe = 'p', mode = 'changed')\nmake(name = 'dropped', probe = 'p', mode = 'dropped')\nmake(name = 'both', probe = 'p', mode = 'both')\nmake(name = 'dict', probe = 'p', mode = 'dict')\n",
    );
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    ctx.file("BUILD.bazel", "exports_files(['data'])\n")
    ctx.file("data", "x\n", executable = False)
    mode = ctx.attr.mode
    if mode == "same":
        return ctx.repo_metadata(attrs_for_reproducibility = {"probe": "p", "mode": mode})
    if mode == "changed":
        return ctx.repo_metadata(attrs_for_reproducibility = {"probe": "other", "mode": mode, "count": 1})
    if mode == "dropped":
        return ctx.repo_metadata(attrs_for_reproducibility = {"undeclared": "v", "mode": mode})
    if mode == "both":
        return ctx.repo_metadata(attrs_for_reproducibility = {"count": 2})
    return {"probe": "changed", "mode": mode}

make = repository_rule(_impl, attrs = {"probe": attr.string(), "mode": attr.string(), "count": attr.int()})
"#,
    );
    f.file("BUILD.bazel", "");
    let debug = |name: &str| -> Vec<String> {
        let output = f.run(&["build", &format!("@{name}//:data")]);
        assert!(output.status.success(), "{}", stderr(&output));
        stderr(&output)
            .lines()
            .filter(|line| line.starts_with("DEBUG: "))
            .map(str::to_owned)
            .collect()
    };
    let prefix = "DEBUG: Repo '+make+";
    let tail = "' indicated that a canonical reproducible form can be obtained by";
    assert_eq!(debug("same"), Vec::<String>::new());
    assert_eq!(
        debug("changed"),
        [
            format!("{prefix}changed{tail} modifying arguments probe = \"other\", count = 1"),
            "DEBUG: Repo +make+changed defined by rule make in @@//:defs.bzl".to_owned()
        ]
    );
    assert_eq!(
        debug("dropped"),
        [
            format!("{prefix}dropped{tail} dropping arguments [probe]"),
            "DEBUG: Repo +make+dropped defined by rule make in @@//:defs.bzl".to_owned()
        ]
    );
    assert_eq!(
        debug("both")[0],
        format!("{prefix}both{tail} modifying arguments count = 2 and dropping [probe, mode]")
    );
    assert_eq!(
        debug("dict")[0],
        format!("{prefix}dict{tail} modifying arguments probe = \"changed\"")
    );
}
