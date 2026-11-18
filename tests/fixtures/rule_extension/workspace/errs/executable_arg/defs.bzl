def _impl(ctx):
    return ctx.super()

base = rule(_impl)
child = rule(_impl, parent = base, executable = True)
