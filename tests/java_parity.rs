//! CLI coverage for N28 (rules_java loading parity, docs/CONFIGURATION.md
//! "JavaOptions version strings" and "package_metadata", docs/LANGUAGE_MODULES.md):
//!
//! * N28a: `java_common.internal_DO_NOT_USE()` semantics getters
//!   (JavaStarlarkCommon.java:383-402) with the default private-API
//!   allowlist (BuiltinRestriction.java:36-104), from a rules_java-shaped
//!   module (local_path_override);
//! * N28b: `--java_runtime_version` and friends (JavaOptions.java:373-403) in
//!   config_setting/select(), target and exec configurations
//!   (builtin_exec_platforms.bzl:350-385);
//! * N28c: `package_metadata` / `applicable_licenses` /
//!   `default_package_metadata` edges analyzed in the no-config
//!   configuration (BaseRuleClasses.java:291-297, NoConfigTransition),
//!   including the rules_license `license()`/`package_info()` shapes.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
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
                .prefix("n28-")
                .tempdir_in(base)
                .unwrap(),
        );
        for (path, content) in files {
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
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }

    /// The `LABEL\tbazel-out/...` lines and output contents of `build ARGS`.
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
        let output = self.run(args);
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
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// N28a

/// rules_java java/private/native.bzl shape (`get_internal_java_common`).
const RULES_JAVA_NATIVE: &str = "def get_internal_java_common():
    return java_common.internal_DO_NOT_USE()
";

/// rules_java java/common/... shape: the getters at `.bzl` initialization.
const RULES_JAVA_X: &str = "load('//java/private:native.bzl', 'get_internal_java_common')
_J = get_internal_java_common()
LEGACY = _J.google_legacy_api_enabled()
MERGE = _J.incompatible_java_info_merge_runtime_module_flags()
DISABLE = _J.incompatible_disable_non_executable_java_binary()
(LEGACY, MERGE, DISABLE) == (False, False, False) or fail('values')
def legacy():
    return _J.google_legacy_api_enabled()
def probe(name):
    native.filegroup(name = name, srcs = [])
";

fn rules_java_workspace(toolchains_bzl: &str) -> Workspace {
    Workspace::new(&[
        (
            "MODULE.bazel",
            "module(name = 'app')\nbazel_dep(name = 'rules_java', version = '9.1.0')\nlocal_path_override(module_name = 'rules_java', path = 'rules_java')\n",
        ),
        (
            "BUILD.bazel",
            "load('@rules_java//toolchains:x.bzl', 'X')\nload('@rules_java//java:x.bzl', 'probe')\nprobe(name = 'probe')\n",
        ),
        (
            "rules_java/MODULE.bazel",
            "module(name = 'rules_java', version = '9.1.0')\n",
        ),
        ("rules_java/java/BUILD.bazel", ""),
        ("rules_java/java/private/BUILD.bazel", ""),
        ("rules_java/toolchains/BUILD.bazel", ""),
        ("rules_java/java/private/native.bzl", RULES_JAVA_NATIVE),
        ("rules_java/java/x.bzl", RULES_JAVA_X),
        ("rules_java/toolchains/x.bzl", toolchains_bzl),
    ])
}

#[test]
fn java_semantics_getters_check_the_default_allowlist() {
    // `@rules_java//toolchains` may call internal_DO_NOT_USE()
    // (java_common.bzl:20-25: ("rules_java", "")) and the unrestricted
    // getter; a restricted getter called from a function of
    // `@rules_java//java` checks that innermost `.bzl` and passes.
    let ws = rules_java_workspace(
        "load('//java:x.bzl', 'legacy')
J = java_common.internal_DO_NOT_USE()
X = (J.incompatible_disable_non_executable_java_binary(), legacy())
(X == (False, False)) or fail('values')
",
    );
    let output = ws.run(&["query", "//:probe"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//:probe\n");
    // The restricted getters from `@rules_java//toolchains` itself: the
    // rules_java entry of the default allowlist is ("rules_java", "java").
    for getter in [
        "google_legacy_api_enabled",
        "incompatible_java_info_merge_runtime_module_flags",
    ] {
        let ws = rules_java_workspace(&format!(
            "J = java_common.internal_DO_NOT_USE()\nX = J.{getter}()\n"
        ));
        let error = ws.rejected(&["query", "//:probe"]);
        assert!(
            error.contains("file '@@rules_java+//toolchains:x.bzl' cannot use private API"),
            "{getter}: {error}"
        );
    }
    // --experimental_google_legacy_api is a Starlark semantics option of
    // every loading command (BuildLanguageOptions.java:313-321): the getter
    // returns True, so RULES_JAVA_X's own assertion fails (paired capture N28
    // A1/legacy_flag: `Error in fail: values`); `--no...` keeps False.
    let ws = rules_java_workspace("X = 1\n");
    for command in ["query", "build"] {
        let error = ws.rejected(&[command, "--experimental_google_legacy_api", "//:probe"]);
        assert!(error.contains("fail: values"), "{error}");
    }
    let output = ws.run(&["query", "--noexperimental_google_legacy_api", "//:probe"]);
    assert!(output.status.success(), "{}", errors(&output));
}

/// The three Java semantics flags (BuildLanguageOptions.java:313-321,
/// 559-567, 653-660) and the getters' values (JavaStarlarkCommon.java:383-402),
/// observed as target names (paired capture N28 B2/semantics_flags).
#[test]
fn java_semantics_flags_reach_the_getters() {
    let ws = rules_java_workspace("X = 1\n");
    ws.file(
        "rules_java/java/x.bzl",
        "load('//java/private:native.bzl', 'get_internal_java_common')
_J = get_internal_java_common()
V = (_J.google_legacy_api_enabled(), _J.incompatible_java_info_merge_runtime_module_flags(), _J.incompatible_disable_non_executable_java_binary())
def probe(name):
    native.filegroup(name = name + '_legacy_%s_merge_%s_disable_%s' % V, srcs = [])
",
    );
    for (flags, expected) in [
        (&[][..], "False_merge_False_disable_False"),
        (
            &["--experimental_google_legacy_api"][..],
            "True_merge_False_disable_False",
        ),
        (
            &["--noexperimental_google_legacy_api"][..],
            "False_merge_False_disable_False",
        ),
        (
            &["--incompatible_java_info_merge_runtime_module_flags"][..],
            "False_merge_True_disable_False",
        ),
        (
            &["--incompatible_disable_non_executable_java_binary=true"][..],
            "False_merge_False_disable_True",
        ),
        (
            &["--noincompatible_disable_non_executable_java_binary"][..],
            "False_merge_False_disable_False",
        ),
    ] {
        let mut args = vec!["query"];
        args.extend_from_slice(flags);
        args.push("//:all");
        let output = ws.run(&args);
        assert!(output.status.success(), "{flags:?}: {}", errors(&output));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("//:probe_legacy_{expected}\n"),
            "{flags:?}"
        );
    }
    let output = ws.run(&["build", "--experimental_google_legacy_api", "//:all"]);
    assert!(output.status.success(), "{}", errors(&output));
    let error = ws.rejected(&["query", "--experimental_google_legacy_api=maybe", "//:all"]);
    assert!(
        error.contains(
            "While parsing option --experimental_google_legacy_api=maybe: 'maybe' is not a boolean"
        ),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// N28b

const LINUX: &str = "--host_platform=//p:linux";

const JAVA_BUILD: &str = r#"load("//r:defs.bzl", "show", "use")
config_setting(name = "jrv_local", values = {"java_runtime_version": "local_jdk"})
config_setting(name = "jrv_11", values = {"java_runtime_version": "remotejdk_11"})
config_setting(name = "jrv_17", values = {"java_runtime_version": "remotejdk_17"})
config_setting(name = "jlv_none", values = {"java_language_version": ""})
config_setting(name = "jlv_17", values = {"java_language_version": "17"})
config_setting(name = "tjrv_11", values = {"tool_java_runtime_version": "remotejdk_11"})
config_setting(name = "tjrv_17", values = {"tool_java_runtime_version": "remotejdk_17"})
config_setting(name = "tjlv_none", values = {"tool_java_language_version": ""})
config_setting(name = "tjlv_17", values = {"tool_java_language_version": "17"})
show(
    name = "m",
    jrv = select({":jrv_local": "local_jdk", ":jrv_11": "remotejdk_11", ":jrv_17": "remotejdk_17", "//conditions:default": "other"}),
    jlv = select({":jlv_none": "none", ":jlv_17": "17", "//conditions:default": "other"}),
    tjrv = select({":tjrv_11": "remotejdk_11", ":tjrv_17": "remotejdk_17", "//conditions:default": "other"}),
    tjlv = select({":tjlv_none": "none", ":tjlv_17": "17", "//conditions:default": "other"}),
)
use(name = "u", tool = ":m", dep = ":m")
"#;

const JAVA_DEFS: &str = r#"def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s: jrv=%s jlv=%s tjrv=%s tjlv=%s at=%s\n" % (
        ctx.label.name, ctx.attr.jrv, ctx.attr.jlv, ctx.attr.tjrv, ctx.attr.tjlv, out.path))
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {
    "jrv": attr.string(),
    "jlv": attr.string(),
    "tjrv": attr.string(),
    "tjlv": attr.string(),
})

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.file.tool, ctx.file.dep],
        command = "cat %s %s > %s" % (ctx.file.tool.path, ctx.file.dep.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use_impl, attrs = {
    "tool": attr.label(cfg = "exec", allow_single_file = True),
    "dep": attr.label(allow_single_file = True),
})
"#;

fn java_workspace() -> Workspace {
    Workspace::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        (
            "os/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nconstraint_setting(name = 'os')\nconstraint_value(name = 'linux', constraint_setting = ':os')\n",
        ),
        (
            "p/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nplatform(name = 'linux', constraint_values = ['//os:linux'])\n",
        ),
        ("r/BUILD.bazel", ""),
        ("r/defs.bzl", JAVA_DEFS),
        ("BUILD.bazel", JAVA_BUILD),
    ])
}

fn java_line(name: &str, jrv: &str, jlv: &str, tjrv: &str, tjlv: &str, root: &str) -> String {
    format!(
        "{name}: jrv={jrv} jlv={jlv} tjrv={tjrv} tjlv={tjlv} at=bazel-out/{root}/bin/{name}.txt\n"
    )
}

#[test]
fn java_versions_select_in_target_and_exec_configurations() {
    let ws = java_workspace();
    let exec_default = java_line(
        "m",
        "remotejdk_11",
        "none",
        "remotejdk_11",
        "none",
        "linux-opt-exec",
    );
    // Defaults (JavaOptions.java:373-403); the exec configuration's java_*
    // are the tool_java_* values (builtin_exec_platforms.bzl:358-359).
    let (lines, contents) = ws.built(&[LINUX, "//:u"]);
    assert_eq!(lines, ["//:u\tbazel-out/k8-fastbuild/bin/u.txt"]);
    assert_eq!(
        contents,
        format!(
            "{exec_default}{}",
            java_line(
                "m",
                "local_jdk",
                "none",
                "remotejdk_11",
                "none",
                "k8-fastbuild"
            )
        )
    );
    // --java_runtime_version only changes the target configuration; its
    // root is unchanged (the top-level configuration is its own baseline)
    // and the exec configuration is the default one.
    let (lines, contents) = ws.built(&[LINUX, "--java_runtime_version=remotejdk_11", "//:u"]);
    assert_eq!(lines, ["//:u\tbazel-out/k8-fastbuild/bin/u.txt"]);
    assert_eq!(
        contents,
        format!(
            "{exec_default}{}",
            java_line(
                "m",
                "remotejdk_11",
                "none",
                "remotejdk_11",
                "none",
                "k8-fastbuild"
            )
        )
    );
    // The tool versions reach the exec configuration as java_* and
    // tool_java_*; the target configuration only as tool_java_*.
    let (_, contents) = ws.built(&[
        LINUX,
        "--tool_java_runtime_version",
        "remotejdk_17",
        "--tool_java_language_version=17",
        "--java_language_version=17",
        "//:u",
    ]);
    assert_eq!(
        contents,
        format!(
            "{}{}",
            java_line(
                "m",
                "remotejdk_17",
                "17",
                "remotejdk_17",
                "17",
                "linux-opt-exec"
            ),
            java_line("m", "local_jdk", "17", "remotejdk_17", "17", "k8-fastbuild")
        )
    );
    // Not CoreOptions: `query` does not register them (QueryCommand.java:59-67;
    // paired capture N28 A2/query_jrv; Bazel exits 2, the native CLI 1).
    let error = ws.rejected(&["query", "--java_runtime_version=remotejdk_11", "//:m"]);
    assert!(
        error.contains("--java_runtime_version=remotejdk_11 :: Unrecognized option: --java_runtime_version=remotejdk_11"),
        "{error}"
    );
    let error = ws.rejected(&["build", "--nojava_language_version", "//:m"]);
    assert!(
        error.contains("--nojava_language_version :: Illegal use of 'no' prefix on non-boolean option: --nojava_language_version"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// N28c

const METADATA_DEFS: &str = r#"MetaInfo = provider(fields = ["text", "path"])

def _meta_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".meta")
    ctx.actions.write(out, ctx.label.name)
    return [MetaInfo(text = ctx.attr.text + " flag=" + ctx.attr._flag[FlagInfo].value + " kinds=" + ",".join([k[MetaInfo].text for k in ctx.attr.kinds]), path = out.path), DefaultInfo(files = depset([out]))]

meta = rule(implementation = _meta_impl, attrs = {
    "text": attr.string(),
    # rules_license's license_kinds: cfg = "exec" (a no-op transition in
    # the no-config configuration, which has no execution platform).
    "kinds": attr.label_list(cfg = "exec", providers = [MetaInfo]),
    "_flag": attr.label(default = "//:flag"),
})

def _bad_impl(ctx):
    fail("metadata rule %s analyzed with output root %s" % (ctx.label.name, ctx.actions.declare_file("x").path))

bad_meta = rule(implementation = _bad_impl)

FlagInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagInfo(value = ctx.build_setting_value)]

string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = ["%s: %s %s" % (str(t.label), t[MetaInfo].text, t[MetaInfo].path) for t in ctx.attr.package_metadata]
    lines.append("files=" + ",".join([f.path for f in ctx.files.package_metadata]))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl)
"#;

fn metadata_workspace(build: &str) -> Workspace {
    Workspace::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("defs.bzl", METADATA_DEFS),
        ("LICENSE", "license text\n"),
        ("BUILD.bazel", build),
    ])
}

const METADATA_BUILD: &str = r#"load(":defs.bzl", "bad_meta", "meta", "show", "string_flag")
package(default_package_metadata = [":lic", ":info"])
string_flag(name = "flag", build_setting_default = "default", applicable_licenses = [])
meta(name = "kind", text = "kind", applicable_licenses = [])
meta(name = "lic", text = "lic", kinds = [":kind"], applicable_licenses = [])
meta(name = "info", text = "info", package_metadata = [])
bad_meta(name = "bad", applicable_licenses = [])
show(name = "x")
show(name = "explicit", applicable_licenses = [":info"])
show(name = "failing", applicable_licenses = [":bad"])
show(name = "file", package_metadata = ["LICENSE"])
show(name = "none", applicable_licenses = [])
filegroup(name = "fg", srcs = ["LICENSE"])
"#;

#[test]
fn package_metadata_is_analyzed_in_the_no_config_configuration() {
    let ws = metadata_workspace(METADATA_BUILD);
    // The package default (BaseRuleClasses.packageMetadataDefault) is
    // analyzed in the no-config configuration, whose CommonOptions.EMPTY_OPTIONS
    // has no Starlark exec transition (CommonOptions.java:34-41): `//:lic`'s
    // exec edge (`kinds`, cfg = "exec"; the rule has no `license_kinds`
    // attribute) fails ExecutionTransitionFactory.create
    // (ExecutionTransitionFactory.java:92-114) at //:lic's location, before
    // any dependency, whatever -c and the Starlark flags are (paired capture
    // N28 A3/x_default, x_opt_flag, fg). A native rule's default likewise.
    for args in [&[][..], &["-c", "opt", "--//:flag=set"][..]] {
        for target in ["//:x", "//:fg"] {
            let mut all = vec!["build"];
            all.extend_from_slice(args);
            all.push(target);
            let error = ws.rejected(&all);
            assert!(
                error.contains(
                    "WS/BUILD.bazel:5:5: expected a Starlark exec transition definition, but was null"
                ),
                "{target}: {error}"
            );
        }
    }
    // Without exec edges the metadata targets analyze there: the
    // explicit alias applicable_licenses, ctx.attr.package_metadata are the
    // no-config targets (root fastbuild-noconfig, Starlark flags at their
    // defaults).
    let (_, contents) = ws.built(&["//:explicit"]);
    assert_eq!(
        contents,
        "@@//:info: info flag=default kinds= bazel-out/fastbuild-noconfig/bin/info.meta\nfiles=bazel-out/fastbuild-noconfig/bin/info.meta\n"
    );
    let (_, contents) = ws.built(&["-c", "opt", "--//:flag=set", "//:explicit"]);
    assert_eq!(
        contents,
        "@@//:info: info flag=default kinds= bazel-out/fastbuild-noconfig/bin/info.meta\nfiles=bazel-out/fastbuild-noconfig/bin/info.meta\n"
    );
    let (_, contents) = ws.built(&["//:none"]);
    assert_eq!(contents, "files=\n");
    // A failing metadata rule fails the dependent's build, analyzed in the
    // no-config configuration.
    let error = ws.rejected(&["build", "//:failing"]);
    assert!(
        error.contains(
            "metadata rule bad analyzed with output root bazel-out/fastbuild-noconfig/bin/x"
        ),
        "{error}"
    );
    // package_metadata allows no files (FileTypeSet.NO_FILE).
    let error = ws.rejected(&["build", "//:file"]);
    assert!(
        error.contains("in package_metadata attribute of show rule //:file: source file '//:LICENSE' is misplaced here (expected no files)"),
        "{error}"
    );
}

#[test]
fn package_metadata_edges_check_visibility_cycles_and_select() {
    // A metadata rule without applicable_licenses = [] receives its own
    // package default: a self-edge (AttributeProvider's pruning is
    // unreachable, docs/COMMON_RULE_ATTRIBUTES.md).
    let ws = metadata_workspace(
        r#"load(":defs.bzl", "meta", "show", "string_flag")
package(default_package_metadata = [":lic"])
string_flag(name = "flag", build_setting_default = "default", applicable_licenses = [])
meta(name = "lic", text = "lic")
show(name = "x")
"#,
    );
    let error = ws.rejected(&["build", "//:x"]);
    assert!(
        error.contains("in meta rule //:lic: cycle in dependency graph:"),
        "{error}"
    );
    assert!(error.contains(".-> //:lic ("), "{error}");
    assert!(error.contains(") [self-edge]"), "{error}");
    // Visibility of the metadata target is checked like any prerequisite.
    let ws = metadata_workspace(
        r#"load(":defs.bzl", "show")
show(name = "x", applicable_licenses = ["//other:lic"])
"#,
    );
    ws.file(
        "other/BUILD.bazel",
        "load('//:defs.bzl', 'meta')\nmeta(name = 'lic', text = 'lic', applicable_licenses = [])\n",
    );
    ws.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'show', 'string_flag')\nstring_flag(name = 'flag', build_setting_default = 'default', visibility = ['//visibility:public'], applicable_licenses = [])\nshow(name = 'x', applicable_licenses = ['//other:lic'])\n",
    );
    let error = ws.rejected(&["build", "//:x"]);
    assert!(
        error
            .contains("Visibility error:\ntarget '//other:lic' is not visible from\ntarget '//:x'"),
        "{error}"
    );
    // select() in the no-config configuration fails explicitly.
    let ws = metadata_workspace(
        r#"load(":defs.bzl", "meta", "show", "string_flag")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
string_flag(name = "flag", build_setting_default = "default", applicable_licenses = [])
meta(name = "lic", text = select({":opt": "opt", "//conditions:default": "d"}), applicable_licenses = [])
show(name = "x", applicable_licenses = [":lic"])
"#,
    );
    let error = ws.rejected(&["build", "//:x"]);
    assert!(
        error.contains("//:lic: select() in the no-config configuration (a package_metadata dependency, NoConfigTransition) is not implemented in the native profile"),
        "{error}"
    );
    // default_compatible_with / default_restricted_to stay explicit.
    let ws = metadata_workspace(
        r#"load(":defs.bzl", "show")
package(default_compatible_with = [":env"])
show(name = "x")
"#,
    );
    let error = ws.rejected(&["build", "//:x"]);
    assert!(
        error.contains("package default_compatible_with (constraint environments)"),
        "{error}"
    );
}

/// rules_license 1.0.0 (MODULE.bazel `version = "1.0.0"`): rules/license.bzl,
/// rules/license_impl.bzl, rules/package_info.bzl and rules/license_kind.bzl
/// verbatim modulo comments/docstrings; rules/providers.bzl without the
/// rules_gathering load (TransitiveLicensesInfo).
const RULES_LICENSE_PROVIDERS: &str = r#"LicenseKindInfo = provider(
    doc = """Provides information about a license_kind instance.""",
    fields = {
        "conditions": "list(string): List of conditions to be met when using this packages under this license.",
        "label": "Label: The full path to the license kind definition.",
        "long_name": "string: Human readable license name",
        "name": "string: Canonical license name",
    },
)

LicenseInfo = provider(
    doc = """Provides information about a license instance.""",
    fields = {
        "copyright_notice": "string: Human readable short copyright notice",
        "label": "Label: label of the license rule",
        "license_kinds": "list(LicenseKindInfo): License kinds ",
        "license_text": "string: The license file path",
        "package_name": "string: Human readable package name",
        "package_url": "URL from which this package was downloaded.",
        "package_version": "Human readable version string",
    },
)

PackageInfo = provider(
    doc = """Provides information about a package.""",
    fields = {
        "type": "string: How to interpret data",
        "label": "Label: label of the package_info rule",
        "package_name": "string: Human readable package name",
        "package_url": "string: URL from which this package was downloaded.",
        "package_version": "string: Human readable version string",
        "purl": "string: package url matching the purl spec (https://github.com/package-url/purl-spec)",
    },
)

ExperimentalMetadataInfo = provider(
    doc = """Generic bag of metadata.""",
    fields = {
        "type": "string: How to interpret data",
        "label": "Label: label of the metadata rule",
        "data": "String->any: Map of names to values",
    },
)
"#;

const RULES_LICENSE_LICENSE: &str = r#"load(
    "@rules_license//rules:providers.bzl",
    "LicenseKindInfo",
)
load(
    "@rules_license//rules:license_impl.bzl",
    "license_rule_impl",
)

_require_license_text_is_a_file = False

_license = rule(
    implementation = license_rule_impl,
    attrs = {
        "license_kinds": attr.label_list(
            mandatory = False,
            providers = [LicenseKindInfo],
            # This should be the null configuration, not the exec.
            cfg = "exec",
        ),
        "copyright_notice": attr.string(),
        "license_text": attr.label(
            allow_single_file = True,
            default = "LICENSE",
        ),
        "package_name": attr.string(),
        "package_url": attr.string(),
        "package_version": attr.string(),
    },
)

def license(
        name,
        license_text = "LICENSE",
        license_kind = None,
        license_kinds = None,
        copyright_notice = None,
        package_name = None,
        package_url = None,
        package_version = None,
        namespace = None,
        tags = [],
        visibility = ["//visibility:public"]):
    if license_kind:
        if license_kinds:
            fail("Can not use both license_kind and license_kinds")
        license_kinds = [license_kind]

    if _require_license_text_is_a_file:
        srcs = native.glob([license_text])
        if len(srcs) != 1 or srcs[0] != license_text:
            fail("Specified license file doesn't exist: %s" % license_text)

    if namespace:
        # buildifier: disable=print
        print("license(namespace=<str>) is deprecated.")

    _license(
        name = name,
        license_kinds = license_kinds,
        license_text = license_text,
        copyright_notice = copyright_notice,
        package_name = package_name,
        package_url = package_url,
        package_version = package_version,
        applicable_licenses = [],
        visibility = visibility,
        tags = tags,
        testonly = 0,
    )
"#;

const RULES_LICENSE_LICENSE_IMPL: &str = r#"load(
    "@rules_license//rules:providers.bzl",
    "LicenseInfo",
    "LicenseKindInfo",
)

_VERBOSITY = 0

def _debug(loglevel, msg):
    if _VERBOSITY > loglevel:
        print(msg)  # buildifier: disable=print


def license_rule_impl(ctx):
    provider = LicenseInfo(
        license_kinds = tuple([k[LicenseKindInfo] for k in ctx.attr.license_kinds]),
        copyright_notice = ctx.attr.copyright_notice,
        package_name = ctx.attr.package_name or ctx.label.package,
        package_url = ctx.attr.package_url,
        package_version = ctx.attr.package_version,
        license_text = ctx.file.license_text,
        label = ctx.label,
    )
    _debug(0, provider)
    return [provider]
"#;

const RULES_LICENSE_KIND: &str = r#"load("@rules_license//rules:providers.bzl", "LicenseKindInfo")


def _license_kind_impl(ctx):
    provider = LicenseKindInfo(
        name = ctx.attr.name,
        label = "@%s//%s:%s" % (
            ctx.label.workspace_name,
            ctx.label.package,
            ctx.label.name,
        ),
        long_name = ctx.attr.long_name,
        conditions = ctx.attr.conditions,
    )
    return [provider]

_license_kind = rule(
    implementation = _license_kind_impl,
    attrs = {
        "conditions": attr.string_list(
            mandatory = True,
        ),
        "canonical_text": attr.label(
            allow_single_file = True,
        ),
        "long_name": attr.string(),
        "url": attr.string(),
    },
)

def license_kind(name, **kwargs):
    if "conditions" not in kwargs:
        kwargs["conditions"] = []
    if "long_name" not in kwargs:
        kwargs["long_name"] = name
    _license_kind(
        name = name,
        applicable_licenses = [],
        **kwargs
    )
"#;

const RULES_LICENSE_PACKAGE_INFO: &str = r#"load(
    "@rules_license//rules:providers.bzl",
    "ExperimentalMetadataInfo",
    "PackageInfo",
)


def _package_info_impl(ctx):
    provider = PackageInfo(
        type = "package_info",
        label = ctx.label,
        package_name = ctx.attr.package_name or ctx.build_file_path.rstrip("/BUILD"),
        package_url = ctx.attr.package_url,
        package_version = ctx.attr.package_version,
        purl = ctx.attr.purl,
    )

    generic_provider = ExperimentalMetadataInfo(
        type = "package_info_alt",
        label = ctx.label,
        data = {
            "package_name": ctx.attr.package_name or ctx.build_file_path.rstrip("/BUILD"),
            "package_url": ctx.attr.package_url,
            "package_version": ctx.attr.package_version,
            "purl": ctx.attr.purl,
        },
    )
    return [provider, generic_provider]

_package_info = rule(
    implementation = _package_info_impl,
    attrs = {
        "package_name": attr.string(),
        "package_url": attr.string(),
        "package_version": attr.string(),
        "purl": attr.string(),
    },
)

def package_info(
        name,
        package_name = None,
        package_url = None,
        package_version = None,
        purl = None,
        **kwargs):
    visibility = kwargs.get("visibility") or ["//visibility:public"]
    _package_info(
        name = name,
        package_name = package_name,
        package_url = package_url,
        package_version = package_version,
        purl = purl,
        applicable_licenses = [],
        visibility = visibility,
        tags = [],
        testonly = 0,
    )
"#;

#[test]
fn rules_license_shaped_metadata_rules_analyze() {
    let ws = Workspace::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nbazel_dep(name = 'rules_license', version = '1.0.0')\nlocal_path_override(module_name = 'rules_license', path = 'rules_license')\n",
        ),
        (
            "rules_license/MODULE.bazel",
            "module(name = 'rules_license', version = '1.0.0', compatibility_level = 1)\n",
        ),
        // rules_license BUILD (root): the package's own metadata targets are
        // its default_applicable_licenses; the macros set
        // applicable_licenses = [] (no self-edge).
        (
            "rules_license/BUILD",
            r#"load("@rules_license//rules:license.bzl", "license")
load("@rules_license//rules:package_info.bzl", "package_info")
package(
    default_applicable_licenses = [":license", ":package_info"],
    default_visibility = ["//visibility:public"],
)
licenses(["notice"])
license(
    name = "license",
    license_kinds = [
        "@rules_license//licenses/spdx:Apache-2.0",
    ],
    license_text = "LICENSE",
)
package_info(
    name = "package_info",
    package_name = "rules_license",
    package_version = "1.0.0",
)
exports_files(["LICENSE", "WORKSPACE"], visibility = ["//visibility:public"])
"#,
        ),
        ("rules_license/LICENSE", "Apache License\n"),
        (
            "rules_license/licenses/spdx/BUILD",
            r#"load("@rules_license//rules:license_kind.bzl", "license_kind")
package(
    default_applicable_licenses = ["//:license"],
    default_visibility = ["//visibility:public"],
)
license_kind(
    name = "Apache-2.0",
    conditions = [],
    url = "https://spdx.org/licenses/Apache-2.0.html",
)
"#,
        ),
        (
            "rules_license/rules/BUILD",
            "package(default_applicable_licenses = ['//:license'], default_visibility = ['//visibility:public'])\n",
        ),
        ("rules_license/rules/providers.bzl", RULES_LICENSE_PROVIDERS),
        ("rules_license/rules/license.bzl", RULES_LICENSE_LICENSE),
        (
            "rules_license/rules/license_impl.bzl",
            RULES_LICENSE_LICENSE_IMPL,
        ),
        ("rules_license/rules/license_kind.bzl", RULES_LICENSE_KIND),
        (
            "rules_license/rules/package_info.bzl",
            RULES_LICENSE_PACKAGE_INFO,
        ),
        ("LICENSE", "main license\n"),
        (
            "os/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nconstraint_setting(name = 'os')\nconstraint_value(name = 'linux', constraint_setting = ':os')\n",
        ),
        (
            "p/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nplatform(name = 'linux', constraint_values = ['//os:linux'])\n",
        ),
        (
            "BUILD.bazel",
            r#"load("@rules_license//rules:license.bzl", "license")
load("@rules_license//rules:providers.bzl", "LicenseInfo", "PackageInfo")
load("//:defs.bzl", "report")
package(default_applicable_licenses = [":license"])
license(
    name = "license",
    license_kinds = ["@rules_license//licenses/spdx:Apache-2.0"],
    license_text = "LICENSE",
    package_name = "main",
)
report(name = "report", deps = ["@rules_license//:license"])
"#,
        ),
        (
            "defs.bzl",
            r#"load("@rules_license//rules:providers.bzl", "LicenseInfo", "PackageInfo")

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    for t in ctx.attr.package_metadata + [d for dep in ctx.attr.deps for d in [dep]]:
        if LicenseInfo in t:
            info = t[LicenseInfo]
            lines.append("%s license %s kinds=%s text=%s" % (t.label, info.package_name, ",".join([k.name for k in info.license_kinds]), info.license_text.path))
        if PackageInfo in t:
            lines.append("%s package_info %s" % (t.label, t[PackageInfo].package_name))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

report = rule(implementation = _report_impl, attrs = {"deps": attr.label_list()})
"#,
        ),
    ]);
    let (lines, contents) = ws.built(&[LINUX, "//:report"]);
    assert_eq!(lines, ["//:report\tbazel-out/k8-fastbuild/bin/report.txt"]);
    assert_eq!(
        contents,
        "@@//:license license main kinds=Apache-2.0 text=LICENSE\n@@rules_license+//:license license  kinds=Apache-2.0 text=external/rules_license+/LICENSE\n"
    );
    // The package_info rule and the license itself (whose own metadata is
    // pruned by the macro, not by Bazel) build directly.
    for target in ["@rules_license//:package_info", "@rules_license//:license"] {
        let output = ws.run(&["build", LINUX, target]);
        assert!(output.status.success(), "{target}: {}", errors(&output));
    }
}

const NOCONFIG_EXTRA: &str = r#"load(":defs.bzl", "MetaInfo")

def _lk_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".meta")
    ctx.actions.write(out, ctx.label.name)
    kinds = ",".join([k[MetaInfo].text + "@" + k[MetaInfo].path for k in ctx.attr.license_kinds])
    tool = ctx.file.tool.path if ctx.file.tool else "-"
    return [MetaInfo(text = ctx.attr.text + " license_kinds=" + kinds + " tool=" + tool, path = out.path), DefaultInfo(files = depset([out]))]

# A rule with an attribute named `license_kinds`: its exec edges (all of
# them) become NoConfigTransition in the no-config configuration.
meta_lk = rule(implementation = _lk_impl, attrs = {
    "text": attr.string(),
    "license_kinds": attr.label_list(cfg = "exec", providers = [MetaInfo]),
    "tool": attr.label(cfg = "exec", allow_single_file = True),
})

def _plain_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".meta")
    ctx.actions.write(out, ctx.label.name)
    return [MetaInfo(text = ctx.attr.text, path = out.path), DefaultInfo(files = depset([out]))]

plain = rule(implementation = _plain_impl, attrs = {"text": attr.string()})

def _frag_impl(ctx):
    return ctx.fragments.cpp

frag = subrule(implementation = _frag_impl, fragments = ["cpp"])

def _uses_frag_impl(ctx):
    frag()
    return [MetaInfo(text = "frag", path = "")]

uses_frag = rule(implementation = _uses_frag_impl, subrules = [frag])
"#;

const NOCONFIG_BUILD: &str = r#"load(":defs.bzl", "show", "string_flag")
load(":extra.bzl", "meta_lk", "plain", "uses_frag")
string_flag(name = "flag", build_setting_default = "default", applicable_licenses = [])
plain(name = "kind", text = "kind", applicable_licenses = [])
plain(name = "tool", text = "tool", applicable_licenses = [])
meta_lk(name = "lk", text = "lk", license_kinds = [":kind"], tool = ":tool", applicable_licenses = [])
show(name = "x", applicable_licenses = [":lk"])
genrule(name = "gen", outs = ["gen.txt"], cmd = "echo gen > $@", applicable_licenses = [])
show(name = "g_rule", applicable_licenses = [":gen"])
show(name = "g_file", applicable_licenses = [":gen.txt"])
uses_frag(name = "uf", applicable_licenses = [])
show(name = "f", applicable_licenses = [":uf"])
"#;

#[test]
fn no_config_exec_edges_follow_execution_transition_factory() {
    let ws = metadata_workspace(NOCONFIG_BUILD);
    ws.file("extra.bzl", NOCONFIG_EXTRA);
    // ExecutionTransitionFactory.java:92-111: a rule with an attribute named
    // `license_kinds` (rules_license's license()) gets NoConfigTransition
    // for its exec edges, so they stay in the no-config configuration
    // (paired capture N28 B11/v6_attr_named_license_kinds).
    let (lines, contents) = ws.built(&["-c", "opt", "//:x"]);
    assert_eq!(lines, ["//:x\tbazel-out/k8-opt/bin/x.txt"]);
    assert_eq!(
        contents,
        "@@//:lk: lk license_kinds=kind@bazel-out/fastbuild-noconfig/bin/kind.meta tool=bazel-out/fastbuild-noconfig/bin/tool.meta bazel-out/fastbuild-noconfig/bin/lk.meta\nfiles=bazel-out/fastbuild-noconfig/bin/lk.meta\n"
    );
    // A genrule always has the exec edge `$genrule_setup`
    // (BazelGenRuleRule.java:44-46): as a metadata dependency (or through
    // its output file) it fails at the genrule, before the NO_FILE check
    // (paired capture N28 B4/genrule_target, generated_file).
    for target in ["//:g_rule", "//:g_file"] {
        let error = ws.rejected(&["build", target]);
        assert!(
            error.contains(
                "WS/BUILD.bazel:8:8: expected a Starlark exec transition definition, but was null"
            ),
            "{target}: {error}"
        );
    }
    // No configuration fragment exists in the no-config configuration
    // (SubruleFragmentCollection.getValue, StarlarkSubrule.java:570-586;
    // FragmentCollectionApi.java:39-49 lists the declared fields).
    let error = ws.rejected(&["build", "//:f"]);
    assert!(
        error
            .contains("There is no configuration fragment named 'cpp'. Available fragments: 'cpp'"),
        "{error}"
    );
    // In the target configuration the subrule's fragment exists.
    let output = ws.run(&["build", "//:uf"]);
    assert!(output.status.success(), "{}", errors(&output));
}

#[test]
fn missing_default_metadata_is_an_assumed_input_file() {
    // Rule.getLabels visits the computed package_metadata default, so a
    // missing same-package default is an assumed input file that does not
    // exist (paired capture N28 B4/missing_default).
    let ws = metadata_workspace(
        "load(':defs.bzl', 'string_flag')\nstring_flag(name = 'flag', build_setting_default = 'default', applicable_licenses = [])\n",
    );
    ws.file(
        "b/BUILD.bazel",
        "load('//:defs.bzl', 'show')\npackage(default_package_metadata = [':nope'])\nshow(name = 'x')\n",
    );
    let error = ws.rejected(&["build", "//b:x"]);
    assert!(
        error.contains("WS/b/BUILD.bazel:3:5: in package_metadata attribute of show rule //b:x: rule '//b:nope' does not exist"),
        "{error}"
    );
}

/// CommonPrerequisiteValidator.validateDirectPrerequisiteVisibility
/// (analysis/CommonPrerequisiteValidator.java:181-221): an implicit
/// (private) attribute of a Starlark rule is visible if the rule's defining
/// `.bzl` package or the consumer sees it; the error names the `.bzl`
/// (:384-386). Explicit attributes (also their defaults) use the consumer.
#[test]
fn implicit_dependencies_are_checked_from_the_rule_definition() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        (
            "BUILD.bazel",
            "load('//r:defs.bzl', 'string_flag')\nstring_flag(name = 'flag', build_setting_default = 'root')\n",
        ),
        (
            "r/BUILD.bazel",
            "load(':defs.bzl', 'string_flag')\nstring_flag(name = 'rflag', build_setting_default = 'r')\n",
        ),
        (
            "f/BUILD.bazel",
            "load('//r:defs.bzl', 'string_flag')\nstring_flag(name = 'hidden', build_setting_default = 'hidden')\nstring_flag(name = 'for_sub', build_setting_default = 'for_sub', visibility = ['//sub:__pkg__'])\n",
        ),
        (
            "r/defs.bzl",
            r#"FlagInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagInfo(value = ctx.build_setting_value)]

string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr._dep[FlagInfo].value + "\n")
    return [DefaultInfo(files = depset([out]))]

def _attrs(default):
    return {"_dep": attr.label(default = default)}

use_rflag = rule(implementation = _use_impl, attrs = _attrs("//r:rflag"))
use_hidden = rule(implementation = _use_impl, attrs = _attrs("//f:hidden"))
use_for_sub = rule(implementation = _use_impl, attrs = _attrs("//f:for_sub"))

def _explicit_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.dep[FlagInfo].value + "\n")
    return [DefaultInfo(files = depset([out]))]

use_explicit = rule(implementation = _explicit_impl, attrs = {"dep": attr.label(default = "//r:rflag")})
"#,
        ),
        (
            "sub/BUILD.bazel",
            "load('//r:defs.bzl', 'use_explicit', 'use_for_sub', 'use_hidden', 'use_rflag')\nuse_rflag(name = 'a')\nuse_for_sub(name = 'b')\nuse_hidden(name = 'c')\nuse_explicit(name = 'd')\n",
        ),
    ]);
    // //r:rflag is private to //r, the rule's definition package
    // (paired capture N28 B10/sub_meta_target_config).
    let (_, contents) = ws.built(&["//sub:a"]);
    assert_eq!(contents, "r\n");
    // Not visible from //r, but from the consumer: the fallback (:210-216).
    let (_, contents) = ws.built(&["//sub:b"]);
    assert_eq!(contents, "for_sub\n");
    // Visible from neither: the error names the defining .bzl.
    let error = ws.rejected(&["build", "//sub:c"]);
    assert!(
        error.contains("WS/sub/BUILD.bazel:4:11: in use_hidden rule //sub:c: Visibility error:\ntarget '//f:hidden' is not visible from\ntarget '//r:defs.bzl'\n"),
        "{error}"
    );
    // An explicit attribute's default is checked from the consumer only.
    let error = ws.rejected(&["build", "//sub:d"]);
    assert!(
        error.contains("in use_explicit rule //sub:d: Visibility error:\ntarget '//r:rflag' is not visible from\ntarget '//sub:d'\n"),
        "{error}"
    );
}

/// The owner of an implicit dependency is the rule class's definition
/// environment label: the `.bzl` being initialized when rule() ran
/// (BzlInitThreadContext.getBzlFile; StarlarkRuleClassFunctions.java:626-631,
/// 1056-1062; CommonPrerequisiteValidator.java:133-143), not a helper `.bzl`
/// that calls rule(). Mirrors the reviewer's genuine Bazel 9.3 capture
/// (/tmp/opencode/n28/gb/ws, packages lib/pub/c2): `//c2:p` builds,
/// `//c2:l` is not visible from `//pub:defs.bzl`.
#[test]
fn implicit_dependency_owner_is_the_initialized_bzl() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name = \"main\")\n"),
        ("BUILD.bazel", ""),
        (
            "defs.bzl",
            "def _show_impl(ctx):\n    print(\"SHOW\", [str(t.label) for t in ctx.attr.package_metadata])\n    return []\nshow = rule(implementation = _show_impl)\n",
        ),
        (
            "lib/BUILD.bazel",
            "load(\"//:defs.bzl\", \"show\")\nshow(name = \"dep\", applicable_licenses = [])\n",
        ),
        (
            "lib/helpers.bzl",
            "def make(impl, attrs):\n    return rule(implementation = impl, attrs = attrs)\n",
        ),
        (
            "pub/BUILD.bazel",
            "load(\"//:defs.bzl\", \"show\")\nshow(name = \"dep\", applicable_licenses = [])\n",
        ),
        (
            "pub/defs.bzl",
            "load(\"//lib:helpers.bzl\", \"make\")\ndef _impl(ctx):\n    print(\"OK\", ctx.label)\n    return []\nr_pub = make(_impl, {\"_d\": attr.label(default = \"//pub:dep\")})\nr_lib = make(_impl, {\"_d\": attr.label(default = \"//lib:dep\")})\n",
        ),
        (
            "c2/BUILD.bazel",
            "load(\"//pub:defs.bzl\", \"r_lib\", \"r_pub\")\nr_pub(name = \"p\")\nr_lib(name = \"l\")\n",
        ),
    ]);
    // //pub:dep is visible from //pub, the initialized .bzl's package.
    let output = ws.run(&["build", "//c2:p"]);
    assert!(output.status.success(), "{}", errors(&output));
    // //lib:dep is visible from //lib (where the rule() call is), but not
    // from //pub nor //c2.
    let error = ws.rejected(&["build", "//c2:l"]);
    assert!(
        error.contains("WS/c2/BUILD.bazel:3:6: in r_lib rule //c2:l: Visibility error:\ntarget '//lib:dep' is not visible from\ntarget '//pub:defs.bzl'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility"),
        "{error}"
    );
}
