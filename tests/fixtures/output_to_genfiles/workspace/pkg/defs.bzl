def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x\n")
    print("ROOTS %s %s %s %s" % (ctx.label.name, ctx.genfiles_dir.path, ctx.bin_dir.path, out.root.path))
    return [DefaultInfo(files = depset([out, ctx.outputs.o]))]

def _impl2(ctx):
    ctx.actions.write(ctx.outputs.o, "y\n")
    return _impl(ctx)

gen = rule(implementation = _impl2, output_to_genfiles = True, attrs = {"o": attr.output()})
