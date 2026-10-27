load(":defs.bzl", "my_bin", "my_test")

genrule(name = "a", outs = ["a.txt", "a2.txt"], cmd = "touch $(OUTS)")

genrule(name = "b", outs = ["b.txt"], cmd = "echo b > $@")

filegroup(name = "empty", srcs = [])

filegroup(name = "src", srcs = ["BUILD"])

genrule(name = "fail", outs = ["f.txt"], cmd = "exit 1")

my_test(name = "t")

my_bin(name = "tool")
