"""py_internal.get_label_repo_runfiles_path (Bazel 9.3 rules/python/PyBuiltins.java:120-128).

py_internal is available to @_builtins, @bazel_tools, rules_python and the
main repository's tools/build_defs/python (BazelRuleClassProvider).
"""

f = py_internal.get_label_repo_runfiles_path

# Called while this file loads.
LOADED = f(Label("@ext//data:e.txt"))

def _probe_impl(ctx):
    lines = [
        "self=" + repr(f(ctx.label)),
        "named=" + repr(f(label = ctx.label)),
        "root=" + repr(f(Label("//:BUILD.bazel"))),
        "nested=" + repr(f(Label("//a/b/c:x"))),
        "external=" + repr(f(Label("@ext//data:e.txt"))),
        "external_root=" + repr(f(Label("@ext//:root"))),
        "canonical=" + repr(f(Label("@@ext+//data:e"))),
        "bazel_tools=" + repr(f(Label("@bazel_tools//tools/test:x"))),
        "deps=" + repr([f(dep.label) for dep in ctx.attr.deps]),
        "loaded=" + repr(LOADED),
        "type=" + type(f(ctx.label)),
        "workspace_prefix=" + "{}/{}".format(ctx.workspace_name, f(ctx.label)),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {"deps": attr.label_list()},
)

def _bad_impl(ctx):
    mode = ctx.attr.mode
    if mode == "string":
        f("//tools/build_defs/python:x")
    elif mode == "none":
        f(None)
    elif mode == "missing":
        f()
    elif mode == "two":
        f(ctx.label, ctx.label)
    elif mode == "unknown_keyword":
        f(lable = ctx.label)
    elif mode == "duplicate":
        f(ctx.label, label = ctx.label)
    return []

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)
