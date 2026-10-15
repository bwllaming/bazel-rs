//! CLI coverage for N16b (docs/CONFIGURATION.md): Starlark build settings on
//! the command line, `config_setting(flag_values = ...)`, `alias` forwarding
//! and `label_flag`/`label_setting` analysis, with the genuine skylib 1.8.2
//! settings of tests/fixtures/build_settings. Expected texts follow the
//! pinned Bazel 9.3 sources cited in src/starlark_options.rs,
//! src/config_setting.rs and src/engine.rs.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let f = Self(
            tempfile::Builder::new()
                .prefix("n16b-")
                .tempdir_in(base)
                .unwrap(),
        );
        let skylib =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_settings/skylib");
        assert!(skylib.join("rules/common_settings.bzl").is_file());
        f.file(
            "MODULE.bazel",
            &format!(
                "module(name='main')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nlocal_path_override(module_name='bazel_skylib',path='{}')\n",
                skylib.display()
            ),
        );
        f.file("defs.bzl", DEFS);
        for (path, content) in files {
            f.file(path, content);
        }
        f
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, path: &str, content: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args([
                "--kill-after=5s",
                "60s",
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
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "timed out: {args:?}");
        output
    }

    /// `(label, contents)` of every built output.
    fn outputs(&self, args: &[&str]) -> Vec<(String, String)> {
        let output = self.run("build", args);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let (label, path) = line.split_once('\t').unwrap();
                (label.to_owned(), fs::read_to_string(path).unwrap())
            })
            .collect()
    }

    /// Contents of the single output of a build.
    fn built(&self, args: &[&str]) -> String {
        let outputs = self.outputs(args);
        assert_eq!(outputs.len(), 1, "{args:?}: {outputs:?}");
        outputs.into_iter().next().unwrap().1
    }

    /// The diagnostic of a failed build (exit 1, no output).
    fn rejected(&self, args: &[&str]) -> String {
        let output = self.run("build", args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output: {args:?}");
        let diagnostic = errors(&output);
        assert!(!diagnostic.contains("panicked"), "{diagnostic}");
        assert!(!diagnostic.contains("actions executed"), "{diagnostic}");
        diagnostic
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const DEFS: &str = r#"
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = ["mode=" + ctx.attr.mode]
    if ctx.attr.dep:
        lines.append("dep=" + str(ctx.attr.dep.label))
        lines.append("files=" + ",".join([f.short_path for f in ctx.files.dep]))
        if BuildSettingInfo in ctx.attr.dep:
            lines.append("value=" + repr(ctx.attr.dep[BuildSettingInfo].value))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {
    "mode": attr.string(),
    "dep": attr.label(allow_files = True),
})

def _raw_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr(ctx.build_setting_value) + "\n")
    return [DefaultInfo(files = depset([out]))]

raw_int = rule(implementation = _raw_impl, build_setting = config.int(flag = True))
raw_list = rule(implementation = _raw_impl, build_setting = config.string_list(flag = True, repeatable = True))
"#;

const FLAGS: &str = r#"
load("@bazel_skylib//rules:common_settings.bzl", "bool_flag", "bool_setting", "string_flag", "string_list_flag")
load(":defs.bzl", "raw_int", "raw_list", "show")
bool_flag(name = "b", build_setting_default = False)
bool_setting(name = "internal", build_setting_default = False)
string_flag(name = "s", build_setting_default = "red")
string_list_flag(name = "sl", build_setting_default = ["x", "y"])
raw_int(name = "i", build_setting_default = 3)
raw_list(name = "r", build_setting_default = [])
alias(name = "b_alias", actual = ":b", visibility = ["//visibility:public"])
alias(name = "b_alias2", actual = ":b_alias")
config_setting(name = "b_on", flag_values = {":b": "True"})
config_setting(name = "blue", flag_values = {":s": "blue"})
config_setting(name = "has_z", flag_values = {":sl": "z"})
config_setting(name = "alias_on", flag_values = {":b_alias": "1"})
config_setting(name = "blue_and_on", flag_values = {":s": "blue", ":b": "yes"})
config_setting(name = "nine", flag_values = {":i": "0x9"})
show(name = "m", mode = select({":b_on": "b_on", ":has_z": "z", ":blue": "blue", "//conditions:default": "default"}))
show(name = "specialized", mode = select({":blue": "blue", ":blue_and_on": "blue_and_on", "//conditions:default": "default"}))
show(name = "via_alias", mode = select({":alias_on": "alias_on", "//conditions:default": "default"}))
show(name = "int_key", mode = select({":nine": "nine", "//conditions:default": "default"}))
show(name = "strict", mode = select({":blue": "blue"}))
show(name = "use_s", dep = ":s")
"#;

#[test]
fn command_line_values_select_flag_values_and_reach_actions() {
    let f = Fixture::new(&[("BUILD.bazel", FLAGS)]);
    for (args, mode) in [
        (&[][..], "default"),
        (&["--//:b"][..], "b_on"),
        (&["--//:b=true", "--no//:b"][..], "default"),
        (&["--//:s=blue"][..], "blue"),
        (&["--//:sl=z"][..], "z"),
        // A string_list value matches if it contains the expected entry.
        (&["--//:sl=a,z,b"][..], "z"),
        (&["--//:sl=a,b"][..], "default"),
        // Equal values: the alias resolves to the same setting.
        (&["--//:b_alias2=1"][..], "b_on"),
    ] {
        let mut command = args.to_vec();
        command.push("//:m");
        assert_eq!(f.built(&command), format!("mode={mode}\n"), "{args:?}");
    }
    // Specialization over flag values (ConfigMatchingProvider.refines).
    assert_eq!(
        f.built(&["--//:s=blue", "--//:b", "//:specialized"]),
        "mode=blue_and_on\n"
    );
    assert_eq!(f.built(&["--//:s=blue", "//:specialized"]), "mode=blue\n");
    // An alias key is resolved to its setting.
    assert_eq!(f.built(&["--//:b", "//:via_alias"]), "mode=alias_on\n");
    // The expected value converts through the setting's type: 0x9 is 9.
    assert_eq!(f.built(&["--//:i=9", "//:int_key"]), "mode=nine\n");
    assert_eq!(f.built(&["//:int_key"]), "mode=default\n");
    // ctx.build_setting_value receives the configured value.
    assert_eq!(f.built(&["//:i"]), "3\n");
    assert_eq!(f.built(&["--//:i=-0b11", "//:i"]), "-3\n");
    assert_eq!(f.built(&["--//:i=4000000000", "//:i"]), "4000000000\n");
    assert_eq!(
        f.built(&["--//:r=one", "--//:r=two", "//:r"]),
        "[\"one\", \"two\"]\n"
    );
    assert_eq!(
        f.built(&["--//:s=green", "//:use_s"]),
        "mode=\ndep=@@//:s\nfiles=\nvalue=\"green\"\n"
    );
    assert_eq!(
        f.built(&["//:use_s"]),
        "mode=\ndep=@@//:s\nfiles=\nvalue=\"red\"\n"
    );
    // query accepts and ignores Starlark options (it does not analyze:
    // runtime/BlazeOptionHandler.java:417-423), even unknown ones.
    let query = f.run(
        "query",
        &["--//:s=blue", "--//nope:x=1", "--no//:b", "//:m"],
    );
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(String::from_utf8_lossy(&query.stdout), "//:m\n");
}

#[test]
fn default_values_keep_the_configuration_identity() {
    let f = Fixture::new(&[("BUILD.bazel", FLAGS)]);
    // The no-match error prints the configuration identifier.
    let identity = |args: &[&str]| {
        let mut command = args.to_vec();
        command.push("//:strict");
        let diagnostic = f.rejected(&command);
        let start = diagnostic
            .find("configuration identifier ")
            .expect("identifier")
            + "configuration identifier ".len();
        diagnostic[start..start + 7].to_owned()
    };
    let unset = identity(&[]);
    for args in [
        &["--//:s=red"][..],
        &["--no//:b"][..],
        &["--//:sl=x,y"][..],
        &["--//:i=3"][..],
        &["--//:s=green", "--//:s=red"][..],
    ] {
        assert_eq!(identity(args), unset, "{args:?}");
    }
    let green = identity(&["--//:s=green"]);
    assert_ne!(green, unset);
    assert_eq!(
        identity(&["--//:b", "--//:s=green"]),
        identity(&["--//:s=green", "--//:b"])
    );
    assert_ne!(identity(&["--//:b"]), unset);
}

#[test]
fn starlark_option_errors_follow_starlark_options_parser() {
    let f = Fixture::new(&[
        ("BUILD.bazel", FLAGS),
        (
            "x/BUILD.bazel",
            "filegroup(name = 'fg')\nalias(name = 'to_fg', actual = ':fg')\nalias(name = 'sel', actual = select({'//conditions:default': '//:s', '//:b_on': '//:b'}))\nalias(name = 'c1', actual = ':c2')\nalias(name = 'c2', actual = ':c1')\nexports_files(['data.txt'])\n",
        ),
    ]);
    f.file("x/data.txt", "data\n");
    let ws = f.path().display().to_string();
    for (args, message) in [
        (
            &["--no//:s"][..],
            "//:s :: Illegal use of 'no' prefix on non-boolean option: //:s".to_owned(),
        ),
        (
            &["--//:s"][..],
            "--//:s :: Expected value after --//:s. Did you mean --//:s=//:m?".to_owned(),
        ),
        (
            &["--//:i=x"][..],
            "While parsing option //:i=x: 'x' is not a int".to_owned(),
        ),
        (
            &["--//:b=maybe"][..],
            "While parsing option //:b=maybe: 'maybe' is not a boolean".to_owned(),
        ),
        (
            &["--//:internal=1"][..],
            "Unrecognized option: //:internal=1".to_owned(),
        ),
        (
            &["--//x:fg=1"][..],
            "//x:fg :: Unrecognized option: //x:fg".to_owned(),
        ),
        (
            &["--//x:to_fg=1"][..],
            "//x:to_fg :: Unrecognized option: //x:to_fg -> //x:fg".to_owned(),
        ),
        (
            &["--//x:data.txt=1"][..],
            "//x:data.txt :: Unrecognized option: //x:data.txt".to_owned(),
        ),
        (
            &["--//x:sel=1"][..],
            "//x:sel :: Failed to load build setting '//x:sel' as it resolves to an alias with an actual value that uses select(): //x:sel. This is not supported as build settings are needed to determine the configuration the select is evaluated in.".to_owned(),
        ),
        (
            &["--//x:c1=1"][..],
            "//x:c1 :: Failed to load build setting '//x:c1' due to a cycle in alias chain: //x:c1 -> //x:c2 -> //x:c1".to_owned(),
        ),
        (
            &["--//:nope=1"][..],
            format!(
                "//:nope :: Error loading option //:nope: no such target '//:nope': target 'nope' not declared in package '' defined by {ws}/BUILD.bazel"
            ),
        ),
        (
            &["-//:s=1"][..],
            "Invalid options syntax: -//:s=1\nNote: Negative target patterns can only appear after the end of options marker ('--'). Flags corresponding to Starlark-defined build settings always start with '--', not '-'.".to_owned(),
        ),
    ] {
        let mut command = args.to_vec();
        command.push("//:m");
        assert_eq!(
            f.rejected(&command),
            format!("bazel-rs: {message}\n"),
            "{args:?}"
        );
    }
}

#[test]
fn flag_values_errors_are_config_setting_attribute_errors() {
    let f = Fixture::new(&[
        ("BUILD.bazel", FLAGS),
        (
            "c/BUILD.bazel",
            r#"load("@bazel_skylib//rules:common_settings.bzl", "bool_flag", "int_flag", "string_list_flag")
load("//:defs.bzl", "show")
bool_flag(name = "b", build_setting_default = False)
int_flag(name = "i", build_setting_default = 1)
string_list_flag(name = "sl", build_setting_default = ["x"])
alias(name = "b2", actual = ":b")
filegroup(name = "fg")
config_setting(name = "missing", flag_values = {":x": "1"})
config_setting(name = "missing_dot", flag_values = {":x.txt": "1"})
config_setting(name = "not_setting", flag_values = {":fg": "1"})
config_setting(name = "conversion", flag_values = {":i": "abc", ":b": "maybe"})
config_setting(name = "twice", flag_values = {":b": "1", ":b2": "0"})
config_setting(name = "multiple", flag_values = {":sl": "a,b"})
config_setting(name = "private", flag_values = {"//:b": "1"})
config_setting(name = "both", values = {"no_such_option": "1"}, flag_values = {":fg": "1"})
[show(name = "use_" + n, mode = select({":" + n: "x", "//conditions:default": "y"})) for n in ["missing", "missing_dot", "not_setting", "conversion", "twice", "multiple", "private", "both"]]
"#,
        ),
    ]);
    let build = format!("{}/c/BUILD.bazel", f.path().display());
    let attribute = |line: u32, name: &str, message: &str| {
        format!(
            "{build}:{line}:15: in flag_values attribute of config_setting rule //c:{name}: {message}"
        )
    };
    for (target, expected) in [
        (
            "missing",
            attribute(8, "missing", "rule '//c:x' does not exist"),
        ),
        (
            "missing_dot",
            attribute(9, "missing_dot", "target '//c:x.txt' does not exist"),
        ),
        (
            "not_setting",
            attribute(
                10,
                "not_setting",
                "error while parsing user-defined configuration values: flag_values keys must be build settings or feature flags and //c:fg is not",
            ),
        ),
        (
            "conversion",
            format!(
                "{}\n{}",
                attribute(
                    11,
                    "conversion",
                    "error while parsing user-defined configuration values: 'abc' cannot be converted to //c:i type int"
                ),
                attribute(
                    11,
                    "conversion",
                    "error while parsing user-defined configuration values: 'maybe' cannot be converted to //c:b type boolean"
                )
            ),
        ),
        (
            "twice",
            attribute(
                12,
                "twice",
                "flag '//c:b' referenced multiple times as ['//c:b', '//c:b2']",
            ),
        ),
        (
            "multiple",
            attribute(
                13,
                "multiple",
                "\"a,b\" not a valid value for flag //c:sl. Only single, exact values are allowed. If you want to match multiple values, consider Skylib's selects.config_setting_group",
            ),
        ),
        (
            "private",
            format!(
                "{build}:14:15: in config_setting rule //c:private: Visibility error:\ntarget '//:b' is not visible from\ntarget '//c:private'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility"
            ),
        ),
        (
            "both",
            format!(
                "{build}:15:15: in values attribute of config_setting rule //c:both: error while parsing configuration settings: unknown option: 'no_such_option'\n{}",
                attribute(
                    15,
                    "both",
                    "error while parsing user-defined configuration values: flag_values keys must be build settings or feature flags and //c:fg is not"
                )
            ),
        ),
    ] {
        // The consumer then reports `errors encountered resolving select()
        // keys` at its location (ConfigConditionsProducer.java:200-211).
        assert_eq!(
            f.rejected(&[&format!("//c:use_{target}")]),
            format!(
                "bazel-rs: {expected}\n{build}:16:6: errors encountered resolving select() keys for //c:use_{target}\n"
            ),
            "{target}"
        );
    }
}

#[test]
fn aliases_forward_files_providers_and_conditions() {
    let f = Fixture::new(&[
        ("BUILD.bazel", FLAGS),
        (
            "a/BUILD.bazel",
            r#"load("//:defs.bzl", "show")
filegroup(name = "fg", srcs = ["data.txt"])
filegroup(name = "other", srcs = ["other.txt"])
genrule(name = "gen", outs = ["gen.txt"], cmd = "echo generated > $@")
alias(name = "to_file", actual = "data.txt")
alias(name = "to_fg", actual = ":fg")
alias(name = "chain", actual = ":to_fg")
alias(name = "to_gen", actual = ":gen")
alias(name = "sel", actual = select({"//:b_on": ":other", "//conditions:default": ":fg"}))
alias(name = "cond", actual = "//:b_on")
alias(name = "constrained", actual = ":fg", target_compatible_with = ["@platforms//os:linux"])
show(name = "use_file", dep = ":to_file")
show(name = "use_chain", dep = ":chain")
show(name = "use_gen", dep = ":to_gen")
show(name = "use_sel", dep = ":sel")
show(name = "use_cond", mode = select({":cond": "on", "//conditions:default": "off"}))
show(name = "use_flag", dep = "//:b_alias")
"#,
        ),
    ]);
    f.file("a/data.txt", "data\n");
    f.file("a/other.txt", "other\n");
    // Dependents see the actual's label and files
    // (AliasConfiguredTarget.getLabel, :178-185).
    assert_eq!(
        f.built(&["//a:use_file"]),
        "mode=\ndep=@@//a:data.txt\nfiles=a/data.txt\n"
    );
    assert_eq!(
        f.built(&["//a:use_chain"]),
        "mode=\ndep=@@//a:fg\nfiles=a/data.txt\n"
    );
    assert_eq!(
        f.built(&["//a:use_gen"]),
        "mode=\ndep=@@//a:gen\nfiles=a/gen.txt\n"
    );
    // select() in `actual` resolves in the alias's configuration.
    assert_eq!(
        f.built(&["//a:use_sel"]),
        "mode=\ndep=@@//a:fg\nfiles=a/data.txt\n"
    );
    assert_eq!(
        f.built(&["--//:b", "//a:use_sel"]),
        "mode=\ndep=@@//a:other\nfiles=a/other.txt\n"
    );
    // An alias to a config_setting is a select() condition.
    assert_eq!(f.built(&["//a:use_cond"]), "mode=off\n");
    assert_eq!(f.built(&["--//:b", "//a:use_cond"]), "mode=on\n");
    // Providers are forwarded (BuildSettingInfo of the skylib flag).
    assert_eq!(
        f.built(&["--//:b", "//a:use_flag"]),
        "mode=\ndep=@@//:b\nfiles=\nvalue=True\n"
    );
    // Requested aliases build their actual's files.
    assert_eq!(f.built(&["//a:chain"]), "data\n");
    assert_eq!(f.built(&["//a:to_gen"]), "generated\n");
    assert_eq!(f.built(&["--//:b", "//a:sel"]), "other\n");
    // target_compatible_with needs platform constraints (N16c).
    assert!(
        f.rejected(&["//a:constrained"])
            .contains("//a:constrained: alias accepted at load time but not analyzed by the native profile: attribute 'target_compatible_with'")
    );
}

#[test]
fn alias_visibility_and_testonly_follow_the_prerequisite_validator() {
    let f = Fixture::new(&[
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'use_private_alias', dep = '//p:private_alias')\nshow(name = 'use_leaky', dep = '//p:leaky')\nshow(name = 'use_public', dep = '//p:public_alias')\nshow(name = 'use_testonly', dep = '//p:to_testonly')\nshow(name = 'use_chain', dep = '//p:chain_to_private')\n",
        ),
        (
            "p/BUILD.bazel",
            "load('//:defs.bzl', 'show')\nfilegroup(name = 'private')\nfilegroup(name = 'helper', testonly = True, visibility = ['//visibility:public'])\nalias(name = 'private_alias', actual = ':private')\nalias(name = 'public_alias', actual = ':private', visibility = ['//visibility:public'])\nalias(name = 'leaky', actual = '//q:hidden', visibility = ['//visibility:public'])\nalias(name = 'to_testonly', actual = ':helper', visibility = ['//visibility:public'])\nalias(name = 'chain_to_private', actual = ':private_alias')\n",
        ),
        ("q/BUILD.bazel", "filegroup(name = 'hidden')\n"),
    ]);
    let root = f.path().display().to_string();
    // The consumer checks the alias's own visibility; the message describes
    // the alias chain (AliasProvider.describeTargetWithAliases).
    assert_eq!(
        f.rejected(&["//:use_private_alias"]),
        format!(
            "bazel-rs: {root}/BUILD.bazel:2:5: in show rule //:use_private_alias: Visibility error:\nalias '//p:private_alias' referring to target '//p:private' is not visible from\ntarget '//:use_private_alias'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility\n"
        )
    );
    assert_eq!(
        f.rejected(&["//:use_chain"]),
        format!(
            "bazel-rs: {root}/BUILD.bazel:6:5: in show rule //:use_chain: Visibility error:\nalias '//p:chain_to_private' referring to target '//p:private' through '//p:private_alias' is not visible from\ntarget '//:use_chain'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility\n"
        )
    );
    // A public alias re-exports a private target of its own package.
    assert_eq!(
        f.built(&["//:use_public"]),
        "mode=\ndep=@@//p:private\nfiles=\n"
    );
    // The alias itself must see its actual.
    assert_eq!(
        f.rejected(&["//:use_leaky"]),
        format!(
            "bazel-rs: {root}/p/BUILD.bazel:6:6: in alias rule //p:leaky: Visibility error:\ntarget '//q:hidden' is not visible from\ntarget '//p:leaky'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility\n"
        )
    );
    // testonly is checked between the consumer and the alias's actual
    // (CommonPrerequisiteValidator.java:730-760); the alias itself is exempt.
    assert_eq!(
        f.rejected(&["//:use_testonly"]),
        format!(
            "bazel-rs: {root}/BUILD.bazel:5:5: in show rule //:use_testonly: non-test target '//:use_testonly' depends on testonly alias '//p:to_testonly' referring to target '//p:helper' and doesn't have testonly attribute set\n"
        )
    );
    let alias = f.run("build", &["//p:to_testonly"]);
    assert!(alias.status.success(), "{}", errors(&alias));
}

#[test]
fn label_flags_and_label_settings_forward_their_value() {
    let f = Fixture::new(&[
        (
            "BUILD.bazel",
            r#"load(":defs.bzl", "show")
filegroup(name = "fg", srcs = ["data.txt"])
filegroup(name = "other", srcs = ["other.txt"])
label_flag(name = "lf", build_setting_default = ":fg", visibility = ["//visibility:public"])
label_setting(name = "ls", build_setting_default = ":fg")
label_flag(name = "remote", build_setting_default = "//p:private")
alias(name = "lf_alias", actual = ":lf")
config_setting(name = "lf_other", flag_values = {":lf": ":other"})
config_setting(name = "lf_other_via_alias", flag_values = {":lf_alias": "//:other"})
config_setting(name = "lf_both", flag_values = {":lf": ":other", ":ls": ":fg"})
show(name = "use_lf", dep = ":lf")
show(name = "use_ls", dep = ":ls")
show(name = "use_remote", dep = ":remote")
show(name = "pick", mode = select({":lf_other": "other", "//conditions:default": "default"}))
show(name = "pick_alias", mode = select({":lf_other_via_alias": "other", "//conditions:default": "default"}))
show(name = "pick_both", mode = select({":lf_both": "both", "//conditions:default": "default"}))
"#,
        ),
        (
            "p/BUILD.bazel",
            "filegroup(name = 'private', srcs = ['p.txt'])\n",
        ),
    ]);
    f.file("data.txt", "data\n");
    f.file("other.txt", "other\n");
    f.file("p/p.txt", "p\n");
    // The default, then a command-line value (LabelBuildSettings.ACTUAL).
    assert_eq!(
        f.built(&["//:use_lf"]),
        "mode=\ndep=@@//:fg\nfiles=data.txt\n"
    );
    assert_eq!(
        f.built(&["--//:lf=//:other", "//:use_lf"]),
        "mode=\ndep=@@//:other\nfiles=other.txt\n"
    );
    assert_eq!(f.built(&["--//:lf=:other", "//:lf"]), "other\n");
    assert_eq!(
        f.built(&["//:use_ls"]),
        "mode=\ndep=@@//:fg\nfiles=data.txt\n"
    );
    // The `:alias` attribute is late-bound: its visibility is not checked
    // (CommonPrerequisiteValidator.java:159-163).
    assert_eq!(
        f.built(&["//:use_remote"]),
        "mode=\ndep=@@//p:private\nfiles=p/p.txt\n"
    );
    // flag_values on label settings compare canonical labels.
    assert_eq!(f.built(&["//:pick"]), "mode=default\n");
    assert_eq!(f.built(&["--//:lf=//:other", "//:pick"]), "mode=other\n");
    assert_eq!(
        f.built(&["--//:lf_alias=:other", "//:pick_alias"]),
        "mode=other\n"
    );
    // Keys are grouped by the label their alias chain ends in
    // (ConfigSetting.java:659-666, 771-787): by default both settings
    // forward to //:fg and collide.
    assert_eq!(
        f.rejected(&["//:pick_both"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:10:15: in flag_values attribute of config_setting rule //:lf_both: flag '//:fg' referenced multiple times as ['//:lf', '//:ls']\n{ws}/BUILD.bazel:16:5: errors encountered resolving select() keys for //:pick_both\n",
            ws = f.path().display()
        )
    );
    assert_eq!(
        f.built(&["--//:lf=//:other", "//:pick_both"]),
        "mode=both\n"
    );
    // label_setting is not a flag (StarlarkOptionsParser.java:185-189).
    assert_eq!(
        f.rejected(&["--//:ls=//:other", "//:use_ls"]),
        "bazel-rs: Unrecognized option: //:ls=//:other\n"
    );
    assert_eq!(
        f.rejected(&["--//:lf=a:b:c", "//:use_lf"]),
        "bazel-rs: While parsing option //:lf=a:b:c: 'a:b:c' is not a label\n"
    );
}

#[test]
fn external_label_flag_with_default_filegroup_builds() {
    // The application probe shape: rules_go's //go/config:pgoprofile label
    // flag with a default filegroup, built by canonical repository name.
    let f = Fixture::new(&[(
        "BUILD.bazel",
        "load(':defs.bzl', 'show')\nfilegroup(name = 'profile', srcs = ['default.pgo'])\nshow(name = 'use', dep = '@rules_go//go/config:pgoprofile')\n",
    )]);
    f.file("default.pgo", "pgo\n");
    let rules_go = f.path().join("rules_go");
    f.file(
        "rules_go/MODULE.bazel",
        "module(name = 'rules_go', version = '0.0.1')\n",
    );
    f.file(
        "rules_go/go/config/BUILD.bazel",
        "label_flag(name = 'pgoprofile', build_setting_default = ':empty', visibility = ['//visibility:public'])\nfilegroup(name = 'empty', visibility = ['//visibility:public'])\n",
    );
    let module = fs::read_to_string(f.path().join("MODULE.bazel")).unwrap();
    f.file(
        "MODULE.bazel",
        &format!(
            "{module}bazel_dep(name = 'rules_go', version = '0.0.1')\nlocal_path_override(module_name = 'rules_go', path = '{}')\n",
            rules_go.display()
        ),
    );
    let output = f.run("build", &["@@rules_go+//go/config:pgoprofile"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(output.stdout.is_empty());
    assert_eq!(
        f.built(&["//:use"]),
        "mode=\ndep=@@rules_go+//go/config:empty\nfiles=\n"
    );
    assert_eq!(
        f.built(&["--@rules_go//go/config:pgoprofile=//:profile", "//:use"]),
        "mode=\ndep=@@//:profile\nfiles=default.pgo\n"
    );
    assert_eq!(
        f.built(&[
            "--@@rules_go+//go/config:pgoprofile=//:profile",
            "@rules_go//go/config:pgoprofile"
        ]),
        "pgo\n"
    );
}

#[test]
fn conditions_through_aliases_validate_the_config_setting() {
    // RuleContext.validateExtraPrerequisites validates the END of the alias
    // chain against the consumer under DEFAULT_PUBLIC (RuleContext.java:2125-2152,
    // AliasConfiguredTarget.getActual :229-233): the alias's own visibility
    // is skipped and the message names the config_setting.
    let f = Fixture::new(&[
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'restricted', mode = select({'//a:public_alias': 'on', '//conditions:default': 'off'}))\nshow(name = 'private_alias', mode = select({'//a:private_alias': 'on', '//conditions:default': 'off'}))\nshow(name = 'fg_key', mode = select({'//a:fg_alias': 'on', '//conditions:default': 'off'}))\n",
        ),
        (
            "a/BUILD.bazel",
            "config_setting(name = 'restricted', values = {'compilation_mode': 'opt'}, visibility = ['//a:__pkg__'])\nalias(name = 'public_alias', actual = ':restricted', visibility = ['//visibility:public'])\nconfig_setting(name = 'public', values = {'compilation_mode': 'opt'})\nalias(name = 'private_alias', actual = ':public')\nfilegroup(name = 'fg')\nalias(name = 'fg_alias', actual = ':fg', visibility = ['//visibility:public'])\n",
        ),
    ]);
    let ws = f.path().display().to_string();
    assert_eq!(
        f.rejected(&["//:restricted"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:2:5: in show rule //:restricted: Visibility error:\ntarget '//a:restricted' is not visible from\ntarget '//:restricted'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility\n"
        )
    );
    assert_eq!(f.built(&["//:private_alias"]), "mode=off\n");
    assert_eq!(f.built(&["-c", "opt", "//:private_alias"]), "mode=on\n");
    // The invalid-condition message names the alias's actual
    // (ConfigConditionsProducer.java:156-165, getTargetLabel).
    assert!(f.rejected(&["//:fg_key"]).starts_with(&format!(
        "bazel-rs: {ws}/BUILD.bazel:4:5: //a:fg is not a valid select() condition for //:fg_key.\n"
    )));
}

#[test]
fn file_prerequisites_missing_edges_and_cycles_follow_bazel() {
    let f = Fixture::new(&[(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
package(default_visibility = ["//visibility:public"])
filegroup(name = "fg", srcs = ["data.txt"])
genrule(name = "gen", outs = ["gen.txt"], cmd = "echo generated > $@")
alias(name = "to_file", actual = "data.txt")
alias(name = "to_gen", actual = ":gen.txt")
alias(name = "to_missing_pkg", actual = "//nope:x")
alias(name = "self", actual = ":self")
alias(name = "cycle_a", actual = ":cycle_b")
alias(name = "cycle_b", actual = ":cycle_a")
label_flag(name = "lf", build_setting_default = ":fg")
alias(name = "lf_alias", actual = ":lf")
show(name = "use_missing_pkg", dep = ":to_missing_pkg")
show(name = "use_missing_target", dep = "//p:nothere")
show(name = "use_self", dep = ":self")
show(name = "use_lf", dep = ":lf")
"#,
    )]);
    f.file(
        "strict.bzl",
        "def _impl(ctx):\n    return []\nstrict = rule(implementation = _impl, attrs = {'dep': attr.label(), 'deps': attr.label_list()})\n",
    );
    f.file(
        "s/BUILD.bazel",
        "load('//:strict.bzl', 'strict')\nstrict(name = 'file', dep = '//:to_file')\nstrict(name = 'gen', dep = '//:gen.txt', deps = ['//:to_gen', '//:data.txt'])\nstrict(name = 'missing', dep = ':nothere')\n",
    );
    f.file("p/BUILD.bazel", "filegroup(name = 'other')\n");
    f.file("data.txt", "data\n");
    let ws = f.path().display().to_string();
    // Starlark label attributes without allow_files allow no files
    // (RuleContext.java:1819-1880, StarlarkAttrModule.java:431-445).
    assert_eq!(
        f.rejected(&["//s:file"]),
        format!(
            "bazel-rs: {ws}/s/BUILD.bazel:2:7: in dep attribute of strict rule //s:file: alias '//:to_file' referring to source file '//:data.txt' is misplaced here (expected no files)\n"
        )
    );
    assert_eq!(
        f.rejected(&["//s:gen"]),
        format!(
            "bazel-rs: {ws}/s/BUILD.bazel:3:7: in dep attribute of strict rule //s:gen: generated file '//:gen.txt' is misplaced here (expected no files)\n{ws}/s/BUILD.bazel:3:7: in deps attribute of strict rule //s:gen: alias '//:to_gen' referring to generated file '//:gen.txt' is misplaced here (expected no files)\n{ws}/s/BUILD.bazel:3:7: in deps attribute of strict rule //s:gen: source file '//:data.txt' is misplaced here (expected no files)\n"
        )
    );
    assert_eq!(
        f.rejected(&["//s:missing"]),
        format!(
            "bazel-rs: {ws}/s/BUILD.bazel:4:7: in dep attribute of strict rule //s:missing: rule '//s:nothere' does not exist\n"
        )
    );
    // Missing dependency edges (TargetUtils.formatMissingEdge, :394-430).
    let package = "no such package 'nope': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - nope";
    assert_eq!(
        f.rejected(&["//:use_missing_pkg"]),
        format!(
            "bazel-rs: {package}\n{ws}/BUILD.bazel:7:6: {package} and referenced by '//:to_missing_pkg'\n"
        )
    );
    // A missing target of another package is first reported at that
    // package's BUILD file (paired capture N16b A10/missing_target_edge).
    let missing = format!(
        "no such target '//p:nothere': target 'nothere' not declared in package 'p' defined by {ws}/p/BUILD.bazel (did you mean other?)"
    );
    assert_eq!(
        f.rejected(&["//:use_missing_target"]),
        format!(
            "bazel-rs: {ws}/p/BUILD.bazel: {missing}\n{ws}/BUILD.bazel:14:5: {missing} and referenced by '//:use_missing_target'\n"
        )
    );
    // The late-bound `:alias` of a label_flag is an implicit dependency.
    assert_eq!(
        f.rejected(&["--//:lf=//:nothere", "//:use_lf"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:11:11: every rule of type label_flag implicitly depends upon the target '//:nothere', but this target could not be found because of: no such target '//:nothere': target 'nothere' not declared in package '' defined by {ws}/BUILD.bazel\n"
        )
    );
    let other = "no such package 'other': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - other";
    assert_eq!(
        f.rejected(&["--//:lf=other", "//:use_lf"]),
        format!(
            "bazel-rs: {other}\n{ws}/BUILD.bazel:11:11: every rule of type label_flag implicitly depends upon the target '//other:other', but this target could not be found because of: {other}\n"
        )
    );
    // Cycles (AbstractLabelCycleReporter.java:81-170); the configuration is
    // the native identifier.
    let cycle = |args: &[&str]| {
        // Every configuration identifier (64 hex digits) becomes ID.
        let mut diagnostic = f.rejected(args);
        while let Some(index) =
            diagnostic
                .match_indices(" (")
                .map(|(index, _)| index)
                .find(|index| {
                    diagnostic[index + 2..].get(..65).is_some_and(|id| {
                        id.ends_with(')') && id[..64].bytes().all(|b| b.is_ascii_hexdigit())
                    })
                })
        {
            diagnostic.replace_range(index + 2..index + 66, "ID");
        }
        diagnostic
    };
    assert_eq!(
        cycle(&["//:self"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:8:6: in alias rule //:self: cycle in dependency graph:\n.-> //:self (ID) [self-edge]\n`--\n"
        )
    );
    assert_eq!(
        cycle(&["//:use_self"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:8:6: in alias rule //:self: cycle in dependency graph:\n    //:use_self (ID)\n    //:use_self (ID)\n.-> //:self (ID) [self-edge]\n`--\n"
        )
    );
    assert_eq!(
        cycle(&["//:cycle_a"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:9:6: in alias rule //:cycle_a: cycle in dependency graph:\n.-> //:cycle_a (ID)\n|   //:cycle_b (ID)\n`-- //:cycle_a (ID)\n"
        )
    );
    assert_eq!(
        cycle(&["--//:lf=//:lf_alias", "//:use_lf"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:11:11: in label_flag rule //:lf: cycle in dependency graph:\n    //:use_lf (ID)\n    //:use_lf (ID)\n.-> //:lf (ID)\n|   //:lf_alias (ID)\n`-- //:lf (ID)\n"
        )
    );
}

#[test]
fn option_texts_and_deprecated_settings_follow_bazel() {
    let f = Fixture::new(&[(
        "BUILD.bazel",
        &format!(
            "{FLAGS}bool_flag(name = \"old\", build_setting_default = False, deprecation = \"old flag\")\n"
        ),
    )]);
    let ws = f.path().display().to_string();
    for (args, message) in [
        // Not Starlark options: the native options parser
        // (OptionsParserImpl.java:806-824, no SpellChecker suggestion).
        (&["--:s=blue"][..], "--:s=blue :: Unrecognized option: --:s=blue".to_owned()),
        (&["--s=blue"][..], "--s=blue :: Unrecognized option: --s=blue".to_owned()),
        (
            &["--@nope//:x=1"][..],
            "@nope//:x :: Error loading option @nope//:x: No repository visible as '@nope' from main repository".to_owned(),
        ),
        // `--no//:b=true` loads the option name `no//:b`.
        (
            &["--no//:b=true"][..],
            "no//:b :: Error loading option no//:b: invalid package name 'no//': package names may not end with '/'".to_owned(),
        ),
        // After `--` every argument is a target pattern; `-` makes it negative.
        (
            &["--", "--//:s=blue"][..],
            "invalid package name '-//': package names may not end with '/'".to_owned(),
        ),
    ] {
        let mut command = args.to_vec();
        command.push("//:m");
        assert_eq!(f.rejected(&command), format!("bazel-rs: {message}\n"), "{args:?}");
    }
    // Loading a deprecated setting through the target pattern phase warns
    // (TargetPatternPhaseFunction.java:255-275), only when it is set.
    let output = f.run("build", &["--//:old", "//:m"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        errors(&output).lines().next(),
        Some(
            format!("WARNING: {ws}/BUILD.bazel:24:10: target '//:old' is deprecated: old flag")
                .as_str()
        )
    );
    let output = f.run("build", &["//:m"]);
    assert!(
        !errors(&output).contains("deprecated"),
        "{}",
        errors(&output)
    );
}
