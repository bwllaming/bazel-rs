
def _observe(ctx):
    # Paths into another external repository (and the workspace), observed
    # through every watching API both contexts share.
    root = ctx.path(Label("@data//:file.txt")).dirname
    ctx.read(root.get_child("file.txt"))
    ctx.read(Label("@data//:label.txt"))
    ctx.watch(root.get_child("missing.txt"))
    root.get_child("dir").readdir()
    ctx.read(ctx.path(Label("//:ws.txt")))
    return root

def _reader(ctx):
    root = _observe(ctx)
    ctx.watch_tree(root.get_child("tree"))
    ctx.template("out.txt", root.get_child("tpl.txt"), {"{NAME}": "x"})
    ctx.file("BUILD.bazel", 'filegroup(name = "all", srcs = ["out.txt"])\n')

reader = repository_rule(_reader)

def _from_ext(ctx):
    ctx.file("BUILD.bazel", 'filegroup(name = "all")\n')

from_ext = repository_rule(_from_ext)

def _ext(module_ctx):
    _observe(module_ctx)
    from_ext(name = "from_ext")

ext = module_extension(_ext)
