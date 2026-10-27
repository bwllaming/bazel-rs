print("ext top")

def _each(x):
    print("ext map_each", x)
    return x

def _er(ctx):
    print("ext rule", ctx.label)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    args = ctx.actions.args()
    args.add_all(["c"], map_each = _each)
    ctx.actions.run_shell(outputs = [out], arguments = [args], command = "echo $@ > " + out.path)
    return [DefaultInfo(files = depset([out]))]

er = rule(_er)
