def _change(settings, attr):
    return {"//command_line_option:plugin": attr.plugins}

_transition = transition(
    implementation = _change,
    inputs = [],
    outputs = ["//command_line_option:plugin"],
)

def _impl(ctx):
    return [DefaultInfo(files = ctx.attr.dep[0][DefaultInfo].files)]

transition_probe = rule(implementation = _impl, attrs = {
    "dep": attr.label(default = "//:probe", cfg = _transition),
    "plugins": attr.string_list(),
    "_allowlist_function_transition": attr.label(default = "@bazel_tools//tools/allowlists/function_transition_allowlist"),
})
