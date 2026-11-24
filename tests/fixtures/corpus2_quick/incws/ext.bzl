def _r(rctx):
    rctx.file("BUILD", "filegroup(name='x')")
r = repository_rule(implementation = _r)
def _ext(mctx):
    for m in mctx.modules:
        for t in m.tags.tag:
            print("TAG", m.name, t.v)
    r(name = "r")
ext = module_extension(implementation = _ext, tag_classes = {"tag": tag_class(attrs = {"v": attr.string()})})
