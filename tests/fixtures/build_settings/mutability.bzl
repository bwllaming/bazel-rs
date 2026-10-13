Info = provider(fields = ["text"])

def _mutating(ctx):
    first = ctx.build_setting_value
    first.append("mutated")
    second = ctx.build_setting_value
    return [Info(text = "|".join(first) + ";" + "|".join(second) + ";" + "|".join(ctx.attr.build_setting_default))]

mutating = rule(implementation = _mutating, build_setting = config.string_list())

def _snapshot(ctx):
    return [Info(text = "|".join(ctx.build_setting_value) + ";" + "|".join(ctx.attr.build_setting_default))]

snapshot_setting = rule(implementation = _snapshot, build_setting = config.string_list())

def late_default(name):
    values = ["seed"]
    snapshot_setting(name = name, build_setting_default = values)
    values.append("late")

def _capture(ctx):
    lines = [dep.label.name + "=" + dep[Info].text for dep in ctx.attr.deps]
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([output]))]

capture = rule(implementation = _capture, attrs = {"deps": attr.label_list()})
