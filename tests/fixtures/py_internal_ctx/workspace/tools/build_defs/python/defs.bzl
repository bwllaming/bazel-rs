"""py_internal's ctx helpers on rules_python's py_binary path (Bazel 9.3
builtins_bzl/common/python/py_internal.bzl, rules/python/PyBuiltins.java):
is_tool_configuration, get_legacy_external_runfiles, is_bzlmod_enabled and
make_runfiles_respect_legacy_external_runfiles.

py_internal is available to @_builtins, @bazel_tools, rules_python and the
main repository's tools/build_defs/python (BazelRuleClassProvider).
"""

ProbeInfo = provider(fields = ["is_tool", "deps"])

def _leaf_impl(ctx):
    return [ProbeInfo(is_tool = py_internal.is_tool_configuration(ctx), deps = [])]

leaf = rule(implementation = _leaf_impl)

def _duck():
    return "duck"

def _probe_impl(ctx):
    runfiles = ctx.runfiles(files = ctx.files.data)
    respected = py_internal.make_runfiles_respect_legacy_external_runfiles(ctx, runfiles)
    duck = struct(configuration = struct(is_tool_configuration = _duck))
    lines = [
        "is_tool=" + repr(py_internal.is_tool_configuration(ctx)),
        "is_tool_named=" + repr(py_internal.is_tool_configuration(ctx = ctx)),
        "type=" + type(py_internal.is_tool_configuration(ctx)),
        "exec_deps=" + repr([(dep[ProbeInfo].is_tool, dep[ProbeInfo].deps) for dep in ctx.attr.exec_deps]),
        "deps=" + repr([(dep[ProbeInfo].is_tool, dep[ProbeInfo].deps) for dep in ctx.attr.deps]),
        "duck=" + repr(py_internal.is_tool_configuration(duck)),
        "legacy=" + repr(py_internal.get_legacy_external_runfiles(ctx)),
        "legacy_none=" + repr(py_internal.get_legacy_external_runfiles()),
        "legacy_any=" + repr(py_internal.get_legacy_external_runfiles(1, 2, x = 3)),
        "bzlmod=" + repr(py_internal.is_bzlmod_enabled(ctx)),
        "bzlmod_named=" + repr(py_internal.is_bzlmod_enabled(ctx = ctx)),
        "respect_same=" + repr(respected == runfiles),
        "respect_files=" + repr([f.short_path for f in respected.files.to_list()]),
        "respect_named=" + repr(py_internal.make_runfiles_respect_legacy_external_runfiles(
            _ctx = None,
            runfiles = "any",
        )),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [
        DefaultInfo(files = depset([out])),
        ProbeInfo(
            is_tool = py_internal.is_tool_configuration(ctx),
            deps = [dep[ProbeInfo].is_tool for dep in ctx.attr.deps + ctx.attr.exec_deps],
        ),
    ]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "deps": attr.label_list(providers = [ProbeInfo]),
        "exec_deps": attr.label_list(cfg = "exec", providers = [ProbeInfo]),
    },
)

def _bad_impl(ctx):
    mode = ctx.attr.mode
    tool = py_internal.is_tool_configuration
    bzlmod = py_internal.is_bzlmod_enabled
    respect = py_internal.make_runfiles_respect_legacy_external_runfiles
    if mode == "tool_missing":
        tool()
    elif mode == "tool_two":
        tool(ctx, ctx)
    elif mode == "tool_unknown_keyword":
        tool(ctxx = ctx)
    elif mode == "tool_duplicate":
        tool(ctx, ctx = ctx)
    elif mode == "tool_none":
        tool(None)
    elif mode == "tool_no_method":
        tool(struct(configuration = 1))
    elif mode == "bzlmod_none":
        bzlmod(None)
    elif mode == "bzlmod_label":
        bzlmod(ctx = ctx.label)
    elif mode == "bzlmod_two":
        bzlmod(ctx, ctx)
    elif mode == "bzlmod_unknown_keyword":
        bzlmod(ctxx = ctx)
    elif mode == "bzlmod_duplicate":
        bzlmod(ctx, ctx = ctx)
    elif mode == "bzlmod_missing":
        bzlmod()
    elif mode == "respect_missing":
        respect(ctx)
    elif mode == "respect_three":
        respect(ctx, ctx, ctx)
    return []

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)
