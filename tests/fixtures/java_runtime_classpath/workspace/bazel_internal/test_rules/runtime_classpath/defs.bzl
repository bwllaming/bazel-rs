f = java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive

# Loading-time calls, subsequently frozen with this module.
_strings = depset(["unchecked"], order = "preorder")
_loading = f(_strings, depset())
_empty = f(depset(order = "postorder"), depset([42]))

Classpath = provider(fields = ["files"])

def _generated(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".jar")
    if ctx.attr.poison:
        ctx.actions.run_shell(outputs = [out], command = "echo EXCLUDED_ACTION_RAN >&2; exit 90")
    else:
        ctx.actions.write(out, ctx.var.get("CONTENT", "generated") + "\n")
    return [DefaultInfo(files = depset([out]))]

generated = rule(implementation = _generated, attrs = {"poison": attr.bool()})

def _report(ctx):
    a, b, c = ctx.files.srcs
    g = ctx.file.generated
    external = ctx.file.external
    canonical = ctx.file.canonical
    tree = ctx.actions.declare_directory(ctx.label.name + ".tree")
    ctx.actions.run_shell(outputs = [tree], command = "mkdir -p \"$1\"", arguments = [tree.path])
    symlink = ctx.actions.declare_symlink(ctx.label.name + ".link")
    ctx.actions.symlink(output = symlink, target_path = "missing")
    result = {"loading": [_loading == _strings, _loading.to_list(), _empty == depset()]}
    for order in ["default", "preorder", "postorder", "topological"]:
        leaf = depset([b, c], order = order)
        jars = depset([a, a, external], transitive = [leaf, leaf, depset([c, g], order = order)], order = order)
        cases = {
            "empty": depset(),
            "remove": depset([b, canonical], transitive = [depset([b])], order = "topological"),
            "all": depset([a, b, c, g, external]),
            "string": depset([a.path]),
            "label": depset([a.owner]),
            "struct": depset([struct(file = a)]),
            "none": depset([None]),
        }
        for name, excluded in cases.items():
            filtered = f(jars, excluded)
            result[order + ":" + name] = struct(
                paths = [file.path for file in filtered.to_list()],
                same = filtered == jars,
                stable = 'order = ' not in repr(filtered),
                type = type(filtered),
                repeated_equal = filtered == f(jars, excluded),
            )
    result["empty_runtime"] = [f(depset(order = order), depset([a])) == depset() for order in ["default", "preorder", "postorder", "topological"]]
    result["artifact_kinds"] = [file.path for file in f(depset([tree, symlink, c]), depset([a])).to_list()]
    result["external_identity"] = [external == canonical, f(depset([external]), depset([canonical])) == depset()]
    jars = depset([a, g, ctx.file.poison, b], order = "preorder")
    excluded = depset([ctx.file.poison] + ([b] if ctx.var.get("EXCLUDE_B", "1") == "1" else []))
    selected = f(jars, excluded)
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode(result))
    return [DefaultInfo(files = depset([out])), Classpath(files = selected)]

report = rule(implementation = _report, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "generated": attr.label(allow_single_file = True),
    "poison": attr.label(allow_single_file = True),
    "external": attr.label(allow_single_file = True),
    "canonical": attr.label(allow_single_file = True),
})

def _consume(ctx):
    selected = ctx.attr.dep[Classpath].files
    f(selected, depset()) == selected or fail("frozen provider identity")
    out = ctx.actions.declare_file(ctx.label.name + ".manifest")
    ctx.actions.run_shell(
        outputs = [out], inputs = selected,
        command = "out=\"$1\"; shift; for f in \"$@\"; do /bin/cat \"$f\"; done > \"$out\"",
        arguments = [out.path] + [file.path for file in selected.to_list()],
    )
    return [DefaultInfo(files = depset([out]))]

consume = rule(implementation = _consume, attrs = {"dep": attr.label(providers = [Classpath])})

def _bad(ctx):
    mode = ctx.attr.mode
    if mode == "missing":
        f(depset())
    elif mode == "extra":
        f(depset(), depset(), depset())
    elif mode == "named":
        f(jars = depset(), excluded_jars = depset())
    elif mode == "list_jars":
        f([], depset())
    elif mode == "list_excluded":
        f(depset(), [])
    elif mode == "none_jars":
        f(None, depset())
    elif mode == "none_excluded":
        f(depset(), None)
    elif mode == "string_runtime":
        f(depset(["bad"]), depset(["bad"]))
    elif mode == "int_runtime":
        f(depset([42]), depset(["ignored"]))
    return []

bad = rule(implementation = _bad, attrs = {"mode": attr.string()})

def _identity(ctx):
    source = ctx.file.source
    generated = ctx.actions.declare_file("a.jar")
    ctx.actions.write(generated, "generated fixture input\n")
    external = ctx.file.external
    jars = depset([source, generated, external])
    filtered = f(jars, depset([generated, external]))
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode({
        "same_basename": source.basename == generated.basename and source.basename == external.basename,
        "same_short_path": source.short_path == generated.short_path,
        "source_generated_distinct": source != generated,
        "alias_equal": source == ctx.file.alias,
        "remaining": [file.path for file in filtered.to_list()],
    }))
    return [DefaultInfo(files = depset([out]))]

identity = rule(implementation = _identity, attrs = {
    "source": attr.label(allow_single_file = True),
    "alias": attr.label(allow_single_file = True),
    "external": attr.label(allow_single_file = True),
})
