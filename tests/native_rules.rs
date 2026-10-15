//! CLI coverage for Bazel 9.3 native rule classes at loading
//! (docs/NATIVE_BUILD_RULES.md). The fixture mirrors real patterns: rules_go
//! generated constraint values, aliases, platforms and toolchains; rules_go
//! config_settings and the nogo alias(select); bazel_skylib's
//! config_setting_group alias chain; rules_python toolchain(target_settings =
//! select(...)); @bazel_tools//tools/cpp label_flag/toolchain_type and a
//! local_config_cc cc_toolchain_suite; the platforms module (constraint
//! settings/values, platform, module_name()/module_version(), REPO.bazel).
//!
//! Gate: `query //pkg:all` lists exactly the targets Bazel lists (sorted
//! labels, rules only, `manual` included), and analysis of the new classes,
//! or of filegroup/genrule values that need select() resolution, fails
//! explicitly before any action runs.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

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

impl Workspace {
    fn fixture() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("native-rules-cli-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_rules"),
            ws.path(),
        );
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

fn labels(package: &str, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("{package}:{name}\n"))
        .collect()
}

/// Expected `bazel query PKG:all` output (rules only, sorted, manual
/// included, FilteringPolicies.NO_FILTER).
const GATE: &[(&str, &[&str])] = &[
    ("//", &["module_native_rules_fixture"]),
    ("//go", &["toolchain"]),
    (
        "//go/toolchain",
        &[
            "amd64",
            "arm64",
            "cgo_constraint",
            "cgo_off",
            "cgo_on",
            "darwin",
            "go_linux_amd64_sdk",
            "go_linux_amd64_toolchain",
            "ios",
            "js",
            "js_wasm",
            "linux",
            "linux_amd64",
            "linux_amd64_cgo",
            "wasm",
            "windows",
        ],
    ),
    (
        "//go/private",
        &[
            "bootstrap_nogo",
            "default_nogo",
            "empty_setting",
            "is_strip_always",
            "is_strip_sometimes_fastbuild",
            "nogo_active",
            "noop_nogo",
            "request_nogo",
            "stamp",
        ],
    ),
    (
        "//skylib",
        &[
            "any_os",
            "any_os_2",
            "any_os_3",
            "linux",
            "macos",
            "only_linux",
            "unix",
            "unix_2",
            "windows",
        ],
    ),
    (
        "//python",
        &[
            "exec_tools_toolchain_enabled",
            "exec_tools_toolchain_type",
            "is_python_3.12",
            "python_3_12_py_exec_tools_toolchain",
            "python_3_12_toolchain",
            "toolchain_type",
        ],
    ),
    (
        "//tools/cpp",
        &[
            "cc_runtimes_toolchain_type",
            "compiler",
            "compiler_deps",
            "current_libc_top",
            "empty_lib",
            "libc_top",
            "link_extra_lib",
            "link_extra_libs",
            "toolchain",
            "toolchain_type",
        ],
    ),
    (
        "//misc",
        &[
            "all_tests",
            "configured_files",
            "copy",
            "defs_doc",
            "deps_query",
            "java_plugins_flag_alias",
            "plain",
            "unconditional_files",
        ],
    ),
    (
        "@@platforms//",
        &[
            "incompatible",
            "incompatible_setting",
            "license",
            "purl_pkg_bazel_platforms_1.1.0",
            "srcs",
        ],
    ),
    (
        "@@platforms//os",
        &["linux", "macos", "os", "osx", "srcs", "windows"],
    ),
    (
        "@@platforms//cpu",
        &["aarch64", "arm64", "cpu", "srcs", "x86_64"],
    ),
    ("@@platforms//host", &["host", "srcs"]),
];

#[test]
fn query_lists_native_rule_targets_like_bazel() {
    let ws = Workspace::fixture();
    for (package, names) in GATE {
        let pattern = format!("{}:all", package.replace("@@platforms", "@platforms"));
        let query = ws.run(&["query", &pattern]);
        assert!(query.status.success(), "{pattern}: {}", errors(&query));
        assert_eq!(stdout(&query), labels(package, names), "{pattern}");
    }
    // Implicit outputs and assumed input files are targets too.
    for target in [
        "//misc:defs_doc.binaryproto",
        "//misc:defs_doc.textproto",
        "//misc:defs.bzl",
        "//misc:linux.txt",
    ] {
        let query = ws.run(&["query", target]);
        assert!(query.status.success(), "{target}: {}", errors(&query));
        assert_eq!(stdout(&query), format!("{target}\n"));
    }
}

#[test]
fn analysis_of_unmodeled_native_rules_fails_explicitly() {
    let ws = Workspace::fixture();
    for (target, diagnostic) in [
        (
            "//go/private:default_nogo",
            "//go/private:default_nogo: native rule alias analysis not implemented in the native profile",
        ),
        (
            "//go/toolchain:go_linux_amd64_toolchain",
            "native rule toolchain analysis not implemented",
        ),
        (
            "//skylib:unix",
            "native rule alias analysis not implemented",
        ),
        (
            "//python:is_python_3.12",
            "native rule config_setting analysis not implemented",
        ),
        (
            "//tools/cpp:link_extra_libs",
            "native rule label_flag analysis not implemented",
        ),
        (
            "//misc:defs_doc.binaryproto",
            "//misc:defs_doc: native rule starlark_doc_extract analysis not implemented",
        ),
        (
            "//misc:copy",
            "//misc:copy: genrule accepted at load time but not analyzed by the native profile: attribute 'cmd' select() (configured attribute resolution)",
        ),
        (
            "//misc:configured_files",
            "attribute 'srcs' select() (configured attribute resolution)",
        ),
        (
            "@platforms//os:srcs",
            "filegroup accepted at load time but not analyzed by the native profile: package default_package_metadata (package metadata dependencies)",
        ),
    ] {
        let build = ws.run(&["build", target]);
        rejected(&build, diagnostic);
        assert!(!errors(&build).contains("actions executed"), "{target}");
    }
    assert!(!ws.path().join("private-cache/actions").exists());
}

#[test]
fn unconditional_selects_and_plain_native_rules_still_build() {
    let ws = Workspace::fixture();
    let build = ws.run(&["build", "//misc:plain"]);
    assert!(build.status.success(), "{}", errors(&build));
    let output = stdout(&build);
    let path = output.trim().split_once('\t').unwrap().1;
    assert_eq!(fs::read_to_string(path).unwrap(), "a\nb\n");
    // cc_toolchain_suite is a plain filegroup (exports.bzl:53-54).
    let suite = ws.run(&["build", "//tools/cpp:toolchain"]);
    assert!(suite.status.success(), "{}", errors(&suite));
}

#[test]
fn build_wildcards_skip_manual_targets_but_query_lists_them() {
    let ws = Workspace::fixture();
    // config_setting, toolchain, constraint_setting/value and platform default
    // to tags=["manual"] (ConfigRuleClasses.java:48-51, ToolchainRule.java:53-57,
    // ConstraintSettingRule.java:47-51, ConstraintValueRule.java:46-50,
    // PlatformRule.java:58-62).
    fs::create_dir_all(ws.path().join("manual")).unwrap();
    fs::write(
        ws.path().join("manual/BUILD.bazel"),
        "config_setting(name='c', values={'cpu': 'k8'})\ntoolchain(name='t', toolchain_type='//go:toolchain', toolchain=':f')\nconstraint_setting(name='s')\nconstraint_value(name='v', constraint_setting=':s')\nplatform(name='p')\nalias(name='hidden', actual=':f', tags=['manual'])\ngenrule(name='g', outs=['g.txt'], cmd='echo g > $@', tags=['manual'])\nfilegroup(name='f', srcs=['BUILD.bazel'])\n",
    )
    .unwrap();
    let query = ws.run(&["query", "//manual:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(
        stdout(&query),
        labels("//manual", &["c", "f", "g", "hidden", "p", "s", "t", "v"])
    );
    let build = ws.run(&["build", "//manual:all"]);
    assert!(build.status.success(), "{}", errors(&build));
    assert_eq!(stdout(&build).lines().count(), 1, "{}", stdout(&build));
    assert!(stdout(&build).starts_with("//manual:f\t"));
    // An explicit label is never filtered.
    rejected(
        &ws.run(&["build", "//manual:t"]),
        "native rule toolchain analysis not implemented",
    );
    let explicit = ws.run(&["build", "//manual:g"]);
    assert!(explicit.status.success(), "{}", errors(&explicit));
}

#[test]
fn load_errors_carry_bazel_messages_and_the_native_footer() {
    let ws = Workspace::fixture();
    fs::create_dir_all(ws.path().join("bad")).unwrap();
    for (build, diagnostic) in [
        (
            "alias(name='a', acutal=':b')",
            "//bad:a: no such attribute 'acutal' in 'alias' rule (did you mean 'actual'?)",
        ),
        (
            "toolchain(name='t', toolchain=':i', toolchain_type=select({'//conditions:default': ':x'}))",
            "//bad:t: attribute \"toolchain_type\" is not configurable",
        ),
        (
            "cc_toolchain_alias(name='x')",
            "This rule has been removed from Bazel. Please add a `load()` statement for it.",
        ),
    ] {
        fs::write(ws.path().join("bad/BUILD.bazel"), build).unwrap();
        let query = ws.run(&["query", "//bad:all"]);
        rejected(&query, diagnostic);
        assert!(errors(&query).ends_with(
            "Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() definitions. select() values load, but native analysis does not resolve them.\n"
        ));
    }
}

#[test]
fn nodep_labels_are_not_input_files_but_build_setting_defaults_are() {
    let ws = Workspace::fixture();
    fs::create_dir_all(ws.path().join("nodep")).unwrap();
    fs::write(ws.path().join("nodep/impl"), "impl\n").unwrap();
    fs::write(ws.path().join("nodep/x"), "x\n").unwrap();
    fs::write(
        ws.path().join("nodep/BUILD.bazel"),
        "toolchain(name='t', toolchain=':impl', toolchain_type='//go:toolchain')\nlabel_flag(name='f', build_setting_default=':x')\n",
    )
    .unwrap();
    // AggregatingAttributeMapper.visitLabels skips NODEP_LABEL attributes.
    let impl_query = ws.run(&["query", "//nodep:impl"]);
    assert!(!impl_query.status.success(), "{}", stdout(&impl_query));
    // Select keys are visited through $config_dependencies even on a NODEP
    // attribute (genuine capture B/nodep_platform_allowed_toolchain_types_select_key).
    fs::write(ws.path().join("nodep/key"), "key\n").unwrap();
    fs::write(
        ws.path().join("nodep/BUILD.bazel"),
        "toolchain(name='t', toolchain=':impl', toolchain_type='//go:toolchain')\nlabel_flag(name='f', build_setting_default=':x')\nplatform(name='p', allowed_toolchain_types=select({':key': [':impl'], '//conditions:default': []}))\n",
    )
    .unwrap();
    let key = ws.run(&["query", "//nodep:key"]);
    assert!(key.status.success(), "{}", errors(&key));
    assert!(!ws.run(&["query", "//nodep:impl"]).status.success());
    let x = ws.run(&["query", "//nodep:x"]);
    assert!(x.status.success(), "{}", errors(&x));
    assert_eq!(stdout(&x), "//nodep:x\n");
}

#[test]
fn pure_attribute_analysis_errors_use_bazel_messages() {
    let ws = Workspace::fixture();
    fs::create_dir_all(ws.path().join("checks")).unwrap();
    fs::write(
        ws.path().join("checks/BUILD.bazel"),
        "config_setting(name='empty')\nconfig_setting(name='empty_values', values={})\nconfig_setting(name='label_key', values={'//x:y': 'z'})\njava_plugins_flag_alias(name='java_plugins_flag_alias')\ngenrule(name='no_cmd', outs=['o.txt'])\n",
    )
    .unwrap();
    for (target, diagnostic) in [
        (
            "//checks:empty",
            "in config_setting rule //checks:empty: Either values, flag_values or constraint_values must be specified and non-empty",
        ),
        (
            "//checks:empty_values",
            "in config_setting rule //checks:empty_values: Either values, flag_values or constraint_values must be specified and non-empty",
        ),
        (
            "//checks:label_key",
            "in values attribute of config_setting rule //checks:label_key: '//x:y' is not a valid setting name, but appears to be a label. Did you mean to place it in flag_values instead?",
        ),
        (
            "//checks:java_plugins_flag_alias",
            "in java_plugins_flag_alias rule //checks:java_plugins_flag_alias: Rule //checks:java_plugins_flag_alias cannot use private rule",
        ),
        (
            "//checks:no_cmd",
            "in cmd attribute of genrule rule //checks:no_cmd: missing value for `cmd` attribute, you can also set `cmd_ps` or `cmd_bat` on Windows and `cmd_bash` on other platforms.",
        ),
    ] {
        rejected(&ws.run(&["build", target]), diagnostic);
    }
}

#[test]
fn repo_file_defaults_apply_to_recursive_queries() {
    let ws = Workspace::fixture();
    fs::write(
        ws.path().join("REPO.bazel"),
        "repo(default_visibility = ['//visibility:public'])\n",
    )
    .unwrap();
    let query = ws.run(&["query", "//go/..."]);
    assert!(query.status.success(), "{}", errors(&query));
    assert!(stdout(&query).starts_with("//go:toolchain\n"));
    fs::write(
        ws.path().join("REPO.bazel"),
        "ignore_directories(['misc'])\n",
    )
    .unwrap();
    rejected(
        &ws.run(&["query", "//go/..."]),
        "ignore_directories() is not implemented by the native profile yet (planned for the N14 ignore-files increment)",
    );
}
