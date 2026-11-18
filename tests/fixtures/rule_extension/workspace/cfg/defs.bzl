FlagInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [FlagInfo(value = ctx.build_setting_value)]

string_flag = rule(_flag_impl, build_setting = config.string(flag = True))

def _append(suffix):
    def _impl(settings, attr):
        return {"//cfg:flag": settings["//cfg:flag"] + suffix}

    return transition(implementation = _impl, inputs = ["//cfg:flag"], outputs = ["//cfg:flag"])

def _base_impl(ctx):
    print("RX|cfg_base_impl|%s|flag=%s" % (ctx.label.name, ctx.attr._flag[FlagInfo].value))
    return []

base = rule(
    _base_impl,
    cfg = _append("+base"),
    attrs = {"_flag": attr.label(default = "//cfg:flag")},
)

def _mid_impl(ctx):
    print("RX|cfg_mid_impl|%s|flag=%s" % (ctx.label.name, ctx.attr._flag[FlagInfo].value))
    return ctx.super()

mid = rule(_mid_impl, parent = base, cfg = _append("+mid"))

def _top_impl(ctx):
    return ctx.super()

top = rule(_top_impl, parent = mid, cfg = _append("+top"))

plain_child = rule(_top_impl, parent = base)
