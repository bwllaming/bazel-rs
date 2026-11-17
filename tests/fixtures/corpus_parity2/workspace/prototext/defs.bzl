# proto.encode_text (rules_cc's rule-based toolchain tests).
P = provider(fields = ["name", "values", "inner"])

CASES = [
    struct(field = 123),
    struct(field = True, other = False),
    struct(field = [1, 2, 3]),
    struct(field = "te\"x\\t\nline", ignored_field = None),
    struct(field = struct(inner_field = "text", ignored_field = None)),
    struct(field = [struct(inner_field = 1), struct(inner_field = 2)]),
    struct(foo = {4: 3, 2: 1}, bar = {"k": struct(a = 1)}),
    struct(zeta = 1, alpha = 2, Beta = 3, _under = 4),
    struct(f = 1.5, g = float("inf"), h = -0.0, i = 1e30),
    struct(t = (1, "a"), r = range(3)),
    P(name = "n", values = ["a", "b"], inner = struct(x = None)),
    struct(empty = struct(), none_list = [], big = 123456789012345678901234567890),
]

ERRORS = {
    "nested_list": struct(field = [[1]]),
    "bad_key": struct(field = {1.5: 1}),
    "bad_value": struct(field = {"k": [1]}),
    "bad_type": struct(field = depset([])),
    "not_struct": [1],
}

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    if ctx.attr.error:
        proto.encode_text(ERRORS[ctx.attr.error])
    text = "".join(["=== %d\n%s" % (i, proto.encode_text(c)) for i, c in enumerate(CASES)])
    ctx.actions.write(out, text + repr(dir(proto)) + "\n")
    return [DefaultInfo(files = depset([out]))]

encode = rule(implementation = _impl, attrs = {"error": attr.string()})
