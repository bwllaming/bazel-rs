A1Info = provider(fields = ["seen"])

def _a1_impl(target, ctx):
    deps = getattr(ctx.rule.attr, "deps", [])
    print("A a1 on %s target=%s rule.attr.deps=%s" % (target.label, target, [str(d) for d in deps]))
    return [A1Info(seen = str(target))]

a1 = aspect(implementation = _a1_impl, attr_aspects = ["deps"])

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _one_impl(ctx):
    for dep in ctx.attr.deps:
        print("M %s: dep=%s label=%s a1=%s loc=%s" % (ctx.label.name, dep, dep.label, dep[A1Info].seen, ctx.expand_location(ctx.attr.loc, ctx.attr.deps)))
    return [DefaultInfo()]

one = rule(implementation = _one_impl, attrs = {"deps": attr.label_list(aspects = [a1]), "loc": attr.string()})
