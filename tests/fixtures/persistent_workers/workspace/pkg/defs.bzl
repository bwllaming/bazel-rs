"""A Starlark rule whose action supports persistent workers (fixture)."""

def _work_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    args = ctx.actions.args()
    args.add("--in", ctx.file.src)
    args.add("--out", out)
    args.add("--mode", ctx.attr.mode)
    if ctx.attr.extra:
        args.add("--extra", ctx.attr.extra)
    if ctx.attr.flagfile:
        args.use_param_file("@%s", use_always = True)
        args.set_param_file_format("multiline")
    requirements = {}
    requirements.update(ctx.attr.requirements)
    ctx.actions.run(
        executable = ctx.executable._worker,
        arguments = ["--startup_flag=" + ctx.attr.startup, args],
        inputs = [ctx.file.src] + ctx.files.data,
        outputs = [out],
        mnemonic = ctx.attr.mnemonic,
        execution_requirements = requirements,
        env = {"TRACE": ctx.var.get("trace", "")},
        progress_message = "Working %{label}",
    )
    return [DefaultInfo(files = depset([out]))]

work = rule(
    implementation = _work_impl,
    attrs = {
        "src": attr.label(allow_single_file = True, mandatory = True),
        "data": attr.label_list(allow_files = True),
        "mode": attr.string(default = "upper"),
        "extra": attr.string(),
        "mnemonic": attr.string(default = "FixtureWork"),
        "startup": attr.string(default = "a"),
        "flagfile": attr.bool(default = True),
        "requirements": attr.string_dict(),
        "_worker": attr.label(
            default = ":worker.py",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
        ),
    },
)

def _tree_impl(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run_shell(
        outputs = [out],
        command = "mkdir -p $1/sub && echo one > $1/one.txt && echo two > $1/sub/two.txt",
        arguments = [out.path],
    )
    return [DefaultInfo(files = depset([out]))]

tree = rule(implementation = _tree_impl)
