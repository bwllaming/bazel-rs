NodeInfo = provider()
BaseInfo = provider()
MiddleInfo = provider()
ClosureInfo = provider()
OtherInfo = provider()

def _node(ctx):
    return [NodeInfo(), DefaultInfo(files = depset(ctx.files.srcs))]

node = rule(implementation = _node, attrs = {"deps": attr.label_list(), "srcs": attr.label_list(allow_files = True)}, provides = [NodeInfo])

def _base(target, ctx):
    return [BaseInfo()]

base = aspect(implementation = _base, required_providers = [NodeInfo], provides = [BaseInfo], toolchains = ["//:tc_type"], attrs = {
    "_witness": attr.label(default = "//:witness.txt", allow_single_file = True),
    "_choice": attr.label(default = "//:choice"),
    "_tool": attr.label(default = "//:tool", cfg = "exec"),
    "_exec_source": attr.label(default = "//:exec-source.txt", cfg = "exec", allow_single_file = True),
})

def _middle(target, ctx):
    return [MiddleInfo()]

middle = aspect(implementation = _middle, required_providers = [NodeInfo], requires = [base], provides = [MiddleInfo])

def _closure(target, ctx):
    return [ClosureInfo()]

closure = aspect(implementation = _closure, attr_aspects = ["deps"], requires = [middle], provides = [ClosureInfo])

def _other(target, ctx):
    return [OtherInfo()]

other = aspect(implementation = _other, attrs = {"_other": attr.label(default = "//:other-aspect.txt", allow_single_file = True)})
decorated = rule(implementation = _node, attrs = {"deps": attr.label_list(aspects = [other]), "srcs": attr.label_list(allow_files = True)}, provides = [NodeInfo])

def _collect(ctx):
    return []

collect = rule(implementation = _collect, attrs = {"target": attr.label(aspects = [closure])})
exec_collect = rule(implementation = _collect, attrs = {"target": attr.label(cfg = "exec", aspects = [closure])})

def _tc(ctx):
    return [platform_common.ToolchainInfo()]

tc_impl = rule(implementation = _tc, attrs = {"srcs": attr.label_list(allow_files = True)})
