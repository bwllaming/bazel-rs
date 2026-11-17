# A tree artifact passed to add_all(expand_directories = True) with a
# map_each that takes a DirectoryExpander. At execution the tree is
# expanded first, so map_each sees its files; without input metadata
# (aquery) Bazel maps the directory itself with a NoopExpander
# (StarlarkCustomCommandLine.java:577-595, 1090-1097). `bad` reads
# tree_relative_path of what the expander lists there, like rules_js's
# js_image_layer.bzl:495, which fails for the directory itself.
def _make(use_relative):
    def _impl(ctx):
        tree = ctx.actions.declare_directory(ctx.label.name + "_tree")
        ctx.actions.run_shell(outputs = [tree], command = "echo x > %s/a.txt" % tree.path)

        def map_entry(f, expander):
            if not f.is_directory:
                return "file:" + f.path
            contents = []
            for c in expander.expand(f):
                contents.append("child:" + (c.tree_relative_path if use_relative else c.path))
            return contents

        args = ctx.actions.args()
        args.add_all([tree], expand_directories = True, map_each = map_entry, allow_closure = True)
        out = ctx.actions.declare_file(ctx.label.name + ".out")
        args.add(out)
        ctx.actions.run_shell(
            inputs = [tree],
            outputs = [out],
            arguments = [args],
            command = "echo \"$@\" > ${@: -1}",
            mnemonic = "Consume",
        )
        return [DefaultInfo(files = depset([out]))]
    return _impl

bad = rule(implementation = _make(True))
good = rule(implementation = _make(False))
