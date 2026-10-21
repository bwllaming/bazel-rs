//! CLI coverage for N18b, the exec transition (docs/CONFIGURATION.md "Exec
//! transition"): Starlark `cfg = "exec"`/`"host"`, genrule `tools`, the exec
//! configuration's options (builtin_exec_platforms.bzl over defaults,
//! ExecutionTransitionFactory's finalizer), its output root, select() and
//! toolchain resolution in it, Starlark flag scopes, no-op transitions of
//! parents without an execution platform and exec-group errors.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(module: &str, files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n18b-")
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
        output
    }

    /// `LABEL\tPATH-BELOW-THE-ACTION-OUTPUTS` lines and the output contents of
    /// `build ARGS`.
    fn built(&self, args: &[&str]) -> (Vec<String>, String) {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let mut lines = Vec::new();
        let mut contents = String::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (label, path) = line.split_once('\t').unwrap();
            contents.push_str(&fs::read_to_string(path).unwrap());
            let relative = path
                .split_once("/outputs/")
                .map_or(path, |(_, relative)| relative);
            lines.push(format!("{label}\t{relative}"));
        }
        (lines, contents)
    }

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
        errors(&output).replace(&self.path().display().to_string(), "WS")
    }
}

fn errors(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // The default memory-cap announcement (crate::memory_limit) is a
    // startup notice, not part of the diagnostics compared here.

    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr
            + "
"
    }
}

const LINUX: &str = "--host_platform=//p:linux";

const BASE: &[(&str, &str)] = &[
    (
        "os/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "osx", constraint_setting = ":os")
constraint_value(name = "windows", constraint_setting = ":os")
"#,
    ),
    (
        "p/BUILD.bazel",
        r#"package(default_visibility = ["//visibility:public"])
platform(name = "linux", constraint_values = ["//os:linux"])
platform(name = "mac", constraint_values = ["//os:osx"])
alias(name = "linux_alias", actual = ":linux")
"#,
    ),
    (
        "f/BUILD.bazel",
        r#"load("//r:defs.bzl", "string_flag")
package(default_visibility = ["//visibility:public"])
string_flag(name = "universal", build_setting_default = "u-default")
string_flag(name = "explicit_universal", build_setting_default = "e-default", scope = "universal")
string_flag(name = "target", build_setting_default = "t-default", scope = "TARGET")
string_flag(name = "project", build_setting_default = "p-default", scope = "project")
string_flag(name = "bogus", build_setting_default = "b-default", scope = "everywhere")
"#,
    ),
    ("r/BUILD.bazel", ""),
    (
        "r/defs.bzl",
        r#"FlagValue = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagValue(value = ctx.build_setting_value)]

string_flag = rule(
    implementation = _flag_impl,
    build_setting = config.string(flag = True),
    attrs = {"scope": attr.string(default = "universal")},
)

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    flags = ",".join([f[FlagValue].value for f in ctx.attr.flags])
    ctx.actions.write(out, "%s: mode=%s stamp=%s define=%s os=%s flags=%s at=%s\n" % (
        ctx.label.name, ctx.attr.mode, ctx.attr.stamp, ctx.attr.define, ctx.attr.os, flags, out.path))
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {
    "mode": attr.string(),
    "stamp": attr.string(),
    "define": attr.string(),
    "os": attr.string(),
    "flags": attr.label_list(providers = [FlagValue]),
})

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.file.tool] + ctx.files.dep,
        command = "(cat %s; echo %s: tool=%s) > %s" % (" ".join([f.path for f in [ctx.file.tool] + ctx.files.dep]), ctx.label.name, ctx.file.tool.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use_impl, attrs = {
    "tool": attr.label(cfg = "exec", allow_single_file = True),
    "dep": attr.label(allow_files = True),
})

host_use = rule(implementation = _use_impl, attrs = {
    "tool": attr.label(cfg = "host", allow_single_file = True),
    "dep": attr.label(allow_files = True),
})

exec_use = rule(implementation = _use_impl, attrs = {
    "tool": attr.label(cfg = config.exec(), allow_single_file = True),
    "dep": attr.label(allow_files = True),
})

group_use = rule(implementation = _use_impl, attrs = {
    "tool": attr.label(cfg = config.exec("g"), allow_single_file = True),
    "dep": attr.label(allow_files = True),
})

def _setting_impl(ctx):
    return [DefaultInfo(files = depset([ctx.file.tool]))]

setting_with_tool = rule(
    implementation = _setting_impl,
    build_setting = config.string(flag = True),
    attrs = {"tool": attr.label(cfg = "exec", allow_single_file = True)},
)

def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.attr.tool_name), DefaultInfo()]

impl = rule(implementation = _impl_impl, attrs = {"tool_name": attr.string()})

def _tc_use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s: toolchain=%s at=%s\n" % (ctx.label.name, ctx.toolchains["//tc:type"].name, out.path))
    return [DefaultInfo(files = depset([out]))]

tc_use = rule(implementation = _tc_use_impl, toolchains = ["//tc:type"])
"#,
    ),
    (
        "tc/BUILD.bazel",
        r#"load("//r:defs.bzl", "impl")
package(default_visibility = ["//visibility:public"])
toolchain_type(name = "type")
impl(name = "linux_impl", tool_name = "linux")
impl(name = "linux_extra_impl", tool_name = "linux_extra")
impl(name = "mac_impl", tool_name = "mac")
toolchain(name = "linux", toolchain_type = ":type", target_compatible_with = ["//os:linux"], toolchain = ":linux_impl")
toolchain(name = "mac", toolchain_type = ":type", target_compatible_with = ["//os:osx"], toolchain = ":mac_impl")
toolchain(name = "linux_extra", toolchain_type = ":type", target_compatible_with = ["//os:linux"], toolchain = ":linux_extra_impl")
"#,
    ),
    (
        "BUILD.bazel",
        r#"load("//r:defs.bzl", "exec_use", "group_use", "host_use", "setting_with_tool", "show", "tc_use", "use")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
config_setting(name = "stamped", values = {"stamp": "1"})
config_setting(name = "x1", define_values = {"x": "1"})
config_setting(name = "host_dbg", values = {"host_compilation_mode": "dbg"})
show(
    name = "m",
    mode = select({":opt": "opt", ":dbg": "dbg", "//conditions:default": "fastbuild"}),
    stamp = select({":stamped": "1", "//conditions:default": "0"}),
    define = select({":x1": "x1", "//conditions:default": "none"}),
    os = select({"//os:linux": "linux", "//os:osx": "osx", "//conditions:default": "other"}),
    flags = ["//f:universal", "//f:explicit_universal", "//f:target"],
)
use(name = "u", tool = ":m", dep = ":m")
host_use(name = "h", tool = ":m")
exec_use(name = "e", tool = ":m")
use(name = "inner", tool = ":m")
use(name = "outer", tool = ":inner")
group_use(name = "group", tool = ":m")
genrule(name = "g", srcs = ["in.txt"], tools = [":m"], outs = ["g.txt"], cmd = "(cat $(SRCS); echo tool=$(location :m); cat $(location :m)) > $@")
setting_with_tool(name = "setting", build_setting_default = "", tool = ":m")
tc_use(name = "t")
use(name = "tu", tool = ":t", dep = ":t")
use(name = "cycle_a", tool = ":cycle_b")
use(name = "cycle_b", tool = ":cycle_a")
show(name = "hm", mode = select({":host_dbg": "host-dbg", "//conditions:default": "host-opt"}))
"#,
    ),
    ("in.txt", "in\n"),
];

const MODULE: &str = "module(name = 'main')\nregister_toolchains('//tc:linux', '//tc:mac')\n";

fn line(
    name: &str,
    mode: &str,
    stamp: &str,
    define: &str,
    os: &str,
    flags: &str,
    root: &str,
) -> String {
    format!(
        "{name}: mode={mode} stamp={stamp} define={define} os={os} flags={flags} at=bazel-out/{root}/bin/{name}.txt\n"
    )
}

const DEFAULT_FLAGS: &str = "u-default,e-default,t-default";

#[test]
fn genrule_tools_and_exec_dependencies_use_the_exec_configuration() {
    let ws = Workspace::new(MODULE, &[]);
    let exec = line(
        "m",
        "opt",
        "0",
        "none",
        "linux",
        DEFAULT_FLAGS,
        "linux-opt-exec",
    );
    // genrule tools: action inputs and $(location), built in the exec
    // configuration (linux-opt-exec: --host_compilation_mode defaults to opt),
    // never in $(SRCS); -c dbg changes only the target configuration.
    for (args, root) in [
        (&[LINUX][..], "k8-fastbuild"),
        (&[LINUX, "-c", "dbg"][..], "k8-dbg"),
    ] {
        let mut all = args.to_vec();
        all.push("//:g");
        let (lines, contents) = ws.built(&all);
        assert_eq!(lines, [format!("//:g\tbazel-out/{root}/bin/g.txt")]);
        assert_eq!(
            contents,
            format!("in\ntool=bazel-out/linux-opt-exec/bin/m.txt\n{exec}")
        );
    }
    // The same target in the target and the exec configuration, side by side.
    let (lines, contents) = ws.built(&[LINUX, "//:u"]);
    assert_eq!(lines, ["//:u\tbazel-out/k8-fastbuild/bin/u.txt"]);
    assert_eq!(
        contents,
        format!(
            "{exec}{}u: tool=bazel-out/linux-opt-exec/bin/m.txt\n",
            line(
                "m",
                "fastbuild",
                "0",
                "none",
                "linux",
                DEFAULT_FLAGS,
                "k8-fastbuild"
            )
        )
    );
    // cfg = "host" (StarlarkAttrModule.java:521-531) and config.exec() are
    // the exec transition too.
    for name in ["h", "e"] {
        let (_, contents) = ws.built(&[LINUX, &format!("//:{name}")]);
        assert_eq!(
            contents,
            format!("{exec}{name}: tool=bazel-out/linux-opt-exec/bin/m.txt\n")
        );
    }
    // Exec of exec is the same configuration (the same root).
    let (_, contents) = ws.built(&[LINUX, "//:outer"]);
    assert_eq!(
        contents,
        format!(
            "{exec}inner: tool=bazel-out/linux-opt-exec/bin/m.txt\nouter: tool=bazel-out/linux-opt-exec/bin/inner.txt\n"
        )
    );
}

#[test]
fn exec_options_follow_bazels_exec_transition() {
    let ws = Workspace::new(MODULE, &[]);
    // compilation_mode = --host_compilation_mode, stamp = false, --define
    // carried, the target platform is the execution platform (the host
    // platform, also under --platforms=//p:mac).
    let (_, contents) = ws.built(&[
        LINUX,
        "--host_compilation_mode=dbg",
        "--stamp",
        "--define=x=1",
        "--platforms=//p:mac",
        "//:u",
    ]);
    assert_eq!(
        contents,
        format!(
            "{}{}u: tool=bazel-out/linux-dbg-exec/bin/m.txt\n",
            line(
                "m",
                "dbg",
                "0",
                "x1",
                "linux",
                DEFAULT_FLAGS,
                "linux-dbg-exec"
            ),
            line(
                "m",
                "fastbuild",
                "1",
                "x1",
                "osx",
                DEFAULT_FLAGS,
                "k8-fastbuild"
            ),
        )
    );
    // --host_compilation_mode is selectable like --compilation_mode and
    // validated by the same converter (CoreOptions.java:406-417).
    let output = ws.run(&["build", LINUX, "--host_compilation_mode=fast", "//:u"]);
    assert_eq!(
        errors(&output),
        "bazel-rs: While parsing option --host_compilation_mode=fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)\n"
    );
    let (_, contents) = ws.built(&["--host_compilation_mode=dbg", "//:hm"]);
    assert_eq!(
        contents,
        "hm: mode=host-dbg stamp= define= os= flags= at=bazel-out/k8-fastbuild/bin/hm.txt\n"
    );
    let (_, contents) = ws.built(&["//:hm"]);
    assert!(contents.starts_with("hm: mode=host-opt "), "{contents}");
    // Starlark flags are carried unless their rule explicitly sets
    // scope = "target" (case-insensitive); an omitted scope is the default
    // scope, which propagates, whatever the attribute default
    // (FunctionTransitionUtil.java:231-251, BuildOptionsScopeFunction.java:180-190).
    let (_, contents) = ws.built(&[
        LINUX,
        "--//f:universal=u",
        "--//f:explicit_universal=e",
        "--//f:target=t",
        "//:u",
    ]);
    assert_eq!(
        contents,
        format!(
            "{}{}u: tool=bazel-out/linux-opt-exec/bin/m.txt\n",
            line(
                "m",
                "opt",
                "0",
                "none",
                "linux",
                "u,e,t-default",
                "linux-opt-exec"
            ),
            line(
                "m",
                "fastbuild",
                "0",
                "none",
                "linux",
                "u,e,t",
                "k8-fastbuild"
            ),
        )
    );
    // Invalid and unmodelled scopes fail when the flag is set
    // (StarlarkOptionsParser.java:256-275).
    assert_eq!(
        ws.rejected(&[LINUX, "--//f:bogus=x", "//:u"]),
        "bazel-rs: Can't load flag --//f:bogus: Invalid \"scope\" attribute value \"everywhere\". Allowed values: [\"universal\", \"target\", \"project\"].\n"
    );
    assert_eq!(
        ws.rejected(&[LINUX, "--//f:project=x", "//:u"]),
        "bazel-rs: --//f:project: build settings with scope = \"project\" (project-scoped flags, PROJECT.scl) are not implemented in the native profile\n"
    );
}

#[test]
fn exec_configurations_resolve_toolchains_for_the_execution_platform() {
    let ws = Workspace::new(MODULE, &[]);
    // The target configuration resolves for //p:mac; the exec configuration
    // for its target platform, the execution platform //p:linux.
    let (_, contents) = ws.built(&[LINUX, "--platforms=//p:mac", "//:tu"]);
    assert_eq!(
        contents,
        "t: toolchain=linux at=bazel-out/linux-opt-exec/bin/t.txt\nt: toolchain=mac at=bazel-out/k8-fastbuild/bin/t.txt\ntu: tool=bazel-out/linux-opt-exec/bin/t.txt\n"
    );
    // --extra_toolchains propagates into the exec configuration
    // (PlatformOptions, builtin_exec_platforms.bzl:411-419), which
    // re-registers its toolchains: the extra linux toolchain wins there.
    let (_, contents) = ws.built(&[
        LINUX,
        "--platforms=//p:mac",
        "--extra_toolchains=//tc:linux_extra",
        "//:tu",
    ]);
    assert_eq!(
        contents,
        "t: toolchain=linux_extra at=bazel-out/linux-opt-exec/bin/t.txt\nt: toolchain=mac at=bazel-out/k8-fastbuild/bin/t.txt\ntu: tool=bazel-out/linux-opt-exec/bin/t.txt\n"
    );
}

#[test]
fn transitions_without_an_execution_platform_and_exec_group_errors() {
    let ws = Workspace::new(MODULE, &[]);
    // A build-setting rule does not use toolchain resolution: no execution
    // platform, so the exec transition is a no-op
    // (DependencyResolutionHelpers.java:223-229, ExecutionTransitionFactory.java:194-197).
    let (lines, _) = ws.built(&[LINUX, "//:setting"]);
    assert_eq!(lines, ["//:setting\tbazel-out/k8-fastbuild/bin/m.txt"]);
    // config.exec("g") of a rule without that exec group
    // (DependencyResolutionHelpers.java:243-253).
    assert_eq!(
        ws.rejected(&[LINUX, "//:group"]),
        "bazel-rs: WS/BUILD.bazel:20:10: Attr 'tool' declares a transition for non-existent exec group 'g'\n"
    );
    // The lazily resolved default host platform (docs/PLATFORMS.md) is the
    // execution platform: without the built-in profile it fails explicitly.
    let diagnostic = ws.rejected(&["//:g"]);
    assert!(
        diagnostic.starts_with(
            "bazel-rs: WS/BUILD.bazel:21:8: While resolving toolchains for target //:g ("
        ) && diagnostic.contains("needs the built-in @bazel_tools repository"),
        "{diagnostic}"
    );
    // A cycle through exec edges prints each element's configuration; the
    // path starts with the requested rule's untrimmed top-level key, then its
    // test-trimmed key (paired capture A4/cycle_a: five entries,
    // TestTrimmingTransitionFactory.java:92-135).
    let diagnostic = ws.rejected(&[LINUX, "//:cycle_a"]);
    let configs = diagnostic
        .match_indices(" (")
        .map(|(index, _)| &diagnostic[index + 2..index + 66])
        .collect::<Vec<_>>();
    assert_eq!(configs.len(), 5, "{diagnostic}");
    let (untrimmed, top, exec) = (configs[0], configs[1], configs[2]);
    assert!(untrimmed != top && top != exec && untrimmed != exec);
    assert_eq!(
        diagnostic,
        format!(
            "bazel-rs: WS/BUILD.bazel:26:4: in use rule //:cycle_b: cycle in dependency graph:\n    //:cycle_a ({untrimmed})\n    //:cycle_a ({top})\n.-> //:cycle_b ({exec})\n|   //:cycle_a ({exec})\n`-- //:cycle_b ({exec})\n"
        )
    );
}

/// The genuine platforms module archive (tests/platforms.rs).
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

/// rules_cc's `_def_parser` shape: an `attr.label(cfg = "exec",
/// allow_single_file = True)` default naming a filegroup that selects on
/// `@platforms//os:windows`, analyzed for the default host platform's
/// execution platform `@platforms//host` (the built-in profile).
#[test]
fn default_host_execution_platform_and_def_parser_shape() {
    let ws = Workspace::new(
        "module(name = 'main')\nbazel_dep(name = 'platforms', version = '1.0.0')\nlocal_path_override(module_name = 'platforms', path = 'platforms')\n",
        &[
            (
                "cc/BUILD.bazel",
                r#"load(":defs.bzl", "cc_like")
filegroup(name = "def_parser", srcs = select({
    "@platforms//os:windows": ["def_parser.exe"],
    "//conditions:default": ["no_op.bat"],
}))
cc_like(name = "lib")
"#,
            ),
            (
                "cc/defs.bzl",
                r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "def_parser=%s\n" % ctx.file._def_parser.path)
    return [DefaultInfo(files = depset([out]))]

cc_like = rule(implementation = _impl, attrs = {
    "_def_parser": attr.label(default = ":def_parser", allow_single_file = True, cfg = "exec"),
})
"#,
            ),
            ("cc/no_op.bat", "@echo off\n"),
            ("cc/def_parser.exe", "MZ"),
        ],
    );
    extract_platforms(&ws.path().join("platforms"));
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let (_, contents) = ws.built(&["--experimental-builtin-modules", &registry, "//cc:lib"]);
    // The execution platform is the end of the default host platform's alias
    // chain, @@platforms//host:host: a default platform name, so the CPU
    // descriptor, and `platforms` is explicit in an exec output path, so no
    // ST- hash (paired capture A5/lib_generated: Bazel's `k8-opt-exec`).
    let source = contents
        .strip_prefix("def_parser=")
        .unwrap_or_else(|| panic!("{contents}"));
    assert_eq!(source, "cc/no_op.bat\n");
    // A generated file in that exec configuration shows the root.
    ws.file(
        "cc/BUILD.bazel",
        r#"load(":defs.bzl", "cc_like")
genrule(name = "gen", outs = ["def_parser.sh"], cmd = "echo > $@")
filegroup(name = "def_parser", srcs = select({
    "@platforms//os:windows": ["def_parser.exe"],
    "//conditions:default": [":gen"],
}))
cc_like(name = "lib")
"#,
    );
    let (_, contents) = ws.built(&["--experimental-builtin-modules", &registry, "//cc:lib"]);
    assert_eq!(
        contents,
        "def_parser=bazel-out/k8-opt-exec/bin/cc/def_parser.sh\n"
    );
    // The same for the default spelled explicitly and for --host_compilation_mode.
    let (_, contents) = ws.built(&[
        "--experimental-builtin-modules",
        &registry,
        "--host_platform=@bazel_tools//tools:host_platform",
        "--host_compilation_mode=dbg",
        "//cc:lib",
    ]);
    assert_eq!(
        contents,
        "def_parser=bazel-out/k8-dbg-exec/bin/cc/def_parser.sh\n"
    );
}

/// Paired-capture corrections: exec roots named after the execution
/// platform (also through an alias), deprecation warnings deduplicated
/// across configurations, the order of scope errors and
/// --incompatible_disable_starlark_host_transitions.
#[test]
fn exec_roots_warnings_scopes_and_host_transitions_follow_bazel() {
    let ws = Workspace::new(
        MODULE,
        &[
            (
                "dp/BUILD.bazel",
                "load('//r:defs.bzl', 'show')\nshow(name = 'old', mode = 'm', deprecation = 'use //dp:new', visibility = ['//visibility:public'])\n",
            ),
            (
                "xb/BUILD.bazel",
                "load('//r:defs.bzl', 'use')\nuse(name = 'dw', tool = '//dp:old', dep = '//dp:old')\n",
            ),
        ],
    );
    // --host_platform=//p:linux_alias: the execution platform is the alias
    // chain's end //p:linux, whose name is the CPU descriptor; `platforms`
    // is explicit, so no ST- hash (paired capture B1/u_host_linux_alias).
    let (_, contents) = ws.built(&["--host_platform=//p:linux_alias", "//:u"]);
    assert!(
        contents.ends_with("u: tool=bazel-out/linux-opt-exec/bin/m.txt\n"),
        "{contents}"
    );
    let (_, contents) = ws.built(&["--host_platform=//p:mac", "//:u"]);
    assert!(
        contents.ends_with("u: tool=bazel-out/mac-opt-exec/bin/m.txt\n"),
        "{contents}"
    );
    // A deprecated dependency reached in the exec and the target
    // configuration warns once (EmittedEventState, paired capture B1/dw).
    let output = ws.run(&["build", LINUX, "//xb:dw"]);
    assert!(output.status.success(), "{}", errors(&output));
    let warning = "WARNING: WS/xb/BUILD.bazel:2:4: in use rule //xb:dw: target '//xb:dw' depends on deprecated target '//dp:old': use //dp:new";
    let stderr = errors(&output).replace(&ws.path().display().to_string(), "WS");
    assert_eq!(stderr.matches(warning).count(), 1, "{stderr}");
    // Bazel's invalid-scope error wins over the native project-scope gap
    // whatever the order (paired capture B1/project_and_bogus).
    for args in [
        ["--//f:project=y", "--//f:bogus=x"],
        ["--//f:bogus=x", "--//f:project=y"],
    ] {
        assert_eq!(
            ws.rejected(&[LINUX, args[0], args[1], "//:u"]),
            "bazel-rs: Can't load flag --//f:bogus: Invalid \"scope\" attribute value \"everywhere\". Allowed values: [\"universal\", \"target\", \"project\"].\n"
        );
    }
    // --incompatible_disable_starlark_host_transitions (default false) makes
    // cfg = "host" a .bzl evaluation error (StarlarkAttrModule.java:520-531).
    let (_, contents) = ws.built(&[
        LINUX,
        "--noincompatible_disable_starlark_host_transitions",
        "//:h",
    ]);
    assert!(
        contents.ends_with("h: tool=bazel-out/linux-opt-exec/bin/m.txt\n"),
        "{contents}"
    );
    let diagnostic = ws.rejected(&[
        LINUX,
        "--incompatible_disable_starlark_host_transitions",
        "//:h",
    ]);
    assert!(
        diagnostic.contains("'cfg = \"host\"' is deprecated and should no longer be used. Please use 'cfg = \"exec\"' instead."),
        "{diagnostic}"
    );
    assert_eq!(
        ws.rejected(&[
            LINUX,
            "--incompatible_disable_starlark_host_transitions=maybe",
            "//:h",
        ]),
        "bazel-rs: While parsing option --incompatible_disable_starlark_host_transitions=maybe: 'maybe' is not a boolean\n"
    );
}
