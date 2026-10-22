def _grouped(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".base")
    ctx.actions.write(out, "base\n")
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(extra = depset([out]))]

grouped = rule(_grouped)

# Adds to the output group its base target already has: genuine builds and
# reports the aspect's own artifact too.
def _extra_aspect(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".aspect")
    ctx.actions.write(out, "aspect\n")
    return [OutputGroupInfo(extra = depset([out]))]

extra_aspect = aspect(_extra_aspect)

def _data_test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\nexec cat pkg/data.txt\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

data_test = rule(
    _data_test,
    test = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)
