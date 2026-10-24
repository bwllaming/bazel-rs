//! N53: the `toolchains` attribute and Make-variable expansion through the
//! CLI (docs/MAKE_VARIABLES.md): genrule commands with TemplateVariableInfo
//! variables from `toolchains` (aspect_rules_aws's resolved_toolchain
//! pattern, ~/code BUILD.bazel:449-462 verbatim) and from toolchain_type
//! entries, toolchain files as tool inputs, the configuration's variables,
//! location functions, `ctx.var` / `ctx.expand_make_variables` /
//! `ctx.expand_location` (the gogenrule.bzl shape), and Bazel's error texts.
//! Every expected byte and message is genuine Bazel 9.3's (paired capture
//! tests/fixtures/genrule_toolchains/reference_receipt.json). The workspace
//! is tests/fixtures/genrule_toolchains/workspace with N30's builtin-closure
//! stubs (tests/fixtures/test_flags/stubs).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/module_archives.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            copy_file(&entry.path(), &target);
        }
    }
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("genrule-toolchains-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        copy_tree(
            &manifest().join("tests/fixtures/genrule_toolchains/workspace"),
            f.0.path(),
        );
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        let mut module = "module(name = \"main\")\nbazel_dep(name = \"aspect_rules_aws\", version = \"0.8.3\")\nlocal_path_override(module_name = \"aspect_rules_aws\", path = \"aws_stub\")\nbazel_dep(name = \"rules_shell\", version = \"0.4.1\")\n".to_owned();
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
                stubs.join(name).display()
            ));
        }
        fs::write(f.0.path().join("MODULE.bazel"), module).unwrap();
        f
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        // Offline: execution platform resolution materializes @platforms.
        seed_module_archives(&self.0.path().join("cache"), &["platforms@1.0.0"]);
        let output = Command::new("timeout")
            .args([
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
        assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
        output
    }

    /// The output file contents of a successful `build ARGS`, by file name.
    fn built(&self, args: &[&str]) -> Vec<(String, String)> {
        let output = self.run("build", args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let (_, path) = line.split_once('\t').unwrap();
                let path = Path::new(path);
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read_to_string(path).unwrap(),
                )
            })
            .collect()
    }

    fn rejected(&self, target: &str, message: &str) {
        let output = self.run("build", &[target]);
        let text = stderr(&output);
        assert!(!output.status.success(), "{target}: {text}");
        let message = message.replace("<<WS>>", &self.0.path().display().to_string());
        assert!(
            text.lines()
                .any(|line| line == format!("bazel-rs: {message}") || line == message),
            "{target}: expected line {message:?} in:\n{text}"
        );
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn one(built: &[(String, String)], name: &str) -> String {
    built
        .iter()
        .find(|(file, _)| file == name)
        .unwrap_or_else(|| panic!("{name} not built: {built:?}"))
        .1
        .clone()
}

#[test]
fn genrule_toolchains_expand_make_variables_with_bazel_bytes() {
    let f = Fixture::new();
    let built = f.built(&[
        "//gt:aws_script",
        "//gt:aws-bin",
        "//gt:file_tool",
        "//gt:type_entry",
        "//gt:make_vars",
        "//gt:vars_and_type",
        "//gt:alias_entry",
        "//gt:locations",
        "//:root_locations",
    ]);
    // ~/code's aws_script: `$(AWS_CLI_BIN)` from the resolved toolchain,
    // `$$@` escaped for the shell.
    assert_eq!(
        one(&built, "aws_wrapper.sh"),
        "#!/bin/bash\nexec /opt/aws/dist/aws \"$@\"\n"
    );
    assert_eq!(one(&built, "aws-bin"), "#!/bin/sh\nexit 0\n");
    // The toolchain target's files are tool inputs (CommandHelper): the
    // generated tool is built before the genrule reads it.
    assert_eq!(
        one(&built, "file_tool.txt"),
        "GENERATED TOOL\nexternal/aspect_rules_aws+/aws/gen_tool.sh\n"
    );
    // A toolchain_type entry is an optional toolchain type.
    assert_eq!(one(&built, "type_entry.txt"), "/opt/aws/dist/aws\n");
    assert_eq!(
        one(&built, "make_vars.txt"),
        "from_vars1 first bar2 tv_mode from_vars1-bazel-out/k8-fastbuild/bin bazel-out/k8-fastbuild/bin bazel-out/k8-fastbuild/bin x86_64 bazel-out/k8-fastbuild/bin/gt bazel-out/k8-fastbuild/bin/gt gt/in.txt gt/in2.txt bazel-out/k8-fastbuild/bin/gt/make_vars.txt unset\n"
    );
    assert_eq!(
        one(&built, "vars_and_type.txt"),
        "from_vars1 /opt/aws/dist/aws gt/in.txt gt/in2.txt\n"
    );
    assert_eq!(one(&built, "alias_entry.txt"), "/opt/aws/dist/aws\n");
    assert_eq!(
        one(&built, "locations.txt"),
        "gt/in.txt gt/in.txt gt/in.txt _main/gt/in.txt gt/in.txt gt/in2.txt gt/tool.sh aspect_rules_aws+/aws/gen_tool.sh bazel-out/k8-fastbuild/bin/gt/locations.txt\n"
    );
    assert_eq!(
        one(&built, "root_locations.txt"),
        "./root.txt ./root.txt ./root.txt _main/root.txt root.txt\n"
    );
    // `--define` beats the toolchains' variables, which beat the global
    // ones; `--define=SRCS` does not reach the genrule's own `$(SRCS)`.
    let built = f.built(&[
        "//gt:make_vars",
        "--define=FOO=cli",
        "--define=SRCS=no",
        "-c",
        "dbg",
    ]);
    assert_eq!(
        one(&built, "make_vars.txt"),
        "cli first bar2 tv_mode cli-bazel-out/k8-dbg/bin bazel-out/k8-dbg/bin bazel-out/k8-dbg/bin x86_64 bazel-out/k8-dbg/bin/gt bazel-out/k8-dbg/bin/gt gt/in.txt gt/in2.txt bazel-out/k8-dbg/bin/gt/make_vars.txt unset\n"
    );
}

#[test]
fn genrule_expansion_and_toolchains_errors_use_bazel_texts() {
    let f = Fixture::new();
    for (target, message) in [
        (
            "type_entry_location",
            "<<WS>>/gt/BUILD.bazel:35:8: in cmd attribute of genrule rule //gt:type_entry_location: label '@@aspect_rules_aws+//aws:aws_toolchain' in $(location) expression expands to no files",
        ),
        (
            "err_undefined",
            "<<WS>>/gt/BUILD.bazel:118:8: in cmd attribute of genrule rule //gt:err_undefined: $(UNDEFINED_X) not defined",
        ),
        (
            "err_brace",
            "<<WS>>/gt/BUILD.bazel:120:8: in cmd attribute of genrule rule //gt:err_brace: '${FOO}' syntax is not supported; use '$(FOO)' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell",
        ),
        (
            "err_dollar",
            "<<WS>>/gt/BUILD.bazel:122:8: in cmd attribute of genrule rule //gt:err_dollar: '$FOO' syntax is not supported; use '$(FOO)' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell",
        ),
        (
            "err_unterminated",
            "<<WS>>/gt/BUILD.bazel:124:8: in cmd attribute of genrule rule //gt:err_unterminated: unterminated variable reference",
        ),
        (
            "err_function",
            "<<WS>>/gt/BUILD.bazel:126:8: in cmd attribute of genrule rule //gt:err_function: $(nope) not defined",
        ),
        (
            "err_location_undeclared",
            "<<WS>>/gt/BUILD.bazel:128:8: in cmd attribute of genrule rule //gt:err_location_undeclared: label '//gt:in.txt' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "err_location_multi",
            "<<WS>>/gt/BUILD.bazel:130:8: in cmd attribute of genrule rule //gt:err_location_multi: label '//gt:two_files' in $(location) expression expands to more than one file, please use $(locations //gt:two_files) instead.  Files (at most 5 shown) are: [gt/in.txt, gt/in2.txt]",
        ),
        (
            "err_location_none",
            "<<WS>>/gt/BUILD.bazel:132:8: in cmd attribute of genrule rule //gt:err_location_none: label '//gt:vars2' in $(location) expression expands to no files",
        ),
        (
            "err_location_label",
            "<<WS>>/gt/BUILD.bazel:134:8: in cmd attribute of genrule rule //gt:err_location_label: invalid label in $(location) expression: invalid target name 'a:b': target names may not contain ':'",
        ),
        (
            "err_src_singleton",
            "<<WS>>/gt/BUILD.bazel:136:8: in cmd attribute of genrule rule //gt:err_src_singleton: variable '$<' : more than one input file",
        ),
        (
            "err_out_singleton",
            "<<WS>>/gt/BUILD.bazel:138:8: in cmd attribute of genrule rule //gt:err_out_singleton: variable '$@' : more than one output file",
        ),
        (
            "err_recursion",
            "<<WS>>/gt/BUILD.bazel:140:8: in cmd attribute of genrule rule //gt:err_recursion: potentially unbounded recursion during expansion of '$(B)'",
        ),
        (
            "err_provider",
            "<<WS>>/gt/BUILD.bazel:142:8: in toolchains attribute of genrule rule //gt:err_provider: '//gt:two_files' does not have mandatory providers: 'TemplateVariableInfo' or 'ToolchainTypeInfo'",
        ),
        (
            "err_provider_starlark",
            "<<WS>>/gt/BUILD.bazel:144:8: in toolchains attribute of genrule rule //gt:err_provider_starlark: '//gt:plain_rule' does not have mandatory providers: 'TemplateVariableInfo' or 'ToolchainTypeInfo'",
        ),
        (
            "err_source",
            "<<WS>>/gt/BUILD.bazel:146:8: in toolchains attribute of genrule rule //gt:err_source: source file '//gt:in.txt' is misplaced here (expected no files)",
        ),
        (
            "err_generated",
            "<<WS>>/gt/BUILD.bazel:148:8: in toolchains attribute of genrule rule //gt:err_generated: generated file '//gt:make_vars.txt' is misplaced here (expected no files)",
        ),
    ] {
        f.rejected(&format!("//gt:{target}"), message);
    }
}

#[test]
fn starlark_toolchains_attribute_feeds_ctx_var_and_expansion() {
    let f = Fixture::new();
    let built = f.built(&[
        "//gt:dump",
        "//gt:dump_files",
        "//gt:dump_alias",
        "//gt:gogen",
        "//gt:locs_ok",
    ]);
    assert_eq!(
        one(&built, "dump.txt"),
        "var={\"TARGET_CPU\": \"x86_64\", \"COMPILATION_MODE\": \"tv_mode\", \"BINDIR\": \"bazel-out/k8-fastbuild/bin\", \"GENDIR\": \"bazel-out/k8-fastbuild/bin\", \"AWS_CLI_BIN\": \"/opt/aws/dist/aws\", \"BAR\": \"bar2\", \"FOO\": \"from_vars1\", \"NESTED\": \"$(FOO)-$(BINDIR)\", \"SHARED\": \"second\"}\nvar_type=dict\nattr.toolchains=[<target //gt:vars1>, <target //gt:vars2>, <target @@aspect_rules_aws+//aws:resolved_toolchain>]\nfiles.toolchains=[]\nexpand_empty=from_vars1 first bar2 /opt/aws/dist/aws from_vars1-bazel-out/k8-fastbuild/bin tv_mode $HOME\nexpand_var=from_vars1 second bar2 /opt/aws/dist/aws from_vars1-bazel-out/k8-fastbuild/bin tv_mode $HOME\nexpand_extra=extra first bar2 /opt/aws/dist/aws extra-bazel-out/k8-fastbuild/bin tv_mode $HOME\n"
    );
    assert_eq!(
        one(&built, "dump_files.txt"),
        "var={\"TARGET_CPU\": \"x86_64\", \"COMPILATION_MODE\": \"fastbuild\", \"BINDIR\": \"bazel-out/k8-fastbuild/bin\", \"GENDIR\": \"bazel-out/k8-fastbuild/bin\", \"AWS_CLI_BIN\": \"external/aspect_rules_aws+/aws/gen_tool.sh\", \"FILES\": \"yes\"}\nvar_type=dict\nattr.toolchains=[<target //gt:vars_files>, <target @@aspect_rules_aws+//aws:file_toolchain>]\nfiles.toolchains=[\"gt/in.txt\", \"gt/in2.txt\", \"bazel-out/k8-fastbuild/bin/external/aspect_rules_aws+/aws/gen_tool.sh\"]\nexpand_empty=yes external/aspect_rules_aws+/aws/gen_tool.sh\nexpand_var=yes external/aspect_rules_aws+/aws/gen_tool.sh\nexpand_extra=yes external/aspect_rules_aws+/aws/gen_tool.sh\n"
    );
    assert_eq!(
        one(&built, "dump_alias.txt"),
        "var={\"TARGET_CPU\": \"x86_64\", \"COMPILATION_MODE\": \"fastbuild\", \"BINDIR\": \"bazel-out/k8-fastbuild/bin\", \"GENDIR\": \"bazel-out/k8-fastbuild/bin\", \"AWS_CLI_BIN\": \"/opt/aws/dist/aws\"}\nvar_type=dict\nattr.toolchains=[<alias target //gt:resolved_alias of @@aspect_rules_aws+//aws:resolved_toolchain>]\nfiles.toolchains=[]\nexpand_empty=/opt/aws/dist/aws\nexpand_var=/opt/aws/dist/aws\nexpand_extra=/opt/aws/dist/aws\n"
    );
    // src/beyondwork.ai/gogenrule.bzl's
    // ctx.expand_make_variables("cmd", ctx.expand_location(ctx.attr.cmd), ctx.var).
    assert_eq!(
        one(&built, "args_gen.go"),
        "gt/tool.sh -o bazel-out/k8-fastbuild/bin/gt/args_gen.go gt/args.peg /opt/aws/dist/aws from_vars1 unset\n"
    );
    assert_eq!(
        one(&built, "locs_ok.out"),
        "expand_location=gt/in.txt gt/in.txt gt/in2.txt gt/in.txt gt/in2.txt _main/gt/in.txt bazel-out/k8-fastbuild/bin/gt/locs_ok.out $(FOO) $$X\n"
    );
    let built = f.built(&["//gt:dump", "--define=FOO=cli", "-c", "dbg"]);
    assert!(
        one(&built, "dump.txt").starts_with("var={\"TARGET_CPU\": \"x86_64\", \"COMPILATION_MODE\": \"tv_mode\", \"BINDIR\": \"bazel-out/k8-dbg/bin\", \"GENDIR\": \"bazel-out/k8-dbg/bin\", \"AWS_CLI_BIN\": \"/opt/aws/dist/aws\", \"BAR\": \"bar2\", \"FOO\": \"cli\","),
        "{built:?}"
    );
    for (target, message) in [
        (
            "dump_undefined",
            "<<WS>>/gt/BUILD.bazel:169:5: in cmd attribute of dump rule //gt:dump_undefined: $(BAR) not defined",
        ),
        (
            "star_err_provider",
            "<<WS>>/gt/BUILD.bazel:175:5: in toolchains attribute of dump rule //gt:star_err_provider: '//gt:two_files' does not have mandatory providers: 'TemplateVariableInfo'",
        ),
        (
            "star_err_type",
            "<<WS>>/gt/BUILD.bazel:177:5: in toolchains attribute of dump rule //gt:star_err_type: '@@aspect_rules_aws+//aws:toolchain_type' does not have mandatory providers: 'TemplateVariableInfo'",
        ),
        (
            "star_err_source",
            "<<WS>>/gt/BUILD.bazel:179:5: in toolchains attribute of dump rule //gt:star_err_source: source file '//gt:in.txt' is misplaced here (expected no files)",
        ),
        (
            "locs_undeclared",
            "<<WS>>/gt/BUILD.bazel:200:5: in locs rule //gt:locs_undeclared: label '//gt:in2.txt' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "locs_unterminated",
            "<<WS>>/gt/BUILD.bazel:202:5: in locs rule //gt:locs_unterminated: unterminated $(location) expression",
        ),
        (
            "locs_multi",
            "<<WS>>/gt/BUILD.bazel:204:5: in locs rule //gt:locs_multi: label '//gt:two_files' in $(location) expression expands to more than one file, please use $(locations //gt:two_files) instead.  Files (at most 5 shown) are: [gt/in.txt, gt/in2.txt]",
        ),
    ] {
        f.rejected(&format!("//gt:{target}"), message);
    }
    // Identical attribute errors are reported once.
    let output = f.run("build", &["//gt:dump_undefined"]);
    assert_eq!(stderr(&output).matches("$(BAR) not defined").count(), 1);
    let output = f.run("build", &["//gt:locs_duplicate"]);
    assert!(
        stderr(&output)
            .contains("Label \"//gt:in.txt\" is found more than once in 'targets' list."),
        "{}",
        stderr(&output)
    );
}

#[test]
fn query_lists_toolchains_edges_and_cquery_stays_unsupported() {
    let f = Fixture::new();
    for (expression, expected) in [
        (
            "deps(//gt:aws_script, 1)",
            "//gt:aws_script\n@aspect_rules_aws//aws:resolved_toolchain\n@bazel_tools//tools/genrule:genrule-setup.sh\n",
        ),
        (
            "deps(//gt:dump, 1)",
            "//gt:dump\n//gt:vars1\n//gt:vars2\n@aspect_rules_aws//aws:resolved_toolchain\n",
        ),
        (
            "deps(//gt:aws_script)",
            "//gt:aws_script\n@aspect_rules_aws//aws:resolved_toolchain\n@aspect_rules_aws//aws:toolchain_type\n@bazel_tools//tools/genrule:genrule-setup.sh\n",
        ),
        (
            "deps(//gt:vars_and_type, 1)",
            "//gt:vars1\n//gt:vars_and_type\n//gt:vars_files\n@aspect_rules_aws//aws:toolchain_type\n@bazel_tools//tools/genrule:genrule-setup.sh\n",
        ),
    ] {
        let output = f.run("query", &[expression]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }
    let output = f.run("cquery", &["deps(//gt:aws_script, 1)"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unsupported command"),
        "{}",
        stderr(&output)
    );
}

/// AliasProvider.getDependencyLabels (analysis/AliasProvider.java:84-97) is
/// the label as written and the actual label only: genuine Bazel 9.3 rejects
/// the middle alias of a chain (receipt post_capture_changes.alias_chain_followup),
/// and genrule `srcs` map only the label as written (BazelGenRule.collectSources).
#[test]
fn location_maps_skip_the_middle_of_alias_chains() {
    let f = Fixture::new();
    let built = f.built(&["//gt:chain_ends", "//gt:chain_tools"]);
    assert_eq!(
        one(&built, "chain_ends.txt"),
        "expand_location=gt/in.txt gt/in.txt\n"
    );
    assert_eq!(one(&built, "chain_tools.txt"), "gt/in.txt gt/in.txt\n");
    for (target, message) in [
        (
            "chain_middle",
            "<<WS>>/gt/BUILD.bazel:217:5: in locs rule //gt:chain_middle: label '//gt:chain_mid' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "chain_targets",
            "<<WS>>/gt/BUILD.bazel:219:5: in locs rule //gt:chain_targets: label '//gt:chain_mid' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "chain_tools_middle",
            "<<WS>>/gt/BUILD.bazel:223:8: in cmd attribute of genrule rule //gt:chain_tools_middle: label '//gt:chain_mid' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "chain_srcs_actual",
            "<<WS>>/gt/BUILD.bazel:225:8: in cmd attribute of genrule rule //gt:chain_srcs_actual: label '//gt:in.txt' in $(location) expression is not a declared prerequisite of this rule",
        ),
    ] {
        f.rejected(&format!("//gt:{target}"), message);
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
