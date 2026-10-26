"""//command_line_option:extra_execution_platforms in Starlark transitions.

`uv_transition` is rules_uv's transition_to_target
(uv/private/transition_to_target.bzl): the transitioned configuration's
execution platforms are the target platforms first, then the host platform
(RegisteredExecutionPlatformsFunction, PlatformKeys). Every rule writes
`<name>.out` naming its toolchain and the paths of its exec dependency's
output, so the output directories (ST- hashes, exec platform names) and
the selected execution platform are pinned.
"""

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(tc_name = ctx.attr.tc_name)]

my_toolchain = rule(_tc_impl, attrs = {"tc_name": attr.string()})

def _uv_transition_impl(settings, _attr):
    return {
        "//command_line_option:extra_execution_platforms": [
            str(platform)
            for platform in settings["//command_line_option:platforms"]
        ],
    }

uv_transition = transition(
    implementation = _uv_transition_impl,
    inputs = ["//command_line_option:platforms"],
    outputs = ["//command_line_option:extra_execution_platforms"],
)

def _read_transition_impl(settings, attr):
    value = settings["//command_line_option:extra_execution_platforms"]
    print("READ {} {}".format(type(value), value))
    if attr.set_none:
        return {"//command_line_option:extra_execution_platforms": None}
    return {"//command_line_option:extra_execution_platforms": value + ["//p:bar"]}

read_transition = transition(
    implementation = _read_transition_impl,
    inputs = ["//command_line_option:extra_execution_platforms"],
    outputs = ["//command_line_option:extra_execution_platforms"],
)

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "leaf\n")
    return [DefaultInfo(files = depset([out]))]

leaf = rule(_leaf_impl)

def _write(ctx, lines):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return out

def _leaf_paths(ctx):
    return ["leaf=" + f.path for f in ctx.files._exec_leaf]

def _tool_impl(ctx):
    out = _write(ctx, ["tc=" + ctx.toolchains["//tc:t"].tc_name] + _leaf_paths(ctx))
    return [DefaultInfo(files = depset([out]))]

tool = rule(
    _tool_impl,
    attrs = {"_exec_leaf": attr.label(default = "//:exec_leaf", cfg = "exec")},
    toolchains = ["//tc:t"],
)

def _plain_impl(ctx):
    out = _write(ctx, _leaf_paths(ctx))
    return [DefaultInfo(files = depset([out]))]

plain = rule(
    _plain_impl,
    attrs = {"_exec_leaf": attr.label(default = "//:exec_leaf", cfg = "exec")},
)

def _pip_impl(ctx):
    files = [f.path for f in ctx.files._tool + ctx.files._plain]
    out = _write(ctx, files)
    return [DefaultInfo(files = depset([out], transitive = [d[DefaultInfo].files for d in ctx.attr._tool + ctx.attr._plain]))]

pip = rule(
    _pip_impl,
    attrs = {
        "_tool": attr.label(default = "//:tool", cfg = uv_transition),
        "_plain": attr.label(default = "//:plain", cfg = uv_transition),
    },
)

def _reader_impl(ctx):
    files = [f.path for f in ctx.files._tool]
    out = _write(ctx, files)
    return [DefaultInfo(files = depset([out], transitive = [d[DefaultInfo].files for d in ctx.attr._tool]))]

reader = rule(
    _reader_impl,
    attrs = {
        "set_none": attr.bool(),
        "_tool": attr.label(default = "//:tool", cfg = read_transition),
    },
)
