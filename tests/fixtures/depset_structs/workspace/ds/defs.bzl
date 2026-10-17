# N49 fixture: depsets of struct() values and Starlark provider instances
# (Depset.checkElement, StarlarkInfo.isImmutable, StructImpl.equals/hashCode,
# Depset element types), within one target and across targets.
Info = provider(fields = ["name", "files", "nested", "none", "fn", "tags"])
Holder = provider(fields = ["rf", "tgt"])
Plain = provider()
DepsInfo = provider(fields = ["items", "holders", "lst"])

def mapper(x):
    return x

def _info(name, files, tags):
    return Info(name = name, files = files, nested = depset([name]), none = None, fn = mapper, tags = tags)

def _leaf_impl(ctx):
    f = ctx.actions.declare_file(ctx.label.name + ".f")
    ctx.actions.write(f, ctx.label.name)
    items = depset(
        [_info(ctx.label.name, (f,), ("a",)), struct(name = ctx.label.name, kind = "struct"), Plain(name = ctx.label.name)],
        order = ctx.attr.order,
    )
    holders = depset([Holder(rf = ctx.runfiles(files = [f]), tgt = None)])
    return [DepsInfo(items = items, holders = holders, lst = ["frozen"]), DefaultInfo(files = depset([f]))]

leaf = rule(_leaf_impl, attrs = {"order": attr.string(default = "default")})

def _show(x):
    if type(x) == "struct" and hasattr(x, "name"):
        return "%s:%s" % (type(x), x.name)
    return type(x)

# Mixed traversal orders (each NestedSet node keeps its own layout).
_ORDER_PAIRS = [
    ("topological", "default"),
    ("topological", "topological"),
    ("preorder", "default"),
    ("preorder", "preorder"),
    ("postorder", "default"),
    ("postorder", "postorder"),
    ("default", "topological"),
    ("default", "preorder"),
    ("default", "postorder"),
    ("default", "default"),
]

def _orders(parent, child):
    a = depset(["a", "b"], order = child)
    b = depset(["b", "x"], order = child)
    return (parent, child, depset(["c", "d", "a"], transitive = [a, b, a], order = parent).to_list())

def _deep():
    leaf = depset(["a", "b"], order = "topological")
    mid = depset(["c"], transitive = [leaf])
    pre = depset(["p", "a"], transitive = [mid], order = "preorder")
    return [
        depset(["e"], transitive = [mid, leaf], order = "topological").to_list(),
        depset(["e"], transitive = [pre, mid]).to_list(),
        depset(["f"], transitive = [depset(["e"], transitive = [pre], order = "default")], order = "preorder").to_list(),
    ]

def _report_impl(ctx):
    deps = ctx.attr.deps
    mine = [_info("own", (), ())] + [
        _info(d.label.name, tuple(d[DefaultInfo].files.to_list()), ("a",))
        for d in deps
    ] + [struct(name = d.label.name, kind = "struct") for d in deps]
    merged = depset(mine, transitive = [d[DepsInfo].items for d in deps], order = ctx.attr.order)
    items = merged.to_list()
    holders = depset([Holder(rf = ctx.runfiles(), tgt = d) for d in deps], transitive = [d[DepsInfo].holders for d in deps])
    hl = holders.to_list()
    lines = [
        "len=%d" % len(items),
        "names=%s" % [_show(x) for x in items],
        "repr=%r" % merged,
        "eq_own=%s" % (items[0] == _info("own", (), ()) if ctx.attr.order != "topological" else "skip"),
        "struct_vs_info=%s" % (struct(name = "x") == Plain(name = "x")),
        "plain_eq=%s" % (Plain(name = "x") == Plain(name = "x")),
        "depset_eq=%s %s %s" % (depset([1]) == depset([1]), depset([1]) == depset([2]), depset() == depset()),
        "nested=%s" % [x.nested.to_list() for x in items if hasattr(x, "nested")],
        "fields=%s" % [dir(x) for x in items[:2]],
        "holders=%d %s" % (len(hl), [(type(h.rf), str(h.tgt)) for h in hl]),
        "same_holder=%s" % (len(depset(hl + hl).to_list())),
        "none=%s" % depset([None, None]).to_list(),
        "nested_depsets=%d" % len(depset([depset([1]), depset([1]), depset([2])]).to_list()),
        "tuple_struct=%r" % depset([(struct(a = 1), "x")]).to_list(),
        "orders=%r" % [_orders(parent, child) for parent, child in _ORDER_PAIRS],
        "deep=%r" % _deep(),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

report = rule(_report_impl, attrs = {"deps": attr.label_list(providers = [DepsInfo]), "order": attr.string(default = "default")})

def _bad_impl(ctx):
    d = ctx.attr.dep[DepsInfo]
    kind = ctx.attr.kind
    if kind == "mutable_field":
        depset([struct(x = [1])])
    elif kind == "list":
        depset([[1]])
    elif kind == "dict":
        depset([{}])
    elif kind == "frozen_list":
        depset([d.lst])
    elif kind == "builtin":
        depset([struct(f = len)])
    elif kind == "mix":
        depset([struct(), "x"])
    elif kind == "mix_transitive":
        depset(["x"], transitive = [d.items])
    elif kind == "mutable_provider_field":
        depset([Info(name = "x", files = [], nested = None, none = None, fn = None, tags = ())])
    elif kind == "frozen_list_field":
        out = depset([struct(l = d.lst)])
        return [DefaultInfo(files = depset([_write(ctx, "%r" % out.to_list())]))]
    elif kind == "lambda_field":
        out = depset([struct(f = lambda x: x)])
        return [DefaultInfo(files = depset([_write(ctx, "%d" % len(out.to_list()))]))]
    return []

def _write(ctx, text):
    f = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(f, text + "\n")
    return f

bad = rule(_bad_impl, attrs = {"dep": attr.label(providers = [DepsInfo]), "kind": attr.string()})
