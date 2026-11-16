"""File.tree_relative_path (starlarkbuildapi/FileApi.java:133-143): the path
of a tree artifact's expanded file below the tree (Artifact.TreeFileArtifact,
actions/Artifact.java:1148-1150); on any other File an error
(Artifact.java:549-552). rules_oci's oci_image_index uses it
(oci/private/image_index.bzl:97)."""

def _tree_impl(ctx):
    tree = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run_shell(
        outputs = [tree],
        command = "mkdir -p {t}/sub/deep && echo a > {t}/a.txt && echo b > {t}/sub/b.json && echo c > {t}/sub/deep/c".format(t = tree.path),
        mnemonic = "Tree",
    )
    return [DefaultInfo(files = depset([tree]))]

tree = rule(implementation = _tree_impl)

def _blob(file):
    return "--blob={}".format(file.tree_relative_path)

def _describe(file):
    return "|".join([
        file.tree_relative_path,
        file.path,
        file.short_path,
        file.basename,
        file.dirname,
        file.extension,
        str(file.is_directory),
        str(file.is_source),
        str(file.is_symlink),
        str(file.owner),
        file.root.path,
        str(hasattr(file, "tree_relative_path")),
        str("tree_relative_path" in dir(file)),
        repr(file),
        str(file),
    ])

def _dir(file):
    return ",".join(dir(file))

def _expanded(file, expander):
    return ["{}={}".format(child.basename, child.tree_relative_path) for child in expander.expand(file)]

def _probe_impl(ctx):
    tree = ctx.files.tree[0]
    plain = ctx.file.plain
    args = ctx.actions.args()
    args.add_all([tree], map_each = _blob)
    args.add_joined("--joined", [tree], join_with = ",", map_each = _blob)
    args.add_all([tree], map_each = _describe)
    args.add_all([tree], map_each = _dir)
    args.add_all([tree], map_each = _expanded, expand_directories = False)
    out = ctx.actions.declare_file(ctx.label.name + "_args.txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [tree],
        arguments = [args],
        command = "for a in \"$@\"; do echo \"$a\"; done > " + out.path,
        mnemonic = "Args",
    )
    params = ctx.actions.declare_file(ctx.label.name + "_params.txt")
    ctx.actions.write(params, args)

    # A File that is not a tree artifact file: the field is listed, hasattr
    # sees it, reading it is the error.
    facts = ctx.actions.declare_file(ctx.label.name + "_facts.txt")
    ctx.actions.write(facts, "\n".join([
        str(hasattr(plain, "tree_relative_path")),
        str("tree_relative_path" in dir(plain)),
        ",".join(dir(plain)),
        str(hasattr(tree, "tree_relative_path")),
        ",".join(dir(tree)),
    ]) + "\n")
    return [DefaultInfo(files = depset([out, params, facts]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "tree": attr.label(),
        "plain": attr.label(allow_single_file = True),
    },
)

def _bad_impl(ctx):
    tree = ctx.files.tree[0]
    plain = ctx.file.plain
    mode = ctx.attr.mode
    args = ctx.actions.args()
    if mode == "analysis_plain":
        args.add(plain.tree_relative_path)
    elif mode == "analysis_tree":
        args.add(tree.tree_relative_path)
    elif mode == "getattr_default":
        args.add(getattr(plain, "tree_relative_path", "default"))
    elif mode == "map_each_plain":
        args.add_all([plain], map_each = _blob)
    elif mode == "map_each_unexpanded":
        args.add_all([tree], map_each = _blob, expand_directories = False)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [tree, plain],
        arguments = [args],
        command = "for a in \"$@\"; do echo \"$a\"; done > " + out.path,
        mnemonic = "Bad",
    )
    return [DefaultInfo(files = depset([out]))]

bad = rule(
    implementation = _bad_impl,
    attrs = {
        "tree": attr.label(),
        "plain": attr.label(allow_single_file = True),
        "mode": attr.string(),
    },
)
