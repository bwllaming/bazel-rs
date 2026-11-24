genrule(
    name = "uses",
    srcs = ["//broken:before"],
    outs = ["uses.txt"],
    cmd = "cat $(SRCS) > $@",
)

genrule(
    name = "uses_data",
    srcs = ["//broken:data.txt"],
    outs = ["uses_data.txt"],
    cmd = "cat $(SRCS) > $@",
)

genrule(
    name = "uses_after",
    srcs = ["//broken:after"],
    outs = ["uses_after.txt"],
    cmd = "cat $(SRCS) > $@",
)
