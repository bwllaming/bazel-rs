"""Symlinks inside tree artifacts (declare_directory outputs).

Bazel 9.3 TreeArtifactValue.visitTree follows every symlink below a tree
root to the file or directory it resolves to; a tree's children are the
regular files reached, at their apparent paths
(ActionOutputMetadataStore.constructTreeArtifactValueFromFilesystem).
Producers here create trees whose entries are symlinks of every shape
(rules_oci's image.sh with permits_treeartifact_uplevel_symlinks among
them); consumers expand the trees in Args and read them, in actions, in a
`run` program's runfiles and in a test's runfiles.
"""

def _tree_impl(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    outputs = [out]
    if ctx.attr.sibling:
        outputs.append(ctx.actions.declare_file(ctx.label.name + ".other"))
    requirements = {"no-sandbox": "1"} if ctx.attr.local else {}
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = outputs,
        # $1: the tree; $2...: the inputs (a tree input is its directory).
        command = "set -e\n" + ctx.attr.cmd,
        arguments = [out.path] + [f.path for f in ctx.files.srcs],
        mnemonic = "TreeSymlinks",
        execution_requirements = requirements,
    )
    return [DefaultInfo(files = depset([out]))]

# A tree artifact made by `cmd` (a bash script; $1 is the tree, $2... the
# srcs). `sibling` declares a second output `<name>.other` of the same
# action; `local` runs it outside the sandbox (no-sandbox).
tree = rule(
    implementation = _tree_impl,
    attrs = {
        "cmd": attr.string(mandatory = True),
        "local": attr.bool(),
        "sibling": attr.bool(),
        "srcs": attr.label_list(allow_files = True),
    },
)

def _alias_impl(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = ctx.file.tree)
    return [DefaultInfo(files = depset([out]))]

# ctx.actions.symlink of a whole tree (a tree output that is a symlink to
# another tree).
tree_alias = rule(
    implementation = _alias_impl,
    attrs = {"tree": attr.label(allow_single_file = True)},
)

# The listing every consumer prints: the tree's children as an action sees
# them (Args expansion), then every regular file reached below the tree
# root, following symlinks, with its executable bit, size and digest.
_LISTING = r"""
listing() {
  tree="$1"
  shift
  echo "expanded:"
  for child in "$@"; do
    echo "  ${child#"$tree"/}"
  done
  echo "files:"
  (cd "$tree" && find -L . -type f | LC_ALL=C sort) | while read -r rel; do
    rel="${rel#./}"
    if [ -x "$tree/$rel" ]; then x=x; else x=-; fi
    size=$(wc -c < "$tree/$rel")
    sum=$(sha256sum < "$tree/$rel" | cut -c1-16)
    if [ "$rel" = "sh" ] && cmp -s "$tree/$rel" /bin/sh; then sum=host-bin-sh; size=host; fi
    echo "  $rel $x $size $sum"
  done
}
"""

def _inspect_impl(ctx):
    tree = ctx.file.tree
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    raw = ctx.actions.declare_file(ctx.label.name + ".raw")
    args = ctx.actions.args()
    args.add(out)
    args.add(raw)
    args.add(tree.path)
    args.add_all([tree])
    ctx.actions.run_shell(
        inputs = [tree],
        outputs = [out, raw],
        arguments = [args],
        mnemonic = "InspectTree",
        command = _LISTING + r"""
out="$1"; raw="$2"; tree="$3"; shift 3
listing "$tree" "$@" > "$out"
# How the staged tree looks without following symlinks (evidence only:
# sandboxes stage inputs differently).
(cd "$tree" && find . -mindepth 1 -printf '%y %P\n' | LC_ALL=C sort) > "$raw"
""",
    )
    return [DefaultInfo(files = depset([out, raw]))]

# A consumer action of a tree: its listing (`<name>.txt`) and the staged
# entries' unfollowed types (`<name>.raw`).
inspect = rule(
    implementation = _inspect_impl,
    attrs = {"tree": attr.label(allow_single_file = True)},
)

def _runner_impl(ctx):
    tree = ctx.file.tree
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    checks = "".join([
        'check "%s" "%s"\n' % (path, content)
        for path, content in sorted(ctx.attr.expect.items())
    ])
    ctx.actions.write(
        executable,
        "#!/bin/bash\nset -e\n" + _LISTING + r"""
status=0
check() {
  if [ "$(cat "$tree/$1")" = "$2" ]; then echo "ok $1"; else echo "mismatch $1"; status=1; fi
}
tree="%s"
listing "$tree"
""" % tree.short_path + checks + "exit $status\n",
        is_executable = True,
    )
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = [tree]),
    )]

_RUNNER_ATTRS = {
    "expect": attr.string_dict(),
    "tree": attr.label(allow_single_file = True),
}

# An executable whose runfiles hold the tree: prints its listing as seen
# through the runfiles tree and checks `expect` (path: content).
runner = rule(
    implementation = _runner_impl,
    executable = True,
    attrs = _RUNNER_ATTRS,
)

runner_test = rule(
    implementation = _runner_impl,
    test = True,
    attrs = _RUNNER_ATTRS,
)
