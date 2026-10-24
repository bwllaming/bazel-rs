"""Two-argument map_each(item, expander) (StarlarkCustomCommandLine.java:
1085-1188: a StarlarkFunction with two or more parameters gets a
DirectoryExpander; expand(file) lists a tree artifact input's files, or just
the file)."""

def _tree(ctx, name):
    d = ctx.actions.declare_directory(name)
    ctx.actions.run_shell(
        outputs = [d],
        command = "mkdir -p {d}/sub && echo a > {d}/a && echo b > {d}/sub/b && echo c > {d}/c".format(d = d.path),
        mnemonic = "Tree",
    )
    return d

def expand_paths(f, expander):
    return ["%s<-%s" % (x.path, f.basename) for x in expander.expand(f)]

def describe(f, expander):
    return "%s:%s:%s:%d" % (type(expander), f.is_directory, f.short_path, len(expander.expand(f)))

def with_rest(f, *rest):
    return "%s rest=%d" % (f.basename, len(rest))

def one(f):
    return f.basename

def _probe_impl(ctx):
    d = _tree(ctx, ctx.label.name + "_d")
    src = ctx.file.src
    runfiles_dir = "app.runfiles"

    # aspect_rules_js js_image_layer's map_entry: a closure over directories
    # expanded before it is called.
    def map_entry(f, expander):
        if not f.is_directory:
            return "%s/%s|%s|%s" % (runfiles_dir, f.short_path, f.root.path, f.owner.name)
        return [x.path for x in expander.expand(f)]

    args = ctx.actions.args()
    args.set_param_file_format("multiline")
    args.add_all([src], map_each = expand_paths)
    args.add_all([src, d], map_each = describe, expand_directories = True)
    args.add_all([d], map_each = expand_paths, expand_directories = False)
    args.add_all([d], map_each = describe, expand_directories = False)
    args.add_all([src, d], map_each = map_entry, allow_closure = True, expand_directories = True, before_each = ",")
    args.add_all([d], map_each = lambda f, e: [x.basename for x in e.expand(f)], allow_closure = True, expand_directories = False)
    args.add_all([src, d], map_each = with_rest, expand_directories = True)
    args.add_all([d], map_each = one, expand_directories = True)
    args.add_all([d], map_each = str, expand_directories = True)
    args.add_joined([src, d], join_with = ";", map_each = describe, expand_directories = True)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, args)

    # The same in a spawn's command line.
    cmd = ctx.actions.args()
    cmd.add_all([d], map_each = expand_paths, expand_directories = False)
    cmd.add_all([d], map_each = map_entry, allow_closure = True)
    listed = ctx.actions.declare_file(ctx.label.name + ".args")
    ctx.actions.run_shell(
        inputs = [d],
        outputs = [listed],
        arguments = [cmd],
        command = "for a in \"$@\"; do echo \"$a\"; done > " + listed.path,
        mnemonic = "Listed",
    )
    return [DefaultInfo(files = depset([out, listed]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {"src": attr.label(allow_single_file = True, default = "a.txt")},
)

def _not_input_impl(ctx):
    d = _tree(ctx, ctx.label.name + "_d")
    other = _tree(ctx, ctx.label.name + "_other")

    def expand_other(f, expander):
        return [x.path for x in expander.expand(other)]

    args = ctx.actions.args()
    args.add_all([d], map_each = expand_other, allow_closure = True, expand_directories = False)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [d],
        outputs = [out],
        arguments = [args],
        command = "echo \"$@\" > " + out.path,
        mnemonic = "NotInput",
    )
    return [DefaultInfo(files = depset([out, other]))]

# expand() of a directory that is no input of the action.
not_input = rule(implementation = _not_input_impl)
