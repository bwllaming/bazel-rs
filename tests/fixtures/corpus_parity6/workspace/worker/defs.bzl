def _produce(ctx):
    listing = ctx.actions.declare_file(ctx.label.name + ".listing")
    named = ctx.actions.declare_file(ctx.label.name + ".named")
    ctx.actions.run_shell(
        outputs = [listing, named],
        command = "echo %s > %s && echo sibling of the listing > %s" % (
            named.path,
            listing.path,
            named.path,
        ),
    )
    return [DefaultInfo(files = depset([listing]))]

produce = rule(implementation = _produce)

def _consume(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    args = ctx.actions.args()
    args.add(out)
    args.add(ctx.file.listing)
    args.use_param_file("@%s", use_always = True)
    args.set_param_file_format("multiline")
    ctx.actions.run(
        outputs = [out],
        inputs = [ctx.file.listing],
        executable = ctx.executable._worker,
        arguments = [args],
        mnemonic = "SiblingReader",
        execution_requirements = {
            "requires-worker-protocol": "json",
            "supports-multiplex-workers": "1" if ctx.attr.multiplex else "0",
            "supports-workers": "1",
        },
    )
    return [DefaultInfo(files = depset([out]))]

consume = rule(
    implementation = _consume,
    attrs = {
        "listing": attr.label(allow_single_file = True),
        "multiplex": attr.bool(),
        "_worker": attr.label(
            default = ":worker.py",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
        ),
    },
)
