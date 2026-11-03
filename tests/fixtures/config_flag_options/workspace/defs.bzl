"""Configuration flag options read through ctx (tests/config_flag_options.rs):
--features/--host_features, the CppOptions flag lists, --dynamic_mode and
--javacopt/--host_javacopt, in the target and the exec configuration, and
through a Starlark transition."""

def _lines(ctx):
    cpp = ctx.fragments.cpp
    java = ctx.fragments.java
    return [
        "features=%r" % sorted(ctx.features),
        "disabled_features=%r" % sorted(ctx.disabled_features),
        "copts=%r" % cpp.copts,
        "cxxopts=%r" % cpp.cxxopts,
        "conlyopts=%r" % cpp.conlyopts,
        "linkopts=%r" % cpp.linkopts,
        "dynamic_mode=%r" % cpp.dynamic_mode(),
        "default_javac_flags=%r" % java.default_javac_flags,
        "default_javac_flags_depset=%r" % java.default_javac_flags_depset.to_list(),
    ]

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = _lines(ctx)
    files = [out]
    if ctx.attr.tool:
        lines.append("--- exec tool")
        lines.extend(ctx.attr.tool[ReportInfo].lines)
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset(files)), ReportInfo(lines = _lines(ctx))]

ReportInfo = provider(doc = "report lines", fields = ["lines"])

report = rule(
    implementation = _report_impl,
    fragments = ["cpp", "java"],
    attrs = {
        "tool": attr.label(cfg = "exec", providers = [ReportInfo]),
    },
)

def _flags_transition_impl(settings, attr):
    return {
        "//command_line_option:features": settings["//command_line_option:features"] + ["zz_transitioned", "-yy_off"],
        "//command_line_option:javacopt": settings["//command_line_option:javacopt"] + ["-Xep:Foo:OFF"],
    }

flags_transition = transition(
    implementation = _flags_transition_impl,
    inputs = [
        "//command_line_option:features",
        "//command_line_option:javacopt",
    ],
    outputs = [
        "//command_line_option:features",
        "//command_line_option:javacopt",
    ],
)

def _transitioned_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(_lines(ctx)) + "\n")
    return [DefaultInfo(files = depset([out]))]

transitioned = rule(
    implementation = _transitioned_impl,
    cfg = flags_transition,
    fragments = ["cpp", "java"],
    attrs = {
        "_allowlist_function_transition": attr.label(
            default = Label("//tools/allowlists/function_transition_allowlist"),
        ),
    },
)
