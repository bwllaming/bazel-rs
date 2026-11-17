def _r_impl(ctx):
    return []

r = rule(_r_impl, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "s": attr.string(default = "d"),
    "m": attr.string(mandatory = True),
    "_priv": attr.string(default = "p"),
    "lk": attr.label_keyed_string_dict(),
    "nc": attr.string(default = "q"),
    "one": attr.label(default = ":x"),
})
t_test = rule(_r_impl, test = True, attrs = {"x": attr.int(default = 4)})

def _dump(name, visibility, **kwargs):
    print("NAME", name, "VIS", visibility)
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))

def _impl_r(name, visibility, **kwargs):
    _dump(name, visibility, **kwargs)
    r(name = name, visibility = visibility, **kwargs)

inh_r = macro(implementation = _impl_r, inherit_attrs = r)
inh_t = macro(implementation = _dump, inherit_attrs = t_test)
inh_common = macro(implementation = _dump, inherit_attrs = "common")
inh_fg = macro(implementation = _dump, inherit_attrs = native.filegroup)
inh_gen = macro(implementation = _dump, inherit_attrs = native.genrule)

def _user(name, visibility, **kwargs):
    _dump(name, visibility, **kwargs)

user = macro(implementation = _user, attrs = {
    "a": attr.string(),
    "b": attr.label(),
    "c": attr.label_list(),
    "d": attr.string(configurable = False, default = "x"),
    "e": attr.int(default = 3),
    "f": attr.bool(),
    "g": attr.string_dict(),
    "h": attr.label(configurable = False),
    "i": attr.string_list(default = ["z"]),
    "j": attr.output(),
    "k": attr.label_keyed_string_dict(),
    "mand": attr.string(mandatory = True),
    "_p": attr.string(default = "pp"),
})
inh_m = macro(implementation = _user, inherit_attrs = user, attrs = {"e": None, "extra": attr.string()})
