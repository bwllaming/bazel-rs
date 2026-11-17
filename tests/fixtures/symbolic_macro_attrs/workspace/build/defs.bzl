def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [ctx.attr.target_triple] + ["%s=%s" % (k.label, v) for k, v in ctx.attr.srcs.items()] + [t for t in ctx.attr.tags]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

_rd = rule(implementation = _impl, attrs = {
    "srcs": attr.label_keyed_string_dict(mandatory = True, allow_files = True),
    "target_triple": attr.string(),
})

def _macro_impl(name, srcs, target_triple, **kwargs):
    _rd(
        name = name,
        srcs = srcs,
        target_triple = target_triple if target_triple else select({
            "//build:on": "on-triple",
            "//conditions:default": "default-triple",
        }),
        **kwargs
    )

rd = macro(implementation = _macro_impl, inherit_attrs = _rd)
