load(":defs.bzl", "env")

config_setting(name = "opt", values = {"compilation_mode": "opt"})

filegroup(name = "tool")

env(
    name = "e",
    mode = select({":opt": "opt", "//conditions:default": "other"}),
    tool = ":tool",
)

platform(name = "p")
