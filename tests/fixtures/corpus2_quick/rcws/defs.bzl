def _env(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs = [out], command = "echo CC=${CC-unset} FOO=${FOO-unset} BAR=${BAR-unset} > " + out.path, use_default_shell_env = True)
    return [DefaultInfo(files = depset([out]))]

env = rule(implementation = _env)
