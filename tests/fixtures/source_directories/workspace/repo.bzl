"""An extracted npm package as rules_js's npm_import leaves it: `package/`."""

def _impl(rctx):
    # rctx.file makes files executable unless told otherwise.
    rctx.file("package/package.json", '{"name": "npmish"}\n')
    rctx.file("package/lib/index.js", "module.exports = 1;\n", executable = False)
    rctx.file("package/bin/cli.js", "#!/bin/sh\necho cli\n", executable = True)

    # No package boundary below a source directory of an external
    # repository (--incompatible_check_external_repo_source_dir_package_boundary
    # defaults to false).
    rctx.file("package/nested/BUILD.bazel", "# not a package boundary\n", executable = False)
    rctx.file("package/nested/n.txt", "n\n", executable = False)

    # An absolute link, a dangling link and an empty directory.
    rctx.symlink("package/lib/index.js", "package/main.js")
    rctx.execute(["ln", "-s", "nowhere", "package/dangling"])
    rctx.execute(["mkdir", "-p", "package/emptydir"])
    rctx.file("BUILD.bazel", """
filegroup(name = "pkg", srcs = ["package"], visibility = ["//visibility:public"])

exports_files(["package"])
""")

pkg_repo = repository_rule(implementation = _impl)
