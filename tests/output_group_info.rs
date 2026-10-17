//! N25d5 follow-ups (docs/CC_TOOLCHAIN.md): the per-target OutputGroupInfo of
//! native rule targets is one instance per configured target, the private
//! `_validation_transitive` group is restricted like Bazel's
//! RuleConfiguredTargetBuilder, and a File reachable only through a closure
//! in a provider is not a known action input.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("output-group-info-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='ogi_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("defs.bzl", defs);
        ws.file("src.txt", "source\n");
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn build(&self, targets: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .arg("build")
            .args(targets)
            .current_dir(self.path())
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    stderr(output)
}

fn rejected(output: &Output, message: &str) {
    let text = stderr(output);
    assert!(!output.status.success(), "unexpected success:\n{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
}

const CHECK: &str = r#"
def _check(ctx):
    a, b = ctx.attr.a, ctx.attr.b
    (OutputGroupInfo in a and a[OutputGroupInfo] == a[OutputGroupInfo]) or fail("same target")
    (a[OutputGroupInfo] == b[OutputGroupInfo]) or fail("two edges, one configured target")
    print("GROUPS", dir(a[OutputGroupInfo]), a[OutputGroupInfo]._hidden_top_level_INTERNAL_.to_list())
    return []

check = rule(implementation = _check, attrs = {"a": attr.label(), "b": attr.label()})
"#;

#[test]
fn native_rule_targets_have_one_output_group_info_per_configured_target() {
    let ws = Workspace::new(
        CHECK,
        "load(':defs.bzl', 'check')\n\
         filegroup(name = 'fg', srcs = ['src.txt'])\n\
         alias(name = 'al', actual = ':fg')\n\
         genrule(name = 'gen', outs = ['gen.txt'], cmd = 'echo > $@')\n\
         check(name = 'fg_twice', a = ':fg', b = ':fg')\n\
         check(name = 'via_alias', a = ':fg', b = ':al')\n\
         check(name = 'genrule', a = ':gen', b = ':gen')\n",
    );
    let text = ok(&ws.build(&[":fg_twice", ":via_alias", ":genrule"]));
    assert!(
        text.contains("GROUPS [\"_hidden_top_level_INTERNAL_\"]"),
        "{text}"
    );
}

const VALIDATION: &str = r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    files = [out] if ctx.attr.nonempty else []
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(**{ctx.attr.group: depset(files)})]

r = rule(implementation = _impl, attrs = {"group": attr.string(), "nonempty": attr.bool()})
"#;

#[test]
fn private_transitive_validation_group_is_restricted() {
    let ws = Workspace::new(
        VALIDATION,
        "load(':defs.bzl', 'r')\n\
         r(name = 'transitive', group = '_validation_transitive')\n\
         r(name = 'transitive_empty', group = '_validation_transitive')\n\
         r(name = 'validation', group = '_validation', nonempty = True)\n\
         r(name = 'validation_empty', group = '_validation')\n",
    );
    // RuleConfiguredTargetBuilder.java:366-377: the key alone is rejected
    // for a rule defined outside the internal allowlist.
    for target in [":transitive", ":transitive_empty"] {
        rejected(
            &ws.build(&[target]),
            "//:defs.bzl cannot access the _transitive_validation private API",
        );
    }
    rejected(
        &ws.build(&[":validation"]),
        "output group '_validation' (validation actions) is not supported by the native profile",
    );
    ok(&ws.build(&[":validation_empty"]));
}

const CLOSURE: &str = r#"
Box = provider(fields = ["get"])

def _producer(ctx):
    hidden = ctx.actions.declare_file("hidden.txt")
    ctx.actions.write(hidden, "hidden")
    def get():
        return hidden
    return [Box(get = get)]

producer = rule(implementation = _producer)

def _consumer(ctx):
    hidden = ctx.attr.dep[Box].get()
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run_shell(inputs = [hidden], outputs = [out], command = "cp $1 $2",
                          arguments = [hidden.path, out.path])
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"dep": attr.label()})
"#;

#[test]
fn file_reachable_only_through_a_provider_closure_is_not_a_known_input() {
    let ws = Workspace::new(
        CLOSURE,
        "load(':defs.bzl', 'consumer', 'producer')\n\
         producer(name = 'producer')\n\
         consumer(name = 'consumer', dep = ':producer')\n",
    );
    // The closure's File is never a provider payload File of the producer
    // (docs/CC_TOOLCHAIN.md), so the input is unknown, explicitly.
    rejected(
        &ws.build(&[":consumer"]),
        "action input bazel-out/k8-fastbuild/bin/hidden.txt is not a dependency or declared output",
    );
}
