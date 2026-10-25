#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("dict-attribute-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main')\n");
        f.file("BUILD.bazel", "");
        f
    }
    fn file(&self, name: &str, text: &str) {
        fs::write(self.0.path().join(name), text).unwrap();
    }
    fn run(&self, command: &str, target: &str) -> Output {
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                target,
            ])
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }
    fn build(&self, target: &str) -> (Vec<u8>, PathBuf, String) {
        let result = self.run("build", target);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output = String::from_utf8(result.stdout).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 1, "{output}");
        let path = PathBuf::from(lines[0].split_once('\t').unwrap().1);
        (
            fs::read(&path).unwrap(),
            path,
            String::from_utf8(result.stderr).unwrap(),
        )
    }
    fn reject(&self, command: &str, target: &str, message: &str) {
        let output = self.run(command, target);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            diagnostic.contains(message)
                || (message == "frozen" && diagnostic.contains("immutable")),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

const REPO: &str = r#"
def _impl(ctx):
    ctx.file("BUILD.bazel", "filegroup(name='data',srcs=['value'],visibility=['//visibility:public'])\n")
    ctx.file("value", "\n".join([key + "=" + ",".join(values) for key, values in ctx.attr.d.items()]) + "\n")
repo = repository_rule(implementation=_impl, attrs={"d":attr.string_list_dict()})
"#;

#[test]
fn string_list_dict_direct_defaults_are_budgeted_across_all_declarations() {
    for count in [1, 3] {
        let f = Fixture::new();
        f.file(
            "repo.bzl",
            r#"
def _impl(ctx):
    print("CALLBACK_RAN")
    ctx.file("BUILD.bazel", "filegroup(name='data',srcs=['value'])\n")
    ctx.file("value", str(len(ctx.attr.d["key"][0])) + "\n")
repo = repository_rule(implementation=_impl, attrs={
    "d":attr.string_list_dict(default={"key":["x" * (3 * 1024 * 1024)]}),
})
"#,
        );
        let mut module = "module(name='main')\nr=use_repo_rule('//:repo.bzl','repo')\n".to_owned();
        for index in 0..count {
            module.push_str(&format!("r(name='made{index}')\n"));
        }
        f.file("MODULE.bazel", &module);
        let result = f.run("build", "@made0//:data");
        let stderr = String::from_utf8_lossy(&result.stderr);
        if count == 1 {
            assert!(result.status.success(), "{stderr}");
            assert!(stderr.contains("CALLBACK_RAN"));
            let stdout = String::from_utf8(result.stdout).unwrap();
            let path = stdout.trim().split_once('\t').unwrap().1;
            assert_eq!(fs::read(path).unwrap(), b"3145728\n");
        } else {
            assert_eq!(result.status.code(), Some(1), "{stderr}");
            assert!(
                stderr.contains("host preparation weighted byte limit"),
                "{stderr}"
            );
            assert!(!stderr.contains("CALLBACK_RAN"));
            let trees = f.0.path().join("cache/repositories/direct/trees");
            assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
        }
    }
}

#[test]
fn string_list_dict_adjacent_typed_list_attributes_accept_tuple_values() {
    let f = Fixture::new();
    f.file("input", "source\n");
    f.file("defs.bzl", r#"
def _impl(ctx):
    for output in ctx.outputs.generated:
        ctx.actions.write(output, ",".join(ctx.attr.names) + ":" + str(len(ctx.files.inputs)) + "\n")
    return []
r = rule(implementation=_impl, attrs={
    "names":attr.string_list(default=("default",)),
    "inputs":attr.label_list(allow_files=True),
    "generated":attr.output_list(),
})
"#);
    f.file("BUILD.bazel", "load(':defs.bzl','r')\nr(name='target',names=('x','x'),inputs=('input',),generated=('first','second'))\n");
    let result = f.run("build", "//:target");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert_eq!(output.lines().count(), 2);
    for line in output.lines() {
        assert_eq!(
            fs::read(line.split_once('\t').unwrap().1).unwrap(),
            b"x,x:1\n"
        );
    }
}

#[test]
fn string_list_dict_custom_snapshots_and_tuple_values_execute() {
    let f = Fixture::new();
    f.file("defs.bzl", r#"
original = {"z": ["before", "before"], "a": ()}
schema = attr.string_list_dict(default=original, doc=None)
original["z"].append("later")
original["new"] = ["later"]
def _impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "\n".join([key + "=" + ",".join(value) for key, value in ctx.attr.d.items()]) + "\n")
    return DefaultInfo(files=depset([output]))
r = rule(implementation=_impl, attrs={"d":schema})
def make(name):
    values = {"z": ["call"], "a": ()}
    r(name=name, d=values)
    values["z"].append("late")
"#);
    f.file("BUILD.bazel", "load(':defs.bzl','r','make')\nr(name='defaults')\nmake(name='supplied')\nr(name='none',d=None)\n");
    for (target, bytes) in [
        ("//:defaults", b"z=before,before\na=\n".as_slice()),
        ("//:supplied", b"z=call\na=\n"),
        ("//:none", b"z=before,before\na=\n"),
    ] {
        assert_eq!(f.build(target).0, bytes);
        assert!(
            f.build(target)
                .2
                .contains("0 actions executed, 1 cache hits")
        );
    }
}

#[test]
fn string_list_dict_nonempty_is_analysis_only_and_nested_empty_is_allowed() {
    let f = Fixture::new();
    f.file("defs.bzl", "def _impl(ctx):\n    return []\nr=rule(implementation=_impl,attrs={'d':attr.string_list_dict(False)})\n");
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl','r')\nr(name='empty')\nr(name='nested',d={'k':[]})\n",
    );
    assert!(f.run("query", "//:empty").status.success());
    f.reject("build", "//:empty", "attribute must be non empty");
    assert!(f.run("build", "//:nested").status.success());
    f.file(
        "repo.bzl",
        &REPO.replace("attr.string_list_dict()", "attr.string_list_dict(False)"),
    );
    f.file(
        "MODULE.bazel",
        "module(name='main')\nr=use_repo_rule('//:repo.bzl','repo')\nr(name='made',d={})\n",
    );
    assert_eq!(f.build("@made//:data").0, b"\n");
}

#[test]
fn string_list_dict_module_end_snapshot_and_repository_order_identity() {
    let f = Fixture::new();
    f.file("repo.bzl", REPO);
    f.file("MODULE.bazel", "module(name='main')\nr=use_repo_rule('//:repo.bzl','repo')\nd={'z':['before'],'a':()}\nr(name='made',d=d)\nd['z'].append('late')\n");
    assert_eq!(f.build("@made//:data").0, b"z=before,late\na=\n");
    let mut paths = Vec::new();
    for (dict, expected) in [
        ("{'z':('1',),'a':['2']}", b"z=1\na=2\n".as_slice()),
        ("{'z':('1',),'a':['2']}", b"z=1\na=2\n"),
        ("{'a':['2'],'z':('1',)}", b"a=2\nz=1\n"),
        ("{'z':('1',),'a':['2']}", b"z=1\na=2\n"),
    ] {
        f.file("MODULE.bazel", &format!("module(name='main')\nr=use_repo_rule('//:repo.bzl','repo')\nr(name='made',d={dict})\n"));
        let (bytes, path, _) = f.build("@made//:data");
        assert_eq!(bytes, expected);
        paths.push(path);
    }
    assert_eq!(paths[0], paths[1]);
    assert_ne!(paths[0], paths[2]);
    assert_eq!(paths[0], paths[3]);
}

#[test]
fn string_list_dict_tags_and_extension_calls_keep_distinct_snapshot_timing() {
    let f = Fixture::new();
    f.file("repo.bzl", REPO);
    f.file("ext.bzl", r#"
load(":repo.bzl", "repo")
def _impl(ctx):
    for module in ctx.modules:
        for tag in module.tags.data:
            repo(name="tagged", d=tag.d)
    values = {"z": ["before"], "a": ()}
    repo(name="first", d=values)
    values["z"].append("after")
    repo(name="second", d=values)
ext = module_extension(implementation=_impl, tag_classes={"data":tag_class(attrs={"d":attr.string_list_dict()})})
"#);
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','ext')\nd={'z':['tag'],'a':()}\ne.data(d=d)\nd['z'].append('late')\nuse_repo(e,'tagged','first','second')\n");
    assert_eq!(f.build("@tagged//:data").0, b"z=tag,late\na=\n");
    assert_eq!(f.build("@first//:data").0, b"z=before\na=\n");
    assert_eq!(f.build("@second//:data").0, b"z=before,after\na=\n");
}

#[test]
fn string_list_dict_frozen_mutations_fail_in_real_analysis_and_repositories() {
    for mutation in ["ctx.attr.d['new'] = []", "ctx.attr.d['z'].append('bad')"] {
        let f = Fixture::new();
        f.file("defs.bzl", &format!("def _impl(ctx):\n    {mutation}\n    return []\nr=rule(implementation=_impl,attrs={{'d':attr.string_list_dict(default={{'z':['good']}})}})\n"));
        f.file("BUILD.bazel", "load(':defs.bzl','r')\nr(name='target')\n");
        f.reject("build", "//:target", "frozen");
        assert!(!f.0.path().join("cache/actions").exists());
        f.file("repo.bzl", &format!("def _impl(ctx):\n    {mutation}\n    ctx.file('BUILD.bazel','')\nrepo=repository_rule(implementation=_impl,attrs={{'d':attr.string_list_dict(default={{'z':['good']}})}})\n"));
        f.file(
            "MODULE.bazel",
            "module(name='main')\nr=use_repo_rule('//:repo.bzl','repo')\nr(name='made')\n",
        );
        f.reject("query", "@made//:all", "frozen");
        let trees = f.0.path().join("cache/repositories/direct/trees");
        assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
    }
}
