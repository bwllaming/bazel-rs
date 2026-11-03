def _impl(ctx):
    a = ctx.actions.declare_file(ctx.label.name + ".a")
    b = ctx.actions.declare_file(ctx.label.name + ".b")
    c = ctx.actions.declare_file(ctx.label.name + ".c")
    w = ctx.actions.declare_file(ctx.label.name + ".w")
    t = ctx.actions.declare_file(ctx.label.name + ".t")
    s = ctx.actions.declare_file(ctx.label.name + ".s")
    u = ctx.actions.declare_symlink(ctx.label.name + ".u")
    p = ctx.actions.declare_file(ctx.label.name + ".p")
    ctx.actions.run_shell(
        outputs = [a],
        inputs = ctx.files.srcs,
        command = "cat $1 > $2",
        arguments = [ctx.files.srcs[0].path, a.path],
        mnemonic = "Shell",
        progress_message = "Shelling %{label} to %{output}",
        env = {"Z": "1", "A": "x y"},
        execution_requirements = {"no-sandbox": "1"},
    )
    ctx.actions.run_shell(outputs = [b], command = "touch " + b.path)
    args = ctx.actions.args()
    args.add("--out", c)
    args.add_all(ctx.files.srcs)
    ctx.actions.run(
        outputs = [c],
        inputs = ctx.files.srcs,
        executable = ctx.executable.tool,
        arguments = ["plain", args],
        mnemonic = "RunTool",
        use_default_shell_env = True,
    )
    ctx.actions.write(w, "hello\nworld\n", is_executable = True)
    ctx.actions.expand_template(template = ctx.file.tmpl, output = t, substitutions = {"{X}": "x", "{Y}": "yy"})
    ctx.actions.symlink(output = s, target_file = a)
    ctx.actions.symlink(output = u, target_path = "../some/where")
    pa = ctx.actions.args()
    pa.add("--flag")
    pa.add_all(ctx.files.srcs)
    pa.use_param_file("@%s", use_always = True)
    ctx.actions.run(outputs = [p], inputs = ctx.files.srcs, executable = ctx.executable.tool, arguments = [pa], mnemonic = "Params")
    return [DefaultInfo(files = depset([a, b, c, w, t, s, u, p]))]

kitchen = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "tmpl": attr.label(allow_single_file = True),
        "tool": attr.label(executable = True, cfg = "exec"),
    },
)

def _asp_impl(target, ctx):
    o = ctx.actions.declare_file(ctx.label.name + ".aspect")
    ctx.actions.write(o, str(ctx.label))
    return [OutputGroupInfo(asp = depset([o]))]

asp = aspect(implementation = _asp_impl, attr_aspects = ["deps"], attrs = {"p": attr.string(default = "v", values = ["v", "w"])})

def _user_impl(ctx):
    o = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(o, "u")
    return [DefaultInfo(files = depset([o]))]

user = rule(implementation = _user_impl, attrs = {"deps": attr.label_list(aspects = [asp])})

def _pw_impl(ctx):
    p = ctx.actions.declare_file(ctx.label.name + ".params")
    a = ctx.actions.args()
    a.add("--x", "y z")
    a.add_all(ctx.files.srcs, format_each = "--src=%s")
    a.add_joined("--j", ["a", "b"], join_with = ",")
    a.add("lbl", ctx.label)
    ctx.actions.write(p, a)
    o = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(outputs = [o], inputs = [p] + ctx.files.srcs, executable = ctx.executable.tool, arguments = ["@" + p.path, o.path], mnemonic = "UsesParams", execution_requirements = {"b": "2", "a": "1"}, env = {"Q": "1"}, use_default_shell_env = True)
    return [DefaultInfo(files = depset([p, o]))]

pw = rule(implementation = _pw_impl, attrs = {"srcs": attr.label_list(allow_files = True), "tool": attr.label(executable = True, cfg = "exec")})
