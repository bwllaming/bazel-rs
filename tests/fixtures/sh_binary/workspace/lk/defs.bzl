P = provider(fields = ["v"])

def _prov_impl(ctx):
    return [P(v = ctx.label.name), DefaultInfo(files = depset(ctx.files.srcs))]

prov = rule(_prov_impl, attrs = {"srcs": attr.label_list(allow_files = True)})

def _lk_impl(ctx):
    lines = []
    for target, value in ctx.attr.m.items():
        lines.append("%s %s %s %s" % (type(target), target.label, value, [f.short_path for f in target.files.to_list()]))
    lines.append("files " + " ".join([f.short_path for f in ctx.files.m]))
    lines.append("type " + type(ctx.attr.m))
    lines.append("len %d" % len(ctx.attr.m))
    first = ctx.attr.m.keys()[0] if ctx.attr.m else None
    lines.append("lookup " + (ctx.attr.m[first] if first else "none"))
    lines.append("in " + str(first in ctx.attr.m))
    lines.append("hash_dict " + str(len({t: 1 for t in ctx.attr.m})))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

lk = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict(allow_files = True)})
lk_rules = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict(allow_rules = ["prov"])})
lk_prov = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict(providers = [P])})
lk_strict = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict()})
lk_both = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict(allow_rules = ["prov"], providers = [P])})
lk_flags = rule(_lk_impl, attrs = {"m": attr.label_keyed_string_dict(allow_files = True, flags = ["SKIP_CONSTRAINTS_OVERRIDE"])})
