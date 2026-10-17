load("//:defs.bzl", "my_library")

def wrapped_library(name, **kwargs):
    my_library(name = name + "_impl", **kwargs)
    native.filegroup(name = name, srcs = [":" + name + "_impl"])
