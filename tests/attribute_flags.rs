#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

//! N57: attribute `flags` semantics — ORDER_INDEPENDENT sorts the converted
//! list value of explicit BUILD arguments and simplified unconditional
//! selects (BuildType.convertFromBuildLangType, packages/BuildType.java:
//! 245-253; labels order by Label.compareTo: repository, package, name),
//! while defaults and analysis-resolved select branches keep their order.
//! DIRECT_COMPILE_TIME_INPUT is accepted and inert (only
//! --compile_one_dependency reads it). `flags` exists only on the label-ish
//! attr constructors (StarlarkAttrModule.java), so `attr.string_list(flags =
//! ...)` fails like genuine Bazel 9.3.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
include!("support/explicit_modules.rs");
include!("support/sealed_trees.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let path = crate::test_temp::root().join(format!(
            "bazel-rs-attrflags-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='attr_flags_fixture')\n");
        ws.file("defs.bzl", defs);
        ws.file("BUILD.bazel", build);
        for file in ["z.h", "a.h", "b.h"] {
            ws.file(file, "\n");
        }
        for file in ["a/b.h", "sub/x.h"] {
            ws.file(file, "\n");
        }
        ws
    }
    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn build(&self, target: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", "build", target])
            .current_dir(&self.0)
            .explicit_modules()
            .output()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn out_path(output: &Output) -> PathBuf {
    assert!(output.status.success(), "{}", errors(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.split_once('\t'))
        .map(|(_, path)| PathBuf::from(path))
        .unwrap()
}

const DEFS: &str = r#"
def _dump(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(
        output = out,
        content = "\n".join([str(f.label) for f in ctx.attr.srcs] + [str(f.label) for f in ctx.attr.ordered]),
    )
    return [DefaultInfo(files = depset([out]))]

def _plain_dump(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output = out, content = "\n".join([str(f.label) for f in ctx.attr.srcs]))
    return [DefaultInfo(files = depset([out]))]

# ORDER_INDEPENDENT on the label list, a plain control second.
dump = rule(
    implementation = _dump,
    attrs = {
        "srcs": attr.label_list(allow_files = True, flags = ["ORDER_INDEPENDENT", "DIRECT_COMPILE_TIME_INPUT"]),
        "ordered": attr.label_list(allow_files = True),
    },
)
# Same ORDER_INDEPENDENT list, but the value only comes from the default or
# from a resolved select: neither path sorts.
defaulted = rule(
    implementation = _plain_dump,
    attrs = {"srcs": attr.label_list(allow_files = True, flags = ["ORDER_INDEPENDENT"], default = ["z.h", "a.h", "b.h"])},
)
"#;

#[test]
fn order_independent_sorts_explicit_and_unconditional_select_values() {
    let ws = Workspace::new(
        DEFS,
        r#"
load(":defs.bzl", "dump", "defaulted")

config_setting(name = "off", values = {"define": "x=0"})

dump(
    name = "explicit",
    srcs = ["z.h", "a/b.h", "a.h", "sub/x.h", "b.h"],
    ordered = ["z.h", "a/b.h", "a.h", "sub/x.h", "b.h"],
)

dump(
    name = "unconditional",
    srcs = ["z.h", "a.h"] + select({"//conditions:default": ["b.h", "a/b.h"]}),
    ordered = [],
)

dump(
    name = "resolved",
    srcs = select({
        ":off": ["a.h", "a/b.h", "b.h"],
        "//conditions:default": ["z.h", "sub/x.h"],
    }),
    ordered = [],
)

defaulted(name = "defaulted")
"#,
    );
    let paths = |name: &str| -> Vec<String> {
        let out = out_path(&ws.build(&format!("//:{name}")));
        fs::read_to_string(out)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    // Explicit values: only the flagged attribute is canonicalized
    // (Label order: name comparison, so a.h < a/b.h < b.h < sub/x.h < z.h).
    assert_eq!(
        paths("explicit"),
        [
            "@@//:a.h",
            "@@//:a/b.h",
            "@@//:b.h",
            "@@//:sub/x.h",
            "@@//:z.h",
            "@@//:z.h",
            "@@//:a/b.h",
            "@@//:a.h",
            "@@//:sub/x.h",
            "@@//:b.h"
        ]
    );
    // An unconditional select is simplified at load and sorted with the
    // direct values (BuildType.selectableConvert + convertFromBuildLangType).
    assert_eq!(
        paths("unconditional"),
        ["@@//:a.h", "@@//:a/b.h", "@@//:b.h", "@@//:z.h"]
    );
    // A resolved (non-unconditional) select keeps its branch order.
    assert_eq!(paths("resolved"), ["@@//:z.h", "@@//:sub/x.h"]);
    // A default is converted once at rule-class creation: not sorted.
    let out = out_path(&ws.build("//:defaulted"));
    assert_eq!(
        fs::read_to_string(out).unwrap().lines().collect::<Vec<_>>(),
        ["@@//:z.h", "@@//:a.h", "@@//:b.h"]
    );
}

#[test]
fn string_list_rejects_flags_like_genuine_bazel() {
    let ws = Workspace::new(
        r#"
def _impl(ctx):
    return []
bad = rule(implementation = _impl, attrs = {"s": attr.string_list(flags = ["ORDER_INDEPENDENT"])})
"#,
        r#"load(":defs.bzl", "bad")

bad(name = 'x')
"#,
    );
    let output = ws.build("//:x");
    assert!(!output.status.success());
    assert!(
        errors(&output).contains("string_list() got unexpected keyword argument 'flags'"),
        "{}",
        errors(&output)
    );
}

/// DIRECT_COMPILE_TIME_INPUT's only reader is --compile_one_dependency
/// (CompileOneDependencyTransformer.java:263), which the native profile
/// rejects explicitly (options are never silently ignored); until it is
/// modelled the flag is inert at loading and analysis.
#[test]
fn compile_one_dependency_fails_explicitly() {
    let ws = Workspace::new(
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output = out, content = "")
    return [DefaultInfo(files = depset([out]))]
r = rule(implementation = _impl, attrs = {
    "deps": attr.label_list(allow_files = True, flags = ["DIRECT_COMPILE_TIME_INPUT"]),
})
"#,
        r#"load(":defs.bzl", "r")

filegroup(name = "fg", srcs = ["a.h"])

r(name = "x", deps = ["a.h"])
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args([
            "--ignore_all_rc_files",
            "build",
            "--compile_one_dependency",
            "//:a.h",
        ])
        .current_dir(&ws.0)
        .explicit_modules()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", errors(&output));
    assert_eq!(
        errors(&output).trim(),
        "bazel-rs: unsupported option \"--compile_one_dependency\"; options are never silently ignored",
        "{}",
        errors(&output)
    );
    // The flag itself stays inert: the same target builds without it.
    assert!(ws.build("//:x").status.success());
}
