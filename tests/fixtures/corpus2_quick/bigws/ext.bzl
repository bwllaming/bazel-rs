def _r(rctx):
    rctx.file("BUILD", "filegroup(name='x', visibility=['//visibility:public'])\n# %d\n" % len(rctx.attr.items))
r = repository_rule(implementation = _r, attrs = {"items": attr.string_list()})
def _ext(mctx):
    r(name = "r", items = ["i%d" % i for i in range(150000)])
ext = module_extension(implementation = _ext)
