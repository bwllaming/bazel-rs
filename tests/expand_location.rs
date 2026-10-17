//! N50b: `ctx.expand_location` (StarlarkRuleContext.expandLocation,
//! makeLabelMap, LocationExpander), interned `Target` values and
//! `attr.label_keyed_string_dict` merging (docs/EXPAND_LOCATION.md). The
//! workspace is tests/fixtures/expand_location/workspace with the genuine
//! rules_shell sources of tests/fixtures/sh_binary/rules_shell; expected
//! outputs and texts are genuine Bazel 9.3.0's (paired capture
//! tests/fixtures/expand_location/reference_receipt.json).
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The builtin-closure modules replaced by N30's stubs
/// (tests/fixtures/test_flags/stubs) so nothing is downloaded; rules_shell
/// is the genuine-source fixture and platforms the genuine 1.0.0 archive.
const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "zlib",
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn extract_platforms(to: &Path) {
    use base64::Engine;
    let archive: serde_json::Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/label_repo_name/platforms_archive.json"))
            .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(archive["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in tar.entries().unwrap() {
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

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("expand-location-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let root = f.0.path();
        copy_tree(
            &manifest().join("tests/fixtures/expand_location/workspace"),
            root,
        );
        copy_tree(
            &manifest().join("tests/fixtures/sh_binary/rules_shell"),
            &root.join("third_party/rules_shell"),
        );
        extract_platforms(&root.join("third_party/platforms"));
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        let mut module = fs::read_to_string(root.join("MODULE.bazel")).unwrap();
        module.push_str(
            "bazel_dep(name = 'platforms', version = '1.0.0')\n\
             local_path_override(module_name = 'rules_shell', path = 'third_party/rules_shell')\n\
             local_path_override(module_name = 'platforms', path = 'third_party/platforms')\n",
        );
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = '{name}', path = '{}')\n",
                stubs.join(name).display()
            ));
        }
        fs::write(root.join("MODULE.bazel"), module).unwrap();
        f
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let output = Command::new("timeout")
            .args([
                "--kill-after=5s",
                "300s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules",
                &registry,
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// Built output files by label suffix (`path` relative to bazel-out/<cfg>/bin).
    fn built(&self, targets: &[&str]) -> Vec<PathBuf> {
        let output = self.run("build", targets);
        ok(&output);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(_, path)| PathBuf::from(path))
            .collect()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
}

fn rejected(output: &Output, message: &str) {
    let text = stderr(output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
    assert!(!text.contains("panicked"), "{text}");
}

fn output_text(paths: &[PathBuf], suffix: &str) -> String {
    let path = paths
        .iter()
        .find(|path| path.to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("no output ending in {suffix}: {paths:?}"));
    fs::read_to_string(path).unwrap()
}

const OK: &str = "$(location :fg) => test/a.txt
$(locations :al) => test/a.txt
$(rootpath a.txt) => test/a.txt
$(execpath :fg) => test/a.txt
$(execpaths :gen) => bazel-out/k8-fastbuild/bin/test/g1.txt bazel-out/k8-fastbuild/bin/test/g2.txt
$(rootpaths :gen) => test/g1.txt test/g2.txt
$(location :tool) => bazel-out/k8-fastbuild/bin/test/tool
$(rlocationpath :tool) => _main/test/tool
$(execpath tool) => bazel-out/k8-fastbuild/bin/test/tool
$(location o.txt) => bazel-out/k8-fastbuild/bin/test/o.txt
$(locations :o1.txt) => bazel-out/k8-fastbuild/bin/test/o1.txt
$(execpath //test:fg) => test/a.txt
$(location @bazel_tools//tools/test:collect_coverage) => bazel-out/k8-fastbuild/bin/external/bazel_tools/tools/test/collect_coverage
$(rlocationpaths @bazel_tools//tools/test:collect_coverage) => bazel_tools/tools/test/collect_coverage
$(rlocationpaths :fg2) => _main/test/a.txt _main/test/b.txt
$(location c.txt) => test/c.txt
$(locations :fg2) => test/a.txt test/b.txt
$(location  :fg ) => test/a.txt
x$(location :fg)y $(location :fg) => xtest/a.txty test/a.txt
plain $(foo) $$ $(location) $(locations) => plain $(foo) $$ $(location) $(locations)
$(execpath a.txt) $(rootpath b.txt) $(execpath d.txt) => test/a.txt test/b.txt test/d.txt
";

const SHORT: &str = "$(location :fg) => test/a.txt
$(locations :al) => test/a.txt
$(rootpath a.txt) => test/a.txt
$(execpath :fg) => test/a.txt
$(execpaths :gen) => bazel-out/k8-fastbuild/bin/test/g1.txt bazel-out/k8-fastbuild/bin/test/g2.txt
$(rootpaths :gen) => test/g1.txt test/g2.txt
$(location :tool) => test/tool
$(rlocationpath :tool) => _main/test/tool
$(execpath tool) => bazel-out/k8-fastbuild/bin/test/tool
$(execpath //test:fg) => test/a.txt
$(location @bazel_tools//tools/test:collect_coverage) => ../bazel_tools/tools/test/collect_coverage
$(rlocationpaths @bazel_tools//tools/test:collect_coverage) => bazel_tools/tools/test/collect_coverage
$(rlocationpaths :fg2) => _main/test/a.txt _main/test/b.txt
$(location c.txt) => test/c.txt
$(locations :fg2) => test/a.txt test/b.txt
$(location  :fg ) => test/a.txt
x$(location :fg)y $(location :fg) => xtest/a.txty test/a.txt
plain $(foo) $$ $(location) $(locations) => plain $(foo) $$ $(location) $(locations)
$(execpath a.txt) $(rootpath b.txt) $(execpath d.txt) => test/a.txt test/b.txt test/d.txt
";

#[test]
fn expand_location_uses_targets_outputs_srcs_deps_and_tools() {
    let f = Fixture::new();
    let built = f.built(&["//test:ok", "//test:short", "//sh:show"]);
    assert_eq!(output_text(&built, "bin/test/ok.txt"), OK);
    assert_eq!(output_text(&built, "bin/test/short.txt"), SHORT);
    // Genuine rules_shell sh_binary `env` (sh_executable.bzl:96-104):
    // expand_location(value, ctx.attr.data, short_paths = True), then Make
    // variables.
    assert_eq!(
        output_text(&built, "bin/sh/show.txt"),
        "COVERAGE=bazel_tools/tools/test/collect_coverage\nGREETING_FILE=_main/sh/greeting.txt\nGREETING_ROOT=sh/greeting.txt\nLIBS=sh/data.txt sh/lib.sh\nMIXED=$HOME sh/greeting.txt\nSRC=sh/bin.sh\n"
    );
}

#[test]
fn expand_location_errors_are_rule_errors_with_bazel_texts() {
    let f = Fixture::new();
    for (target, messages) in [
        (
            "//test:err_undeclared",
            vec![
                "in loc rule //test:err_undeclared: label '//test:nope' in $(location) expression is not a declared prerequisite of this rule",
            ],
        ),
        (
            "//test:err_multi",
            vec![
                "in loc rule //test:err_multi: label '//test:gen' in $(location) expression expands to more than one file, please use $(locations //test:gen) instead.  Files (at most 5 shown) are: [bazel-out/k8-fastbuild/bin/test/g1.txt, bazel-out/k8-fastbuild/bin/test/g2.txt]",
            ],
        ),
        (
            "//test:err_rlocation_multi",
            vec![
                "in loc rule //test:err_rlocation_multi: label '//test:fg2' in $(location) expression expands to more than one file, please use $(locations //test:fg2) instead.  Files (at most 5 shown) are: [_main/test/a.txt, _main/test/b.txt]",
            ],
        ),
        (
            "//test:err_invalid",
            vec![
                "in loc rule //test:err_invalid: invalid label in $(location) expression: invalid target name 'b:c': target names may not contain ':'",
            ],
        ),
        (
            "//test:err_unterminated",
            vec!["in loc rule //test:err_unterminated: unterminated $(location) expression"],
        ),
        (
            "//test:err_empty",
            vec![
                "in loc rule //test:err_empty: label '//test:empty' in $(locations) expression expands to no files",
            ],
        ),
        // Expansion continues after a rule error; every error is reported.
        (
            "//test:err_two",
            vec![
                "in loc rule //test:err_two: label '//test:nope1' in $(location) expression is not a declared prerequisite of this rule",
                "in loc rule //test:err_two: label '//test:nope2' in $(location) expression is not a declared prerequisite of this rule",
            ],
        ),
        // `data` is not in Starlark's location map (allowData false).
        (
            "//test:err_data_only",
            vec![
                "in loc rule //test:err_data_only: label '//test:e.txt' in $(location) expression is not a declared prerequisite of this rule",
            ],
        ),
        (
            "//test:err_dup",
            vec!["Label \"//test:c.txt\" is found more than once in 'targets' list."],
        ),
        (
            "//test:err_none",
            vec![
                "in call to expand_location(), parameter 'targets' got value of type 'NoneType', want 'sequence'",
            ],
        ),
        (
            "//test:err_type",
            vec!["at index 0 of targets, got element of type string, want Target"],
        ),
        (
            "//loc:x",
            vec!["file '//loc:defs.bzl' cannot use private API"],
        ),
        (
            "//sh:envbad",
            vec![
                "in sh_binary rule //sh:envbad: label '//sh:nope' in $(location) expression is not a declared prerequisite of this rule",
            ],
        ),
    ] {
        let output = f.run("build", &[target]);
        for message in messages {
            rejected(&output, message);
        }
    }
}

#[test]
fn targets_are_interned_per_configured_target() {
    let f = Fixture::new();
    let built = f.built(&["//ident:x"]);
    assert_eq!(
        output_text(&built, "bin/ident/x.txt"),
        "deps0==lk0 True\ndeps0==data0 True\ndeps0==single True\ndeps0==alias False\ndeps0==tool False\nlk[deps0] v\nin True\nset 3\n"
    );
}

#[test]
fn label_keyed_string_dict_concatenation_keeps_the_last_value() {
    let f = Fixture::new();
    let built = f.built(&["//lkm:all"]);
    for (name, text) in [
        (
            "sel_then_dict",
            "@@//lkm:p1=dict1\n@@//lkm:p2=dict2\n@@//lkm:p3=dict3\n",
        ),
        (
            "dict_then_sel",
            "@@//lkm:p3=dict3\n@@//lkm:p2=sel2\n@@//lkm:p1=sel1\n",
        ),
        (
            "default_only",
            "@@//lkm:p1=dict1\n@@//lkm:p2=dict2\n@@//lkm:p3=dict3\n",
        ),
        ("two_selects", "@@//lkm:p2=a2\n@@//lkm:p1=b1\n"),
    ] {
        assert_eq!(
            output_text(&built, &format!("bin/lkm/{name}.txt")),
            text,
            "{name}"
        );
    }
    let opt = f.built(&["--compilation_mode=opt", "//lkm:all"]);
    for (name, text) in [
        (
            "sel_then_dict",
            "@@//lkm:p1=dict1\n@@//lkm:p4=opt4\n@@//lkm:p3=dict3\n@@//lkm:p2=dict2\n",
        ),
        (
            "dict_then_sel",
            "@@//lkm:p3=dict3\n@@//lkm:p2=dict2\n@@//lkm:p4=opt4\n",
        ),
        (
            "two_selects",
            "@@//lkm:p2=a2\n@@//lkm:p1=a1\n@@//lkm:p3=b3\n",
        ),
    ] {
        assert_eq!(
            output_text(&opt, &format!("bin/lkm/{name}.txt")),
            text,
            "{name}"
        );
    }
    // Two spellings of one label in a branch fail at loading, selected or not.
    rejected(
        &f.run("build", &["//lkdup:all"]),
        "//lkdup:unselected_branch_dup: duplicate labels in each branch in select expression of attribute 'm' of 'lk' (including '//lkdup:opt'): //lkdup:p1 (as [\":p1\", \"//lkdup:p1\"])",
    );
}
