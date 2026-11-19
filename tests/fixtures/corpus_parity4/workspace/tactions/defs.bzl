# B46: Target.actions argv of spawns whose Args genuine expands at analysis
# without input metadata: a bare `%s` parameter file, tree artifacts (kept
# as their paths, or mapped by map_each with a NoopExpander).

def _short(file):
    return file.short_path

def _tree_children(directory, expander):
    return [f.path for f in expander.expand(directory)]

def _tree_name(directory):
    return "dir=" + directory.basename

def _subject_impl(ctx):
    tree = ctx.actions.declare_directory(ctx.label.name + "_tree")
    ctx.actions.run_shell(
        outputs = [tree],
        command = "mkdir -p $1 && echo a > $1/a.txt",
        arguments = [tree.path],
        mnemonic = "MakeTree",
    )
    tool = ctx.file.tool

    # A parameter file named by its bare path (`use_param_file("%s")`).
    launch = ctx.actions.args().use_param_file("%s", use_always = True).set_param_file_format("multiline")
    launch.add("binary_type=Java")
    launch.add_joined([tool], map_each = _short, join_with = ";", format_joined = "classpath=%s")
    launch.add_joined(["-Da=1", "-Db=2"], join_with = "\t", format_joined = "jvm_flags=%s")
    out1 = ctx.actions.declare_file(ctx.label.name + ".launch")
    ctx.actions.run(
        executable = tool,
        outputs = [out1],
        arguments = ["first", launch, out1.path],
        mnemonic = "Launch",
    )

    # Tree artifacts: deferred to execution, at analysis their paths.
    plain = ctx.actions.args()
    plain.add("--dirs")
    plain.add_all([tree])
    plain.add_all([tree], map_each = _tree_name, format_each = "--name=%s")
    plain.add_all([tree], map_each = _tree_children, before_each = "--child")
    out2 = ctx.actions.declare_file(ctx.label.name + ".trees")
    ctx.actions.run(
        executable = tool,
        inputs = [tree],
        outputs = [out2],
        arguments = [plain, out2.path],
        mnemonic = "Trees",
    )

    # The same with a parameter file.
    spilled = ctx.actions.args().use_param_file("@%s", use_always = True)
    spilled.add_all([tree], map_each = _tree_name)
    spilled.add("last")
    out3 = ctx.actions.declare_file(ctx.label.name + ".spilled")
    ctx.actions.run(
        executable = tool,
        inputs = [tree],
        outputs = [out3],
        arguments = [spilled],
        mnemonic = "Spilled",
    )
    return [DefaultInfo(files = depset([out1, out2, out3]))]

subject = rule(
    implementation = _subject_impl,
    attrs = {"tool": attr.label(allow_single_file = True, executable = True, cfg = "exec")},
)

def _check_impl(ctx):
    lines = []
    for action in ctx.attr.target.actions:
        lines.append("%s %r" % (action.mnemonic, action.argv))
    return [AnalysisTestResultInfo(success = True, message = "\n".join(lines))]

def check(name, target):
    testing.analysis_test(
        name = name,
        implementation = _check_impl,
        attrs = {"target": attr.label()},
        attr_values = {"target": target},
    )
