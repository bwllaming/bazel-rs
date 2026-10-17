load("@rules_shell//shell:sh_binary.bzl", "sh_binary")
load(":defs.bzl", "loc", "loc_common")

genrule(name = "gen", outs = ["g1.txt", "g2.txt"], cmd = "touch $(OUTS)")

sh_binary(name = "tool", srcs = ["tool.sh"])

filegroup(name = "fg", srcs = ["a.txt"])

filegroup(name = "fg2", srcs = ["a.txt", "b.txt"])

filegroup(name = "empty", srcs = [])

alias(name = "al", actual = ":fg")

INPUTS = [
    "$(location :fg)",
    "$(locations :al)",
    "$(rootpath a.txt)",
    "$(execpath :fg)",
    "$(execpaths :gen)",
    "$(rootpaths :gen)",
    "$(location :tool)",
    "$(rlocationpath :tool)",
    "$(execpath tool)",
    "$(location o.txt)",
    "$(locations :o1.txt)",
    "$(execpath //test:fg)",
    "$(location @bazel_tools//tools/test:collect_coverage)",
    "$(rlocationpaths @bazel_tools//tools/test:collect_coverage)",
    "$(rlocationpaths :fg2)",
    "$(location c.txt)",
    "$(locations :fg2)",
    "$(location  :fg )",
    "x$(location :fg)y $(location :fg)",
    "plain $(foo) $$ $(location) $(locations)",
    "$(execpath a.txt) $(rootpath b.txt) $(execpath d.txt)",
]

loc_common(name = "ok", inputs = INPUTS, out = "o.txt", outs = ["o1.txt", "o2.txt"])

loc_common(name = "short", inputs = [i for i in INPUTS if "o.txt" not in i and "o1.txt" not in i], short = True)

loc_common(name = "err_undeclared", inputs = ["$(location :nope)"])

loc_common(name = "err_multi", inputs = ["$(location :gen)"])

loc_common(name = "err_rlocation_multi", inputs = ["$(rlocationpath :fg2)"])

loc_common(name = "err_invalid", inputs = ["$(location //a:b:c)"])

loc_common(name = "err_unterminated", inputs = ["a $(location :fg"])

loc_common(name = "err_empty", inputs = ["$(locations :empty)"])

loc_common(name = "err_two", inputs = ["$(location :nope1)", "ok $(location :fg)", "$(execpath :nope2)"])

loc(name = "err_data_only", inputs = ["$(location e.txt)"], mode = "notargets", data = ["e.txt"])

loc(name = "err_dup", mode = "dup", data = ["c.txt"])

loc(name = "err_none", mode = "none")

loc(name = "err_type", mode = "type")
