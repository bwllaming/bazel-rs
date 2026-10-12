load("//advanced/provider_defs:defs.bzl", "OtherInfo", "PayloadInfo")

def _bad_impl(ctx):
    if ctx.attr.mode == "invalid_fields":
        return [PayloadInfo(unlisted = "not allowed")]
    if ctx.attr.mode == "missing":
        return [ctx.attr.dep[OtherInfo]]
    if ctx.attr.mode == "duplicate":
        return [PayloadInfo(message = "first"), PayloadInfo(message = "second")]
    if ctx.attr.mode == "frozen":
        ctx.attr.dep[PayloadInfo].words.append("not allowed")
    return []

bad = rule(implementation = _bad_impl, attrs = {
    "mode": attr.string(mandatory = True),
    "dep": attr.label(default = "//advanced/producer:payload"),
})
