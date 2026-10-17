load("//tc:defs.bzl", "FnInfo")
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    gen = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.write(gen, "g")
    args = ctx.actions.args()
    args.add_all([ctx.file.src, gen], map_each = ctx.attr.tc[FnInfo].fn)
    if ctx.attr.builtin:
        args.add_all([ctx.file.src, gen], map_each = str)
        args.add_joined([ctx.file.src.basename], join_with = ",", map_each = repr)
    if ctx.attr.closure:
        args.add_all([ctx.file.src], map_each = ctx.attr.tc[FnInfo].closure)
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [gen],
        arguments = [args],
        command = "echo \"$@\" > " + out.path,
    )
    roots = [ctx.file.src.root, gen.root, ctx.file.src.root]
    info = ctx.actions.declare_file(ctx.label.name + ".roots")
    ctx.actions.write(info, " ".join([
        repr([r.path for r in roots]),
        repr(roots),
        repr(len({r: 1 for r in roots})),
        type(gen.root),
        repr(dir(gen.root)),
        repr(ctx.file.src.root == ctx.file.src.root),
        repr(ctx.file.src.root == ctx.file._main.root),
    ]) + "\n")
    return [DefaultInfo(files = depset([out, info]))]
use = rule(_impl, attrs = {"tc": attr.label(), "src": attr.label(allow_single_file = True), "closure": attr.bool(), "builtin": attr.bool(), "_main": attr.label(default = "//use:a.txt", allow_single_file = True)})
