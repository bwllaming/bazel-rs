print("bzl top")

def _tr(settings, attr):
    print("in transition")
    return {"//command_line_option:compilation_mode": "dbg"}

tr = transition(implementation = _tr, inputs = [], outputs = ["//command_line_option:compilation_mode"])

def _asp(target, ctx):
    print("in aspect", target.label)
    return []

asp = aspect(_asp, attr_aspects = ["deps"])

def _r(ctx):
    print("in rule", ctx.label, "two", sep = "|")
    print("multi\nline")
    return []

r = rule(_r, attrs = {"deps": attr.label_list(aspects = [asp]), "t": attr.label_list(cfg = tr)})

def mac(name):
    print("in macro")
    r(name = name)

def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit %d\n" % ctx.attr.code, is_executable = True)
    return [DefaultInfo(executable = out)]

my_test = rule(_t, test = True, attrs = {"code": attr.int()})
