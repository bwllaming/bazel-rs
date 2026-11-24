genrule(
    name = "before",
    srcs = ["//good:good"],
    outs = ["before.txt"],
    cmd = "cat $(SRCS) > $@",
)

filegroup(
    name = "group",
    srcs = [":before"],
)

exports_files(["data.txt"])

fail("broken on purpose")

genrule(
    name = "after",
    outs = ["after.txt"],
    cmd = "echo after > $@",
)
