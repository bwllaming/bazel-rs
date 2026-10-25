//! `attr.string_keyed_label_dict`, `attr.label_list_dict`, computed (function)
//! defaults and `analysis_test_transition` through the CLI (docs/ATTR_DICTS_COMPUTED_DEFAULTS.md).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("attr-dicts-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main')\n");
        f
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
            ])
            .args(args)
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }

    /// The genuine built-in profile with the captured offline registry.
    fn run_builtin(&self, command: &str, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let mut all = vec!["--experimental-builtin-modules", registry.as_str()];
        all.extend_from_slice(args);
        self.run(command, &all)
    }

    fn built(&self, output: &Output) -> String {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        fs::read_to_string(path).unwrap()
    }

    fn rejected(&self, output: &Output, message: &str) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stderr = stderr
            .lines()
            .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
            .collect::<Vec<_>>()
            .join("\n");
        let stderr = if stderr.is_empty() {
            stderr
        } else {
            stderr + "\n"
        };
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty(), "{stderr}");
        assert!(
            stderr.contains(message),
            "expected {message:?} in:\n{stderr}"
        );
        stderr
    }
}

const DICT_RULES: &str = r#"
Info = provider(fields = ["v"])

def _leaf(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name + "\n")
    return [DefaultInfo(files = depset([out])), Info(v = ctx.label.name)]

leaf = rule(implementation = _leaf)

def _impl(ctx):
    lines = []
    for key, target in ctx.attr.m.items():
        lines.append("m %s=%s %s" % (key, target[Info].v, target.label))
    for key, targets in ctx.attr.l.items():
        lines.append("l %s=%s" % (key, ",".join([t[Info].v for t in targets])))
    lines.append("files.m=" + ",".join([f.basename for f in ctx.files.m]))
    lines.append("files.l=" + ",".join([f.basename for f in ctx.files.l]))
    lines.append("types=%s,%s,%s" % (type(ctx.attr.m), type(ctx.attr.l), type(ctx.attr.l.values()[0]) if ctx.attr.l else "-"))
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

r = rule(implementation = _impl, attrs = {
    "m": attr.string_keyed_label_dict(providers = [Info]),
    "l": attr.label_list_dict(default = {"d": ["//:a"]}),
    "src": attr.string_keyed_label_dict(allow_files = True),
    "req": attr.label_list_dict(allow_empty = False),
})
"#;

#[test]
fn label_dict_attributes_analyze_dependencies_and_resolve_selects() {
    let f = Fixture::new();
    f.file("defs.bzl", DICT_RULES);
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "leaf", "r")
leaf(name = "a")
leaf(name = "b")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
r(name = "t", m = {"x": ":a", "y": "//:b", "z": ":a"}, l = {"p": [":b", ":a"], "q": []}, req = {"k": [":a"]})
r(name = "d", req = {"k": [":b"]})
r(name = "s", l = select({":opt": {"k": [":a"]}, "//conditions:default": {"k": [":b"]}}), req = {"k": []})
"#,
    );
    assert_eq!(
        f.built(&f.run("build", &["//:t"])),
        "m x=a @@//:a\nm y=b @@//:b\nm z=a @@//:a\nl p=b,a\nl q=\nfiles.m=a.txt,b.txt\nfiles.l=b.txt,a.txt\ntypes=dict,dict,list\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["//:d"])),
        "l d=a\nfiles.m=\nfiles.l=a.txt\ntypes=dict,dict,list\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["//:s"])),
        "l k=b\nfiles.m=\nfiles.l=b.txt\ntypes=dict,dict,list\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["--compilation_mode=opt", "//:s"])),
        "l k=a\nfiles.m=\nfiles.l=a.txt\ntypes=dict,dict,list\n"
    );
}

#[test]
fn label_dict_attributes_report_bazel_loading_and_analysis_errors() {
    let f = Fixture::new();
    f.file("defs.bzl", DICT_RULES);
    f.file("data.txt", "data\n");
    for (call, target, message) in [
        (
            "r(name = 't', req = {'k': [':a', ':a']})",
            "//:t",
            "Label '//:a' is duplicated in the 'req' attribute of rule 't'",
        ),
        (
            "r(name = 't', req = {'k': []}, m = {'k': 'data.txt'})",
            "//:t",
            "in m attribute of r rule //:t: source file '//:data.txt' is misplaced here (expected no files)",
        ),
        (
            "r(name = 't', req = {})",
            "//:t",
            "in req attribute of r rule //:t: attribute must be non empty",
        ),
        (
            "filegroup(name = 'plain')\nr(name = 't', req = {'k': []}, m = {'k': ':plain'})",
            "//:t",
            "does not have mandatory providers",
        ),
        (
            "r(name = 't', req = {}, m = select({'//conditions:default': {'k': 1}}))",
            "//:t",
            "expected value of type 'string' for dict value element, but got 1 (int)",
        ),
    ] {
        f.file(
            "BUILD.bazel",
            &format!("load(':defs.bzl', 'leaf', 'r')\nleaf(name = 'a')\n{call}\n"),
        );
        f.rejected(&f.run("build", &[target]), message);
    }
    // A file is accepted where allow_files permits it.
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'leaf', 'r')\nleaf(name = 'a')\nr(name = 't', req = {'k': []}, src = {'k': 'data.txt'})\n",
    );
    f.built(&f.run("build", &["//:t"]));
}

#[test]
fn repository_rules_and_tags_bind_label_dicts_with_caller_mapping() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        r#"module(name = "main")
overlay = use_repo_rule("//:repo.bzl", "overlay")
overlay(name = "direct", files = {"BUILD.in": "//:overlay.BUILD", "sub/data.txt": ":data.txt"}, groups = {"g": ["//:data.txt", "@main//:overlay.BUILD"], "e": []})
ext = use_extension("//:ext.bzl", "ext")
ext.tag(files = {"BUILD.in": "//:overlay.BUILD"}, groups = {"g": [":data.txt"]})
use_repo(ext, "made")
"#,
    );
    f.file(
        "repo.bzl",
        r#"def _impl(ctx):
    lines = ["%s=%s" % (k, v) for k, v in ctx.attr.files.items()]
    lines += ["%s=%s" % (k, ",".join([str(l) for l in v])) for k, v in ctx.attr.groups.items()]
    lines.append("types=%s,%s,%s" % (type(ctx.attr.files), type(ctx.attr.files.values()[0]), type(ctx.attr.groups)))
    ctx.file("summary.txt", "\n".join(lines) + "\n")
    ctx.file("BUILD.bazel", "exports_files(['summary.txt'])\n")

overlay = repository_rule(implementation = _impl, attrs = {
    "files": attr.string_keyed_label_dict(),
    "groups": attr.label_list_dict(),
})
"#,
    );
    f.file(
        "ext.bzl",
        r#"load(":repo.bzl", "overlay")

def _ext(mctx):
    for mod in mctx.modules:
        for tag in mod.tags.tag:
            print("TAG", type(tag.files), {k: str(v) for k, v in tag.files.items()}, {k: [str(x) for x in v] for k, v in tag.groups.items()})
            overlay(name = "made", files = tag.files, groups = {"g": tag.groups["g"] + [Label("//:overlay.BUILD")], "s": ["//:data.txt"]})

ext = module_extension(implementation = _ext, tag_classes = {"tag": tag_class(attrs = {
    "files": attr.string_keyed_label_dict(),
    "groups": attr.label_list_dict(),
})})
"#,
    );
    f.file(
        "BUILD.bazel",
        "exports_files(['overlay.BUILD', 'data.txt'])\n",
    );
    f.file("overlay.BUILD", "\n");
    f.file("data.txt", "data\n");
    assert_eq!(
        f.built(&f.run("build", &["@direct//:summary.txt"])),
        "BUILD.in=@@//:overlay.BUILD\nsub/data.txt=@@//:data.txt\ng=@@//:data.txt,@@//:overlay.BUILD\ne=\ntypes=dict,Label,dict\n"
    );
    let made = f.run("build", &["@made//:summary.txt"]);
    assert!(
        String::from_utf8_lossy(&made.stderr).contains(
            "TAG dict {\"BUILD.in\": \"@@//:overlay.BUILD\"} {\"g\": [\"@@//:data.txt\"]}"
        ),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    assert_eq!(
        f.built(&made),
        "BUILD.in=@@//:overlay.BUILD\ng=@@//:data.txt,@@//:overlay.BUILD\ns=@@//:data.txt\ntypes=dict,Label,dict\n"
    );
    // Projections are immutable, like Bazel's Dict.immutableCopyOf values.
    f.file(
        "repo.bzl",
        r#"def _impl(ctx):
    ctx.attr.files["x"] = Label("//:data.txt")

overlay = repository_rule(implementation = _impl, attrs = {
    "files": attr.string_keyed_label_dict(),
    "groups": attr.label_list_dict(),
})
"#,
    );
    let output = f.run("build", &["@direct//:summary.txt"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("trying to mutate a frozen dict value"),
        "{stderr}"
    );
}

#[test]
fn bazel_tools_http_rules_load_and_bind_string_keyed_label_dicts() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        r#"module(name = "main")
http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(name = "arch", urls = ["https://example.invalid/a.tar.gz"], files = {"BUILD.bazel": 1}, remote_file_urls = {"REPO.bazel": ["https://example.invalid/REPO.bazel"]})
"#,
    );
    f.file(
        "defs.bzl",
        "load('@bazel_tools//tools/build_defs/repo:http.bzl', 'http_archive', 'http_file')\nprint('HTTP_LOADED', http_archive, http_file)\nloaded = True\n",
    );
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'loaded')\nfilegroup(name = 'f')\n",
    );
    let output = f.run_builtin("build", &["//:f"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "HTTP_LOADED <starlark repository rule @@bazel_tools//tools/build_defs/repo:http.bzl%http_archive>"
    ));
    // Demanding the repository binds `files` (attr.string_keyed_label_dict)
    // before any implementation code runs or network is used.
    f.rejected(
        &f.run_builtin("query", &["@arch//:all"]),
        "in call to 'http_archive' repo rule with name 'arch', expected value of type 'string' for dict value element, but got 1 (int)",
    );
}

#[test]
fn computed_private_label_defaults_follow_rules_cc_def_parser_shape() {
    let f = Fixture::new();
    f.file(
        "semantics.bzl",
        r#"def _def_parser_computed_default(name, tags):
    # This is needed to break the dependency cycle.
    if "__DONT_DEPEND_ON_DEF_PARSER__" in tags or "def_parser" in name:
        return None
    else:
        return Label("//tools:def_parser")

def _get_def_parser():
    return attr.label(
        default = _def_parser_computed_default,
        allow_single_file = True,
    )

def _extra(name, srcs):
    return [Label("//tools:" + s) for s in srcs]

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    parser = ctx.file._def_parser
    ctx.actions.write(out, "parser=%s extra=%s\n" % (parser.basename if parser else None, [f.basename for f in ctx.files._extra]))
    return [DefaultInfo(files = depset([out]))]

cc_like = rule(implementation = _impl, attrs = {
    "srcs": attr.string_list(),
    "_def_parser": _get_def_parser(),
    "_extra": attr.label_list(default = _extra, allow_files = True),
})
"#,
    );
    f.file(
        "tools/BUILD.bazel",
        "exports_files(['def_parser', 'a', 'b'])\n",
    );
    for name in ["def_parser", "a", "b"] {
        f.file(&format!("tools/{name}"), "x\n");
    }
    f.file(
        "BUILD.bazel",
        r#"load(":semantics.bzl", "cc_like")
cc_like(name = "lib", srcs = ["a", "b"])
cc_like(name = "my_def_parser")
cc_like(name = "tagged", tags = ["__DONT_DEPEND_ON_DEF_PARSER__"])
"#,
    );
    assert_eq!(
        f.built(&f.run("build", &["//:lib"])),
        "parser=def_parser extra=[\"a\", \"b\"]\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["//:my_def_parser"])),
        "parser=None extra=[]\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["//:tagged"])),
        "parser=None extra=[]\n"
    );
    // A missing computed dependency is an ordinary missing-edge error.
    fs::remove_file(f.0.path().join("tools/BUILD.bazel")).unwrap();
    f.rejected(&f.run("build", &["//:lib"]), "tools");
    // Bazel's loading errors.
    f.file(
        "bad.bzl",
        "def _impl(ctx):\n    return []\nbad = rule(_impl, attrs = {'_p': attr.label(default = lambda name: '//tools:a')})\n",
    );
    f.file(
        "public.bzl",
        "def _impl(ctx):\n    return []\npublic = rule(_impl, attrs = {'p': attr.label(default = lambda name: None)})\n",
    );
    f.file("BUILD.bazel", "load(':bad.bzl', 'bad')\nbad(name = 'x')\n");
    f.rejected(
        &f.run("build", &["//:x"]),
        "Cannot compute default value of attribute '_p' in rule '//:x': expected 'label', but got 'string'",
    );
    f.file(
        "BUILD.bazel",
        "load(':public.bzl', 'public')\npublic(name = 'x')\n",
    );
    f.rejected(
        &f.run("build", &["//:x"]),
        "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found 'p'",
    );
}

#[test]
fn skylib_unittest_loads_with_analysis_test_transition_and_fails_analysis_explicitly() {
    let f = Fixture::new();
    let skylib = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_settings/skylib");
    assert!(
        skylib.join("lib/unittest.bzl").is_file(),
        "genuine fixture missing"
    );
    f.file(
        "MODULE.bazel",
        &format!(
            "module(name='main')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nlocal_path_override(module_name='bazel_skylib',path='{}')\n",
            skylib.display()
        ),
    );
    f.file(
        "tests.bzl",
        r#"load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")

def _impl(ctx):
    env = analysistest.begin(ctx)
    asserts.true(env, True)
    return analysistest.end(env)

opt_test = analysistest.make(_impl, config_settings = {"//command_line_option:compilation_mode": "opt"})
t = analysis_test_transition(settings = {"//command_line_option:compilation_mode": "dbg"})
print("TRANSITION", t, type(t), AnalysisTestResultInfo)
"#,
    );
    f.file(
        "BUILD.bazel",
        r#"load(":tests.bzl", "opt_test")
filegroup(name = "subject")
opt_test(name = "check", target_under_test = ":subject")
"#,
    );
    let output = f.run_builtin("build", &["//:subject"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "TRANSITION <analysis_test_transition object> transition <function AnalysisTestResultInfo>"
    ));
    // N28c: skylib's unittest toolchain_type now analyzes (its package
    // default metadata is analyzed in the no-config configuration instead
    // of failing as a gap), so the analysis test's toolchain resolution
    // expands every registered toolchain. The built-in fixture registry
    // carries the BCR patches of @rules_shell and the other registered
    // toolchain modules (tests/fixtures/builtin_modules/provenance.json), and
    // rules_java's `@local_jdk` package globs through the JDK's `bin` link
    // since N28d, so resolution reaches `@bazel_tools//tools/test`'s
    // `use_default_test_toolchain` config_setting; N30 models its
    // `use_target_platform_for_tests` option, so analysis reaches the
    // analysis_test gap this test guards.
    let stderr = f.rejected(
        &f.run_builtin("build", &["//:check"]),
        "analysis of //:check: declared rule features accepted at load time but not analyzed by the native profile: rule(analysis_test=True)",
    );
    assert!(
        !stderr.contains("use_target_platform_for_tests"),
        "{stderr}"
    );
    assert!(!stderr.contains("unittest.bzl"), "{stderr}");
    // analysis_test_transition is restricted to analysis_test rules.
    f.file(
        "misuse.bzl",
        "def _impl(ctx):\n    return []\nr = rule(_impl, attrs = {'d': attr.label(cfg = analysis_test_transition(settings = {}))})\n",
    );
    f.file("BUILD.bazel", "load(':misuse.bzl', 'r')\nr(name = 'x')\n");
    f.rejected(
        &f.run_builtin("build", &["//:x"]),
        "Only rule definitions with analysis_test=True may have attributes with analysis_test_transition transitions",
    );
}

#[test]
fn rule_context_attribute_values_are_frozen_like_bazel() {
    let f = Fixture::new();
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    if ctx.attr.mode == "dict":
        ctx.attr.m["x"] = ctx.attr.m["x"]
    elif ctx.attr.mode == "list":
        ctx.attr.l["p"].append(ctx.attr.l["p"][0])
    elif ctx.attr.mode == "strings":
        ctx.attr.s.append("x")
    elif ctx.attr.mode == "tags":
        ctx.attr.tags.append("x")
    elif ctx.attr.mode == "files":
        ctx.files.l.append(None)
    elif ctx.attr.mode == "string_dict":
        ctx.attr.sd["a"] = "b"
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "ok\n")
    return [DefaultInfo(files = depset([out]))]

r = rule(implementation = _impl, attrs = {
    "mode": attr.string(),
    "m": attr.string_keyed_label_dict(),
    "l": attr.label_list_dict(),
    "s": attr.string_list(),
    "sd": attr.string_dict(),
})
"#,
    );
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'r')\nfilegroup(name = 'a')\n[r(name = m, mode = m, m = {'x': ':a'}, l = {'p': [':a']}, s = ['q'], sd = {'a': 'c'}) for m in ['dict', 'list', 'strings', 'tags', 'files', 'string_dict', 'none']]\n",
    );
    for (target, kind) in [
        ("dict", "dict"),
        ("list", "list"),
        ("strings", "list"),
        ("tags", "list"),
        ("files", "list"),
        ("string_dict", "dict"),
    ] {
        f.rejected(
            &f.run("build", &[&format!("//:{target}")]),
            &format!("trying to mutate a frozen {kind} value"),
        );
    }
    assert_eq!(f.built(&f.run("build", &["//:none"])), "ok\n");
}

#[test]
fn dict_select_union_unknown_repositories_and_all_empty_attributes_follow_bazel() {
    let f = Fixture::new();
    f.file("defs.bzl", DICT_RULES);
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "leaf", "r")
leaf(name = "a")
leaf(name = "b")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
r(name = "u", l = {"k": [":a"]} | select({":opt": {"o": [":b"]}, "//conditions:default": {}}), m = select({"//conditions:default": {"s": ":a"}}) | {"z": ":b"}, req = {"k": []})
x = {"k": [":a"]}
x |= select({"//conditions:default": {"j": [":b"]}})
r(name = "assign", l = x, req = {"k": []})
r(name = "nope", m = {"x": "@nope//:a"}, req = {"k": []})
r(name = "ok", req = {"k": []})
r(name = "empty")
"#,
    );
    assert_eq!(
        f.built(&f.run("build", &["//:u"])),
        "m s=a @@//:a\nm z=b @@//:b\nl k=a\nfiles.m=a.txt,b.txt\nfiles.l=a.txt\ntypes=dict,dict,list\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["--compilation_mode=opt", "//:u"])),
        "m s=a @@//:a\nm z=b @@//:b\nl k=a\nl o=b\nfiles.m=a.txt,b.txt\nfiles.l=a.txt,b.txt\ntypes=dict,dict,list\n"
    );
    assert_eq!(
        f.built(&f.run("build", &["//:assign"])),
        "l k=a\nl j=b\nfiles.m=\nfiles.l=a.txt,b.txt\ntypes=dict,dict,list\n"
    );
    // An apparent repository the main repository does not see fails only the
    // target's dependency edge at analysis (the package still loads).
    let stderr = f.rejected(
        &f.run("build", &["//:nope"]),
        "no such package '@@[unknown repo 'nope' requested from @@]//': The repository '@@[unknown repo 'nope' requested from @@]' could not be resolved: No repository visible as '@nope' from main repository and referenced by '//:nope'",
    );
    assert!(
        stderr.starts_with(
            "bazel-rs: no such package '@@[unknown repo 'nope' requested from @@]//': "
        ),
        "{stderr}"
    );
    f.built(&f.run("build", &["//:ok"]));
    // allow_empty=False reports every empty attribute in attribute order.
    let stderr = f.rejected(
        &f.run("build", &["//:empty"]),
        "in req attribute of r rule //:empty: attribute must be non empty",
    );
    assert!(!stderr.contains("in l attribute"), "{stderr}");
    f.file(
        "nonempty.bzl",
        "def _impl(ctx):\n    return []\nnonempty = rule(_impl, attrs = {'m': attr.string_keyed_label_dict(allow_empty = False), 'l': attr.label_list_dict(allow_empty = False)})\n",
    );
    f.file(
        "both/BUILD.bazel",
        "load('//:nonempty.bzl', 'nonempty')\nnonempty(name = 'ne_unset')\n",
    );
    let stderr = f.rejected(
        &f.run("build", &["//both:ne_unset"]),
        "/both/BUILD.bazel:2:9: in m attribute of nonempty rule //both:ne_unset: attribute must be non empty\n",
    );
    // Attribute errors at the rule's location (genuine Bazel 9.3: both
    // lines at 2:9, tests/fixtures/attr_dicts_computed_defaults/reference_receipt.json).
    assert!(
        stderr.ends_with(
            "/both/BUILD.bazel:2:9: in l attribute of nonempty rule //both:ne_unset: attribute must be non empty\n"
        ),
        "{stderr}"
    );
}

#[test]
fn computed_defaults_reading_builtin_computed_attributes_fail_like_bazel() {
    let f = Fixture::new();
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "ps=" + ",".join([t.label.name for t in ctx.attr._ps]) + "\n")
    return [DefaultInfo(files = depset([out]))]

def _t(name, testonly):
    return []

def _v(name, visibility):
    return [Label("//:nvis_%d" % len(visibility))]

tprobe = rule(implementation = _impl, attrs = {"_ps": attr.label_list(default = _t)})
vprobe = rule(implementation = _impl, attrs = {"_ps": attr.label_list(default = _v)})
"#,
    );
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'tprobe', 'vprobe')\ntprobe(name = 't', testonly = True)\nvprobe(name = 'v', visibility = ['//visibility:public'])\nfilegroup(name = 'nvis_1')\n",
    );
    f.rejected(
        &f.run("build", &["//:t"]),
        "Cannot compute default value of attribute '_ps' in rule '//:t': No such regular (non computed) attribute 'testonly'.\nAvailable attributes: $config_dependencies, compatible_with, exec_compatible_with, exec_group_compatible_with, generator_function, generator_location, generator_name, name, restricted_to, tags, transitive_configs, visibility",
    );
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'vprobe')\nvprobe(name = 'v', visibility = ['//visibility:public'])\nfilegroup(name = 'nvis_1')\n",
    );
    assert_eq!(f.built(&f.run("build", &["//:v"])), "ps=nvis_1\n");
}
