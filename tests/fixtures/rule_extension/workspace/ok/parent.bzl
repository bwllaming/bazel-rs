P = provider(fields = ["v", "chain"])

def _parent_impl(ctx):
    print("RX|parent_impl|%s|msg=%s|deps=%s|tool=%s" % (
        ctx.label.name,
        ctx.attr.msg,
        [str(d.label) for d in ctx.attr.deps],
        ctx.attr._tool.label,
    ))
    out = ctx.actions.declare_file(ctx.label.name + ".parent.txt")
    ctx.actions.write(out, "parent %s\n" % ctx.attr.msg)
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho %s\n" % ctx.attr.msg, is_executable = True)
    return [DefaultInfo(files = depset([out]), executable = exe), P(v = ctx.attr.msg, chain = ["parent"])]

def _parent_init(name, msg = "none", **kwargs):
    print("RX|parent_init|%s|msg=%s|keys=%s" % (name, msg, sorted(kwargs.keys())))
    return {"msg": msg + "+parent_init"}

parent_rule = rule(
    implementation = _parent_impl,
    initializer = _parent_init,
    executable = True,
    attrs = {
        "msg": attr.string(),
        "deps": attr.label_list(),
        "_tool": attr.label(default = "//ok:tool.txt", allow_single_file = True),
    },
)

def _plain_impl(ctx):
    return [P(v = "plain", chain = [])]

plain_rule = rule(implementation = _plain_impl)
