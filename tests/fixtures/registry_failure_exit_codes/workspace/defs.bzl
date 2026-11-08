def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out)]

my_test = rule(_t, test = True)
my_bin = rule(_t, executable = True)

def _flag(ctx):
    return []

str_flag = rule(_flag, build_setting = config.string(flag = True))
