//! CLI coverage for N16c (docs/PLATFORMS.md): `constraint_setting`,
//! `constraint_value` and `platform` analysis, target platform resolution
//! (`--platforms`, `--host_platform`, the default host platform of the
//! genuine built-in profile), `config_setting(constraint_values = ...)` and
//! `constraint_value` select() keys. Expected texts follow the pinned Bazel
//! 9.3 sources cited in src/platforms.rs, src/config_setting.rs and
//! src/engine.rs.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(module: &str, files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n16c-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", module);
        ws.file("defs.bzl", DEFS);
        for (path, content) in files {
            ws.file(path, content);
        }
        ws
    }

    /// The constraint packages of CONSTRAINTS plus `files`.
    fn constrained(files: &[(&str, &str)]) -> Self {
        let mut all = CONSTRAINTS.to_vec();
        all.extend_from_slice(files);
        Self::new("module(name = 'main')\n", &all)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, path: &str, content: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    /// Contents of the single output of `build ARGS`.
    fn built(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let lines = stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{stdout}");
        fs::read_to_string(lines[0].split_once('\t').unwrap().1).unwrap()
    }

    /// The diagnostic of a failing `build ARGS`, with the workspace path
    /// replaced by `WS`.
    fn rejected(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output on failure");
        let diagnostic = errors(&output);
        assert!(!diagnostic.contains("panicked"), "{diagnostic}");
        assert!(!diagnostic.contains("actions executed"), "{diagnostic}");
        diagnostic.replace(&self.path().display().to_string(), "WS")
    }
}

fn errors(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Startup notice, not a diagnostic (crate::memory_limit).
    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr + "\n"
    }
}

/// `show` writes its `mode` attribute.
const DEFS: &str = r#"
def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "mode=" + repr(ctx.attr.mode) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {"mode": attr.string(default = "unset")})

def _setting_impl(ctx):
    return []

# A Starlark build setting: toolchain resolution is disabled
# (packages/RuleClass.java:916-918).
setting = rule(
    implementation = _setting_impl,
    build_setting = config.string(flag = True),
    attrs = {"x": attr.string()},
)
"#;

/// Constraint packages shaped like @platforms//os and @platforms//cpu, a
/// setting with a default value, and platforms.
const CONSTRAINTS: &[(&str, &str)] = &[
    (
        "os/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "osx", constraint_setting = ":os")
alias(name = "macos", actual = ":osx")
"#,
    ),
    (
        "cpu/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "cpu")
constraint_value(name = "x86_64", constraint_setting = ":cpu")
constraint_value(name = "arm64", constraint_setting = ":cpu")
"#,
    ),
    (
        "libc/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "libc", default_constraint_value = ":glibc")
constraint_value(name = "glibc", constraint_setting = ":libc")
constraint_value(name = "musl", constraint_setting = ":libc")
"#,
    ),
    (
        "p/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
platform(name = "linux_x86", constraint_values = ["//os:linux", "//cpu:x86_64"])
# Inherits os, overrides cpu (Platform.create, ConstraintCollection.get).
platform(name = "linux_arm", parents = [":linux_x86"], constraint_values = ["//cpu:arm64", "//libc:musl"])
platform(name = "mac", constraint_values = ["//os:macos"])
platform(name = "bare")
alias(name = "mac_alias", actual = ":mac")
"#,
    ),
];

#[test]
fn constraint_values_and_direct_keys_follow_the_target_platform() {
    let ws = Workspace::constrained(&[(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
config_setting(name = "is_linux", constraint_values = ["//os:linux"])
config_setting(name = "is_linux_x86", constraint_values = ["//os:linux", "//cpu:x86_64"])
config_setting(name = "is_glibc", constraint_values = ["//libc:glibc"])
config_setting(name = "is_mac", constraint_values = ["//os:macos"])
show(name = "direct", mode = select({"//os:linux": "linux", "//os:macos": "osx", "//conditions:default": "other"}))
show(name = "special", mode = select({":is_linux": "linux", ":is_linux_x86": "linux_x86", "//conditions:default": "other"}))
show(name = "libc", mode = select({":is_glibc": "glibc", "//libc:musl": "musl", "//conditions:default": "unknown"}))
show(name = "mac", mode = select({":is_mac": "mac", "//conditions:default": "other"}))
# A direct key specifies no constraint values: a config_setting with the same
# constraint refines it (ConfigMatchingProvider.refines).
show(name = "refined", mode = select({"//os:linux": "key", ":is_linux": "setting"}))
"#,
    )]);
    for (platform, direct, special, libc, mac) in [
        ("//p:linux_x86", "linux", "linux_x86", "glibc", "other"),
        // Parent inheritance (os) and override (cpu); the libc default does
        // not apply once a value is set.
        ("//p:linux_arm", "linux", "linux", "musl", "other"),
        // Aliases resolve to the actual constraint_value and platform.
        ("//p:mac", "osx", "other", "glibc", "mac"),
        ("//p:mac_alias", "osx", "other", "glibc", "mac"),
        // A platform without constraints: unset settings never match, a
        // setting's default_constraint_value always applies.
        ("//p:bare", "other", "other", "glibc", "other"),
    ] {
        let flag = format!("--platforms={platform}");
        for (target, expected) in [
            ("//:direct", direct),
            ("//:special", special),
            ("//:libc", libc),
            ("//:mac", mac),
        ] {
            assert_eq!(
                ws.built(&[&flag, target]),
                format!("mode=\"{expected}\"\n"),
                "{platform} {target}"
            );
        }
    }
    assert_eq!(
        ws.built(&["--platforms=//p:linux_x86", "//:refined"]),
        "mode=\"setting\"\n"
    );
    // Without --platforms, --host_platform is the target platform
    // (PlatformMappingValue.java:154-158).
    assert_eq!(
        ws.built(&["--host_platform=//p:mac", "//:direct"]),
        "mode=\"osx\"\n"
    );
    // Only the first --platforms entry counts (PlatformOptions.java:189-200).
    assert_eq!(
        ws.built(&["--platforms=//p:mac,//p:linux_x86", "//:direct"]),
        "mode=\"osx\"\n"
    );
    // A plain build without platform-dependent selects needs no platform.
    ws.file(
        "plain/BUILD.bazel",
        "genrule(name = 'g', outs = ['g.txt'], cmd = 'echo g > $@')\n",
    );
    let output = ws.run(&["build", "//plain:g"]);
    assert!(output.status.success(), "{}", errors(&output));
}

#[test]
fn rules_go_go_config_shaped_constraint_keys() {
    // rules_go BUILD.bazel go_config (amd64 = select over
    // //go/constraints/amd64:v2..v4 constraint_values, default None) and
    // go/constraints/amd64/BUILD.bazel.
    let ws = Workspace::constrained(&[
        (
            "go/constraints/amd64/BUILD.bazel",
            r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "amd64")
constraint_value(name = "v1", constraint_setting = ":amd64")
constraint_value(name = "v2", constraint_setting = ":amd64")
constraint_value(name = "v3", constraint_setting = ":amd64")
constraint_value(name = "v4", constraint_setting = ":amd64")
"#,
        ),
        (
            "BUILD.bazel",
            r#"load(":defs.bzl", "show")
show(
    name = "go_config",
    mode = select({
        "//go/constraints/amd64:v2": "v2",
        "//go/constraints/amd64:v3": "v3",
        "//go/constraints/amd64:v4": "v4",
        # The default is v1.
        "//conditions:default": None,
    }),
)
platform(name = "v3", parents = ["//p:linux_x86"], constraint_values = ["//go/constraints/amd64:v3"])
platform(name = "v1", parents = ["//p:linux_x86"], constraint_values = ["//go/constraints/amd64:v1"])
"#,
        ),
    ]);
    for (platform, expected) in [
        ("//:v3", "\"v3\""),
        ("//:v1", "\"unset\""),
        ("//p:linux_x86", "\"unset\""),
    ] {
        assert_eq!(
            ws.built(&[&format!("--platforms={platform}"), "//:go_config"]),
            format!("mode={expected}\n"),
            "{platform}"
        );
    }
}

#[test]
fn invalid_conditions_and_config_setting_errors() {
    let ws = Workspace::constrained(&[
        (
            "e/BUILD.bazel",
            r#"load("//:defs.bzl", "setting", "show")
setting(name = "s", build_setting_default = "", x = select({"//os:linux": "a", "//conditions:default": "b"}))
filegroup(name = "fg")
config_setting(name = "not_cv", constraint_values = [":fg", "//os:linux"])
config_setting(name = "dup", constraint_values = ["//os:linux", "//os:macos"])
config_setting(name = "file", constraint_values = ["BUILD.bazel"])
show(name = "use_not_cv", mode = select({":not_cv": "a", "//conditions:default": "b"}))
show(name = "use_dup", mode = select({":dup": "a", "//conditions:default": "b"}))
show(name = "use_file", mode = select({":file": "a", "//conditions:default": "b"}))
show(name = "use_setting_key", mode = select({"//cpu:cpu": "a", "//conditions:default": "b"}))
show(name = "use_platform_key", mode = select({"//p:mac": "a", "//conditions:default": "b"}))
alias(name = "selecting_alias", actual = select({"//os:linux": ":fg", "//conditions:default": ":fg"}))
"#,
        ),
        ("BUILD.bazel", ""),
    ]);
    let platform = "--platforms=//p:linux_x86";
    let invalid = |key: &str, target: &str, location: &str| {
        format!(
            "bazel-rs: WS/e/BUILD.bazel:{location}: {key} is not a valid select() condition for {target}.\nTo inspect the select(), run: bazel query --output=build {target}.\nFor more help, see https://bazel.build/reference/be/functions#select.\n\n\n"
        )
    };
    // A rule without toolchain resolution has no target platform: a
    // constraint_value key is invalid (ConfigConditions.java:66-83).
    assert_eq!(
        ws.rejected(&[platform, "//e:s"]),
        invalid("//os:linux", "//e:s", "2:8")
    );
    assert_eq!(
        ws.rejected(&[platform, "//e:use_setting_key"]),
        invalid("//cpu:cpu", "//e:use_setting_key", "10:5")
    );
    assert_eq!(
        ws.rejected(&[platform, "//e:use_platform_key"]),
        invalid("//p:mac", "//e:use_platform_key", "11:5")
    );
    // An alias with select() keys resolves toolchains (Alias.java:78-98).
    let output = ws.run(&["build", platform, "//e:selecting_alias"]);
    assert!(output.status.success(), "{}", errors(&output));
    for (target, location, message) in [
        (
            "use_not_cv",
            "7:5",
            "WS/e/BUILD.bazel:4:15: in constraint_values attribute of config_setting rule //e:not_cv: //e:fg is not a constraint_value",
        ),
        (
            "use_dup",
            "8:5",
            "WS/e/BUILD.bazel:5:15: in config_setting rule //e:dup: Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx]",
        ),
        (
            "use_file",
            "9:5",
            "WS/e/BUILD.bazel:6:15: in constraint_values attribute of config_setting rule //e:file: source file '//e:BUILD.bazel' is misplaced here (expected no files)",
        ),
    ] {
        assert_eq!(
            ws.rejected(&[platform, &format!("//e:{target}")]),
            format!(
                "bazel-rs: {message}\nWS/e/BUILD.bazel:{location}: errors encountered resolving select() keys for //e:{target}\n"
            ),
            "{target}"
        );
    }
}

#[test]
fn platform_and_constraint_rule_errors() {
    let ws = Workspace::constrained(&[(
        "e/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
filegroup(name = "fg")
constraint_value(name = "bad_cv", constraint_setting = ":fg")
constraint_value(name = "alias_cv", constraint_setting = "//os:macos")
platform(name = "two_parents", parents = ["//p:linux_x86", "//p:mac"])
platform(name = "bad_deps", constraint_values = [":fg", "//p:mac"], parents = [":fg"], required_settings = [":fg"])
platform(name = "dup", constraint_values = ["//os:linux", "//cpu:x86_64", "//os:macos", "//cpu:arm64"])
platform(name = "both", remote_execution_properties = "r", exec_properties = {"a": "1"})
platform(name = "flagged", flags = ["--define=a=1"])
platform(name = "selected", check_toolchain_types = select({"//conditions:default": True, ":on": False}))
config_setting(name = "on", values = {"stamp": "1"})
constraint_setting(name = "other_default", default_constraint_value = "//os:linux")
constraint_setting(name = "missing_default", default_constraint_value = ":nothere")
alias(name = "selecting", actual = select({"//conditions:default": "//p:mac", ":on": "//p:mac"}))
exports_files(["data.txt"])
"#,
    )]);
    let at = |line: u32, column: u32, message: &str| {
        format!("WS/e/BUILD.bazel:{line}:{column}: {message}")
    };
    for (target, expected) in [
        (
            "bad_cv",
            at(
                3,
                17,
                "in constraint_setting attribute of constraint_value rule //e:bad_cv: filegroup rule '//e:fg' is misplaced here (expected constraint_setting) and '//e:fg' does not have mandatory providers: 'ConstraintSettingInfo'",
            ),
        ),
        (
            "alias_cv",
            at(
                4,
                17,
                "in constraint_setting attribute of constraint_value rule //e:alias_cv: alias '//os:macos' referring to constraint_value rule '//os:osx' is misplaced here (expected constraint_setting) and '//os:osx' does not have mandatory providers: 'ConstraintSettingInfo'",
            ),
        ),
        (
            "two_parents",
            at(
                5,
                9,
                "in parents attribute of platform rule //e:two_parents: parents attribute must have a single value",
            ),
        ),
        (
            "bad_deps",
            [
                at(
                    6,
                    9,
                    "in constraint_values attribute of platform rule //e:bad_deps: '//e:fg' does not have mandatory providers: 'ConstraintValueInfo'",
                ),
                at(
                    6,
                    9,
                    "in constraint_values attribute of platform rule //e:bad_deps: '//p:mac' does not have mandatory providers: 'ConstraintValueInfo'",
                ),
                at(
                    6,
                    9,
                    "in parents attribute of platform rule //e:bad_deps: '//e:fg' does not have mandatory providers: 'PlatformInfo'",
                ),
                at(
                    6,
                    9,
                    "in required_settings attribute of platform rule //e:bad_deps: filegroup rule '//e:fg' is misplaced here (expected config_setting)",
                ),
            ]
            .join("\n"),
        ),
        (
            "dup",
            at(
                7,
                9,
                "in constraint_values attribute of platform rule //e:dup: Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx], constraint_setting //cpu:cpu has [//cpu:x86_64, //cpu:arm64]",
            ),
        ),
        (
            "both",
            at(
                8,
                9,
                "in exec_properties attribute of platform rule //e:both: Platform contains both remote_execution_properties and exec_properties. Prefer exec_properties over the deprecated remote_execution_properties.",
            ),
        ),
        (
            "other_default",
            at(
                12,
                19,
                "in default_constraint_value attribute of constraint_setting rule //e:other_default: The default constraint value must be defined in the same package as the constraint setting itself.",
            ),
        ),
        (
            "missing_default",
            at(
                13,
                19,
                "in default_constraint_value attribute of constraint_setting rule //e:missing_default: The default constraint value '//e:nothere' does not exist",
            ),
        ),
    ] {
        assert_eq!(
            ws.rejected(&[&format!("//e:{target}")]),
            format!("bazel-rs: {expected}\n"),
            "{target}"
        );
    }
    // Analyzed directly (in the target configuration: platform has no
    // transition), platforms with flags or select() and constraint rules
    // build (nothing to build).
    for target in [
        "//e:flagged",
        "//e:selected",
        "//p:linux_arm",
        "//os:macos",
        "//libc:libc",
    ] {
        let output = ws.run(&["build", target]);
        assert!(output.status.success(), "{target}: {}", errors(&output));
        assert!(output.stdout.is_empty(), "{target}");
    }
    // As target platforms: flags are not modelled; analysis errors are
    // followed by InvalidPlatformException's text; non-platforms fail
    // before analysis (PlatformLookupUtil.java:160-210).
    let not_a_platform = |label: &str| {
        format!("Target {label} was referenced as a platform, but does not provide PlatformInfo")
    };
    for (platform, expected) in [
        (
            "//e:flagged",
            "platform //e:flagged sets flags [--define=a=1]: platform-based flags (platform(flags = ...), analysis/platform/PlatformFunction.java:102-125) are not implemented in the native profile".to_owned(),
        ),
        (
            "//e:dup",
            format!(
                "{}\n{}",
                at(
                    7,
                    9,
                    "in constraint_values attribute of platform rule //e:dup: Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx], constraint_setting //cpu:cpu has [//cpu:x86_64, //cpu:arm64]"
                ),
                not_a_platform("//e:dup")
            ),
        ),
        ("//e:fg", not_a_platform("//e:fg")),
        ("//os:linux", not_a_platform("//os:linux")),
        ("//e:data.txt", not_a_platform("//e:data.txt")),
        // An alias with select() keys uses toolchain resolution.
        ("//e:selecting", not_a_platform("//e:selecting")),
        // Loading errors are printed with their exception class (paired
        // capture A4/platforms_e_nope, A4/platforms_nopkg_p).
        (
            "//e:nope",
            "com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//e:nope': target 'nope' not declared in package 'e' defined by WS/e/BUILD.bazel".to_owned(),
        ),
        (
            "//nopkg:p",
            "com.google.devtools.build.lib.packages.BuildFileNotFoundException: no such package 'nopkg': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - nopkg".to_owned(),
        ),
        (
            "@nope//:p",
            "com.google.devtools.build.lib.packages.BuildFileNotFoundException: no such package '@@[unknown repo 'nope' requested from @@]//': The repository '@@[unknown repo 'nope' requested from @@]' could not be resolved: No repository visible as '@nope' from main repository".to_owned(),
        ),
        // Platforms are analyzed in the empty configuration
        // (PlatformFunction.java:46-52): select() in the platform's tree is
        // an explicit gap.
        (
            "//e:selected",
            "//e:selected: select() in the dependency tree of platform //e:selected is not implemented in the native profile: Bazel analyzes platforms and their dependencies in the empty configuration (analysis/platform/PlatformFunction.java:46-52, analysis/config/CommonOptions.java:32-42), which the native profile does not model".to_owned(),
        ),
    ] {
        assert_eq!(
            ws.rejected(&[&format!("--platforms={platform}"), "//p:bare"]),
            format!("bazel-rs: {expected}\n"),
            "{platform}"
        );
    }
    // A non-default --host_platform is the execution platform of
    // toolchain-resolving rules only: a platform target resolves no
    // toolchains (paired capture A4/host_platform_fg builds in Bazel) ...
    let output = ws.run(&[
        "build",
        "--platforms=//p:bare",
        "--host_platform=//e:fg",
        "//p:bare",
    ]);
    assert!(output.status.success(), "{}", errors(&output));
    // ... while a filegroup reports it while resolving its toolchains
    // (B2/host_host_platform_fg_platforms_p_mac; the configuration
    // identifier is the native one).
    let diagnostic = ws.rejected(&["--platforms=//p:bare", "--host_platform=//e:fg", "//e:fg"]);
    let (head, tail) = diagnostic
        .split_once(" (")
        .unwrap_or_else(|| panic!("{diagnostic}"));
    assert_eq!(
        head,
        "bazel-rs: WS/e/BUILD.bazel:2:10: While resolving toolchains for target //e:fg"
    );
    assert!(tail[..7].bytes().all(|b| b.is_ascii_hexdigit()), "{tail}");
    assert_eq!(&tail[7..], format!("): {}\n", not_a_platform("//e:fg")));
    // Without --platforms, --host_platform is the target platform of every
    // toolchain-resolving rule.
    assert_eq!(
        ws.rejected(&["--host_platform=//e:fg", "//e:fg"]),
        format!("bazel-rs: {}\n", not_a_platform("//e:fg"))
    );
}

#[test]
fn platforms_are_resolved_as_in_the_empty_configuration() {
    let ws = Workspace::constrained(&[(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
label_flag(name = "plat_flag", build_setting_default = "//p:mac")
label_flag(name = "fg_flag", build_setting_default = ":fg")
alias(name = "flag_alias", actual = ":plat_flag")
filegroup(name = "fg")
alias(name = "sel_alias", actual = select({":opt": "//p:mac", "//conditions:default": "//p:linux_x86"}))
platform(name = "sel_parent", parents = [":sel_alias"])
platform(name = "flag_parent", parents = [":plat_flag"])
platform(name = "required", constraint_values = ["//os:linux"], required_settings = [":opt"])
platform(name = "required_child", parents = [":required"])
show(name = "direct", mode = select({"//os:linux": "linux", "//os:macos": "osx", "//conditions:default": "other"}))
"#,
    )]);
    let not_a_platform = |label: &str| {
        format!("Target {label} was referenced as a platform, but does not provide PlatformInfo")
    };
    // A label setting has no value in the empty configuration
    // (CommonOptions.java:32-42): it provides no PlatformInfo, and Bazel names
    // it (paired capture B2/platforms*_plat_flag, *_fg_flag).
    for (platform, named) in [
        ("//:plat_flag", "//:plat_flag"),
        ("//:fg_flag", "//:fg_flag"),
        ("//:flag_alias", "//:plat_flag"),
    ] {
        for option in ["--platforms", "--host_platform"] {
            assert_eq!(
                ws.rejected(&[&format!("{option}={platform}"), "//:direct"]),
                format!("bazel-rs: {}\n", not_a_platform(named)),
                "{option}={platform}"
            );
        }
    }
    let gap = |what: &str, platform: &str| {
        format!(
            "bazel-rs: {what} in the dependency tree of platform {platform} is not implemented in the native profile: Bazel analyzes platforms and their dependencies in the empty configuration (analysis/platform/PlatformFunction.java:46-52, analysis/config/CommonOptions.java:32-42), which the native profile does not model\n"
        )
    };
    for (platform, what) in [
        // (a) select() anywhere in the tree.
        ("//:sel_parent", "//:sel_alias: select()"),
        // (c) required_settings (execution platform resolution only, N17).
        (
            "//:required",
            "//:required: required_settings (config_setting matching for execution platform resolution, N17)",
        ),
        (
            "//:required_child",
            "//:required: required_settings (config_setting matching for execution platform resolution, N17)",
        ),
    ] {
        assert_eq!(
            ws.rejected(&[&format!("--platforms={platform}"), "//:direct"]),
            gap(what, platform),
            "{platform}"
        );
    }
    // (b) A label setting deeper in the tree has no providers there either:
    // the platform's prerequisite error (paired capture A8/gap_flag_parent).
    assert_eq!(
        ws.rejected(&["--platforms=//:flag_parent", "//:direct"]),
        format!(
            "bazel-rs: WS/BUILD.bazel:9:9: in parents attribute of platform rule //:flag_parent: '//:plat_flag' does not have mandatory providers: 'PlatformInfo'\n{}\n",
            not_a_platform("//:flag_parent")
        )
    );
    // Analyzed directly, they build in the target configuration.
    for target in ["//:sel_parent", "//:flag_parent", "//:required"] {
        let output = ws.run(&["build", target]);
        assert!(output.status.success(), "{target}: {}", errors(&output));
    }
    // A label setting's command-line value does not reach a platform.
    assert_eq!(
        ws.rejected(&[
            "--platforms=//:plat_flag",
            "--//:plat_flag=//p:linux_x86",
            "//:direct"
        ]),
        format!("bazel-rs: {}\n", not_a_platform("//:plat_flag"))
    );
}

/// Rules reading the platform providers (paired capture B4).
const PLATFORM_API: &str = r#"
def _w(ctx, text):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, text)
    return [DefaultInfo(files = depset([out]))]

def _has_impl(ctx):
    return _w(ctx, "has=" + repr(ctx.target_platform_has_constraint(ctx.attr.cv[platform_common.ConstraintValueInfo])) + "\n")

has = rule(implementation = _has_impl, attrs = {"cv": attr.label(providers = [platform_common.ConstraintValueInfo])})

def _provided_impl(ctx):
    d = ctx.attr.dep
    return _w(ctx, "platform=%s\ncv=%s\ncs=%s\n" % (platform_common.PlatformInfo in d, platform_common.ConstraintValueInfo in d, platform_common.ConstraintSettingInfo in d))

provided = rule(implementation = _provided_impl, attrs = {"dep": attr.label()})

def _cs_impl(ctx):
    cs = ctx.attr.dep[platform_common.ConstraintSettingInfo]
    return _w(ctx, "has_default=%s\n" % cs.has_default_constraint_value)

cs_has_default = rule(implementation = _cs_impl, attrs = {"dep": attr.label()})

def _bad_impl(ctx):
    ctx.target_platform_has_constraint("//os:linux")
    return []

bad = rule(implementation = _bad_impl)

def _setting_impl(ctx):
    return _w(ctx, "has=" + repr(ctx.target_platform_has_constraint(ctx.attr.cv[platform_common.ConstraintValueInfo])) + "\n")

setting_has = rule(implementation = _setting_impl, build_setting = config.string(flag = True), attrs = {"cv": attr.label()})
"#;

#[test]
fn starlark_reads_platform_providers_and_the_target_platform() {
    let ws = Workspace::constrained(&[
        ("api.bzl", PLATFORM_API),
        (
            "BUILD.bazel",
            r#"load(":api.bzl", "bad", "cs_has_default", "has", "provided", "setting_has")
has(name = "has_linux", cv = "//os:linux")
has(name = "has_macos_alias", cv = "//os:macos")
has(name = "has_glibc", cv = "//libc:glibc")
[provided(name = "provided_" + n.replace("/", "_").replace(":", "_"), dep = n) for n in ["//p:mac", "//p:mac_alias", "//os:linux", "//os:os"]]
cs_has_default(name = "cs_libc", dep = "//libc:libc")
cs_has_default(name = "cs_os", dep = "//os:os")
bad(name = "bad")
setting_has(name = "setting_has", build_setting_default = "", cv = "//os:linux")
"#,
        ),
    ]);
    // ctx.target_platform_has_constraint (StarlarkRuleContext.java:902-904):
    // the target platform's value for the setting, defaults included.
    for (platform, linux, macos, glibc) in [
        ("//p:linux_x86", "True", "False", "True"),
        ("//p:mac", "False", "True", "True"),
        ("//p:linux_arm", "True", "False", "False"),
    ] {
        for (target, expected) in [
            ("//:has_linux", linux),
            ("//:has_macos_alias", macos),
            ("//:has_glibc", glibc),
        ] {
            assert_eq!(
                ws.built(&[&format!("--platforms={platform}"), target]),
                format!("has={expected}\n"),
                "{platform} {target}"
            );
        }
    }
    // A build setting rule has no toolchain context: always False.
    assert_eq!(
        ws.built(&["--platforms=//p:linux_x86", "//:setting_has"]),
        "has=False\n"
    );
    // dep[platform_common.X] and `in` (aliases forward the providers).
    for (target, expected) in [
        ("//:provided___p_mac", "platform=True\ncv=False\ncs=False\n"),
        (
            "//:provided___p_mac_alias",
            "platform=True\ncv=False\ncs=False\n",
        ),
        (
            "//:provided___os_linux",
            "platform=False\ncv=True\ncs=False\n",
        ),
        ("//:provided___os_os", "platform=False\ncv=False\ncs=True\n"),
        ("//:cs_libc", "has_default=True\n"),
        ("//:cs_os", "has_default=False\n"),
    ] {
        assert_eq!(
            ws.built(&["--platforms=//p:linux_x86", target]),
            expected,
            "{target}"
        );
    }
    let diagnostic = ws.rejected(&["--platforms=//p:linux_x86", "//:bad"]);
    assert!(
        diagnostic.contains("in call to target_platform_has_constraint(), parameter 'constraintValue' got value of type 'string', want 'ConstraintValueInfo'"),
        "{diagnostic}"
    );
    // The default host platform is resolved only when the implementation
    // asks for it (here: unavailable without the built-in profile).
    let diagnostic = ws.rejected(&["//:has_linux"]);
    assert!(
        diagnostic.starts_with("bazel-rs: the target platform @@bazel_tools//tools:host_platform (the default host platform) needs the built-in @bazel_tools repository"),
        "{diagnostic}"
    );
    assert_eq!(ws.built(&["//:cs_libc"]), "has_default=True\n");
}

#[test]
fn load_and_ordering_errors_follow_bazel() {
    let ws = Workspace::constrained(&[
        (
            "BUILD.bazel",
            r#"load(":defs.bzl", "show")
config_setting(name = "cv_setting", constraint_values = ["//os:os"])
config_setting(name = "cv_missing", constraint_values = ["//os:nothere"])
show(name = "use_cv_missing", mode = select({":cv_missing": "a", "//conditions:default": "b"}))
show(name = "missing_key", mode = select({"//os:nothere": "a", "//conditions:default": "b"}))
"#,
        ),
        (
            "dupl/BUILD.bazel",
            r#"load("//:defs.bzl", "show")
filegroup(name = "fg")
config_setting(name = "dup_same", constraint_values = ["//os:linux", "//os:linux"])
platform(name = "dup_same_platform", constraint_values = ["//os:linux", "//os:linux"])
"#,
        ),
        (
            "dupsel/BUILD.bazel",
            r#"filegroup(name = "a", srcs = ["x", "y"] + select({"//os:linux": ["x"], "//conditions:default": []}))
filegroup(name = "b", srcs = select({"//os:linux": ["z", "z"], "//conditions:default": []}))
"#,
        ),
    ]);
    // A config_setting reports a non-constraint_value before it needs the
    // (default host) target platform (paired capture B2/direct_cv_setting).
    assert_eq!(
        ws.rejected(&["//:cv_setting"]),
        "bazel-rs: WS/BUILD.bazel:2:15: in constraint_values attribute of config_setting rule //:cv_setting: //os:os is not a constraint_value\n"
    );
    // A missing target of another package is first reported at its BUILD
    // file (B2/lx_use_cv_missing, lx_missing_key).
    let missing = "no such target '//os:nothere': target 'nothere' not declared in package 'os' defined by WS/os/BUILD.bazel";
    assert_eq!(
        ws.rejected(&["--platforms=//p:linux_x86", "//:use_cv_missing"]),
        format!(
            "bazel-rs: WS/os/BUILD.bazel: {missing}\nWS/BUILD.bazel:3:15: {missing} and referenced by '//:cv_missing'\nWS/BUILD.bazel:4:5: errors encountered resolving select() keys for //:use_cv_missing\n"
        )
    );
    assert_eq!(
        ws.rejected(&["--platforms=//p:linux_x86", "//:missing_key"]),
        format!(
            "bazel-rs: WS/os/BUILD.bazel: {missing}\nWS/BUILD.bazel:5:5: errors encountered resolving select() keys for //:missing_key\n"
        )
    );
    // Duplicate labels in a label list are package errors
    // (RuleClass.checkForDuplicateLabels): every target of the package fails.
    let duplicated = "bazel-rs: WS/dupl/BUILD.bazel:3:15: Label '//os:linux' is duplicated in the 'constraint_values' attribute of rule 'dup_same'\nWS/dupl/BUILD.bazel:4:9: Label '//os:linux' is duplicated in the 'constraint_values' attribute of rule 'dup_same_platform'\n";
    for target in ["//dupl:fg", "//dupl:dup_same"] {
        assert_eq!(ws.rejected(&[target]), duplicated, "{target}");
    }
    assert_eq!(
        ws.rejected(&["--platforms=//dupl:dup_same_platform", "//:missing_key"]),
        duplicated
    );
    // Each select() branch and the plain value are checked separately
    // (AggregatingAttributeMapper.java:280-303).
    assert_eq!(
        ws.rejected(&["//dupsel:a"]),
        "bazel-rs: WS/dupsel/BUILD.bazel:2:10: Label '//dupsel:z' is duplicated in the 'srcs' attribute of rule 'b'\n"
    );
    // query has no platform options (QueryCommand.java:59-67).
    let output = ws.run(&["query", "--platforms=//p:mac", "//:cv_setting"]);
    assert_eq!(
        errors(&output),
        "bazel-rs: --platforms=//p:mac :: Unrecognized option: --platforms=//p:mac\n"
    );
}

#[test]
fn refined_constraint_values_validate_platforms_and_specialize() {
    let ws = Workspace::constrained(&[(
        "BUILD.bazel",
        r#"load(":defs.bzl", "show")
constraint_setting(name = "distro", default_constraint_value = ":unknown", refines_constraint_value = "//os:linux")
constraint_value(name = "unknown", constraint_setting = ":distro")
constraint_value(name = "debian", constraint_setting = ":distro")
platform(name = "debian_linux", constraint_values = ["//os:linux", ":debian"])
platform(name = "debian_only", constraint_values = [":debian"])
platform(name = "debian_mac", constraint_values = ["//os:macos", ":debian"])
platform(name = "unknown_mac", constraint_values = ["//os:macos", ":unknown"])
config_setting(name = "is_debian", constraint_values = [":debian"])
config_setting(name = "is_linux", constraint_values = ["//os:linux"])
show(name = "pick", mode = select({":is_linux": "linux", ":is_debian": "debian"}))
"#,
    )]);
    // The refining value's config_setting also specifies the refined value
    // (ConfigSetting.getSpecifiedConstraintValues): it is more specialized.
    assert_eq!(
        ws.built(&["--platforms=//:debian_linux", "//:pick"]),
        "mode=\"debian\"\n"
    );
    // The default value carries no refinement implication.
    let output = ws.run(&["build", "//:unknown_mac"]);
    assert!(output.status.success(), "{}", errors(&output));
    for (platform, line, message) in [
        (
            "debian_only",
            6,
            "constraint_value //:debian refines //os:linux, but platform //:debian_only does not set the latter. Fix with:\n  buildozer 'add constraint_values //os:linux' //:debian_only",
        ),
        (
            "debian_mac",
            7,
            "constraint_value //:debian refines //os:linux, but platform //:debian_mac sets the conflicting constraint_value //os:osx for //os",
        ),
    ] {
        assert_eq!(
            ws.rejected(&[&format!("//:{platform}")]),
            format!(
                "bazel-rs: WS/BUILD.bazel:{line}:9: in constraint_values attribute of platform rule //:{platform}: {message}\n"
            )
        );
    }
}

/// Extract the genuine platforms 1.0.0 module archive
/// (tests/fixtures/label_repo_name/platforms_archive.json) into `to`.
fn extract_platforms(to: &Path) {
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/label_repo_name/platforms_archive.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
    assert_eq!(
        format!("{digest:x}"),
        manifest["sha256"].as_str().unwrap(),
        "genuine platforms archive"
    );
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
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

/// A rule that prints before asking for the target platform.
const PRINT_DEFS: &str = r#"
def _impl(ctx):
    print("N16C-PRINT-MARKER")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    has = ctx.target_platform_has_constraint(ctx.attr.cv[platform_common.ConstraintValueInfo])
    ctx.actions.write(out, "has=" + repr(has) + "\n")
    return [DefaultInfo(files = depset([out]))]

printing = rule(implementation = _impl, attrs = {"cv": attr.label()})
"#;

const PRINT_BUILD: &str =
    "load(':defs.bzl', 'printing')\nprinting(name = 'p', cv = '@platforms//os:linux')\n";

#[test]
fn default_host_platform_comes_from_the_platforms_host_extension() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'platforms', version = '1.0.0')\nlocal_path_override(module_name = 'platforms', path = 'platforms')\n",
        &[
            (
                "BUILD.bazel",
                r#"load(":api.bzl", "has")
load(":defs.bzl", "show")
config_setting(name = "linux_x86_64", constraint_values = ["@platforms//os:linux", "@platforms//cpu:x86_64"])
show(name = "host", mode = select({
    "@platforms//os:osx": "osx",
    ":linux_x86_64": "linux_x86_64",
    "@platforms//os:linux": "linux",
    "//conditions:default": "other",
}))
show(name = "arm", mode = select({"@platforms//cpu:aarch64": "arm", "//conditions:default": "not arm"}))
platform(name = "mac", constraint_values = ["@platforms//os:osx", "@platforms//cpu:aarch64"])
has(name = "has_linux", cv = "@platforms//os:linux")
"#,
            ),
            ("api.bzl", PLATFORM_API),
            ("print/BUILD.bazel", PRINT_BUILD),
            ("print/defs.bzl", PRINT_DEFS),
        ],
    );
    extract_platforms(&ws.path().join("platforms"));
    let builtin = [
        "--experimental-builtin-modules".to_owned(),
        format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            env!("CARGO_MANIFEST_DIR")
        ),
    ];
    let with = |args: &[&str]| {
        let mut all = builtin.iter().map(String::as_str).collect::<Vec<_>>();
        all.extend_from_slice(args);
        ws.built(&all)
    };
    // @bazel_tools//tools:host_platform is an alias of @platforms//host,
    // whose HOST_CONSTRAINTS the host_platform extension derives from
    // repository_ctx.os (linux/amd64 on this host profile, docs/HOST_OS.md).
    assert_eq!(with(&["//:host"]), "mode=\"linux_x86_64\"\n");
    assert_eq!(with(&["//:arm"]), "mode=\"not arm\"\n");
    // The default spelled explicitly is the same configuration.
    assert_eq!(
        with(&["--platforms=@bazel_tools//tools:host_platform", "//:host"]),
        "mode=\"linux_x86_64\"\n"
    );
    assert_eq!(
        with(&["--platforms=@platforms//host", "//:host"]),
        "mode=\"linux_x86_64\"\n"
    );
    assert_eq!(with(&["--platforms=//:mac", "//:host"]), "mode=\"osx\"\n");
    assert_eq!(with(&["--platforms=//:mac", "//:arm"]), "mode=\"arm\"\n");
    // ctx.target_platform_has_constraint resolves the default host platform
    // when the implementation asks for it.
    assert_eq!(with(&["//:has_linux"]), "has=True\n");
    // The analysis is then repeated; print() output of the discarded first
    // attempt is dropped, so it appears once.
    let mut args = vec!["build"];
    args.extend(builtin.iter().map(String::as_str));
    args.push("//print:p");
    let output = ws.run(&args);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(
        errors(&output).matches("N16C-PRINT-MARKER").count(),
        1,
        "{}",
        errors(&output)
    );
    assert_eq!(with(&["--platforms=//:mac", "//:has_linux"]), "has=False\n");
    // Without the built-in profile there is no @bazel_tools: the default host
    // platform fails explicitly where it is needed, an explicit platform works.
    let output = ws.run(&["build", &builtin[1], "//:host"]);
    assert_eq!(output.status.code(), Some(1), "{}", errors(&output));
    assert!(
        errors(&output).starts_with("bazel-rs: the target platform @@bazel_tools//tools:host_platform (the default host platform) needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide ("),
        "{}",
        errors(&output)
    );
    let output = ws.run(&["build", &builtin[1], "--platforms=//:mac", "//:host"]);
    assert!(output.status.success(), "{}", errors(&output));
}

/// tests/fixtures/extra_execution_platforms in a fresh workspace: the
/// genuine platforms 1.0.0 module extracted into `platforms`, and N30's
/// builtin-closure stubs as `local_path_override`s, which keep the built-in
/// profile offline (none registers a toolchain of the fixture's type;
/// tests/aspects.rs's approach).
fn extra_execution_platforms_fixture() -> Workspace {
    fn copy_tree(from: &Path, to: &Path) {
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir_all(&target).unwrap();
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ws = Workspace(
        tempfile::Builder::new()
            .prefix("xep-")
            .tempdir_in(base)
            .unwrap(),
    );
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    copy_tree(
        &manifest.join("tests/fixtures/extra_execution_platforms/workspace"),
        ws.path(),
    );
    extract_platforms(&ws.path().join("platforms"));
    let mut module = fs::read_to_string(ws.path().join("MODULE.bazel")).unwrap();
    for name in [
        "apple_support",
        "buildozer",
        "protobuf",
        "rules_cc",
        "rules_java",
        "rules_python",
        "rules_shell",
        "zlib",
    ] {
        module.push_str(&format!(
            "local_path_override(module_name = '{name}', path = '{}')\n",
            manifest
                .join("tests/fixtures/test_flags/stubs")
                .join(name)
                .display()
        ));
    }
    fs::write(ws.path().join("MODULE.bazel"), module).unwrap();
    ws
}

/// `COMMAND ARGS` under the built-in module profile with the offline
/// fixture registry, as the paired capture runs it.
fn run_builtin(ws: &Workspace, command: &str, args: &[&str]) -> Output {
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut all = vec![command, "--experimental-builtin-modules", registry.as_str()];
    all.extend_from_slice(args);
    ws.run(&all)
}

/// The top-level outputs of a successful `build ARGS`: path below
/// `bazel-out` and contents, sorted by path.
fn built_outputs(ws: &Workspace, args: &[&str]) -> Vec<(String, String)> {
    let output = run_builtin(ws, "build", args);
    assert!(output.status.success(), "{args:?}: {}", errors(&output));
    let mut outputs = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let path = line.split_once('\t').unwrap().1;
            (
                path.split_once("/bazel-out/").unwrap().1.to_owned(),
                fs::read_to_string(path).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    outputs.sort();
    outputs.dedup();
    outputs
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(path, contents)| ((*path).to_owned(), (*contents).to_owned()))
        .collect()
}

#[test]
fn module_execution_platform_registrations_follow_bazel() {
    // Genuine capture: tests/fixtures/extra_execution_platforms/module_registrations.
    let ws = extra_execution_platforms_fixture();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extra_execution_platforms/module_registrations/dep");
    fs::create_dir_all(ws.path().join("dep")).unwrap();
    for name in ["MODULE.bazel", "BUILD.bazel"] {
        fs::copy(fixture.join(name), ws.path().join("dep").join(name)).unwrap();
    }
    let mut module = fs::read_to_string(ws.path().join("MODULE.bazel")).unwrap();
    module.push_str(
        "bazel_dep(name = 'dep')\nlocal_path_override(module_name = 'dep', path = 'dep')\nregister_execution_platforms('//p:bar')\n",
    );
    fs::write(ws.path().join("MODULE.bazel"), module).unwrap();
    assert_eq!(
        built_outputs(&ws, &["//:tool", "//:plain"]),
        owned(&[
            (
                "k8-fastbuild/bin/plain.out",
                "leaf=bazel-out/bar-opt-exec/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/tool.out",
                "tc=freebsd_exec\nleaf=bazel-out/freebsd-opt-exec/bin/exec_leaf.out\n"
            ),
        ])
    );
}

#[test]
fn extra_execution_platforms_come_before_the_host_platform() {
    // Paired capture tests/fixtures/extra_execution_platforms
    // (reference_receipt.json; genuine Bazel 9.3.0 outputs below).
    let ws = extra_execution_platforms_fixture();
    // rules_uv's transition_to_target: extra_execution_platforms = the
    // target platforms ("@@bazel_tools//tools:host_platform", an alias of
    // the host platform): the host only, in a configuration with an ST- hash
    // (also its exec configuration's).
    assert_eq!(
        built_outputs(&ws, &["//:pip"]),
        owned(&[
            (
                "k8-fastbuild-ST-f7ead6e6535a/bin/plain.out",
                "leaf=bazel-out/k8-opt-exec-ST-f7ead6e6535a/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild-ST-f7ead6e6535a/bin/tool.out",
                "tc=linux_exec\nleaf=bazel-out/k8-opt-exec-ST-f7ead6e6535a/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/pip.out",
                "bazel-out/k8-fastbuild-ST-f7ead6e6535a/bin/tool.out\nbazel-out/k8-fastbuild-ST-f7ead6e6535a/bin/plain.out\n"
            ),
        ])
    );
    // The transitioned execution platforms are //p:foo, then the host: the
    // toolchain resolves for //p:foo (first of the platforms with the most
    // toolchains), and a rule without toolchain types runs on //p:foo.
    assert_eq!(
        built_outputs(&ws, &["--platforms=//p:foo", "//:pip"]),
        owned(&[
            (
                "k8-fastbuild-ST-51b47c7fe43b/bin/plain.out",
                "leaf=bazel-out/foo-opt-exec-ST-51b47c7fe43b/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild-ST-51b47c7fe43b/bin/tool.out",
                "tc=freebsd_exec\nleaf=bazel-out/foo-opt-exec-ST-51b47c7fe43b/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/pip.out",
                "bazel-out/k8-fastbuild-ST-51b47c7fe43b/bin/tool.out\nbazel-out/k8-fastbuild-ST-51b47c7fe43b/bin/plain.out\n"
            ),
        ])
    );
    // On the command line (part of the exec baseline: no ST- hash); an
    // alias resolves; the last occurrence wins.
    let foo = owned(&[
        (
            "k8-fastbuild/bin/plain.out",
            "leaf=bazel-out/foo-opt-exec/bin/exec_leaf.out\n",
        ),
        (
            "k8-fastbuild/bin/tool.out",
            "tc=freebsd_exec\nleaf=bazel-out/foo-opt-exec/bin/exec_leaf.out\n",
        ),
    ]);
    assert_eq!(
        built_outputs(
            &ws,
            &["--extra_execution_platforms=//p:foo", "//:tool", "//:plain"]
        ),
        foo
    );
    assert_eq!(
        built_outputs(
            &ws,
            &[
                "--extra_execution_platforms=//p:foo_alias",
                "//:tool",
                "//:plain"
            ]
        ),
        foo
    );
    assert_eq!(
        built_outputs(
            &ws,
            &[
                "--extra_execution_platforms=//p:bar",
                "--extra_execution_platforms=//p:foo",
                "//:plain"
            ]
        ),
        foo[..1]
    );
    // The transition writes the value the configuration already has: a
    // no-op (the option compares as strings).
    assert_eq!(
        built_outputs(
            &ws,
            &[
                "--extra_execution_platforms=@@bazel_tools//tools:host_platform",
                "//:pip"
            ]
        ),
        owned(&[
            (
                "k8-fastbuild/bin/pip.out",
                "bazel-out/k8-fastbuild/bin/tool.out\nbazel-out/k8-fastbuild/bin/plain.out\n"
            ),
            (
                "k8-fastbuild/bin/plain.out",
                "leaf=bazel-out/k8-opt-exec/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/tool.out",
                "tc=linux_exec\nleaf=bazel-out/k8-opt-exec/bin/exec_leaf.out\n"
            ),
        ])
    );
    // A transition reads the option as a list of strings.
    let output = run_builtin(&ws, "build", &["//:reader"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains("READ list []"),
        "{}",
        errors(&output)
    );
    assert_eq!(
        built_outputs(&ws, &["//:reader"]),
        owned(&[
            (
                "k8-fastbuild-ST-1373b923d83a/bin/tool.out",
                "tc=linux_exec\nleaf=bazel-out/k8-opt-exec-ST-1373b923d83a/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/reader.out",
                "bazel-out/k8-fastbuild-ST-1373b923d83a/bin/tool.out\n"
            ),
        ])
    );
    let output = run_builtin(
        &ws,
        "build",
        &["--extra_execution_platforms=//p:foo", "//:reader"],
    );
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains("READ list [\"//p:foo\"]"),
        "{}",
        errors(&output)
    );
    assert_eq!(
        built_outputs(&ws, &["--extra_execution_platforms=//p:foo", "//:reader"]),
        owned(&[
            (
                "k8-fastbuild-ST-8292844d64e9/bin/tool.out",
                "tc=freebsd_exec\nleaf=bazel-out/foo-opt-exec-ST-8292844d64e9/bin/exec_leaf.out\n"
            ),
            (
                "k8-fastbuild/bin/reader.out",
                "bazel-out/k8-fastbuild-ST-8292844d64e9/bin/tool.out\n"
            ),
        ])
    );
}

#[test]
fn extra_execution_platform_errors_follow_bazel() {
    let ws = extra_execution_platforms_fixture();
    let rejected = |args: &[&str]| {
        let output = run_builtin(&ws, "build", args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output on failure");
        errors(&output).replace(&ws.path().display().to_string(), "WS")
    };
    // The print() output precedes the transition's error; the edge names
    // the private attribute by its native name (`$tool`).
    let text = rejected(&["//:none_reader"]);
    assert!(
        text.contains("READ list []\nbazel-rs: WS/defs.bzl:38:29: 'None' value not allowed for List-type option 'extra_execution_platforms'. Please use '[]' instead if trying to set option to empty value.\nWS/BUILD.bazel:17:7: on dependency edge //:none_reader ("),
        "{text}"
    );
    assert!(
        text.contains(
            ") -|$tool|-> //:tool: Errors encountered while applying Starlark transition\n"
        ),
        "{text}"
    );
    // RegisteredExecutionPlatformsCycleReporter: a registered target that
    // resolves toolchains itself.
    assert_eq!(
        rejected(&["--extra_execution_platforms=//p:not_platform", "//:tool"]),
        "bazel-rs: Misconfigured execution platforms: //p:not_platform is declared as a platform but has inappropriate dependencies. Execution platforms should not have dependencies that themselves require toolchain resolution.\n.-> RegisteredExecutionPlatforms\n|   //p:not_platform\n|   toolchain types \n`-- RegisteredExecutionPlatforms\n"
    );
    let text = rejected(&["--extra_execution_platforms=//p:missing", "//:tool"]);
    assert!(
        text.starts_with(
            "bazel-rs: WS/BUILD.bazel:5:5: While resolving toolchains for target //:tool ("
        ),
        "{text}"
    );
    assert!(
        text.ends_with("): invalid registered execution platform '//p:missing': no such target '//p:missing': target 'missing' not declared in package 'p' defined by WS/p/BUILD.bazel\n"),
        "{text}"
    );
    // Two registered labels of one platform crash Bazel 9.3 (exit 37,
    // ImmutableMap.buildOrThrow): explicit natively. `//p:all` keeps the
    // alias (alias-like rules may have any provider).
    for platforms in ["//p:all", "//p:foo,//p:foo_alias"] {
        let text = rejected(&[
            &format!("--extra_execution_platforms={platforms}"),
            "//:tool",
        ]);
        assert!(
            text.starts_with("bazel-rs: the registered execution platforms //p:foo and //p:foo_alias are the same platform //p:foo: Bazel 9.3 crashes on it (IllegalArgumentException"),
            "{text}"
        );
    }
    // `query` registers no PlatformOptions.
    let output = run_builtin(
        &ws,
        "query",
        &["--extra_execution_platforms=//p:foo", "//:tool"],
    );
    assert_eq!(
        errors(&output),
        "bazel-rs: --extra_execution_platforms=//p:foo :: Unrecognized option: --extra_execution_platforms=//p:foo\n"
    );
}
