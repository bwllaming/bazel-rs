load("@dep//:info.bzl", "Info")
def _setting(ctx):
    return Info(value = ctx.build_setting_value)
string_flag = rule(implementation = _setting, build_setting = config.string(flag = True))
bool_flag = rule(implementation = _setting, build_setting = config.bool(flag = True))
int_flag = rule(implementation = _setting, build_setting = config.int(flag = True))
private_setting = rule(implementation = _setting, build_setting = config.string(flag = False))
def _read(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "|".join([str(f[Info].value) for f in ctx.attr.flags]) + "|" + ctx.attr.selected + "|" + ctx.var["COMPILATION_MODE"])
    return [DefaultInfo(files = depset([out]))]
reader = rule(implementation = _read, attrs = {"flags": attr.label_list(), "selected": attr.string()})
def _change(settings, attr):
    return {"//command_line_option:flavor": "transition-" + settings["//command_line_option:flavor"]}
change = transition(implementation = _change, inputs = ["//command_line_option:flavor"], outputs = ["//command_line_option:flavor"])
def _forward(ctx):
    return [DefaultInfo(files = ctx.attr.dep[0][DefaultInfo].files)]
through_transition = rule(implementation = _forward, attrs = {
    "dep": attr.label(cfg = change),
    "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist"),
})
