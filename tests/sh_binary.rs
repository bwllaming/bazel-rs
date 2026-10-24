//! N50: genuine rules_shell 0.6.1 sh_binary / sh_test / sh_library analysis
//! and build (docs/SH_BINARY.md): `attr.label_keyed_string_dict` rule values
//! (`ctx.attr` dict[Target, string], `ctx.files`, select(), dependency
//! validation and Bazel's errors), `flags = ["SKIP_CONSTRAINTS_OVERRIDE"]`,
//! `allow_rules`, `coverage_common.instrumented_files_info` and the
//! `skip_conflict_checking` allowlist. The rule sources are byte-identical
//! copies of the genuine module (tests/fixtures/sh_binary/README.md); the
//! workspace is tests/fixtures/sh_binary/workspace, whose expected outputs
//! and texts are genuine Bazel 9.3.0's (paired capture
//! tests/fixtures/sh_binary/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
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
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            copy_file(&entry.path(), &target);
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
                .prefix("sh-binary-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let root = f.0.path();
        copy_tree(&manifest().join("tests/fixtures/sh_binary/workspace"), root);
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
                "600s",
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

/// `//dump` of genuine Bazel 9.3.0: the DefaultInfo/runfiles model of the
/// genuine sh_binary, use_bash_launcher sh_binary, sh_library, sh_test, the
/// runfiles alias (root_symlinks via attr.label_keyed_string_dict) and
/// @bazel_tools' collect_coverage, a rules_shell sh_binary every test needs.
const DUMP: &str = r#"target @@bazel_tools//tools/test:collect_coverage
  files ../bazel_tools/tools/test/collect_coverage.sh ../bazel_tools/tools/test/collect_coverage
  executable ../bazel_tools/tools/test/collect_coverage
  default_runfiles.files ../bazel_tools/tools/test/collect_coverage.sh ../bazel_tools/tools/test/collect_coverage
  default_runfiles.symlinks 
  default_runfiles.root_symlinks 
  default_runfiles.empty_filenames 
  data_runfiles.files ../bazel_tools/tools/test/collect_coverage.sh ../bazel_tools/tools/test/collect_coverage
  data_runfiles.symlinks 
  data_runfiles.root_symlinks 
  data_runfiles.empty_filenames 
  env {} inherit []
target @@//sh:bin
  files sh/bin.sh sh/bin
  executable sh/bin
  default_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/bin.sh sh/bin
  default_runfiles.symlinks 
  default_runfiles.root_symlinks 
  default_runfiles.empty_filenames 
  data_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/bin.sh sh/bin
  data_runfiles.symlinks 
  data_runfiles.root_symlinks 
  data_runfiles.empty_filenames 
  env {"GREETING": "hi"} inherit ["HOME"]
target @@//sh:launcher
  files sh/bin.sh sh/launcher
  executable sh/launcher
  default_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/bin.sh sh/launcher
  default_runfiles.symlinks 
  default_runfiles.root_symlinks bazel_tools/tools/bash/runfiles/runfiles.bash=../rules_shell+/shell/runfiles/runfiles.bash
  default_runfiles.empty_filenames 
  data_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/bin.sh sh/launcher
  data_runfiles.symlinks 
  data_runfiles.root_symlinks bazel_tools/tools/bash/runfiles/runfiles.bash=../rules_shell+/shell/runfiles/runfiles.bash
  data_runfiles.empty_filenames 
  env {} inherit []
target @@//sh:lib
  files sh/lib.sh sh/data.txt
  executable None
  default_runfiles.files sh/data.txt sh/lib.sh
  default_runfiles.symlinks 
  default_runfiles.root_symlinks 
  default_runfiles.empty_filenames 
  data_runfiles.files sh/data.txt sh/lib.sh
  data_runfiles.symlinks 
  data_runfiles.root_symlinks 
  data_runfiles.empty_filenames 
target @@//sh:t
  files sh/t.sh sh/t
  executable sh/t
  default_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/t.sh sh/t
  default_runfiles.symlinks 
  default_runfiles.root_symlinks 
  default_runfiles.empty_filenames 
  data_runfiles.files sh/greeting.txt sh/data.txt sh/lib.sh sh/t.sh sh/t
  data_runfiles.symlinks 
  data_runfiles.root_symlinks 
  data_runfiles.empty_filenames 
  env {} inherit []
target @@rules_shell+//shell/runfiles:runfiles_impl
  files 
  executable None
  default_runfiles.files 
  default_runfiles.symlinks 
  default_runfiles.root_symlinks bazel_tools/tools/bash/runfiles/runfiles.bash=../rules_shell+/shell/runfiles/runfiles.bash
  default_runfiles.empty_filenames 
  data_runfiles.files 
  data_runfiles.symlinks 
  data_runfiles.root_symlinks bazel_tools/tools/bash/runfiles/runfiles.bash=../rules_shell+/shell/runfiles/runfiles.bash
  data_runfiles.empty_filenames 
"#;

#[test]
fn genuine_rules_shell_sh_binary_sh_test_and_sh_library_analyze_and_build() {
    let f = Fixture::new();
    let built = f.built(&["//dump", "//sh:all"]);
    assert_eq!(output_text(&built, "bin/dump/dump.txt"), DUMP);
    // ctx.actions.write of the bash launcher, the toolchain path from the
    // registered sh_toolchain (sh_executable.bzl:39-58).
    let launcher = output_text(&built, "bin/sh/launcher");
    assert!(
        launcher.starts_with("#!/bin/bash\n\n# --- begin runfiles.bash initialization v3 ---\n"),
        "{launcher}"
    );
    assert!(
        launcher.ends_with(
            "\nrunfiles_export_envvars\n\nexec \"$(rlocation \"_main/sh/bin.sh\")\" \"$@\"\n"
        ),
        "{launcher}"
    );
    // Without the launcher the entrypoint is a symlink to the script
    // (ctx.actions.symlink, sh_executable.bzl:61-65).
    let entry = built
        .iter()
        .find(|path| path.to_string_lossy().ends_with("bin/sh/bin"))
        .unwrap();
    assert!(
        fs::symlink_metadata(entry)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(entry).unwrap(),
        fs::read_to_string(f.0.path().join("sh/bin.sh")).unwrap()
    );
}

#[test]
fn label_keyed_string_dict_values_are_targets_in_key_order() {
    let f = Fixture::new();
    let built = f.built(&["//lk:all"]);
    assert_eq!(
        output_text(&built, "bin/lk/plain.txt"),
        "Target @@//lk:p2 second [\"lk/b.txt\"]\nTarget @@//lk:p1 first [\"lk/a.txt\"]\nTarget @@//lk:c.txt file [\"lk/c.txt\"]\nTarget @@//lk:fg group [\"lk/c.txt\", \"lk/a.txt\"]\nTarget @@//lk:p2 aliased [\"lk/b.txt\"]\nfiles lk/b.txt lk/a.txt lk/c.txt\ntype dict\nlen 5\nlookup second\nin True\nhash_dict 5\n"
    );
    // select() branches and `select() | dict` concatenation.
    assert_eq!(
        output_text(&built, "bin/lk/sel.txt"),
        "Target @@//lk:p2 dflt [\"lk/b.txt\"]\nTarget @@//lk:a.txt always [\"lk/a.txt\"]\nfiles lk/b.txt lk/a.txt\ntype dict\nlen 2\nlookup dflt\nin True\nhash_dict 2\n"
    );
    assert_eq!(
        output_text(&built, "bin/lk/rules.txt"),
        "Target @@//lk:p1 r [\"lk/a.txt\"]\nfiles lk/a.txt\ntype dict\nlen 1\nlookup r\nin True\nhash_dict 1\n"
    );
    assert_eq!(
        output_text(&built, "bin/lk/prov_ok.txt"),
        "Target @@//lk:p1 x [\"lk/a.txt\"]\nTarget @@//lk:p2 y [\"lk/b.txt\"]\nfiles lk/a.txt lk/b.txt\ntype dict\nlen 2\nlookup x\nin True\nhash_dict 2\n"
    );
    assert_eq!(
        output_text(&built, "bin/lk/flags.txt"),
        "Target @@//lk:a.txt f [\"lk/a.txt\"]\nfiles lk/a.txt\ntype dict\nlen 1\nlookup f\nin True\nhash_dict 1\n"
    );
    let opt = f.built(&["--compilation_mode=opt", "//lk:sel"]);
    assert_eq!(
        output_text(&opt, "bin/lk/sel.txt"),
        "Target @@//lk:p1 opt [\"lk/a.txt\"]\nTarget @@//lk:a.txt always [\"lk/a.txt\"]\nfiles lk/a.txt\ntype dict\nlen 2\nlookup opt\nin True\nhash_dict 2\n"
    );
}

#[test]
fn label_keyed_string_dict_and_allow_rules_errors_use_bazel_texts() {
    let f = Fixture::new();
    for (target, message) in [
        // LabelKeyedDictType.convert (packages/BuildType.java:449-490).
        (
            "//e1:all",
            "//e1:dup: duplicate labels in attribute 'm' of 'lk': //e1:a.txt (as [\"a.txt\", \":a.txt\", \"//e1:a.txt\"])",
        ),
        // RuleContext.Builder.validateRuleDependency (analysis/RuleContext.java:2002-2096).
        (
            "//e2:all",
            "in m attribute of lk_rules rule //e2:bad: genrule rule '//e2:g' is misplaced here (expected prov)",
        ),
        (
            "//e2:all",
            "in m attribute of lk_rules rule //e2:bad: alias '//e2:ga' referring to genrule rule '//e2:g' is misplaced here (expected prov)",
        ),
        (
            "//e3:all",
            "in m attribute of lk_prov rule //e3:bad: '//e3:fg' does not have mandatory providers: 'P'",
        ),
        (
            "//e4:all",
            "in m attribute of lk_strict rule //e4:bad: source file '//e4:f.txt' is misplaced here (expected no files)",
        ),
        // Attribute.Builder.setPropertyFlag (packages/Attribute.java:466-479).
        ("//e5:all", "unknown attribute flag 'BOGUS_FLAG'"),
        (
            "//e6:all",
            "in m attribute of lk_both rule //e6:bad: filegroup rule '//e6:fg' is misplaced here (expected prov) and '//e6:fg' does not have mandatory providers: 'P'",
        ),
        (
            "//bad:b",
            "in deps attribute of sh_binary rule //bad:b: genrule rule '//bad:g' is misplaced here (expected sh_library)",
        ),
    ] {
        rejected(&f.run("build", &[target]), message);
    }
    // SKIP_CONSTRAINTS_OVERRIDE and DIRECT_COMPILE_TIME_INPUT are modelled
    // no-ops (only --compile_one_dependency reads the latter), so the target
    // analyzes; ORDER_INDEPENDENT canonicalizes (N57) and other flags stay an
    // explicit gap.
    ok(&f.run("build", &["//flags:x"]));
}

#[test]
fn coverage_common_instrumented_files_info_and_private_api_allowlist() {
    let f = Fixture::new();
    let built = f.built(&["//cov:ok"]);
    assert_eq!(
        output_text(&built, "bin/cov/ok.txt"),
        "InstrumentedFilesInfo\nstruct(instrumented_files = depset([]), metadata_files = depset([]))\ndepset([])\ndepset([])\nTrue\n"
    );
    for (target, message) in [
        (
            "//cov:ctx",
            "in call to instrumented_files_info(), parameter 'ctx' got value of type 'NoneType', want 'ctx'",
        ),
        (
            "//cov:source",
            "at index 0 of source_attributes, got element of type int, want string",
        ),
        ("//cov:env", "file '//cov:defs.bzl' cannot use private API"),
        (
            "//cov:support",
            "at index 0 of coverage_support_files, got element of type int, want one of depset, File or FilesToRunProvider",
        ),
        (
            "//cov:seq",
            "in call to instrumented_files_info(), parameter 'dependency_attributes' got value of type 'string', want 'sequence'",
        ),
        // BuiltinRestriction.AllowlistEntry.reposMatch: `rules_shell+` is
        // allowlisted (the genuine root_symlinks rule above), other modules
        // are not.
        (
            "//skip:x",
            "file '@@other_rules+//:defs.bzl' cannot use private API",
        ),
    ] {
        rejected(&f.run("build", &[target]), message);
    }
}

#[test]
fn label_keyed_string_dict_keys_are_query_edges() {
    let f = Fixture::new();
    let output = f.run("query", &["deps(//lk:plain, 1)"]);
    ok(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    for label in [
        "//lk:p1",
        "//lk:p2",
        "//lk:p2_alias",
        "//lk:c.txt",
        "//lk:fg",
    ] {
        assert!(text.lines().any(|line| line == label), "{label} in {text}");
    }
    // Both select() branches and the concatenated dict (configurable query edges).
    let output = f.run("query", &["labels(m, //lk:sel)"]);
    ok(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    for label in ["//lk:p1", "//lk:p2", "//lk:a.txt"] {
        assert!(text.lines().any(|line| line == label), "{label} in {text}");
    }
}

/// `fs::copy`, except that a file with any execute bit is re-created through
/// `write_executable_script` (bytes and mode kept).
fn copy_file(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(from).unwrap().permissions().mode() & 0o7777;
    if mode & 0o111 != 0 {
        write_executable_script(to, fs::read(from).unwrap(), mode);
    } else {
        fs::copy(from, to).unwrap();
    }
}
