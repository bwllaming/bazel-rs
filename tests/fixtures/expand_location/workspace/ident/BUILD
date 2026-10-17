load(":defs.bzl", "ident")

filegroup(name = "fg", srcs = ["BUILD"])

alias(name = "al", actual = ":fg")

ident(name = "x", deps = [":fg", ":al"], data = [":fg"], single = ":fg", tool = ":fg", lk = {":fg": "v", ":al": "w"})
