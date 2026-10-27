"""py_internal.is_singleton_depset (Bazel 9.3 builtins_bzl/common/python/py_internal.bzl,
rules/python/PyBuiltins.java:76-89: NestedSet.isSingleton). py_internal is
available in the main repository's tools/build_defs/python."""

def _probe_impl(ctx):
    one = depset([1])
    cases = [
        ("empty", depset([])),
        ("one", one),
        ("one_twice", depset([1, 1])),
        ("two", depset([1, 2])),
        ("only_child", depset(transitive = [one])),
        ("same_direct_and_child", depset([1], transitive = [one])),
        ("two_singleton_children", depset(transitive = [one, depset([1])])),
        ("child_of_two", depset(transitive = [depset([1, 2])])),
        ("empty_child", depset([], transitive = [depset([])])),
        ("topological", depset(["a"], order = "topological")),
        ("files", depset(ctx.files.srcs)),
        ("file", depset(ctx.files.srcs[:1])),
    ]
    lines = ["%s=%r" % (name, py_internal.is_singleton_depset(value)) for name, value in cases]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(implementation = _probe_impl, attrs = {"srcs": attr.label_list(allow_files = True)})

def _list_impl(ctx):
    py_internal.is_singleton_depset([1])

wrong_type = rule(implementation = _list_impl)

def _named_impl(ctx):
    py_internal.is_singleton_depset(value = depset([1]))

named = rule(implementation = _named_impl)
