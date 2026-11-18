def _base_impl(ctx):
    return []

def _impl(ctx):
    return []

base = rule(_base_impl)
child = rule(_impl, parent = base)
