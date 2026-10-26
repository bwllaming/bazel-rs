NodeInfo = provider(fields = ["name"])
BaseInfo = provider(fields = ["files"])
MiddleInfo = provider(fields = ["files"])
ClosureInfo = provider(fields = ["files"])

def _flag(ctx):
    return []

flag = rule(implementation = _flag, build_setting = config.bool(flag = True))

def _node(ctx):
    print("RULE:" + ctx.label.name)
    return [NodeInfo(name = ctx.label.name), DefaultInfo(files = depset(ctx.files.srcs))]

node = rule(implementation = _node, attrs = {"deps": attr.label_list(), "srcs": attr.label_list(allow_files = True)}, provides = [NodeInfo])

def _base(target, ctx):
    print("BASE:" + ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".mark")
    ctx.actions.run_shell(inputs = [ctx.file._witness], outputs = [out], command = "printf '" + ctx.label.name + ":' > " + out.path + "; cat " + ctx.file._witness.path + " >> " + out.path)
    return [BaseInfo(files = depset([out]))]

base = aspect(implementation = _base, required_providers = [NodeInfo], provides = [BaseInfo], attrs = {"_witness": attr.label(default = "//:witness.txt", allow_single_file = True)})

def _middle(target, ctx):
    print("MIDDLE:" + ctx.label.name)
    return [MiddleInfo(files = target[BaseInfo].files)]

middle = aspect(implementation = _middle, required_providers = [NodeInfo], requires = [base], provides = [MiddleInfo])

def _closure(target, ctx):
    print("CLOSURE:" + ctx.label.name)
    return [ClosureInfo(files = depset(transitive = [d[ClosureInfo].files for d in ctx.rule.attr.deps] + [target[MiddleInfo].files]))]

closure = aspect(implementation = _closure, attr_aspects = ["deps"], requires = [middle], provides = [ClosureInfo])

def _collect(ctx):
    return [DefaultInfo(files = ctx.attr.target[ClosureInfo].files)]

collect = rule(implementation = _collect, attrs = {"target": attr.label(aspects = [closure])})

def _split_modes(settings, attr):
    return {"dbg": {"//command_line_option:compilation_mode": "dbg"}, "opt": {"//command_line_option:compilation_mode": "opt"}}

split_modes = transition(implementation = _split_modes, inputs = [], outputs = ["//command_line_option:compilation_mode"])

def _force_dbg(settings, attr):
    return {"//command_line_option:compilation_mode": "dbg"}

force_dbg = transition(implementation = _force_dbg, inputs = [], outputs = ["//command_line_option:compilation_mode"])
transitioned_node = rule(implementation = _node, cfg = force_dbg, attrs = {"deps": attr.label_list(), "srcs": attr.label_list(allow_files = True), "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")}, provides = [NodeInfo])

def _multi_collect(ctx):
    return [DefaultInfo(files = depset(transitive = [d[ClosureInfo].files for d in ctx.attr.targets] + [ctx.attr.exec_target[ClosureInfo].files]))]

multi_collect = rule(implementation = _multi_collect, attrs = {"targets": attr.label_list(cfg = split_modes, aspects = [closure]), "exec_target": attr.label(cfg = "exec", aspects = [closure]), "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")})
