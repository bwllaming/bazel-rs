def _impl(ctx):
    return ctx.super()

base = rule(_impl, attrs = {"d": attr.label()})
child = rule(_impl, parent = base, attrs = {"d": attr.label_list()})
