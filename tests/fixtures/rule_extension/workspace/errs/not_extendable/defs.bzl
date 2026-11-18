def _impl(ctx):
    return []

closed = rule(_impl, extendable = False)
child = rule(_impl, parent = closed)
