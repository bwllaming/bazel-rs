f = py_internal.is_singleton_depset
P = provider(fields = ["x"])
Q = provider(fields = ["x"])
Result = provider(fields = ["singleton", "files"])

def row(name, d):
    return "%s=%s/%d" % (name, f(d), len(d.to_list()))

def matrix():
    result = []
    for order in ["default", "postorder", "preorder", "topological"]:
        a = depset(["a"], order = order)
        b = depset(["b"], order = order)
        ab = depset(["a", "b"], order = order)
        cases = [
            ("empty", depset(order = order)),
            ("single", a),
            ("multiple", ab),
            ("direct_duplicates", depset(["a", "a"], order = order)),
            ("empty_children", depset(transitive = [depset(), depset()], order = order)),
            ("chain", depset(transitive = [depset(transitive = [a], order = order)], order = order)),
            ("reused", depset(transitive = [a, a, a], order = order)),
            ("equal_subsets", depset(transitive = [a, depset(["a"], order = order)], order = order)),
            ("direct_hoisted", depset(["a", "a"], transitive = [a, a], order = order)),
            ("direct_other", depset(["b"], transitive = [a], order = order)),
            ("two_subsets", depset(transitive = [a, b], order = order)),
            ("reused_multiple", depset(transitive = [ab, ab], order = order)),
            ("direct_in_multiple", depset(["a"], transitive = [ab], order = order)),
            ("default_child", depset(["a"], transitive = [depset(["a"])], order = order)),
            ("default_parent", depset(transitive = [a])),
            ("diamond", depset(transitive = [depset(transitive = [ab], order = order), depset(["a"], transitive = [ab], order = order)], order = order)),
        ]
        result.extend([row(order + "/" + name, d) for name, d in cases])
    for name, values in [
        ("ints", [1, 1]),
        ("bools", [True, True]),
        ("none", [None, None]),
        ("labels", [Label(":input.txt"), Label(":input.txt")]),
        ("tuple_mixed", [(1, "s", None), (1, "s", None)]),
        ("struct_equal", [struct(x = 1), struct(x = 1)]),
        ("struct_distinct", [struct(x = 1), struct(x = 2)]),
        ("provider_equal", [P(x = 1), P(x = 1)]),
        ("mixed_providers", [P(x = 1), Q(x = 1)]),
        ("mixed_struct_provider", [struct(x = 1), P(x = 1)]),
        ("nested_depsets", [depset([1]), depset([1])]),
    ]:
        result.append(row(name, depset(values)))
        result.append(row(name + "/transitive", depset(transitive = [depset([v]) for v in values])))
    return result

LOAD_RESULTS = matrix()

def _leaf(ctx):
    files = depset(ctx.files.srcs)
    return [DefaultInfo(files = files), Result(singleton = f(files), files = files)]

leaf = rule(implementation = _leaf, attrs = {"srcs": attr.label_list(allow_files = True)})

def _probe(ctx):
    files = ctx.attr.dep[Result].files
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = LOAD_RESULTS + matrix() + [
        row("files", files),
        row("files_duplicate", depset(files.to_list(), transitive = [files, files])),
        "provider_bool=%s" % ctx.attr.dep[Result].singleton,
    ]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out] + ctx.files.repo))]

probe = rule(implementation = _probe, attrs = {"dep": attr.label(), "repo": attr.label(allow_files = True)})

def _repo(ctx):
    yes = f(depset([struct(x = "repository")]))
    ctx.file("result.txt", "repository=%s\n%s\n" % (yes, "\n".join(matrix())))
    ctx.file("BUILD.bazel", 'exports_files(["result.txt"])' if yes else 'fail("not singleton")')

singleton_repo = repository_rule(implementation = _repo)
