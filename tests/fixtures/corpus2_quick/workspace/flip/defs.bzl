_S = ["//command_line_option:compilation_mode", "//command_line_option:cpu", "//command_line_option:extra_execution_platforms", "//command_line_option:host_cpu", "//command_line_option:host_platform", "//command_line_option:platform_suffix"]
def _flip(settings, attr):
    print("IN", settings)
    if settings["//command_line_option:cpu"] != "bazel_env":
        return settings | {"//command_line_option:compilation_mode": "opt", "//command_line_option:cpu": "bazel_env", "//command_line_option:platform_suffix": ""}
    return settings | {"//command_line_option:cpu": settings["//command_line_option:host_cpu"], "//command_line_option:extra_execution_platforms": [settings["//command_line_option:host_platform"]]}
flip = transition(implementation = _flip, inputs = _S, outputs = _S)
def _w(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.bin_dir.path + "\n")
    return [DefaultInfo(files = depset([out] + ctx.files.dep))]
w = rule(implementation = _w, attrs = {"dep": attr.label_list(cfg = flip)})
