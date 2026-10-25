//! `attr.label`/`attr.label_list`/label dictionaries with `allow_files` or
//! `allow_single_file` given as a list of extensions, through the CLI
//! (docs/LOAD_DECLARATIONS.md, "Extension-list allow_files"): file
//! prerequisites are filtered by name (RuleContext.validateDirectPrerequisiteType,
//! analysis/RuleContext.java:1845-1886), every prerequisite must produce a
//! matching file (validateDirectPrerequisiteFileTypes, :1945-1994), and
//! `ctx.files` stays the unfiltered files to build
//! (PrerequisiteArtifacts.nestedSet, analysis/PrerequisiteArtifacts.java:64-72).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);

const DEFS: &str = r#"
def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".srcs")
    names = [f.basename for f in ctx.files.srcs]
    main = ctx.file.main.basename if ctx.attr.main else "-"
    ctx.actions.write(out, "srcs=%s main=%s\n" % (",".join(names), main))
    return [DefaultInfo(files = depset([out]))]

# rules_go go_library shape (go/private/rules/library.bzl).
go_library = rule(_lib_impl, attrs = {
    "srcs": attr.label_list(allow_files = [".go", ".s"]),
    "main": attr.label(allow_single_file = [".go"]),
})

def _emit_impl(ctx):
    outs = [ctx.actions.declare_file(name) for name in ctx.attr.names]
    for out in outs:
        ctx.actions.write(out, out.basename)
    return [DefaultInfo(files = depset(outs))]

emit = rule(_emit_impl, attrs = {"names": attr.string_list()})

def _dict_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".files")
    ctx.actions.write(out, "m=%s l=%s\n" % (
        ",".join([f.basename for f in ctx.files.m]),
        ",".join([f.basename for f in ctx.files.l]),
    ))
    return [DefaultInfo(files = depset([out]))]

dicts = rule(_dict_impl, attrs = {
    "m": attr.string_keyed_label_dict(allow_files = [".txt"]),
    "l": attr.label_list_dict(allow_files = [".go", ".s", ".txt"]),
})

def _none_impl(ctx):
    return []

none_allowed = rule(_none_impl, attrs = {"srcs": attr.label_list(allow_files = [])})
ne = rule(_none_impl, attrs = {
    "srcs": attr.label_list(allow_files = [".go"], allow_empty = False),
    "deps": attr.label_list(allow_empty = False),
})
odd = rule(_none_impl, attrs = {"srcs": attr.label_list(allow_files = ["tar.gz", "Makefile", ".a"])})
"#;

const BUILD: &str = r#"load(":defs.bzl", "dicts", "emit", "go_library", "ne", "none_allowed", "odd")

genrule(name = "gen", outs = ["gen.go"], cmd = "echo package gen > $@")
genrule(name = "gen_txt", outs = ["gen.txt"], cmd = "echo x > $@")
filegroup(name = "fg", srcs = ["c.go", "readme.txt"])
emit(name = "mixed", names = ["m.go", "m.txt"])
emit(name = "txt_only", names = ["t.txt"])
emit(name = "two_go", names = ["x.go", "y.go"])
alias(name = "to_notes", actual = "notes.txt")

go_library(name = "ok", srcs = ["a.go", "b.s", ":gen", ":mixed", ":fg"], main = "a.go")
go_library(name = "bad_src", srcs = ["a.go", "notes.txt", ":to_notes"])
go_library(name = "bad_gen", srcs = [":gen.txt"])
go_library(name = "missing", srcs = ["missing.txt", "nodot"])
go_library(name = "bad_rule", srcs = [":txt_only"])
go_library(name = "main_txt", main = "notes.txt")
go_library(name = "main_two", main = ":two_go")
go_library(name = "main_rule_txt", main = ":txt_only")
go_library(name = "main_rule", main = ":gen")
none_allowed(name = "none", srcs = ["a.go"])
none_allowed(name = "none_rule", srcs = [":mixed"])
odd(name = "odd", srcs = ["x.tar.gz", "dir/Makefile", "Makefile"])
odd(name = "odd_bad", srcs = ["x.gz"])
dicts(name = "dict_ok", m = {"x": "notes.txt", "y": ":mixed"}, l = {"k": ["a.go", "notes.txt"]})
dicts(name = "dict_bad", m = {"x": "a.go"}, l = {"k": [":two_go"], "j": ["b.s"]})
package_group(name = "pg")
go_library(name = "pg_ext", srcs = [":pg", "notes.txt"])
go_library(name = "rule_then_file", srcs = [":txt_only", "notes.txt"])
go_library(name = "two_attrs", main = ":txt_only", srcs = [":two_go", ":txt_only"])
ne(name = "ne", srcs = [":txt_only"])
go_library(name = "dir_ok", srcs = ["godir.go"])
go_library(name = "dir_bad", srcs = ["dir"])
genrule(name = "dir_used", srcs = ["godir.go"], outs = ["dir_used.txt"], cmd = "touch $@")
"#;

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("allow-files-ext-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name = 'main')\n");
        f.file("defs.bzl", DEFS);
        f.file("BUILD.bazel", BUILD);
        for name in [
            "a.go",
            "b.s",
            "c.go",
            "readme.txt",
            "notes.txt",
            "x.tar.gz",
            "x.gz",
            "Makefile",
            "dir/Makefile",
            "godir.go/inner.txt",
        ] {
            f.file(name, "x\n");
        }
        f
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn build(&self, target: &str) -> Output {
        let output = Command::new("timeout")
            .args([
                "--kill-after=5s",
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                target,
            ])
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {target}");
        output
    }

    fn built(&self, target: &str) -> String {
        let output = self.build(target);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        fs::read_to_string(path).unwrap()
    }

    /// The whole stderr of a failed build, workspace path as `WS`.
    fn rejected(&self, target: &str) -> String {
        let output = self.build(target);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        // Startup notice, not a diagnostic (crate::memory_limit).
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
        stderr.replace(&self.0.path().display().to_string(), "WS")
    }
}

#[test]
fn go_library_shaped_extension_lists_build_with_unfiltered_ctx_files() {
    let f = Fixture::new();
    // Rule prerequisites need one matching file; ctx.files keeps all of
    // them (m.txt, readme.txt); ctx.file is the single matching file.
    assert_eq!(
        f.built("//:ok"),
        "srcs=a.go,b.s,gen.go,m.go,m.txt,c.go,readme.txt main=a.go\n"
    );
    assert_eq!(f.built("//:main_rule"), "srcs= main=gen.go\n");
    // Entries are plain suffixes: no leading dot, exact names, multi-dot.
    let odd = f.build("//:odd");
    assert!(
        odd.status.success(),
        "{}",
        String::from_utf8_lossy(&odd.stderr)
    );
}

#[test]
fn mismatched_file_prerequisites_report_both_bazel_errors() {
    let f = Fixture::new();
    let at = |row: &str, rule: &str, target: &str, attr: &str, message: &str| {
        format!("WS/BUILD.bazel:{row}: in {attr} attribute of {rule} rule //:{target}: {message}\n")
    };
    let go = "(expected .go or .s)";
    // Source file, then the same file through an alias
    // (RuleContext.java:1819-1830, 1882-1885, 1975-1993). The alias's
    // does-not-produce error is the identical event, reported once.
    assert_eq!(
        f.rejected("//:bad_src"),
        format!(
            "bazel-rs: {}{}{}",
            at(
                "12:11",
                "go_library",
                "bad_src",
                "srcs",
                &format!("source file '//:notes.txt' is misplaced here {go}")
            ),
            at(
                "12:11",
                "go_library",
                "bad_src",
                "srcs",
                &format!("'//:notes.txt' does not produce any go_library srcs files {go}")
            ),
            at(
                "12:11",
                "go_library",
                "bad_src",
                "srcs",
                &format!(
                    "alias '//:to_notes' referring to source file '//:notes.txt' is misplaced here {go}"
                )
            ),
        )
    );
    assert_eq!(
        f.rejected("//:bad_gen"),
        format!(
            "bazel-rs: {}{}",
            at(
                "13:11",
                "go_library",
                "bad_gen",
                "srcs",
                &format!("generated file '//:gen.txt' is misplaced here {go}")
            ),
            at(
                "13:11",
                "go_library",
                "bad_gen",
                "srcs",
                &format!("'//:gen.txt' does not produce any go_library srcs files {go}")
            ),
        )
    );
    // A missing input file is `target` even without a dot: the set is never
    // NO_FILE (RuleContext.java:1868-1880).
    assert_eq!(
        f.rejected("//:missing"),
        format!(
            "bazel-rs: {}{}{}{}",
            at(
                "14:11",
                "go_library",
                "missing",
                "srcs",
                "target '//:missing.txt' does not exist"
            ),
            at(
                "14:11",
                "go_library",
                "missing",
                "srcs",
                &format!("'//:missing.txt' does not produce any go_library srcs files {go}")
            ),
            at(
                "14:11",
                "go_library",
                "missing",
                "srcs",
                "target '//:nodot' does not exist"
            ),
            at(
                "14:11",
                "go_library",
                "missing",
                "srcs",
                &format!("'//:nodot' does not produce any go_library srcs files {go}")
            ),
        )
    );
    assert_eq!(
        f.rejected("//:main_txt"),
        format!(
            "bazel-rs: {}{}",
            at(
                "16:11",
                "go_library",
                "main_txt",
                "main",
                "source file '//:notes.txt' is misplaced here (expected .go)"
            ),
            at(
                "16:11",
                "go_library",
                "main_txt",
                "main",
                "'//:notes.txt' does not produce any go_library main files (expected .go)"
            ),
        )
    );
    // An empty list allows nothing: `expected nothing` (StringUtil.java:72).
    assert_eq!(
        f.rejected("//:none"),
        format!(
            "bazel-rs: {}{}",
            at(
                "20:13",
                "none_allowed",
                "none",
                "srcs",
                "source file '//:a.go' is misplaced here (expected nothing)"
            ),
            at(
                "20:13",
                "none_allowed",
                "none",
                "srcs",
                "'//:a.go' does not produce any none_allowed srcs files (expected nothing)"
            ),
        )
    );
    assert_eq!(
        f.rejected("//:odd_bad"),
        format!(
            "bazel-rs: {}{}",
            at(
                "23:4",
                "odd",
                "odd_bad",
                "srcs",
                "source file '//:x.gz' is misplaced here (expected tar.gz, Makefile, or .a)"
            ),
            at(
                "23:4",
                "odd",
                "odd_bad",
                "srcs",
                "'//:x.gz' does not produce any odd srcs files (expected tar.gz, Makefile, or .a)"
            ),
        )
    );
}

#[test]
fn rule_prerequisites_need_a_matching_file_to_build() {
    let f = Fixture::new();
    // Attribute errors at the rule's location, like file prerequisites'
    // (RuleContext.java:1966-1993).
    for (target, row, message) in [
        (
            "bad_rule",
            "15:11",
            "in srcs attribute of go_library rule //:bad_rule: '//:txt_only' does not produce any go_library srcs files (expected .go or .s)",
        ),
        (
            "main_two",
            "17:11",
            "in main attribute of go_library rule //:main_two: '//:two_go' must produce a single file",
        ),
        (
            "main_rule_txt",
            "18:11",
            "in main attribute of go_library rule //:main_rule_txt: '//:txt_only' does not produce any go_library main files (expected .go)",
        ),
        (
            "none_rule",
            "21:13",
            "in srcs attribute of none_allowed rule //:none_rule: '//:mixed' does not produce any none_allowed srcs files (expected nothing)",
        ),
    ] {
        assert_eq!(
            f.rejected(&format!("//:{target}")),
            format!("bazel-rs: WS/BUILD.bazel:{row}: {message}\n")
        );
    }
}

#[test]
fn every_prerequisite_error_is_reported_in_attribute_then_prerequisite_order() {
    let f = Fixture::new();
    let lines = |row: &str, rule: &str, target: &str, errors: &[(&str, &str)]| {
        let mut text = "bazel-rs: ".to_owned();
        for (attr, message) in errors {
            text.push_str(&format!(
                "WS/BUILD.bazel:{row}: in {attr} attribute of {rule} rule //:{target}: {message}\n"
            ));
        }
        text
    };
    let go = "(expected .go or .s)";
    // A package group: the file-type error, then
    // CommonPrerequisiteValidator's own prefixed message
    // (CommonPrerequisiteValidator.java:48-59, 365-375), then the file.
    assert_eq!(
        f.rejected("//:pg_ext"),
        lines(
            "27:11",
            "go_library",
            "pg_ext",
            &[
                (
                    "srcs",
                    &format!("'//:pg' does not produce any go_library srcs files {go}")
                ),
                (
                    "srcs",
                    "in srcs attribute of go_library rule //:pg_ext: package group '//:pg' is misplaced here (they are only allowed in the visibility attribute)",
                ),
                (
                    "srcs",
                    &format!("source file '//:notes.txt' is misplaced here {go}")
                ),
                (
                    "srcs",
                    &format!("'//:notes.txt' does not produce any go_library srcs files {go}"),
                ),
            ],
        )
    );
    assert_eq!(
        f.rejected("//:rule_then_file"),
        lines(
            "28:11",
            "go_library",
            "rule_then_file",
            &[
                (
                    "srcs",
                    &format!("'//:txt_only' does not produce any go_library srcs files {go}"),
                ),
                (
                    "srcs",
                    &format!("source file '//:notes.txt' is misplaced here {go}")
                ),
                (
                    "srcs",
                    &format!("'//:notes.txt' does not produce any go_library srcs files {go}"),
                ),
            ],
        )
    );
    // Declaration order (srcs, main), not the BUILD call's keyword order.
    assert_eq!(
        f.rejected("//:two_attrs"),
        lines(
            "29:11",
            "go_library",
            "two_attrs",
            &[
                (
                    "srcs",
                    &format!("'//:txt_only' does not produce any go_library srcs files {go}"),
                ),
                (
                    "main",
                    "'//:txt_only' does not produce any go_library main files (expected .go)",
                ),
            ],
        )
    );
    // validateExtraPrerequisites' empty attributes follow (RuleContext.java:1502-1503, 2111).
    assert_eq!(
        f.rejected("//:ne"),
        lines(
            "30:3",
            "ne",
            "ne",
            &[
                (
                    "srcs",
                    "'//:txt_only' does not produce any ne srcs files (expected .go)",
                ),
                ("deps", "attribute must be non empty"),
            ],
        )
    );
}

#[test]
fn label_dictionaries_filter_the_same_way() {
    let f = Fixture::new();
    assert_eq!(
        f.built("//:dict_ok"),
        "m=notes.txt,m.go,m.txt l=a.go,notes.txt\n"
    );
    assert_eq!(
        f.rejected("//:dict_bad"),
        "bazel-rs: WS/BUILD.bazel:25:6: in m attribute of dicts rule //:dict_bad: source file '//:a.go' is misplaced here (expected .txt)\nWS/BUILD.bazel:25:6: in m attribute of dicts rule //:dict_bad: '//:a.go' does not produce any dicts m files (expected .txt)\n"
    );
}

#[test]
fn source_directories_are_file_targets_matched_by_name() {
    let f = Fixture::new();
    // A source directory is an input file target (Package.java:893-920),
    // matched by its name (RuleContext.java:1865).
    assert_eq!(f.built("//:dir_ok"), "srcs=godir.go main=-\n");
    assert_eq!(
        f.rejected("//:dir_bad"),
        "bazel-rs: WS/BUILD.bazel:32:11: in srcs attribute of go_library rule //:dir_bad: source file '//:dir' is misplaced here (expected .go or .s)\nWS/BUILD.bazel:32:11: in srcs attribute of go_library rule //:dir_bad: '//:dir' does not produce any go_library srcs files (expected .go or .s)\n"
    );
    // As an action input it builds, like Bazel's (A6 dir_used): tracked by
    // content and staged as a directory (docs/SOURCE_DIRECTORIES.md; the
    // receipt's EXPECTED_NATIVE_UNSUPPORTED row is superseded).
    assert_eq!(f.built("//:dir_used"), "");
}

/// Merge adaptations of N27 with N18c, N23a and N28c: a transitioned
/// attribute's prerequisites are validated in its edge configuration (the
/// dependency analyzed there), a source directory among a requested
/// target's runfiles builds (docs/SOURCE_DIRECTORIES.md), and
/// package_metadata file errors precede the rule's own attribute errors in
/// one prerequisite pass.
#[test]
fn transitioned_runfiles_and_metadata_prerequisites() {
    let f = Fixture::new();
    f.file(
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        "package_group(name = 'function_transition_allowlist', packages = ['public'])\n",
    );
    f.file(
        "flags.bzl",
        r#"def _flag_impl(ctx):
    return []
string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))
"#,
    );
    f.file(
        "tr.bzl",
        r#"P = provider(fields = ["mode"])

def _t_impl(settings, attr):
    return {"//:mode": "on"}

t = transition(implementation = _t_impl, inputs = [], outputs = ["//:mode"])

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".srcs")
    ctx.actions.write(out, "srcs=%s\n" % ",".join([f.basename for f in ctx.files.srcs]))
    return [DefaultInfo(files = depset([out]))]

ALLOW = {"_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")}
ATTRS = dict(ALLOW)
ATTRS["srcs"] = attr.label_list(allow_files = [".go"], cfg = t)
tlib = rule(_lib_impl, attrs = ATTRS)

def _prov_impl(ctx):
    return [P(mode = "x")]

prov = rule(_prov_impl)
NEEDS = dict(ALLOW)
NEEDS["deps"] = attr.label_list(providers = [P], cfg = t)
def _needs_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".modes")
    ctx.actions.write(out, "modes=%s\n" % ",".join([d[P].mode for d in ctx.attr.deps]))
    return [DefaultInfo(files = depset([out]))]

needs = rule(_needs_impl, attrs = NEEDS)

def _rf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]), runfiles = ctx.runfiles(files = ctx.files.data))]

rf = rule(_rf_impl, attrs = {"data": attr.label_list(allow_files = True)})
"#,
    );
    f.file(
        "BUILD.bazel",
        &format!(
            r#"{BUILD}
load(":flags.bzl", "string_flag")
load(":tr.bzl", "needs", "prov", "rf", "tlib")
string_flag(name = "mode", build_setting_default = "off")
config_setting(name = "on", flag_values = {{":mode": "on"}})
alias(name = "picked", actual = select({{":on": "a.go", "//conditions:default": "notes.txt"}}))
tlib(name = "t_ok", srcs = [":picked"])
alias(name = "picked_bad", actual = select({{":on": "notes.txt", "//conditions:default": "a.go"}}))
tlib(name = "t_bad", srcs = [":picked_bad"])
filegroup(name = "pr_or_none", srcs = [])
alias(name = "maybe_prov", actual = select({{":on": ":p", "//conditions:default": ":pr_or_none"}}))
prov(name = "p")
needs(name = "n_ok", deps = [":maybe_prov"])
rf(name = "rf_dir", data = ["godir.go"])
"#
        ),
    );
    // The alias resolves in the transitioned configuration (`a.go`), which
    // passes `.go`; the target configuration would pick `notes.txt`.
    assert_eq!(f.built("//:t_ok"), "srcs=a.go\n");
    let stderr = f.rejected("//:t_bad");
    assert_eq!(
        stderr,
        "bazel-rs: WS/BUILD.bazel:42:5: in srcs attribute of tlib rule //:t_bad: alias '//:picked_bad' referring to source file '//:notes.txt' is misplaced here (expected .go)\nWS/BUILD.bazel:42:5: in srcs attribute of tlib rule //:t_bad: '//:notes.txt' does not produce any tlib srcs files (expected .go)\n"
    );
    // Providers are those of the dependency in the transitioned
    // configuration (`:p` provides P).
    assert_eq!(f.built("//:n_ok"), "modes=x\n");
    // package_metadata (a built-in attribute, no-config edges) is checked
    // first in the same pass, then the rule's own attributes.
    f.file(
        "md/BUILD.bazel",
        "load('//:defs.bzl', 'go_library')\ngo_library(name = 'm', srcs = ['x.txt'], package_metadata = ['meta.txt'])\n",
    );
    f.file("md/x.txt", "x\n");
    f.file("md/meta.txt", "x\n");
    let stderr = f.rejected("//md:m");
    let metadata = stderr
        .find("in package_metadata attribute of go_library rule //md:m: source file '//md:meta.txt' is misplaced here (expected no files)")
        .unwrap_or_else(|| panic!("{stderr}"));
    let srcs = stderr
        .find("in srcs attribute of go_library rule //md:m: source file '//md:x.txt' is misplaced here (expected .go or .s)")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(metadata < srcs, "{stderr}");
    // A source directory among the requested target's runfiles needs
    // nothing built (docs/SOURCE_DIRECTORIES.md).
    assert_eq!(f.built("//:rf_dir"), "x");
}
