"""The aspect context's own attribute structs (docs/ASPECTS.md "The aspect context").

`ctx.attr`, `ctx.files`, `ctx.file` and `ctx.executable` of an aspect are
built from the main aspect's attributes (StarlarkRuleContext.java:270-285,
StarlarkAttributesCollection.Builder.addAttribute): every dependency
attribute has a `ctx.files` entry, `allow_single_file` ones a `ctx.file`
entry, executable label attributes a `ctx.executable` entry whose
FilesToRunProvider (runfiles) a spawn running the File picks up.
"""

def _tool_impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        exe,
        "#!/bin/sh\n{ echo \"tool=$0\"; cat \"$0.runfiles/_main/data.txt\"; } > \"$1\"\n",
        is_executable = True,
    )
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(_tool_impl, executable = True, attrs = {"data": attr.label(allow_single_file = True)})

def _two_impl(ctx):
    first = ctx.actions.declare_file(ctx.label.name + ".1")
    second = ctx.actions.declare_file(ctx.label.name + ".2")
    ctx.actions.write(first, "1\n")
    ctx.actions.write(second, "2\n")
    return [DefaultInfo(files = depset([first, second]))]

two_files = rule(_two_impl)

def _names(values):
    return [value.label.name for value in values]

def _paths(files):
    return [file.path for file in files]

def _attrs_aspect_impl(target, ctx):
    print("DIR attr={}".format(dir(ctx.attr)))
    print("DIR files={}".format(dir(ctx.files)))
    print("DIR file={}".format(dir(ctx.file)))
    print("DIR executable={}".format(dir(ctx.executable)))
    print("VAL exec_tool={} target_tool={} optional={}".format(
        ctx.executable._tool.path,
        ctx.executable._target_tool.path,
        ctx.executable._optional.path,
    ))
    print("VAL config={} list_attr={} list_files={}".format(
        ctx.file._config.path,
        _names(ctx.attr._list),
        _paths(ctx.files._list),
    ))
    print("VAL tool_files={} config_files={} param={} dict={} ints={}".format(
        _paths(ctx.files._tool),
        _paths(ctx.files._config),
        ctx.attr.param,
        ctx.attr._dict,
        ctx.attr._ints,
    ))
    out = ctx.actions.declare_file(ctx.label.name + ".exe")
    ctx.actions.run(outputs = [out], executable = ctx.executable._tool, arguments = [out.path])
    return [OutputGroupInfo(exe_out = depset([out]))]

attrs_aspect = aspect(
    implementation = _attrs_aspect_impl,
    attrs = {
        "_tool": attr.label(default = "//:tool", executable = True, cfg = "exec"),
        "_target_tool": attr.label(default = "//:tool", executable = True, cfg = "target"),
        "_optional": attr.label(default = "//:config.toml", allow_single_file = True, executable = True, cfg = "exec"),
        "_config": attr.label(default = "//:config.toml", allow_single_file = True),
        "_list": attr.label_list(default = ["//:two", "//:config.toml", "//:two_alias"], allow_files = True),
        "_dict": attr.string_dict(default = {"k": "v"}),
        "_ints": attr.int_list(default = [1, 2]),
        "param": attr.string(default = "p", values = ["p", "q"]),
    },
)

def _lib_impl(ctx):
    return [DefaultInfo()]

lib = rule(_lib_impl)
