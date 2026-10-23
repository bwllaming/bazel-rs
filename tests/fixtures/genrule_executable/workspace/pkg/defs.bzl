def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    for name in ["exe_dep", "plain_dep"]:
        dep = getattr(ctx.attr, name)
        if dep:
            executable = dep[DefaultInfo].files_to_run.executable
            lines.append("%s.files_to_run.executable=%s" % (name, executable.short_path if executable else None))
            lines.append("%s.files=%s" % (name, [f.short_path for f in dep[DefaultInfo].files.to_list()]))
    if ctx.attr.tool:
        lines.append("ctx.executable.tool=" + ctx.executable.tool.short_path)
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

# Reads an executable genrule as a dependency and as an executable tool.
probe = rule(
    implementation = _probe_impl,
    attrs = {
        "exe_dep": attr.label(),
        "plain_dep": attr.label(),
        "tool": attr.label(cfg = "exec", executable = True),
    },
)

def _run_tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        tools = [ctx.attr.tool[DefaultInfo].files_to_run],
        command = "%s from-starlark > %s" % (ctx.executable.tool.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

# Runs an executable genrule's output through its FilesToRunProvider.
run_tool = rule(
    implementation = _run_tool_impl,
    attrs = {"tool": attr.label(cfg = "exec", executable = True)},
)

def _single_file_tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "ctx.executable.tool=%s\nctx.file.tool=%s\n" % (
        ctx.executable.tool.short_path,
        ctx.file.tool.short_path,
    ))
    return [DefaultInfo(files = depset([out]))]

# build_stack_rules_proto's protoc toolchain rule attribute.
single_file_tool = rule(
    implementation = _single_file_tool_impl,
    attrs = {"tool": attr.label(allow_single_file = True, executable = True, cfg = "exec")},
)
