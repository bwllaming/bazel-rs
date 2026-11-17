
def _dump(name, visibility, **kwargs):
    print("NAME", name, "VIS", visibility)
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))
m = macro(implementation = _dump, inherit_attrs = 1)
