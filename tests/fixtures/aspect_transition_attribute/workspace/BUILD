load(":defs.bzl", "env", "wrap")

filegroup(name = "leaf")

wrap(name = "w", target = ":leaf")

env(name = "e", wrapped = [":w"])

platform(name = "p")
