# ctx.info_file / ctx.version_file (StarlarkRuleContext.java:994-1004): the
# workspace status action's bazel-out/{stable,volatile}-status.txt.
def _describe(f):
    return "%r %s path=%s short_path=%s basename=%s dirname=%s extension=%s is_source=%s is_directory=%s owner=%r root=%s" % (
        f, type(f), f.path, f.short_path, f.basename, f.dirname, f.extension, f.is_source, f.is_directory, f.owner, f.root.path)

def _ws_impl(ctx):
    args = ctx.actions.args()
    args.add(ctx.info_file)
    args.add_all([ctx.version_file])
    lines = [
        _describe(ctx.info_file),
        _describe(ctx.version_file),
        "eq=%s %s" % (ctx.info_file == ctx.info_file, ctx.info_file == ctx.version_file),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    argfile = ctx.actions.declare_file(ctx.label.name + ".args")
    ctx.actions.write(argfile, args)
    return [DefaultInfo(files = depset([out, argfile]))]

ws = rule(_ws_impl)

def _ws_input_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.version_file],
        command = "grep -c BUILD_TIMESTAMP %s > %s" % (ctx.version_file.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

ws_input = rule(_ws_input_impl)
