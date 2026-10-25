def _impl(rctx):
    root = str(rctx.path("."))
    segments = root.split("/")
    rctx.file("BUILD.bazel", """
genrule(name = "show", outs = ["show.txt"], cmd = "echo {last_two} > $@")
genrule(name = "full", outs = ["full.txt"], cmd = "echo {root} > $@")
""".format(last_two = segments[-2] + "/" + segments[-1], root = root))

probe = repository_rule(implementation = _impl)
