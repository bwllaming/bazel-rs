load("//lkm:defs.bzl", "lk")

config_setting(name = "opt", values = {"compilation_mode": "opt"})

filegroup(name = "p1", srcs = [])

lk(name = "unselected_branch_dup", m = select({":opt": {":p1": "a", "//lkdup:p1": "b"}, "//conditions:default": {}}))
