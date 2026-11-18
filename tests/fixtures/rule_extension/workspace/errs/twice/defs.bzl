def _base_impl(ctx):
    return []

def _impl(ctx):
    ctx.super()
    return ctx.super()

base = rule(_base_impl)
child = rule(_impl, parent = base)
