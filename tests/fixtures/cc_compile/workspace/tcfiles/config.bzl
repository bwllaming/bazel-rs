# OSS corpus B45: a toolchain whose file groups differ, so a compile's
# toolchain inputs show which group it stages.
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc:cc_toolchain_config_lib.bzl", "tool_path")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

def _impl(ctx):
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "tcfiles",
        host_system_name = "local",
        target_system_name = "local",
        target_cpu = "k8",
        target_libc = "unknown",
        compiler = "gcc",
        abi_version = "unknown",
        abi_libc_version = "unknown",
        tool_paths = [
            tool_path(name = name, path = "/usr/bin/" + name)
            for name in ["gcc", "ar", "ld", "cpp", "nm", "objdump", "strip"]
        ],
    )

config = rule(implementation = _impl, provides = [CcToolchainConfigInfo])
