def _each(x):
    print("in map_each", x)
    return x

def _r(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    args = ctx.actions.args()
    args.add_all(["a", "b"], map_each = _each)
    ctx.actions.run_shell(outputs = [out], arguments = [args], command = "echo $@ > " + out.path)
    return [DefaultInfo(files = depset([out]))]

r = rule(_r)
