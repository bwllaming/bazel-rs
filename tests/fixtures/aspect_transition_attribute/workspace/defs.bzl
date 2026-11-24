Info = provider(fields = ["label", "inner"])

def _flip_impl(settings, _attr):
    if settings["//command_line_option:compilation_mode"] != "opt":
        return {"//command_line_option:compilation_mode": "opt"}
    return {"//command_line_option:compilation_mode": "dbg"}

flip = transition(
    implementation = _flip_impl,
    inputs = ["//command_line_option:compilation_mode"],
    outputs = ["//command_line_option:compilation_mode"],
)

def _extract_impl(target, ctx):
    inner = None
    if hasattr(ctx.rule.attr, "target") and ctx.rule.attr.target:
        dep = ctx.rule.attr.target
        dep = dep[0] if type(dep) == "list" else dep
        inner = "%s %s %s" % (type(ctx.rule.attr.target), dep.label, dep[Info].label if Info in dep else "no-info")
    return [Info(label = str(target.label), inner = inner)]

extract = aspect(implementation = _extract_impl)

def _wrap_impl(ctx):
    return [DefaultInfo()]

wrap = rule(_wrap_impl, attrs = {"target": attr.label(cfg = flip, aspects = [extract])})

def _env_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "".join(["%s: %s\n" % (w[Info].label, w[Info].inner) for w in ctx.attr.wrapped]))
    return [DefaultInfo(files = depset([out]))]

env = rule(_env_impl, attrs = {"wrapped": attr.label_list(cfg = flip, aspects = [extract])})

def _top_impl(target, ctx):
    return []

top = aspect(implementation = _top_impl, attr_aspects = ["deps"])
