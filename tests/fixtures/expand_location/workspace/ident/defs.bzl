def _impl(ctx):
    d = ctx.attr.deps
    lines = [
        "deps0==lk0 %s" % (d[0] == ctx.attr.lk.keys()[0]),
        "deps0==data0 %s" % (d[0] == ctx.attr.data[0]),
        "deps0==single %s" % (d[0] == ctx.attr.single),
        "deps0==alias %s" % (d[0] == d[1]),
        "deps0==tool %s" % (d[0] == ctx.attr.tool),
        "lk[deps0] %s" % ctx.attr.lk[d[0]],
        "in %s" % (ctx.attr.data[0] in ctx.attr.lk),
        "set %d" % len({t: 1 for t in d + ctx.attr.data + [ctx.attr.single, ctx.attr.tool] + ctx.attr.lk.keys()}),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

ident = rule(_impl, attrs = {
    "deps": attr.label_list(),
    "data": attr.label_list(),
    "single": attr.label(),
    "tool": attr.label(cfg = "exec"),
    "lk": attr.label_keyed_string_dict(),
})
