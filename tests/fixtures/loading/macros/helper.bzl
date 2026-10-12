FROZEN_INPUTS = ["assets/a.txt", "assets/b.txt"]
OPTIONS = struct(suffix = ".txt", inputs = FROZEN_INPUTS)
_private = "not loadable"

def copy_rule(name, source):
    native.genrule(
        name = name,
        srcs = [source],
        outs = [name + OPTIONS.suffix],
        cmd = "cat $(SRCS) > $@",
    )

def mutate_frozen():
    FROZEN_INPUTS.append("forbidden.txt")
