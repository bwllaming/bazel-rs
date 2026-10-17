"""F3 end to end: the genuine @bazel_tools//tools/cpp:host_xcodes provider."""

load("//:defs.bzl", "info_lines")

def _host_impl(ctx):
    info = ctx.attr._xcode_config[apple_common.XcodeVersionConfig]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(info_lines("host_xcodes", info)) + "\n")
    return [DefaultInfo(files = depset([out]))]

host = rule(
    implementation = _host_impl,
    attrs = {
        # rules_go CGO_ATTRS._xcode_config / local_config_cc
        # cc_toolchain_config._xcode_config.
        "_xcode_config": attr.label(default = "@bazel_tools//tools/cpp:host_xcodes"),
    },
)
