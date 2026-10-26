load(":defs.bzl", "only_p", "plain", "r")

plain(name = "leaf")

plain(name = "mid", deps = [":leaf"])

r(name = "top", deps = [":mid"], p = "x", n = 2)

r(name = "top_defaults", deps = [":mid"])

only_p(name = "top_d", deps = [":mid"])
