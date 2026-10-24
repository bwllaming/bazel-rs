//! N55: one `Target` value per configured target for the whole analysis.
//! Bazel hands every consumer — and every provider that stores it — the same
//! ConfiguredTarget instance per (label, configuration)
//! (skyframe/ConfiguredTargetKey.java:60; `AbstractConfiguredTarget` has no
//! equals override, so equality is Java identity,
//! analysis/configuredtargets/AbstractConfiguredTarget.java:46,95-97).
//! Natively the engine allocates each configured target's Target data once,
//! keyed as Bazel keys the configured target (src/engine.rs
//! `ActionPlanning`), so identity crosses rule contexts: a Target carried in
//! a dependency's provider equals the consumer's own `ctx.attr` Target, a
//! depset TargetItem snapshot equals the other producer's snapshot, two
//! attributes with the same Starlark transition see one instance, an alias
//! edge and an exec-configured edge stay distinct (an alias is its own
//! AliasConfiguredTarget, rules/AliasConfiguredTarget.java:72,134-144), a
//! source file reached from target- and exec-configured attributes is one
//! null-configuration instance (InputFileConfiguredTarget.java:36-41), two
//! pre-transition configurations converging through a Starlark rule
//! transition see one post-delegation instance
//! (TargetAndConfigurationProducer.java:244-251), and an alias of a
//! rule-transitioned target carries the actual's analysis. `cc_internal.freeze`
//! of a list containing a Target keeps its identity (N49b's explicit error
//! is gone). Expected outputs are genuine Bazel 9.3.0's (paired capture
//! artifacts/target_identity-capture.py, receipt
//! tests/fixtures/target_identity/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/module_archives.rs");

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// cc_internal through a main-repository file on cc_common's private-API
/// allowlist ("", "third_party/bazel_rules/rules_cc"); freeze itself has no
/// restriction (CcStarlarkInternal.java:950-960).
const ALLOW_BZL: &str = r#"cc_internal = cc_common.internal_DO_NOT_USE()
"#;

const DEFS: &str = r#"
HeldInfo = provider(fields = ["t", "l"])
DInfo = provider(fields = ["d"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]

leaf = rule(_leaf_impl)

def _producer_impl(ctx):
    return [
        # The dep Target carried in provider fields: one frozen with the
        # producer's module, one in a depset crossing targets.
        HeldInfo(t = ctx.attr.dep, l = [ctx.attr.dep, "x"]),
        DInfo(d = depset([ctx.attr.dep])),
        DefaultInfo(files = depset()),
    ]

producer = rule(_producer_impl, attrs = {"dep": attr.label()})

def _set_mode(settings, attr):
    return {"//flags:mode": 1}

same_mode = transition(implementation = _set_mode, inputs = [], outputs = ["//flags:mode"])

def _set_other(settings, attr):
    return {"//flags:other": 1}

set_other = transition(implementation = _set_other, inputs = [], outputs = ["//flags:other"])

# A rule transition (rule(cfg = ...)): the switcher is analyzed in the one
# configuration it maps every request to (mode 1, other 0), whatever
# configuration the edge requested (TargetAndConfigurationProducer
# delegates to the transitioned key).
def _fix_impl(settings, attr):
    return {"//flags:mode": 1, "//flags:other": 0}

fix_flags = transition(implementation = _fix_impl, inputs = [], outputs = ["//flags:mode", "//flags:other"])

def _switcher_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]

switcher = rule(_switcher_impl, cfg = fix_flags)

def _consumer_impl(ctx):
    own = ctx.attr.dep
    merged = depset(transitive = [ctx.attr.producer[DInfo].d, ctx.attr.producer2[DInfo].d])
    frozen = _cc.freeze([own, "x"])
    frozen_dict = _cc.freeze({"t": own, "x": 1})
    keyed = {own: 1}
    keyed[ctx.attr.lk.keys()[0]] = 2
    rows = [
        "attr_dict=%s" % (own == ctx.attr.lk.keys()[0]),
        "dict_key=%d" % len(keyed),
        "provider=%s" % (ctx.attr.producer[HeldInfo].t == own),
        "provider_list=%s" % (ctx.attr.producer[HeldInfo].l[0] == own),
        "depset_direct=%s" % (ctx.attr.producer[DInfo].d.to_list()[0] == own),
        "depset_merged=%d" % len(merged.to_list()),
        "same_transition=%s" % (ctx.attr.a == ctx.attr.b),
        "transition_vs_plain=%s" % (own == ctx.attr.a),
        "alias=%s" % (own == ctx.attr.via_alias),
        "exec=%s" % (own == ctx.attr.tool),
        "source_exec=%s" % (ctx.attr.src == ctx.attr.src_tool),
        "rule_transition=%s" % (ctx.attr.converge_a == ctx.attr.converge_b),
        "alias_actual=%s %s" % (
            ctx.attr.via_sw == ctx.attr.dep_plain,
            ctx.attr.via_sw[DefaultInfo].files.to_list()[0].short_path == "switcher.txt",
        ),
        "freeze_type=%s %s" % (type(frozen), type(frozen_dict)),
        "freeze_eq=%s %s" % (frozen == [own, "x"], frozen_dict == {"t": own, "x": 1}),
        "frozen_depset=%d" % len(depset([struct(f = frozen)]).to_list()),
        "frozen_depset_identity=%s" % (depset([struct(f = frozen)]).to_list()[0].f[0] == own),
        "repr=%s" % repr([own, ctx.attr.via_alias, ctx.attr.tool]),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(rows) + "\n")
    return [DefaultInfo(files = depset([out]))]

consumer = rule(_consumer_impl, attrs = {
    "dep": attr.label(),
    "lk": attr.label_keyed_string_dict(),
    "producer": attr.label(),
    "producer2": attr.label(),
    "via_alias": attr.label(),
    "tool": attr.label(cfg = "exec"),
    "a": attr.label(cfg = same_mode),
    "b": attr.label(cfg = same_mode),
    "src": attr.label(allow_files = True),
    "src_tool": attr.label(cfg = "exec", allow_files = True),
    "dep_plain": attr.label(),
    "converge_a": attr.label(cfg = set_other),
    "converge_b": attr.label(cfg = same_mode),
    "via_sw": attr.label(),
})
"#;

/// The negative freeze cases (Bazel's copy is shallow; only a mutable
/// element fails explicitly, N49b).
const BAD_DEFS: &str = r#"
def _bad_impl(ctx):
    kind = ctx.attr.kind
    frozen = _cc.freeze([ctx.attr.dep, "x"])
    if kind == "mutable_element":
        _cc.freeze([ctx.attr.dep, [1]])
    elif kind == "mutate":
        frozen.append("y")
    return []

bad = rule(_bad_impl, attrs = {"dep": attr.label(), "kind": attr.string()})
"#;

const BUILD: &str = r#"load(":defs.bzl", "leaf", "producer", "consumer", "switcher")
leaf(name = "leaf")
alias(name = "alias", actual = ":leaf")
switcher(name = "switcher")
alias(name = "sw_alias", actual = ":switcher")
producer(name = "p1", dep = ":leaf")
producer(name = "p2", dep = ":leaf")
consumer(
    name = "consumer",
    dep = ":leaf",
    lk = {":leaf": "v"},
    producer = ":p1",
    producer2 = ":p2",
    via_alias = ":alias",
    tool = ":leaf",
    a = ":leaf",
    b = ":leaf",
    src = ":data.txt",
    src_tool = ":data.txt",
    dep_plain = ":switcher",
    converge_a = ":switcher",
    converge_b = ":switcher",
    via_sw = ":sw_alias",
)
"#;

const BAD_BUILD: &str = r#"load(':defs.bzl', 'bad')
bad(name = 'bad', dep = ':leaf', kind = 'mutable_element')
bad(name = 'bad_mutate', dep = ':leaf', kind = 'mutate')
"#;

/// The exact rows genuine Bazel 9.3.0 prints (paired capture,
/// `reference_receipt.json` row O/consumer).
const EXPECTED: &str = "attr_dict=True\n\
         dict_key=1\n\
         provider=True\n\
         provider_list=True\n\
         depset_direct=True\n\
         depset_merged=1\n\
         same_transition=True\n\
         transition_vs_plain=False\n\
         alias=False\n\
         exec=False\n\
         source_exec=True\n\
         rule_transition=True\n\
         alias_actual=False True\n\
         freeze_type=list dict\n\
         freeze_eq=True True\n\
         frozen_depset=1\n\
         frozen_depset_identity=True\n\
         repr=[<target //:leaf>, <alias target //:alias of //:leaf>, <target //:leaf>]\n";

/// `target.files` for rule, source-file, output-file and alias targets
/// (package `files`): every read is the target's one files-to-build depset,
/// also `target[DefaultInfo].files` (AbstractConfiguredTarget.get,
/// analysis/configuredtargets/AbstractConfiguredTarget.java:216-225, and
/// DefaultInfo.DelegatingDefaultInfo.getFiles, analysis/DefaultInfo.java:153-157:
/// the FileProvider's one NestedSet; Depset.equals is its identity).
const FILES_DEFS: &str = r#"
def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]

leaf = rule(_leaf_impl)

def _writer_impl(ctx):
    ctx.actions.write(ctx.outputs.out, ctx.label.name)
    return [DefaultInfo(files = depset([ctx.outputs.out]))]

writer = rule(_writer_impl, attrs = {"out": attr.output()})

def _check_impl(ctx):
    dep = ctx.attr.dep
    keyed = {}
    keyed[dep.files] = 1
    keyed[dep[DefaultInfo].files] = 2
    rows = [
        "rule=%s %s %s" % (
            dep.files == dep.files,
            dep.files == dep[DefaultInfo].files,
            dep[DefaultInfo].files == dep[DefaultInfo].files,
        ),
        "source=%s" % (ctx.attr.src.files == ctx.attr.src.files),
        "output=%s" % (ctx.attr.out.files == ctx.attr.out.files),
        "alias=%s" % (ctx.attr.via_alias.files == ctx.attr.via_alias.files),
        "dict_keys=%d" % len(keyed),
        "depset_elements=%d" % len(depset([dep.files, dep.files]).to_list()),
        "transitive=%s %s" % (
            depset(transitive = [dep.files]) == dep.files,
            depset(transitive = [dep.files, dep.files]) == dep.files,
        ),
        "in_list=%s" % (dep.files in [dep.files]),
        "contents=%s" % [f.short_path for f in dep.files.to_list()],
        "repr=%s" % repr(ctx.attr.src.files),
        "type=%s %s" % (type(dep.files), bool(dep.files)),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(rows) + "\n")
    return [DefaultInfo(files = depset([out]))]

check = rule(_check_impl, attrs = {
    "dep": attr.label(),
    "src": attr.label(allow_files = True),
    "out": attr.label(allow_files = True),
    "via_alias": attr.label(),
})
"#;

const FILES_BUILD: &str = r#"load(":defs.bzl", "check", "leaf", "writer")

leaf(name = "leaf")

alias(name = "alias", actual = ":leaf")

writer(name = "writer", out = "writer.out")

check(
    name = "check",
    dep = ":leaf",
    src = ":data.txt",
    out = ":writer.out",
    via_alias = ":alias",
)
"#;

/// The exact rows genuine Bazel 9.3.0 prints for `//files:check` (paired
/// capture, tests/fixtures/target_identity/files_reference_receipt.json).
const FILES_EXPECTED: &str = "rule=True True True\n\
         source=True\n\
         output=True\n\
         alias=True\n\
         dict_keys=1\n\
         depset_elements=1\n\
         transitive=True True\n\
         in_list=True\n\
         contents=[\"files/leaf.txt\"]\n\
         repr=depset([<source file files/data.txt>])\n\
         type=depset True\n";

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-target-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='target_identity')\n");
        ws.file("data.txt", "data\n");
        ws.file(
            "flags/BUILD.bazel",
            "load(':defs.bzl', 'mode_flag', 'other_flag')\nmode_flag(name = 'mode', build_setting_default = 0)\nother_flag(name = 'other', build_setting_default = 0)\n",
        );
        ws.file(
            "flags/defs.bzl",
            "def _flag_impl(ctx):\n    return []\n\nmode_flag = rule(implementation = _flag_impl, build_setting = config.int(flag = True))\nother_flag = rule(implementation = _flag_impl, build_setting = config.int(flag = True))\n",
        );
        ws.file("third_party/bazel_rules/rules_cc/BUILD.bazel", "");
        ws.file("third_party/bazel_rules/rules_cc/allow.bzl", ALLOW_BZL);
        ws.file("defs.bzl", &format!("load(\"//third_party/bazel_rules/rules_cc:allow.bzl\", _cc = \"cc_internal\")\n{DEFS}"));
        ws.file("BUILD.bazel", BUILD);
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
            .env("BAZEL_RS_VERIFY_VIEWS", "1")
            .output()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The one shared Target value per configured target (N55): identity across
/// rule contexts, providers, depsets, transitions, alias/exec edges and
/// `cc_internal.freeze`. Genuine Bazel 9.3.0 prints the same rows (paired
/// capture, `reference_receipt.json` row O/consumer).
#[test]
fn one_target_value_per_configured_target_across_the_analysis() {
    let ws = Workspace::new();
    // Offline: the genuine registry fixture, and the @platforms archive
    // that execution platform resolution materializes.
    seed_module_archives(&ws.0.join("bazel-rs-out"), &["platforms@1.0.0"]);
    let result = ws.run(&[
        "build",
        "--jobs=4",
        "--experimental-builtin-modules",
        &builtin_registry_flag(),
        "//:consumer",
    ]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let path = PathBuf::from(stdout.lines().next().unwrap().split_once('\t').unwrap().1);
    assert_eq!(fs::read_to_string(path).unwrap(), EXPECTED);
}

/// `target.files` is one depset per target (W0-C2): equal across reads and
/// to `target[DefaultInfo].files`, so one dict key, one depset element and a
/// reused transitive set, for every configured-target class. Genuine Bazel
/// 9.3.0 prints the same rows (paired capture, `files_reference_receipt.json`).
#[test]
fn target_files_is_one_depset_per_target() {
    let ws = Workspace::new();
    ws.file("files/defs.bzl", FILES_DEFS);
    ws.file("files/BUILD.bazel", FILES_BUILD);
    ws.file("files/data.txt", "data\n");
    let result = ws.run(&[
        "build",
        "--experimental-builtin-modules",
        &builtin_registry_flag(),
        "//files:check",
    ]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let path = PathBuf::from(stdout.lines().next().unwrap().split_once('\t').unwrap().1);
    assert_eq!(fs::read_to_string(path).unwrap(), FILES_EXPECTED);
}

/// The frozen copy keeps Bazel's shallow-copy semantics (N49b): the copy
/// itself is frozen, but a mutable element still fails explicitly — the
/// configured-target exception (whose re-allocation would not keep the
/// Target's identity) is gone with N55. Genuine Bazel accepts the mutable
/// element (declared native gap, `reference_receipt.json` row E/freeze_mutable).
#[test]
fn freeze_with_a_target_keeps_the_other_explicit_errors() {
    let ws = Workspace::new();
    ws.file(
        "defs.bzl",
        &format!(
            "{}\n{}",
            fs::read_to_string(ws.0.join("defs.bzl")).unwrap(),
            BAD_DEFS
        ),
    );
    ws.file(
        "BUILD.bazel",
        &format!(
            "{}\n{}",
            fs::read_to_string(ws.0.join("BUILD.bazel")).unwrap(),
            BAD_BUILD
        ),
    );
    for (target, message) in [
        (
            "//:bad",
            "cc_internal.freeze of a list containing a mutable value of type 'list' (Bazel's shallow immutable copy keeps it mutable) is not implemented in the native profile",
        ),
        ("//:bad_mutate", "trying to mutate a frozen list value"),
    ] {
        let result = ws.run(&[
            "build",
            "--experimental-builtin-modules",
            &builtin_registry_flag(),
            target,
        ]);
        assert_eq!(result.status.code(), Some(1), "{target}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains(&format!("error: {message}\n")),
            "{target}: {stderr}"
        );
    }
}
