# --incompatible_exclude_starlark_flags_from_exec_config (protobuf's
# bazel9.bazelrc) and --incompatible_enable_apple_toolchain_resolution
# (protobuf's .bazelrc): the exec configuration's Starlark flags and its
# output directory.
FlagInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagInfo(value = ctx.build_setting_value)]

string_flag = rule(
    implementation = _flag_impl,
    build_setting = config.string(flag = True),
    # skylib declares the attribute the same way.
    attrs = {"scope": attr.string(default = "universal")},
)

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "|".join([
        ctx.attr._plain[FlagInfo].value,
        ctx.attr._universal[FlagInfo].value,
        ctx.attr._target[FlagInfo].value,
        ctx.bin_dir.path,
    ]) + "\n")
    return [DefaultInfo(files = depset([out]))]

tool = rule(
    implementation = _tool_impl,
    attrs = {
        "_plain": attr.label(default = "//execflags:plain"),
        "_target": attr.label(default = "//execflags:target"),
        "_universal": attr.label(default = "//execflags:universal"),
    },
)

def _use_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.tool))]

use_tool = rule(
    implementation = _use_impl,
    attrs = {"tool": attr.label(cfg = "exec")},
)
