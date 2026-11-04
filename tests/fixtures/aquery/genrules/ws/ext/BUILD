# $(@D) and $(RULEDIR) of an external repository's root package.
genrule(
    name = "multi",
    outs = ["a/x.txt", "b.txt"],
    cmd_bash = "echo x > $(@D)/a/x.txt; echo $(@D) $(RULEDIR) > $(location b.txt)",
)

genrule(
    name = "single",
    outs = ["s.txt"],
    cmd_bash = "echo $(@D) $(RULEDIR) > $@",
)
