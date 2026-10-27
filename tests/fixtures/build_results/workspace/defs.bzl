def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\necho ran %s\nexit %d\n" % (ctx.label.name, ctx.attr.code), is_executable = True)
    return [DefaultInfo(executable = out)]

my_test = rule(_t, test = True, attrs = {"code": attr.int()})
my_bin = rule(_t, executable = True, attrs = {"code": attr.int()})

def _asp(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".asp.txt")
    ctx.actions.write(out, "x")
    return [OutputGroupInfo(asp_out = depset([out]))]

asp = aspect(_asp)
