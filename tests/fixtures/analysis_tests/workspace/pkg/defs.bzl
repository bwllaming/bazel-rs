SubjectInfo = provider(fields = ["mode", "flag"])
ActionsInfo = provider(fields = ["actions", "bin_path"])

def _subject_impl(ctx):
    if ctx.attr.fail:
        fail("subject failed: " + ctx.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "hello " + ctx.var["COMPILATION_MODE"] + "\n", is_executable = True)
    cat = ctx.actions.declare_file(ctx.label.name + ".cat")
    args = ctx.actions.args()
    args.add(out)
    args.add("--x", "y z")
    args.add(cat)
    ctx.actions.run_shell(
        outputs = [cat],
        inputs = [out] + ctx.files.srcs,
        command = "cat $1 > $4",
        arguments = [args],
        mnemonic = "Cat",
        env = {"A": "1", "B": "two"},
        execution_requirements = {"no-remote": "1"},
        progress_message = "Catting %{label}",
    )
    tpl = ctx.actions.declare_file(ctx.label.name + ".exp")
    ctx.actions.expand_template(
        template = ctx.file.template,
        output = tpl,
        substitutions = {"{NAME}": ctx.label.name, "{MODE}": ctx.var["COMPILATION_MODE"]},
    )
    deps = [d[SubjectInfo].mode for d in ctx.attr.deps]
    return [
        DefaultInfo(files = depset([out, cat, tpl])),
        SubjectInfo(mode = ctx.var["COMPILATION_MODE"], flag = "",),
    ]

subject = rule(
    _subject_impl,
    attrs = {
        "fail": attr.bool(),
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "template": attr.label(allow_single_file = True, default = "//pkg:tpl.in"),
        "_flag": attr.label(default = "//pkg:flag"),
    },
)

def _actions_aspect_impl(target, ctx):
    return [ActionsInfo(actions = target.actions, bin_path = ctx.bin_dir.path)]

actions_aspect = aspect(implementation = _actions_aspect_impl, attr_aspects = [])

def _describe(a):
    lines = ["mnemonic=" + a.mnemonic]
    lines.append("argv=" + repr(a.argv))
    lines.append("content=" + repr(a.content))
    lines.append("substitutions=" + repr(a.substitutions))
    lines.append("inputs=" + repr([f.short_path for f in a.inputs.to_list()]))
    lines.append("outputs=" + repr([f.short_path for f in a.outputs.to_list()]))
    lines.append("env=" + repr(a.env))
    lines.append("type=" + type(a))
    return "\n  ".join(lines)

def _check_impl(ctx):
    t = ctx.attr.target_under_test
    lines = ["CHECK " + str(ctx.label) + " list=" + str(type(t) == "list")]
    if type(t) == "list":
        t = t[0]
    if AnalysisFailureInfo in t:
        for cause in t[AnalysisFailureInfo].causes.to_list():
            lines.append("cause label=" + str(cause.label) + " message=" + cause.message)
        lines.append("failure type=" + type(t[AnalysisFailureInfo]) + " causes type=" + type(t[AnalysisFailureInfo].causes))
    else:
        lines.append("mode=" + t[SubjectInfo].mode)
        lines.append("bin_path=" + t[ActionsInfo].bin_path)
        lines.append("files=" + repr([f.path for f in t.files.to_list()]))
        for a in t[ActionsInfo].actions:
            lines.append("action " + _describe(a))
    print("\n".join(lines))
    return [AnalysisTestResultInfo(success = ctx.attr.expect_success, message = "\n".join(lines))]

_ATTRS = {
    "expect_success": attr.bool(default = True),
}

def _make(settings):
    attrs = dict(_ATTRS)
    attrs["target_under_test"] = attr.label(
        aspects = [actions_aspect],
        mandatory = True,
        cfg = analysis_test_transition(settings = settings) if settings else None,
    )
    return rule(_check_impl, attrs = attrs, analysis_test = True)

opt_check_test = _make({"//command_line_option:compilation_mode": "opt", "//pkg:flag": "set"})
plain_check_test = _make({})
failure_check_test = _make({"//command_line_option:allow_analysis_failures": True})

def _ta_impl(env, target):
    pass

def _analysis_impl(ctx):
    t = ctx.attr.target
    t = t[0] if type(t) == "list" else t
    print("ANALYSIS_TEST " + str(ctx.label) + " mode=" + t[SubjectInfo].mode + " v=" + ctx.attr.value + " rule=" + str(ctx.attr._extra.label if ctx.attr._extra else None))
    return [AnalysisTestResultInfo(success = True, message = "ok")]

def make_analysis_test(name, target):
    testing.analysis_test(
        name = name,
        implementation = _analysis_impl,
        attrs = {
            "target": attr.label(cfg = analysis_test_transition(settings = {"//command_line_option:compilation_mode": "dbg"})),
            "value": attr.string(),
            "_extra": attr.label(default = "//pkg:tpl.in", allow_single_file = True),
        },
        attr_values = {"target": target, "value": "v1", "tags": ["manual_tag"]},
    )
