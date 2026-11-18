def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\necho %s\n" % ctx.attr.flag, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

my_bin = rule(_bin, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "flag": attr.string(),
})
