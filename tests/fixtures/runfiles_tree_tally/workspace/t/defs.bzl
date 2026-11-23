def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho tool > \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(
    implementation = _tool,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _use(ctx):
    outs = []
    for i in range(ctx.attr.n):
        out = ctx.actions.declare_file("%s_%d.txt" % (ctx.label.name, i))
        ctx.actions.run_shell(
            outputs = [out],
            tools = depset([ctx.executable.tool]),
            command = "%s %s" % (ctx.executable.tool.path, out.path),
        )
        outs.append(out)
    return [DefaultInfo(files = depset(outs))]

use_exec = rule(
    implementation = _use,
    attrs = {"tool": attr.label(executable = True, cfg = "exec"), "n": attr.int(default = 1)},
)

use_target = rule(
    implementation = _use,
    attrs = {"tool": attr.label(executable = True, cfg = "target"), "n": attr.int(default = 1)},
)
