def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    mid = ctx.actions.declare_file(ctx.label.name + ".mid")
    ctx.actions.write(mid, "hello\n")
    ctx.actions.run_shell(
        inputs = [mid] + ctx.files.srcs,
        outputs = [out],
        command = "cat $1 > $2",
        arguments = [mid.path, out.path],
        env = {"ZED": "1", "ALPHA": "2"},
        mnemonic = "MyShell",
    )
    tooled = ctx.actions.declare_file(ctx.label.name + ".tooled")
    ctx.actions.run(
        inputs = ctx.files.srcs,
        tools = [ctx.executable.tool],
        outputs = [tooled],
        executable = ctx.executable.tool,
        arguments = [tooled.path],
        mnemonic = "MyTool",
    )
    other = ctx.actions.declare_file(ctx.label.name + ".other")
    ctx.actions.run(
        outputs = [other],
        executable = "/bin/true",
        arguments = ["x"],
        mnemonic = "MyRun",
    )
    return [
        DefaultInfo(files = depset([out, tooled])),
        OutputGroupInfo(compilation_outputs = depset([other]) if ctx.attr.compile else depset()),
    ]

my_rule = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "compile": attr.bool(default = True),
        "tool": attr.label(default = ":tool.sh", executable = True, cfg = "exec", allow_single_file = True),
    },
)
