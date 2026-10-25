#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("symbolic-macro-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='macro_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("defs/BUILD.bazel", "");
        ws.file("defs/macros.bzl", defs);
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

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn untouched(&self) {
        assert!(!self.path().join("executed-marker").exists());
        assert!(!self.path().join("private-cache/actions").exists());
        assert!(!self.path().join("bazel-rs-out").exists());
    }

    fn marker_rule(&self) -> String {
        format!(
            "genrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
            self.path().join("executed-marker").display()
        )
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", errors(output));
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

fn outputs(output: &Output) -> Vec<PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .collect()
}

fn content(output: &Output) -> Vec<Vec<u8>> {
    outputs(output)
        .iter()
        .map(|p| fs::read(p).unwrap())
        .collect()
}

const LOAD: &str = "load('//defs:macros.bzl','m')\n";
const ZERO: &str = "def _impl(name, visibility):\n    pass\nm=macro(implementation=_impl)\n";
const WRITE: &str = r#"
def _write(ctx):
    out = ctx.actions.declare_file(ctx.label.name + '.txt')
    ctx.actions.write(out, ctx.attr.content)
    return [DefaultInfo(files=depset([out]))]
write = rule(implementation=_write, attrs={'content':attr.string()})
"#;

#[test]
fn symbolic_macro_native_custom_and_private_closure_actions_are_cached() {
    let ws = Workspace::new(
        "load('//helpers:factory.bzl','make_impl')\n_private=macro(implementation=make_impl('macro-ok\\n'),attrs={},inherit_attrs=None,finalizer=False,doc=None)\nm=_private\n",
        "",
    );
    ws.file("helpers/BUILD.bazel", "");
    ws.file("helpers/factory.bzl", &format!(r#"
{WRITE}
def _legacy(name, text):
    write(name=name+'_custom', content=text)
def make_impl(text):
    def _closed(name, visibility):
        if native.package_name() != 'app':
            fail('wrong physical caller')
        _legacy(name, text)
        native.genrule(name=name, srcs=[':'+name+'_custom'], outs=[name+'.out'], cmd='cat $(SRCS) > $@', visibility=visibility)
    return _closed
"#));
    ws.file("relay/BUILD.bazel", "");
    ws.file(
        "relay/reexport.bzl",
        "load('//defs:macros.bzl', alias='m')\nm=alias\n",
    );
    ws.file(
        "app/BUILD.bazel",
        "load('//relay:reexport.bzl','m')\nm(name='demo')\n",
    );
    success(&ws.run(&["query", "//app:all"]));
    ws.untouched();
    let cold = ws.run(&["build", "--jobs=4", "//app:demo"]);
    assert_eq!(content(&cold), [b"macro-ok\n".to_vec()]);
    assert!(errors(&cold).contains("2 actions executed"));
    let paths = outputs(&cold);
    assert!(paths[0].starts_with(ws.path().join("private-cache")));
    let hashes: Vec<_> = paths
        .iter()
        .map(|p| blake3::hash(&fs::read(p).unwrap()))
        .collect();
    let warm = ws.run(&["build", "//app:demo"]);
    assert_eq!(outputs(&warm), paths);
    assert_eq!(content(&warm), [b"macro-ok\n".to_vec()]);
    assert_eq!(
        outputs(&warm)
            .iter()
            .map(|p| blake3::hash(&fs::read(p).unwrap()))
            .collect::<Vec<_>>(),
        hashes
    );
    assert!(errors(&warm).contains("0 actions executed, 2 cache hits"));
}

#[test]
fn symbolic_macro_private_rules_and_outputs_ignore_callers_public_default() {
    let ws = Workspace::new(
        r#"
def _impl(name, visibility):
    native.genrule(name=name+'_private', outs=[name+'_private.out'], cmd='printf macro-ok > $@')
    native.filegroup(name=name, srcs=[':'+name+'_private'], visibility=visibility)
m=macro(implementation=_impl)
"#,
        "",
    );
    ws.file("app/BUILD.bazel", &format!("{LOAD}package(default_visibility=['//visibility:public'])\nm(name='demo')\nfilegroup(name='rule_consumer',srcs=[':demo_private'])\nfilegroup(name='output_consumer',srcs=[':demo_private.out'])\n"));
    for target in ["//app:rule_consumer", "//app:output_consumer"] {
        rejected(&ws.run(&["build", target]), "not visible");
        ws.untouched();
    }
    ws.file(
        "other/BUILD.bazel",
        "filegroup(name='use',srcs=['//app:demo'])\n",
    );
    assert_eq!(
        content(&ws.run(&["build", "//other:use"])),
        [b"macro-ok".to_vec()]
    );
    // Visibility protects dependency edges, not explicitly selected private roots.
    assert_eq!(
        content(&ws.run(&["build", "//app:demo_private"])),
        [b"macro-ok".to_vec()]
    );
}

#[test]
fn symbolic_macro_cannot_read_ordinary_caller_private_rule_output_or_file() {
    for dependency in [":ordinary", ":ordinary.out", ":data"] {
        let ws = Workspace::new(
            &format!(
                "def _impl(name, visibility):\n    native.filegroup(name=name,srcs=[{dependency:?}],visibility=visibility)\nm=macro(implementation=_impl)\n"
            ),
            "",
        );
        ws.file("app/data", "private source");
        ws.file("app/BUILD.bazel", &format!("{LOAD}genrule(name='ordinary',outs=['ordinary.out'],cmd='printf ordinary > $@')\nexports_files(['data'],visibility=['//visibility:private'])\nm(name='demo')\n"));
        rejected(&ws.run(&["build", "//app:demo"]), "not visible");
        ws.untouched();
        ws.file("app/BUILD.bazel", &format!("{LOAD}genrule(name='ordinary',outs=['ordinary.out'],cmd='printf ordinary > $@',visibility=['//defs:__pkg__'])\nexports_files(['data'],visibility=['//defs:__pkg__'])\nm(name='demo')\n"));
        let expected = if dependency == ":data" {
            b"private source".as_slice()
        } else {
            b"ordinary"
        };
        assert_eq!(
            content(&ws.run(&["build", "//app:demo"])),
            [expected.to_vec()]
        );
    }
}

#[test]
fn symbolic_macro_definition_owner_survives_reexports_and_instance_names() {
    let ws = Workspace::new(
        r#"
def _impl(name, visibility):
    if name == 'seed':
        native.genrule(name=name+'_private',outs=[name+'.out'],cmd='printf owner > $@')
    else:
        native.filegroup(name=name,srcs=[':seed_private'],visibility=visibility)
m=macro(implementation=_impl)
"#,
        "",
    );
    ws.file("relay/BUILD.bazel", "");
    ws.file(
        "relay/defs.bzl",
        "load('//defs:macros.bzl', imported='m')\nreexport=imported\n",
    );
    ws.file("otherdefs/BUILD.bazel", "");
    ws.file("otherdefs/macros.bzl", "def _impl(name,visibility):\n    native.filegroup(name=name,srcs=[':seed_private'],visibility=visibility)\nforeign=macro(implementation=_impl)\n");
    ws.file("app/BUILD.bazel", &format!("{LOAD}load('//relay:defs.bzl','reexport')\nload('//otherdefs:macros.bzl','foreign')\nm(name='seed')\nreexport(name='friend')\nforeign(name='stranger')\n"));
    rejected(&ws.run(&["build", "//app:stranger"]), "not visible");
    ws.untouched();
    assert_eq!(
        content(&ws.run(&["build", "//app:friend"])),
        [b"owner".to_vec()]
    );
}

#[test]
fn symbolic_macro_sources_require_ordinary_declaration_even_in_definition_package() {
    let defs = "def _impl(name,visibility):\n    native.filegroup(name=name,srcs=['data'],visibility=visibility)\nm=macro(implementation=_impl)\ndef _establish(ctx):\n    fail('MUST_NOT_ANALYZE_ESTABLISHER')\nestablish=rule(implementation=_establish,attrs={'dep':attr.label(default=':data',allow_files=True)})\n";
    for declaration in [
        "",
        "exports_files(['data'])\n",
        "filegroup(name='unbuilt',srcs=['data'])\n",
        "establish(name='unbuilt')\n",
    ] {
        for before in [true, false] {
            let ws = Workspace::new(defs, "");
            ws.file("defs/data", "genuine-source-content\n");
            let invocation = "m(name='demo')\n";
            let body = if before {
                format!("{declaration}{invocation}")
            } else {
                format!("{invocation}{declaration}")
            };
            ws.file(
                "defs/BUILD.bazel",
                &format!("load('//defs:macros.bzl','m','establish')\n{body}"),
            );
            let result = ws.run(&["build", "//defs:demo"]);
            if declaration.is_empty() {
                rejected(&result, "not declared");
                ws.untouched();
            } else {
                assert_eq!(content(&result), [b"genuine-source-content\n".to_vec()]);
                assert!(!errors(&result).contains("MUST_NOT_ANALYZE_ESTABLISHER"));
            }
        }
    }
    // Cross-package exports must also grant the macro's definition authority.
    let ws = Workspace::new(defs, "");
    ws.file("app/data", "public source");
    ws.file(
        "app/BUILD.bazel",
        &format!(
            "{LOAD}exports_files(['data'],visibility=['//visibility:public'])\nm(name='demo')\n"
        ),
    );
    assert_eq!(
        content(&ws.run(&["build", "//app:demo"])),
        [b"public source".to_vec()]
    );
}

#[test]
fn symbolic_macro_factory_owner_is_initializing_file_not_callback_lexical_file() {
    for grant in ["defs", "factory"] {
        let ws = Workspace::new("load('//factory:factory.bzl','make')\nm=make()\n", "");
        ws.file("factory/BUILD.bazel", "");
        ws.file(
            "factory/factory.bzl",
            r#"
def _impl(name,visibility):
    native.filegroup(name=name,srcs=[':secret'],visibility=visibility)
def make():
    return macro(implementation=_impl)
"#,
        );
        ws.file("app/BUILD.bazel", &format!("{LOAD}genrule(name='secret',outs=['secret.out'],cmd='printf authority > $@',visibility=['//{grant}:__pkg__'])\nm(name='demo')\n"));
        let result = ws.run(&["build", "//app:demo"]);
        if grant == "defs" {
            assert_eq!(content(&result), [b"authority".to_vec()]);
        } else {
            rejected(&result, "not visible");
            ws.untouched();
        }
    }
}

#[test]
fn symbolic_macro_canonical_repository_identity_is_not_package_text() {
    let ws = Workspace::new("", "");
    ws.file("MODULE.bazel", "module(name='macro_fixture')\nbazel_dep(name='dep',version='1')\nlocal_path_override(module_name='dep',path='repos/dep')\n");
    ws.file("repos/dep/MODULE.bazel", "module(name='dep',version='1')\n");
    ws.file("repos/dep/defs/BUILD.bazel", "");
    ws.file(
        "repos/dep/defs/macros.bzl",
        r#"
def _impl(name,visibility):
    if name == 'consume':
        native.filegroup(name=name,srcs=[':ordinary'],visibility=visibility)
    else:
        native.genrule(name=name+'_private',outs=[name+'_private.out'],cmd='printf external-owner > $@')
        native.filegroup(name=name,srcs=[':'+name+'_private'],visibility=visibility)
m=macro(implementation=_impl)
"#,
    );
    ws.file("defs/BUILD.bazel", "load('@dep//defs:macros.bzl','m')\nm(name='demo',visibility=['//visibility:public'])\nm(name='consume')\ngenrule(name='ordinary',outs=['ordinary.out'],cmd='printf main-owner > $@')\nfilegroup(name='bad',srcs=[':demo_private'])\nfilegroup(name='good',srcs=[':demo'])\n");
    for target in ["//defs:bad", "//defs:consume"] {
        rejected(&ws.run(&["build", target]), "not visible");
        ws.untouched();
    }
    assert_eq!(
        content(&ws.run(&["build", "//defs:good"])),
        [b"external-owner".to_vec()]
    );
}

#[test]
fn symbolic_macro_namespace_checks_selected_targets_not_all_artifacts() {
    for roots in [vec![":outside"], vec![":demo", ":outside"]] {
        let ws = Workspace::new(
            "def _impl(name,visibility):\n    native.genrule(name=name,outs=['outside'],cmd='printf outside-content > $@')\nm=macro(implementation=_impl)\n",
            &format!("{LOAD}m(name='demo')\n"),
        );
        for pattern in ["//:all", "//...", "//:demo", "//:outside"] {
            success(&ws.run(&["query", pattern]));
        }
        ws.untouched();
        let mut args = vec!["build"];
        args.extend(roots);
        rejected(&ws.run(&args), "macro");
        ws.untouched();
        let result = ws.run(&["build", ":demo"]);
        assert_eq!(content(&result), [b"outside-content".to_vec()]);
        assert!(errors(&result).contains("1 actions executed, 0 cache hits"));
    }
    let ws = Workspace::new(
        "def _impl(name,visibility):\n    native.genrule(name='outside',outs=[name+'.out'],cmd='printf forbidden > $@')\nm=macro(implementation=_impl)\n",
        &format!("{LOAD}m(name='demo')\n"),
    );
    for pattern in ["//:all", "//:outside", "//:demo.out"] {
        success(&ws.run(&["query", pattern]));
    }
    rejected(&ws.run(&["build", ":demo.out"]), "macro");
    ws.untouched();
    let ws = Workspace::new(
        r#"
def _write(ctx):
    out=ctx.actions.declare_file('outside_dynamic')
    ctx.actions.write(out,'dynamic-content')
    return [DefaultInfo(files=depset([out]))]
write=rule(implementation=_write)
def _impl(name,visibility):
    write(name=name,visibility=visibility)
m=macro(implementation=_impl)
"#,
        &format!("{LOAD}m(name='demo')\n"),
    );
    rejected(&ws.run(&["query", ":outside_dynamic"]), "not declared");
    assert_eq!(
        content(&ws.run(&["build", ":demo"])),
        [b"dynamic-content".to_vec()]
    );
}

#[test]
fn symbolic_macro_namespace_separators_unselected_invalid_and_ordinary_prefixes() {
    let ws = Workspace::new(
        r#"
def _impl(name,visibility):
    for suffix in ['', '_x', '-x', '.x', '_', '-', '.', 'other']:
        native.genrule(name=name+suffix,outs=[name+suffix+'.out'],cmd='printf namespace > $@')
m=macro(implementation=_impl)
"#,
        &format!(
            "{LOAD}m(name='demo')\ngenrule(name='demoordinary',outs=['ordinary.out'],cmd='printf ordinary > $@')\n"
        ),
    );
    let query = ws.run(&["query", "//:all"]);
    success(&query);
    assert!(String::from_utf8_lossy(&query.stdout).contains("//:demoother\n"));
    for invalid in [":demo_", ":demo-", ":demo.", ":demoother"] {
        rejected(&ws.run(&["build", invalid]), "macro");
        ws.untouched();
    }
    for valid in [":demo", ":demo_x", ":demo-x", ":demo.x"] {
        assert_eq!(content(&ws.run(&["build", valid])), [b"namespace".to_vec()]);
    }
    assert_eq!(
        content(&ws.run(&["build", ":demoordinary"])),
        [b"ordinary".to_vec()]
    );
}

#[test]
fn symbolic_macro_exact_name_collisions_include_empty_macros_outputs_and_exports() {
    for other in [
        "m(name='demo')",
        "filegroup(name='demo')",
        "genrule(name='other',outs=['demo'],cmd='true')",
        "exports_files(['demo'])",
    ] {
        for before in [true, false] {
            let declarations = if before {
                format!("{other}\nm(name='demo')\n")
            } else {
                format!("m(name='demo')\n{other}\n")
            };
            let ws = Workspace::new(ZERO, &format!("{LOAD}{declarations}"));
            ws.file("demo", "physical source");
            rejected(&ws.run(&["query", "//:all"]), "conflict");
            ws.untouched();
        }
    }
    let ws = Workspace::new(
        ZERO,
        &format!("{LOAD}m(name='demo')\nm(name='demo_child')\n"),
    );
    let query = ws.run(&["query", "//:all"]);
    success(&query);
    assert!(query.stdout.is_empty());
}

#[test]
fn symbolic_macro_reserved_name_does_not_become_implicit_source() {
    let ws = Workspace::new(
        ZERO,
        &format!("{LOAD}m(name='demo')\nfilegroup(name='consumer',srcs=['demo'])\n"),
    );
    ws.file("demo", "must not synthesize");
    let query = ws.run(&["query", "//:all"]);
    success(&query);
    assert_eq!(String::from_utf8_lossy(&query.stdout), "//:consumer\n");
    rejected(&ws.run(&["query", ":demo"]), "not declared");
    rejected(&ws.run(&["build", ":consumer"]), "not declared");
    ws.untouched();
}

#[test]
fn symbolic_macro_visibility_is_sorted_typed_and_preserves_duplicates() {
    let ws = Workspace::new(
        &format!(
            r#"
{WRITE}
def typed():
    return (Label('//z:__pkg__'), Label('//a/z:__pkg__'), Label('//a:__subpackages__'), Label('//a:__pkg__'))
def _impl(name,visibility):
    if type(visibility) != 'list':
        fail('visibility must be a list')
    for item in visibility:
        if type(item) != type(Label('//:probe')):
            fail('visibility must contain typed Labels')
    write(name=name,content='|'.join([str(item) for item in visibility]),visibility=visibility)
m=macro(implementation=_impl)
"#
        ),
        "",
    );
    ws.file("app/BUILD.bazel", r#"
load('//defs:macros.bzl','m','typed')
package(default_visibility=['//visibility:public'])
m(name='default_none',visibility=None)
m(name='empty',visibility=[])
m(name='recursive',visibility=['//:__subpackages__'])
m(name='duplicates',visibility=['//z:__pkg__','//app:__pkg__','//visibility:private','//z:__pkg__','//app:__pkg__'])
m(name='public',visibility=['//z:__pkg__','//visibility:public','//visibility:private'])
m(name='typed',visibility=typed())
m(name='external',visibility=['@@dep//z:__pkg__','//z:__pkg__'])
"#);
    for (name, expected) in [
        ("default_none", "@@//visibility:public"),
        ("empty", "@@//app:__pkg__"),
        ("recursive", "@@//:__subpackages__|@@//app:__pkg__"),
        (
            "duplicates",
            "@@//app:__pkg__|@@//app:__pkg__|@@//z:__pkg__|@@//z:__pkg__",
        ),
        ("public", "@@//visibility:public"),
        (
            "typed",
            "@@//a:__pkg__|@@//a:__subpackages__|@@//a/z:__pkg__|@@//app:__pkg__|@@//z:__pkg__",
        ),
        ("external", "@@//app:__pkg__|@@//z:__pkg__|@@dep//z:__pkg__"),
    ] {
        assert_eq!(
            content(&ws.run(&["build", &format!("//app:{name}")])),
            [expected.as_bytes().to_vec()],
            "{name}"
        );
    }
}

#[test]
fn symbolic_macro_visibility_is_frozen_without_freezing_caller_list() {
    let ws = Workspace::new(
        &format!(
            r#"
{WRITE}
def _impl(name,visibility):
    write(name=name,content='|'.join([str(v) for v in visibility]))
m=macro(implementation=_impl)
"#
        ),
        "",
    );
    ws.file("app/BUILD.bazel", &format!("{LOAD}v=[]\nm(name='before',visibility=v)\nv.append('//visibility:public')\nm(name='after',visibility=v)\n"));
    assert_eq!(
        content(&ws.run(&["build", "//app:before"])),
        [b"@@//app:__pkg__".to_vec()]
    );
    assert_eq!(
        content(&ws.run(&["build", "//app:after"])),
        [b"@@//visibility:public".to_vec()]
    );
    let frozen = Workspace::new(
        "def _impl(name,visibility):\n    visibility.append(Label('//z:__pkg__'))\nm=macro(implementation=_impl)\n",
        &format!("{LOAD}m(name='demo')\n"),
    );
    let output = frozen.run(&["query", "//:all"]);
    // Bazel 9.3 Starlark.checkMutable text (Starlark.java:262-265; docs/STARLARK_VENDOR.md).
    rejected(&output, "trying to mutate a frozen list value");
    frozen.untouched();
}

#[test]
fn symbolic_macro_late_package_default_does_not_retroactively_publish_macro() {
    let ws = Workspace::new(
        &format!(
            r#"
{WRITE}
def _impl(name,visibility):
    if visibility != [Label('//app:__pkg__')]:
        fail('macro visibility must snapshot the call')
    write(name=name+'_private',content='private')
    write(name=name,content='snapshot',visibility=visibility)
m=macro(implementation=_impl)
"#
        ),
        "",
    );
    ws.file("app/BUILD.bazel", &format!("{LOAD}genrule(name='ordinary',outs=['ordinary.out'],cmd='printf ordinary > $@')\nm(name='demo')\npackage(default_visibility=['//visibility:public'])\nfilegroup(name='bad_local',srcs=[':demo_private'])\n"));
    ws.file("other/BUILD.bazel", "filegroup(name='ordinary',srcs=['//app:ordinary'])\nfilegroup(name='macro',srcs=['//app:demo'])\n");
    for target in ["//other:macro", "//app:bad_local"] {
        rejected(&ws.run(&["build", target]), "not visible");
        ws.untouched();
    }
    assert_eq!(
        content(&ws.run(&["build", "//other:ordinary"])),
        [b"ordinary".to_vec()]
    );
    assert_eq!(
        content(&ws.run(&["build", "//app:demo"])),
        [b"snapshot".to_vec()]
    );
}

#[test]
fn symbolic_macro_constructor_and_invocation_validate_before_callback() {
    for constructor in [
        "macro()",
        "macro(_impl)",
        "macro(implementation='not a function')",
        "macro(implementation=str)",
        "macro(implementation=len)",
        "macro(implementation=provider(fields=[]))",
        "macro(implementation=rule(implementation=_rule))",
        "macro(implementation=_impl,attrs={'name':None})",
        "macro(implementation=_impl,attrs={'visibility':None})",
    ] {
        let ws = Workspace::new(
            &format!(
                "def _impl(name,visibility):\n    fail('CALLBACK_RAN')\ndef _rule(ctx):\n    return []\nm={constructor}\n"
            ),
            &format!("{LOAD}m(name='demo')\n"),
        );
        let result = ws.run(&["query", "//:all"]);
        rejected(&result, "macro");
        assert!(
            !errors(&result).contains("CALLBACK_RAN"),
            "{constructor}: {}",
            errors(&result)
        );
        ws.untouched();
    }
    for invocation in [
        "m()",
        "m('demo')",
        "m(name=None)",
        "m(name=1)",
        "m(name='')",
        "m(name='../bad')",
        "m(name='demo',unknown=None)",
        "m(name='demo',visibility='//visibility:public')",
        "m(name='demo',visibility=[1])",
    ] {
        let ws = Workspace::new(
            "def _impl(name,visibility):\n    fail('CALLBACK_RAN')\nm=macro(implementation=_impl)\n",
            &format!("{LOAD}{invocation}\n"),
        );
        let result = ws.run(&["query", "//:all"]);
        rejected(&result, "");
        assert!(
            !errors(&result).contains("CALLBACK_RAN"),
            "{invocation}: {}",
            errors(&result)
        );
        ws.untouched();
    }
    for option in [
        "attrs={'value':attr.string()}",
        "inherit_attrs='common'",
        "finalizer=True",
    ] {
        let ws = Workspace::new(
            &format!(
                "def _impl(name,visibility):\n    pass\nm=macro(implementation=_impl,{option})\n"
            ),
            &format!("{LOAD}m(name='demo')\n"),
        );
        rejected(&ws.run(&["query", "//:all"]), "unsupported");
        ws.untouched();
    }
}

#[test]
fn symbolic_macro_constructor_export_and_module_loading_phases() {
    let ws = Workspace::new(
        "def _impl(name,visibility):\n    native.filegroup(name=name,visibility=visibility)\n_private=macro(implementation=_impl,attrs={'removed':None})\nm=_private\ndef make():\n    return macro(implementation=_impl)\n",
        &format!("{LOAD}m(name='demo')\n"),
    );
    success(&ws.run(&["query", ":demo"]));
    ws.file("BUILD.bazel", "load('//defs:macros.bzl','make')\nmake()\n");
    rejected(&ws.run(&["query", "//:all"]), "initialization");
    ws.untouched();
    for source in [
        "def _impl(name,visibility):\n    pass\nheld=[macro(implementation=_impl)]\ndef m(name):\n    held[0](name=name)\n",
        "def _impl(name,visibility):\n    pass\nheld={'macro':macro(implementation=_impl)}\ndef m(name):\n    held['macro'](name=name)\n",
    ] {
        let ws = Workspace::new(source, &format!("{LOAD}m(name='demo')\n"));
        rejected(&ws.run(&["query", "//:all"]), "export");
        ws.untouched();
    }
    let init = Workspace::new(&format!("{ZERO}m(name='init')\n"), LOAD);
    rejected(&init.run(&["query", "//:all"]), "macro");
    init.untouched();
    for file in ["BUILD.bazel", "MODULE.bazel"] {
        let ws = Workspace::new("", "");
        ws.file(file, "macro()\n");
        // Preserve external coverage as well as local root-recording rejection.
        let target = if file == "MODULE.bazel" {
            "@missing//:all"
        } else {
            "//:all"
        };
        let result = ws.run(&["query", target]);
        assert!(
            !result.status.success(),
            "macro constructor exposed in {file}"
        );
        rejected(&result, "macro");
        ws.untouched();
        if file == "MODULE.bazel" {
            for command in ["query", "build"] {
                rejected(&ws.run(&[command, "//:all"]), "macro");
                ws.untouched();
            }
        }
    }
    // A MODULE-loaded .bzl gets the constructor too, without fake SDK globals.
    let ws = Workspace::new(
        r#"
def _macro(name,visibility):
    pass
m=macro(implementation=_macro)
def _repo(ctx):
    ctx.file('BUILD.bazel', "exports_files(['data'])\n")
    ctx.file('data', 'module-loaded-macro\n', executable=False)
make=repository_rule(implementation=_repo)
"#,
        "filegroup(name='use',srcs=['@made//:data'])\n",
    );
    ws.file("MODULE.bazel", "module(name='macro_fixture')\nmake=use_repo_rule('//defs:macros.bzl','make')\nmake(name='made')\n");
    assert_eq!(
        content(&ws.run(&["build", ":use"])),
        [b"module-loaded-macro\n".to_vec()]
    );
}

#[test]
fn symbolic_macro_non_none_nested_and_native_phase_failures_never_execute() {
    for body in ["    return 7\n", "    nested(name=name+'_nested')\n"] {
        let ws = Workspace::new(
            &format!(
                "def _zero(name,visibility):\n    pass\nnested=macro(implementation=_zero)\ndef _impl(name,visibility):\n    native.genrule(name=name,outs=[name+'.out'],cmd='printf forbidden > $@')\n{body}m=macro(implementation=_impl)\n"
            ),
            "",
        );
        ws.file(
            "BUILD.bazel",
            &format!("{LOAD}{}m(name='demo')\n", ws.marker_rule()),
        );
        let result = ws.run(&["build", ":first", ":demo"]);
        rejected(
            &result,
            if body.contains("nested") {
                "unsupported"
            } else {
                "None"
            },
        );
        ws.untouched();
    }
    for native_call in [
        "native.glob(['missing'])",
        "native.package(default_visibility=['//visibility:public'])",
        "native.exports_files(['data'])",
    ] {
        for helper in [false, true] {
            let call = if helper { "legacy()" } else { native_call };
            let ws = Workspace::new(
                &format!(
                    "load('//helpers:legacy.bzl','legacy')\ndef _impl(name,visibility):\n    {call}\nm=macro(implementation=_impl)\n"
                ),
                "",
            );
            ws.file("helpers/BUILD.bazel", "");
            ws.file(
                "helpers/legacy.bzl",
                &format!("def legacy():\n    {native_call}\n"),
            );
            ws.file("data", "source");
            ws.file(
                "BUILD.bazel",
                &format!("{LOAD}{}m(name='demo')\n", ws.marker_rule()),
            );
            let result = ws.run(&["build", ":first"]);
            // exports_files is an explicit D subset limit, not upstream phase parity.
            rejected(
                &result,
                if native_call.contains("exports_files") {
                    "unsupported"
                } else {
                    "macro"
                },
            );
            assert!(
                !errors(&result).contains("matched no files"),
                "glob read before phase guard"
            );
            ws.untouched();
        }
    }
}
