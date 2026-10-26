def _a(target, ctx):
    print("A", target.label, "p=" + ctx.attr.p, "n=" + str(ctx.attr.n), ctx.aspect_ids)
    return []

a = aspect(
    _a,
    attr_aspects = ["deps"],
    attrs = {
        "p": attr.string(default = "d", values = ["d", "x", "y"]),
        "n": attr.int(default = 0, values = [0, 1, 2]),
    },
)

def _r(ctx):
    return []

r = rule(_r, attrs = {
    "deps": attr.label_list(aspects = [a]),
    "p": attr.string(default = "y"),
    "n": attr.int(default = 1),
})

plain = rule(_r, attrs = {"deps": attr.label_list()})

only_p = rule(_r, attrs = {"deps": attr.label_list(aspects = [a]), "p": attr.string(default = "d"), "n": attr.int()})
