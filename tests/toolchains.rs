//! CLI coverage for N17a/N17b (docs/TOOLCHAINS.md): `toolchain_type` and
//! `toolchain` analysis, registered toolchains (`register_toolchains`,
//! `--extra_toolchains`), toolchain resolution against the target platform and
//! the host execution platform, and `platform_common.ToolchainInfo` /
//! `ctx.toolchains`. Expected texts follow the pinned Bazel 9.3 sources cited
//! in src/toolchains.rs, src/toolchain_info.rs and src/engine.rs.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(module: &str, files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n17-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", module);
        for (path, content) in BASE.iter().chain(files) {
            ws.file(path, content);
        }
        ws
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
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    /// Contents of the single output of `build --host_platform=//p:linux ARGS`.
    fn built(&self, args: &[&str]) -> String {
        let mut command = vec!["build", "--host_platform=//p:linux"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let lines = stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{stdout}");
        fs::read_to_string(lines[0].split_once('\t').unwrap().1).unwrap()
    }

    /// The diagnostic of a failing `build --host_platform=//p:linux ARGS`,
    /// with the workspace path replaced by `WS` and configuration
    /// identifiers by `CONFIG`.
    fn rejected(&self, args: &[&str]) -> String {
        let mut command = vec!["build", "--host_platform=//p:linux"];
        command.extend_from_slice(args);
        self.rejected_raw(&command)
    }

    fn rejected_raw(&self, command: &[&str]) -> String {
        let output = self.run(command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{command:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output on failure");
        let diagnostic = errors(&output);
        assert!(!diagnostic.contains("panicked"), "{diagnostic}");
        assert!(!diagnostic.contains("actions executed"), "{diagnostic}");
        let diagnostic = diagnostic.replace(&self.path().display().to_string(), "WS");
        normalize_config(&diagnostic)
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

/// `(abcdef0)` configuration identifiers in `While resolving toolchains for
/// target L (CONFIG)` become `(CONFIG)`.
fn normalize_config(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(" (") {
        result.push_str(&rest[..index]);
        let candidate = &rest[index + 2..];
        if candidate.len() >= 8
            && candidate[..7].bytes().all(|byte| byte.is_ascii_hexdigit())
            && candidate.as_bytes()[7] == b')'
        {
            result.push_str(" (CONFIG)");
            rest = &candidate[8..];
        } else {
            result.push_str(" (");
            rest = candidate;
        }
    }
    result.push_str(rest);
    result
}

/// Constraints, platforms, toolchain rules and a toolchain-using rule.
const BASE: &[(&str, &str)] = &[
    (
        "os/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "osx", constraint_setting = ":os")
# A setting with a default value, like zig's libc variants.
constraint_setting(name = "libc", default_constraint_value = ":unconstrained")
constraint_value(name = "unconstrained", constraint_setting = ":libc")
constraint_value(name = "gnu.2.28", constraint_setting = ":libc")
"#,
    ),
    (
        "p/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
platform(name = "linux", constraint_values = ["//os:linux"])
platform(name = "mac", constraint_values = ["//os:osx"])
platform(name = "gnu", constraint_values = ["//os:linux", "//os:gnu.2.28"])
platform(name = "quiet", constraint_values = ["//os:linux"], missing_toolchain_error = "")
platform(name = "custom", constraint_values = ["//os:linux"], missing_toolchain_error = "Install the right SDK.")
platform(
    name = "strict",
    constraint_values = ["//os:linux"],
    check_toolchain_types = True,
    allowed_toolchain_types = ["//tc:type"],
)
alias(name = "linux_alias", actual = ":linux")
"#,
    ),
    ("r/BUILD.bazel", ""),
    (
        "r/defs.bzl",
        r#"def _impl_impl(ctx):
    return [
        platform_common.ToolchainInfo(name = ctx.attr.tool_name, tool = ctx.file.tool),
        DefaultInfo(),
    ]

impl = rule(
    implementation = _impl_impl,
    attrs = {"tool_name": attr.string(), "tool": attr.label(allow_single_file = True)},
)

def _noinfo_impl(ctx):
    return []

noinfo = rule(implementation = _noinfo_impl)

def _broken_impl(ctx):
    fail("broken implementation")

broken = rule(implementation = _broken_impl)

def _show(ctx, toolchain):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    info = ctx.toolchains[toolchain]
    inputs = [info.tool] if info.tool else []
    ctx.actions.run_shell(
        outputs = [out],
        inputs = inputs,
        command = "echo name=%s tool=%s > %s" % (info.name, info.tool.basename if info.tool else None, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

def _use_impl(ctx):
    return _show(ctx, "//tc:type")

use = rule(implementation = _use_impl, toolchains = ["//tc:type"])

def _use_other_impl(ctx):
    return _show(ctx, "//tc:other")

use_other = rule(implementation = _use_other_impl, toolchains = ["//tc:other"])
"#,
    ),
    (
        "tc/BUILD.bazel",
        r#"load("//r:defs.bzl", "impl")
package(default_visibility = ["//visibility:public"])
toolchain_type(name = "type")
toolchain_type(name = "other")
impl(name = "linux_impl", tool_name = "linux", tool = "tool.sh")
impl(name = "mac_impl", tool_name = "mac")
impl(name = "other_impl", tool_name = "other")
toolchain(name = "mac", toolchain_type = ":type", target_compatible_with = ["//os:osx"], toolchain = ":mac_impl")
toolchain(name = "linux", toolchain_type = ":type", exec_compatible_with = ["//os:linux"], target_compatible_with = ["//os:linux"], toolchain = ":linux_impl")
toolchain(name = "other_any", toolchain_type = ":other", toolchain = ":other_impl")
"#,
    ),
    ("tc/tool.sh", "#!/bin/sh\n"),
    (
        "u/BUILD.bazel",
        r#"load("//r:defs.bzl", "use", "use_other")
use(name = "u")
use_other(name = "o")
genrule(name = "g", outs = ["g.txt"], cmd = "echo g > $@")
"#,
    ),
];

const REGISTER_TC: &str = "module(name = 'main')\nregister_toolchains('//tc:all')\n";

#[test]
fn resolution_selects_by_target_and_execution_platform() {
    let ws = Workspace::new(REGISTER_TC, &[]);
    // The target platform is the host platform: //tc:linux (registration
    // order is :all's name order: linux, mac, other_any).
    assert_eq!(ws.built(&["//u:u"]), "name=linux tool=tool.sh\n");
    // --platforms changes the target platform only; the execution platform
    // stays the host platform.
    assert_eq!(
        ws.built(&["--platforms=//p:mac", "//u:u"]),
        "name=mac tool=None\n"
    );
    // Aliases resolve to the platform.
    assert_eq!(
        ws.built(&["--platforms=//p:linux_alias", "//u:u"]),
        "name=linux tool=tool.sh\n"
    );
    // //tc:linux needs a linux execution platform.
    let diagnostic = ws.rejected_raw(&[
        "build",
        "--host_platform=//p:mac",
        "--platforms=//p:linux",
        "//u:u",
    ]);
    assert_eq!(
        diagnostic,
        "bazel-rs: WS/u/BUILD.bazel:2:4: While resolving toolchains for target //u:u (CONFIG): No matching toolchains found for types:\n  //tc:type\nTo debug, rerun with --toolchain_resolution_debug='//tc:type'\nFor more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro.\n"
    );
    // Rules without toolchain types resolve nothing (the default host
    // platform is not needed natively), and the toolchain targets
    // themselves analyze.
    for target in ["//u:g", "//tc:linux", "//tc:type"] {
        let output = ws.run(&["build", target]);
        assert!(output.status.success(), "{target}: {}", errors(&output));
    }
    // A toolchain-using rule needs the default host platform: without the
    // built-in profile it fails explicitly.
    let diagnostic = ws.rejected_raw(&["build", "//u:u"]);
    assert!(
        diagnostic.starts_with("bazel-rs: WS/u/BUILD.bazel:2:4: While resolving toolchains for target //u:u (CONFIG): the target platform @@bazel_tools//tools:host_platform (the default host platform) needs the built-in @bazel_tools repository"),
        "{diagnostic}"
    );
}

#[test]
fn registration_order_follows_registered_toolchains_function() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'dep')\nregister_toolchains('//order/...')\nregister_toolchains('//tc:all')\nregister_toolchains('//dev:all', dev_dependency = True)\n",
        &[
            (
                "dep/MODULE.bazel",
                "module(name = 'dep', version = '1.0')\nregister_toolchains('//:dep_tc')\nregister_toolchains('//:dev_only', dev_dependency = True)\n",
            ),
            (
                "dep/BUILD.bazel",
                r#"load("@@//r:defs.bzl", "impl")
toolchain_type(name = "dep_type", visibility = ["//visibility:public"])
impl(name = "dep_impl", tool_name = "dep")
toolchain(name = "dep_tc", toolchain_type = "@@//tc:other", toolchain = ":dep_impl")
toolchain(name = "dev_only", toolchain_type = ":dep_type", toolchain = ":dep_impl")
"#,
            ),
            // //order/...: subpackages before their parent, sorted.
            (
                "order/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'order')\ntoolchain(name = 'tc', toolchain_type = '//tc:other', toolchain = ':i')\n",
            ),
            (
                "order/b/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'order/b')\ntoolchain(name = 'tc', toolchain_type = '//tc:other', toolchain = ':i')\n",
            ),
            (
                "order/a/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'order/a')\ntoolchain(name = 'tc', toolchain_type = '//tc:other', toolchain = ':i')\n",
            ),
            (
                "order/a/deep/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'order/a/deep')\ntoolchain(name = 'tc', toolchain_type = '//tc:other', toolchain = ':i')\n",
            ),
            // :all lists a package's targets by name.
            (
                "names/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'names')\ntoolchain(name = 'z', toolchain_type = '//tc:other', toolchain = ':i')\nimpl(name = 'j', tool_name = 'names_a')\ntoolchain(name = 'a', toolchain_type = '//tc:other', toolchain = ':j')\nfilegroup(name = 'fg')\n",
            ),
            (
                "dev/BUILD.bazel",
                "load('//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'dev')\ntoolchain(name = 'tc', toolchain_type = '//dev:type', toolchain = ':i')\ntoolchain_type(name = 'type', visibility = ['//visibility:public'])\n",
            ),
            (
                "w/BUILD.bazel",
                "load(':defs.bzl', 'dev')\ndev(name = 'dev')\n",
            ),
            (
                "wd/BUILD.bazel",
                "load('//w:dep.bzl', 'dep')\ndep(name = 'dep')\n",
            ),
            (
                "w/dep.bzl",
                r#"load(":defs.bzl", "show_impl")
dep = rule(implementation = show_impl, toolchains = [config_common.toolchain_type("@dep//:dep_type", mandatory = False)], attrs = {"_type": attr.label(default = "@dep//:dep_type")})
"#,
            ),
            (
                "w/defs.bzl",
                r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    info = ctx.toolchains[ctx.attr._type.label]
    ctx.actions.write(out, "name=%s\n" % (info.name if info else None))
    return [DefaultInfo(files = depset([out]))]

show_impl = _impl
dev = rule(implementation = _impl, toolchains = [config_common.toolchain_type("//dev:type", mandatory = False)], attrs = {"_type": attr.label(default = "//dev:type")})
"#,
            ),
        ],
    );
    // //order/... first: order/a/deep, order/a, order/b, order.
    assert_eq!(ws.built(&["//u:o"]), "name=order/a/deep tool=None\n");
    // --extra_toolchains come first, the last one winning; duplicates keep
    // the last copy (PlatformOptions.getNormalized).
    assert_eq!(
        ws.built(&["--extra_toolchains=//names:all,//order/b:tc", "//u:o"]),
        "name=order/b tool=None\n"
    );
    assert_eq!(
        ws.built(&[
            "--extra_toolchains=//order/b:tc",
            "--extra_toolchains=//names:all",
            "//u:o"
        ]),
        "name=names_a tool=None\n"
    );
    assert_eq!(
        ws.built(&[
            "--extra_toolchains=//names:z,//order/b:tc,//names:z",
            "//u:o"
        ]),
        "name=names tool=None\n"
    );
    // An explicit label is kept whatever its rule class.
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//names:fg", "//u:o"]),
        "bazel-rs: WS/u/BUILD.bazel:3:10: While resolving toolchains for target //u:o (CONFIG): invalid registered toolchain '//names:fg': target does not provide the DeclaredToolchainInfo provider\n"
    );
    // The root's dev registrations count unless --ignore_dev_dependency;
    // a dependency's never do.
    assert_eq!(ws.built(&["//w:dev"]), "name=dev\n");
    assert_eq!(ws.built(&["//wd:dep"]), "name=None\n");
    // --ignore_dev_dependency also drops the root's overrides
    // (ModuleThreadContext.java:373), so it is checked without the module.
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nregister_toolchains('//dev:all', dev_dependency = True)\n",
    );
    assert_eq!(ws.built(&["//w:dev"]), "name=dev\n");
    assert_eq!(
        ws.built(&["--ignore_dev_dependency", "//w:dev"]),
        "name=None\n"
    );
    // The dependency's registrations follow the root's (//order/... wins).
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'dep')\nregister_toolchains('//tc:type')\n",
    );
    assert_eq!(
        ws.rejected(&["//u:o"]),
        "bazel-rs: WS/u/BUILD.bazel:3:10: While resolving toolchains for target //u:o (CONFIG): invalid registered toolchain '//tc:type': target does not provide the DeclaredToolchainInfo provider\n"
    );
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'dep')\n",
    );
    assert_eq!(ws.built(&["//u:o"]), "name=dep tool=None\n");
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'dep')\nregister_toolchains('//tc:other_any')\n",
    );
    assert_eq!(ws.built(&["//u:o"]), "name=other tool=None\n");
    // Negative registration patterns fail explicitly.
    let diagnostic = ws.rejected(&["--extra_toolchains=-//tc:other_any", "//u:o"]);
    assert_eq!(
        diagnostic,
        "bazel-rs: negative toolchain registration pattern \"-//tc:other_any\" is not implemented in the native profile\n"
    );
    // --extra_toolchains is not a query option; --toolchain_resolution_debug
    // stays unsupported.
    let output = ws.run(&["query", "--extra_toolchains=//tc:all", "//u:all"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains(
            "--extra_toolchains=//tc:all :: Unrecognized option: --extra_toolchains=//tc:all"
        ),
        "{}",
        errors(&output)
    );
    let diagnostic = ws.rejected(&["--toolchain_resolution_debug=.*", "//u:o"]);
    assert!(
        diagnostic.contains("unsupported option")
            && diagnostic.contains("--toolchain_resolution_debug"),
        "{diagnostic}"
    );
}

#[test]
fn constraint_matching_and_toolchain_attributes() {
    let ws = Workspace::new(
        "module(name = 'main')\nregister_toolchains('//libc:all')\nregister_toolchains('//tc:all')\n",
        &[
            (
                "libc/BUILD.bazel",
                r#"load("//r:defs.bzl", "impl")
package(default_visibility = ["//visibility:public"])
impl(name = "gnu_impl", tool_name = "gnu")
impl(name = "any_impl", tool_name = "any")
impl(name = "same_impl", tool_name = "same")
toolchain_type(name = "variant")
toolchain_type(name = "same_type")
# A toolchain constrained to a non-default value of a setting with a default:
# a platform that does not set libc has the default (`unconstrained`).
toolchain(name = "a_gnu", toolchain_type = ":variant", target_compatible_with = ["//os:gnu.2.28"], toolchain = ":gnu_impl")
toolchain(name = "b_any", toolchain_type = ":variant", toolchain = ":any_impl")
toolchain(name = "same", toolchain_type = ":same_type", use_target_platform_constraints = True, toolchain = ":same_impl")
"#,
            ),
            (
                "libc/use/BUILD.bazel",
                "load(':defs.bzl', 'variant', 'same')\nvariant(name = 'v')\nsame(name = 's')\n",
            ),
            (
                "libc/use/defs.bzl",
                r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "name=%s\n" % ctx.toolchains[ctx.attr._type.label].name)
    return [DefaultInfo(files = depset([out]))]

variant = rule(implementation = _impl, toolchains = ["//libc:variant"], attrs = {"_type": attr.label(default = "//libc:variant")})
same = rule(implementation = _impl, toolchains = ["//libc:same_type"], attrs = {"_type": attr.label(default = "//libc:same_type")})
"#,
            ),
        ],
    );
    // The platform side matches through the setting's default; the
    // toolchain's own gnu.2.28 rejects it (checkConstraints).
    assert_eq!(ws.built(&["//libc/use:v"]), "name=any\n");
    assert_eq!(
        ws.built(&["--platforms=//p:gnu", "//libc/use:v"]),
        "name=gnu\n"
    );
    // use_target_platform_constraints: the execution platform must have the
    // target platform's constraints.
    assert_eq!(ws.built(&["//libc/use:s"]), "name=same\n");
    assert!(
        ws.rejected(&["--platforms=//p:mac", "//libc/use:s"])
            .contains("No matching toolchains found for types:\n  //libc:same_type\n")
    );
    // Toolchain.create errors, also when the toolchain is unrelated to the
    // requested type: every registered toolchain is analyzed.
    for (attributes, expected) in [
        (
            "use_target_platform_constraints = True, exec_compatible_with = ['//os:linux']",
            "bazel-rs: WS/bad/BUILD.bazel:1:10: in use_target_platform_constraints attribute of toolchain rule //bad:bad: Cannot set use_target_platform_constraints to True and also set exec_compatible_with or target_compatible_with\n",
        ),
        (
            "exec_compatible_with = ['//os:linux', '//os:osx'], target_compatible_with = ['//os:unconstrained', '//os:gnu.2.28']",
            "bazel-rs: WS/bad/BUILD.bazel:1:10: in exec_compatible_with attribute of toolchain rule //bad:bad: Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx]\nWS/bad/BUILD.bazel:1:10: in target_compatible_with attribute of toolchain rule //bad:bad: Duplicate constraint values detected: constraint_setting //os:libc has [//os:unconstrained, //os:gnu.2.28]\n",
        ),
        (
            "target_settings = [':fg']",
            "bazel-rs: WS/bad/BUILD.bazel:1:10: in target_settings attribute of toolchain rule //bad:bad: filegroup rule '//bad:fg' is misplaced here (expected config_setting)\n",
        ),
        (
            "target_settings = ['f.txt']",
            "bazel-rs: WS/bad/BUILD.bazel:1:10: in target_settings attribute of toolchain rule //bad:bad: source file '//bad:f.txt' is misplaced here (expected no files)\n",
        ),
        (
            "toolchain_type = ':fg'",
            "bazel-rs: WS/bad/BUILD.bazel:1:10: in toolchain_type attribute of toolchain rule //bad:bad: filegroup rule '//bad:fg' is misplaced here (expected toolchain_type) and '//bad:fg' does not have mandatory providers: 'ToolchainTypeInfo'\n",
        ),
    ] {
        let toolchain_type = if attributes.starts_with("toolchain_type") {
            String::new()
        } else {
            "toolchain_type = '//tc:other', ".to_owned()
        };
        ws.file(
            "bad/BUILD.bazel",
            &format!(
                "toolchain(name = 'bad', {toolchain_type}{attributes}, toolchain = ':fg')\nfilegroup(name = 'fg')\n"
            ),
        );
        ws.file("bad/f.txt", "");
        assert_eq!(ws.rejected(&["//bad:bad"]), expected, "{attributes}");
        assert_eq!(
            ws.rejected(&["--extra_toolchains=//bad:bad", "//u:u"]),
            expected,
            "{attributes}"
        );
    }
}

#[test]
fn target_settings_select_toolchains_by_configuration() {
    let ws = Workspace::new(
        "module(name = 'main')\nregister_toolchains('//ts:all')\nregister_toolchains('//tc:other_any')\n",
        &[(
            "ts/BUILD.bazel",
            r#"load("//r:defs.bzl", "impl")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
config_setting(name = "never", define_values = {"never": "1"})
# skylib config_setting_group shape: an alias whose actual is a select().
alias(name = "opt_group", actual = select({":opt": ":opt", "//conditions:default": ":never"}))
impl(name = "opt_impl", tool_name = "opt")
impl(name = "group_impl", tool_name = "group")
toolchain(name = "a_opt", toolchain_type = "//tc:other", target_settings = [":opt"], toolchain = ":opt_impl")
toolchain(name = "b_group", toolchain_type = "//tc:type", target_settings = [":opt_group"], toolchain = ":group_impl")
"#,
        )],
    );
    assert_eq!(ws.built(&["//u:o"]), "name=other tool=None\n");
    assert_eq!(ws.built(&["-c", "opt", "//u:o"]), "name=opt tool=None\n");
    assert_eq!(ws.built(&["-c", "opt", "//u:u"]), "name=group tool=None\n");
    assert!(
        ws.rejected(&["//u:u"])
            .contains("No matching toolchains found for types:\n  //tc:type\n")
    );
    assert_eq!(
        ws.built(&["-c", "opt", "--define=never=1", "//u:u"]),
        "name=group tool=None\n"
    );
}

#[test]
fn resolution_errors_follow_bazel() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'm', version = '1.0')\nlocal_path_override(module_name = 'm', path = 'm')\nregister_toolchains('//tc:all')\n",
        &[
            ("m/MODULE.bazel", "module(name = 'm', version = '1.0')\n"),
            (
                "m/BUILD.bazel",
                "toolchain_type(name = 'm_type', visibility = ['//visibility:public'])\n",
            ),
        ],
    );
    // One rule class per list of requested types.
    ws.file(
        "e/defs.bzl",
        r#"def _impl(ctx):
    return []

hint_only = rule(implementation = _impl, toolchains = ["//e:hint"])
two = rule(implementation = _impl, toolchains = ["//e:plain", "//e:hint"])
module_type = rule(implementation = _impl, toolchains = ["@m//:m_type"])
not_a_type = rule(implementation = _impl, toolchains = ["//e:fg"])
toolchain_as_type = rule(implementation = _impl, toolchains = ["//tc:linux"])
missing_type = rule(implementation = _impl, toolchains = ["//e:nope"])
foreign_missing = rule(implementation = _impl, toolchains = ["//tc:nope"])
alias_type = rule(implementation = _impl, toolchains = ["//e:type_alias"])
optional_alias = rule(implementation = _impl, toolchains = [config_common.toolchain_type("//e:hint", mandatory = False)])
"#,
    );
    ws.file(
        "e/BUILD.bazel",
        r#"load(":defs.bzl", "hint_only", "two", "module_type", "not_a_type", "toolchain_as_type", "missing_type", "foreign_missing", "alias_type", "optional_alias")
toolchain_type(name = "hint", no_match_error = "Register a hint toolchain.")
toolchain_type(name = "plain")
filegroup(name = "fg")
alias(name = "type_alias", actual = "//tc:type")
hint_only(name = "hint_only")
two(name = "two")
module_type(name = "module_type")
not_a_type(name = "not_a_type")
toolchain_as_type(name = "toolchain_as_type")
missing_type(name = "missing_type")
foreign_missing(name = "foreign_missing")
alias_type(name = "alias_type")
optional_alias(name = "optional")
"#,
    );
    let footer = "For more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro.\n";
    assert_eq!(
        ws.rejected(&["//e:hint_only"]),
        format!(
            "bazel-rs: WS/e/BUILD.bazel:6:10: While resolving toolchains for target //e:hint_only (CONFIG): No matching toolchains found for types:\n  //e:hint: Register a hint toolchain.\nTo debug, rerun with --toolchain_resolution_debug='//e:hint'\n{footer}"
        )
    );
    // Every missing mandatory type, in request order; the platform's own
    // missing_toolchain_error last (empty: nothing).
    assert_eq!(
        ws.rejected(&["//e:two"]),
        format!(
            "bazel-rs: WS/e/BUILD.bazel:7:4: While resolving toolchains for target //e:two (CONFIG): No matching toolchains found for types:\n  //e:plain\n  //e:hint: Register a hint toolchain.\nTo debug, rerun with --toolchain_resolution_debug='//e:plain|//e:hint'\n{footer}"
        )
    );
    assert!(
        ws.rejected(&["--platforms=//p:quiet", "//e:two"])
            .ends_with("--toolchain_resolution_debug='//e:plain|//e:hint'\n")
    );
    assert!(
        ws.rejected(&["--platforms=//p:custom", "//e:two"])
            .ends_with(
                "--toolchain_resolution_debug='//e:plain|//e:hint'\nInstall the right SDK.\n"
            )
    );
    // Labels with regex characters are quoted (PlatformConfiguration.java:134-142).
    assert!(
        ws.rejected(&["//e:module_type"]).contains(
            "No matching toolchains found for types:\n  @@m+//:m_type\nTo debug, rerun with --toolchain_resolution_debug='\\Q@@m+//:m_type\\E'\n"
        )
    );
    assert_eq!(
        ws.rejected(&["//e:not_a_type"]),
        "bazel-rs: WS/e/BUILD.bazel:9:11: While resolving toolchains for target //e:not_a_type (CONFIG): Target //e:fg was referenced as a toolchain type, but does not provide ToolchainTypeInfo\n"
    );
    assert_eq!(
        ws.rejected(&["//e:toolchain_as_type"]),
        "bazel-rs: WS/e/BUILD.bazel:10:18: While resolving toolchains for target //e:toolchain_as_type (CONFIG): Target //tc:linux was referenced as a toolchain type, but is a toolchain instance. Is the rule definition for the target you're building setting \"toolchains =\" to a toolchain() instead of the expected toolchain_type()?\n"
    );
    assert_eq!(
        ws.rejected(&["//e:missing_type"]),
        "bazel-rs: WS/e/BUILD.bazel: no such target '//e:nope': target 'nope' not declared in package 'e' defined by WS/e/BUILD.bazel\nWS/e/BUILD.bazel:11:13: While resolving toolchains for target //e:missing_type (CONFIG): com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//e:nope': target 'nope' not declared in package 'e' defined by WS/e/BUILD.bazel\n"
    );
    assert_eq!(
        ws.rejected(&["//e:foreign_missing"]),
        "bazel-rs: WS/tc/BUILD.bazel: no such target '//tc:nope': target 'nope' not declared in package 'tc' defined by WS/tc/BUILD.bazel\nWS/e/BUILD.bazel:12:16: While resolving toolchains for target //e:foreign_missing (CONFIG): com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//tc:nope': target 'nope' not declared in package 'tc' defined by WS/tc/BUILD.bazel\n"
    );
    // An alias type resolves to the actual type.
    let output = ws.run(&["build", "--host_platform=//p:linux", "//e:alias_type"]);
    assert!(output.status.success(), "{}", errors(&output));
    // An optional type without a toolchain resolves to nothing.
    let output = ws.run(&["build", "--host_platform=//p:linux", "//e:optional"]);
    assert!(output.status.success(), "{}", errors(&output));
}

#[test]
fn execution_constraints_and_allowed_toolchain_types() {
    let ws = Workspace::new(
        REGISTER_TC,
        &[
            (
                "x/BUILD.bazel",
                "load(':defs.bzl', 'on_osx', 'on_osx_typed', 'on_linux')\non_osx(name = 'osx')\non_osx_typed(name = 'osx_typed')\non_linux(name = 'linux')\n",
            ),
            (
                "x/defs.bzl",
                r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "ok\n")
    return [DefaultInfo(files = depset([out]))]

on_osx = rule(implementation = _impl, exec_compatible_with = ["//os:osx"])
on_osx_typed = rule(implementation = _impl, exec_compatible_with = ["//os:osx"], toolchains = ["//tc:type"])
on_linux = rule(implementation = _impl, exec_compatible_with = ["//os:linux"])
"#,
            ),
        ],
    );
    assert_eq!(ws.built(&["//x:linux"]), "ok\n");
    // rule(exec_compatible_with) filters the host platform out
    // (PlatformKeys.filterExecutionPlatforms; NoMatchingPlatformData.formatError).
    assert_eq!(
        ws.rejected(&["//x:osx"]),
        "bazel-rs: WS/x/BUILD.bazel:2:7: While resolving toolchains for target //x:osx (CONFIG): Unable to find an execution platform for target platform //p:linux from available execution platforms []\n"
    );
    assert_eq!(
        ws.rejected(&["//x:osx_typed"]),
        "bazel-rs: WS/x/BUILD.bazel:3:13: While resolving toolchains for target //x:osx_typed (CONFIG): No matching toolchains found for types:\n  //tc:type\nTo debug, rerun with --toolchain_resolution_debug='//tc:type'\nFor more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro.\n"
    );
    // A host platform with check_toolchain_types is no execution platform
    // for a rule without toolchain types. (Its allowed_toolchain_types are
    // covered by the unit tests of src/toolchains.rs: with genuine Bazel the
    // built-in rules_java toolchains' target_settings already fail on such a
    // host, paired capture A6/execution_constraints/strict_u, so a typed
    // build cannot reproduce Bazel here.)
    assert_eq!(
        ws.rejected_raw(&["build", "--host_platform=//p:strict", "//u:g"]),
        "bazel-rs: WS/u/BUILD.bazel:4:8: While resolving toolchains for target //u:g (CONFIG): Unable to find an execution platform for target platform //p:strict from available execution platforms [//p:strict]\n"
    );
    // The target's exec_compatible_with filters too; ctx.attr exposes its
    // constraint values.
    ws.file(
        "y/BUILD.bazel",
        "load(':defs.bzl', 'show_exec')\nshow_exec(name = 'linux', exec_compatible_with = ['//os:linux'])\nshow_exec(name = 'osx', exec_compatible_with = ['//os:osx'])\nshow_exec(name = 'not_constraint', exec_compatible_with = [':fg'])\nfilegroup(name = 'fg')\n",
    );
    ws.file(
        "y/defs.bzl",
        r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    values = ctx.attr.exec_compatible_with
    ctx.actions.write(out, "%s %r\n" % (values[0].label, platform_common.ConstraintValueInfo in values[0]))
    return [DefaultInfo(files = depset([out]))]

show_exec = rule(implementation = _impl)
"#,
    );
    assert_eq!(ws.built(&["//y:linux"]), "@@//os:linux True\n");
    assert_eq!(
        ws.rejected(&["//y:osx"]),
        "bazel-rs: WS/y/BUILD.bazel:3:10: While resolving toolchains for target //y:osx (CONFIG): Unable to find an execution platform for target platform //p:linux from available execution platforms []\n"
    );
    assert_eq!(
        ws.rejected(&["//y:not_constraint"]),
        "bazel-rs: WS/y/BUILD.bazel:4:10: While resolving toolchains for target //y:not_constraint (CONFIG): Target //y:fg was referenced as a constraint_value, but does not provide ConstraintValueInfo\n"
    );
    // A missing or non-constraint label in rule(exec_compatible_with).
    ws.file(
        "x/defs.bzl",
        r#"def _impl(ctx):
    return []

on_fg = rule(implementation = _impl, exec_compatible_with = ["//x:fg"])
"#,
    );
    ws.file(
        "x/BUILD.bazel",
        "load(':defs.bzl', 'on_fg')\non_fg(name = 'fg_rule')\nfilegroup(name = 'fg')\n",
    );
    assert_eq!(
        ws.rejected(&["//x:fg_rule"]),
        "bazel-rs: WS/x/BUILD.bazel:2:6: While resolving toolchains for target //x:fg_rule (CONFIG): Target //x:fg was referenced as a constraint_value, but does not provide ConstraintValueInfo\n"
    );
}

#[test]
fn starlark_toolchain_context() {
    let ws = Workspace::new(
        REGISTER_TC,
        &[
            (
                "s/BUILD.bazel",
                r#"load(":defs.bzl", "probe", "setting", "bad_key", "undeclared", "no_info", "broken_impl", "auto")
load("//r:defs.bzl", "noinfo", "broken")
alias(name = "type_alias", actual = "//tc:type")
alias(name = "impl_alias", actual = "//tc:linux_impl")
noinfo(name = "noinfo_impl")
broken(name = "broken_impl_target")
toolchain_type(name = "noinfo_type")
toolchain_type(name = "broken_type")
toolchain_type(name = "alias_impl_type")
toolchain(name = "noinfo_tc", toolchain_type = ":noinfo_type", toolchain = ":noinfo_impl")
toolchain(name = "broken_tc", toolchain_type = ":broken_type", toolchain = ":broken_impl_target")
toolchain(name = "alias_tc", toolchain_type = ":alias_impl_type", toolchain = ":impl_alias")
probe(name = "probe")
setting(name = "setting", build_setting_default = "")
bad_key(name = "bad_key")
undeclared(name = "undeclared")
no_info(name = "no_info")
broken_impl(name = "broken")
auto(name = "auto")
"#,
            ),
            (
                "s/defs.bzl",
                r#"def _probe_impl(ctx):
    t = ctx.toolchains
    by_label = t[Label("//tc:type")]
    by_string = t["//tc:type"]
    relative = t[":type_alias"]
    lines = [
        "type=" + type(t),
        "repr=" + repr(t),
        "label=" + by_label.name,
        "same=%r" % (by_label == by_string and by_string == relative),
        "info_type=" + type(by_label),
        "info_dir=%r" % dir(by_label),
        "tool=" + by_label.tool.basename,
        "optional=%r" % t["//tc:other_type_missing"],
        "optional_in=%r" % ("//tc:other_type_missing" in t),
        "actual_in=%r" % ("//tc:type" in t),
        "undeclared_in=%r" % ("//tc:other" in t),
        "alias_impl=" + t["//s:alias_impl_type"].name,
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    toolchains = [
        "//s:type_alias",
        config_common.toolchain_type("//tc:other_type_missing", mandatory = False),
        "//s:alias_impl_type",
    ],
)

def _setting_impl(ctx):
    ctx.toolchains["//tc:type"]

setting = rule(implementation = _setting_impl, build_setting = config.string(flag = True))

def _bad_key_impl(ctx):
    ctx.toolchains[1]

bad_key = rule(implementation = _bad_key_impl, toolchains = ["//tc:type"])

def _undeclared_impl(ctx):
    ctx.toolchains["//tc:other"]

undeclared = rule(implementation = _undeclared_impl, toolchains = ["//tc:type"])

def _empty(ctx):
    return []

no_info = rule(implementation = _empty, toolchains = ["//s:noinfo_type"])
broken_impl = rule(implementation = _empty, toolchains = ["//s:broken_type"])
auto = rule(implementation = _empty, toolchains = ["//tc:type"], attrs = {"_use_auto_exec_groups": attr.bool(default = True)})
"#,
            ),
            ("tc/missing/BUILD.bazel", ""),
        ],
    );
    ws.file(
        "tc/BUILD.bazel",
        &(BASE
            .iter()
            .find(|(path, _)| *path == "tc/BUILD.bazel")
            .unwrap()
            .1
            .to_owned()
            + "toolchain_type(name = 'other_type_missing')\n"),
    );
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nregister_toolchains('//tc:all')\nregister_toolchains('//s:all')\n",
    );
    assert_eq!(
        ws.built(&["//s:probe"]),
        "type=ToolchainContext\nrepr=<toolchain_context.resolved_labels: //s:type_alias, //tc:type, //tc:other_type_missing, //s:alias_impl_type>\nlabel=linux\nsame=True\ninfo_type=ToolchainInfo\ninfo_dir=[\"name\", \"tool\"]\ntool=tool.sh\noptional=None\noptional_in=True\nactual_in=True\nundeclared_in=False\nalias_impl=linux\n"
    );
    // Rules without toolchain resolution (build settings).
    assert!(
        ws.rejected(&["//s:setting"])
            .contains("Toolchains are not valid in this context"),
    );
    assert!(
        ws.rejected(&["//s:bad_key"])
            .contains("Toolchains only supports indexing by toolchain type, got int instead")
    );
    assert!(
        ws.rejected(&["//s:undeclared"]).contains(
            "In undeclared rule //s:undeclared, toolchain type //tc:other was requested but only types [//tc:type] are configured"
        )
    );
    // ResolvedToolchainContext.load: the implementation must provide
    // ToolchainInfo; its analysis errors are its own.
    assert_eq!(
        ws.rejected(&["//s:no_info"]),
        "bazel-rs: WS/s/BUILD.bazel:17:8: While resolving toolchains for target //s:no_info (CONFIG): toolchain type //s:noinfo_type resolved to target //s:noinfo_impl, but that target does not provide ToolchainInfo\n"
    );
    assert!(
        ws.rejected(&["//s:broken"])
            .contains("broken implementation"),
    );
    // Automatic exec groups resolve natively (docs/EXEC_GROUPS.md).
    let output = ws.run(&["build", "--host_platform=//p:linux", "//s:auto"]);
    assert!(output.status.success(), "{}", errors(&output));
}

#[test]
fn whole_repository_registration_patterns() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'dep', version = '1.0')\nlocal_path_override(module_name = 'dep', path = 'third_party/dep')\nregister_toolchains('//...')\nregister_toolchains('@dep//...')\nregister_toolchains('//third_party/...')\n",
        &[
            (".bazelignore", "third_party\n"),
            (
                "third_party/dep/MODULE.bazel",
                "module(name = 'dep', version = '1.0')\n",
            ),
            (
                "third_party/dep/BUILD.bazel",
                "load('@@//r:defs.bzl', 'impl')\ntoolchain_type(name = 'dep_type', visibility = ['//visibility:public'])\nimpl(name = 'i', tool_name = 'dep')\ntoolchain(name = 'tc', toolchain_type = ':dep_type', toolchain = ':i')\n",
            ),
            (
                "third_party/dep/sub/BUILD.bazel",
                "load('@@//r:defs.bzl', 'impl')\nimpl(name = 'i', tool_name = 'dep/sub')\ntoolchain(name = 'tc', toolchain_type = '//:dep_type', toolchain = ':i')\n",
            ),
            (
                "w/BUILD.bazel",
                "load(':defs.bzl', 'dep')\ndep(name = 'dep')\n",
            ),
            (
                "w/defs.bzl",
                r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "name=%s\n" % ctx.toolchains["@dep//:dep_type"].name)
    return [DefaultInfo(files = depset([out]))]

dep = rule(implementation = _impl, toolchains = ["@dep//:dep_type"])
"#,
            ),
        ],
    );
    // `//...` and `@dep//...` (TargetPattern.TargetsBelowDirectory): the
    // whole repositories, subpackages first; a pattern below an ignored
    // directory warns and selects nothing.
    let output = ws.run(&["build", "--host_platform=//p:linux", "//w:dep"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains(
            "WARNING: Pattern '//third_party/...' was filtered out by ignored directory 'third_party'\n"
        ),
        "{}",
        errors(&output)
    );
    assert_eq!(ws.built(&["//w:dep"]), "name=dep/sub\n");
    assert_eq!(ws.built(&["//u:u"]), "name=linux tool=tool.sh\n");
}

#[test]
fn registration_pattern_errors_and_toolchain_cycles() {
    let ws = Workspace::new(
        REGISTER_TC,
        &[
            (
                "cyc/BUILD.bazel",
                r#"load("//r:defs.bzl", "use", "impl")
use(name = "user")
impl(name = "i", tool_name = "cyc")
toolchain(name = "type_is_user", toolchain_type = ":user", toolchain = ":i")
toolchain(name = "exec_user", toolchain_type = "//tc:other", exec_compatible_with = [":user"], toolchain = ":i")
"#,
            ),
            (
                "ur/BUILD.bazel",
                "load(':defs.bzl', 'unknown_repo')\nunknown_repo(name = 'unknown_repo')\n",
            ),
            (
                "ur/defs.bzl",
                "def _impl(ctx):\n    return []\n\nunknown_repo = rule(implementation = _impl, toolchains = [\"@nope//:t\"])\n",
            ),
        ],
    );
    // Bazel 9.3 crashes on `:*` and `:all-targets` registrations
    // (FilteringPolicies.java:120): an explicit failure natively.
    for pattern in ["//tc:*", "//tc:all-targets"] {
        assert_eq!(
            ws.rejected(&[&format!("--extra_toolchains={pattern}"), "//u:o"]),
            format!(
                "bazel-rs: toolchain registration pattern '{pattern}' (the :{} wildcard) is not implemented in the native profile: Bazel 9.3 crashes expanding it (NullPointerException in FilteringPolicies.RuleTypeFilter.shouldRetain, pkgcache/FilteringPolicies.java:120)\n",
                pattern.rsplit_once(':').unwrap().1
            )
        );
    }
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//nothere/...", "//u:o"]),
        "bazel-rs: WS/u/BUILD.bazel:3:10: While resolving toolchains for target //u:o (CONFIG): invalid registered toolchain '//nothere/...': no targets found beneath 'nothere'\n"
    );
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//nothere:all", "//u:o"]),
        "bazel-rs: WS/u/BUILD.bazel:3:10: While resolving toolchains for target //u:o (CONFIG): invalid registered toolchain '//nothere:all': while parsing '//nothere:all': no such package 'nothere': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - nothere\n"
    );
    assert_eq!(
        ws.rejected(&["--extra_toolchains=@nope//:x", "//u:o"]),
        "bazel-rs: Analysis of target '//u:o' failed; build aborted: No repository visible as '@nope' from main repository\n"
    );
    // A registered target that needs toolchain resolution itself
    // (RegisteredToolchainsCycleReporter); of several, Bazel names the last.
    let cycle = |head: &str, path: &str| {
        format!(
            "bazel-rs: Misconfigured toolchains: {head} is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains\n{path}|   toolchain types //tc:type\n|   toolchain type //tc:type\n`-- RegisteredToolchains\n"
        )
    };
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//cyc:user", "//u:o"]),
        cycle("//cyc:user", "|   //cyc:user\n")
    );
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//cyc:exec_user", "//u:o"]),
        cycle("//cyc:exec_user", "|   //cyc:exec_user\n|   //cyc:user\n")
    );
    assert_eq!(
        ws.rejected(&["--extra_toolchains=//cyc:all", "//u:o"]),
        cycle(
            "//cyc:type_is_user",
            "|   //cyc:type_is_user\n|   //cyc:user\n"
        )
    );
    // An unknown apparent repository in rule(toolchains) fails only at
    // resolution (Label.parseWithRepoContext); the rule still loads.
    let query = ws.run(&["query", "//ur:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    let message = "no such package '@@[unknown repo 'nope' requested from @@]//': The repository '@@[unknown repo 'nope' requested from @@]' could not be resolved: No repository visible as '@nope' from main repository";
    assert_eq!(
        ws.rejected(&["//ur:unknown_repo"]),
        format!(
            "bazel-rs: {message}\nWS/ur/BUILD.bazel:2:13: While resolving toolchains for target //ur:unknown_repo (CONFIG): com.google.devtools.build.lib.packages.BuildFileNotFoundException: {message}\n"
        )
    );
}

#[test]
fn string_keys_use_the_calling_bzl_and_starlark_api_details() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'm', version = '1.0', repo_name = 'mm')\nlocal_path_override(module_name = 'm', path = 'm')\nregister_toolchains('//tc:all')\nregister_toolchains('//s2:all')\n",
        &[
            ("m/MODULE.bazel", "module(name = 'm', version = '1.0')\n"),
            (
                "m/BUILD.bazel",
                "toolchain_type(name = 'm_type', visibility = ['//visibility:public'])\n",
            ),
            (
                "m/helper.bzl",
                r#"def by_relative_string(ctx):
    return ctx.toolchains["//:m_type"].name

def by_main_apparent(ctx):
    return ctx.toolchains["@mm//:m_type"].name

def key():
    return "//:m_type"
"#,
            ),
            (
                "s2/defs.bzl",
                r#"load("@mm//:helper.bzl", "by_relative_string", "by_main_apparent", "key")

def _w(ctx, text):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "name=%s\n" % text)
    return [DefaultInfo(files = depset([out]))]

def _rel(ctx):
    return _w(ctx, by_relative_string(ctx))

def _app(ctx):
    return _w(ctx, by_main_apparent(ctx))

def _main_relative(ctx):
    return _w(ctx, ctx.toolchains["//:m_type"].name)

def _key_from_other_file(ctx):
    # The key comes from helper.bzl, the access is in this file.
    return _w(ctx, "@mm//:m_type" in ctx.toolchains and ctx.toolchains["@mm" + key()].name)

def _api(ctx):
    t = ctx.toolchains
    return _w(ctx, "%r %r %r" % (dir(t), [str(x) for x in t.toolchain_types()], type(t.toolchain_types())))

T = ["@mm//:m_type"]
via_relative_string = rule(implementation = _rel, toolchains = T)
via_main_apparent = rule(implementation = _app, toolchains = T)
main_relative = rule(implementation = _main_relative, toolchains = T)
key_from_other_file = rule(implementation = _key_from_other_file, toolchains = T)
api = rule(implementation = _api, toolchains = T)

def _ti_pos(ctx):
    platform_common.ToolchainInfo(1)

ti_pos = rule(implementation = _ti_pos)

def _dep(ctx):
    return []

dep_required = rule(implementation = _dep, attrs = {"dep": attr.label(providers = [platform_common.ToolchainInfo])})

def _bad_string(ctx):
    ctx.toolchains["//tc:type:bad:"]

bad_string = rule(implementation = _bad_string, toolchains = ["//tc:type"])
"#,
            ),
            (
                "s2/BUILD.bazel",
                r#"load(":defs.bzl", "via_relative_string", "via_main_apparent", "main_relative", "key_from_other_file", "api", "ti_pos", "dep_required", "bad_string")
load("//r:defs.bzl", "impl")
via_relative_string(name = "rel")
via_main_apparent(name = "app")
main_relative(name = "main_relative")
key_from_other_file(name = "key")
api(name = "api")
ti_pos(name = "ti_pos")
dep_required(name = "dep_required_fg", dep = ":fg")
bad_string(name = "bad_string")
filegroup(name = "fg")
impl(name = "m_impl", tool_name = "m")
toolchain(name = "m_tc", toolchain_type = "@mm//:m_type", toolchain = ":m_impl")
"#,
            ),
        ],
    );
    // String keys use the repository mapping of the innermost calling .bzl
    // (LabelConverter.forBzlEvaluatingThread): in @m's helper.bzl, `//`
    // is @m's repository and `@mm` is not visible.
    assert_eq!(ws.built(&["//s2:rel"]), "name=m\n");
    assert!(
        ws.rejected(&["//s2:app"]).contains(
            "In via_main_apparent rule //s2:app, toolchain type @@[unknown repo 'mm' requested from @@m+]//:m_type was requested but only types [@@m+//:m_type] are configured"
        )
    );
    assert!(
        ws.rejected(&["//s2:main_relative"]).contains(
            "In main_relative rule //s2:main_relative, toolchain type //:m_type was requested but only types [@@m+//:m_type] are configured"
        )
    );
    assert_eq!(ws.built(&["//s2:key"]), "name=m\n");
    assert_eq!(
        ws.built(&["//s2:api"]),
        "name=[\"toolchain_types\"] [\"@@m+//:m_type\"] \"list\"\n"
    );
    assert!(
        ws.rejected(&["//s2:ti_pos"])
            .contains("ToolchainInfo() got unexpected positional argument")
    );
    assert!(
        ws.rejected(&["//s2:dep_required_fg"]).contains(
            "in dep attribute of dep_required rule //s2:dep_required_fg: '//s2:fg' does not have mandatory providers: 'ToolchainInfo'"
        )
    );
    assert!(
        ws.rejected(&["//s2:bad_string"]).contains(
            "Unable to parse toolchain label '//tc:type:bad:': invalid target name 'type:bad:': target names may not contain ':'"
        )
    );
}

#[test]
fn escaped_toolchain_contexts_and_many_string_keys() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'm', version = '1.0')\nlocal_path_override(module_name = 'm', path = 'm')\nregister_toolchains('//tc:all')\n",
        &[
            ("m/MODULE.bazel", "module(name = 'm', version = '1.0')\n"),
            ("m/BUILD.bazel", ""),
            (
                "m/consume.bzl",
                r#"load("@@//esc:providers.bzl", "Escaped")

# "//tc:type" in @m's .bzl names @@m+//tc:type.
def _foreign(ctx):
    t = ctx.attr.dep[Escaped].toolchains
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "in=%r\n" % ("//tc:type" in t))
    return [DefaultInfo(files = depset([out]))]

foreign = rule(implementation = _foreign, attrs = {"dep": attr.label()})

def _foreign_index(ctx):
    ctx.attr.dep[Escaped].toolchains["//tc:type"]

foreign_index = rule(implementation = _foreign_index, attrs = {"dep": attr.label()})
"#,
            ),
            (
                "esc/providers.bzl",
                "Escaped = provider(fields = ['toolchains'])\n",
            ),
            (
                "esc/defs.bzl",
                r#"load(":providers.bzl", "Escaped")

def _producer(ctx):
    return [Escaped(toolchains = ctx.toolchains)]

producer = rule(implementation = _producer, toolchains = ["//tc:type"])

def _consumer(ctx):
    t = ctx.attr.dep[Escaped].toolchains
    info = t["//tc:type"]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [info.tool],
        command = "echo %s %s %r > %s" % (info.name, info.tool.basename, ":type" in t, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"dep": attr.label()})

def _many(ctx):
    names = [ctx.toolchains["//tc:type"].name for _ in range(300)]
    found = len([1 for _ in range(300) if "//tc:type" in ctx.toolchains])
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%d %d %s\n" % (len(names), found, names[-1]))
    return [DefaultInfo(files = depset([out]))]

many = rule(implementation = _many, toolchains = ["//tc:type"])
"#,
            ),
            (
                "esc/BUILD.bazel",
                r#"load(":defs.bzl", "producer", "consumer", "many")
load("@m//:consume.bzl", "foreign", "foreign_index")
producer(name = "p")
consumer(name = "c", dep = ":p")
foreign(name = "f", dep = ":p")
foreign_index(name = "fi", dep = ":p")
many(name = "many")
"#,
            ),
        ],
    );
    // A ctx.toolchains value stored in a provider is indexed downstream with
    // the downstream caller's .bzl: in main's esc/defs.bzl "//tc:type" is the
    // producer's type (and ":type" is //esc:type, not requested); in @m's
    // consume.bzl it is @@m+//tc:type.
    assert_eq!(ws.built(&["//esc:c"]), "linux tool.sh False\n");
    assert_eq!(ws.built(&["//esc:f"]), "in=False\n");
    assert!(
        ws.rejected(&["//esc:fi"]).contains(
            "In producer rule //esc:p, toolchain type @@m+//tc:type was requested but only types [//tc:type] are configured"
        )
    );
    // Many string-keyed accesses in one implementation.
    assert_eq!(ws.built(&["//esc:many"]), "300 300 linux\n");
}
