"""Macros inheriting attributes (rules_py's `py_pex_binary`, OSS corpus B91)."""

def _impl(ctx):
    pass

my_rule = rule(
    implementation = _impl,
    doc = "A rule to inherit from.",
    attrs = {"srcs": attr.label_list(doc = "Sources.")},
)

def _macro_impl(name, visibility, **kwargs):
    my_rule(name = name, visibility = visibility, **kwargs)

from_rule = macro(
    implementation = _macro_impl,
    inherit_attrs = my_rule,
    doc = "Inherits my_rule's attributes, the common ones included.",
)

def _common_impl(name, visibility, **kwargs):
    pass

from_common = macro(
    implementation = _common_impl,
    inherit_attrs = "common",
    doc = "Inherits the common attributes.",
)
