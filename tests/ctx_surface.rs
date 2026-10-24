//! CLI coverage for N23c (docs/CTX_SURFACE.md): the rule-implementation `ctx`
//! members genuine rule sets read during analysis — `ctx.fragments` and its
//! configuration fragments, `ctx.configuration`, `ctx.bin_dir` /
//! `ctx.genfiles_dir`, `ctx.var`, `ctx.workspace_name`,
//! `ctx.build_file_path`, `ctx.features`, `ctx.exec_groups`,
//! `ctx.expand_make_variables`, `ctx.tokenize` and friends — with the values
//! and messages of the paired capture against Bazel 9.3
//! (tests/fixtures/ctx_surface/reference_receipt.json; the receipt's native
//! `rust-` roots are historical: N41 closed divergence D1, roots are `k8-`).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use base64::Engine as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n23c-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file(
            "MODULE.bazel",
            "module(name = 'main')\nbazel_dep(name = 'platforms', version = '1.0.0')\nlocal_path_override(module_name = 'platforms', path = 'platforms')\n",
        );
        for (path, content) in files {
            ws.file(path, content);
        }
        extract_platforms(&ws.path().join("platforms"));
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
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            env!("CARGO_MANIFEST_DIR")
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(args[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&args[1..])
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }

    /// The concatenated output contents of `build ARGS`.
    fn built(&self, args: &[&str]) -> String {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        let output = self.run(&command);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let mut contents = String::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (_, path) = line.split_once('\t').unwrap();
            contents.push_str(&fs::read_to_string(path).unwrap());
        }
        contents
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
        errors(&output)
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

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

/// `//test` is on BuiltinRestriction's default allowlist (packages/BuiltinRestriction.java:36-104).
const DEFS: &str = r#"
ShowInfo = provider(fields = ["lines"])

def _show(ctx):
    c = ctx.configuration
    p = ctx.fragments.platform
    L = [
        "dir(ctx)=" + repr(dir(ctx)),
        "fragments=" + type(ctx.fragments) + " " + repr(ctx.fragments) + " " + repr(dir(ctx.fragments)),
        "hasattr=" + repr([hasattr(ctx.fragments, f) for f in ["cpp", "java", "platform", "py", "nope"]]),
        "configuration=" + type(c) + " " + repr(c) + " " + repr(dir(c)),
        "cfg=" + repr([c.bin_dir.path, c.genfiles_dir.path, c.host_path_separator, c.default_shell_env, c.test_env, c.coverage_enabled, len(c.short_id), c.is_tool_configuration(), c.stamp_binaries(), c.has_separate_genfiles_directory(), c.is_sibling_repository_layout(), c.runfiles_enabled(), c.disabled_features()]),
        "roots=" + type(ctx.bin_dir) + " " + repr(ctx.bin_dir) + " " + ctx.bin_dir.path + " " + ctx.genfiles_dir.path + " " + repr([ctx.bin_dir == ctx.genfiles_dir, dir(ctx.bin_dir)]),
        "var=" + repr(ctx.var),
        "workspace_name=" + ctx.workspace_name + " build_file_path=" + ctx.build_file_path,
        "features=" + repr([ctx.features, ctx.disabled_features]),
        "coverage=" + repr([ctx.coverage_instrumented(), ctx.coverage_instrumented(ctx.attr.dep)]),
        "exec_groups=" + type(ctx.exec_groups) + " " + repr(ctx.exec_groups) + " " + repr("g" in ctx.exec_groups),
        "expand_make=" + ctx.expand_make_variables("cmd", "$(COMPILATION_MODE) $$HOME $(FOO) $(TARGET_CPU) $(BINDIR)", {"FOO": "bar"}),
        "expand_location=" + ctx.expand_location("plain $$ $(FOO) $(location)"),
        "tokenize=" + repr(ctx.tokenize("a 'b c' d\\ e")),
        "check_placeholders=" + repr([ctx.check_placeholders("%{a}x", ["a"]), ctx.check_placeholders("%{a}%{b}", ["a"])]),
        "package_relative_label=" + repr([ctx.package_relative_label(":x"), ctx.package_relative_label("//a:b")]),
        "split_attr=" + repr(ctx.split_attr) + " created_actions=" + repr(ctx.created_actions()),
        "platform=" + repr([type(p), p.host_platform, p.platform, p.platforms]),
        "cpp=" + repr([ctx.fragments.cpp.compilation_mode(), ctx.fragments.cpp.force_pic(), ctx.fragments.cpp.should_strip_binaries(), ctx.fragments.cpp.copts]),
        "java=" + repr([type(ctx.fragments.java), ctx.fragments.java.strict_java_deps, ctx.fragments.java.use_ijars(), ctx.fragments.java.default_javac_flags_depset, ctx.fragments.java.one_version_enforcement_level]),
        "apple=" + repr([ctx.fragments.apple.single_arch_platform, ctx.fragments.apple.single_arch_platform.platform_type, ctx.fragments.apple.xcode_version_flag, ctx.fragments.apple.prefer_mutual_xcode, ctx.fragments.apple.include_xcode_exec_requirements]),
        "objc=" + repr([ctx.fragments.objc.copts_for_current_compilation_mode, ctx.fragments.objc.target_should_alwayslink(ctx)]),
        "coverage_fragment=" + repr(ctx.fragments.coverage.output_generator),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(L) + "\n")
    return [DefaultInfo(files = depset([out])), ShowInfo(lines = L)]

show = rule(
    implementation = _show,
    attrs = {"dep": attr.label()},
    fragments = ["cpp", "java", "apple", "objc", "coverage"],
)

def _top(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = ctx.attr.tool[ShowInfo].lines
    ctx.actions.write(out, "\n".join(lines[3:7] + [lines[18]]) + "\n")
    return [DefaultInfo(files = depset([out]))]

top = rule(implementation = _top, attrs = {"tool": attr.label(cfg = "exec")})

def _err(ctx):
    k = ctx.attr.case
    if k == "undeclared": x = ctx.fragments.cpp
    elif k == "unknown": x = ctx.fragments.py
    elif k == "hasattr": x = hasattr(ctx.fragments, "apple")
    elif k == "make_undefined": x = ctx.expand_make_variables("cmd", "$(NOPE) a", {})
    elif k == "make_single": x = ctx.expand_make_variables("copts", "$F", {"F": "1"})
    elif k == "make_subst": x = ctx.expand_make_variables("cmd", "x", {"F": 1})
    elif k == "exec_group": x = ctx.exec_groups["nope"]
    elif k == "tokenize": x = ctx.tokenize("'unterminated")
    elif k == "info_file": x = ctx.info_file
    elif k == "location": x = ctx.expand_location("$(location :x)")
    elif k == "unknown_member": x = ctx.configuration.nope
    elif k == "android": x = ctx.fragments.android.desugar_java8
    elif k == "super": x = ctx.super()
    elif k == "aspect_ids": x = ctx.aspect_ids
    elif k == "build_setting_value": x = ctx.build_setting_value
    elif k == "var_mutate": x = ctx.var.update({"A": "b"})
    else: fail("bad case " + k)
    return []

err = rule(implementation = _err, attrs = {"case": attr.string()}, fragments = ["java", "android"])
"#;

const PRIVATE_DEFS: &str = r#"
def _p(ctx):
    k = ctx.attr.case
    if k == "stamp": x = ctx.configuration.stamp_binaries()
    elif k == "java": x = ctx.fragments.java.use_ijars()
    elif k == "strict": fail("VALUE " + ctx.fragments.java.strict_java_deps)
    elif k == "short_paths": x = ctx.expand_location("x", short_paths = True)
    return []

p = rule(implementation = _p, attrs = {"case": attr.string()}, fragments = ["java"])
"#;

const BUILD: &str = r#"load(":defs.bzl", "err", "show", "top")
package(features = ["pkg_on", "-pkg_off", "both"])
show(name = "s", dep = ":dep", features = ["rule_on", "-both"])
show(name = "sx", dep = ":dep")
top(name = "t", tool = ":sx")
filegroup(name = "dep", srcs = [])
"#;

fn workspace() -> Workspace {
    let cases = [
        "undeclared",
        "unknown",
        "hasattr",
        "make_undefined",
        "make_single",
        "make_subst",
        "exec_group",
        "tokenize",
        "location",
        "unknown_member",
        "android",
        "super",
        "aspect_ids",
        "build_setting_value",
        "var_mutate",
    ];
    let mut build = BUILD.to_owned();
    for case in cases {
        build.push_str(&format!("err(name = 'e_{case}', case = '{case}')\n"));
    }
    let private = ["stamp", "java", "strict", "short_paths"]
        .map(|case| format!("p(name = '{case}', case = '{case}')\n"))
        .concat();
    Workspace::new(&[
        ("test/defs.bzl", DEFS),
        ("test/BUILD.bazel", &build),
        ("pkg/defs.bzl", PRIVATE_DEFS),
        ("pkg/BUILD", &format!("load(':defs.bzl', 'p')\n{private}")),
    ])
}

/// Genuine Bazel 9.3's 36 StarlarkRuleContext members.
const CTX_DIR: &str = r#"dir(ctx)=["actions", "aspect_ids", "attr", "bin_dir", "build_file_path", "build_setting_value", "check_placeholders", "configuration", "coverage_instrumented", "created_actions", "disabled_features", "exec_groups", "executable", "expand_location", "expand_make_variables", "features", "file", "files", "fragments", "genfiles_dir", "info_file", "label", "outputs", "package_relative_label", "resolve_command", "resolve_tools", "rule", "runfiles", "split_attr", "super", "target_platform_has_constraint", "tokenize", "toolchains", "var", "version_file", "workspace_name"]"#;

#[test]
fn rule_context_members_report_the_configuration() {
    let ws = workspace();
    let expected = [
        CTX_DIR,
        r#"fragments=fragments <unknown object com.google.devtools.build.lib.analysis.FragmentCollection> ["android", "apple", "bazel_android", "coverage", "cpp", "j2objc", "java", "objc", "platform", "proto"]"#,
        "hasattr=[True, True, True, False, False]",
        r#"configuration=configuration <unknown object com.google.devtools.build.lib.analysis.config.BuildConfigurationValue> ["bin_dir", "coverage_enabled", "default_shell_env", "disabled_features", "genfiles_dir", "has_separate_genfiles_directory", "host_path_separator", "is_sibling_repository_layout", "is_tool_configuration", "runfiles_enabled", "short_id", "stamp_binaries", "test_env"]"#,
        r#"cfg=["bazel-out/k8-fastbuild/bin", "bazel-out/k8-fastbuild/bin", ":", {"PATH": "/bin:/usr/bin:/usr/local/bin"}, {}, False, 7, False, False, False, False, True, set()]"#,
        r#"roots=root <derived root> bazel-out/k8-fastbuild/bin bazel-out/k8-fastbuild/bin [True, ["path"]]"#,
        r#"var={"TARGET_CPU": "x86_64", "COMPILATION_MODE": "fastbuild", "BINDIR": "bazel-out/k8-fastbuild/bin", "GENDIR": "bazel-out/k8-fastbuild/bin"}"#,
        "workspace_name=_main build_file_path=test/BUILD.bazel",
        r#"features=[["pkg_on", "rule_on"], ["both", "pkg_off"]]"#,
        "coverage=[False, False]",
        "exec_groups=ExecGroupCollection <ctx.exec_groups: > False",
        "expand_make=fastbuild $HOME bar x86_64 bazel-out/k8-fastbuild/bin",
        "expand_location=plain $$ $(FOO) $(location)",
        r#"tokenize=["a", "b c", "d e"]"#,
        "check_placeholders=[True, False]",
        r#"package_relative_label=[Label("//test:x"), Label("//a:b")]"#,
        "split_attr=struct() created_actions=None",
        r#"platform=["platform", Label("@@bazel_tools//tools:host_platform"), Label("@@bazel_tools//tools:host_platform"), [Label("@@bazel_tools//tools:host_platform")]]"#,
        r#"cpp=["fastbuild", False, True, []]"#,
        r#"java=["java", "default", True, depset([], order = "preorder"), "OFF"]"#,
        r#"apple=[macos, "macos", None, True, False]"#,
        r#"objc=[["-O0", "-DDEBUG=1"], False]"#,
        "coverage_fragment=None",
        "",
    ];
    if std::env::consts::ARCH != "x86_64" {
        return;
    }
    assert_eq!(ws.built(&["//test:s"]), expected.join("\n"));
    // -c opt and --define: COMPILATION_MODE, the opt root and the sorted
    // command-line variables after the global ones.
    let opt = ws.built(&[
        "-c",
        "opt",
        "--define=FOO=bar",
        "--define",
        "A=1",
        "//test:sx",
    ]);
    assert!(
        opt.contains(r#"var={"TARGET_CPU": "x86_64", "COMPILATION_MODE": "opt", "BINDIR": "bazel-out/k8-opt/bin", "GENDIR": "bazel-out/k8-opt/bin", "A": "1", "FOO": "bar"}"#),
        "{opt}"
    );
    // The package's features (FeatureSet.merge with no rule features).
    assert!(
        opt.contains(r#"features=[["both", "pkg_on"], ["pkg_off"]]"#),
        "{opt}"
    );
    assert!(opt.contains(r#"cpp=["opt", False, False, []]"#), "{opt}");
    assert!(opt.contains(r#"objc=[[], False]"#), "{opt}");
    // The exec configuration: is_tool_configuration, the exec root and the
    // execution platform's CPU.
    assert_eq!(
        ws.built(&["//test:t"]),
        [
            r#"configuration=configuration <unknown object com.google.devtools.build.lib.analysis.config.BuildConfigurationValue> ["bin_dir", "coverage_enabled", "default_shell_env", "disabled_features", "genfiles_dir", "has_separate_genfiles_directory", "host_path_separator", "is_sibling_repository_layout", "is_tool_configuration", "runfiles_enabled", "short_id", "stamp_binaries", "test_env"]"#,
            r#"cfg=["bazel-out/k8-opt-exec/bin", "bazel-out/k8-opt-exec/bin", ":", {"PATH": "/bin:/usr/bin:/usr/local/bin"}, {}, False, 7, True, False, False, False, True, set()]"#,
            r#"roots=root <derived root> bazel-out/k8-opt-exec/bin bazel-out/k8-opt-exec/bin [True, ["path"]]"#,
            r#"var={"TARGET_CPU": "x86_64", "COMPILATION_MODE": "opt", "BINDIR": "bazel-out/k8-opt-exec/bin", "GENDIR": "bazel-out/k8-opt-exec/bin"}"#,
            // The exec transition appends -g0 to --copt/--cxxopt
            // (builtin_exec_platforms.bzl:341-342).
            r#"cpp=["opt", False, True, ["-g0"]]"#,
            "",
        ]
        .join("\n")
    );
}

#[test]
fn rule_context_errors_use_bazel_texts_or_fail_explicitly() {
    let ws = workspace();
    for (case, text) in [
        (
            "undeclared",
            "err has to declare 'cpp' as a required fragment in order to access it. Please update the 'fragments' argument of the rule definition (for example: fragments = [\"cpp\"])",
        ),
        (
            "unknown",
            "There is no configuration fragment named 'py'. Available fragments: 'platform', 'coverage', 'android', 'bazel_android', 'apple', 'cpp', 'java', 'j2objc', 'objc', 'proto'",
        ),
        (
            "hasattr",
            "err has to declare 'apple' as a required fragment in order to access it.",
        ),
        (
            "make_undefined",
            "in cmd attribute of err rule //test:e_make_undefined: $(NOPE) not defined",
        ),
        (
            "make_single",
            "in copts attribute of err rule //test:e_make_single: '$F' syntax is not supported; use '$(F)' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell",
        ),
        (
            "make_subst",
            "got dict<string, int> for 'additional_substitutions', want dict<string, string>",
        ),
        (
            "exec_group",
            "In err rule //test:e_exec_group, unrecognized exec group 'nope' requested. Available exec groups: []",
        ),
        (
            "tokenize",
            "unterminated quotation while tokenizing ''unterminated'",
        ),
        // N53: the location map of the rule (docs/MAKE_VARIABLES.md).
        (
            "location",
            "in err rule //test:e_location: label '//test:x' in $(location) expression is not a declared prerequisite of this rule",
        ),
        (
            "unknown_member",
            "'configuration' value has no field or method 'nope'",
        ),
        (
            "android",
            "configuration fragment member 'android.desugar_java8' is not implemented by the native profile",
        ),
        ("super", "Can't use 'super' call, the rule has no parent."),
        (
            "aspect_ids",
            "'aspect_ids' is only available in aspect implementations",
        ),
        (
            "build_setting_value",
            "attempting to access 'build_setting_value' of non-build setting //test:e_build_setting_value",
        ),
        ("var_mutate", "trying to mutate a frozen dict value"),
    ] {
        let error = ws.rejected(&[&format!("//test:e_{case}")]);
        assert!(error.contains(text), "{case}: {error}");
    }
    for (case, text) in [
        ("stamp", "file '//pkg:defs.bzl' cannot use private API"),
        ("java", "file '//pkg:defs.bzl' cannot use private API"),
        ("strict", "VALUE default"),
        (
            "short_paths",
            "file '//pkg:defs.bzl' cannot use private API",
        ),
    ] {
        let error = ws.rejected(&[&format!("//pkg:{case}")]);
        assert!(error.contains(text), "{case}: {error}");
    }
    // An implementation-time attribute error fails the target even when the
    // implementation returns, before its own failure.
    let error = ws.rejected(&["//test:e_make_undefined"]);
    assert!(!error.contains("Traceback"), "{error}");
}

/// The no-config configuration (the `package_metadata` edge,
/// BaseRuleClasses.java:291-297) has no configuration
/// fragments (CommonOptions.EMPTY_OPTIONS): FragmentCollectionApi's
/// unknown-field text with an empty list, and `$(TARGET_CPU)` is empty.
#[test]
fn no_config_configuration_has_no_fragments() {
    let ws = Workspace::new(&[
        (
            "defs.bzl",
            r#"
def _leaf(ctx):
    if ctx.attr.read:
        return [DefaultInfo(files = depset([]))] if ctx.fragments.cpp else []
    print("LEAF dir=%s var=%s bin=%s" % (dir(ctx.fragments), ctx.var["TARGET_CPU"] == "", ctx.bin_dir.path))
    return []

leaf = rule(implementation = _leaf, attrs = {"read": attr.bool()}, fragments = ["cpp"])

def _root(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "ok\n")
    return [DefaultInfo(files = depset([out]))]

root = rule(implementation = _root)
"#,
        ),
        (
            "BUILD.bazel",
            r#"load(":defs.bzl", "leaf", "root")
leaf(name = "leaf", package_metadata = [])
leaf(name = "reader", read = True, package_metadata = [])
root(name = "ok", package_metadata = [":leaf"])
root(name = "bad", package_metadata = [":reader"])
"#,
        ),
    ]);
    let output = ws.run(&["build", "//:ok"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        errors(&output).contains("LEAF dir=[] var=True bin=bazel-out/fastbuild-noconfig/bin"),
        "{}",
        errors(&output)
    );
    let error = ws.rejected(&["//:bad"]);
    assert!(
        error.contains("There is no configuration fragment named 'cpp'. Available fragments: ''"),
        "{error}"
    );
}

/// Configuration-dependent fragment fields, package feature layers and
/// attribute-provided Make variables (N23c review B1-B4).
#[test]
fn configuration_dependent_fields_feature_layers_and_attribute_variables() {
    let ws = Workspace::new(&[
        // PackageArgs: REPO.bazel's layer under the package()'s, merged with
        // FeatureSet.merge (the finer layer wins).
        ("REPO.bazel", "repo(features = ['-x', 'r'])\n"),
        (
            "test/defs.bzl",
            r#"
VInfo = provider(fields = ["lines"])

def _vars(ctx):
    return [platform_common.TemplateVariableInfo(ctx.attr.vars)]

vars = rule(implementation = _vars, attrs = {"vars": attr.string_dict()})

def _f(ctx):
    L = [
        "features=" + repr([ctx.features, ctx.disabled_features]),
        "java=" + repr(ctx.fragments.java.default_jvm_opts),
        "objc=" + repr(ctx.fragments.objc.uses_device_debug_entitlements),
        "var=" + repr(ctx.var),
        "make=" + ctx.expand_make_variables("cmd", "$(CC) $(T) $(C)", {}),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(L) + "\n")
    return [DefaultInfo(files = depset([out])), VInfo(lines = L)]

f = rule(implementation = _f, fragments = ["java", "objc"], attrs = {
    "_toolchains": attr.label_list(default = [":t1", ":t2"]),
    "_cc_toolchain": attr.label(default = ":cc"),
})

def _top(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(ctx.attr.tool[VInfo].lines[1:3]) + "\n")
    return [DefaultInfo(files = depset([out]))]

top = rule(implementation = _top, attrs = {"tool": attr.label(cfg = "exec")})
"#,
        ),
        (
            "test/BUILD.bazel",
            r#"load(":defs.bzl", "f", "top", "vars")
package(features = ["x", "-r"])
vars(name = "t1", vars = {"T": "t1", "C": "t1c"})
vars(name = "t2", vars = {"T": "t2", "Z": "z"})
vars(name = "cc", vars = {"CC": "gcc", "C": "cc"})
f(name = "f")
f(name = "fx")
top(name = "top", tool = ":fx")
"#,
        ),
    ]);
    // ctx.var: TemplateVariableInfo values merged later-wins and sorted
    // (MakeVariableSupplier.java:63-70); lookups take the first provider in
    // `$toolchains`, `$cc_toolchain` order (:52-61).
    assert_eq!(
        ws.built(&["//test:f"]),
        [
            r#"features=[["x"], ["r"]]"#,
            "java=[]",
            "objc=True",
            r#"var={"TARGET_CPU": "x86_64", "COMPILATION_MODE": "fastbuild", "BINDIR": "bazel-out/k8-fastbuild/bin", "GENDIR": "bazel-out/k8-fastbuild/bin", "C": "cc", "CC": "gcc", "T": "t2", "Z": "z"}"#,
            "make=gcc t1 t1c",
            "",
        ]
        .join("\n")
    );
    let opt = ws.built(&["-c", "opt", "//test:f"]);
    assert!(opt.contains("\nobjc=False\n"), "{opt}");
    // The exec transition's --jvmopt and --host_compilation_mode (opt).
    assert_eq!(
        ws.built(&["//test:top"]),
        "java=[\"-XX:ErrorFile=/dev/stderr\"]\nobjc=False\n"
    );
}

/// Merge adaptations (N23c x N18c/N30/N25d2a): the ctx surface is built per
/// configured-target key, so a dependency behind an attribute transition, a
/// rule-transitioned target and a target planned early for a config_setting
/// (N30's flag_values flush) each see their own configuration in ctx.var,
/// ctx.bin_dir, ctx.configuration and ctx.fragments (`//test` is on the
/// private-API allowlist for `cpp.compilation_mode()`).
const TRANSITION_DEFS: &str = r#"
FFI = config_common.FeatureFlagInfo
def _to_dbg(settings, attr):
    return {"//command_line_option:compilation_mode": "dbg"}
to_dbg = transition(implementation = _to_dbg, inputs = [], outputs = ["//command_line_option:compilation_mode"])
def _line(ctx):
    return "%s var=%s bin=%s cfg_bin=%s cpp=%s" % (
        ctx.label.name,
        ctx.var["COMPILATION_MODE"],
        ctx.bin_dir.path,
        ctx.configuration.bin_dir.path,
        ctx.fragments.cpp.compilation_mode(),
    )
LineInfo = provider(fields = ["line"])
def _leaf(ctx):
    return [LineInfo(line = _line(ctx))]
leaf = rule(implementation = _leaf, fragments = ["cpp"])
leaf_dbg = rule(implementation = _leaf, fragments = ["cpp"], cfg = to_dbg)
def _key(ctx):
    print("KEY " + _line(ctx))
    return [FFI(value = ctx.var["COMPILATION_MODE"])]
key = rule(implementation = _key, fragments = ["cpp"])
def _top(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [_line(ctx)] + [d[LineInfo].line for d in ctx.attr.edge + ctx.attr.deps] + [ctx.attr.mode]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]
top = rule(implementation = _top, fragments = ["cpp"], attrs = {
    "edge": attr.label_list(cfg = to_dbg),
    "deps": attr.label_list(),
    "mode": attr.string(),
})
"#;

const TRANSITION_BUILD: &str = r#"load(":defs.bzl", "key", "leaf", "leaf_dbg", "top")
leaf(name = "edge_leaf")
leaf_dbg(name = "rule_leaf")
key(name = "k")
config_setting(name = "k_opt", flag_values = {":k": "opt"})
top(
    name = "t",
    edge = [":edge_leaf"],
    deps = [":rule_leaf"],
    mode = select({":k_opt": "key saw opt", "//conditions:default": "key did not see opt"}),
)
"#;

#[test]
fn ctx_surface_is_per_configured_target_key() {
    let ws = Workspace::new(&[
        ("test/defs.bzl", TRANSITION_DEFS),
        ("test/BUILD.bazel", TRANSITION_BUILD),
    ]);
    let built = ws.built(&["-c", "opt", "//test:t"]);
    let lines: Vec<&str> = built.lines().collect();
    assert_eq!(
        lines[0], "t var=opt bin=bazel-out/k8-opt/bin cfg_bin=bazel-out/k8-opt/bin cpp=opt",
        "{built}"
    );
    for (line, name) in [(lines[1], "edge_leaf"), (lines[2], "rule_leaf")] {
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields[0], name, "{built}");
        assert_eq!(fields[1], "var=dbg", "{built}");
        assert_eq!(fields[4], "cpp=dbg", "{built}");
        let bin = fields[2].strip_prefix("bin=").unwrap();
        assert!(
            bin.starts_with("bazel-out/k8-dbg") && bin.ends_with("/bin"),
            "{built}"
        );
        assert_eq!(fields[3], format!("cfg_bin={bin}"), "{built}");
    }
    // The flag_values key ran early (Engine::plan_pending) in its own
    // configuration.
    assert_eq!(lines[3], "key saw opt", "{built}");
    let output = ws.run(&["build", "-c", "opt", "//test:t"]);
    assert!(
        errors(&output).contains("KEY k var=opt bin=bazel-out/k8-opt/bin"),
        "{}",
        errors(&output)
    );
}

/// The apple fragment's version flags are null because the Apple version
/// options are not accepted natively. Once `--xcode_version` and the
/// `--<platform>_sdk_version` / `--<platform>_minimum_os` options are
/// modelled, crate::ctx_surface must read them (as N25d2a DottedVersion
/// values) and this test must change with it.
#[test]
fn apple_version_flags_stay_none_while_the_options_are_rejected() {
    let ws = workspace();
    for option in ["--xcode_version=15.0", "--macos_minimum_os=13.0"] {
        let error = ws.rejected(&[option, "//test:sx"]);
        assert!(error.contains(option.split('=').next().unwrap()), "{error}");
    }
}

/// N40 models `--{ios,macos,tvos,watchos}_sdk_version`: the apple fragment's
/// `*_sdk_version_flag` is the option's DottedVersion
/// (AppleConfiguration.java:104-107), None while unset, also in the exec
/// configuration (AppleCommandLineOptions propagates them,
/// builtin_exec_platforms.bzl:170-176).
#[test]
fn apple_sdk_version_flags_read_the_configuration() {
    let ws = workspace();
    ws.file(
        "sdk/defs.bzl",
        r#"
Flags = provider(fields = ["text"])

def _flags(ctx):
    a = ctx.fragments.apple
    values = [a.ios_sdk_version_flag, a.macos_sdk_version_flag, a.tvos_sdk_version_flag, a.watchos_sdk_version_flag]
    text = repr([(type(v), str(v)) for v in values])
    if ctx.attr.tool:
        text = text + " exec=" + ctx.attr.tool[Flags].text
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, text + "\n")
    return [DefaultInfo(files = depset([out])), Flags(text = text)]

flags = rule(
    implementation = _flags,
    attrs = {"tool": attr.label(cfg = "exec")},
    fragments = ["apple"],
)
"#,
    );
    ws.file(
        "sdk/BUILD.bazel",
        "load(':defs.bzl', 'flags')\nflags(name = 'tool')\nflags(name = 'f', tool = ':tool')\n",
    );
    let none = r#"[("NoneType", "None"), ("NoneType", "None"), ("NoneType", "None"), ("NoneType", "None")]"#;
    assert_eq!(ws.built(&["//sdk:f"]), format!("{none} exec={none}\n"));
    let set = r#"[("DottedVersion", "17.0"), ("DottedVersion", "13.1.0"), ("NoneType", "None"), ("DottedVersion", "10")]"#;
    assert_eq!(
        ws.built(&[
            "--ios_sdk_version=17.0",
            "--macos_sdk_version=13.1.0",
            "--watchos_sdk_version=10",
            "//sdk:f",
        ]),
        format!("{set} exec={set}\n")
    );
}
