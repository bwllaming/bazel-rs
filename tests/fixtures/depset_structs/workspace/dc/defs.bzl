# Depset elements holding functions that are not top-level defs: closures,
# lambdas and nested defs (StarlarkFunction.isImmutable is true for every
# Starlark function; StarlarkFunction.equals/hashCode compare the
# per-instantiation identity token), as skylib's DirectoryInfo carries them
# (rules/directory/providers.bzl `_init_directory_info`) into rules_cc's
# `cc_args` depset (cc/toolchains/args.bzl).

def top(x):
    return "top:" + x

def _suffixer(suffix):
    def inner(x):
        return x + suffix

    return inner

CInfo = provider(fields = ["name", "files", "get", "glob", "suffix"])
HInfo = provider(fields = ["twins", "same", "d", "s", "fn"])

def _init(**kwargs):
    # skylib's DirectoryInfo shape: lambdas capturing the struct of the fields.
    self = struct(**kwargs)
    kwargs.update(
        get = lambda path: self.name + "/" + path,
        glob = lambda include, exclude = [], allow_empty = False: [self.name + "/" + i for i in include if i not in exclude],
    )
    return kwargs

# Frozen with this module (loading phase).
TOP_LAMBDA = lambda: "module-lambda"
TOP_DEPSET = depset([struct(f = TOP_LAMBDA), struct(f = TOP_LAMBDA), struct(f = top)])
TOP_FN_DEPSET = depset([top, top, TOP_LAMBDA, _suffixer("!")])

# Statements that allocate enough for the module evaluation to collect
# garbage between them, with depsets of this module's functions alive.
_GC_STRUCT = struct(f = lambda: "gc-lambda", t = (TOP_LAMBDA,))
_GC_DEPSET = depset([_GC_STRUCT])
_GC_TUPLES = depset([(_suffixer("#"), 1), (top, 2)])
_GARBAGE1 = len([str(i) * 20 for i in range(30000)])
_GARBAGE2 = len([(i, str(i)) for i in range(30000)])
_GARBAGE3 = len([[i] for i in range(30000)])
_GC_ITEMS = _GC_DEPSET.to_list()
GC_ROW = "gc=%s %s %s %r" % (
    _GC_ITEMS[0].f(),
    _GC_ITEMS[0] == _GC_STRUCT,
    _GC_ITEMS[0].t[0] == TOP_LAMBDA,
    [t[0]("x") for t in _GC_TUPLES.to_list()],
)

def _leaf_impl(ctx):
    f = ctx.actions.declare_file(ctx.label.name + ".f")
    ctx.actions.write(f, ctx.label.name)
    info = CInfo(**_init(name = ctx.label.name, files = depset([f]), suffix = _suffixer("-" + ctx.label.name)))
    twin_a = CInfo(**_init(name = "twin", files = depset(), suffix = None))
    twin_b = CInfo(**_init(name = "twin", files = depset(), suffix = None))
    name = ctx.label.name
    shared = lambda: "shared-" + name
    s = struct(fn = shared, n = 1)

    # A depset built before this target's module freezes, holding its own
    # (not yet frozen) closures; read again by consumers after the freeze.
    d = depset([s, struct(fn = shared, n = 1), struct(fn = lambda: "other", n = 1)])
    return [
        info,
        HInfo(twins = [twin_a, twin_b], same = [info, info], d = d, s = s, fn = shared),
        DefaultInfo(files = depset([f])),
    ]

leaf = rule(_leaf_impl)

def _calls(items):
    return [
        (i.name, i.get("x"), i.glob(["a", "b", "c"], exclude = ["b"]), i.suffix("y") if i.suffix else None)
        for i in items
    ]

def _report_impl(ctx):
    infos = [d[CInfo] for d in ctx.attr.deps]
    first = ctx.attr.deps[0]
    order = ctx.attr.order
    lines = []

    # Provider instances with closure fields, deduplicated by equality
    # (field-wise; the functions by identity), in every order.
    d = depset([first[CInfo]] + infos, order = order)
    items = d.to_list()
    lines.append("len=%d" % len(items))
    lines.append("names=%r" % [i.name for i in items])
    lines.append("calls=%r" % _calls(items))
    lines.append("eq=%s %s %s" % (items[0] == first[CInfo], items[0].get == first[CInfo].get, items[0].glob == first[CInfo].get))
    lines.append("in=%s %s" % (first[CInfo] in items, first[HInfo].twins[0] in items))
    lines.append("types=%s %s" % (type(items[0].get), type(items[0].suffix)))
    lines.append("repr=%r" % [items[0].get, items[0].suffix])
    lines.append("nested=%r" % [i.name for i in depset([first[CInfo]], transitive = [d, depset(infos, order = order)], order = order).to_list()])

    # Equal fields, distinct closures: kept apart; the same instance twice: one.
    twins = first[HInfo].twins
    lines.append("twins=%d same=%d" % (len(depset(twins).to_list()), len(depset(first[HInfo].same).to_list())))
    lines.append("twins_eq=%s" % (twins[0] == twins[1]))

    # A depset the dependency built before its freeze, combined with its
    # frozen struct and closure.
    h = first[HInfo]
    lines.append("dep_depset=%d %r" % (len(h.d.to_list()), [(x.fn(), x.n) for x in h.d.to_list()]))
    lines.append("dep_combined=%d" % len(depset([h.s, struct(fn = h.fn, n = 1)], transitive = [h.d]).to_list()))
    lines.append("dep_fn_eq=%s %s" % (h.d.to_list()[0].fn == h.fn, h.d.to_list()[0] == h.s))

    # Closures of this implementation (not yet frozen).
    name = ctx.label.name
    mine = lambda: "mine-" + name
    m = struct(f = mine)
    local = depset([m, struct(f = mine), struct(f = lambda: "mine-" + name)], order = order)
    ll = local.to_list()
    lines.append("local=%d %r %s %s" % (len(ll), [x.f() for x in ll], ll[0] == m, ll[0].f == mine))
    lines.append("local_repr=%r" % local)
    fns = depset([mine, mine, _suffixer("?")]).to_list()
    lines.append("local_fn=%d %r %r" % (len(fns), fns[0](), fns[1]("z")))

    # A struct method value is the function itself.
    lines.append("method=%d" % len(depset([struct(g = m.f), m]).to_list()))

    # Module-level depsets with lambdas (frozen with the .bzl module).
    lines.append("module=%d %r" % (len(TOP_DEPSET.to_list()), [x.f("m") if x.f == top else x.f() for x in TOP_DEPSET.to_list()]))
    lines.append("module_fns=%d %r" % (len(TOP_FN_DEPSET.to_list()), repr(TOP_FN_DEPSET)))
    lines.append(GC_ROW)
    lines.append("gc_after_freeze=%s %s" % (_GC_DEPSET.to_list()[0].f(), _GC_DEPSET.to_list()[0] == _GC_STRUCT))

    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [
        DefaultInfo(files = depset([out])),
        # Re-exported after this target's freeze (the `top` rule reads it).
        HInfo(twins = [], same = [], d = local, s = m, fn = mine),
    ]

report = rule(_report_impl, attrs = {
    "deps": attr.label_list(providers = [CInfo]),
    "order": attr.string(default = "default"),
})

def _top_impl(ctx):
    h = ctx.attr.dep[HInfo]
    items = depset([h.s], transitive = [h.d]).to_list()
    lines = [
        "top=%d %r" % (len(items), [x.f() for x in items]),
        "top_eq=%s %s" % (items[0] == h.s, items[0].f == h.fn),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

top_rule = rule(_top_impl, attrs = {"dep": attr.label(providers = [HInfo])})

def _transient_impl(ctx):
    # Closures capturing `ctx` in depsets the implementation discards; the
    # module freeze keeps nothing of them.
    d = depset([struct(f = lambda: ctx.label.name)])
    names = [x.f() for x in d.to_list()]
    t = depset([(lambda: ctx.label.package, 1)], transitive = [depset([(lambda: "x", 2)])])
    names += [x[0]() for x in t.to_list()]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "transient=%r\n" % names)
    return [DefaultInfo(files = depset([out]))]

transient = rule(_transient_impl)

def _bad_impl(ctx):
    kind = ctx.attr.kind
    if kind == "bound_method":
        depset([struct(f = "x".upper)])
    elif kind == "builtin_element":
        depset([len])
    elif kind == "dict_get":
        depset([struct(f = {}.get)])
    elif kind == "kept_ctx":
        # Kept: a returned provider holds the depset of a closure capturing
        # `ctx` (genuine succeeds; natively `ctx` cannot be frozen).
        return [HInfo(twins = [], same = [], d = depset([struct(f = lambda: ctx.label)]), s = None, fn = None)]
    return []

bad = rule(_bad_impl, attrs = {"kind": attr.string()})
