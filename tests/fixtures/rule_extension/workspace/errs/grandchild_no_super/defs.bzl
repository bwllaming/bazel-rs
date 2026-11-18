def _base_impl(ctx):
    return []

def _mid_impl(ctx):
    return []

def _impl(ctx):
    return ctx.super()

base = rule(_base_impl)
mid = rule(_mid_impl, parent = base)
child = rule(_impl, parent = mid)
