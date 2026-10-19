"""N58 Starlark aspects: definition parameters, propagation, providers,
aspect-on-aspect, `--aspects`, the ValidateTarget aspect path and the aspect
context. Expected texts are genuine Bazel 9.3 strings (paired capture
tests/fixtures/aspects/reference_receipt.json)."""

FileInfo = provider(fields = ["note"])
UpperInfo = provider(fields = ["note"])
CountInfo = provider(fields = ["count"])
NeedInfo = provider(fields = ["v"])

def _count_aspect_impl(target, ctx):
    names = []
    counts = []
    if hasattr(ctx.rule.attr, "deps"):
        for dep in ctx.rule.attr.deps:
            names.append(dep.label.name)
            counts.append(dep[CountInfo].count)
    count = 1
    for value in counts:
        count += value
    print("ASPECT %s kind=%s extension=%s rule-attr=%s" % (ctx.label, ctx.rule.kind, ctx.attr.extension, names))
    out = ctx.actions.declare_file(target.label.name + "." + ctx.attr.extension)
    ctx.actions.write(out, "aspect-of:" + target.label.name)
    return [
        CountInfo(count = count),
        FileInfo(note = "n"),
        OutputGroupInfo(aspect_out = depset([out])),
    ]

count_aspect = aspect(
    implementation = _count_aspect_impl,
    attr_aspects = ["deps"],
    provides = [FileInfo],
    attrs = {"extension": attr.string(default = "cnt", values = ["cnt", "out"])},
    required_aspect_providers = [],
)

def _upper_aspect_impl(target, ctx):
    print("UPPER %s sees=%s ids=%s" % (ctx.label, target[FileInfo].note, ctx.aspect_ids))
    return [UpperInfo(note = "u" + target[FileInfo].note)]

upper_aspect = aspect(
    implementation = _upper_aspect_impl,
    required_aspect_providers = [[FileInfo]],
)

def _files_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".files")
    ctx.actions.write(out, ",".join([f.short_path for f in target[DefaultInfo].files.to_list()]))
    return [OutputGroupInfo(files_group = depset([out]))]

files_aspect = aspect(implementation = _files_aspect_impl, attr_aspects = ["deps"])

def _need_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".need")
    ctx.actions.write(out, "need")
    return [NeedInfo(v = 1), OutputGroupInfo(need_group = depset([out]))]

need_aspect = aspect(
    implementation = _need_aspect_impl,
    required_providers = [NeedInfo],
    attr_aspects = ["deps"],
)

def _diag_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".diag")
    ctx.actions.write(out, "diag")
    return [OutputGroupInfo(_validation = depset([out]))]

diag_aspect = aspect(implementation = _diag_aspect_impl, attr_aspects = ["deps"])

def _dup_aspect_impl(target, ctx):
    return [FileInfo(note = "dup")]

dup_aspect = aspect(implementation = _dup_aspect_impl)

def _tool_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".withtool")
    ctx.actions.write(out, ctx.file._tool.short_path)
    return [OutputGroupInfo(tool_group = depset([out]))]

tool_aspect = aspect(
    implementation = _tool_aspect_impl,
    attrs = {"_tool": attr.label(default = Label("//:aspect_tool"), allow_single_file = True)},
)

def _fragments_aspect_impl(target, ctx):
    return []

fragments_aspect = aspect(
    implementation = _fragments_aspect_impl,
    fragments = ["cpp"],
)

GAInfo = provider(fields = ["which"])

def _gen_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".genaspect")
    ctx.actions.write(out, str(ctx.label))
    return [GAInfo(which = "gen"), OutputGroupInfo(ga_group = depset([out]))]

def _gen_generating_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".generating")
    ctx.actions.write(out, str(ctx.label))
    return [GAInfo(which = "generating"), OutputGroupInfo(generating_group = depset([out]))]

gen_aspect = aspect(implementation = _gen_aspect_impl, attr_aspects = ["deps"])
gen_generating_aspect = aspect(
    implementation = _gen_generating_aspect_impl,
    attr_aspects = ["deps"],
    apply_to_generating_rules = True,
)

def _tool_rule_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".tool")
    ctx.actions.write(out, "tool")
    return [DefaultInfo(files = depset([out]))]

aspect_tool = rule(implementation = _tool_rule_impl)

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".lib")
    ctx.actions.write(out, "lib")
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _app_impl(ctx):
    outs = []
    for dep in ctx.attr.deps:
        out = ctx.actions.declare_file(ctx.label.name + "." + dep.label.name)
        ctx.actions.write(out, "count=%d note=%s" % (dep[CountInfo].count, dep[UpperInfo].note))
        outs.append(out)
    return [DefaultInfo(files = depset(outs))]

app = rule(
    implementation = _app_impl,
    attrs = {"deps": attr.label_list(aspects = [count_aspect, upper_aspect])},
)

def _has_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".has")
    ctx.actions.write(out, "has")
    return [DefaultInfo(files = depset([out])), NeedInfo(v = 0)]

has_rule = rule(implementation = _has_impl, attrs = {"deps": attr.label_list()})

def _plain_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".plain")
    ctx.actions.write(out, "plain")
    return [DefaultInfo(files = depset([out]))]

plain_rule = rule(implementation = _plain_impl, attrs = {"deps": attr.label_list()})

def _aggregate_impl(ctx):
    names = []
    for dep in ctx.attr.deps:
        names.append(dep.label.name)
    out = ctx.actions.declare_file(ctx.label.name + ".agg")
    ctx.actions.write(out, ",".join(names))
    return [DefaultInfo(files = depset([out]))]

aggregate = rule(
    implementation = _aggregate_impl,
    attrs = {"deps": attr.label_list(aspects = [need_aspect])},
)

def _val_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "ok")
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset([out])),
    ]

val = rule(implementation = _val_impl)

def _eater_impl(ctx):
    return [DefaultInfo(files = depset([f for d in ctx.attr.deps for f in d[DefaultInfo].files.to_list()]))]

eater = rule(implementation = _eater_impl, attrs = {"deps": attr.label_list()})

def _gen_impl(ctx):
    out1 = ctx.actions.declare_file(ctx.label.name + ".gen1")
    ctx.actions.write(out1, "gen")
    return [DefaultInfo(files = depset([out1]))]

g = rule(implementation = _gen_impl, outputs = {"gen1": "%{name}.gen1"})

def _use_impl(ctx):
    parts = []
    for name, dep in [("dep1", ctx.attr.dep1), ("dep2", ctx.attr.dep2)]:
        parts.append(name + "=" + (dep[GAInfo].which if GAInfo in dep else "none"))
    out = ctx.actions.declare_file(ctx.label.name + ".which")
    ctx.actions.write(out, " ".join(parts))
    return [DefaultInfo(files = depset([out]))]

use = rule(
    implementation = _use_impl,
    attrs = {
        "dep1": attr.label(aspects = [gen_aspect], allow_single_file = True),
        "dep2": attr.label(aspects = [gen_generating_aspect], allow_single_file = True),
    },
)

def _covered_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".cov")
    ctx.actions.write(out, "cov")
    return [
        DefaultInfo(files = depset([out])),
        coverage_common.instrumented_files_info(
            ctx,
            source_attributes = ["srcs"],
            dependency_attributes = ["deps"],
            extensions = ["go"],
        ),
    ]

covered_rule = rule(implementation = _covered_impl, attrs = {"deps": attr.label_list()})

def _covering_aspect_impl(target, ctx):
    # A related aspect may legitimately share the instrumentation
    # implementation with the base rule: its InstrumentedFilesInfo wins
    # over the base target's, without a duplicate-provider error.
    return [
        coverage_common.instrumented_files_info(
            ctx,
            source_attributes = [],
            dependency_attributes = ["deps"],
            extensions = ["cc"],
        ),
    ]

covering_aspect = aspect(implementation = _covering_aspect_impl, attr_aspects = ["deps"])

def _covering_aspect_two_impl(target, ctx):
    return [coverage_common.instrumented_files_info(ctx)]

covering_aspect_two = aspect(implementation = _covering_aspect_two_impl)

def _covered_consumer_impl(ctx):
    # The merged aspect-configured dependency carries the aspect's
    # InstrumentedFilesInfo (the base target's copy was dropped for it);
    # the values are opaque without a coverage configuration, so the
    # consumer only proves the merge succeeded.
    out = ctx.actions.declare_file(ctx.label.name + ".reads")
    ctx.actions.write(out, ",".join([dep.label.name for dep in ctx.attr.deps]))
    return [DefaultInfo(files = depset([out]))]

covered_consumer = rule(
    implementation = _covered_consumer_impl,
    attrs = {"deps": attr.label_list(aspects = [covering_aspect])},
)

covered_two = rule(
    implementation = _covered_consumer_impl,
    attrs = {"deps": attr.label_list(aspects = [covering_aspect, covering_aspect_two])},
)

def _dup_base_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".b")
    ctx.actions.write(out, "b")
    return [DefaultInfo(files = depset([out])), FileInfo(note = "base")]

dup_base = rule(implementation = _dup_base_impl)

def _dup_consumer_impl(ctx):
    return [DefaultInfo()]

dup_consumer = rule(
    implementation = _dup_consumer_impl,
    attrs = {"deps": attr.label_list(aspects = [count_aspect, dup_aspect])},
)
