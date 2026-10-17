//! Real CLI checks for active-extension-only native rule introspection.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("existing-rules-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file(
            "MODULE.bazel",
            "module(name='main')\ne=use_extension('//:defs.bzl','ext')\nuse_repo(e,'made')\n",
        );
        f.file("BUILD.bazel","genrule(name='copy',srcs=['@made//:value'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
        f.file("repo.bzl","def _impl(ctx):\n    print('REPO_CALLBACK_EXECUTED')\n    ctx.file('BUILD.bazel',\"exports_files(['value'])\\n\")\n    ctx.file('value',ctx.attr.value)\nr=repository_rule(_impl,attrs={'value':attr.string()})\n");
        f
    }
    fn file(&self, name: &str, text: &str) {
        fs::write(self.0.path().join(name), text).unwrap();
    }
    fn defs(&self, body: &str) {
        self.file("defs.bzl",&format!("load(':repo.bzl','r')\none=native.existing_rule\nall_rules=native.existing_rules\ndef _impl(ctx):\n{}\next=module_extension(implementation=_impl)\n",body.lines().map(|l|format!("    {l}\n")).collect::<String>()));
    }
    fn run(&self) -> Output {
        Command::new("timeout")
            .args([
                "60s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "//:copy",
            ])
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
    fn reject(&self, marker: &str) {
        let result = self.run();
        let error = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(1), "{error}");
        assert!(result.stdout.is_empty());
        assert!(error.contains(marker), "{error}");
        for directory in ["trees", "manifests", "locks"] {
            let path = self
                .0
                .path()
                .join("cache/repositories/direct")
                .join(directory);
            assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
        }
    }
}

#[test]
fn existing_rules_active_extension_returns_empty_before_and_after_recording() {
    let f = Fixture::new();
    f.defs("before=one('made')\nempty=all_rules()\ncopy=dict(empty)\ncopy['writable']='yes'\nr(name='made',value=str(before)+'|'+type(empty)+'|'+str(len(empty))+'|'+copy['writable'])\nif one('made') != None or all_rules() != {} or one('../not-a-name') != None or one('') != None:\n    fail('incorrect repository lookup')");
    for _ in 0..2 {
        let output = f.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("REPO_CALLBACK_EXECUTED")
                .count(),
            1
        );
        let line = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            fs::read(line.trim().split_once('\t').unwrap().1).unwrap(),
            b"None|dict|0|yes"
        );
    }
}

#[test]
fn existing_rules_initialization_rejects_through_both_module_loading_paths() {
    for direct in [false, true] {
        for function in ["existing_rule('made')", "existing_rules()"] {
            let f = Fixture::new();
            if direct {
                f.file(
                    "MODULE.bazel",
                    "module(name='main')\nr=use_repo_rule('//:defs.bzl','r')\nr(name='made')\n",
                );
            }
            f.file("defs.bzl",&format!("load(':repo.bzl',_r='r')\nx=native.{function}\nr=_r\ndef _impl(ctx):\n    r(name='made')\next=module_extension(implementation=_impl)\n"));
            f.reject("can only be used while evaluating a BUILD file, a legacy macro, or a rule finalizer");
        }
    }
}

#[test]
fn existing_rules_repository_alias_calls_reject_before_and_after_writes() {
    for direct in [false, true] {
        for after_write in [false, true] {
            for function in ["existing_rule('made')", "existing_rules()"] {
                let f = Fixture::new();
                let (name, args) = function.split_once('(').unwrap();
                f.file("repo.bzl",&format!("alias=native.{name}\ndef _impl(ctx):\n{}    value=alias({args}\nr=repository_rule(_impl)\n",if after_write {"    ctx.file('partial','must not publish')\n"} else {""}));
                if direct {
                    f.file(
                        "MODULE.bazel",
                        "module(name='main')\nr=use_repo_rule('//:repo.bzl','r')\nr(name='made')\n",
                    );
                } else {
                    f.defs("r(name='made')");
                }
                f.reject("can only be used while evaluating a BUILD file, a legacy macro, or a rule finalizer");
            }
        }
    }
}

#[test]
fn existing_rules_empty_dictionary_rejects_every_mutating_operation() {
    for mutation in [
        "value['x']=1",
        "value.update({'x':1})",
        "value.setdefault('x',1)",
        "value.clear()",
    ] {
        let f = Fixture::new();
        f.defs(&format!("value=all_rules()\n{mutation}\nr(name='made')"));
        let output = f.run();
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert_eq!(output.status.code(), Some(1), "{error}");
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
        assert!(!error.contains("repo_callback_executed"));
        let trees = f.0.path().join("cache/repositories/direct/trees");
        assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
    }
}

#[test]
fn existing_rules_argument_binding_is_strict_and_positional_only() {
    for call in [
        "one()",
        "one(None)",
        "one(1)",
        "one([])",
        "one(name='made')",
        "one('made','extra')",
        "one('made',other=1)",
        "all_rules(1)",
        "all_rules(name='made')",
    ] {
        let f = Fixture::new();
        f.defs(&format!("{call}\nr(name='made')"));
        let output = f.run();
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{call}: {error}");
        let diagnostic = error.to_lowercase();
        assert!(
            diagnostic.contains("argument") || diagnostic.contains("parameter"),
            "{error}"
        );
        assert!(
            !error.contains("can only be used while evaluating"),
            "argument checks must run before body: {error}"
        );
        assert!(!error.contains("REPO_CALLBACK_EXECUTED"));
    }
}

#[test]
fn existing_rules_do_not_hide_duplicate_repository_recordings() {
    let f = Fixture::new();
    f.defs("r(name='made',value='first')\nif one('made') == None and all_rules() == {}:\n    r(name='made',value='second')");
    let output = f.run();
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{error}");
    assert!(error.contains("duplicate generated repository"), "{error}");
    assert!(!error.contains("REPO_CALLBACK_EXECUTED"));
    let trees = f.0.path().join("cache/repositories/direct/trees");
    assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
}
