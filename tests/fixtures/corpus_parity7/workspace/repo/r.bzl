def _r(rctx):
    if rctx.attr.mode == "link":
        rctx.file("real", "")
        rctx.symlink("real", "REPO.bazel")
        rctx.file("ignore_list", "ignored\n")
    elif rctx.attr.mode == "foreign":
        # Linked from another repository's tree.
        rctx.symlink(Label("@linked//:real"), "REPO.bazel")
        rctx.symlink(Label("@linked//:ignore_list"), ".bazelignore")
    else:
        rctx.symlink("made", "REPO.bazel")
    rctx.file("BUILD.bazel", """
filegroup(name = "t", srcs = ["data.txt"])
genrule(name = "g", srcs = ["REPO.bazel"], outs = ["g.txt"], cmd = "cat $< > $@; echo end >> $@")
""")
    rctx.file("data.txt", "data\n")

r = repository_rule(_r, attrs = {"mode": attr.string()})
