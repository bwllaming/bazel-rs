def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, " ".join(ctx.attr.opts + [str(d.label) for d in ctx.attr.deps]) + "\n")
    return [DefaultInfo(files = depset([out]))]

r = rule(implementation = _impl, attrs = {
    "deps": attr.label_list(),
    "opts": attr.string_list(),
})

# B33: a symbolic macro given a label by its caller: the inner target's
# dependency is checked against the caller's location (visibility
# delegation, CommonPrerequisiteValidator.isVisibleToConsumer).
def _wrap_impl(name, visibility, deps):
    r(name = name + "_inner", deps = deps, visibility = visibility)

wrap = macro(implementation = _wrap_impl, attrs = {
    "deps": attr.label_list(),
})

# The label is the macro's own, not the caller's: checked against the
# macro's definition package.
def _fixed_impl(name, visibility):
    r(name = name + "_inner", deps = [Label("//vis/bad:private_dep")], visibility = visibility)

fixed = macro(implementation = _fixed_impl)
