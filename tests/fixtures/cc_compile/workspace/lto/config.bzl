# OSS corpus B32: a toolchain with rules_cc's thin_lto feature (the flag
# sets of rules_cc's own test toolchain, tests/cc/testutil/toolchains), so
# a statically linked binary gets the LTO indexing and backend actions.
load("@rules_cc//cc:action_names.bzl", "ACTION_NAMES")
load("@rules_cc//cc:cc_toolchain_config_lib.bzl", "feature", "flag_group", "flag_set", "tool_path")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

_THIN_LTO = feature(
    name = "thin_lto",
    flag_sets = [
        flag_set(
            actions = [
                ACTION_NAMES.cpp_link_executable,
                ACTION_NAMES.cpp_link_dynamic_library,
                ACTION_NAMES.cpp_link_nodeps_dynamic_library,
                ACTION_NAMES.cpp_link_static_library,
            ],
            flag_groups = [
                flag_group(
                    expand_if_available = "thinlto_param_file",
                    flags = ["thinlto_param_file=%{thinlto_param_file}"],
                ),
            ],
        ),
        flag_set(
            actions = [ACTION_NAMES.c_compile, ACTION_NAMES.cpp_compile],
            flag_groups = [
                flag_group(flags = ["-flto=thin"]),
                flag_group(
                    flags = ["lto_indexing_bitcode=%{lto_indexing_bitcode_file}"],
                    expand_if_available = "lto_indexing_bitcode_file",
                ),
            ],
        ),
        flag_set(
            actions = [ACTION_NAMES.lto_indexing],
            flag_groups = [
                flag_group(
                    flags = [
                        "param_file=%{thinlto_indexing_param_file}",
                        "prefix_replace=%{thinlto_prefix_replace}",
                    ],
                ),
                flag_group(
                    flags = ["object_suffix_replace=%{thinlto_object_suffix_replace}"],
                    expand_if_available = "thinlto_object_suffix_replace",
                ),
                flag_group(
                    flags = ["thinlto_merged_object_file=%{thinlto_merged_object_file}"],
                    expand_if_available = "thinlto_merged_object_file",
                ),
            ],
        ),
        flag_set(
            actions = [ACTION_NAMES.lto_backend],
            flag_groups = [
                flag_group(
                    flags = [
                        "thinlto_index=%{thinlto_index}",
                        "thinlto_output_object_file=%{thinlto_output_object_file}",
                        "thinlto_input_bitcode_file=%{thinlto_input_bitcode_file}",
                    ],
                ),
                flag_group(
                    flags = ["fdo=%{fdo_profile_path}"],
                    expand_if_available = "fdo_profile_path",
                ),
            ],
        ),
    ],
)

def _impl(ctx):
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "lto",
        host_system_name = "local",
        target_system_name = "local",
        target_cpu = "k8",
        target_libc = "unknown",
        compiler = "gcc",
        abi_version = "unknown",
        abi_libc_version = "unknown",
        features = [
            _THIN_LTO,
            feature(name = "supports_start_end_lib", enabled = True),
            feature(name = "supports_pic", enabled = True),
            feature(name = "thin_lto_linkstatic_tests_use_shared_nonlto_backends"),
        ],
        tool_paths = [
            tool_path(name = name, path = "/usr/bin/" + name)
            for name in ["gcc", "ar", "ld", "cpp", "nm", "objdump", "strip"]
        ],
    )

config = rule(implementation = _impl, provides = [CcToolchainConfigInfo])
