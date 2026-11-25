def _impl(ctx):
    # Target.actions first, the target platform after (B61: the retry for
    # the views, then the one for the deferred target platform).
    n = len(ctx.attr.dep.actions)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%d cpu=%s has=%s\n" % (
        n,
        ctx.var["TARGET_CPU"],
        ctx.target_platform_has_constraint(ctx.attr._value[platform_common.ConstraintValueInfo]),
    ))
    return [DefaultInfo(files = depset([out]))]

platform_reader = rule(_impl, attrs = {
    "dep": attr.label(),
    "_value": attr.label(default = "//platform_reader:v"),
})
