//! Default-only execution of genuine skylib settings and native providers.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/exit_codes.rs");

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("setting-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let skylib =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_settings/skylib");
        assert!(
            skylib.join("rules/common_settings.bzl").is_file(),
            "genuine fixture missing"
        );
        f.file("MODULE.bazel", &format!("module(name='main')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nlocal_path_override(module_name='bazel_skylib',path='{}')\n", skylib.display()));
        f
    }
    fn file(&self, path: &str, content: &str) {
        fs::write(self.0.path().join(path), content).unwrap();
    }
    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules",
            ])
            .arg(format!(
                "--registry=file://{}/tests/fixtures/builtin_modules/registry",
                env!("CARGO_MANIFEST_DIR")
            ))
            .args(args)
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }
    fn build_bytes(&self, target: &str) -> (Vec<u8>, String) {
        let result = self.run("build", &[target]);
        success(&result);
        let text = String::from_utf8(result.stdout).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let (_, output) = text.lines().next().unwrap().split_once('\t').unwrap();
        (
            fs::read(PathBuf::from(output)).unwrap(),
            String::from_utf8(result.stderr).unwrap(),
        )
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn reject(output: &Output, message: &str) {
    // Exit 2 for an option error (crate::exit_code), 1 otherwise.
    assert_eq!(
        output.status.code(),
        Some(rejection_code(&String::from_utf8_lossy(&output.stderr))),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const CONSUMER: &str = r#"
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
def _impl(ctx):
    lines = []
    for target in ctx.attr.settings:
        value = target[BuildSettingInfo].value
        text = "|".join(value) if type(value) == "list" else str(value)
        lines.append(target.label.name + "=" + text)
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "\n".join(lines) + "\n")
    return [DefaultInfo(files=depset([output]))]
consume = rule(implementation=_impl, attrs={"settings":attr.label_list()})
"#;

#[test]
fn all_nine_genuine_rules_produce_real_provider_values_and_cache_outputs() {
    let f = Fixture::new();
    f.file("consume.bzl", CONSUMER);
    f.file("BUILD.bazel", r#"
load("@bazel_skylib//rules:common_settings.bzl", "bool_flag", "bool_setting", "int_flag", "int_setting", "string_flag", "string_setting", "string_list_flag", "string_list_setting", "repeatable_string_flag")
load(":consume.bzl", "consume")
bool_flag(name="bool_flag", build_setting_default=True)
bool_setting(name="bool_setting", build_setting_default=False)
int_flag(name="int_flag", build_setting_default=7)
int_setting(name="int_setting", build_setting_default=-2)
string_flag(name="string_flag", build_setting_default="red")
string_setting(name="string_setting", build_setting_default="plain")
string_list_flag(name="string_list_flag", build_setting_default=["a", "", "b"])
string_list_setting(name="string_list_setting", build_setting_default=("x", "y"))
repeatable_string_flag(name="repeatable_string_flag", build_setting_default=["seed", "tail"])
consume(name="values", settings=[":bool_flag", ":bool_setting", ":int_flag", ":int_setting", ":string_flag", ":string_setting", ":string_list_flag", ":string_list_setting", ":repeatable_string_flag"])
"#);
    let expected = b"bool_flag=True\nbool_setting=False\nint_flag=7\nint_setting=-2\nstring_flag=red\nstring_setting=plain\nstring_list_flag=a||b\nstring_list_setting=x|y\nrepeatable_string_flag=seed|tail\n";
    for (executed, hits) in [(1, 0), (0, 1)] {
        let (bytes, stderr) = f.build_bytes("//:values");
        assert_eq!(bytes, expected);
        assert!(
            stderr.contains(&format!("{executed} actions executed, {hits} cache hits")),
            "{stderr}"
        );
    }
    let settings = f.run("build", &["//:bool_flag", "//:repeatable_string_flag"]);
    success(&settings);
    assert!(settings.stdout.is_empty());
    assert!(String::from_utf8_lossy(&settings.stderr).contains("0 actions executed"));
}

#[test]
fn genuine_template_provider_is_consumed_explicitly_without_make_expansion() {
    let f = Fixture::new();
    f.file("BUILD.bazel", "load('@bazel_skylib//rules:common_settings.bzl','string_flag')\nload(':consume.bzl','consume')\nstring_flag(name='setting',build_setting_default='red',make_variable='FLAG')\nconsume(name='value',setting=':setting')\n");
    f.file(
        "consume.bzl",
        r#"
def _impl(ctx):
    if platform_common.TemplateVariableInfo not in ctx.attr.setting:
        fail("missing native template provider")
    provider = ctx.attr.setting[platform_common.TemplateVariableInfo]
    first = provider.variables
    first["FLAG"] = "mutated"
    output = ctx.actions.declare_file("value.txt")
    ctx.actions.write(output, provider.variables["FLAG"] + "\n")
    return [DefaultInfo(files=depset([output]))]
consume = rule(implementation=_impl, attrs={"setting":attr.label()})
"#,
    );
    assert_eq!(f.build_bytes("//:value").0, b"red\n");
}

#[test]
fn setting_getter_copies_and_call_time_defaults_are_isolated() {
    let f = Fixture::new();
    f.file("BUILD.bazel", "load(':defs.bzl','make','consume')\nmake(name='seed',seed='seed')\nmake(name='other',seed='other')\nconsume(name='values',settings=[':seed',':other'])\n");
    f.file("defs.bzl", r#"
Info = provider(fields=["text"])
def _setting(ctx):
    first = ctx.build_setting_value
    first.append("mutated")
    second = ctx.build_setting_value
    return Info(text=ctx.label.name + "=" + "|".join(first) + ";" + "|".join(second) + ";" + "|".join(ctx.attr.build_setting_default))
setting = rule(implementation=_setting, build_setting=config.string_list())
def make(name, seed):
    values = [seed]
    setting(name=name, build_setting_default=values)
    values.append("late")
def _consume(ctx):
    output = ctx.actions.declare_file("values.txt")
    ctx.actions.write(output, "\n".join([target[Info].text for target in ctx.attr.settings]) + "\n")
    return [DefaultInfo(files=depset([output]))]
consume = rule(implementation=_consume, attrs={"settings":attr.label_list()})
"#);
    assert_eq!(
        f.build_bytes("//:values").0,
        b"seed=seed|mutated;seed;seed\nother=other|mutated;other;other\n"
    );
}

#[test]
fn actual_genuine_validation_is_analysis_time_and_precedes_actions() {
    let f = Fixture::new();
    f.file("consume.bzl", CONSUMER);
    f.file("BUILD.bazel", "load('@bazel_skylib//rules:common_settings.bzl','string_flag')\nload(':consume.bzl','consume')\nstring_flag(name='setting',build_setting_default='green',values=['red','blue'])\nconsume(name='value',settings=[':setting'])\n");
    success(&f.run("query", &["//:setting"]));
    reject(&f.run("build", &["//:value"]), "invalid value 'green'");
    assert!(!f.0.path().join("cache/actions").exists());
    f.file("BUILD.bazel", "load('@bazel_skylib//rules:common_settings.bzl','string_flag')\nstring_flag(name='setting',build_setting_default='red',make_variable='bad-name')\n");
    reject(
        &f.run("build", &["//:setting"]),
        "invalid make variable name",
    );
}

#[test]
fn default_change_cache_round_trip_and_cli_rejections_remain_distinct() {
    let f = Fixture::new();
    f.file("consume.bzl", CONSUMER);
    for (value, executed, hits) in [("red", 1, 0), ("blue", 1, 0), ("blue", 0, 1), ("red", 0, 1)] {
        f.file("BUILD.bazel", &format!("load('@bazel_skylib//rules:common_settings.bzl','string_flag')\nload(':consume.bzl','consume')\nstring_flag(name='setting',build_setting_default='{value}')\nconsume(name='value',settings=[':setting'])\n"));
        let (bytes, stderr) = f.build_bytes("//:value");
        assert_eq!(bytes, format!("setting={value}\n").as_bytes());
        assert!(
            stderr.contains(&format!("{executed} actions executed, {hits} cache hits")),
            "{stderr}"
        );
    }
    // Command-line values reach the genuine implementation (N16b); the
    // non-boolean forms fail like StarlarkOptionsParser.parseArg (:313-339).
    let overridden = f.run("build", &["--//:setting=override", "//:value"]);
    success(&overridden);
    let text = String::from_utf8(overridden.stdout).unwrap();
    let (_, output) = text.trim_end().split_once('\t').unwrap();
    assert_eq!(fs::read(output).unwrap(), b"setting=override\n");
    reject(
        &f.run("build", &["--//:setting", "//:value"]),
        "bazel-rs: --//:setting :: Expected value after --//:setting. Did you mean --//:setting=//:value?\n",
    );
    reject(
        &f.run("build", &["--no//:setting", "//:value"]),
        "bazel-rs: //:setting :: Illegal use of 'no' prefix on non-boolean option: //:setting\n",
    );
}
