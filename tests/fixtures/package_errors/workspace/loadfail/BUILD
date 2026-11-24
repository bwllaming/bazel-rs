load(":missing.bzl", "nothing")

genrule(
    name = "unreached",
    outs = ["unreached.txt"],
    cmd = "echo x > $@",
)
