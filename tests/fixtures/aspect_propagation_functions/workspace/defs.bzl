def _one(value):
    if type(value) == "Label":
        return "L(" + str(value) + ")"
    return repr(value)

def _show(value):
    if type(value) == "list":
        return "[" + ",".join([_one(v) for v in value]) + "]"
    return _one(value)

def _attr_aspects(ctx):
    r = ctx.rule
    print("F", r.label, r.qualified_kind.file_label, r.qualified_kind.rule_name, "mode=" + ctx.attr.mode,
          "deps=" + _show(r.attr.deps.value), "deps_tool=" + str(r.attr.deps.is_tool),
          "tool=" + _show(r.attr.tool.value), "tool_tool=" + str(r.attr.tool.is_tool),
          "s=" + _show(r.attr.s.value), "name=" + _show(r.attr.name.value),
          "tags=" + _show(r.attr.tags.value), "type=" + type(ctx), type(r))
    if r.attr.s.value == "stop":
        return []
    return [ctx.attr.mode]

def _pred(ctx):
    print("P", ctx.rule.label, ctx.rule.qualified_kind.rule_name, "tags=" + _show(ctx.rule.attr.tags.value), "mode=" + ctx.attr.mode,
          "has_deps=" + str(hasattr(ctx.rule.attr, "deps")))
    return "skip" not in ctx.rule.attr.tags.value

def _impl(target, ctx):
    print("A", target.label.name, ctx.attr.mode)
    return []

fa = aspect(
    _impl,
    attr_aspects = _attr_aspects,
    propagation_predicate = _pred,
    attrs = {"mode": attr.string(default = "deps", values = ["deps", "extra"])},
)

def _bad_list(ctx):
    return "deps"

bad_list = aspect(_impl, attr_aspects = _bad_list)

def _bad_star(ctx):
    return ["*"]

bad_star = aspect(_impl, attr_aspects = _bad_star)

def _bad_pred(ctx):
    return "yes"

bad_pred = aspect(_impl, attr_aspects = ["deps"], propagation_predicate = _bad_pred)

def _r(ctx):
    return []

lib = rule(_r, attrs = {
    "deps": attr.label_list(),
    "extra": attr.label_list(),
    "tool": attr.label(cfg = "exec"),
    "s": attr.string(),
})

top = rule(_r, attrs = {
    "deps": attr.label_list(aspects = [fa]),
    "mode": attr.string(default = "extra"),
})
