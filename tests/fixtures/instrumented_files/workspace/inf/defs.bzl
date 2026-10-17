# N54 fixture: coverage_common.instrumented_files_info and InstrumentedFilesInfo
# without --collect_code_coverage (InstrumentedFilesInfo.EMPTY), native
# filegroup/genrule/output-file forwarding, parameter validation, and a
# Starlark-transition coverage configuration (native: explicit).
def _lib_impl(ctx):
    info = coverage_common.instrumented_files_info(
        ctx,
        source_attributes = ["srcs"],
        dependency_attributes = ["deps", "data"],
        extensions = ["go"],
    )
    out = ctx.actions.declare_file(ctx.label.name + ".a")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out])), info]

lib = rule(_lib_impl, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "data": attr.label_list(allow_files = True),
})

def _outlib_impl(ctx):
    ctx.actions.write(ctx.outputs.out, ctx.label.name)
    return [coverage_common.instrumented_files_info(ctx, source_attributes = ["srcs"])]

outlib = rule(_outlib_impl, attrs = {"srcs": attr.label_list(allow_files = True), "out": attr.output()})

def _plain_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".p")
    ctx.actions.write(out, "p")
    return [DefaultInfo(files = depset([out]))]

plain = rule(_plain_impl)

def _describe(d):
    if InstrumentedFilesInfo not in d:
        return "%s: absent" % d.label.name
    i = d[InstrumentedFilesInfo]
    return "%s: %s %s %s %s %s" % (d.label.name, type(i), repr(i), dir(i), i.instrumented_files.to_list(), repr(i.metadata_files))

def _probe_impl(ctx):
    f = ctx.file.src
    full = coverage_common.instrumented_files_info(
        ctx = ctx,
        source_attributes = [],
        dependency_attributes = [],
        coverage_support_files = depset(),
        coverage_environment = {},
        extensions = None,
        metadata_files = [f],
        reported_to_actual_sources = None,
        baseline_coverage_files = [f],
    )
    plain = coverage_common.instrumented_files_info(ctx)
    keys = {}
    keys[full] = 1
    keys[plain] = 2
    lines = [_describe(d) for d in ctx.attr.deps] + [
        "full=%r plain=%r eq=%s hash=%d" % (full, plain, full == plain, len(keys)),
        "provider=%s %s %s" % (InstrumentedFilesInfo, type(InstrumentedFilesInfo), InstrumentedFilesInfo == InstrumentedFilesInfo),
        "files_type=%s %s" % (type(full.instrumented_files), full.instrumented_files == plain.instrumented_files),
        "empty_ext=%r" % coverage_common.instrumented_files_info(ctx, extensions = [], source_attributes = ["src", "nope"]),
        "instrumented=%s" % ctx.coverage_instrumented(),
        "dep_instrumented=%s" % [ctx.coverage_instrumented(d) for d in ctx.attr.deps],
        "coverage_enabled=%s" % ctx.configuration.coverage_enabled,
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(_probe_impl, attrs = {"deps": attr.label_list(allow_files = True), "src": attr.label(allow_single_file = True)})

def _bad_impl(ctx):
    f = ctx.file.src
    k = ctx.attr.kind
    c = coverage_common.instrumented_files_info
    if k == "ctx_string":
        c("x")
    elif k == "source_string":
        c(ctx, source_attributes = "srcs")
    elif k == "source_int":
        c(ctx, source_attributes = ["a", 1])
    elif k == "deps_none":
        c(ctx, dependency_attributes = [None])
    elif k == "ext_string":
        c(ctx, extensions = "go")
    elif k == "ext_int":
        c(ctx, extensions = [1])
    elif k == "metadata_string":
        c(ctx, metadata_files = ["x"])
    elif k == "metadata_depset":
        c(ctx, metadata_files = depset([f]))
    elif k == "baseline_string":
        c(ctx, baseline_coverage_files = ["x"])
    elif k == "reported_strings":
        c(ctx, reported_to_actual_sources = depset(["x"]))
    elif k == "reported_list":
        c(ctx, reported_to_actual_sources = [])
    elif k == "reported_tuples":
        c(ctx, reported_to_actual_sources = depset([("a", "b")]))
    elif k == "env_private":
        c(ctx, coverage_environment = {"A": "B"})
    elif k == "env_int":
        c(ctx, coverage_environment = {1: "x"})
    elif k == "env_list":
        c(ctx, coverage_environment = [])
    elif k == "support_int":
        c(ctx, coverage_support_files = [1])
    elif k == "support_strings":
        c(ctx, coverage_support_files = depset(["x"]))
    elif k == "support_file":
        c(ctx, coverage_support_files = [f])
    elif k == "support_string_arg":
        c(ctx, coverage_support_files = "x")
    elif k == "unknown_kw":
        c(ctx, sources = [])
    elif k == "two_positional":
        c(ctx, [])
    elif k == "missing_ctx":
        c()
    elif k == "construct":
        InstrumentedFilesInfo()
    elif k == "get_env":
        c(ctx).get_coverage_environment()
    elif k == "get_support":
        c(ctx).get_coverage_support_files()
    elif k == "order":
        c(ctx, source_attributes = [1], extensions = [2])
    return []

bad = rule(_bad_impl, attrs = {"kind": attr.string(), "src": attr.label(allow_single_file = True, default = "//inf:a.go")})

def _cov_impl(settings, attr):
    return {"//command_line_option:collect_code_coverage": True}

_cov = transition(implementation = _cov_impl, inputs = [], outputs = ["//command_line_option:collect_code_coverage"])

def _cov_edge_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.dep]))]

cov_edge = rule(_cov_edge_impl, attrs = {"dep": attr.label(cfg = _cov)})
