# Like rules that load @bazel_features: the extension's bzlTransitiveDigest
# covers @features_version//:version.bzl, whose bytes carry native.bazel_version.
load("@features//:features.bzl", "BAZEL_VERSION")

def _repo_impl(rctx):
    rctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    rctx.file("v.txt", rctx.attr.version + "\n")

_repo = repository_rule(_repo_impl, attrs = {"version": attr.string()})

def _impl(mctx):
    _repo(name = "probe", version = BAZEL_VERSION)

ext = module_extension(_impl)
