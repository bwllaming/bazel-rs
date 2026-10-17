# //test is on BuiltinRestriction's default allowlist (short_paths = True).
def _impl(ctx):
    for out in [ctx.outputs.out] + ctx.outputs.outs:
        if out:
            ctx.actions.write(out, "")
    m = ctx.attr.mode
    if m == "dup":
        ctx.expand_location("x", ctx.attr.data + ctx.attr.data)
    elif m == "none":
        ctx.expand_location("x", None)
    elif m == "type":
        ctx.expand_location("x", ["a"])
    targets = [] if m == "notargets" else ctx.attr.data + ctx.attr.extra
    lines = []
    for s in ctx.attr.inputs:
        lines.append(s + " => " + ctx.expand_location(s, targets, short_paths = ctx.attr.short))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

loc = rule(_impl, attrs = {
    "inputs": attr.string_list(),
    "mode": attr.string(),
    "short": attr.bool(),
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(allow_files = True),
    "implementation_deps": attr.label_list(allow_files = True),
    "tools": attr.label_list(allow_files = True, cfg = "exec"),
    "data": attr.label_list(allow_files = True),
    "extra": attr.label_list(allow_files = True),
    "out": attr.output(),
    "outs": attr.output_list(),
})

def loc_common(name, **kwargs):
    loc(
        name = name,
        srcs = [":fg", "a.txt", ":al"],
        deps = [":tool", ":gen", "b.txt", ":empty"],
        implementation_deps = [":fg2"],
        tools = ["d.txt"],
        data = ["c.txt", ":tool"],
        extra = ["@bazel_tools//tools/test:collect_coverage", ":fg2"],
        **kwargs
    )
