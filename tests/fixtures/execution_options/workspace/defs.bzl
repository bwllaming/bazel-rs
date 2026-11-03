def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.label.name)]

my_toolchain = rule(implementation = _tc_impl)

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.toolchains["//:t"].name)
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use_impl, toolchains = ["//:t"])
