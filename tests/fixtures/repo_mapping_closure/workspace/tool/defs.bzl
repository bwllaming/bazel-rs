def _gen(ctx):
    ctx.actions.run(
        executable = ctx.executable.maker,
        arguments = [ctx.outputs.out.path],
        outputs = [ctx.outputs.out],
    )

gen = rule(_gen, attrs = {
    "out": attr.output(),
    "maker": attr.label(cfg = "exec", executable = True, allow_single_file = True, default = "@maker//:maker.sh"),
})
