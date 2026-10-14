def _impl(ctx):
    out = ctx.actions.declare_file("consumed.json")
    ctx.actions.run_shell(
        inputs = [ctx.file.src],
        outputs = [out],
        command = '/bin/cat "$1" > "$2"',
        arguments = [ctx.file.src.path, out.path],
        mnemonic = "ModuleIOConsumer",
        use_default_shell_env = False,
    )
    return [DefaultInfo(files = depset([out]))]

consume_observation = rule(
    implementation = _impl,
    attrs = {"src": attr.label(allow_single_file = True, mandatory = True)},
)
