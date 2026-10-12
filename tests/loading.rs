use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-loading-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='loading_fixture')\n");
        ws.file("BUILD.bazel", "");
        ws
    }
    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn outputs(o: &Output) -> Vec<PathBuf> {
    assert!(o.status.success(), "{}", stderr(o));
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| PathBuf::from(l.split_once('\t').unwrap().1))
        .collect()
}

#[test]
fn frozen_macros_use_callers_package_and_native_is_a_struct() {
    let ws = Workspace::new();
    ws.file("macros/BUILD.bazel", "");
    ws.file(
        "macros/helper.bzl",
        "CONFIG = struct(suffix='.txt')\ndef output_name(name):\n    return name + CONFIG.suffix\n",
    );
    ws.file("macros/nested/defs.bzl", "load(':helper.bzl', rename='output_name')\ndef copy(name, *, prefix=''):\n    if type(native) != 'struct':\n        fail('native must be struct')\n    native.genrule(name=name,srcs=native.glob(['*.in']),outs=[rename(name)],cmd='cat $(SRCS) > $@')\n    native.filegroup(name=name+'_context', srcs=[], visibility=['//visibility:public'])\n");
    ws.file(
        "BUILD.bazel",
        "load('//macros:nested/defs.bzl','copy')\ncopy(name='root')\n",
    );
    ws.file("input.in", "root context\n");
    ws.file(
        "other/BUILD.bazel",
        "load('//macros:nested/defs.bzl','copy')\ncopy(name='other')\n",
    );
    ws.file("other/input.in", "other context\n");
    let result = ws.run(&["build", "--jobs=2", "//:root", "//other:other"]);
    let paths = outputs(&result);
    assert_eq!(paths.len(), 2);
    assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "root context\n");
    assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "other context\n");
    assert!(stderr(&result).contains("2 actions executed"));
}

#[test]
fn package_defaults_apply_to_earlier_rules_but_not_exports() {
    let ws = Workspace::new();
    ws.file("pkg/BUILD.bazel", "filegroup(name='implicit',srcs=['data.txt'])\nfilegroup(name='private',srcs=['data.txt'],visibility=['//visibility:private'])\npackage(default_visibility=['//visibility:public'])\n");
    ws.file("pkg/data.txt", "defaulted");
    ws.file("private/BUILD.bazel", "package(default_visibility=['//visibility:private'])\nexports_files(['exported.txt'],visibility=None,licenses=None)\nexports_files(['exported.txt'])\n");
    ws.file("private/exported.txt", "public export");
    ws.file("BUILD.bazel", "filegroup(name='use',srcs=['//pkg:implicit','//pkg:data.txt','//private:exported.txt'])\nfilegroup(name='bad',srcs=['//pkg:private'])\n");
    assert_eq!(outputs(&ws.run(&["build", ":use"])).len(), 2);
    let result = ws.run(&["build", ":bad"]);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("not visible"));
}

#[test]
fn glob_excludes_subpackages_and_handles_dotfiles_like_bazel() {
    let ws = Workspace::new();
    ws.file("BUILD.bazel", "filegroup(name='files',srcs=glob(['**/*.txt'],exclude=['skip.txt']))\nfilegroup(name='hidden',srcs=glob(['.*.txt']))\n");
    for path in [
        "root.txt",
        "skip.txt",
        ".hidden.txt",
        "sub/deep.txt",
        "nested/subpackage.txt",
    ] {
        ws.file(path, path);
    }
    ws.file("nested/BUILD.bazel", "");
    let paths = outputs(&ws.run(&["build", ":files"]));
    let relative: Vec<_> = paths
        .iter()
        .map(|p| p.strip_prefix(&ws.0).unwrap())
        .collect();
    assert_eq!(relative, [Path::new("root.txt"), Path::new("sub/deep.txt")]);
    assert_eq!(
        outputs(&ws.run(&["build", ":hidden"])),
        [ws.0.join(".hidden.txt")]
    );
}

#[test]
fn empty_globs_are_errors_by_default_and_before_any_execution() {
    let ws = Workspace::new();
    for src in [
        "glob(['missing'])",
        "glob(['present','missing'])",
        "glob(['present'],exclude=['present'])",
        "glob(['present'],exclude=None)",
    ] {
        ws.file("present", "exists");
        ws.file("BUILD.bazel", &format!("genrule(name='first',outs=['out'],cmd='echo hi > $@')\nfilegroup(name='bad',srcs={src})\n"));
        let result = ws.run(&["build", ":first"]);
        assert!(!result.status.success(), "accepted {src}");
        assert!(!ws.0.join("bazel-rs-out").exists());
    }
    ws.file(
        "BUILD.bazel",
        "filegroup(name='empty',srcs=glob(['missing'],allow_empty=True))\n",
    );
    assert!(ws.run(&["build", ":empty"]).status.success());
}

#[test]
fn bzl_top_level_package_mutation_and_load_cycles_fail() {
    let ws = Workspace::new();
    ws.file("BUILD.bazel", "load(':bad.bzl','value')\n");
    for source in [
        "native.filegroup(name='injected')\nvalue=1",
        "value=native.glob(['*'])",
        "native.exports_files(['x'])\nvalue=1",
    ] {
        ws.file("bad.bzl", source);
        let result = ws.run(&["query", "//:all"]);
        assert!(!result.status.success());
        assert!(stderr(&result).contains("outside package evaluation"));
        assert!(result.stdout.is_empty());
    }
    ws.file("bad.bzl", "load(':other.bzl','value')\n");
    ws.file("other.bzl", "load(':bad.bzl','value')\n");
    assert!(stderr(&ws.run(&["query", "//:all"])).contains("load cycle"));
}

#[test]
fn package_exports_and_build_syntax_are_strict() {
    let ws = Workspace::new();
    for source in [
        "package()",
        "package(default_visibility=None)",
        "package(default_visibility=[])\npackage(default_visibility=[])",
        "exports_files(['x'],licenses=['notice'])",
        "exports_files(['x'])\nexports_files(['x'],visibility=['//visibility:private'])",
        "exports_files(['x'])\nfilegroup(name='x')",
        "exports_files(['x'])\ngenrule(name='g',outs=['x'],cmd='true')",
        "filegroup(**{'name':'x'})",
        "filegroup(name='x',srcs=list(*[['x']]))",
        "native.filegroup(name='x')",
        "value=struct(name='x')",
    ] {
        ws.file("BUILD.bazel", source);
        assert!(
            !ws.run(&["query", "//:all"]).status.success(),
            "accepted {source}"
        );
    }
}
