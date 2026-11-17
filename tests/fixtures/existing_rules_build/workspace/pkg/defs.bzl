def _impl(ctx):
    pass

myrule = rule(
    implementation = _impl,
    attrs = {
        "s": attr.string(default = "dflt"),
        "i": attr.int(default = 7),
        "b": attr.bool(),
        "l": attr.label(),
        "ll": attr.label_list(),
        "sl": attr.string_list(),
        "sd": attr.string_dict(),
        "sld": attr.string_list_dict(),
        "lksd": attr.label_keyed_string_dict(),
        "o": attr.output(),
        "ol": attr.output_list(),
        "il": attr.int_list(),
        "_tool": attr.label(default = "//other:tool"),
        "sel": attr.string(),
    },
)

my_test = rule(implementation = _impl, test = True, attrs = {"x": attr.string()})

def dump(name):
    r = native.existing_rule(name)
    if r == None:
        print("NONE %s" % name)
        return
    print("RULE %s type=%s str=%s repr=%r len=%d bool=%s" % (name, type(r), str(r), r, len(r), bool(r)))
    for k in r.keys():
        v = r[k]
        print("ATTR %s.%s %s %r" % (name, k, type(v), v))
    print("GET %s missing=%r default=%r in=%s notin=%s" % (name, r.get("nope"), r.get("nope", 5), "name" in r, "nope" in r))
    print("ITEMS %s %d %d" % (name, len(r.items()), len(r.values())))
    print("ITER %s %s" % (name, [k for k in r] == r.keys()))

def legacy(name, **kwargs):
    native.genrule(name = name, outs = [name + ".out"], cmd = "touch $@", **kwargs)
    dump(name)

def snapshot():
    rules = native.existing_rules()
    print("RULES type=%s repr=%r len=%d keys=%r" % (type(rules), rules, len(rules), rules.keys()))
    return rules

def _sym_impl(name, visibility):
    native.filegroup(name = name + "_fg")

sym = macro(implementation = _sym_impl)

def _sym_er_impl(name, visibility):
    native.existing_rules()

sym_er = macro(implementation = _sym_er_impl)
