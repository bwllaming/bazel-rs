def _impl(ctx):
    outs = ctx.outputs.outs + [ctx.outputs.out]
    for f in outs:
        ctx.actions.write(f, f.short_path + "\n")
    return [DefaultInfo(files = depset(outs))]

hashes = rule(implementation = _impl, attrs = {
    "outs": attr.output_list(),
    "out": attr.output(),
})
FISH = Label("//pkg:fish.md5")
