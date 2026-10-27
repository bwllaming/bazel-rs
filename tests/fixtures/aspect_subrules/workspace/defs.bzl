def _sub_impl(ctx, prefix):
    out = ctx.actions.declare_file(ctx.label.name + ".sub")
    ctx.actions.write(out, "{} {}\n".format(prefix, ctx.label))
    print("S", ctx.label, repr(ctx), type(ctx.actions))
    return out

sub = subrule(implementation = _sub_impl)

def _inner_impl(ctx):
    return "inner"

inner = subrule(implementation = _inner_impl)

def _outer_impl(ctx):
    print("O", ctx.label, inner())
    return None

outer = subrule(implementation = _outer_impl, subrules = [inner])

def _attr_sub_impl(ctx, _dep):
    return _dep

attr_sub = subrule(implementation = _attr_sub_impl, attrs = {"_dep": attr.label(default = "//:dep")})

def _a_impl(target, ctx):
    out = sub("p")
    outer()
    print("A", target.label, dir(ctx.attr))
    return [OutputGroupInfo(sub = depset([out]))]

asp = aspect(_a_impl, attr_aspects = ["deps"], subrules = [sub, outer])

def _other_impl(ctx):
    return None

other = subrule(implementation = _other_impl)

def _bad_impl(target, ctx):
    other()
    return []

bad = aspect(_bad_impl, subrules = [sub])

def _nested_impl(target, ctx):
    inner()
    return []

nested = aspect(_nested_impl, subrules = [outer])

def _attr_impl(target, ctx):
    print("D", attr_sub())
    return []

attr_asp = aspect(_attr_impl, subrules = [attr_sub])

def _r(ctx):
    return []

lib = rule(_r, attrs = {"deps": attr.label_list()})

def _bad_rule_impl(ctx):
    other()
    return []

bad_rule = rule(_bad_rule_impl, subrules = [sub])

def _file_sub_impl(ctx, _data):
    return _data

file_sub = subrule(implementation = _file_sub_impl, attrs = {"_data": attr.label(default = "//:data.txt", allow_single_file = True)})

def _file_impl(target, ctx):
    file_sub()
    return []

file_asp = aspect(_file_impl, subrules = [file_sub])
