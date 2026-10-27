//! CLI: `.bzl` loading of rules_cc/rules_python/rules_go-shaped top-level
//! patterns against the native language-module globals (cc_common,
//! apple_common, py_internal, ...). Local fixtures only; no analysis-time C++.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture(tempfile::TempDir);

const ROOT_MODULE: &str = "module(name='app')
bazel_dep(name='rules_cc',version='0.2.0')
local_path_override(module_name='rules_cc',path='rules_cc')
bazel_dep(name='rules_python',version='1.0.0')
local_path_override(module_name='rules_python',path='rules_python')
bazel_dep(name='other_rules',version='1')
local_path_override(module_name='other_rules',path='other_rules')
";

/// rules_cc cc/private/cc_internal.bzl (verbatim statement at line 17).
const CC_INTERNAL: &str = "\"\"\"Gracefully resolve cc_internal.\"\"\"

# buildifier: disable=native-cc-common
cc_internal = cc_common.internal_DO_NOT_USE() if hasattr(cc_common, \"internal_DO_NOT_USE\") else struct()
";

/// rules_cc cc/private/cc_info.bzl: an empty compilation context built at
/// load time with an opaque HeaderInfo (cc_info.bzl:134).
const CC_INFO: &str = "load('//cc/private:cc_internal.bzl', _cc_internal = 'cc_internal')

CcCompilationContextInfo = provider(fields = ['headers', '_header_info'])

_EMPTY_COMPILATION_CONTEXT = CcCompilationContextInfo(
    headers = depset(),
    _header_info = _cc_internal.create_header_info(),
)

CcInfo = provider(fields = ['compilation_context'])

EMPTY = CcInfo(compilation_context = _EMPTY_COMPILATION_CONTEXT)
(type(EMPTY.compilation_context._header_info) == 'HeaderInfo') or fail('header info')
";

/// rules_cc cc/private/cc_common.bzl:735 pattern over native_cc_common.bzl.
const CC_COMMON: &str = "load('//cc/private:cc_internal.bzl', _cc_internal = 'cc_internal')
load('//cc/private/rules_impl:native_cc_common.bzl', _cc_common_internal = 'native_cc_common')

_PRIVATE_STARLARKIFICATION_ALLOWLIST = [('_builtins', ''), ('rules_cc', '')]

def _cc_toolchain_variables(*, vars):
    _cc_internal.check_private_api(allowlist = _PRIVATE_STARLARKIFICATION_ALLOWLIST)
    return _cc_internal.cc_toolchain_variables(vars = vars)

cc_common = struct(
    do_not_use_tools_cpp_compiler_present = _cc_common_internal.do_not_use_tools_cpp_compiler_present,
    get_tool_for_action = _cc_common_internal.get_tool_for_action,
    cc_toolchain_variables = _cc_toolchain_variables,
)
";

/// rules_cc rules_impl patterns: objc_common.bzl:22, objc_compilation_support
/// .bzl:24, cc_toolchain_alias.bzl, cc_test.bzl exec groups, semantics.bzl.
const RULES_IMPL: &str = "apple_toolchain = apple_common.apple_toolchain()
XcodeVersionInfo = apple_common.XcodeVersionConfig
ToolchainInfo = platform_common.ToolchainInfo
TemplateVariableInfo = platform_common.TemplateVariableInfo
ExecutionInfo = testing.ExecutionInfo
FeatureFlagInfo = config_common.FeatureFlagInfo
CC_TOOLCHAIN_TYPE = config_common.toolchain_type('//cc:toolchain_type', mandatory = False)
EXEC_GROUPS = {'test': exec_group(), 'cpp_link': exec_group(toolchains = [CC_TOOLCHAIN_TYPE])}
LIBC_TOP = configuration_field(fragment = 'cpp', name = 'libc_top')
XCODE_CONFIG = configuration_field(fragment = 'apple', name = 'xcode_config_label')
COVERAGE = configuration_field(fragment = 'coverage', name = 'output_generator')
PROTO_COMPILER = configuration_field(fragment = 'proto', name = 'proto_compiler')
(apple_toolchain.sdk_dir() == '__BAZEL_XCODE_SDKROOT__') or fail('sdk_dir')
(str(CC_TOOLCHAIN_TYPE.toolchain_type) == '@@rules_cc+//cc:toolchain_type') or fail(str(CC_TOOLCHAIN_TYPE.toolchain_type))
(proto_common_do_not_use.INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION) or fail('proto')
";

const RULES_CC_DEFS: &str = "load('//cc/private:cc_info.bzl', 'CcInfo', 'EMPTY')
load('//cc/private:cc_common.bzl', 'cc_common')
load('//cc/private/rules_impl:objc_common.bzl', 'EXEC_GROUPS', 'ToolchainInfo')

(cc_common.do_not_use_tools_cpp_compiler_present == None) or fail('compiler_present')
(type(EXEC_GROUPS['cpp_link']) == 'exec_group' and type(ToolchainInfo) == 'Provider') or fail('shapes')

def cc_probe(name):
    native.filegroup(name = name, srcs = ['root.txt'])
";

/// rules_python tools/build_defs/python/private/py_internal_renamed.bzl and
/// python/private/common.bzl top-level patterns.
const PY_INTERNAL: &str = "py_internal_renamed = py_internal
_testing = testing
_platform_common = platform_common
_coverage_common = coverage_common
PackageSpecificationInfo = getattr(py_internal, 'PackageSpecificationInfo', None)
(PackageSpecificationInfo != None) or fail('PackageSpecificationInfo')
";

/// rules_go go/platform/apple.bzl top-level pattern from an ordinary module.
const APPLE: &str = "_PLATFORMS = {
    'armv7-apple-ios': (apple_common.platform.ios_device, apple_common.platform_type.ios),
    'armv7-apple-tvos': (apple_common.platform.tvos_device, apple_common.platform_type.tvos),
    'armv7k-apple-watchos': (apple_common.platform.watchos_device, apple_common.platform_type.watchos),
    'i386-apple-ios': (apple_common.platform.ios_simulator, apple_common.platform_type.ios),
    'i386-apple-tvos': (apple_common.platform.tvos_simulator, apple_common.platform_type.tvos),
    'i386-apple-watchos': (apple_common.platform.watchos_simulator, apple_common.platform_type.watchos),
}
native_cc_common = cc_common
OK = _PLATFORMS['armv7-apple-ios'][0].name_in_plist
";

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("cc-common-loading-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file("MODULE.bazel", ROOT_MODULE);
        f.file("root.txt", "ROOT\n");
        f.file(
            "rules_cc/MODULE.bazel",
            "module(name='rules_cc',version='0.2.0')\n",
        );
        for package in ["cc", "cc/private", "cc/private/rules_impl"] {
            f.file(&format!("rules_cc/{package}/BUILD.bazel"), "");
        }
        f.file("rules_cc/cc/private/cc_internal.bzl", CC_INTERNAL);
        f.file(
            "rules_cc/cc/private/rules_impl/native_cc_common.bzl",
            "\"\"\"Native implementation of cc_common.\"\"\"\n\n# buildifier: disable=native-cc-common\nnative_cc_common = cc_common\n",
        );
        f.file("rules_cc/cc/private/cc_info.bzl", CC_INFO);
        f.file("rules_cc/cc/private/cc_common.bzl", CC_COMMON);
        f.file("rules_cc/cc/private/rules_impl/objc_common.bzl", RULES_IMPL);
        f.file("rules_cc/cc/defs.bzl", RULES_CC_DEFS);
        f.file(
            "rules_python/MODULE.bazel",
            "module(name='rules_python',version='1.0.0')\n",
        );
        f.file("rules_python/python/BUILD.bazel", "");
        f.file("rules_python/python/defs.bzl", PY_INTERNAL);
        f.file(
            "other_rules/MODULE.bazel",
            "module(name='other_rules',version='1')\n",
        );
        f.file("other_rules/BUILD.bazel", "");
        f.file("other_rules/apple.bzl", APPLE);
        f.file("other_rules/internal.bzl", CC_INTERNAL);
        f.file(
            "other_rules/removed.bzl",
            "def impl(target):\n    return target[CcInfo]\n",
        );
        f.file(
            "other_rules/python.bzl",
            "py_internal_renamed = py_internal\n",
        );
        f.file(
            "other_rules/java.bzl",
            "def get():\n    return java_common.internal_DO_NOT_USE()\nX = get()\n",
        );
        f
    }

    fn file(&self, name: &str, contents: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        // Cargo supplies this worktree's freshly built executable.
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_bazel-rs"));
        assert!(binary.is_file());
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "600s"])
            .arg(binary)
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .explicit_modules()
            .output()
            .unwrap();
        assert!(
            output.status.code().is_some(),
            "signal: {}",
            errors(&output)
        );
        assert!(
            !matches!(output.status.code(), Some(124 | 137)),
            "timeout: {}",
            errors(&output)
        );
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }

    fn build_file(&self, loads: &str) {
        self.file(
            "BUILD.bazel",
            format!(
                "{loads}\nload('@rules_cc//cc:defs.bzl', 'cc_probe')\ncc_probe(name = 'probe')\n"
            ),
        );
    }

    fn query(&self) {
        let output = self.run(&["query", "//:probe"]);
        assert!(output.status.success(), "{}", errors(&output));
        assert_eq!(output.stdout, b"//:probe\n");
    }

    fn reject(&self, marker: &str) {
        let output = self.run(&["query", "//:probe"]);
        let diagnostic = errors(&output);
        assert_eq!(output.status.code(), Some(1), "{diagnostic}");
        assert!(output.stdout.is_empty(), "partial success");
        assert!(diagnostic.contains(marker), "{marker:?}: {diagnostic}");
        assert!(!diagnostic.contains("Variable `"), "{diagnostic}");
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn rules_cc_shaped_closure_loads_with_native_language_modules() {
    let fixture = Fixture::new();
    fixture.build_file(
        "load('@rules_python//python:defs.bzl', 'py_internal_renamed')\nload('@other_rules//:apple.bzl', 'OK')",
    );
    fixture.query();
}

#[test]
fn private_api_from_another_module_is_rejected() {
    let fixture = Fixture::new();
    fixture.build_file("load('@other_rules//:internal.bzl', 'cc_internal')");
    fixture.reject("file '@@other_rules+//:internal.bzl' cannot use private API");
    fixture.build_file("load('@other_rules//:java.bzl', 'X')");
    fixture.reject("file '@@other_rules+//:java.bzl' cannot use private API");
}

#[test]
fn guarded_predeclareds_fail_at_compile_time() {
    let fixture = Fixture::new();
    fixture.build_file("load('@other_rules//:removed.bzl', 'impl')");
    fixture.reject(
        "The CcInfo symbol has been removed, add the following to your BUILD/bzl file:\n\nload(\"@rules_cc//cc/common:cc_info.bzl\", \"CcInfo\")",
    );
    fixture.build_file("load('@other_rules//:python.bzl', 'py_internal_renamed')");
    fixture.reject(
        "py_internal may only be used from one of the following repositories or prefixes: @@_builtins//, @@bazel_tools//, @@rules_python//, tools/build_defs/python",
    );
}
