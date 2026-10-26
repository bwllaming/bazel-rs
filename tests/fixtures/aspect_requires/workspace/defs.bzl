# aspect(requires = [...]) expansion (AspectsList.Builder) and the
# AspectCollection rules it feeds (required aspects propagate with their
# requirer, see it, and apply regardless of their required_providers).

PA = provider(fields = ["v"])
PB = provider(fields = ["v"])
PE = provider(fields = ["v"])
PP = provider(fields = ["v"])
PQ = provider(fields = ["v"])
Never = provider(fields = [])
Marker = provider(fields = [])

def _a_impl(target, ctx):
    print("A %s tag=%s ids=%s" % (target.label.name, ctx.attr.tag, ctx.aspect_ids))
    return [PA(v = target.label.name)]

# Does not propagate itself: applied along `deps` only with an aspect that
# requires it.
asp_a = aspect(
    implementation = _a_impl,
    attrs = {"tag": attr.string(default = "t0", values = ["t0", "t1"])},
    provides = [PA],
)

def _b_impl(target, ctx):
    print("B %s a=%s ids=%s" % (target.label.name, target[PA].v, ctx.aspect_ids))
    return [PB(v = "b(%s)" % target[PA].v)]

# Sees asp_a through `requires` alone (no required_aspect_providers).
asp_b = aspect(
    implementation = _b_impl,
    requires = [asp_a],
    provides = [PB],
    attr_aspects = ["deps"],
)

def _e_impl(target, ctx):
    print("E %s b=%s a=%s ids=%s" % (target.label.name, target[PB].v, PA in target, ctx.aspect_ids))
    return [PE(v = "e(%s)" % target[PB].v)]

# Transitive: asp_e requires asp_b, which requires asp_a.
asp_e = aspect(
    implementation = _e_impl,
    requires = [asp_b],
    provides = [PE],
    attr_aspects = ["deps"],
)

def _p_impl(target, ctx):
    return [PP(v = target.label.name)]

asp_p = aspect(implementation = _p_impl, provides = [PP])

def _q_impl(target, ctx):
    print("Q %s pp=%s ids=%s" % (target.label.name, PP in target, ctx.aspect_ids))
    return [PQ(v = target.label.name)]

# Sees asp_p through required_aspect_providers only: asp_p does not
# propagate along `deps` with it.
asp_q = aspect(
    implementation = _q_impl,
    required_aspect_providers = [PP],
    provides = [PQ],
    attr_aspects = ["deps"],
)

PN = provider(fields = ["v"])
PM = provider(fields = ["v"])

def _n_impl(target, ctx):
    print("N %s" % target.label.name)
    return [PN(v = target.label.name)]

# Requires a provider no rule advertises: kept on the path because asp_m
# requires it, but not applicable (no implementation, no providers).
asp_n = aspect(implementation = _n_impl, required_providers = [Never], provides = [PN])

def _m_impl(target, ctx):
    print("M %s pn=%s ids=%s" % (target.label.name, PN in target, ctx.aspect_ids))
    return [PM(v = target.label.name)]

asp_m = aspect(implementation = _m_impl, requires = [asp_n], provides = [PM], attr_aspects = ["deps"])

def _lib_impl(ctx):
    return [Marker()]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()}, provides = [Marker])

def _write(ctx, lines):
    out = ctx.actions.declare_file(ctx.label.name + ".req")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

def _use_b_impl(ctx):
    return _write(ctx, [d[PB].v for d in ctx.attr.deps])

use_b = rule(implementation = _use_b_impl, attrs = {"deps": attr.label_list(aspects = [asp_b])})

def _use_e_impl(ctx):
    return _write(ctx, [d[PE].v for d in ctx.attr.deps])

# Listing a required aspect before its requirer is legal (asp_a first).
use_e = rule(implementation = _use_e_impl, attrs = {"deps": attr.label_list(aspects = [asp_a, asp_e])})

def _use_q_impl(ctx):
    return _write(ctx, [d[PQ].v for d in ctx.attr.deps])

use_q = rule(implementation = _use_q_impl, attrs = {"deps": attr.label_list(aspects = [asp_p, asp_q])})

def _use_m_impl(ctx):
    return _write(ctx, [d[PM].v for d in ctx.attr.deps])

use_m = rule(implementation = _use_m_impl, attrs = {"deps": attr.label_list(aspects = [asp_m])})
