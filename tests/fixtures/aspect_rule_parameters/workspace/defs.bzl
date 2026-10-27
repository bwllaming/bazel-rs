def _a(target, ctx):
    print("A", target.label, ctx.aspect_ids)
    return []

# `p` (no default) and `s` (empty default) are required parameters.
req = aspect(_a, attrs = {
    "p": attr.int(values = [0, 1, 2]),
    "s": attr.string(values = ["", "x"]),
    "d": attr.string(default = "x", values = ["x", "y"]),
})

novals = aspect(_a, attrs = {"n": attr.int(default = 3)})

flag = aspect(_a, attrs = {"b": attr.bool()})

def _r(ctx):
    return []

good = rule(_r, attrs = {"deps": attr.label_list(aspects = [req]), "p": attr.int(default = 2), "s": attr.string()})

wrong_type = rule(_r, attrs = {"deps": attr.label_list(aspects = [req]), "p": attr.string(), "s": attr.string()})

missing = rule(_r, attrs = {"deps": attr.label_list(aspects = [req]), "p": attr.int()})

uses_novals = rule(_r, attrs = {"deps": attr.label_list(aspects = [novals])})

uses_flag = rule(_r, attrs = {"deps": attr.label_list(aspects = [flag])})

lib = rule(_r)
