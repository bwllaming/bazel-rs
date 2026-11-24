load("//:defs.bzl", "Info", "describe")

First = provider(fields = [])

def _first_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".first")
    ctx.actions.write(out, "first")
    return [First()]

first_aspect = aspect(_first_impl, provides = [First])

def _second_impl(target, ctx):
    return [Info(lines = describe(target))]

second_aspect = aspect(_second_impl, required_aspect_providers = [First])

def _collect_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        lines += dep[Info].lines
    out = ctx.actions.declare_file(ctx.label.name + ".actions")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

collect2 = rule(_collect_impl, attrs = {"deps": attr.label_list(aspects = [first_aspect, second_aspect])})
