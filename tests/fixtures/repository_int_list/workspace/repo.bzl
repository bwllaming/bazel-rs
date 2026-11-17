def _impl(rctx):
    v = rctx.attr.lengths
    print("VALUE %s %s %r %d" % (rctx.original_name, type(v), v, len(v)))
    rctx.file("BUILD.bazel", 'filegroup(name = "files")\n')

repo = repository_rule(implementation = _impl, attrs = {"lengths": attr.int_list(default = [1, 2])})

def _ext(mctx):
    repo(name = "from_ext", lengths = [7, 8, 9])

ext = module_extension(implementation = _ext)
