def _impl(ctx):
    out = ctx.actions.declare_file("same/libgenerated.so")
    ctx.actions.write(out, "external generated fixture\n")
    return [DefaultInfo(files = depset([out]))]
generated = rule(implementation = _impl)
