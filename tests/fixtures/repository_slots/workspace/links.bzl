def _sibling(rctx):
    rctx.file("BUILD.bazel", "exports_files(['file'])\n")
    rctx.file("file", "sibling\n")

sibling = repository_rule(implementation = _sibling)

def _linker(rctx):
    # Fetched first: the sibling's directory exists next to this one.
    rctx.path(Label("@sibling//:file"))
    rctx.file("inner", "inner\n")
    # A path relative to this repository's directory.
    rctx.symlink("../+sibling+sibling/file", "through_path")
    result = rctx.execute(["sh", "-c", " && ".join([
        "ln -s ../+sibling+sibling/file relative",
        "ln -s \"$PWD/../+sibling+sibling/file\" absolute",
        "ln -s inner inside_relative",
        "ln -s \"$PWD/inner\" inside_absolute",
    ])])
    if result.return_code:
        fail(result.stderr)
    rctx.file("BUILD.bazel", "exports_files(['through_path', 'relative', 'absolute', 'inside_relative', 'inside_absolute'])\n")

linker = repository_rule(implementation = _linker)
