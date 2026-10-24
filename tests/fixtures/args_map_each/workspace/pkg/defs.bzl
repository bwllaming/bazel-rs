"""Args map_each with built-in callables (Args.validateMapEach,
analysis/starlark/Args.java:366-388: any StarlarkCallable; only a
StarlarkFunction must be global)."""

def _probe_impl(ctx):
    # aspect_rules_js's js_image_layer (js/private/js_image_layer.bzl:510-545).
    entries = ctx.actions.args()
    entries.set_param_file_format("multiline")
    entries.add("{")
    entries.add_joined(
        ["bin/app", {"root": "bazel-out/k8-fastbuild/bin", "dest": "app.sh"}],
        join_with = ":",
        map_each = json.encode,
    )
    entries.add_all(
        ["x", ["y", 1], {"k": True, "a": None}],
        map_each = json.encode,
        before_each = ",",
    )
    entries.add("}")
    entries_json = ctx.actions.declare_file(ctx.label.name + "_entries.json")
    ctx.actions.write(entries_json, content = entries)

    # Other built-ins as map_each, in a spawn's command line.
    args = ctx.actions.args()
    args.add_all([1, True, "s"], map_each = str)
    args.add_all(depset(["p", "q"]), map_each = json.encode, format_each = "<%s>")
    args.add_joined(["u", "v"], join_with = "+", map_each = json.encode, format_joined = "[%s]")
    args.add_all(["a", "b"], map_each = json.encode, uniquify = True)
    args.add_all([], map_each = json.encode, omit_if_empty = False)
    out = ctx.actions.declare_file(ctx.label.name + "_args.txt")
    ctx.actions.run_shell(
        outputs = [out],
        arguments = [args],
        command = "for a in \"$@\"; do echo \"$a\"; done > " + out.path,
        mnemonic = "Args",
    )
    return [DefaultInfo(files = depset([entries_json, out]))]

probe = rule(implementation = _probe_impl)

def _bad_impl(ctx):
    args = ctx.actions.args()
    mode = ctx.attr.mode
    if mode == "joined_int":
        args.add_joined(["a"], join_with = ",", map_each = 1)
    elif mode == "all_string":
        args.add_all(["a"], map_each = "json")
    elif mode == "all_int_bad_format":
        args.add_all(["a"], map_each = 1, format_each = "no placeholder")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, args)
    return [DefaultInfo(files = depset([out]))]

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)
