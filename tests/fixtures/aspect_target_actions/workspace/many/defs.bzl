def _impl(ctx):
    # The actions of 40 targets, one after the other.
    counts = [len(dep.actions) for dep in ctx.attr.deps]
    total = 0
    for n in counts:
        total += n
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%d %d\n" % (len(counts), total))
    return [DefaultInfo(files = depset([out]))]

count_all = rule(_impl, attrs = {"deps": attr.label_list()})

Seen = provider(fields = ["n"])

def _seen_impl(target, ctx):
    total = 0
    for dep in ctx.rule.attr.deps:
        total += len(dep.actions)
    return [Seen(n = total)]

seen_aspect = aspect(_seen_impl)

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%d\n" % ctx.attr.dep[Seen].n)
    return [DefaultInfo(files = depset([out]))]

report = rule(_report_impl, attrs = {"dep": attr.label(aspects = [seen_aspect])})
