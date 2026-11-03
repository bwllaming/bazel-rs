Info = provider(fields = ["files", "names"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".o")
    ctx.actions.write(out, ctx.label.name)
    files = depset([out] + ctx.files.srcs, transitive = [d[Info].files for d in ctx.attr.deps])
    names = depset([ctx.label.name], transitive = [d[Info].names for d in ctx.attr.deps], order = ctx.attr.order)
    return [DefaultInfo(files = depset([out])), Info(files = files, names = names)]

leaf = rule(
    implementation = _leaf_impl,
    attrs = {
        "deps": attr.label_list(providers = [Info]),
        "srcs": attr.label_list(allow_files = True),
        "order": attr.string(default = "default"),
    },
)

def _short(f):
    return f.basename

def _maybe(s):
    if s.startswith("b"):
        return None
    return [s + "1", s + "2"]

def _dirmap(f, expander):
    return [x.path for x in expander.expand(f)]

def _impl(ctx):
    files = depset(transitive = [d[Info].files for d in ctx.attr.deps])
    names = depset(["top"], transitive = [d[Info].names for d in ctx.attr.deps], order = ctx.attr.order)
    tree = ctx.actions.declare_directory(ctx.label.name + ".dir")
    ctx.actions.run_shell(outputs = [tree], command = "mkdir -p %s && touch %s/x" % (tree.path, tree.path))
    outs = []
    def run(name, args, inputs = None):
        o = ctx.actions.declare_file(ctx.label.name + "." + name)
        outs.append(o)
        ctx.actions.run(
            outputs = [o],
            inputs = inputs or depset(),
            executable = ctx.executable.tool,
            arguments = args + [o.path],
            mnemonic = "M" + name,
        )

    a = ctx.actions.args()
    a.add_all(files)
    run("plain", [a], files)

    a = ctx.actions.args()
    a.add_all(files, map_each = _short, uniquify = True, before_each = "-f")
    a.add_joined("--names", names, join_with = ",", format_joined = "[%s]")
    a.add_all(names, map_each = _maybe, format_each = "n=%s", omit_if_empty = True, terminate_with = "--")
    run("mapped", [a], files)

    a = ctx.actions.args()
    a.add_all("--empty", depset([]), omit_if_empty = False)
    a.add_all(depset(["only"]))
    a.add_all(depset(["x", "y", "x"], order = "preorder"))
    a.add_all(depset([1, 2, 3]))
    run("misc", [a])

    a = ctx.actions.args()
    a.add_all(depset([tree], transitive = [files]), expand_directories = True)
    a.add_all(depset([tree]), expand_directories = False, format_each = "d=%s")
    run("tree", [a], depset([tree], transitive = [files]))

    a = ctx.actions.args()
    a.add_all(depset([tree]), map_each = _dirmap)
    run("treemap", [a], depset([tree]))

    a = ctx.actions.args()
    a.add_all(files)
    a.use_param_file("@%s", use_always = True)
    a.set_param_file_format("multiline")
    run("param", [a], files)

    pf = ctx.actions.declare_file(ctx.label.name + ".params")
    a = ctx.actions.args()
    a.add_all(files, map_each = _short)
    a.add_all(names)
    ctx.actions.write(pf, a)
    outs.append(pf)

    for n, text in [("w255", "a" * 255), ("w256", "b" * 256), ("w257", "c" * 257), ("wlong", "line %d\n" * 20 + "\n".join(["entry-%d" % i for i in range(300)])), ("wuni", "é中" * 200)]:
        w = ctx.actions.declare_file(ctx.label.name + "." + n)
        ctx.actions.write(w, text, is_executable = n == "w257")
        outs.append(w)
    return [DefaultInfo(files = depset(outs))]

dep_args = rule(
    implementation = _impl,
    attrs = {
        "deps": attr.label_list(providers = [Info]),
        "order": attr.string(default = "default"),
        "tool": attr.label(executable = True, cfg = "exec", default = ":tool"),
    },
)
