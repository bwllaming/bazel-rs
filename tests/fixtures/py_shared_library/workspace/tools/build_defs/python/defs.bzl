"""py_internal.cc_helper.is_valid_shared_library_artifact (Bazel 9.3
builtins_bzl/common/python/py_internal.bzl:101-131), as rules_python's
py_library calls it for its data files (`merge_uses_shared_libraries`).

py_internal is available to @_builtins, @bazel_tools, rules_python and the
main repository's tools/build_defs/python (BazelRuleClassProvider).
"""

def _probe_impl(ctx):
    f = py_internal.cc_helper.is_valid_shared_library_artifact
    lines = ["%s=%r" % (file.basename, f(file)) for file in ctx.files.libs]
    lines.extend([
        "named=%r" % f(shared_library = ctx.files.libs[0]),
        "duck_extension=%r" % f(struct(extension = "so", basename = "x")),
        "duck_none=%r" % f(struct(extension = None, basename = "libx.so.1")),
        "duck_versioned=%r" % f(struct(extension = "txt", basename = "lib.dylib.7_x")),
    ])
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {"libs": attr.label_list(allow_files = True)},
)

def _bad_impl(ctx):
    f = py_internal.cc_helper.is_valid_shared_library_artifact
    mode = ctx.attr.mode
    if mode == "missing":
        f()
    elif mode == "two":
        f(1, 2)
    elif mode == "none":
        f(None)
    elif mode == "basename_int":
        f(struct(extension = "x", basename = 1))
    elif mode == "empty_part":
        f(struct(extension = "", basename = "libfoo.so.1."))
    elif mode == "no_basename":
        f(struct(extension = "x"))
    return []

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)
