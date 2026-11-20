def _broken_binary(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.run_shell(
        outputs = [exe],
        command = "echo the link fails >&2; exit 1",
    )
    return [DefaultInfo(
        executable = exe,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

broken_binary = rule(
    implementation = _broken_binary,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)
