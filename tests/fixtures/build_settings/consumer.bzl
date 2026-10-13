load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

def _consume(ctx):
    lines = []
    for dep in ctx.attr.deps:
        value = dep[BuildSettingInfo].value
        text = "|".join(value) if type(value) == "list" else str(value)
        lines.append(dep.label.name + "=" + text)
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([output]))]

consume = rule(implementation = _consume, attrs = {"deps": attr.label_list()})
