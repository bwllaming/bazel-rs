J = java_common.internal_DO_NOT_USE()
Options = provider(fields = ["values", "ctx"])

def _variables(ctx):
    return [platform_common.ToolchainInfo(), platform_common.TemplateVariableInfo({"TOOL": ctx.attr.value, "CHOICE": "toolchain"})]

variables = rule(_variables, attrs = {"value": attr.string()})

def _probe(ctx):
    case = ctx.attr.case
    if case == "missing_tokenize":
        values = J.expand_java_opts(ctx, "misc")
    elif case == "positional_tokenize":
        values = J.expand_java_opts(ctx, "misc", True)
    elif case == "named_ctx":
        values = J.expand_java_opts(ctx = ctx, attr = "misc", tokenize = True)
    elif case == "bad_ctx":
        values = J.expand_java_opts(None, "misc", tokenize = True)
    elif case == "bad_attr":
        values = J.expand_java_opts(ctx, 42, tokenize = True)
    elif case == "bad_tokenize":
        values = J.expand_java_opts(ctx, "misc", tokenize = 1)
    elif case == "bad_exec":
        values = J.expand_java_opts(ctx, "misc", tokenize = True, exec_paths = "yes")
    elif case == "escape":
        return [Options(values = [], ctx = ctx)]
    elif case == "escaped":
        values = J.expand_java_opts(ctx.attr.dep[Options].ctx, "misc", tokenize = True)
    elif ctx.attr.exec_paths:
        values = J.expand_java_opts(ctx, ctx.attr.attribute, tokenize = ctx.attr.tokenize, exec_paths = ctx.attr.exec_paths)
    else:
        values = J.expand_java_opts(ctx, ctx.attr.attribute, tokenize = ctx.attr.tokenize)
    if case == "mutate":
        values.append("bad")
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode(values))
    return [DefaultInfo(files = depset([out])), Options(values = values, ctx = None)]

_attrs = {
    "misc": attr.string_list(),
    "javacopts": attr.string_list(),
    "attribute": attr.string(default = "misc"),
    "tokenize": attr.bool(default = True),
    "exec_paths": attr.bool(),
    "case": attr.string(),
    "dep": attr.label(),
    "srcs": attr.label_list(allow_files = True),
    "data": attr.label_list(allow_files = True),
    "deps": attr.label_list(allow_files = True),
    "ints": attr.int_list(),
}
probe = rule(_probe, attrs = _attrs)
resolved = rule(_probe, attrs = _attrs, toolchains = ["//:type"])

def _generated(ctx):
    files = [ctx.actions.declare_file(ctx.label.name + suffix) for suffix in ["-a.txt", "-b.txt"]]
    for file in files:
        ctx.actions.write(file, "generated")
    return [DefaultInfo(files = depset(files))]

generated = rule(_generated)

def _consume(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".args")
    args = ctx.actions.args()
    args.add(out.path)
    args.add_all(ctx.attr.dep[Options].values)
    ctx.actions.run_shell(outputs = [out], arguments = [args], command = 'out="$1"; shift; printf "<%s>\\n" "$@" > "$out"')
    return [DefaultInfo(files = depset([out]))]

consume = rule(_consume, attrs = {"dep": attr.label(mandatory = True)})
