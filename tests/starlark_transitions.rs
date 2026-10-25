//! CLI coverage for N18c, Starlark 1:1 configuration transitions
//! (docs/CONFIGURATION.md "Starlark transitions (N18c)"): rule (`cfg = t`)
//! and attribute (`attr.label(cfg = t)`) transitions on Starlark build
//! settings and the modelled native options, their output roots,
//! default-valued outputs, label settings, `select()` in the rule
//! transition's `attr`, idempotency, the function transition allowlist,
//! every evaluation/validation error and the explicit gaps (split results,
//! unmodelled native options), and a shape of rules_go's transitions with
//! the genuine `transition.bzl`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        Self::with_module("module(name = 'main')\n", files)
    }

    fn with_module(module: &str, files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n18c-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", module);
        for (path, content) in BASE.iter().chain(files) {
            ws.file(path, content);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, path: &str, content: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
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

    /// `LABEL\tPATH-BELOW-THE-ACTION-OUTPUTS` lines, the concatenated
    /// output contents and stderr of a successful `build ARGS`.
    fn built(&self, args: &[&str]) -> (Vec<String>, String, String) {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let mut lines = Vec::new();
        let mut contents = String::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (label, path) = line.split_once('\t').unwrap();
            if let Ok(text) = fs::read_to_string(path) {
                contents.push_str(&text);
            }
            let relative = path
                .split_once("/outputs/")
                .map_or(path, |(_, relative)| relative);
            lines.push(format!("{label}\t{relative}"));
        }
        (lines, contents, errors(&output))
    }

    fn rejected(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output on failure");
        errors(&output).replace(&self.path().display().to_string(), "WS")
    }
}

fn errors(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Startup notice, not a diagnostic (crate::memory_limit).
    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr + "\n"
    }
}

/// The `bazel-out/<mnemonic>` root of a built line.
fn root(line: &str) -> &str {
    let path = line.split_once('\t').unwrap().1;
    let rest = path.strip_prefix("bazel-out/").unwrap();
    &rest[..rest.find('/').unwrap()]
}

const ALLOWLIST: &str = r#"package_group(name = "function_transition_allowlist", packages = ["public"])
"#;

const BASE: &[(&str, &str)] = &[
    (
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        ALLOWLIST,
    ),
    (
        "os/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "osx", constraint_setting = ":os")
"#,
    ),
    (
        "p/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
platform(name = "linux", constraint_values = ["//os:linux"])
platform(name = "mac", constraint_values = ["//os:osx"])
"#,
    ),
    (
        "f/BUILD.bazel",
        r#"load("//r:flags.bzl", "bool_flag", "int_flag", "string_flag", "string_list_flag")
package(default_visibility = ["//visibility:public"])
string_flag(name = "color", build_setting_default = "blue")
string_flag(name = "target", build_setting_default = "t-default", scope = "target")
int_flag(name = "n", build_setting_default = 0)
bool_flag(name = "b", build_setting_default = False)
string_list_flag(name = "l", build_setting_default = [])
label_flag(name = "lf", build_setting_default = ":dfl")
filegroup(name = "dfl", srcs = ["dfl.txt"])
filegroup(name = "other", srcs = ["other.txt"])
alias(name = "color_alias", actual = ":color")
alias(name = "color_select", actual = select({":dbg": ":color", "//conditions:default": ":color"}))
config_setting(name = "red", flag_values = {":color": "red"})
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
"#,
    ),
    ("f/dfl.txt", "dfl\n"),
    ("f/other.txt", "other\n"),
    ("r/BUILD.bazel", ""),
    (
        "r/flags.bzl",
        r#"FlagInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagInfo(value = ctx.build_setting_value)]

bool_flag = rule(implementation = _flag_impl, build_setting = config.bool(flag = True))
int_flag = rule(implementation = _flag_impl, build_setting = config.int(flag = True))
string_list_flag = rule(implementation = _flag_impl, build_setting = config.string_list(flag = True))
string_flag = rule(
    implementation = _flag_impl,
    build_setting = config.string(flag = True),
    attrs = {"scope": attr.string(default = "universal")},
)
"#,
    ),
    (
        "r/defs.bzl",
        r#"load(":flags.bzl", "FlagInfo")

ALLOW = {"_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")}
FLAGS = {
    "_color": attr.label(default = "//f:color"),
    "_n": attr.label(default = "//f:n"),
    "_lf": attr.label(default = "//f:lf"),
}

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    deps = []
    for dep in ctx.attr.deps:
        deps.extend([f.path for f in dep[DefaultInfo].files.to_list()])
    ctx.actions.write(out, "%s color=%s n=%s lf=%s deps=%s\n" % (
        ctx.label.name,
        ctx.attr._color[FlagInfo].value,
        ctx.attr._n[FlagInfo].value,
        ctx.attr._lf.label,
        deps,
    ))
    return [DefaultInfo(files = depset([out]))]

def show_rule(cfg = None, deps_cfg = None, extra = {}):
    attrs = dict(ALLOW) if cfg or deps_cfg else {}
    attrs["deps"] = attr.label_list(cfg = deps_cfg, allow_files = True) if deps_cfg else attr.label_list(allow_files = True)
    attrs.update(FLAGS)
    attrs.update(extra)
    return rule(_show_impl, cfg = cfg, attrs = attrs)

def _red_dbg_impl(settings, attr):
    return {"//f:color": "red-" + attr.name, "//command_line_option:compilation_mode": "dbg"}

red_dbg = transition(
    implementation = _red_dbg_impl,
    inputs = [],
    outputs = ["//f:color", "//command_line_option:compilation_mode"],
)

def _defaults_impl(settings, attr):
    return {"//f:color": "blue", "//command_line_option:compilation_mode": settings["//command_line_option:compilation_mode"]}

defaults = transition(
    implementation = _defaults_impl,
    inputs = ["//command_line_option:compilation_mode"],
    outputs = ["//f:color", "//command_line_option:compilation_mode"],
)

def _inc_impl(settings, attr):
    return {"//f:n": settings["//f:n"] + 1}

inc = transition(implementation = _inc_impl, inputs = ["//f:n"], outputs = ["//f:n"])

def _set_n_impl(settings, attr):
    return {"//f:n": 7}

set_n = transition(implementation = _set_n_impl, inputs = [], outputs = ["//f:n"])

show = show_rule()
red_dbg_rule = show_rule(cfg = red_dbg)
defaults_rule = show_rule(cfg = defaults)
inc_rule = show_rule(cfg = inc)
set_n_rule = show_rule(cfg = set_n)
red_dbg_deps = show_rule(deps_cfg = red_dbg)
"#,
    ),
    ("e/BUILD.bazel", ""),
];

/// A rule transition writing a Starlark flag and `--compilation_mode`: the
/// target and its dependencies are analyzed in the transitioned
/// configuration, under `k8-dbg-ST-<hash>` (the ST hash covers the
/// Starlark flag, compilation_mode is explicit in the path).
#[test]
fn rule_transition_on_a_starlark_flag_and_compilation_mode() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "red_dbg_rule", "show")
red_dbg_rule(name = "a", deps = [":dep"])
show(name = "dep")
"#,
    )]);
    let (lines, contents, _) = ws.built(&["//:a"]);
    let mnemonic = root(&lines[0]);
    assert!(mnemonic.starts_with("k8-dbg-ST-"), "{lines:?}");
    assert_eq!(mnemonic.len(), "k8-dbg-ST-".len() + 12);
    assert_eq!(
        contents,
        format!("a color=red-a n=0 lf=@@//f:dfl deps=[\"bazel-out/{mnemonic}/bin/dep.txt\"]\n")
    );
    // Requested on its own, the dependency stays in the top-level root.
    let (lines, contents, _) = ws.built(&["//:dep", "-c", "opt"]);
    assert_eq!(root(&lines[0]), "k8-opt");
    assert_eq!(contents, "dep color=blue n=0 lf=@@//f:dfl deps=[]\n");
    // The ST hash depends on the values (another top-level -c gives the
    // same transitioned configuration: compilation_mode is written).
    let (lines, _, _) = ws.built(&["//:a", "-c", "opt"]);
    assert_eq!(root(&lines[0]), mnemonic);
}

/// A transition that writes the current values (defaults) is a no-op:
/// outputs equal to a setting's default are removed (StarlarkTransition.validate),
/// so the root and action keys are those of the top-level configuration.
#[test]
fn default_writing_transition_keeps_the_top_level_configuration() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "defaults_rule")
defaults_rule(name = "x")
"#,
    )]);
    let (lines, contents, _) = ws.built(&["//:x"]);
    assert_eq!(root(&lines[0]), "k8-fastbuild");
    assert_eq!(contents, "x color=blue n=0 lf=@@//f:dfl deps=[]\n");
    let (lines, _, _) = ws.built(&["//:x", "-c", "dbg"]);
    assert_eq!(root(&lines[0]), "k8-dbg");
    // The same rule without the transition reuses the action: identical keys.
    ws.file(
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "show")
show(name = "x")
"#,
    );
    let output = ws.run(&["build", "//:x"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains("0 actions executed, 1 cache hits"),
        "{}",
        errors(&output)
    );
}

/// An outgoing 1:1 attribute transition: every dependency of the attribute
/// is analyzed in the transitioned configuration; a source file has no
/// configuration but the transition is still evaluated; `attr.label` with a
/// Starlark transition is a list of Targets in ctx.attr.
#[test]
fn attribute_transition_configures_dependencies() {
    let ws = Workspace::new(&[
        (
            "BUILD.bazel",
            r#"load("//r:defs.bzl", "red_dbg_deps", "show")
load("//e:edge.bzl", "edge")
red_dbg_deps(name = "b", deps = [":dep", "src.txt"])
show(name = "dep")
edge(name = "single", dep = ":dep")
edge(name = "none")
"#,
        ),
        ("src.txt", "src\n"),
        (
            "e/edge.bzl",
            r#"load("//r:defs.bzl", "ALLOW", "red_dbg")

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s %s\n" % (type(ctx.attr.dep), [t.label for t in ctx.attr.dep], [f.path for f in ctx.files.dep]))
    return [DefaultInfo(files = depset([out]))]

edge = rule(_impl, attrs = dict(ALLOW, dep = attr.label(cfg = red_dbg)))
"#,
        ),
    ]);
    let (lines, contents, _) = ws.built(&["//:b"]);
    assert_eq!(root(&lines[0]), "k8-fastbuild");
    let mnemonic = contents
        .split("deps=[\"bazel-out/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    assert!(mnemonic.starts_with("k8-dbg-ST-"), "{contents}");
    assert_eq!(
        contents,
        format!(
            "b color=blue n=0 lf=@@//f:dfl deps=[\"bazel-out/{mnemonic}/bin/dep.txt\", \"src.txt\"]\n"
        )
    );
    let (_, contents, _) = ws.built(&["//:none"]);
    assert_eq!(contents, "list [] []\n");
    let (_, contents, _) = ws.built(&["//:single"]);
    let single = contents
        .strip_prefix("list [Label(\"//:dep\")] [\"bazel-out/k8-dbg-ST-")
        .unwrap_or_else(|| panic!("{contents}"));
    assert!(single.ends_with("/bin/dep.txt\"]\n"), "{contents}");
    // Another rule name is another transitioned configuration.
    assert_ne!(&single[..12], &mnemonic["k8-dbg-ST-".len()..]);
}

/// Label-typed settings: a Label value, or a string parsed in the
/// transition's package context (the defining `.bzl` file's package, so
/// `:x` is relative to it; FunctionTransitionUtil.java:617-635).
#[test]
fn label_flag_outputs_take_labels_and_strings() {
    let ws = Workspace::new(&[
        (
            "BUILD.bazel",
            r#"load("//e:lf.bzl", "by_label", "by_string", "relative")
by_label(name = "l")
by_string(name = "s")
relative(name = "rel")
"#,
        ),
        (
            "e/lf.bzl",
            r#"load("//r:defs.bzl", "show_rule")

def _label(settings, attr):
    return {"//f:lf": Label("//f:other")}

def _string(settings, attr):
    return {"//f:lf": "//f:other"}

def _relative(settings, attr):
    return {"//f:lf": ":x"}

by_label = show_rule(cfg = transition(implementation = _label, inputs = [], outputs = ["//f:lf"]))
by_string = show_rule(cfg = transition(implementation = _string, inputs = [], outputs = ["//f:lf"]))
relative = show_rule(cfg = transition(implementation = _relative, inputs = [], outputs = ["//f:lf"]))
"#,
        ),
        (
            "e/BUILD.bazel",
            "filegroup(name = 'x', visibility = ['//visibility:public'])\n",
        ),
    ]);
    let (lines, contents, _) = ws.built(&["//:l", "//:s"]);
    assert_eq!(
        contents,
        "l color=blue n=0 lf=@@//f:other deps=[]\ns color=blue n=0 lf=@@//f:other deps=[]\n"
    );
    // Equal values give the same configuration.
    assert_eq!(root(&lines[0]), root(&lines[1]));
    assert!(root(&lines[0]).starts_with("k8-fastbuild-ST-"));
    let (_, contents, _) = ws.built(&["//:rel"]);
    assert_eq!(contents, "rel color=blue n=0 lf=@@//e:x deps=[]\n");
}

/// A rule transition whose second application changes the configuration
/// again (NON_IDEMPOTENT) is analyzed once, in the first transitioned
/// configuration, without applying the transition again
/// (ConfiguredTargetKey.shouldApplyRuleTransition = false).
#[test]
fn non_idempotent_rule_transition_applies_once() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "inc_rule", "show")
inc_rule(name = "count", deps = [":dep"])
show(name = "dep")
show(name = "parent", deps = [":count"])
"#,
    )]);
    let (lines, contents, _) = ws.built(&["//:count"]);
    let mnemonic = root(&lines[0]).to_owned();
    assert!(mnemonic.starts_with("k8-fastbuild-ST-"));
    assert_eq!(
        contents,
        format!("count color=blue n=1 lf=@@//f:dfl deps=[\"bazel-out/{mnemonic}/bin/dep.txt\"]\n")
    );
    let (_, contents, _) = ws.built(&["//:count", "--//f:n=5"]);
    assert!(contents.starts_with("count color=blue n=6 "), "{contents}");
    // A dependent in the top-level configuration sees the delegate.
    let (_, contents, _) = ws.built(&["//:parent"]);
    assert_eq!(
        contents,
        format!(
            "parent color=blue n=0 lf=@@//f:dfl deps=[\"bazel-out/{mnemonic}/bin/count.txt\"]\n"
        )
    );
}

/// An idempotent rule transition: the target requested at top level and as
/// a dependency is one configured target with one action.
#[test]
fn idempotent_rule_transition_does_not_duplicate_actions() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "set_n_rule", "show")
set_n_rule(name = "seven")
show(name = "a", deps = [":seven"])
show(name = "b", deps = [":seven"])
"#,
    )]);
    let output = ws.run(&["build", "//:seven", "//:a", "//:b"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains("3 actions executed, 0 cache hits"),
        "{}",
        errors(&output)
    );
    let (_, contents, _) = ws.built(&["//:a"]);
    assert!(contents.contains("/bin/seven.txt"), "{contents}");
}

/// The rule transition's `attr` holds raw values; a select() is resolved in
/// the requested configuration unless a key reads a setting the transition
/// writes, then the attribute is absent with Bazel's message
/// (StarlarkRuleTransitionProvider.java:131-206). getattr defaults work.
#[test]
fn select_in_rule_transition_attributes() {
    let ws = Workspace::new(&[
        (
            "BUILD.bazel",
            r#"load("//e:sel.bzl", "reads_color", "reads_mode")
reads_color(name = "c", mode = select({"//f:red": "x", "//conditions:default": "y"}))
reads_mode(name = "m", mode = select({"//f:dbg": "dbg-branch", "//conditions:default": "other"}))
reads_mode(name = "plain", mode = "given")
"#,
        ),
        (
            "e/sel.bzl",
            r#"load("//r:defs.bzl", "show_rule")

def _color(settings, attr):
    return {"//f:color": getattr(attr, "mode", "absent")}

def _mode(settings, attr):
    return {"//f:color": attr.mode + "-" + getattr(attr, "nope", "dflt")}

reads_color = show_rule(cfg = transition(implementation = _color, inputs = [], outputs = ["//f:color"]), extra = {"mode": attr.string()})
reads_mode = show_rule(cfg = transition(implementation = _mode, inputs = [], outputs = ["//f:color"]), extra = {"mode": attr.string()})
"#,
        ),
    ]);
    let (_, contents, _) = ws.built(&["//:c", "//:plain"]);
    assert_eq!(
        contents,
        "c color=absent n=0 lf=@@//f:dfl deps=[]\nplain color=given-dflt n=0 lf=@@//f:dfl deps=[]\n"
    );
    let (_, contents, _) = ws.built(&["//:m", "-c", "dbg"]);
    assert_eq!(
        contents,
        "m color=dbg-branch-dflt n=0 lf=@@//f:dfl deps=[]\n"
    );
    // Reading the omitted attribute directly.
    ws.file(
        "e/sel.bzl",
        r#"load("//r:defs.bzl", "show_rule")

def _color(settings, attr):
    return {"//f:color": attr.mode}

def _mode(settings, attr):
    return {}

reads_color = show_rule(cfg = transition(implementation = _color, inputs = [], outputs = ["//f:color"]), extra = {"mode": attr.string()})
reads_mode = reads_color
"#,
    );
    let error = ws.rejected(&["//:c"]);
    assert!(
        error.contains("No attribute 'mode'. Either this attribute does not exist for this rule or the attribute was not resolved because it is set by a select that reads flags the transition may set."),
        "{error}"
    );
    assert!(
        error.ends_with(
            "WS/BUILD.bazel:2:12: Errors encountered while applying Starlark transition\n"
        ),
        "{error}"
    );
}

/// Rule transition errors: an error event at the transition (validation,
/// conversion) or implementation (result shape) location, then
/// `Errors encountered while applying Starlark transition` at the rule; type
/// errors of Starlark settings and settings loading errors are the
/// TransitionException text at the rule.
#[test]
fn rule_transition_errors_follow_bazel() {
    let ws = Workspace::new(&[
        (
            "e/errors.bzl",
            r#"load("//r:defs.bzl", "show_rule")

def _returns_int(settings, attr):
    return 1

def _undeclared(settings, attr):
    return {"//f:color": "x", "//f:other": "y"}

def _missing(settings, attr):
    return {"//f:color": "x"}

def _int_for_string(settings, attr):
    return {"//f:color": 1}

def _int_for_label(settings, attr):
    return {"//f:lf": 1}

def _bad_label(settings, attr):
    return {"//f:lf": "//f:bad:label"}

def _none_platforms(settings, attr):
    return {"//command_line_option:platforms": None}

def _bool_mode(settings, attr):
    return {"//command_line_option:compilation_mode": True}

def _bad_mode(settings, attr):
    return {"//command_line_option:compilation_mode": "fast"}

def _empty(settings, attr):
    return {}

def _copt(settings, attr):
    return {"//command_line_option:copt": ["-O2"]}

def _fails(settings, attr):
    fail("boom")

def _two(settings, attr):
    return [{"//f:color": "a"}, {"//f:color": "b"}]

def _one(settings, attr):
    return [{"//f:color": "only"}]

def _int_list(settings, attr):
    return {"//f:l": [1]}

def _bool_for_int(settings, attr):
    return {"//f:b": 2}

def _rule(impl, inputs, outputs):
    return show_rule(cfg = transition(implementation = impl, inputs = inputs, outputs = outputs))

returns_int = _rule(_returns_int, [], ["//f:color"])
undeclared = _rule(_undeclared, [], ["//f:color"])
missing = _rule(_missing, [], ["//f:color", "//f:n", "//f:b"])
int_for_string = _rule(_int_for_string, [], ["//f:color"])
int_for_label = _rule(_int_for_label, [], ["//f:lf"])
bad_label = _rule(_bad_label, [], ["//f:lf"])
none_platforms = _rule(_none_platforms, [], ["//command_line_option:platforms"])
bool_mode = _rule(_bool_mode, [], ["//command_line_option:compilation_mode"])
bad_mode = _rule(_bad_mode, [], ["//command_line_option:compilation_mode"])
unknown_option = _rule(_empty, ["//command_line_option:nope"], [])
non_configurable = _rule(_empty, [], ["//command_line_option:check_visibility"])
define = _rule(_empty, [], ["//command_line_option:define"])
copt = _rule(_copt, [], ["//command_line_option:copt"])
not_a_setting = _rule(_empty, ["//f:dfl"], [])
missing_target = _rule(_empty, ["//f:nothere"], [])
alias_select = _rule(_empty, ["//f:color_select"], [])
fails = _rule(_fails, [], [])
two = _rule(_two, [], ["//f:color"])
one = _rule(_one, [], ["//f:color"])
int_list = _rule(_int_list, [], ["//f:l"])
bool_for_int = _rule(_bool_for_int, [], ["//f:b"])
"#,
        ),
        (
            "BUILD.bazel",
            r#"load("//e:errors.bzl", "returns_int", "undeclared", "missing", "int_for_string", "int_for_label", "bad_label", "none_platforms", "bool_mode", "bad_mode", "unknown_option", "non_configurable", "define", "copt", "not_a_setting", "missing_target", "alias_select", "fails", "two", "one", "int_list", "bool_for_int")
returns_int(name = "returns_int")
undeclared(name = "undeclared")
missing(name = "missing")
int_for_string(name = "int_for_string")
int_for_label(name = "int_for_label")
bad_label(name = "bad_label")
none_platforms(name = "none_platforms")
bool_mode(name = "bool_mode")
bad_mode(name = "bad_mode")
unknown_option(name = "unknown_option")
non_configurable(name = "non_configurable")
define(name = "define")
copt(name = "copt")
not_a_setting(name = "not_a_setting")
missing_target(name = "missing_target")
alias_select(name = "alias_select")
fails(name = "fails")
two(name = "two")
one(name = "one")
int_list(name = "int_list")
bool_for_int(name = "bool_for_int")
"#,
        ),
    ]);
    let event = |line: usize,
                 column: usize,
                 message: &str,
                 rule_line: usize,
                 rule_column: usize| {
        format!(
            "bazel-rs: WS/e/errors.bzl:{line}:{column}: {message}\nWS/BUILD.bazel:{rule_line}:{rule_column}: Errors encountered while applying Starlark transition\n"
        )
    };
    let at_rule = |rule_line: usize, rule_column: usize, message: &str| {
        format!("bazel-rs: WS/BUILD.bazel:{rule_line}:{rule_column}: {message}\n")
    };
    // RegularTransition.evaluate errors are at the implementation (`def`
    // name); validation and conversion errors at the transition() call.
    assert_eq!(
        ws.rejected(&["//:returns_int"]),
        event(
            3,
            5,
            "transition function returned int, want dict or list of dicts",
            2,
            12
        )
    );
    assert_eq!(
        ws.rejected(&["//:undeclared"]),
        event(
            6,
            5,
            "invalid result from transition function: transition function returned undeclared output '//f:other'",
            3,
            11
        )
    );
    assert_eq!(
        ws.rejected(&["//:missing"]),
        event(
            9,
            5,
            "invalid result from transition function: transition outputs [//f:b,//f:n] were not defined by transition function",
            4,
            8
        )
    );
    assert_eq!(
        ws.rejected(&["//:int_for_string"]),
        at_rule(
            5,
            15,
            "expected value of type 'string' for //f:color, but got 1 (int)"
        )
    );
    // _rule's transition() call: line 52, column 38 (its left parenthesis).
    let transition_event = |message: &str, rule_line: usize, rule_column: usize| {
        event(52, 38, message, rule_line, rule_column)
    };
    assert_eq!(
        ws.rejected(&["//:int_for_label"]),
        transition_event(
            "Invalid value type for option '//f:lf': want label, got int",
            6,
            14
        )
    );
    // Bazel's label syntax text (LabelParser), paired capture A08/bad_label.
    assert_eq!(
        ws.rejected(&["//:bad_label"]),
        transition_event(
            "Error parsing value for option '//f:lf': invalid target name 'bad:label': target names may not contain ':'",
            7,
            10
        )
    );
    assert_eq!(
        ws.rejected(&["//:none_platforms"]),
        transition_event(
            "'None' value not allowed for List-type option 'platforms'. Please use '[]' instead if trying to set option to empty value.",
            8,
            15
        )
    );
    assert_eq!(
        ws.rejected(&["//:bool_mode"]),
        transition_event("Invalid value type for option 'compilation_mode'", 9, 10)
    );
    assert_eq!(
        ws.rejected(&["//:bad_mode"]),
        transition_event(
            "OptionsParsingError for option 'compilation_mode': Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)",
            10,
            9
        )
    );
    assert_eq!(
        ws.rejected(&["//:unknown_option"]),
        transition_event(
            "transition inputs [//command_line_option:nope] do not correspond to valid settings",
            11,
            15
        )
    );
    assert_eq!(
        ws.rejected(&["//:non_configurable"]),
        transition_event(
            "transition outputs [//command_line_option:check_visibility] cannot be changed: they are non-configurable",
            12,
            17
        )
    );
    assert_eq!(
        ws.rejected(&["//:define"]),
        transition_event(
            "Starlark transition on --define not supported - try using build settings (https://bazel.build/rules/config#user-defined-build-settings).",
            13,
            7
        )
    );
    assert_eq!(
        ws.rejected(&["//:copt"]),
        at_rule(
            14,
            5,
            "Starlark transition on '//command_line_option:copt' is not implemented in the native profile (modelled native options: compilation_mode, host_compilation_mode, stamp, strip, force_pic, platforms, host_platform, extra_toolchains, cpu, collect_code_coverage, java_runtime_version, java_language_version, tool_java_runtime_version, tool_java_language_version; read only: define, copt)"
        )
    );
    assert_eq!(
        ws.rejected(&["//:not_a_setting"]),
        at_rule(
            15,
            14,
            "attempting to transition on '//f:dfl' which is not a build setting"
        )
    );
    let error = ws.rejected(&["//:missing_target"]);
    assert!(
        error.starts_with("bazel-rs: WS/BUILD.bazel:16:15: com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//f:nothere'"),
        "{error}"
    );
    assert_eq!(
        ws.rejected(&["//:alias_select"]),
        at_rule(
            17,
            13,
            "attempting to transition on aliased build setting '//f:color_select', the actual value of which uses select(). Aliased build settings with configured actual values is not supported."
        )
    );
    let error = ws.rejected(&["//:fails"]);
    assert!(error.contains("boom"), "{error}");
    assert!(
        error.ends_with(
            "WS/BUILD.bazel:18:6: Errors encountered while applying Starlark transition\n"
        ),
        "{error}"
    );
    assert_eq!(
        ws.rejected(&["//:two"]),
        event(
            45,
            5,
            "Rule transition only allowed to return a single transitioned configuration.",
            19,
            4
        )
        .replace("errors.bzl:45:5", "errors.bzl:52:38")
    );
    let (_, contents, _) = ws.built(&["//:one"]);
    assert_eq!(contents, "one color=only n=0 lf=@@//f:dfl deps=[]\n");
    assert_eq!(
        ws.rejected(&["//:int_list"]),
        at_rule(
            21,
            9,
            "expected value of type 'string' for element 0 of //f:l, but got 1 (int)"
        )
    );
    assert_eq!(
        ws.rejected(&["//:bool_for_int"]),
        at_rule(
            22,
            13,
            "expected one of [False, True, 0, 1] for //f:b, but got 2 (int)"
        )
    );
}

/// Attribute transition failures are reported as dependency edge errors
/// (DependencyProducer.getMessageWithEdgeTransitionInfo); split results are
/// N18d and fail explicitly.
#[test]
fn attribute_transition_errors_name_the_edge() {
    let ws = Workspace::new(&[
        (
            "e/edges.bzl",
            r#"load("//r:defs.bzl", "show_rule")

def _int_for_string(settings, attr):
    return {"//f:color": 1}

def _split(settings, attr):
    return {"a": {"//f:color": "a"}, "b": {"//f:color": "b"}}

def _one_split(settings, attr):
    return [{"//f:color": "a"}]

int_edge = show_rule(deps_cfg = transition(implementation = _int_for_string, inputs = [], outputs = ["//f:color"]))
split_edge = show_rule(deps_cfg = transition(implementation = _split, inputs = [], outputs = ["//f:color"]))
one_split_edge = show_rule(deps_cfg = transition(implementation = _one_split, inputs = [], outputs = ["//f:color"]))
"#,
        ),
        (
            "BUILD.bazel",
            r#"load("//e:edges.bzl", "int_edge", "split_edge", "one_split_edge")
load("//r:defs.bzl", "show")
int_edge(name = "i", deps = [":dep"])
split_edge(name = "s", deps = [":dep"])
one_split_edge(name = "o", deps = [":dep"])
show(name = "dep")
"#,
        ),
    ]);
    let top = "25220f3";
    assert_eq!(
        ws.rejected(&["//:i"]),
        format!(
            "bazel-rs: WS/BUILD.bazel:3:9: on dependency edge //:i ({top}) -|deps|-> //:dep: expected value of type 'string' for //f:color, but got 1 (int)\n"
        )
    );
    for (target, line, count) in [("s", 4, 2), ("o", 5, 1)] {
        let column = if target == "s" { 11 } else { 15 };
        assert_eq!(
            ws.rejected(&[&format!("//:{target}")]),
            format!(
                "bazel-rs: WS/BUILD.bazel:{line}:{column}: on dependency edge //:{target} ({top}) -|deps|-> //:dep: a Starlark transition that returns {count} configurations (a split transition) is not implemented in the native profile (N18d)\n"
            )
        );
    }
}

/// The function transition allowlist: a declared allowlist package_group is
/// checked after the implementation (INSTANCE or DEFINITION location); its
/// declaration is validated by rule(); an undeclared one is added with the
/// `@bazel_tools` default, which needs the built-in module profile.
#[test]
fn function_transition_allowlist() {
    let ws = Workspace::new(&[
        (
            "tools/allowlists/function_transition_allowlist/BUILD.bazel",
            r#"package_group(name = "function_transition_allowlist", packages = ["//allowed/..."])
"#,
        ),
        (
            "allowed/defs.bzl",
            r#"load("//r:defs.bzl", "red_dbg", "show_rule")
defined_here = show_rule(cfg = red_dbg)
"#,
        ),
        (
            "allowed/BUILD.bazel",
            r#"load("//r:defs.bzl", "red_dbg_rule")
red_dbg_rule(name = "ok")
"#,
        ),
        (
            "denied/BUILD.bazel",
            r#"load("//r:defs.bzl", "red_dbg_rule")
load("//allowed:defs.bzl", "defined_here")
red_dbg_rule(name = "no")
defined_here(name = "by_definition")
"#,
        ),
    ]);
    ws.built(&["//allowed:ok", "//denied:by_definition"]);
    assert_eq!(
        ws.rejected(&["//denied:no"]),
        "bazel-rs: WS/denied/BUILD.bazel:3:13: in red_dbg_rule rule //denied:no: Non-allowlisted use of Starlark transition\n"
    );
    // rule() validation of a declared allowlist attribute.
    for (attrs, message) in [
        (
            "{'_allowlist_function_transition': attr.string(default = 'x')}",
            "_allowlist_function_transition attribute must be a label type",
        ),
        (
            "{'_allowlist_function_transition': attr.label()}",
            "_allowlist_function_transition attribute must have a default value",
        ),
        (
            "{'_allowlist_function_transition': attr.label(default = '//tools:other')}",
            "_allowlist_function_transition attribute (//tools:other) does not have the expected value //tools/allowlists/function_transition_allowlist:function_transition_allowlist",
        ),
    ] {
        ws.file(
            "e/bad.bzl",
            &format!(
                "load('//r:defs.bzl', 'red_dbg')\ndef _impl(ctx):\n    return []\nbad = rule(_impl, cfg = red_dbg, attrs = {attrs})\n"
            ),
        );
        ws.file(
            "e/BUILD.bazel",
            "load(':bad.bzl', 'bad')\nbad(name = 'x')\n",
        );
        let error = ws.rejected(&["//e:x"]);
        assert!(error.contains(message), "{error}");
    }
    ws.file(
        "e/bad.bzl",
        "def _impl(ctx):\n    return []\nbad = rule(_impl, attrs = {'_allowlist_function_transition': attr.label(default = '//tools/allowlists/function_transition_allowlist')})\n",
    );
    let error = ws.rejected(&["//e:x"]);
    assert!(
        error.contains("Unused function-based split transition allowlist: //e:bad.bzl NORMAL"),
        "{error}"
    );
    // The implicit allowlist (cfg = exec, @bazel_tools default).
    ws.file(
        "e/bad.bzl",
        "load('//r:defs.bzl', 'red_dbg')\ndef _impl(ctx):\n    return [DefaultInfo(files = depset([]))]\nbad = rule(_impl, cfg = red_dbg)\n",
    );
    let error = ws.rejected(&["//e:x", "--host_platform=//p:linux"]);
    assert_eq!(
        error,
        "bazel-rs: WS/e/BUILD.bazel:2:4: the Starlark transition allowlist @@bazel_tools//tools/allowlists/function_transition_allowlist:function_transition_allowlist of //e:x needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide; drop --experimental-builtin-modules=false, or declare `_allowlist_function_transition = attr.label(default = \"//tools/allowlists/function_transition_allowlist\")` with a package_group of that name\n"
    );
}

fn extract_platforms(to: &Path) {
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/label_repo_name/platforms_archive.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = to.join(entry.path().unwrap());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&path).unwrap();
            continue;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// With the built-in module profile, the implicit allowlist is the genuine
/// `@bazel_tools` package_group (`packages = ["public"]`), analyzed in the
/// exec configuration of the default host platform.
#[test]
fn implicit_allowlist_with_builtin_modules() {
    let ws = Workspace::with_module(
        "module(name = 'main')\nbazel_dep(name = 'platforms', version = '1.0.0')\nlocal_path_override(module_name = 'platforms', path = 'platforms')\n",
        &[
            (
                "e/implicit.bzl",
                r#"load("//r:defs.bzl", "red_dbg")
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "allowlist=%s\n" % ctx.attr._allowlist_function_transition.label)
    return [DefaultInfo(files = depset([out]))]
implicit = rule(_impl, cfg = red_dbg)
"#,
            ),
            (
                "e/BUILD.bazel",
                "load(':implicit.bzl', 'implicit')\nimplicit(name = 'x')\n",
            ),
        ],
    );
    extract_platforms(&ws.path().join("platforms"));
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let (lines, contents, _) = ws.built(&["--experimental-builtin-modules", &registry, "//e:x"]);
    assert!(root(&lines[0]).starts_with("k8-dbg-ST-"), "{lines:?}");
    assert_eq!(
        contents,
        "allowlist=@@bazel_tools//tools/allowlists/function_transition_allowlist:function_transition_allowlist\n"
    );
}

/// A transition writing `--platforms` changes the target platform of the
/// transitioned configuration (select() on constraints, toolchain
/// resolution); a transitioned configuration's exec edges use the
/// top-level exec baseline and drop `scope = "target"` flags the transition
/// wrote (so its exec root equals the top-level one's).
#[test]
fn platforms_and_exec_edges_below_a_transition() {
    let ws = Workspace::new(&[
        (
            "e/plat.bzl",
            r#"load("//r:defs.bzl", "ALLOW")

def _to_mac(settings, attr):
    return {"//command_line_option:platforms": ":mac", "//f:target": "set"}

to_mac = transition(implementation = _to_mac, inputs = [], outputs = ["//command_line_option:platforms", "//f:target"])

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "os=%s tool=%s\n" % (ctx.attr.os, [f.path for f in ctx.files._tool]))
    return [DefaultInfo(files = depset([out]))]

plat = rule(_impl, cfg = to_mac, attrs = dict(ALLOW, os = attr.string(), _tool = attr.label(default = "//e:tool", cfg = "exec")))
"#,
        ),
        (
            "e/BUILD.bazel",
            r#"load(":plat.bzl", "plat")
package(default_visibility = ["//visibility:public"])
platform(name = "mac", constraint_values = ["//os:osx"])
genrule(name = "tool", outs = ["tool.sh"], cmd = "echo > $@")
plat(name = "x", os = select({"//os:osx": "osx", "//os:linux": "linux"}))
"#,
        ),
    ]);
    let (lines, contents, _) = ws.built(&["--host_platform=//p:linux", "//e:x"]);
    assert!(root(&lines[0]).starts_with("k8-fastbuild-ST-"), "{lines:?}");
    assert_eq!(
        contents,
        "os=osx tool=[\"bazel-out/linux-opt-exec/bin/e/tool.sh\"]\n"
    );
}

/// rules_go's genuine `go/private/rules/transition.bzl` (Apache-2.0, copied
/// as test data with tests/fixtures/starlark_transitions/PROVENANCE.md)
/// with go_library/go_binary/go_context_data/nogo-shaped rules: srcs/data/
/// embedsrcs/cdeps with `non_go_transition`, coverdata with
/// `non_request_nogo_transition`, a nogo-shaped rule with
/// `go_tool_transition` and a binary with `go_transition`.
#[test]
fn rules_go_shaped_transitions() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/starlark_transitions/go_shaped");
    let skylib = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_settings/skylib");
    let mut files = Vec::new();
    collect(&fixture, &fixture, &mut files);
    let owned = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let ws = Workspace::with_module(
        &format!(
            "module(name = 'go_shaped')\nbazel_dep(name = 'bazel_skylib', version = '1.8.2')\nlocal_path_override(module_name = 'bazel_skylib', path = '{}')\n",
            skylib.display()
        ),
        &owned,
    );
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let profile = [
        "--experimental-builtin-modules",
        &registry,
        "--host_platform=//p:linux",
    ];
    let (lines, contents, _) = ws.built(&[&profile[..], &["//app:bin", "//app:lib"]].concat());
    let expected = fs::read_to_string(fixture.join("expected.txt")).unwrap();
    let mut actual = String::new();
    for line in &lines {
        actual.push_str(&format!("{}\n", line.split('\t').next().unwrap()));
    }
    actual.push_str(&contents);
    // Configuration hashes are the native ST- digests.
    assert_eq!(normalize(&actual), expected.trim_end().to_owned() + "\n");
    // With race enabled on the command line, go_binary's go_transition
    // records the original value and non_go_transition restores it.
    let (_, contents, _) = ws.built(&[&profile[..], &["//app:bin", "--//go/config:race"]].concat());
    assert!(contents.contains("bin race=True"), "{contents}");
    assert!(contents.contains("data race=True"), "{contents}");
    // nogo's go_tool_transition resets the Go settings and sets
    // bootstrap_nogo; static/pure keep the command-line value.
    let (lines, contents, _) =
        ws.built(&[&profile[..], &["//go:nogo_actual", "--//go/config:pure"]].concat());
    assert!(root(&lines[0]).starts_with("k8-fastbuild-ST-"), "{lines:?}");
    assert!(
        contents.contains("\nnogo_actual race=False pure=True linkmode=normal tags=[] request_nogo=False bootstrap_nogo=True original_race=\"\"\n"),
        "{contents}"
    );
    // rules_go's own go_library declaration: its `srcs` extension list
    // (library.bzl:89-91) is analyzed since N27; the prerequisites of the
    // transitioned `srcs` are validated in their edge configuration.
    let shaped = fs::read_to_string(ws.path().join("go/private/rules/shaped.bzl")).unwrap();
    ws.file(
        "go/private/rules/shaped.bzl",
        &shaped.replacen(
            "\"srcs\": attr.label_list(allow_files = True, cfg = non_go_transition),\n        \"deps\": attr.label_list(providers = [GoInfo]),\n        \"importpath\"",
            "\"srcs\": attr.label_list(allow_files = [\".go\", \".s\"], cfg = non_go_transition),\n        \"deps\": attr.label_list(providers = [GoInfo]),\n        \"importpath\"",
            1,
        ),
    );
    let (lines, _, _) = ws.built(&[&profile[..], &["//app:lib"]].concat());
    assert!(
        lines.iter().any(|line| line.starts_with("//app:lib\t")),
        "{lines:?}"
    );
}

fn collect(base: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(base, &path, files);
        } else if path.file_name().is_some_and(|name| name != "expected.txt") {
            files.push((
                path.strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

/// Replace `ST-<12 hex>` with `ST-#`.
fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find("-ST-") {
        out.push_str(&rest[..index + 4]);
        out.push('#');
        rest = &rest[index + 4 + 12..];
    }
    out.push_str(rest);
    out
}

/// Paired-capture fixes (tests/fixtures/starlark_transitions/reference_receipt.json):
/// an alias and its actual in one transition, reading through an alias
/// (which resets the actual), the native option value types, `--define`,
/// `--copt` and `--cpu`, `None` for non-list options, print() replay and the
/// `Available attributes` list.
#[test]
fn settings_details_values_and_replay_follow_bazel() {
    let ws = Workspace::new(&[
        (
            "e/fix.bzl",
            r#"load("//r:defs.bzl", "show_rule")

def _read_alias(settings, attr):
    return {"//f:color": "alias=%r" % settings["//f:color_alias"]}

def _alias_into_n(settings, attr):
    return {"//f:n": len(settings["//f:color_alias"])}

def _types(settings, attr):
    return {"//f:color": "%s %s %s %s %r %r" % (
        type(settings["//command_line_option:compilation_mode"]),
        type(settings["//command_line_option:host_compilation_mode"]),
        type(settings["//command_line_option:strip"]),
        type(settings["//command_line_option:cpu"]),
        settings["//command_line_option:define"],
        settings["//command_line_option:copt"],
    )}

def _cpu(settings, attr):
    return {"//command_line_option:cpu": "arm"}

def _none_mode(settings, attr):
    return {"//command_line_option:compilation_mode": None}

def _none_stamp(settings, attr):
    return {"//command_line_option:stamp": None}

def _printing(settings, attr):
    print("printing", attr.name)
    return {"//f:color": "p"}

def _missing(settings, attr):
    return {"//f:color": attr.nope}

def _t(impl, inputs = [], outputs = ["//f:color"]):
    return transition(implementation = impl, inputs = inputs, outputs = outputs)

read_alias = show_rule(cfg = _t(_read_alias, ["//f:color_alias"]))
alias_into_n = show_rule(cfg = _t(_alias_into_n, ["//f:color_alias"], ["//f:n"]))
types = show_rule(cfg = _t(_types, [
    "//command_line_option:compilation_mode",
    "//command_line_option:host_compilation_mode",
    "//command_line_option:strip",
    "//command_line_option:cpu",
    "//command_line_option:define",
    "//command_line_option:copt",
]))
cpu = show_rule(cfg = _t(_cpu, [], ["//command_line_option:cpu"]))
none_mode = show_rule(cfg = _t(_none_mode, [], ["//command_line_option:compilation_mode"]))
none_stamp = show_rule(cfg = _t(_none_stamp, [], ["//command_line_option:stamp"]))
printing = show_rule(cfg = _t(_printing))
missing = show_rule(deps_cfg = _t(_missing))
"#,
        ),
        (
            "e/BUILD.bazel",
            r#"load(":fix.bzl", "alias_into_n", "cpu", "missing", "none_mode", "none_stamp", "printing", "read_alias", "types")
read_alias(name = "read_alias")
alias_into_n(name = "alias_into_n")
types(name = "types")
cpu(name = "cpu")
none_mode(name = "none_mode")
none_stamp(name = "none_stamp")
printing(name = "printing")
missing(name = "missing", deps = [":printing"])
"#,
        ),
    ]);
    // An alias and its actual are both referenced: the actual is seen twice
    // (StarlarkBuildSettingsDetailsFunction.getBuildSettingPackages).
    assert_eq!(
        ws.rejected(&["//e:read_alias", "--//f:color=red"]),
        "bazel-rs: WS/e/BUILD.bazel:2:11: Dependency cycle involving '//f:color' detected in aliased build settings\n"
    );
    // Reading through an alias sees the default and resets the actual to
    // it (addDefaultStarlarkOptions + unalias + validate): the transitioned
    // configuration has the default color with or without --//f:color=red
    // (its ST hash then differs, against a top-level baseline with
    // color=red, as Bazel's 6fa34fb87cd8 / b80645a7f898).
    let (plain, contents, _) = ws.built(&["//e:alias_into_n"]);
    assert_eq!(
        contents,
        "alias_into_n color=blue n=4 lf=@@//f:dfl deps=[]\n"
    );
    let (set, contents, _) = ws.built(&["//e:alias_into_n", "--//f:color=red"]);
    assert_eq!(
        contents,
        "alias_into_n color=blue n=4 lf=@@//f:dfl deps=[]\n"
    );
    assert_ne!(root(&plain[0]), root(&set[0]));
    // Native option values: enum class names, the host CPU, empty lists.
    let (_, contents, _) = ws.built(&["//e:types"]);
    assert!(
        contents.starts_with("types color=CompilationMode CompilationMode StripMode string [] [] "),
        "{contents}"
    );
    // A non-empty --define is unreadable (a Guava ImmutableList).
    for (args, class) in [
        (&["--define=a=b"][..], "SingletonImmutableList"),
        (
            &["--define=a=b", "--define=c=d"][..],
            "RegularImmutableList",
        ),
    ] {
        let mut command = args.to_vec();
        command.push("//e:types");
        assert_eq!(
            ws.rejected(&command),
            format!(
                "bazel-rs: WS/e/fix.bzl:9:5: before calling _types: Input build setting //command_line_option:define is of type class com.google.common.collect.{class}, which is unreadable in Starlark. Please submit a feature request.\nWS/e/BUILD.bazel:4:6: Errors encountered while applying Starlark transition\n"
            )
        );
    }
    // --cpu is explicit in the output directory (Bazel `arm-fastbuild`).
    let (lines, _, _) = ws.built(&["//e:cpu"]);
    assert_eq!(root(&lines[0]), "arm-fastbuild");
    // None for an enum option is its default; for a boolean field Bazel's
    // reflective IllegalArgumentException.
    let (lines, _, _) = ws.built(&["//e:none_mode"]);
    assert_eq!(root(&lines[0]), "k8-fastbuild");
    assert_eq!(
        ws.rejected(&["//e:none_stamp"]),
        "bazel-rs: WS/e/fix.bzl:36:22: IllegalArgumentError for option 'stamp': Can not set boolean field com.google.devtools.build.lib.analysis.config.CoreOptions.stampBinaries to null value\nWS/e/BUILD.bazel:7:11: Errors encountered while applying Starlark transition\n"
    );
    // Every application prints (a cache hit replays the output): the rule
    // transition, its idempotency check and the delegate's application.
    let (_, _, stderr) = ws.built(&["//e:printing"]);
    assert_eq!(stderr.matches("printing printing").count(), 3, "{stderr}");
    // StructImpl's unknown-field message lists every attribute of the rule.
    let error = ws.rejected(&["//e:missing"]);
    assert!(
        error.contains("No attribute 'nope' in attr. Make sure you declared a rule attribute with this name.\nAvailable attributes: _action_listener, _allowlist_function_transition, _color, _config_dependencies, _lf, _n, aspect_hints, compatible_with, deprecation, deps, exec_compatible_with, exec_group_compatible_with, exec_properties, expect_failure, features, generator_function, generator_location, generator_name, name, package_metadata, restricted_to, tags, target_compatible_with, testonly, toolchains, transitive_configs, visibility\n"),
        "{error}"
    );
}

/// N28b's JavaOptions version strings in Starlark transitions: read as
/// strings, written as given, selectable after the transition, part of the
/// transitioned identity (an ST hash; StringConverter options are not
/// explicit in the path); a non-string value is Bazel's invalid type.
#[test]
fn java_version_options_in_transitions() {
    let ws = Workspace::new(&[
        (
            "j/defs.bzl",
            r#"load("//r:defs.bzl", "ALLOW")

def _jdk_impl(settings, attr):
    print("READ", settings["//command_line_option:java_runtime_version"], repr(settings["//command_line_option:java_language_version"]), settings["//command_line_option:tool_java_runtime_version"])
    return {
        "//command_line_option:java_runtime_version": "remotejdk_17",
        "//command_line_option:java_language_version": "17",
    }

jdk17 = transition(
    implementation = _jdk_impl,
    inputs = [
        "//command_line_option:java_runtime_version",
        "//command_line_option:java_language_version",
        "//command_line_option:tool_java_runtime_version",
    ],
    outputs = [
        "//command_line_option:java_runtime_version",
        "//command_line_option:java_language_version",
    ],
)

def _bad_impl(settings, attr):
    return {"//command_line_option:java_runtime_version": 17}

bad = transition(implementation = _bad_impl, inputs = [], outputs = ["//command_line_option:java_runtime_version"])

def _r_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.v + "\n")
    return [DefaultInfo(files = depset([out]))]

ATTRS = dict(ALLOW)
ATTRS["v"] = attr.string()
jdk_rule = rule(_r_impl, cfg = jdk17, attrs = ATTRS)
bad_rule = rule(_r_impl, cfg = bad, attrs = ATTRS)
"#,
        ),
        (
            "j/BUILD.bazel",
            r#"load(":defs.bzl", "bad_rule", "jdk_rule")
config_setting(name = "jdk17", values = {"java_runtime_version": "remotejdk_17", "java_language_version": "17"})
V = select({":jdk17": "jdk17", "//conditions:default": "other"})
jdk_rule(name = "j", v = V)
bad_rule(name = "bad", v = V)
"#,
        ),
    ]);
    let (lines, contents, stderr) = ws.built(&["//j:j"]);
    assert!(
        stderr.contains("READ local_jdk \"\" remotejdk_11"),
        "{stderr}"
    );
    assert_eq!(contents, "jdk17\n");
    let mnemonic = root(&lines[0]);
    assert!(mnemonic.starts_with("k8-fastbuild-ST-"), "{lines:?}");
    // Writing the values the configuration already has is a no-op.
    let (lines, contents, _) = ws.built(&[
        "//j:j",
        "--java_runtime_version=remotejdk_17",
        "--java_language_version=17",
    ]);
    assert_eq!(root(&lines[0]), "k8-fastbuild");
    assert_eq!(contents, "jdk17\n");
    assert!(
        ws.rejected(&["//j:bad"])
            .contains("Invalid value type for option 'java_runtime_version'"),
    );
}
