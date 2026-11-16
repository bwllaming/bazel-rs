def _slow_repo_impl(rctx):
    rctx.execute(["sleep", "2"])
    rctx.file("BUILD", 'genrule(name = "r", outs = ["r.txt"], cmd = "echo r > $@", visibility = ["//visibility:public"])\n')

slow_repo = repository_rule(implementation = _slow_repo_impl)
