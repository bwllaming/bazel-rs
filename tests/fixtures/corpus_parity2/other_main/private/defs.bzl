def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr._hidden)
    return [DefaultInfo(files = depset([out]))]

def _init(name, **kwargs):
    return {"_hidden": "set by the initializer of " + name}

private_init = rule(
    implementation = _impl,
    initializer = _init,
    attrs = {"_hidden": attr.string(default = "default")},
)
