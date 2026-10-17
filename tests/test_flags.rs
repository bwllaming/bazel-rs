//! N30: `--use_target_platform_for_tests`, the default test exec group,
//! `config_common.FeatureFlagInfo` and `config_feature_flag` through the CLI
//! (docs/TEST_FLAGS.md). Expected texts are genuine Bazel 9.3 strings
//! (paired capture tests/fixtures/test_flags/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

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

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("test-flags-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main')\n");
        f
    }

    /// A root module that overrides the toolchain-registering modules of the
    /// builtin closure with tests/fixtures/test_flags/stubs.
    fn stubbed() -> Self {
        let f = Self::new();
        let stubs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_flags/stubs");
        let mut module = "module(name = 'main')\n".to_owned();
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = '{name}', path = '{}')\n",
                stubs.join(name).display()
            ));
        }
        f.file("MODULE.bazel", &module);
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
                "300s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }

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
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    stderr(output)
}

fn rejected(output: &Output, message: &str) -> String {
    let text = stderr(output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
    text
}

const SHOW: &str = r#"
def _show(ctx):
    print("SHOW", ctx.label, ctx.attr.s)
    return []
show = rule(implementation = _show, attrs = {"s": attr.string()})
"#;

const WARNING: &str = "WARNING: Option 'use_target_platform_for_tests' is deprecated: Tests select an execution platform matching all constraints of the target platform by default. Instead of using this flag, make sure that all test target platform are registered as execution platforms.";

#[test]
fn use_target_platform_for_tests_is_a_selectable_deprecated_option() {
    let f = Fixture::new();
    f.file("defs.bzl", SHOW);
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
config_setting(name = "utp", values = {"use_target_platform_for_tests": "true"})
config_setting(name = "utp_bad", values = {"use_target_platform_for_tests": "maybe"})
show(name = "sel", s = select({":utp": "T", "//conditions:default": "F"}))
show(name = "sel_bad", s = select({":utp_bad": "T", "//conditions:default": "F"}))
"#,
    );
    let text = ok(&f.run("build", &["//:sel"]));
    assert!(
        text.contains("SHOW //:sel F") && !text.contains("WARNING"),
        "{text}"
    );
    let text = ok(&f.run(
        "build",
        &[
            "--use_target_platform_for_tests",
            "--use_target_platform_for_tests=yes",
            "//:sel",
        ],
    ));
    assert_eq!(text.matches(WARNING).count(), 1, "{text}");
    assert!(text.contains("SHOW //:sel T"), "{text}");
    let text = ok(&f.run("build", &["--use_target_platform_for_tests=0", "//:sel"]));
    assert!(
        text.contains(WARNING) && text.contains("SHOW //:sel F"),
        "{text}"
    );
    let text = ok(&f.run("build", &["--nouse_target_platform_for_tests", "//:sel"]));
    assert!(text.contains("SHOW //:sel F"), "{text}");
    rejected(
        &f.run(
            "build",
            &["--use_target_platform_for_tests=maybe", "//:sel"],
        ),
        "While parsing option --use_target_platform_for_tests=maybe: 'maybe' is not a boolean",
    );
    rejected(
        &f.run("build", &["//:sel_bad"]),
        "in values attribute of config_setting rule //:utp_bad: error while parsing configuration settings: While parsing option --use_target_platform_for_tests=maybe: 'maybe' is not a boolean",
    );
    // A CoreOptions option: query accepts (and ignores) it.
    let output = f.run("query", &["--use_target_platform_for_tests", "//:sel"]);
    let text = ok(&output);
    assert!(text.contains(WARNING), "{text}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//:sel\n");
}

const FEATURE_RULES: &str = r#"
FFI = config_common.FeatureFlagInfo
def _ret(ctx):
    return [FFI(value = ctx.attr.v)]
ret = rule(implementation = _ret, attrs = {"v": attr.string()}, provides = [FFI])
def _bs(ctx):
    print("BS", ctx.label, ctx.build_setting_value)
    return [FFI(value = ctx.attr.v)]
bs = rule(implementation = _bs, build_setting = config.string(flag = True), attrs = {"v": attr.string()})
def _probe(ctx):
    for d in ctx.attr.deps:
        f = d[FFI]
        print("D", d.label, FFI in d, type(f), repr(f), f.value, f.error, f.is_valid_value("a"), f.is_valid_value("zz"))
    return []
probe = rule(implementation = _probe, attrs = {"deps": attr.label_list(providers = [FFI])})
"#;

#[test]
fn starlark_feature_flag_info_keys_match_by_provider_value() {
    let f = Fixture::new();
    f.file("defs.bzl", &format!("{FEATURE_RULES}{SHOW}"));
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "bs", "probe", "ret", "show")
ret(name = "r", v = "hello")
bs(name = "s", build_setting_default = "cmd", v = "prov")
alias(name = "r_alias", actual = ":r")
config_setting(name = "r_hello", flag_values = {":r_alias": "hello"})
config_setting(name = "r_other", flag_values = {":r": "other"})
config_setting(name = "s_prov", flag_values = {":s": "prov"})
config_setting(name = "s_cmd", flag_values = {":s": "cmd"})
show(name = "sel_r", s = select({":r_hello": "H", ":r_other": "O"}))
show(name = "sel_r_other", s = select({":r_other": "O"}))
show(name = "sel_s", s = select({":s_prov": "P", ":s_cmd": "C"}))
probe(name = "p", deps = [":r", ":r_alias"])
"#,
    );
    let text = ok(&f.run("build", &["//:sel_r", "//:sel_s"]));
    assert!(text.contains("SHOW //:sel_r H"), "{text}");
    // The provider value wins over the build setting's own (command-line)
    // value (ConfigSetting.java:656-680), and the key's implementation runs
    // before the config_setting is matched.
    assert!(text.contains("SHOW //:sel_s P"), "{text}");
    let bs = text.find("BS //:s cmd").expect(&text);
    assert!(bs < text.find("SHOW //:sel_s").unwrap(), "{text}");
    let text = ok(&f.run("build", &["--//:s=other", "//:sel_s"]));
    assert!(
        text.contains("BS //:s other") && text.contains("SHOW //:sel_s P"),
        "{text}"
    );
    let text = rejected(
        &f.run("build", &["//:sel_r_other"]),
        "configurable attribute \"s\" in //:sel_r_other doesn't match this configuration. Would a default condition help?",
    );
    assert!(text.contains("Conditions checked:\n //:r_other"), "{text}");
    let text = ok(&f.run("build", &["//:p"]));
    assert!(
        text.contains("D //:r True FeatureFlagInfo struct(error = None, value = \"hello\") hello None True True"),
        "{text}"
    );
}

/// Rules for flag_values keys whose implementations run early
/// (Engine::plan_pending), before the rest of phase 1: they still get the
/// prerequisite/duplicate checks, per-edge (transitioned) configurations,
/// rule-transition delegation and runfiles of ordinary planning.
const EARLY_RULES: &str = r#"
FFI = config_common.FeatureFlagInfo
Marker = provider(fields = [])
Val = provider(fields = ["v"])
ALLOW = {"_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")}
def _sf(ctx):
    return []
sf = rule(implementation = _sf, build_setting = config.string(flag = True))
def _set_t(settings, attr):
    return {"//:sf": "t"}
set_t = transition(implementation = _set_t, inputs = [], outputs = ["//:sf"])
def _leaf(ctx):
    return [Val(v = ctx.attr.v)]
leaf = rule(implementation = _leaf, attrs = {"v": attr.string()})
def _needs(ctx):
    print("RAN", ctx.label)
    return [FFI(value = "x")]
needs = rule(implementation = _needs, attrs = {
    "deps": attr.label_list(providers = [Marker]),
    "any": attr.label_list(),
})
def _via_edge(ctx):
    print("EDGE", ctx.label, ctx.attr.dep[0][Val].v)
    return [FFI(value = ctx.attr.dep[0][Val].v)]
via_edge = rule(implementation = _via_edge, attrs = dict(ALLOW, dep = attr.label_list(cfg = set_t)))
def _constant(ctx):
    print("RULE_TR", ctx.label)
    return [FFI(value = "rt")]
via_rule = rule(implementation = _constant, cfg = set_t, attrs = ALLOW)
def _with_data(ctx):
    return [FFI(value = "r"), DefaultInfo(runfiles = ctx.runfiles(files = ctx.files.data))]
with_data = rule(implementation = _with_data, attrs = {"data": attr.label_list(allow_files = True)})
def _rf_probe(ctx):
    for d in ctx.attr.deps:
        print("RF", d.label, sorted([f.short_path for f in d[DefaultInfo].default_runfiles.files.to_list()]), ctx.attr.s)
    return []
rf_probe = rule(implementation = _rf_probe, attrs = {"deps": attr.label_list(), "s": attr.string()})
"#;

#[test]
fn early_planned_flag_keys_keep_checks_edge_configurations_and_runfiles() {
    let f = Fixture::new();
    f.file("defs.bzl", &format!("{EARLY_RULES}{SHOW}"));
    f.file(
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        "package_group(name = \"function_transition_allowlist\", packages = [\"public\"])\n",
    );
    f.file("data.txt", "data\n");
    f.file("a.txt", "a\n");
    f.file("dir/inner.txt", "inner\n");
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "leaf", "needs", "rf_probe", "sf", "show", "via_edge", "via_rule", "with_data")
sf(name = "sf", build_setting_default = "")
config_setting(name = "sf_t", flag_values = {":sf": "t"})
leaf(name = "leaf", v = select({":sf_t": "T", "//conditions:default": "D"}))
leaf(name = "plain")
needs(name = "bad_providers", deps = [":plain"])
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
needs(name = "bad_dup", any = [":leaf"] + select({":dbg": [], "//conditions:default": [":leaf"]}))
config_setting(name = "providers_x", flag_values = {":bad_providers": "x"})
config_setting(name = "dup_x", flag_values = {":bad_dup": "x"})
show(name = "sel_providers", s = select({":providers_x": "X", "//conditions:default": "-"}))
show(name = "sel_dup", s = select({":dup_x": "X", "//conditions:default": "-"}))
via_edge(name = "edge", dep = [":leaf"])
config_setting(name = "edge_t", flag_values = {":edge": "T"})
show(name = "sel_edge", s = select({":edge_t": "transitioned", "//conditions:default": "untransitioned"}))
via_rule(name = "rule_tr")
config_setting(name = "rule_tr_rt", flag_values = {":rule_tr": "rt"})
show(name = "sel_rule_tr", s = select({":rule_tr_rt": "handed back", "//conditions:default": "lost"}))
filegroup(name = "group", srcs = ["a.txt"])
with_data(name = "data", data = ["data.txt", ":group"])
config_setting(name = "data_r", flag_values = {":data": "r"})
rf_probe(name = "rf", deps = [":data"], s = select({":data_r": "matched", "//conditions:default": "-"}))
with_data(name = "data_dir", data = ["dir"])
config_setting(name = "data_dir_r", flag_values = {":data_dir": "r"})
show(name = "sel_dir", s = select({":data_dir_r": "D", "//conditions:default": "-"}))
"#,
    );
    // (i) N27's prerequisite pass and the held-back duplicate-label errors
    // run before an early-planned implementation, which then never runs.
    let text = rejected(
        &f.run("build", &["//:sel_providers"]),
        "in deps attribute of needs rule //:bad_providers: '//:plain' does not have mandatory providers: 'Marker'",
    );
    assert!(!text.contains("RAN"), "{text}");
    let text = rejected(
        &f.run("build", &["//:sel_dup"]),
        "in any attribute of needs rule //:bad_dup: Label '//:leaf' is duplicated\n",
    );
    assert!(!text.contains("RAN"), "{text}");
    // (ii) N18c: an early-planned key sees its dependency in the attribute
    // transition's configuration, and a rule-transitioned key hands its
    // providers back to the requested key.
    let text = ok(&f.run("build", &["//:sel_edge"]));
    assert!(
        text.contains("EDGE //:edge T") && text.contains("SHOW //:sel_edge transitioned"),
        "{text}"
    );
    let text = ok(&f.run("build", &["//:sel_rule_tr"]));
    assert!(
        text.contains("RULE_TR //:rule_tr") && text.contains("SHOW //:sel_rule_tr handed back"),
        "{text}"
    );
    // (iii) N23a: runfiles of an early-planned key reach its dependents,
    // and N27's source-directory gate covers a requested early-planned
    // key's runfiles.
    let text = ok(&f.run("build", &["//:rf"]));
    assert!(
        text.contains("RF //:data [\"a.txt\", \"data.txt\"] matched"),
        "{text}"
    );
    rejected(
        &f.run("build", &["//:sel_dir", "//:data_dir"]),
        "//:data_dir: source directory '//:dir' in its runfiles is not supported by the native profile",
    );
}

#[test]
fn feature_flag_info_constructor_follows_java_binding() {
    let f = Fixture::new();
    for (expression, message) in [
        (
            "FFI(value = None)",
            "in call to FeatureFlagInfo(), parameter 'value' got value of type 'NoneType', want 'string'",
        ),
        (
            "FFI()",
            "FeatureFlagInfo() missing 1 required positional argument: value",
        ),
        (
            "FFI(value = 'a', other = 1)",
            "FeatureFlagInfo() got unexpected keyword argument 'other'",
        ),
        (
            "FFI('x').is_valid_value(1)",
            "in call to is_valid_value(), parameter 'value' got value of type 'int', want 'string'",
        ),
    ] {
        f.file(
            "defs.bzl",
            &format!(
                "FFI = config_common.FeatureFlagInfo\ndef _impl(ctx):\n    i = FFI('x')\n    print('T', type(i), repr(i), dir(i), i == i, i == FFI('x'), {{i: 1}}[i])\n    {expression}\n    return []\nr = rule(implementation = _impl)\n"
            ),
        );
        f.file("BUILD.bazel", "load(':defs.bzl', 'r')\nr(name = 'x')\n");
        let text = rejected(&f.run("build", &["//:x"]), message);
        assert!(
            text.contains(
                "T FeatureFlagInfo struct(error = None, value = \"x\") [\"error\", \"is_valid_value\", \"value\"] True False 1"
            ),
            "{text}"
        );
    }
    f.file(
        "defs.bzl",
        "def _impl(ctx):\n    i = config_common.FeatureFlagInfo(value = 'x')\n    i.value = 'z'\n    return []\nr = rule(implementation = _impl)\n",
    );
    rejected(
        &f.run("build", &["//:x"]),
        "cannot set .value field of FeatureFlagInfo value",
    );
    // Transitions setting feature flags stay explicit.
    f.file(
        "defs.bzl",
        "t = config_common.config_feature_flag_transition('flags')\n",
    );
    f.file("BUILD.bazel", "load(':defs.bzl', 't')\n");
    rejected(
        &f.run("query", &["//:all"]),
        "config_common.config_feature_flag_transition is not implemented in the native profile",
    );
}

#[test]
fn config_feature_flag_needs_the_builtin_allowlist() {
    let f = Fixture::new();
    f.file(
        "BUILD.bazel",
        "config_feature_flag(name = 'ff', allowed_values = ['a', 'b'], default_value = 'a')\n",
    );
    // Loading works without the built-in profile (query, label_kind).
    let output = f.run("query", &["//:ff"]);
    ok(&output);
    rejected(
        &f.run("build", &["//:ff"]),
        "config_feature_flag rule //:ff: its allowlist @@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag needs the built-in @bazel_tools repository",
    );
}

#[test]
fn builtin_test_exec_group_and_config_feature_flags() {
    let f = Fixture::stubbed();
    f.file(
        "defs.bzl",
        &format!(
            r##"{FEATURE_RULES}{SHOW}
def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out)]
my_test = rule(implementation = _t, test = True)
def _notc(ctx):
    return [DefaultInfo()]
notc = rule(implementation = _notc)
def _tc(ctx):
    print("TEST_TOOLCHAIN", ctx.label)
    return [platform_common.ToolchainInfo()]
tc = rule(implementation = _tc)
"##
        ),
    );
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "my_test", "notc", "probe", "show", "tc")
my_test(name = "a_test")
filegroup(name = "notatoolchain")
notc(name = "noinfo")
tc(name = "custom")
toolchain(name = "bad_impl_tc", toolchain = ":noinfo", toolchain_type = "@bazel_tools//tools/test:default_test_toolchain_type")
toolchain(name = "custom_tc", toolchain = ":custom", toolchain_type = "@bazel_tools//tools/test:default_test_toolchain_type")
config_feature_flag(name = "ff", allowed_values = ["a", "b"], default_value = "a")
config_feature_flag(name = "nodef", allowed_values = ["b", "a"])
config_feature_flag(name = "dup", allowed_values = ["a", "b", "a", "b", "c"], default_value = "a")
config_feature_flag(name = "both", allowed_values = ["a", "a"], default_value = "q")
config_feature_flag(name = "empty", allowed_values = [])
config_setting(name = "ff_a", flag_values = {":ff": "a"})
config_setting(name = "ff_b", flag_values = {":ff": "b"})
config_setting(name = "ff_bad", flag_values = {":ff": "zz"})
config_setting(name = "nodef_a", flag_values = {":nodef": "a"})
show(name = "sel_ff", s = select({":ff_a": "A", ":ff_b": "B"}))
show(name = "sel_bad", s = select({":ff_bad": "x", "//conditions:default": "D"}))
show(name = "sel_nodef", s = select({":nodef_a": "x", "//conditions:default": "D"}))
probe(name = "p", deps = [":ff", ":nodef"])
"#,
    );
    // The test exec group resolves the genuine default test toolchain.
    ok(&f.run_builtin("build", &["//:a_test"]));
    let text = ok(&f.run_builtin("build", &["--extra_toolchains=//:custom_tc", "//:a_test"]));
    assert!(text.contains("TEST_TOOLCHAIN //:custom"), "{text}");
    rejected(
        &f.run_builtin(
            "build",
            &["--extra_toolchains=//:notatoolchain", "//:a_test"],
        ),
        "While resolving toolchains for target //:a_test (",
    );
    rejected(
        &f.run_builtin(
            "build",
            &["--extra_toolchains=//:notatoolchain", "//:a_test"],
        ),
        "invalid registered toolchain '//:notatoolchain': target does not provide the DeclaredToolchainInfo provider",
    );
    // A non-test rule without toolchains resolves nothing.
    ok(&f.run_builtin(
        "build",
        &["--extra_toolchains=//:notatoolchain", "//:noinfo"],
    ));
    rejected(
        &f.run_builtin("build", &["--extra_toolchains=//:bad_impl_tc", "//:a_test"]),
        "toolchain type @@bazel_tools//tools/test:default_test_toolchain_type resolved to target //:noinfo, but that target does not provide ToolchainInfo",
    );
    let text = ok(&f.run_builtin("build", &["--use_target_platform_for_tests", "//:a_test"]));
    assert!(text.contains(WARNING), "{text}");
    ok(&f.run_builtin(
        "build",
        &[
            "--@bazel_tools//tools/test:incompatible_use_default_test_toolchain=false",
            "//:a_test",
        ],
    ));
    // config_feature_flag.
    ok(&f.run_builtin("build", &["//:ff"]));
    let text = ok(&f.run_builtin("build", &["//:sel_ff", "//:p"]));
    assert!(text.contains("SHOW //:sel_ff A"), "{text}");
    assert!(
        text.contains(
            "D //:ff True FeatureFlagInfo struct(error = None, value = \"a\") a None True False"
        ),
        "{text}"
    );
    assert!(
        text.contains("D //:nodef True FeatureFlagInfo struct(error = \"Feature flag //:nodef has no default but no value was explicitly specified.\", value = None) None Feature flag //:nodef has no default but no value was explicitly specified. True False"),
        "{text}"
    );
    rejected(
        &f.run_builtin("build", &["//:dup"]),
        "in allowed_values attribute of config_feature_flag rule //:dup: cannot contain duplicates, but contained multiple of [\"a\", \"b\"]",
    );
    let text = rejected(
        &f.run_builtin("build", &["//:both"]),
        "in allowed_values attribute of config_feature_flag rule //:both: cannot contain duplicates, but contained multiple of [\"a\"]",
    );
    assert!(
        text.contains("in default_value attribute of config_feature_flag rule //:both: must be one of [\"a\"], but was \"q\""),
        "{text}"
    );
    rejected(
        &f.run_builtin("build", &["//:empty"]),
        "in allowed_values attribute of config_feature_flag rule //:empty: attribute must be non empty",
    );
    rejected(
        &f.run_builtin("build", &["//:sel_bad"]),
        "in flag_values attribute of config_setting rule //:ff_bad: error while parsing user-defined configuration values: 'zz' is not a valid value for '//:ff'",
    );
    rejected(
        &f.run_builtin("build", &["//:sel_nodef"]),
        "Unresolvable config_settings for configurable attribute \"s\" in //:sel_nodef:\nconfig_setting //:nodef_a is unresolvable because: Feature flag //:nodef has no default but no value was explicitly specified.",
    );
    rejected(
        &f.run_builtin("build", &["--//:ff=b", "//:ff"]),
        "//:ff :: Unrecognized option: //:ff",
    );
}

/// Genuine rules_python 1.7.0 (a module of the built-in closure): its
/// version config_settings key on `_python_version_major_minor_flag`, a
/// non-flag build setting returning only FeatureFlagInfo, and on
/// `python_version`, whose FeatureFlagInfo carries the configured value.
#[test]
fn genuine_rules_python_version_config_settings() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'rules_python', version = '1.7.0')\n",
    );
    f.file("defs.bzl", SHOW);
    f.file(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
show(name = "py", s = select({
    "@rules_python//python/config_settings:is_python_3.12": "312",
    "@rules_python//python/config_settings:is_python_3.13": "313",
    "//conditions:default": "other",
}))
show(name = "ft", s = select({"@rules_python//python/config_settings:is_py_freethreaded": "ft", "//conditions:default": "other"}))
"#,
    );
    let text = ok(&f.run_builtin(
        "build",
        &[
            "--@rules_python//python/config_settings:python_version=3.12",
            "//:py",
            "//:ft",
        ],
    ));
    assert!(
        text.contains("SHOW //:py 312") && text.contains("SHOW //:ft other"),
        "{text}"
    );
    let text = ok(&f.run_builtin(
        "build",
        &[
            "--@rules_python//python/config_settings:python_version=3.13.1",
            "//:py",
        ],
    ));
    assert!(text.contains("SHOW //:py 313"), "{text}");
}
