load(":root_export.bzl", "ROOT", "ROOT_DICT")
load("@shared//:export.bzl", "EXTERNAL", "EXTERNAL_DICT")
load("@shared//:helper.bzl", "copy_dep", "local_dep")
load(":reader_a.bzl", "read_a")
load(":reader_b.bzl", "read_b")

def render():
    root, external = copy_dep(ROOT), Label(EXTERNAL)
    if root != ROOT or external != EXTERNAL or read_a() != read_b():
        fail("PRODUCER_COPY_EQUALITY")
    if ROOT_DICT[root] != "root-frozen-key" or EXTERNAL_DICT[external] != "external-frozen-key":
        fail("PRODUCER_FROZEN_KEYS")
    if local_dep() != external:
        fail("PRODUCER_LEXICAL_STRING")
    return str(root) + "\n" + str(external) + "\n" + external.workspace_name + "\n"

def _produce(ctx):
    print("LP_ANALYSIS_CALLBACK")
    output = ctx.actions.declare_file("data.txt")
    ctx.actions.write(output, render())
    return [DefaultInfo(files=depset([output]))]

produce = rule(implementation=_produce)

def declare_custom():
    produce(name="producer")
    native.genrule(name="consumer", srcs=[":producer"], outs=["result.txt"], cmd="cat $(SRCS) > $@")
    native.filegroup(name="result", srcs=[":producer", ":consumer"])
