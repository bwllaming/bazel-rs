SeenInfo = provider(fields = ["text"])

def _impl_rule(ctx):
    return [platform_common.ToolchainInfo(version = ctx.attr.version)]

toolchain_impl = rule(_impl_rule, attrs = {"version": attr.string()})

def _seen_aspect_impl(target, ctx):
    if platform_common.ToolchainInfo in target:
        # The toolchain node, reached through toolchains_aspects.
        return [SeenInfo(text = "toolchain " + target[platform_common.ToolchainInfo].version)]
    has = "//aegtc:type" in ctx.rule.toolchains
    seen = "-"
    if has and SeenInfo in ctx.rule.toolchains["//aegtc:type"]:
        seen = ctx.rule.toolchains["//aegtc:type"][SeenInfo].text
    return [SeenInfo(text = "%s in=%s seen=%s" % (target.label.name, has, seen))]

# B85: ctx.rule.toolchains of a base target using automatic exec groups.
seen_aspect = aspect(
    implementation = _seen_aspect_impl,
    toolchains_aspects = ["//aegtc:type"],
)

def _user_impl(ctx):
    return []

user = rule(_user_impl, toolchains = ["//aegtc:type"])

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.dep[SeenInfo].text + "\n")
    return [DefaultInfo(files = depset([out]))]

report = rule(_report_impl, attrs = {"dep": attr.label(aspects = [seen_aspect])})
