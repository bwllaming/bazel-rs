def _empty(rctx):
    rctx.file("BUILD.bazel", "")

_empty_repo = repository_rule(_empty)

def _toolchains(mctx):
    for name in ["zstd_toolchains", "bsd_tar_toolchains", "jq_toolchains"]:
        _empty_repo(name = name)

toolchains = module_extension(
    _toolchains,
    tag_classes = {"zstd": tag_class(), "jq": tag_class(), "tar": tag_class()},
)
