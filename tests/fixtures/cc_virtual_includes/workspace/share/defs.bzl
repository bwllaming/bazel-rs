"""Actions.canBeShared within one target: two registrations writing one output."""

def _tool_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(executable, "#!/bin/sh\necho \"$2\" > \"$1\"\n", is_executable = True)
    data = ctx.actions.declare_file(ctx.label.name + ".data")
    ctx.actions.write(data, "data\n")
    return [DefaultInfo(executable = executable, runfiles = ctx.runfiles(files = [data]))]

# An executable with runfiles: a FilesToRunProvider with runfiles support.
tool = rule(implementation = _tool_impl, executable = True)

def _run_twice_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    for i in range(2):
        if ctx.attr.mode == "shell":
            ctx.actions.run_shell(
                outputs = [out],
                command = "echo same > \"$1\"",
                arguments = [out.path],
                mnemonic = "Twice",
                progress_message = "run %{label} " + str(i),
            )
        elif ctx.attr.mode == "tool_same":
            ctx.actions.run(
                executable = ctx.executable.tool,
                arguments = [out.path, "same"],
                outputs = [out],
                mnemonic = "Twice",
                progress_message = "run %{label} " + str(i),
            )
        elif ctx.attr.mode == "tool_diff":
            ctx.actions.run(
                executable = ctx.executable.tool,
                arguments = [out.path, "call " + str(i)],
                outputs = [out],
                mnemonic = "Twice",
            )
        else:
            fail("unknown mode " + ctx.attr.mode)
    return [DefaultInfo(files = depset([out]))]

run_twice = rule(
    implementation = _run_twice_impl,
    attrs = {
        "mode": attr.string(),
        "tool": attr.label(executable = True, cfg = "exec"),
    },
)

def _args_twice_impl(ctx):
    directory = ctx.actions.declare_directory(ctx.label.name + "_dir")
    ctx.actions.run_shell(
        outputs = [directory],
        command = "mkdir -p \"$1\" && echo x > \"$1/f\"",
        arguments = [directory.path],
    )
    out = ctx.actions.declare_file(ctx.label.name + ".args")
    for _ in range(2):
        args = ctx.actions.args()
        args.add_all([directory])
        ctx.actions.write(out, args)
    return [DefaultInfo(files = depset([out]))]

# Args content expanding a tree artifact, written twice.
args_twice = rule(implementation = _args_twice_impl)
