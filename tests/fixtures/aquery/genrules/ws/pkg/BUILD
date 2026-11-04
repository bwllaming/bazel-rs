filegroup(name = "fg", srcs = ["in.txt", "two.txt"])

genrule(
    name = "tool",
    outs = ["tool.sh"],
    cmd = "printf '#!/bin/sh\\necho tool\\n' > $@",
    executable = True,
)

# cmd_bash alone.
genrule(name = "bash_only", outs = ["bash_only.txt"], cmd_bash = "echo bash > $@")

# cmd_bash wins over cmd.
genrule(name = "both", outs = ["both.txt"], cmd = "echo cmd > $@", cmd_bash = "echo bash > $@")

# cmd_bat and cmd_ps only apply on Windows.
genrule(
    name = "windows",
    outs = ["windows.txt"],
    cmd = "echo cmd > $@",
    cmd_bat = "echo bat > $@",
    cmd_ps = "echo ps > $@",
)

# A selected cmd_bash.
genrule(
    name = "selected",
    srcs = ["in.txt"],
    outs = ["selected.txt"],
    cmd = "echo cmd > $@",
    cmd_bash = select({"//conditions:default": "cat $(SRCS) > $@ && echo $(RULEDIR) $(@D) $(COMPILATION_MODE) $(BINDIR) $(GENDIR) $(TARGET_CPU) >> $@"}),
)

# An explicit empty cmd_bash still wins.
genrule(name = "empty_bash", outs = ["empty_bash.txt"], cmd = "echo cmd > $@", cmd_bash = "")

# message, local, output_to_bindir.
genrule(
    name = "message",
    srcs = ["in.txt"],
    outs = ["message.txt"],
    cmd_bash = "cp $< $@",
    local = True,
    message = "Copying a file",
    output_to_bindir = True,
)

# stamp: 1 stamps, -1 follows --stamp, 0 never.
genrule(name = "stamped", outs = ["stamped.txt"], cmd_bash = "grep -c BUILD_ bazel-out/stable-status.txt > $@", stamp = 1)
genrule(name = "stamp_auto", outs = ["stamp_auto.txt"], cmd_bash = "echo auto > $@", stamp = -1)
genrule(name = "unstamped", outs = ["unstamped.txt"], cmd_bash = "echo no > $@", stamp = 0)

# heuristic_label_expansion: labels of srcs/tools expand to their single file.
genrule(
    name = "heuristic",
    srcs = ["in.txt"],
    outs = ["heuristic.txt"],
    tools = [":tool"],
    cmd_bash = ":tool > $@; cat in.txt //pkg:in.txt >> $@; echo two.txt heuristic.txt >> $@",
    heuristic_label_expansion = True,
)

# Common attributes.
genrule(
    name = "common",
    outs = ["common.txt"],
    cmd_bash = "echo common > $@",
    exec_compatible_with = ["@platforms//os:linux"],
    aspect_hints = [],
    compatible_with = [],
)

genrule(name = "all_ok", outs = ["all_ok.txt"], srcs = [":bash_only", ":both", ":windows", ":selected", ":message", ":stamped", ":stamp_auto", ":unstamped", ":heuristic", ":common"], cmd = "cat $(SRCS) > $@")
