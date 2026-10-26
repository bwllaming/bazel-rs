_tag = tag_class(attrs = {"name": attr.string(), "value": attr.string(), "numbers": attr.int_list()})

def _repo_impl(rctx):
    rctx.file("BUILD.bazel", "filegroup(name = 'all')\n")
    rctx.file("value.txt", rctx.attr.value)

value_repo = repository_rule(implementation = _repo_impl, attrs = {"value": attr.string()})

def _ext_impl(mctx):
    names = []
    for mod in mctx.modules:
        for tag in mod.tags.tag:
            if tag.name not in names:
                names.append(tag.name)
                value_repo(name = tag.name, value = tag.value)
    value_repo(name = "unused_repo", value = "nobody")

ext = module_extension(implementation = _ext_impl, tag_classes = {"tag": _tag})
