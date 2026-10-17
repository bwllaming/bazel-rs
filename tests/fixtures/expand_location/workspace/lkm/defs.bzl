def _impl(ctx):
    lines = ["%s=%s" % (t.label, v) for t, v in ctx.attr.m.items()]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

lk = rule(_impl, attrs = {"m": attr.label_keyed_string_dict(allow_files = True)})
