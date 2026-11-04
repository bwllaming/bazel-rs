load(":rules.bzl", "my_rule")

exports_files(["tool.sh"])

genrule(name = "g", outs = ["g.txt"], cmd = "echo hi > $@")

my_rule(name = "s", srcs = ["a.txt", "b.txt"])

my_rule(name = "nocompile", compile = False)
