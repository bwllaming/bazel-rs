
def _dump(name, visibility, **kwargs):
    print("NAME", name, "VIS", visibility)
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))

user = macro(implementation = _dump, attrs = {
    "a": attr.string(),
    "aa": attr.string(),
    "b": attr.label(),
    "d": attr.string(configurable = False, default = "x"),
    "e": attr.int(default = 3),
    "mand": attr.string(mandatory = True),
    "_p": attr.string(default = "pp"),
})
