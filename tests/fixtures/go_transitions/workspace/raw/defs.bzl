load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
def _t(settings, attr):
    v = attr.tags_value
    return {"//raw:tags_alias": tuple(v) if attr.as_tuple else list(v)}
_tr = transition(implementation = _t, inputs = [], outputs = ["//raw:tags_alias"])
def _edge(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.dep]))]
edge = rule(_edge, attrs = {"dep": attr.label(cfg = _tr), "tags_value": attr.string_list(), "as_tuple": attr.bool()})
def _show(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    v = ctx.attr._tags[BuildSettingInfo].value
    ctx.actions.write(out, "%s %s %s %s\n" % (ctx.bin_dir.path, type(v), v, ctx.attr.picked))
    return [DefaultInfo(files = depset([out]))]
show = rule(_show, attrs = {"_tags": attr.label(default = "//go/config:tags"), "picked": attr.string()})
