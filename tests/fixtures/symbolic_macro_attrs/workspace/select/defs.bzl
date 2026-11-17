def _dump(name, visibility, **kwargs):
    for k, v in kwargs.items():
        print("KW", k, type(v), repr(v))
m = macro(implementation = _dump, attrs = {"a": attr.label_list(), "s": attr.string(), "t": attr.string_list(), "o": attr.output_list()}, inherit_attrs = "common")
