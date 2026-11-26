def _r(rctx):
    if rctx.attr.mode == "link":
        rctx.file("real", "")
        rctx.symlink("real", "REPO.bazel")
        rctx.file("ignore_list", "ignored\n")
        rctx.file("sub/x.txt", "x\n")
    elif rctx.attr.mode == "foreign":
        # Linked from another repository's tree.
        rctx.symlink(Label("@linked//:real"), "REPO.bazel")
        rctx.symlink(Label("@linked//:ignore_list"), ".bazelignore")
    elif rctx.attr.mode == "overlay":
        # B86: a source directory linked from another repository's tree,
        # with an output below it.
        rctx.symlink(rctx.path(Label("@linked//:real")).dirname.get_child("sub"), "inc")
    else:
        rctx.symlink("made", "REPO.bazel")
    extra = ""
    if rctx.attr.mode == "overlay":
        extra = """
genrule(name = "o", outs = ["inc/gen.txt"], cmd = "echo overlay > $@")
"""
    rctx.file("BUILD.bazel", """
filegroup(name = "t", srcs = ["data.txt"])
genrule(name = "g", srcs = ["REPO.bazel"], outs = ["g.txt"], cmd = "cat $< > $@; echo end >> $@")
""" + extra)
    rctx.file("data.txt", "data\n")

r = repository_rule(_r, attrs = {"mode": attr.string()})
