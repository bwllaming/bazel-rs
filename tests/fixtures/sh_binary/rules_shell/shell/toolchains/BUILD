# Offline test replacement (not genuine): the toolchain sh_config generates
# in @local_config_shell for a Linux host whose `which bash` is /bin/bash
# (shell/private/repositories/sh_config.bzl).
load(":sh_toolchain.bzl", "sh_toolchain")

sh_toolchain(
    name = "linux_sh",
    path = "/bin/bash",
)

toolchain(
    name = "linux_sh_toolchain",
    toolchain = ":linux_sh",
    toolchain_type = "@rules_shell//shell:toolchain_type",
    target_compatible_with = [
        "@platforms//os:linux",
    ],
)
