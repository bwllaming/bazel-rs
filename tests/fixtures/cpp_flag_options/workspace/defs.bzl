"""The report build setting (rule(build_setting = config.string(flag = True)),
the string_flag definition itself; Bazel 9.3 has no native string_flag
symbol)."""

BuildSettingInfo = provider(doc = "x", fields = ["value"])

def _report_setting_impl(ctx):
    return [BuildSettingInfo(value = ctx.build_setting_value)]

report_setting = rule(
    implementation = _report_setting_impl,
    build_setting = config.string(flag = True),
)

_ALLOW = attr.label(default = Label("//tools/allowlists/function_transition_allowlist"))

"""Starlark transitions on the CppOptions flag lists and --crosstool_top
(N66, tests/fixtures/cpp_flag_options).

A `*_writer` rule's incoming transition writes the native option; its child
dependency (no `cfg`) is analyzed in the written configuration, and the
child's own transition reads the option back into the report build setting,
so each report file holds the AFTER value. The output paths carry the ST-
hash of the transitioned configuration.
"""

def _cpp_flags_impl(settings, attr):
    return {
        "//command_line_option:copt": attr.copts,
        "//command_line_option:cxxopt": attr.cxxopts,
        "//command_line_option:conlyopt": attr.conlyopts,
        "//command_line_option:linkopt": attr.linkopts,
    }

cpp_flags_transition = transition(
    implementation = _cpp_flags_impl,
    inputs = [],
    outputs = [
        "//command_line_option:copt",
        "//command_line_option:cxxopt",
        "//command_line_option:conlyopt",
        "//command_line_option:linkopt",
    ],
)

def _cpp_report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(
        out,
        repr((
            ctx.fragments.cpp.copts,
            ctx.fragments.cpp.cxxopts,
            ctx.fragments.cpp.conlyopts,
            ctx.fragments.cpp.linkopts,
        )),
    )
    return [DefaultInfo(files = depset([out]))]

cpp_report = rule(
    implementation = _cpp_report_impl,
    cfg = cpp_flags_transition,
    fragments = ["cpp"],
    attrs = {
        "copts": attr.string_list(default = []),
        "cxxopts": attr.string_list(default = []),
        "conlyopts": attr.string_list(default = []),
        "linkopts": attr.string_list(default = []),
        "_allowlist_function_transition": _ALLOW,
    },
)

def _per_file_write_impl(settings, attr):
    return {"//command_line_option:per_file_copt": attr.per_file_copt}

per_file_write = transition(
    implementation = _per_file_write_impl,
    inputs = [],
    outputs = ["//command_line_option:per_file_copt"],
)

def _host_write_impl(settings, attr):
    return {
        "//command_line_option:host_copt": attr.host_copt,
        "//command_line_option:host_cxxopt": attr.host_cxxopt,
    }

host_write = transition(
    implementation = _host_write_impl,
    inputs = [],
    outputs = [
        "//command_line_option:host_copt",
        "//command_line_option:host_cxxopt",
    ],
)

def _crosstool_write_impl(settings, attr):
    return {"//command_line_option:crosstool_top": attr.crosstool_top}

crosstool_write = transition(
    implementation = _crosstool_write_impl,
    inputs = [],
    outputs = ["//command_line_option:crosstool_top"],
)

def _bad_write_impl(settings, attr):
    kind = attr.kind
    if kind == "per_file":
        return {
            "//command_line_option:per_file_copt": attr.value,
            "//command_line_option:copt": [],
        }
    if kind == "copt_none":
        return {"//command_line_option:per_file_copt": [], "//command_line_option:copt": None}
    if kind == "copt_int":
        return {"//command_line_option:per_file_copt": [], "//command_line_option:copt": [1]}
    if kind == "copt_string_value":
        return {"//command_line_option:per_file_copt": [], "//command_line_option:copt": attr.value[0]}
    fail("unknown kind")

bad_write = transition(
    implementation = _bad_write_impl,
    inputs = [],
    outputs = ["//command_line_option:per_file_copt", "//command_line_option:copt"],
)

def _forward_impl(ctx):
    return [DefaultInfo(files = ctx.attr.child[DefaultInfo].files)]

def _writer_rule(transition, extra_attrs = {}):
    return rule(
        implementation = _forward_impl,
        cfg = transition,
        attrs = dict(
            {"child": attr.label(mandatory = True), "_allowlist_function_transition": _ALLOW},
            **extra_attrs
        ),
    )

per_file_writer = _writer_rule(per_file_write, {"per_file_copt": attr.string_list(default = [])})
host_writer = _writer_rule(host_write, {
    "host_copt": attr.string_list(default = []),
    "host_cxxopt": attr.string_list(default = []),
})
crosstool_writer = _writer_rule(crosstool_write, {"crosstool_top": attr.string(default = "")})
bad_writer = _writer_rule(bad_write, {
    "kind": attr.string(),
    "value": attr.string_list(default = []),
})

# The readers: a transition that reads the option (the AFTER value of its
# incoming configuration) and writes it into the report build setting.
def _per_file_read_impl(settings, attr):
    return {"//:report": repr(settings["//command_line_option:per_file_copt"])}

def _host_read_impl(settings, attr):
    return {
        "//:report": repr((
            settings["//command_line_option:host_copt"],
            settings["//command_line_option:host_cxxopt"],
        )),
    }

def _crosstool_read_impl(settings, attr):
    return {"//:report": str(settings["//command_line_option:crosstool_top"])}

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, str(ctx.attr.setting[BuildSettingInfo].value))
    return [DefaultInfo(files = depset([out]))]

def _reader_rule(transition):
    return rule(
        implementation = _report_impl,
        cfg = transition,
        attrs = {
            "setting": attr.label(default = Label("//:report")),
            "_allowlist_function_transition": _ALLOW,
        },
    )

def _reader(impl, options):
    return transition(
        implementation = impl,
        inputs = options,
        outputs = ["//:report"],
    )

per_file_reader = _reader_rule(_reader(_per_file_read_impl, ["//command_line_option:per_file_copt"]))
host_reader = _reader_rule(_reader(_host_read_impl, [
    "//command_line_option:host_copt",
    "//command_line_option:host_cxxopt",
]))
crosstool_reader = _reader_rule(_reader(_crosstool_read_impl, ["//command_line_option:crosstool_top"]))

# An exec dependency: the tool (a cpp_report) is analyzed in the exec
# configuration of this rule's own configuration, so its output path shows
# the exec transition's `--host_copt`/`--host_cxxopt` + `-g0`.
def _exec_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, repr(ctx.attr.tool[DefaultInfo].files.to_list()[0].path))
    return [DefaultInfo(files = depset([out]))]

exec_report = rule(
    implementation = _exec_impl,
    attrs = {"tool": attr.label(cfg = "exec")},
)

exec_host_report = rule(
    implementation = _exec_impl,
    cfg = host_write,
    attrs = {
        "tool": attr.label(cfg = "exec"),
        "host_copt": attr.string_list(default = []),
        "host_cxxopt": attr.string_list(default = []),
        "_allowlist_function_transition": _ALLOW,
    },
)
