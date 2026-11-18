load(":parent.bzl", "P", "parent_rule")

def _child_impl(ctx):
    print("RX|child_impl_before|%s|extra=%s|attrs=%s" % (ctx.label.name, ctx.attr.extra, sorted([a for a in dir(ctx.attr) if not a.startswith("_") or a in ["_tool", "_extra_tool"]])))
    providers = ctx.super()
    print("RX|child_impl_after|%s|%s" % (ctx.label.name, [type(p) for p in providers]))
    p = [x for x in providers if type(x) == "struct" or hasattr(x, "chain")]
    out = []
    for x in providers:
        if hasattr(x, "chain"):
            out.append(P(v = x.v + "+child", chain = x.chain + ["child"]))
        else:
            out.append(x)
    return out + [OutputGroupInfo(child = depset([ctx.file._extra_tool]))]

def _child_init(name, extra = "", **kwargs):
    print("RX|child_init|%s|extra=%s|keys=%s" % (name, extra, sorted(kwargs.keys())))
    return {"extra": extra + "+child_init", "msg": kwargs.get("msg", "m") + "+from_child"}

child_rule = rule(
    implementation = _child_impl,
    parent = parent_rule,
    initializer = _child_init,
    attrs = {
        "extra": attr.string(),
        "deps": attr.label_list(default = ["//ok:dep"]),
        "_extra_tool": attr.label(default = "//ok:tool.txt", allow_single_file = True),
    },
)

def _grandchild_impl(ctx):
    providers = ctx.super()
    print("RX|grandchild_impl|%s|%s" % (ctx.label.name, [x.chain for x in providers if hasattr(x, "chain")]))
    return providers

grandchild_rule = rule(
    implementation = _grandchild_impl,
    parent = child_rule,
)

def _consumer_impl(ctx):
    for d in ctx.attr.deps:
        print("RX|consumer|%s|v=%s|chain=%s|files=%s" % (d.label.name, d[P].v, d[P].chain, sorted([f.basename for f in d[DefaultInfo].files.to_list()])))
    return []

consumer = rule(_consumer_impl, attrs = {"deps": attr.label_list(providers = [P])})
