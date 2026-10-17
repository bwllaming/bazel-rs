def _tool(ctx):
    return [platform_common.ToolchainInfo(v = ctx.attr.v)]

tool = rule(_tool, attrs = {"v": attr.string()})

def _use(ctx):
    print("N45|toolchain|" + ctx.toolchains["//:tt"].v)
    return []

use = rule(_use, toolchains = ["//:tt"])

def _holder(ctx):
    print("N45|holder|%s|%s" % (ctx.label.name, [str(d.label) for d in ctx.attr.deps]))
    return []

holder = rule(_holder, attrs = {"deps": attr.label_list(), "lazy": attr.label_list()})

def _init(**kwargs):
    return kwargs

def _lifted(ctx):
    print("N45|initializer_select|%s" % ctx.attr.v)
    return []

lifted = rule(_lifted, initializer = _init, attrs = {"v": attr.string_list(), "d": attr.label_list()})
