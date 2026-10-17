# N49b fixture: frozen lists and dicts inside depset struct elements
# (StarlarkList.equals, Dict.equals/hashCode, Starlark.isImmutable), the
# single-transitive depset identity shortcut (NestedSetBuilder.buildInternal,
# Depset.fromDirectAndTransitive) and the order-incompatibility message.

# Frozen when this module is loaded.
FROZEN = ["g1", ("t",)]
FROZEN_DICT = {"b": 2, "a": 1}
FROZEN_DICT2 = {"a": 1, "b": 2}
FROZEN_NESTED = [[1, 2], {"k": [3]}, (4,)]

FInfo = provider(fields = ["lst", "dct", "files", "nested", "same"])

def _fleaf_impl(ctx):
    f = ctx.actions.declare_file(ctx.label.name + ".f")
    ctx.actions.write(f, ctx.label.name)
    return [
        FInfo(
            lst = ["x", f],
            dct = {"k": [1, 2], "j": None, "f": f},
            files = [f],
            nested = [[1], (2,), {"m": {"n": [5]}}],
            # Equal contents to FROZEN (frozen with this target).
            same = ["g1", ("t",)],
        ),
        DefaultInfo(files = depset([f])),
    ]

fleaf = rule(_fleaf_impl)

def _frozen_rows(d):
    s1 = struct(l = d.lst, m = d.dct)
    s2 = struct(l = d.lst, m = d.dct)
    items = depset([s1, s2]).to_list()
    x = items[0]
    lt = depset([struct(l = FROZEN), struct(l = tuple(FROZEN))]).to_list()
    same = depset([struct(l = FROZEN), struct(l = d.same)]).to_list()
    dicts = depset([struct(m = FROZEN_DICT), struct(m = FROZEN_DICT2)]).to_list()
    nested = depset([struct(n = FROZEN_NESTED), Holder2(v = d.nested)]).to_list()
    top_tuple = depset([(FROZEN,), (FROZEN,), (d.same,)]).to_list()
    return [
        "frozen_len=%d" % len(items),
        "frozen_repr=%r" % items,
        "frozen_types=%s %s %s" % (type(x.l), type(x.m), type(x.l[1])),
        "frozen_eq=%s %s %s" % (x == s1, x.l == d.lst, x.m == d.dct),
        "frozen_in=%s %s" % (s1 in items, struct(l = d.lst, m = d.dct) in items),
        "list_vs_tuple=%d %r" % (len(lt), lt),
        "same_contents=%d %r" % (len(same), same),
        "dict_order=%d %r %s" % (len(dicts), dicts, dicts[0].m == FROZEN_DICT2),
        "nested=%d %r" % (len(nested), nested),
        "nested_types=%s" % ([type(v) for v in nested[0].n] + [type(v) for v in nested[1].v]),
        "top_tuple=%d %r" % (len(top_tuple), top_tuple),
        "frozen_depset_repr=%r" % depset([struct(l = FROZEN, m = FROZEN_DICT)]),
        "files=%r" % depset([struct(files = d.files)]).to_list()[0].files,
        "dict_keys=%s" % list(depset([struct(m = FROZEN_DICT)]).to_list()[0].m.keys()),
    ]

Holder2 = provider(fields = ["v"])

def _identity_rows(ctx):
    x = depset([1, 2])
    one = depset([1])
    t = depset([1, 2], order = "topological")
    p = depset([1, 2], order = "preorder")
    tone = depset([1], order = "topological")
    empty_files = ctx.attr.empty.files
    rows = [
        ("single", depset(transitive = [x]) == x),
        ("empty_sibling", depset(transitive = [depset(), x]) == x),
        ("empty_sibling_after", depset(transitive = [x, depset()]) == x),
        ("empty_incompatible_sibling", depset(transitive = [x, depset(order = "topological")]) == x),
        ("repeated", depset(transitive = [x, x]) == x),
        ("chain", depset(transitive = [depset(transitive = [x])]) == x),
        ("direct_equal_singleton", depset([1], transitive = [one]) == one),
        ("direct_dup_equal_singleton", depset([1, 1], transitive = [one]) == one),
        ("direct_other_singleton", depset([2], transitive = [one]) == one),
        ("direct_in_multi", depset([1], transitive = [x]) == x),
        ("two_sets", depset(transitive = [x, one]) == x),
        ("compatible_order", depset(transitive = [x], order = "preorder") == x),
        ("default_of_preorder", depset(transitive = [p]) == p),
        ("same_preorder", depset(transitive = [p], order = "preorder") == p),
        ("topological", depset(transitive = [t], order = "topological") == t),
        ("topological_repeated", depset(transitive = [t, t], order = "topological") == t),
        ("topological_direct", depset([1], transitive = [tone], order = "topological") == tone),
        ("preorder_direct_other_order", depset([1], transitive = [one], order = "preorder") == one),
        ("in_list", depset(transitive = [x]) in [x]),
        ("not_in_list", depset(transitive = [x, one]) in [x]),
        ("dict_key", {x: "v"}.get(depset(transitive = [x]), "missing")),
        ("dict_key_other", {x: "v"}.get(depset([1, 2]), "missing")),
        ("struct_dedup", len(depset([struct(d = x), struct(d = depset(transitive = [x]))]).to_list())),
        ("depset_dedup", len(depset([x, depset(transitive = [x]), depset(transitive = [depset(), x, x])]).to_list())),
        ("empty_result", depset(transitive = [depset(), depset(order = "preorder")]) == depset()),
        ("empty_typed_skipped", depset(["s"], transitive = [empty_files]).to_list()),
        ("empty_typed_result", depset(transitive = [empty_files]) == depset()),
        ("repr", repr([depset(transitive = [x]), depset(transitive = [t], order = "topological"), depset(transitive = [x], order = "preorder")])),
        ("list", [depset(transitive = [x], order = "preorder").to_list(), depset([3], transitive = [x]).to_list()]),
    ]
    return ["%s=%r" % (name, value) for name, value in rows]

def _freport_impl(ctx):
    lines = _frozen_rows(ctx.attr.dep[FInfo]) + _identity_rows(ctx)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

freport = rule(_freport_impl, attrs = {"dep": attr.label(providers = [FInfo]), "empty": attr.label()})

def _fbad_impl(ctx):
    d = ctx.attr.dep[FInfo]
    kind = ctx.attr.kind
    if kind == "append":
        depset([struct(l = d.lst)]).to_list()[0].l.append(1)
    elif kind == "append_global":
        depset([struct(l = FROZEN)]).to_list()[0].l.append(1)
    elif kind == "append_nested":
        depset([struct(l = FROZEN_NESTED)]).to_list()[0].l[0].append(1)
    elif kind == "dict_pop":
        depset([struct(m = d.dct)]).to_list()[0].m.pop("k")
    elif kind == "dict_setdefault":
        depset([struct(m = FROZEN_DICT)]).to_list()[0].m.setdefault("z", 1)
    elif kind == "top_frozen_dict":
        depset([FROZEN_DICT])
    elif kind == "top_frozen_list_global":
        depset([FROZEN])
    elif kind == "order_pre_post":
        depset(["x"], order = "preorder", transitive = [depset(["y"], order = "postorder")])
    elif kind == "order_topo_pre":
        depset(["x"], order = "topological", transitive = [depset(["y"], order = "preorder")])
    elif kind == "order_post_topo":
        depset(order = "postorder", transitive = [depset(["y"], order = "topological")])
    elif kind == "order_after_type":
        depset(["x"], order = "preorder", transitive = [depset([1], order = "postorder")])
    elif kind == "order_second":
        depset(["x"], order = "preorder", transitive = [depset(["y"], order = "preorder"), depset(["z"], order = "topological")])
    return []

fbad = rule(_fbad_impl, attrs = {"dep": attr.label(providers = [FInfo]), "kind": attr.string()})
