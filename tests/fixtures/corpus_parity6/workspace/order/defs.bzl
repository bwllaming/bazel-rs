def _broken_test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.run_shell(
        outputs = [exe],
        command = "exit 1",
    )
    return [DefaultInfo(executable = exe)]

broken_test = rule(
    implementation = _broken_test,
    test = True,
)
