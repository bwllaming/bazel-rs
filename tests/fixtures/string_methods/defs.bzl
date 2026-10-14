# Generated from a byte-level model of Bazel 9.3 StringModule/Printer.
# Non-ASCII samples are raw UTF-8; controls use octal escapes valid in Bazel.
SAMPLES = [
    "",  # 0
    "abc",  # 1
    "ABC",  # 2
    "Hello World",  # 3
    "hELLO wORLD",  # 4
    "a1b",  # 5
    "Catch-22",  # 6
    "HAL-9000",  # 7
    "123",  # 8
    " \t\n\013\014\r ",  # 9
    "\034",  # 10
    "\037",  # 11
    "\177",  # 12
    "\034a\037",  # 13
    " a\177 ",  # 14
    "é",  # 15 U+00E9
    "õ",  # 16 U+00F5
    "õa",  # 17 U+00F5
    "ß",  # 18 U+00DF
    "ßa",  # 19 U+00DF
    "µa",  # 20 U+00B5
    "ºa",  # 21 U+00BA
    "ªa",  # 22 U+00AA
    "aÉ",  # 23 U+00C9
    "Éa",  # 24 U+00C9
    "ÉA",  # 25 U+00C9
    "élan",  # 26 U+00E9
    "Élan",  # 27 U+00C9
    "ÉLAN",  # 28 U+00C9
    "Straße",  # 29 U+00DF
    "中x",  # 30 U+4E2D
    "x中",  # 31 U+4E2D
    " ",  # 32 U+00A0
    " a ",  # 33 U+00A0 U+00A0
    "　",  # 34 U+3000
    "　a",  # 35 U+3000
    "a　",  # 36 U+3000
    "",  # 37 U+0085
    "²",  # 38 U+00B2
    "١",  # 39 U+0661
    "\"q\\\"",  # 40
]
STRIP_CHARS = [
    ("xxaxx", "x"),
    (" \034a\034 ", "\034"),
    ("éaé", "é"),
    ("ê", "é"),
    (" \034 a\037 ", None),
]
METHODS = ["lower", "upper", "capitalize", "title", "isalpha", "isalnum", "isdigit", "isspace", "islower", "isupper", "istitle", "strip", "lstrip", "rstrip", "repr"]
# Rows where Bazel's byte-wise result is not valid UTF-8 (see docs/STRING_METHODS.md).
DIVERGENT = {
    "capitalize": [30, 34, 35],
    "title": [23, 29, 30, 33, 34, 35],
    "strip_chars": [3],
    "lstrip_chars": [3],
}

def _apply(method, s):
    if method == "lower":
        return s.lower()
    if method == "upper":
        return s.upper()
    if method == "capitalize":
        return s.capitalize()
    if method == "title":
        return s.title()
    if method == "isalpha":
        return s.isalpha()
    if method == "isalnum":
        return s.isalnum()
    if method == "isdigit":
        return s.isdigit()
    if method == "isspace":
        return s.isspace()
    if method == "islower":
        return s.islower()
    if method == "isupper":
        return s.isupper()
    if method == "istitle":
        return s.istitle()
    if method == "strip":
        return s.strip()
    if method == "lstrip":
        return s.lstrip()
    if method == "rstrip":
        return s.rstrip()
    if method == "repr":
        return repr(s)
    fail("unknown method " + method)

def _apply_chars(method, s, chars):
    if method == "strip":
        return s.strip(chars)
    if method == "lstrip":
        return s.lstrip(chars)
    return s.rstrip(chars)

def _row(method, i, value):
    tag = "divergent" if i in DIVERGENT.get(method, []) else "bazel"
    text = str(value) if type(value) == "bool" else repr(value)
    return "%s %s %d %s" % (tag, method, i, text)

def report():
    rows = []
    for method in METHODS:
        for i, s in enumerate(SAMPLES):
            rows.append(_row(method, i, _apply(method, s)))
    for i, (s, chars) in enumerate(STRIP_CHARS):
        for method in ["strip", "lstrip", "rstrip"]:
            rows.append(_row(method + "_chars", i, _apply_chars(method, s, chars)))
    return "\n".join(rows) + "\n"

def _check(ctx):
    impl = ctx.actions.declare_file(ctx.label.name + ".impl.txt")
    build = ctx.actions.declare_file(ctx.label.name + ".build.txt")
    ctx.actions.write(impl, report())
    ctx.actions.write(build, ctx.attr.build_report)
    return [DefaultInfo(files = depset([build, impl]))]

check = rule(implementation = _check, attrs = {"build_report": attr.string()})
