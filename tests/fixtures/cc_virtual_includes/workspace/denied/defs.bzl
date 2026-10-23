"""ctx.actions.declare_shareable_artifact and symlink(use_exec_root_for_source)
outside the internal Starlark API allowlist."""

def _denied_impl(ctx):
    if ctx.attr.mode == "declare":
        ctx.actions.declare_shareable_artifact("denied/x.txt")
    elif ctx.attr.mode == "int_path":
        ctx.actions.declare_shareable_artifact(1)
    elif ctx.attr.mode == "symlink":
        out = ctx.actions.declare_file(ctx.label.name + ".h")
        ctx.actions.symlink(
            output = out,
            target_file = ctx.file.src,
            use_exec_root_for_source = False,
        )
        return [DefaultInfo(files = depset([out]))]
    return []

denied = rule(
    implementation = _denied_impl,
    attrs = {
        "mode": attr.string(),
        "src": attr.label(allow_single_file = True),
    },
)
