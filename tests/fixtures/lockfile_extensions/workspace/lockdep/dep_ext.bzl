"""An extension defined in a non-root module and used by two modules."""

load("@bazel_tools//tools/build_defs/repo:utils.bzl", "maybe")  # buildifier: disable=load
load("@lockdep//:dep_repo.bzl", "dep_repo")

_item = tag_class(attrs = {
    "value": attr.string(),
    "label": attr.label(),
})

def _dep_ext_impl(ctx):
    print("LOCKEXT_EVAL dep_ext")
    values = []
    for mod in ctx.modules:
        for tag in mod.tags.item:
            values.append("%s@%s:%s" % (mod.name, mod.version, tag.value))
    dep_repo(name = "dep_gen", values = values, root = Label("@lockdep//:lib"))

dep_ext = module_extension(
    implementation = _dep_ext_impl,
    tag_classes = {"item": _item},
)
