load(":defs.bzl", "gen", "tool")

tool(name = "t", data = ["data.txt"])

# Two execution platforms whose output directory names are both k8
# (`host` platforms): the tool is analyzed in two exec configurations that
# differ only in --platforms, and their runfiles support actions are equal
# and shared, as in bb-storage (a js_binary used as a js_run_binary tool,
# B69).
gen(name = "on_a", exec_compatible_with = ["//plat:a"], tool = ":t")

gen(name = "on_b", tool = ":t")
