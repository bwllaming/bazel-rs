def _r(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr([[str(d.label) for d in ctx.attr.deps], ctx.attr.strs, ctx.attr.ints]) + "\n")
    return [DefaultInfo(files = depset([out]))]
r = rule(implementation = _r, attrs = {"deps": attr.label_list(), "strs": attr.string_list(), "ints": attr.int_list()})
