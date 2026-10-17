"""F1: a hermetic main-repository cc_toolchain_config rule (fixed tool paths)."""

load("@rules_cc//cc:action_names.bzl", "ACTION_NAMES")
load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "action_config",
    "artifact_name_pattern",
    "env_entry",
    "env_set",
    "feature",
    "flag_group",
    "flag_set",
    "make_variable",
    "tool",
    "tool_path",
    "with_feature_set",
)
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

_COMPILE = [ACTION_NAMES.c_compile, ACTION_NAMES.cpp_compile, ACTION_NAMES.assemble, ACTION_NAMES.preprocess_assemble]
_LINK = [ACTION_NAMES.cpp_link_executable, ACTION_NAMES.cpp_link_dynamic_library, ACTION_NAMES.cpp_link_nodeps_dynamic_library]

def _impl(ctx):
    features = [
        feature(
            name = "default_compile_flags",
            enabled = True,
            flag_sets = [
                flag_set(actions = _COMPILE, flag_groups = [flag_group(flags = ["-fixture-compile", "-Wall"])]),
                flag_set(
                    actions = _COMPILE,
                    flag_groups = [flag_group(flags = ["-g0", "-O2"])],
                    with_features = [with_feature_set(features = ["opt"])],
                ),
                flag_set(
                    actions = _COMPILE,
                    flag_groups = [flag_group(flags = ["-g"])],
                    with_features = [with_feature_set(features = ["dbg"])],
                ),
                flag_set(
                    actions = _COMPILE,
                    flag_groups = [flag_group(flags = ["-fastbuild-flag"])],
                    with_features = [with_feature_set(features = ["fastbuild"])],
                ),
            ],
        ),
        feature(
            name = "default_link_flags",
            enabled = True,
            flag_sets = [flag_set(actions = _LINK, flag_groups = [flag_group(flags = ["-fixture-link", "-lm"])])],
        ),
        feature(name = "opt"),
        feature(name = "dbg"),
        feature(name = "fastbuild"),
        feature(
            name = "host",
            flag_sets = [flag_set(actions = _COMPILE, flag_groups = [flag_group(flags = ["-DFIXTURE_HOST"])])],
        ),
        feature(
            name = "nonhost",
            flag_sets = [flag_set(actions = _COMPILE, flag_groups = [flag_group(flags = ["-DFIXTURE_NONHOST"])])],
        ),
        feature(name = "supports_pic", enabled = True),
        feature(name = "supports_dynamic_linker", enabled = True),
        feature(name = "supports_start_end_lib", enabled = False),
        feature(
            name = "fixture_env",
            enabled = True,
            env_sets = [env_set(
                actions = _COMPILE + _LINK,
                env_entries = [env_entry(key = "FIXTURE_ENV", value = "on"), env_entry(key = "PWD", value = "/proc/self/cwd")],
            )],
        ),
        feature(
            name = "fixture_requested",
            flag_sets = [flag_set(actions = _COMPILE, flag_groups = [flag_group(flags = ["-requested"])])],
        ),
        feature(
            name = "fixture_disabled",
            enabled = True,
            flag_sets = [flag_set(actions = _COMPILE, flag_groups = [flag_group(flags = ["-should-be-disabled"])])],
        ),
        feature(
            name = "sysroot_flag",
            enabled = True,
            flag_sets = [flag_set(
                actions = _COMPILE + _LINK,
                flag_groups = [flag_group(flags = ["--sysroot=%{sysroot}"], expand_if_available = "sysroot")],
            )],
        ),
        feature(
            name = "parse_headers",
            flag_sets = [flag_set(actions = [ACTION_NAMES.cpp_header_parsing], flag_groups = [flag_group(flags = ["-fsyntax-only"])])],
        ),
    ]
    action_configs = [
        action_config(
            action_name = ACTION_NAMES.c_compile,
            enabled = True,
            tools = [
                tool(path = "bin/fixture-clang-dbg", with_features = [with_feature_set(features = ["dbg"])]),
                tool(path = "bin/fixture-clang"),
            ],
            implies = ["fixture_env"],
        ),
    ]
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "fixture-" + ctx.attr.cpu,
        host_system_name = "x86_64-unknown-linux-gnu",
        target_system_name = "fixture-system",
        target_cpu = ctx.attr.cpu,
        target_libc = "fixture-libc",
        compiler = "fixture-compiler",
        abi_version = "fixture-abi",
        abi_libc_version = "fixture-abi-libc",
        features = features,
        action_configs = action_configs,
        artifact_name_patterns = [artifact_name_pattern(category_name = "executable", prefix = "", extension = ".exe")],
        cxx_builtin_include_directories = [
            "/usr/include",
            "%sysroot%/usr/include",
            "%workspace%/tc/include",
            "%crosstool_top%/inc",
            "relative/inc",
            "%package(@@//tc/sub)%/pkg_inc",
            "%package(//tc)%",
        ],
        tool_paths = [
            tool_path(name = "gcc", path = "/usr/bin/fixture-gcc"),
            tool_path(name = "cpp", path = "/usr/bin/fixture-cpp"),
            tool_path(name = "ar", path = "/usr/bin/fixture-ar"),
            tool_path(name = "ld", path = "bin/fixture-ld"),
            tool_path(name = "nm", path = "/usr/bin/fixture-nm"),
            tool_path(name = "objdump", path = "/usr/bin/fixture-objdump"),
            tool_path(name = "objcopy", path = "/usr/bin/fixture-objcopy"),
            tool_path(name = "strip", path = "/usr/bin/fixture-strip"),
            tool_path(name = "gcov", path = "/usr/bin/fixture-gcov"),
            tool_path(name = "llvm-profdata", path = "/usr/bin/fixture-profdata"),
        ],
        make_variables = [
            make_variable(name = "CC_FLAGS", value = "-legacy-cc-flags"),
            make_variable(name = "FIXTURE_MAKE_VAR", value = "fixture-value"),
        ],
        builtin_sysroot = ctx.attr.sysroot,
    )

fixture_cc_toolchain_config = rule(
    implementation = _impl,
    attrs = {
        "cpu": attr.string(mandatory = True),
        "sysroot": attr.string(default = ""),
    },
    provides = [CcToolchainConfigInfo],
)
