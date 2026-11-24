V = provider(fields = ["value"])

def _setting(ctx):
    return [V(value = ctx.build_setting_value)]

setting = rule(implementation = _setting, build_setting = config.string(flag = True, allow_multiple = True))

def _show(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr([ctx.attr.s[V].value, type(ctx.attr.s[V].value), ctx.attr.sel]) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show, attrs = {"s": attr.label(default = "//am:f"), "sel": attr.string()})

def _append(settings, attr):
    print("IN", settings["//am:f"])
    return {"//am:f": settings["//am:f"] + ["t"]}

append = transition(implementation = _append, inputs = ["//am:f"], outputs = ["//am:f"])

def _reset(settings, attr):
    return {"//am:f": ["d"]}

reset = transition(implementation = _reset, inputs = [], outputs = ["//am:f"])

def _single(settings, attr):
    return {"//am:f": "x"}

single = transition(implementation = _single, inputs = [], outputs = ["//am:f"])

def _fwd(ctx):
    return [DefaultInfo(files = ctx.attr.dep[0][DefaultInfo].files)]

fwd_append = rule(implementation = _fwd, attrs = {"dep": attr.label(cfg = append)})
fwd_reset = rule(implementation = _fwd, attrs = {"dep": attr.label(cfg = reset)})
fwd_single = rule(implementation = _fwd, attrs = {"dep": attr.label(cfg = single)})
