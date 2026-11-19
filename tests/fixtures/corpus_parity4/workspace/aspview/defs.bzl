# B33: an aspect propagating over `deps` only, on a target naming one
# dependency in `deps` and `exports`: ctx.rule.attr.deps sees the
# aspect-applied view, ctx.rule.attr.exports the plain target.
P = provider(fields = ["n"])

def _a_impl(target, ctx):
    n = 1
    for d in ctx.rule.attr.deps:
        n += d[P].n
    seen = [P in e for e in ctx.rule.attr.exports]
    return [P(n = "%s %s" % (n, seen) if ctx.rule.attr.exports else n)]

a = aspect(implementation = _a_impl, attr_aspects = ["deps"])

def _r_impl(ctx):
    return []

r = rule(implementation = _r_impl, attrs = {"deps": attr.label_list(), "exports": attr.label_list()})

def _top_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, str(ctx.attr.dep[P].n) + "\n")
    return [DefaultInfo(files = depset([out]))]

top = rule(implementation = _top_impl, attrs = {"dep": attr.label(aspects = [a])})
