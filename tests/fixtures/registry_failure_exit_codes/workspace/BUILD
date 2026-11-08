load(":defs.bzl", "my_bin", "my_test", "str_flag")

genrule(name = "a", outs = ["a.txt"], cmd = "touch $@")

my_test(name = "t")

my_bin(name = "tool")

str_flag(name = "flag", build_setting_default = "d")
