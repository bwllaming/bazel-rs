def _repo(rctx):
    rctx.file("BUILD.bazel", "")

repo = repository_rule(implementation = _repo)

def _ext(mctx):
    for name in ["a", "b", "d"]:
        repo(name = name)
    return mctx.extension_metadata(root_module_direct_deps = ["a", "b"], root_module_direct_dev_deps = ["d"])

ext = module_extension(implementation = _ext)
