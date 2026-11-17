def _impl(ctx):
    print("VALUE %s %s" % (ctx.label.name, ctx.attr.v))
    return []

probe = rule(implementation = _impl, attrs = {"v": attr.string()})
