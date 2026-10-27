load(":defs.bzl", "kitchen")

def legacy_pair(name, **kwargs):
    kitchen(name = name + "_a", s = "a", **kwargs)
    native.filegroup(name = name, srcs = [":" + name + "_a"])
