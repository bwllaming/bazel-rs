def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\necho ran-bin\n", is_executable = True)
    return [DefaultInfo(executable = exe)]

my_bin = rule(_bin, executable = True)

def _test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho ran-test\n", is_executable = True)
    return [DefaultInfo(executable = exe)]

my_test = rule(_test, test = True)
