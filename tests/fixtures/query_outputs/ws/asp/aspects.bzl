NeedInfo = provider(fields = [])

def _aspect_impl(target, ctx):
    return []

# Applies only to dependencies whose rule class advertises NeedInfo.
needs_aspect = aspect(
    implementation = _aspect_impl,
    attr_aspects = ["deps"],
    required_providers = [NeedInfo],
    attrs = {"_needs_tool": attr.label(default = "//asp:needs_tool")},
)

# Applies to every rule dependency.
open_aspect = aspect(
    implementation = _aspect_impl,
    attr_aspects = ["deps"],
    attrs = {"_open_tool": attr.label(default = "//asp:open_tool")},
)

def _impl(ctx):
    return []

def _provider_impl(ctx):
    return [NeedInfo()]

aspected = rule(
    implementation = _impl,
    attrs = {"deps": attr.label_list(aspects = [needs_aspect, open_aspect])},
)

plain = rule(implementation = _impl)

advertising = rule(implementation = _provider_impl, provides = [NeedInfo])
