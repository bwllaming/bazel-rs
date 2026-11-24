def _count_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".count")
    ctx.actions.write(out, "%d %s\n" % (len(ctx.attr.dep.actions), [a.mnemonic for a in ctx.attr.dep.actions]))
    return [DefaultInfo(files = depset([out]))]

count = rule(_count_impl, attrs = {"dep": attr.label()})
