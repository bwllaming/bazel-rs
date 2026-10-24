//! N45: repository mapping of extension-generated repositories and Bazel's
//! invalid-repository labels (docs/EXTENSION_REPO_MAPPING.md). Offline:
//! local_path modules only. Expected values are the genuine Bazel 9.3.0
//! observations of tests/fixtures/ext_repo_mapping/reference_receipt.json.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

const EXT: &str = r#"
def _plat(rctx):
    print("N45|fetch|" + rctx.original_name)
    rctx.file("BUILD.bazel", 'filegroup(name = "impl", visibility = ["//visibility:public"])\n')

plat = repository_rule(_plat)

def _source(prefix, texts):
    source = 'def _p(text, label):\n    print("N45|%s|%%s|%%s" %% (text, str(label)))\n' % prefix
    for text in texts:
        source += '_p(%r, Label(%r))\n' % (text, text)
    return source + "probe = 1\n"

def _probe_repo(rctx):
    rctx.file("defs.bzl", _source(rctx.attr.prefix, rctx.attr.texts))
    build = 'load(":defs.bzl", "probe")\nload("@dep//:rules.bzl", "holder")\n'
    build += 'holder(name = "ok", deps = ["@x_amd64//:impl"])\n'
    build += 'holder(name = "bad", deps = ["@x_armv6//:impl"])\n'
    build += 'filegroup(name = "fg", srcs = ["@nope//:x"])\n'
    rctx.file("BUILD.bazel", build)

probe_repo = repository_rule(_probe_repo, attrs = {"prefix": attr.string(), "texts": attr.string_list()})

def _ext(mctx):
    plat(name = "x_amd64")
    plat(name = "x_unused")
    probe_repo(name = "probe", prefix = "probe", texts = [
        "@x_amd64//:impl", "@probe//:x", "@dep//:x", "@fx//:x", "@x_armv6//:impl", "@//:x",
        "@@dep++ext+x_amd64//:x", "//:x",
    ])

ext = module_extension(_ext)

def _ext2(mctx):
    # Named like a bazel_dep of the hosting module: inside the extension's
    # repositories the generated one wins (buildKeepingLast).
    plat(name = "other")
    probe_repo(name = "probe2", prefix = "probe2", texts = ["@other//:x", "@dep//:x", "@probe//:x"])

ext2 = module_extension(_ext2)

def _rr(rctx):
    rctx.file("defs.bzl", _source(rctx.original_name, ["@" + rctx.attr.peer + "//:x", "@probe//:x", "@//:x", "@fx//:x", "@dep//:x"]))
    rctx.file("BUILD.bazel", 'load(":defs.bzl", "probe")\n')

rr = repository_rule(_rr, attrs = {"peer": attr.string()})
"#;

const RULES: &str = r#"
def _holder(ctx):
    print("N45|holder|%s|%s" % (ctx.label.name, [str(d.label) for d in ctx.attr.deps]))
    return []

holder = rule(_holder, attrs = {"deps": attr.label_list()})

def _init(**kwargs):
    return kwargs

def _lifted(ctx):
    print("N45|initializer_select|%s" % ctx.attr.v)
    return []

lifted = rule(_lifted, initializer = _init, attrs = {"v": attr.string_list()})
"#;

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("ext-repo-mapping-")
                .tempdir()
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file(
            "MODULE.bazel",
            "module(name = 'fx')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'dep')\nbazel_dep(name = 'other', version = '1.0')\nlocal_path_override(module_name = 'other', path = 'other')\next = use_extension('@dep//:ext.bzl', 'ext')\nuse_repo(ext, 'probe')\next2 = use_extension('@dep//:ext.bzl', 'ext2')\nuse_repo(ext2, 'probe2')\nrr = use_repo_rule('@dep//:ext.bzl', 'rr')\nrr(name = 'rr1', peer = 'rr2')\nrr(name = 'rr2', peer = 'rr1')\n",
        );
        f.file(
            "dep/MODULE.bazel",
            "module(name = 'dep', version = '1.0')\nbazel_dep(name = 'other', version = '1.0')\n",
        );
        f.file(
            "other/MODULE.bazel",
            "module(name = 'other', version = '1.0')\n",
        );
        f.file("other/BUILD.bazel", "");
        f.file("dep/BUILD.bazel", "");
        f.file("dep/ext.bzl", EXT);
        f.file("dep/rules.bzl", RULES);
        f.file("BUILD.bazel", "");
        f
    }

    fn file(&self, relative: &str, bytes: &str) {
        let path = self.0.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(!matches!(output.status.code(), Some(124 | 137)), "timeout");
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn prints(output: &Output) -> Vec<String> {
    errors(output)
        .lines()
        .filter(|line| line.starts_with("N45|"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn generated_repositories_see_siblings_host_mapping_and_themselves() {
    let f = Fixture::new();
    let output = f.run(&["query", "@probe//:all"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        prints(&output),
        [
            "N45|probe|@x_amd64//:impl|@@dep++ext+x_amd64//:impl",
            "N45|probe|@probe//:x|@@dep++ext+probe//:x",
            "N45|probe|@dep//:x|@@dep+//:x",
            "N45|probe|@fx//:x|@@[unknown repo 'fx' requested from @@dep++ext+probe]//:x",
            "N45|probe|@x_armv6//:impl|@@[unknown repo 'x_armv6' requested from @@dep++ext+probe (did you mean 'x_amd64'?)]//:impl",
            "N45|probe|@//:x|@@[unknown repo '' requested from @@dep++ext+probe]//:x",
            "N45|probe|@@dep++ext+x_amd64//:x|@@dep++ext+x_amd64//:x",
            "N45|probe|//:x|@@dep++ext+probe//:x",
        ]
    );
    // Loading the hub-like package needs no sibling repository.
    assert!(!errors(&output).contains("N45|fetch|"));
}

#[test]
fn generated_name_wins_over_hosting_module_mapping() {
    let f = Fixture::new();
    let output = f.run(&["query", "@probe2//:all"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        prints(&output),
        [
            "N45|probe2|@other//:x|@@dep++ext2+other//:x",
            "N45|probe2|@dep//:x|@@dep+//:x",
            "N45|probe2|@probe//:x|@@[unknown repo 'probe' requested from @@dep++ext2+probe2 (did you mean 'probe2'?)]//:x",
        ]
    );
}

#[test]
fn use_repo_rule_repositories_see_each_other_and_the_using_module() {
    let f = Fixture::new();
    let output = f.run(&["query", "@rr1//:all"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        prints(&output),
        [
            "N45|rr1|@rr2//:x|@@+rr+rr2//:x",
            "N45|rr1|@probe//:x|@@dep++ext+probe//:x",
            "N45|rr1|@//:x|@@//:x",
            "N45|rr1|@fx//:x|@@//:x",
            "N45|rr1|@dep//:x|@@dep+//:x",
        ]
    );
}

#[test]
fn sibling_dependency_materializes_only_the_demanded_sibling() {
    let f = Fixture::new();
    let output = f.run(&["build", "@probe//:ok"]);
    assert!(output.status.success(), "{}", errors(&output));
    let lines = prints(&output);
    assert!(lines.contains(&"N45|fetch|x_amd64".to_owned()), "{lines:?}");
    assert!(
        !lines.contains(&"N45|fetch|x_unused".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"N45|holder|ok|[\"@@dep++ext+x_amd64//:impl\"]".to_owned()),
        "{lines:?}"
    );
}

#[test]
fn invalid_sibling_edges_fail_only_at_analysis_with_bazel_text() {
    let f = Fixture::new();
    for (target, name, suggestion) in [
        ("@probe//:bad", "x_armv6", " (did you mean 'x_amd64'?)"),
        ("@probe//:fg", "nope", ""),
    ] {
        let output = f.run(&["build", target]);
        assert_eq!(output.status.code(), Some(1), "{}", errors(&output));
        let repository =
            format!("@@[unknown repo '{name}' requested from @@dep++ext+probe{suggestion}]");
        let expected = format!(
            "no such package '{repository}//': The repository '{repository}' could not be resolved: No repository visible as '@{name}' from repository '@@dep++ext+probe' and referenced by '@@dep++ext+probe//:{}'",
            target.rsplit(':').next().unwrap()
        );
        assert!(errors(&output).contains(&expected), "{}", errors(&output));
    }
}

#[test]
fn initializer_select_keys_resolve_in_the_target_package() {
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "load('@dep//:rules.bzl', 'lifted')\nconfig_setting(name = 'fast', values = {'compilation_mode': 'fastbuild'})\nlifted(name = 'sel', v = select({'@fx//:fast': ['root'], '//conditions:default': ['default']}))\n",
    );
    let output = f.run(&["build", "//:sel"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(prints(&output), ["N45|initializer_select|[\"root\"]"]);
}

#[test]
fn fixture_workspace_is_shared_with_the_paired_capture() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ext_repo_mapping");
    for path in [
        "workspace/MODULE.bazel",
        "workspace/dep/ext.bzl",
        "oci_workspace/MODULE.bazel",
    ] {
        assert!(root.join(path).is_file(), "{path}");
    }
}

#[test]
fn visibility_labels_in_unknown_repositories_are_deferred() {
    // Genuine Bazel 9.3 (offline check recorded in docs/EXTENSION_REPO_MAPPING.md):
    // a __pkg__/__subpackages__ scope in a non-visible repository matches no
    // package; a package_group label there fails only when it is loaded.
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "filegroup(name = 'a', srcs = ['//x:b'])\nfilegroup(name = 'd', srcs = ['//x:c'])\nfilegroup(name = 'e', srcs = ['//x:g'])\n",
    );
    f.file(
        "x/BUILD.bazel",
        "filegroup(name = 'b', visibility = ['@nope//:__pkg__', '//:__pkg__'])\nfilegroup(name = 'c', visibility = ['@nope//:__pkg__', '@nope//:__subpackages__'])\nfilegroup(name = 'g', visibility = ['@nope//:grp'])\n",
    );
    let output = f.run(&["build", "//:a"]);
    assert!(output.status.success(), "{}", errors(&output));
    let output = f.run(&["build", "//:d"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains("in filegroup rule //:d: Visibility error:\ntarget '//x:c' is not visible from\ntarget '//:d'"),
        "{}",
        errors(&output)
    );
    let output = f.run(&["build", "//:e"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains("no such package '@@[unknown repo 'nope' requested from @@]//': The repository '@@[unknown repo 'nope' requested from @@]' could not be resolved: No repository visible as '@nope' from main repository"),
        "{}",
        errors(&output)
    );
}

#[test]
fn user_written_bracketed_repository_names_are_invalid() {
    let f = Fixture::new();
    f.file(
        "br/BUILD.bazel",
        "filegroup(name = 'x', srcs = [\"@@[unknown repo 'nope' requested from @@]//:t\"])\n",
    );
    let output = f.run(&["query", "//br:all"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains("//br:x: invalid label '@@[unknown repo 'nope' requested from @@]//:t' in element 0 of attribute 'srcs' of 'filegroup': invalid repository name '[unknown repo 'nope' requested from @@]': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'"),
        "{}",
        errors(&output)
    );
    f.file(
        "br2/defs.bzl",
        "l = Label(\"@@[unknown repo 'nope' requested from @@]//:t\")\n",
    );
    f.file("br2/BUILD.bazel", "load(':defs.bzl', 'l')\n");
    let output = f.run(&["query", "//br2:all"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains("invalid label in Label(): invalid repository name '[unknown repo 'nope' requested from @@]': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'"),
        "{}",
        errors(&output)
    );
}

#[test]
fn invalid_labels_sort_by_apparent_repository_name() {
    // PackageIdentifier.compareTo uses RepositoryName.getName() (genuine:
    // ["@@//:b", "@@aaa+//:a", "@@[unknown repo 'mmm' ...]//:a", "@@[unknown repo 'zzz' ...]//:a"]).
    let f = Fixture::new();
    f.file(
        "s/defs.bzl",
        "print('N45|sorted|' + str([str(l) for l in sorted([Label('@zzz//:a'), Label('@@aaa+//:a'), Label('@mmm//:a'), Label('//:b')])]))\nprint('N45|lt|' + str(Label('@zzz//:a') < Label('@@aaa+//:a')))\nx = 1\n",
    );
    f.file("s/BUILD.bazel", "load(':defs.bzl', 'x')\n");
    let output = f.run(&["query", "//s:all"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        prints(&output),
        [
            "N45|sorted|[\"@@//:b\", \"@@aaa+//:a\", \"@@[unknown repo 'mmm' requested from @@]//:a\", \"@@[unknown repo 'zzz' requested from @@]//:a\"]",
            "N45|lt|False",
        ]
    );
}
