load(":helper.bzl", copy_alias = "copy_rule", inputs = "FROZEN_INPUTS")

def loading_pipeline(name):
    if type(native) != "struct":
        fail("native must be a struct")
    found = native.glob(["assets/**/*.txt"], exclude = ["assets/omit.txt"], exclude_directories = 1, allow_empty = False)
    if found != inputs:
        fail("glob must exclude files and stop at package boundaries: " + str(found))
    directories = native.glob(["assets/*"], exclude = ["assets/*.txt"], exclude_directories = 0)
    if directories != ["assets/plain"]:
        fail("glob directory inclusion or subpackage exclusion differs: " + str(directories))
    if native.glob(["absent-*.txt"], allow_empty = True) != []:
        fail("allow_empty glob must return an empty list")
    copy_alias(name = name + "_seed", source = found[0])
    copy_alias(name = name + "_left", source = ":" + name + "_seed")
    copy_alias(name = name + "_right", source = ":" + name + "_seed")
    copy_alias(name = name + "_independent", source = found[1])
    native.genrule(
        name = name + "_join",
        srcs = [":" + name + "_left", ":" + name + "_right", ":" + name + "_independent"],
        outs = [name + "_join.txt"],
        cmd = "cat $(SRCS) > $@",
    )
    copy_alias(name = name, source = ":" + name + "_join")
