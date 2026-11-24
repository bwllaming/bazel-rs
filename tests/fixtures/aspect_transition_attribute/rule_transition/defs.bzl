_SETTINGS = [
    "//command_line_option:compilation_mode",
    "//command_line_option:cpu",
    "//command_line_option:extra_execution_platforms",
    "//command_line_option:host_cpu",
    "//command_line_option:host_platform",
    "//command_line_option:platform_suffix",
]

def _flip_impl(settings, _attr):
    if settings["//command_line_option:cpu"] != "flipped":
        return settings | {
            "//command_line_option:compilation_mode": "opt",
            "//command_line_option:cpu": "flipped",
            "//command_line_option:platform_suffix": "",
        }
    return settings | {
        "//command_line_option:cpu": settings["//command_line_option:host_cpu"],
        "//command_line_option:extra_execution_platforms": [settings["//command_line_option:host_platform"]],
    }

# Not idempotent (bazel_env.bzl's `_flip_output_dir`): its delegate is
# analyzed without applying it again.
flip = transition(
    implementation = _flip_impl,
    inputs = _SETTINGS,
    outputs = _SETTINGS,
)

def _env_impl(ctx):
    return [DefaultInfo()]

env = rule(_env_impl, cfg = flip, attrs = {
    "mode": attr.string(),
    "tool": attr.label(cfg = "exec"),
})

def _top_impl(target, ctx):
    print("top on %s: mode=%s tool=%s" % (target.label, ctx.rule.attr.mode, ctx.rule.attr.tool.label))
    return []

top = aspect(implementation = _top_impl, attr_aspects = ["deps"])
