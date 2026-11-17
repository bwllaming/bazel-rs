
def _dump(name, visibility, **kwargs):
    print("NAME", name, "VIS", visibility)
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))
m = macro(implementation = _dump, attrs = {1: attr.string()})
