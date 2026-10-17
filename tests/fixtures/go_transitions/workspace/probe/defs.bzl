# N47 fixture: the configuration a target sees below rules_go 0.63.0's genuine
# transitions (//go/private/rules:transition.bzl, an unmodified copy shared
# with tests/fixtures/starlark_transitions/go_shaped) and below transitions on
# each //command_line_option rules_go touches (platforms, compilation_mode,
# collect_code_coverage). `cfg_probe` writes one report per configuration.
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load(
    "//go/private/rules:transition.bzl",
    "go_cross_transition",
    "go_stdlib_transition",
    "go_tool_transition",
    "go_transition",
    "non_go_tool_transition",
    "non_go_transition",
    "non_request_nogo_transition",
)

_FLAGS = {
    "static": "//go/config:static",
    "race": "//go/config:race",
    "msan": "//go/config:msan",
    "pure": "//go/config:pure",
    "debug": "//go/config:debug",
    "linkmode": "//go/config:linkmode",
    "tags": "//go/config:tags",
    "request_nogo": "//go/private:request_nogo",
    "bootstrap_nogo": "//go/private:bootstrap_nogo",
    "original_static": "//go/private/rules:original_static",
    "original_race": "//go/private/rules:original_race",
    "original_pure": "//go/private/rules:original_pure",
    "original_tags": "//go/private/rules:original_tags",
    "original_pgoprofile": "//go/private/rules:original_pgoprofile",
    "sdk_version": "//go/toolchain:sdk_version",
    "cov_seen": "//probe:cov_seen",
}

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".leaf")
    ctx.actions.write(out, "leaf coverage_enabled=%s\n" % ctx.configuration.coverage_enabled)
    return [DefaultInfo(files = depset([out]))]

cfg_leaf = rule(_leaf_impl)

def _probe_impl(ctx):
    lines = [
        "probe " + ctx.label.name,
        "bin_dir=" + ctx.bin_dir.path,
        "coverage_enabled=%s" % ctx.configuration.coverage_enabled,
        "COMPILATION_MODE=" + ctx.var["COMPILATION_MODE"],
        "BINDIR=" + ctx.var["BINDIR"],
        "platform=%s" % ctx.fragments.platform.platform,
        "pgoprofile=%s" % ctx.attr._pgoprofile.label,
        "exec_tool=%s" % ctx.file._exec_tool.path,
    ]
    for name, _ in sorted(_FLAGS.items()):
        lines.append("%s=%r" % (name, getattr(ctx.attr, "_" + name)[BuildSettingInfo].value))
    out = ctx.actions.declare_file(ctx.label.name + "." + ctx.attr.tag + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

cfg_probe = rule(
    _probe_impl,
    attrs = dict(
        {"_" + name: attr.label(default = label) for name, label in _FLAGS.items()},
        tag = attr.string(default = "t"),
        _pgoprofile = attr.label(default = "//go/config:pgoprofile"),
        _exec_tool = attr.label(default = "//probe:leaf", cfg = "exec", allow_single_file = True),
    ),
    fragments = ["platform"],
)

def _forward(ctx):
    deps = ctx.attr.dep if type(ctx.attr.dep) == "list" else [ctx.attr.dep]
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in deps]))]

def _wrapper(transition, extra = {}):
    return rule(
        _forward,
        attrs = dict(extra, dep = attr.label(mandatory = True, cfg = transition)),
    )

# rules_go's attribute transitions as rules_go applies them.
stdlib_edge = _wrapper(go_stdlib_transition)
go_tool_edge = _wrapper(go_tool_transition)
non_go_tool_edge = _wrapper(non_go_tool_transition)
non_go_edge = _wrapper(non_go_transition)
non_request_nogo_edge = _wrapper(non_request_nogo_transition)

# go_transition as go_binary's rule transition (binary.bzl), with go_binary's
# transition-read attributes; `dep` crosses non_go_transition (data) and
# `stdlib` go_stdlib_transition, as go_binary's deps do.
def _go_binary_like_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [
        d[DefaultInfo].files
        for d in ctx.attr.data + ctx.attr.stdlib + ctx.attr.direct
    ]))]

go_binary_like = rule(
    _go_binary_like_impl,
    attrs = {
        "data": attr.label_list(cfg = non_go_transition),
        "stdlib": attr.label_list(cfg = go_stdlib_transition),
        "direct": attr.label_list(),
        "goos": attr.string(default = "auto"),
        "goarch": attr.string(default = "auto"),
        "race": attr.string(default = "auto"),
        "msan": attr.string(default = "auto"),
        "static": attr.string(default = "auto"),
        "pure": attr.string(default = "auto"),
        "gotags": attr.string_list(),
        "linkmode": attr.string(default = "auto"),
        "pgoprofile": attr.label(allow_files = True, default = "//go/config:empty"),
    },
    cfg = go_transition,
)

# go_cross_binary's rule transition (cross.bzl): sdk_version, compilation_mode
# and platform attributes.
def _cross_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.direct]))]

go_cross_like = rule(
    _cross_impl,
    attrs = {
        "direct": attr.label_list(),
        "platform": attr.label(),
        "sdk_version": attr.string(),
        "compilation_mode": attr.string(default = ""),
    },
    cfg = go_cross_transition,
)

# Transitions on --collect_code_coverage and the other rules_go options in
# their Starlark input/output forms.
def _cov_impl(settings, attr):
    out = {"//command_line_option:collect_code_coverage": attr.value}
    if attr.value == "@bool_true":
        out["//command_line_option:collect_code_coverage"] = True
    elif attr.value == "@bool_false":
        out["//command_line_option:collect_code_coverage"] = False
    elif attr.value == "@none":
        out["//command_line_option:collect_code_coverage"] = None
    elif attr.value == "@int":
        out["//command_line_option:collect_code_coverage"] = 1
    elif attr.value == "@toggle":
        out["//command_line_option:collect_code_coverage"] = not settings["//command_line_option:collect_code_coverage"]
    out["//probe:cov_seen"] = "%s:%r" % (
        type(settings["//command_line_option:collect_code_coverage"]),
        settings["//command_line_option:collect_code_coverage"],
    )
    return out

_cov_transition = transition(
    implementation = _cov_impl,
    inputs = ["//command_line_option:collect_code_coverage"],
    outputs = ["//command_line_option:collect_code_coverage", "//probe:cov_seen"],
)

cov_edge = _wrapper(_cov_transition, {"value": attr.string()})

def _opt_impl(settings, attr):
    out = {}
    if attr.mode:
        out["//command_line_option:compilation_mode"] = attr.mode
    else:
        out["//command_line_option:compilation_mode"] = settings["//command_line_option:compilation_mode"]
    if attr.platforms:
        out["//command_line_option:platforms"] = attr.platforms if len(attr.platforms) > 1 else attr.platforms[0]
    else:
        out["//command_line_option:platforms"] = settings["//command_line_option:platforms"]
    out["//probe:cov_seen"] = "mode=%s:%r platforms=%s:%r" % (
        type(settings["//command_line_option:compilation_mode"]),
        settings["//command_line_option:compilation_mode"],
        type(settings["//command_line_option:platforms"]),
        [str(p) for p in settings["//command_line_option:platforms"]],
    )
    return out

_opt_transition = transition(
    implementation = _opt_impl,
    inputs = ["//command_line_option:compilation_mode", "//command_line_option:platforms"],
    outputs = ["//command_line_option:compilation_mode", "//command_line_option:platforms", "//probe:cov_seen"],
)

opt_edge = _wrapper(_opt_transition, {"mode": attr.string(), "platforms": attr.string_list()})

# Coverage instrumentation queries in a coverage-enabled configuration.
def _instrumented_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".inst")
    ctx.actions.write(out, "instrumented=%s\n" % ctx.coverage_instrumented())
    return [DefaultInfo(files = depset([out]))]

instrumented_probe = rule(_instrumented_impl)

def _generator_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.write(out, "output_generator=%s\n" % ctx.fragments.coverage.output_generator)
    return [DefaultInfo(files = depset([out]))]

generator_probe = rule(_generator_impl, fragments = ["coverage"])
