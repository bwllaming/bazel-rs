# bazel_features 1.42.1 private/version_repo.bzl, reduced: version.bzl holds
# native.bazel_version, the running Bazel's release.
def _version_repo_impl(rctx):
    rctx.file("BUILD.bazel", "exports_files(['version.bzl'])\n")
    rctx.file("version.bzl", "version = '" + native.bazel_version + "'")

version_repo = repository_rule(_version_repo_impl, local = True)

def _version_extension_impl(mctx):
    version_repo(name = "features_version")

version_extension = module_extension(_version_extension_impl)
