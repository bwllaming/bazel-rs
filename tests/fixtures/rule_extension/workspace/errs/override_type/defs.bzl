def _impl(ctx):
    return ctx.super()

base = rule(_impl, attrs = {"s": attr.string()})
child = rule(_impl, parent = base, attrs = {"s": attr.string(default = "x")})
