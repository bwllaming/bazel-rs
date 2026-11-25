"""A slow test and a slow persistent-worker action (fixture)."""

def _slow_test_impl(ctx):
    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        script,
        "#!/bin/sh\nsleep 15.0732\nexit 0\n",
        is_executable = True,
    )
    return [DefaultInfo(executable = script)]

slow_test = rule(implementation = _slow_test_impl, test = True)

def _slow_work_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    args = ctx.actions.args()
    args.add(out)
    args.use_param_file("@%s", use_always = True)
    args.set_param_file_format("multiline")
    ctx.actions.run(
        executable = ctx.executable._worker,
        arguments = [args],
        outputs = [out],
        mnemonic = "SlowWork",
        execution_requirements = {
            "supports-workers": "1",
            "requires-worker-protocol": "json",
        },
        progress_message = "Working %{label}",
    )
    return [DefaultInfo(files = depset([out]))]

slow_work = rule(
    implementation = _slow_work_impl,
    attrs = {
        "_worker": attr.label(
            default = ":slow_worker.py",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
        ),
    },
)
