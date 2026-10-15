"""An ordinary analyzable rule that also carries accepted, unexercised declarations."""

SimpleInfo = provider(fields = ["text"])

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output = out, content = ctx.attr.content + ":" + ctx.attr.mode)
    return [DefaultInfo(files = depset([out])), SimpleInfo(text = ctx.attr.content)]

simple_write = rule(
    implementation = _impl,
    attrs = {
        "content": attr.string(mandatory = True),
        "mode": attr.string(default = "a", values = ["a", "b"]),
        "counts": attr.int_list(),
        # Declared but empty: not exercised, so analysis still runs natively.
        "tools": attr.label_list(cfg = "exec"),
    },
    fragments = ["cpp"],
    provides = [SimpleInfo],
)
