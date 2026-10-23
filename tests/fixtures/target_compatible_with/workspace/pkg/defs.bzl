"""Rules for target_compatible_with (IncompatibleTargetChecker)."""

def _writer_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [ctx.label.name]
    for dep in ctx.attr.deps:
        lines.extend([file.short_path for file in dep[DefaultInfo].files.to_list()])
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

# A Starlark rule (toolchain resolution enabled): target_compatible_with is
# checked against its target platform.
writer = rule(
    implementation = _writer_impl,
    attrs = {"deps": attr.label_list()},
)

def _tool_user_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(
        executable = ctx.executable.tool,
        arguments = [out.path],
        outputs = [out],
    )
    return [DefaultInfo(files = depset([out]))]

# Runs a tool built in the exec configuration (whose target platform is the
# execution platform).
tool_user = rule(
    implementation = _tool_user_impl,
    attrs = {"tool": attr.label(executable = True, cfg = "exec")},
)

def _tool_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(executable, "#!/bin/sh\necho tool > \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = executable)]

tool = rule(implementation = _tool_impl, executable = True)

def _setting_impl(ctx):
    return []

# A build setting: no toolchain resolution, so no compatibility check.
setting = rule(
    implementation = _setting_impl,
    build_setting = config.string(flag = True),
)
