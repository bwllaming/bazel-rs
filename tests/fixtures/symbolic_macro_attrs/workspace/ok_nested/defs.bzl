
def _in(name, visibility, **kwargs):
    print("IN", name, visibility, kwargs)
    native.filegroup(name = name, visibility = visibility, **kwargs)
    native.filegroup(name = name + "_priv")
inner = macro(implementation = _in, inherit_attrs = native.filegroup)
def _out(name, visibility, srcs, **kwargs):
    print("OUT", name, visibility, srcs)
    inner(name = name, visibility = visibility, srcs = srcs)
    inner(name = name + "_b", srcs = ["c"])
    inner(name = name + "-c", visibility = ["//other:__pkg__"])
    native.filegroup(name = name + ".d")
outer = macro(implementation = _out, inherit_attrs = native.filegroup)
