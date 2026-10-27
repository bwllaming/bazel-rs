AInfo = provider(fields = ["where"])

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(tc_name = ctx.attr.tc_name)]

my_toolchain = rule(_tc_impl, attrs = {"tc_name": attr.string(), "deps": attr.label_list()})

def _lib_impl(ctx):
    return []

lib = rule(_lib_impl, attrs = {"deps": attr.label_list()})

uses_tc = rule(_lib_impl, attrs = {"deps": attr.label_list()}, toolchains = ["//tc:t", "//tc:u"])

def _show(ctx):
    if not hasattr(ctx.rule, "toolchains"):
        return "-"
    tcs = ctx.rule.toolchains
    parts = [repr(tcs)]
    for t in ["//tc:t", "//tc:u"]:
        if t in tcs:
            p = tcs[t]
            parts.append("{}: {} label={} a={}".format(t, repr(p), p.label, (p[AInfo].where if AInfo in p else "-")))
    return " ".join(parts)

def _ta_impl(target, ctx):
    print("TA", ctx.rule.kind, target.label, _show(ctx))
    return [AInfo(where = str(target.label))]

ta = aspect(_ta_impl, attr_aspects = ["deps"], toolchains_aspects = ["//tc:t"])

def _tf(ctx):
    print("TF", ctx.rule.label, ctx.attr.which)
    return [Label("//tc:" + ctx.attr.which)]

tfa = aspect(_ta_impl, attr_aspects = ["deps"], toolchains_aspects = _tf, attrs = {"which": attr.string(default = "u", values = ["t", "u"])})

tall = aspect(_ta_impl, toolchains_aspects = ["*"])

def _plain_impl(target, ctx):
    tcs = ctx.rule.toolchains
    print("PL", target.label, repr(tcs))
    if ctx.rule.kind == "uses_tc":
        p = tcs["//tc:t"]
        print("PL2", repr(p), p.label, AInfo in p)
        tcs["//tc:missing"]
    return []

plain = aspect(_plain_impl)
