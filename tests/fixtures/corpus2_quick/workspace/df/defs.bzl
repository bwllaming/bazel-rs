def _r(ctx):
    f = ctx.actions.declare_file(ctx.attr.name_)
    ctx.actions.write(f, "x")
    return [DefaultInfo(files = depset([f]))]
r = rule(implementation = _r, attrs = {"name_": attr.string()})
