"""Module docstring.

With a second paragraph.
"""

load("//lib:dep.bzl", "DepInfo", _dep_helper = "dep_helper")

MyInfo = provider(
    doc = """My provider.

    Indented detail.
    """,
    fields = {
        "foo": "The foo field.",
        "bar": """Multi
          line bar.""",
        "_hidden": "private",
    },
)

PlainInfo = provider()

ListInfo = provider("Fields as list.", fields = ["a", "b"])

def _init_impl(x, y = 2, *, z = "z", **kwargs):
    """Init for InitInfo.

    Args:
      x: the x.
      y: the y.
      z: keyword z.
      **kwargs: extras.
    """
    return {"x": x, "y": y}

InitInfo, _new_init_info = provider(doc = "Has init.", fields = ["x", "y"], init = _init_impl)

def my_function(name, srcs = [], deps = None, label = Label("//pkg:foo"), other = Label("@bazel_tools//tools:bzl_srcs"), *args, flag = True, **kwargs):
    """Does something.

    A longer description
    spanning two lines.

    Args:
      name: The name.
      srcs: Source files
        continued here.

        And a second paragraph.
      deps (list, optional): Deps.
      label: A label.
      other: Another label.
      *args: Positional extras.
      flag: A flag.
      **kwargs: Keyword extras.

    Returns:
      A thing,
        indented more.

    Deprecated:
      Use something else.
    """
    pass

def undocumented(a, b = {"k": [1, 2]}, c = (1,), d = -3):
    pass

def summary_only():
    """Just a summary."""
    pass

def _private_function():
    pass

renamed_dep_helper = _dep_helper
reexported_dep_info = DepInfo

lambda_fn = lambda x: x

def _rule_impl(ctx):
    pass

my_rule = rule(
    implementation = _rule_impl,
    doc = """A rule.

    Detail line.
    """,
    attrs = {
        "srcs": attr.label_list(allow_files = True, doc = "Sources."),
        "dep": attr.label(providers = [MyInfo], doc = "A dep."),
        "deps2": attr.label_list(providers = [[MyInfo, DepInfo], [ListInfo]]),
        "required": attr.string(mandatory = True, doc = "Required string."),
        "choice": attr.string(default = "a", values = ["a", "b"]),
        "count": attr.int(default = 3, values = [1, 3]),
        "flag": attr.bool(default = True),
        "names": attr.string_list(default = ["x", "y"]),
        "ints": attr.int_list(default = [1, 2]),
        "sd": attr.string_dict(default = {"k": "v"}),
        "sld": attr.string_list_dict(default = {"k": ["v"]}),
        "lsd": attr.label_keyed_string_dict(),
        "out": attr.output(doc = "An output."),
        "outs": attr.output_list(),
        "default_label": attr.label(default = "//pkg:foo"),
        "default_label2": attr.label(default = Label("//lib:dep")),
        "ext_label": attr.label(default = "@bazel_tools//tools/sh:toolchain_type"),
        "sk": attr.string_keyed_label_dict(),
        "lld": attr.label_list_dict(),
        "_private": attr.label(default = "//pkg:foo"),
    },
    provides = [MyInfo, DepInfo],
)

my_binary = rule(implementation = _rule_impl, executable = True)

my_test = rule(implementation = _rule_impl, test = True, attrs = {"size2": attr.string()})

def _aspect_impl(target, ctx):
    return []

my_aspect = aspect(
    implementation = _aspect_impl,
    doc = "An aspect.",
    attr_aspects = ["deps", "_private_dep"],
    attrs = {
        "param": attr.string(values = ["p", "q"], default = "p", doc = "Param."),
        "_tool": attr.label(default = "//pkg:foo"),
    },
)

plain_aspect = aspect(implementation = _aspect_impl)

def _macro_impl(name, visibility, srcs, opt):
    pass

my_macro = macro(
    implementation = _macro_impl,
    doc = "A symbolic macro.",
    attrs = {
        "srcs": attr.label_list(doc = "Macro srcs."),
        "opt": attr.string(configurable = False, default = "o"),
    },
)

my_finalizer = macro(implementation = lambda name, visibility: None, finalizer = True)

def _repo_impl(rctx):
    pass

my_repo = repository_rule(
    implementation = _repo_impl,
    doc = "A repo rule.",
    attrs = {
        "url": attr.string(doc = "URL."),
        "sha": attr.string(mandatory = True),
        "build_file": attr.label(),
    },
    environ = ["FOO", "BAR"],
)

plain_repo = repository_rule(implementation = _repo_impl)

def _ext_impl(mctx):
    pass

_tag = tag_class(doc = "A tag.", attrs = {"name": attr.string(mandatory = True), "v": attr.int(default = 1, doc = "Version.")})

my_ext = module_extension(
    implementation = _ext_impl,
    doc = "An extension.",
    tag_classes = {"install": _tag, "other": tag_class()},
)

namespace = struct(
    fn = my_function,
    info = MyInfo,
    inner = struct(rule = my_rule, value = 1),
    _private = summary_only,
)

#: Doc comment for a constant.
#: Second line.
MY_CONSTANT = 42

OTHER = "x"  #: Trailing doc comment.

UNDOCUMENTED_CONSTANT = [1]
