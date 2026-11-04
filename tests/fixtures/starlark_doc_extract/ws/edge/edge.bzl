load("@dep_mod//sub:lib.bzl", "ExtInfo", "ext_function")
"""Not a module docstring: loads come first."""

ext_reexport = ext_function
ExtInfoAlias = ExtInfo

def _impl(ctx):
    pass

def _computed(name):
    return "//edge:" + name

setting = rule(
    implementation = _impl,
    build_setting = config.string(flag = True),
    doc = "A build setting with ünïcödé and \ttab.",
)

tool_rule = rule(
    implementation = _impl,
    attrs = {
        "tool": attr.label(cfg = "exec", executable = True, allow_single_file = True, doc = "Exec tool."),
        "srcs": attr.label_list(allow_files = [".txt"], providers = [DefaultInfo, ExtInfo]),
        "_computed": attr.label(default = _computed),
        "neg": attr.int(default = -5),
        "s": attr.string(default = "quote\"back\\slash\nnewline"),
        "mixed": attr.string(values = ["x", "y"]),
        "lkd": attr.label_keyed_string_dict(default = {"//edge:a": "x", "@dep_mod//sub:lib": "y"}),
        "skd": attr.string_keyed_label_dict(default = {"k": "@dep_mod//sub:lib"}),
        "lld": attr.label_list_dict(default = {"k": ["//edge:a", "//edge"]}),
        "ll": attr.label_list(default = ["//edge:edge", "//:sde_fixture", "@dep_mod//:dep_mod"]),
    },
    doc = """
    Leading blank line doc.
    """,
)

def fn_with_tabs(a):
    """Tab indented docstring.

	Args:
	  a: the a.
	"""
    pass

def defaults(a = 1.5, b = ("x", Label("@dep_mod//sub:lib")), c = {Label("//edge:a"): [None, True]}, d = "é", e = struct(x = 1), f = ExtInfo, g = len):
    """Defaults.

    Arguments:
      a: alias Arguments: header.
      f (Provider, deprecated): attributes in parentheses.
      nonexistent: ignored.
    """
    pass

def kwonly(*, x, y = 2):
    """Keyword only.

    Returns:
      Nothing
       at all.
    """
    pass

ns = struct(
    inner = struct(fn = kwonly, deeper = struct(p = ExtInfo)),
    other = defaults,
    info = ExtInfo,
)

ns2 = struct(x = fn_with_tabs)

_private_ns = struct(visible = kwonly)

#: The answer, with ünïcödé.
ANSWER = 42

#: A struct constant.
CONFIG = struct(a = 1)

#: Typed list.
LIST = [1, 2]

#: A label.
LABEL = Label("//edge:a")

def _aspect_impl(target, ctx):
    return []

attr_fn_aspect = aspect(implementation = _aspect_impl, attr_aspects = ["*"], required_providers = [ExtInfo])

unexported_holder = struct(r = provider(doc = "never exported"))

def varargs_middle(a, *rest, b, c = None, **options):
    """Varargs before keyword-only parameters.

    Args:
      a: first.
      *rest: the rest.
      b: keyword b.
      **options: the options.
    """
    pass
