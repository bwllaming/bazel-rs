"""Stub sh_binary: an executable that does nothing (analysis fixture only)."""

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out)]

sh_binary = rule(
    implementation = _impl,
    executable = True,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
