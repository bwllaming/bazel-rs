load(":defs.bzl", "kitchen", "user", "pw")
genrule(
    name = "tool_gen",
    outs = ["tool.sh"],
    cmd = "echo '#!/bin/sh' > $@",
    executable = True,
)
kitchen(name = "k", srcs = ["in.txt", "in2.txt"], tmpl = "t.tmpl", tool = ":tool_gen")
genrule(
    name = "g",
    srcs = ["in.txt"],
    outs = ["g1.txt", "g2.txt"],
    tools = [":tool_gen"],
    cmd = "$(location :tool_gen) $(SRCS) > $(location g1.txt); touch $(location g2.txt)",
)
user(name = "u1", deps = [":g", ":k"])
pw(name = "pw", srcs = ["in.txt"], tool = ":tool_gen", tags = ["no-cache"])
