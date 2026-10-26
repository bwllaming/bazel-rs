load(":defs.bzl", "mac", "my_test", "r")

print("build top")

r(name = "leaf")

r(name = "leaf2")

r(name = "top", deps = [":leaf"], t = [":leaf2", "@r//:f"])

mac(name = "m")

genrule(name = "gen", outs = ["gen.txt"], cmd = "echo hi > $@")

my_test(name = "t")

my_test(name = "f", code = 1)
