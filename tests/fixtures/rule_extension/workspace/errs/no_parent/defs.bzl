def _impl(ctx):
    return ctx.super()

child = rule(_impl)
