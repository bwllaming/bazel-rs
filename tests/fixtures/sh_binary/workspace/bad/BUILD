load("@rules_shell//shell:sh_binary.bzl", "sh_binary")

genrule(name = "g", outs = ["g.sh"], cmd = "echo > $@")

sh_binary(name = "b", srcs = ["b.sh"], deps = [":g"])
