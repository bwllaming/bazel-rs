
def _dump(name, visibility, **kwargs):
    print("NAME", name, "VIS", visibility)
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))
def _d():
    return None
m = macro(implementation = _dump, attrs = {'a': attr.label(default = _d)})
