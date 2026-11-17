def _impl(ctx):
    pass

lrule = rule(implementation = _impl, attrs = {
    "lab": attr.label(default = "@dep//:f", allow_files = True),
    "opt": attr.label(allow_files = True),
    "labs": attr.label_list(allow_files = True),
    "skl": attr.string_keyed_label_dict(allow_files = True),
    "lld": attr.string_dict(),
})

def show(name):
    r = native.existing_rule(name)
    for k, v in r.items():
        print("ATTR %s.%s %s %r" % (name, k, type(v), v))
