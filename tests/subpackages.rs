//! Bazel 9.3 native.subpackages captures, real macro dependencies, and loading
//! snapshot invalidation. See docs/SUBPACKAGES.md for reference receipts.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // isolated CLI fixtures

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("subpackages-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for dir in ["ws", "cache", "home", "tmp"] {
            fs::create_dir(f.0.path().join(dir)).unwrap();
        }
        f.file("MODULE.bazel", "module(name='fixture')\n");
        f.file("BUILD.bazel", "");
        f
    }

    fn ws(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    fn file(&self, path: &str, text: &str) {
        let path = self.ws().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn link(&self, target: &str, path: &str) {
        let path = self.ws().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("cache").display()
            ))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("BAZEL_RS_SNAPSHOT", mode)
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("BAZEL_RS_SNAPSHOT_VERIFY_STRICT", "1")
            .current_dir(self.ws())
            .explicit_modules()
            .output()
            .unwrap()
    }

    fn query(&self, target: &str) -> String {
        let out = self.run(&["query", target], "off");
        assert!(out.status.success(), "{}", err(&out));
        err(&out)
    }

    fn rejected(&self, target: &str, message: &str) {
        let out = self.run(&["query", target], "off");
        assert!(!out.status.success(), "unexpected success: {message}");
        assert!(
            err(&out).contains(message),
            "expected {message:?}: {}",
            err(&out)
        );
    }

    fn manifest(&self, target: &str, mode: &str, expected: &str) -> String {
        let out = self.run(&["build", target], mode);
        assert!(out.status.success(), "{}", err(&out));
        let stdout = String::from_utf8(out.stdout.clone()).unwrap();
        let (_, path) = stdout.trim().split_once('\t').unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), expected, "{stdout}");
        err(&out)
    }
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const MACRO: &str = r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    paths = sorted([f.short_path for d in ctx.attr.deps for f in d[DefaultInfo].files.to_list()])
    ctx.actions.write(out, "\n".join(ctx.attr.names) + "\n" + "|".join(paths) + "\n")
    return [DefaultInfo(files=depset([out]))]
_manifest = rule(implementation=_impl, attrs={"names": attr.string_list(), "deps": attr.label_list()})
def distribution(name):
    names = native.subpackages(include=["**"], allow_empty=True)
    prefix = native.package_name()
    labels = ["//" + (prefix + "/" if prefix else "") + p + ":data" for p in names]
    native.filegroup(name=name + "_files", srcs=labels)
    _manifest(name=name, names=names, deps=[":" + name + "_files"])
def listing(name):
    _manifest(name=name, names=native.subpackages(include=["**"], allow_empty=True))
"#;

const DATA: &str =
    "filegroup(name='data', srcs=['value.txt'], visibility=['//visibility:public'])\n";

#[test]
fn nearest_boundaries_order_patterns_mutable_strings_and_markers() {
    let f = Fixture::new();
    for dir in [
        "a",
        "a/deeper",
        "z/deep",
        ".hidden",
        "both",
        "dir-marker/BUILD.bazel",
        "[bracket]",
        "{brace}",
        "(paren)",
        "paren",
        "colon:name",
        "back\\slash",
    ] {
        f.file(
            &format!("p/{dir}/BUILD"),
            "this child is deliberately not evaluated !!!\n",
        );
    }
    f.file("p/both/BUILD.bazel", "");
    f.file("p/marker-target", "");
    f.link("../marker-target", "p/linked-marker/BUILD.bazel");
    f.link("a", "p/linked");
    f.link("missing", "p/dangling");
    f.file("p/dangling-marker/child/BUILD", "");
    f.link("missing", "p/dangling-marker/BUILD.bazel");
    for marker in ["MODULE.bazel", "REPO.bazel", "WORKSPACE"] {
        f.file(&format!("p/nested-repo/{marker}"), "");
    }
    f.file("p/nested-repo/child/BUILD", "");
    f.file("check.bzl", r#"
def check():
    expected = ["(paren)", ".hidden", "[bracket]", "a", "back\\slash", "both", "colon:name", "dangling-marker/child", "dir-marker/BUILD.bazel", "linked", "linked-marker", "nested-repo/child", "paren", "z/deep", "{brace}"]
    value = native.subpackages(include=["z/**", "a", "**", "a"])
    if type(value) != "list" or value != expected or [type(x) for x in value] != ["string"] * len(value):
        fail(value)
    value.append("mutation")
    if native.subpackages(include=["**"]) != expected:
        fail("a returned list must be fresh")
    if native.subpackages(include=["a/**", "z/**"]) != ["a", "z/deep"]:
        fail("nearest package boundaries")
    if native.subpackages(include=["a/deeper"], allow_empty=True) != []:
        fail("must not cross a BUILD, even for a literal pattern")
    if native.subpackages(include=["**"], exclude=["*hidden*"]) != expected:
        fail("compound exclusions do not match hidden names")
    if native.subpackages(include=("a",), exclude=()) != ["a"]:
        fail("tuple sequence")
    if native.subpackages(include=range(0), allow_empty=True) != []:
        fail("empty sequence")
    if native.subpackages(include=["[bracket]", "{brace}", "(paren)", "colon:name", "back\\slash"]) != ["(paren)", "[bracket]", "back\\slash", "colon:name", "{brace}"]:
        fail("these characters are literals")
    if native.subpackages(include=["*(paren)*"]) != ["(paren)", "paren"]:
        fail("UnixGlob regex parentheses")
    if native.subpackages(include=["**"], exclude=["", "../a", "/a", "a//x", "a/."]) != expected:
        fail("literal excludes are not validated")
    if "a" in native.subpackages(include=["**"], exclude=["?"]):
        fail("question wildcard is allowed in exclusions")
    native.filegroup(name="ok")
"#);
    f.file("p/BUILD.bazel", "load('//:check.bzl', 'check')\ncheck()\n");
    f.query("//p:ok");
}

#[test]
fn captured_argument_pattern_and_context_errors() {
    let f = Fixture::new();
    f.file("p/yes/BUILD", "");
    for (call, message) in [
        (
            "subpackages()",
            "subpackages() missing 1 required named argument: include",
        ),
        (
            "subpackages(['*'])",
            "subpackages() got unexpected positional argument",
        ),
        (
            "subpackages(include='*')",
            "parameter 'include' got value of type 'string', want 'sequence'",
        ),
        (
            "subpackages(include=[1])",
            "expected value of type 'string' for element 0 of 'subpackages' argument, but got 1 (int)",
        ),
        (
            "subpackages(include=['**'], exclude=None)",
            "parameter 'exclude' got value of type 'NoneType', want 'sequence'",
        ),
        (
            "subpackages(include=['**'], allow_empty=1)",
            "parameter 'allow_empty' got value of type 'int', want 'bool'",
        ),
        (
            "subpackages(include=[], nope=True)",
            "subpackages() got unexpected keyword argument 'nope'",
        ),
        (
            "subpackages(include=[])",
            "all subpackages in subpackages() have been excluded, but allow_empty is set to False ",
        ),
        (
            "subpackages(include=['yes', 'missing'])",
            "subpackages pattern 'missing' didn't match anything, but allow_empty is set to False (the default value)",
        ),
        (
            "subpackages(include=['yes'], exclude=['yes'])",
            "all subpackages in subpackages() have been excluded, but allow_empty is set to False ",
        ),
        (
            "subpackages(include=[''])",
            "invalid glob pattern '': pattern cannot be empty",
        ),
        (
            "subpackages(include=['/a'])",
            "invalid glob pattern '/a': pattern cannot be absolute",
        ),
        (
            "subpackages(include=['../a'])",
            "invalid glob pattern '../a': segment '..' not permitted",
        ),
        (
            "subpackages(include=['a/.'])",
            "invalid glob pattern 'a/.': segment '.' not permitted",
        ),
        (
            "subpackages(include=['a//b'])",
            "invalid glob pattern 'a//b': empty segment not permitted",
        ),
        (
            "subpackages(include=['a**'])",
            "invalid glob pattern 'a**': recursive wildcard must be its own segment",
        ),
        (
            "subpackages(include=['?'])",
            "invalid glob pattern '?': wildcard ? forbidden",
        ),
        (
            "subpackages(include=['yes'], exclude=['a**'])",
            "recursive wildcard must be its own segment (in glob pattern 'a**')",
        ),
        (
            "subpackages(include=['missing'], exclude=['a**'])",
            "subpackages pattern 'missing' didn't match anything",
        ),
        (
            "subpackages(include=['missing', 'a**'])",
            "invalid glob pattern 'a**': recursive wildcard must be its own segment",
        ),
        (
            "subpackages(include=range(1))",
            "expected value of type 'string' for element 0 of 'subpackages' argument, but got 0 (int)",
        ),
    ] {
        f.file("p/BUILD.bazel", &format!("{call}\nfilegroup(name='ok')\n"));
        f.rejected("//p:ok", message);
    }
    let context = "subpackages() can only be used while evaluating a BUILD file or a legacy macro";
    f.file("bad.bzl", "x = native.subpackages(include=['**'])\n");
    f.file(
        "p/BUILD.bazel",
        "load('//:bad.bzl', 'x')\nfilegroup(name='ok')\n",
    );
    f.rejected("//p:ok", context);
    f.file("sym.bzl", "def _impl(name, visibility):\n    native.subpackages(include=['**'])\nm = macro(implementation=_impl)\n");
    f.file("p/BUILD.bazel", "load('//:sym.bzl', 'm')\nm(name='ok')\n");
    f.rejected("//p:ok", context);
    f.file("impl.bzl", "def _impl(ctx):\n    native.subpackages(include=['**'])\n    return []\nr = rule(implementation=_impl)\n");
    f.file("p/BUILD.bazel", "load('//:impl.bzl', 'r')\nr(name='ok')\n");
    let out = f.run(&["build", "//p:ok"], "off");
    assert!(!out.status.success());
    assert!(err(&out).contains(context), "{}", err(&out));
    f.file("repo.bzl", "def _impl(ctx):\n    native.subpackages(include=['**'])\nr = repository_rule(implementation=_impl)\n");
    f.file(
        "MODULE.bazel",
        "module(name='fixture')\nr = use_repo_rule('//:repo.bzl', 'r')\nr(name='bad')\n",
    );
    f.rejected("@bad//:ok", context);
}

#[test]
fn ignored_boundaries_are_results_but_ignored_nonpackages_are_not_traversed() {
    let f = Fixture::new();
    f.file(".bazelignore", "p/prefix\np/ignored-parent\n");
    f.file("REPO.bazel", "ignore_directories(['**/pattern'])\n");
    for dir in [
        "prefix",
        "pattern",
        "ignored-parent/child",
        "a/pattern/child",
        "yes/child",
    ] {
        f.file(&format!("p/{dir}/BUILD"), "");
    }
    f.file(
        "p/BUILD.bazel",
        "print('RESULT', subpackages(include=['**']))\nfilegroup(name='ok')\n",
    );
    assert!(
        f.query("//p:ok")
            .contains("RESULT [\"pattern\", \"prefix\", \"yes/child\"]")
    );
    f.rejected("//p/prefix:all", "Package is considered deleted");
}

#[test]
fn real_macro_dependencies_use_build_bazel_precedence_in_main_and_external_repos() {
    let f = Fixture::new();
    f.file("defs.bzl", MACRO);
    f.file(
        "p/BUILD.bazel",
        "load('//:defs.bzl', 'distribution')\ndistribution(name='manifest')\n",
    );
    f.file("p/a/BUILD", "fail('BUILD.bazel must win')\n");
    f.file("p/a/BUILD.bazel", DATA);
    f.file("p/a/value.txt", "main source bytes\n");
    f.file("p/a/deeper/BUILD", "fail('must not be loaded')\n");
    f.manifest("//p:manifest", "off", "a\np/a/value.txt\n");
    let out = f.run(&["build", "//p:manifest_files"], "off");
    assert!(out.status.success(), "{}", err(&out));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let (_, path) = stdout.trim().split_once('\t').unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "main source bytes\n");
    f.file("ext/MODULE.bazel", "module(name='ext', version='1.0')\n");
    f.file("ext/defs.bzl", MACRO);
    f.file(
        "ext/BUILD.bazel",
        "load(':defs.bzl', 'distribution')\ndistribution(name='manifest')\n",
    );
    f.file("ext/a/BUILD", DATA);
    f.file("ext/a/value.txt", "external source bytes\n");
    f.file("MODULE.bazel", "module(name='fixture')\nbazel_dep(name='ext', version='1.0')\nlocal_path_override(module_name='ext', path='ext')\n");
    let out = f.run(&["build", "@ext//:manifest_files"], "off");
    assert!(out.status.success(), "{}", err(&out));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let (_, path) = stdout.trim().split_once('\t').unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "external source bytes\n");
}

#[test]
fn symlink_boundaries_dangling_cycles_and_host_policy() {
    let f = Fixture::new();
    f.file(
        "p/BUILD.bazel",
        "print('RESULT', subpackages(include=['self']))\nfilegroup(name='ok')\n",
    );
    f.link(".", "p/self");
    assert!(f.query("//p:ok").contains("RESULT [\"self\"]"));
    f.file(
        "p/BUILD.bazel",
        "subpackages(include=['s*'])\nfilegroup(name='ok')\n",
    );
    f.rejected(
        "//p:ok",
        "Symlink issue while evaluating globs: Infinite symlink expansion:",
    );
    fs::remove_file(f.ws().join("p/self")).unwrap();
    f.link("loop", "p/loop");
    f.file(
        "p/BUILD.bazel",
        "subpackages(include=['*'], exclude=['loop'], allow_empty=True)\nfilegroup(name='ok')\n",
    );
    f.rejected("//p:ok", "error globbing [*] - [loop] op=SUBPACKAGES:");
    fs::remove_file(f.ws().join("p/loop")).unwrap();
    f.link("missing", "p/dangling");
    f.query("//p:ok");
    fs::create_dir_all(f.0.path().join("host")).unwrap();
    fs::write(f.0.path().join("host/BUILD"), "").unwrap();
    symlink(f.0.path().join("host"), f.ws().join("p/host")).unwrap();
    f.rejected("//p:ok", "outside its repository root");
}

#[test]
fn snapshots_invalidate_build_boundaries_ignored_dirs_and_link_targets() {
    let f = Fixture::new();
    f.file("defs.bzl", MACRO);
    f.file(
        "p/BUILD.bazel",
        "load('//:defs.bzl', 'listing')\nlisting(name='manifest')\n",
    );
    for dir in ["a/deep", "b/deep"] {
        f.file(&format!("p/{dir}/BUILD"), DATA);
        f.file(&format!("p/{dir}/value.txt"), dir);
    }
    let initial = "a/deep\nb/deep\n\n";
    assert!(f.manifest("//p:manifest", "on", initial).contains("saved "));
    assert!(f.manifest("//p:manifest", "on", initial).contains("hit "));
    assert!(
        f.manifest("//p:manifest", "verify", initial)
            .contains("verified ")
    );
    let changed = |expected: &str| {
        let log = f.manifest("//p:manifest", "on", expected);
        assert!(log.contains("miss ") && log.contains("saved "), "{log}");
        let log = f.manifest("//p:manifest", "verify", expected);
        assert!(log.contains("verified "), "{log}");
    };
    // Adding/removing a BUILD in a directory already traversed changes the
    // nearest boundary, not merely recursive discovery's list of packages.
    f.file("p/a/BUILD.bazel", DATA);
    f.file("p/a/value.txt", "a");
    changed("a\nb/deep\n\n");
    fs::remove_file(f.ws().join("p/a/BUILD.bazel")).unwrap();
    changed(initial);
    f.file(".bazelignore", "p/a\n");
    changed("b/deep\n\n");
    fs::remove_file(f.ws().join(".bazelignore")).unwrap();
    changed(initial);
    f.file("REPO.bazel", "ignore_directories(['p/b'])\n");
    changed("a/deep\n\n");
    fs::remove_file(f.ws().join("REPO.bazel")).unwrap();
    changed(initial);
    f.link("a", "p/link");
    changed("a/deep\nb/deep\nlink/deep\n\n");
    // The link's bytes do not change: creation/removal of a BUILD in its
    // target must invalidate both logical traversals.
    f.file("p/a/BUILD", DATA);
    changed("a\nb/deep\nlink\n\n");
    fs::remove_file(f.ws().join("p/a/BUILD")).unwrap();
    changed("a/deep\nb/deep\nlink/deep\n\n");
    fs::remove_file(f.ws().join("p/link")).unwrap();
    f.link("../missing", "p/link");
    changed(initial);
    f.file("missing/BUILD", DATA);
    f.file("missing/value.txt", "new symlink target");
    changed("a/deep\nb/deep\nlink\n\n");
    fs::remove_file(f.ws().join("missing/BUILD")).unwrap();
    changed(initial);
    f.link("../../marker", "p/linked-marker/BUILD.bazel");
    changed(initial);
    f.file("marker", "");
    changed("a/deep\nb/deep\nlinked-marker\n\n");
    fs::remove_file(f.ws().join("marker")).unwrap();
    changed(initial);
}
