# Files reachable through configured-target values and through struct /
# provider elements of depsets in a dependency's provider are known inputs
# of its dependents (Bazel imposes no input restriction).
P = provider(fields = ["target", "items", "nested"])
Item = provider(fields = ["f", "tag"])

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".dat")
    ctx.actions.write(out, "data of %s\n" % ctx.label.name)
    return [DefaultInfo(files = depset([out]), runfiles = ctx.runfiles(files = [out]))]

gen = rule(_gen_impl)

def _producer_impl(ctx):
    f = ctx.actions.declare_file(ctx.label.name + ".item")
    ctx.actions.write(f, "item of %s\n" % ctx.label.name)
    g = ctx.actions.declare_file(ctx.label.name + ".nested")
    ctx.actions.write(g, "nested of %s\n" % ctx.label.name)
    return [P(
        target = ctx.attr.dep,
        items = depset([struct(f = f, tag = "s"), Item(f = f, tag = "i")]),
        nested = depset([struct(d = depset([g]), t = (ctx.attr.dep,))]),
    )]

producer = rule(_producer_impl, attrs = {"dep": attr.label()})

def _consumer_impl(ctx):
    p = ctx.attr.producer[P]
    target_files = p.target[DefaultInfo].files.to_list()
    item_files = [x.f for x in p.items.to_list()]
    nested_files = [f for x in p.nested.to_list() for f in x.d.to_list()]
    tuple_target_files = [f for x in p.nested.to_list() for t in x.t for f in t[DefaultInfo].files.to_list()]
    inputs = target_files + item_files + nested_files + tuple_target_files
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = inputs,
        command = "cat %s > %s" % (" ".join([f.path for f in inputs]), out.path),
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(_consumer_impl, attrs = {"producer": attr.label(providers = [P])})
