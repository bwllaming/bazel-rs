"""Small trusted fixtures for the initial custom-rule API, not general rules."""

def _text_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr.text)
    return [DefaultInfo(files = depset([out]))]

text = rule(implementation = _text_impl, attrs = {
    "text": attr.string(mandatory = True),
})

_words = ["first"]
_words_attr = attr.string_list(default = _words)
_words.append("not-in-snapshot")

def _scalar_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    content = "%s|%s|%s|%s|%s\n" % (
        str(ctx.label), ctx.attr.message, ",".join(ctx.attr.words),
        ctx.attr.number, ctx.attr.enabled,
    )
    ctx.actions.write(output = out, content = content)
    # Official Bazel 9.3 accepts a bare provider as well as a provider list.
    return DefaultInfo(files = depset([out]))

scalar = rule(implementation = _scalar_impl, attrs = {
    "message": attr.string(default = "héllo 雪"),
    "words": _words_attr,
    "number": attr.int(default = 7),
    "enabled": attr.bool(default = True),
})

def _chain_impl(ctx):
    seed = ctx.actions.declare_file(ctx.label.name + ".seed")
    out = ctx.actions.declare_file(ctx.label.name + ".joined")
    # Deliberately register the consumer before its own producer. File.path is
    # only an execution argument: native/reference configuration roots differ.
    ctx.actions.run_shell(
        inputs = depset([ctx.file.src, seed]),
        outputs = [out],
        command = 'false; cat "$1" "$2" > "$3"; printf "%s|%s\\n" "$4" "$CUSTOM_WORD" >> "$3"',
        arguments = [ctx.file.src.path, seed.path, out.path, "argument value"],
        env = {"CUSTOM_WORD": "env value"},
    )
    ctx.actions.write(seed, "seed\n")
    return [DefaultInfo(files = depset([out]))]

chain = rule(implementation = _chain_impl, attrs = {
    "src": attr.label(allow_single_file = True, mandatory = True),
})

def _forward_impl(ctx):
    return [DefaultInfo(files = depset(
        direct = [ctx.file.src],
        transitive = [target[DefaultInfo].files for target in ctx.attr.more],
        order = "postorder",
    ))]

forward = rule(implementation = _forward_impl, attrs = {
    "src": attr.label(default = "//:a.txt", allow_single_file = True),
    "more": attr.label_list(default = ["//:b.txt"], allow_files = True),
})

def _inspect_impl(ctx):
    files = ctx.attr.dep[DefaultInfo].files.to_list()
    content = "|".join([f.short_path for f in files]) + "\n"
    content += "|".join([f.short_path for f in ctx.files.dep]) + "\n"
    content += ctx.file.src.short_path + "|" + ctx.file.src.basename + "\n"
    for out in [ctx.outputs.out] + ctx.outputs.extras:
        ctx.actions.write(out, content)
    # Returning None defaults to the predeclared outputs, not dynamic outputs.

inspect = rule(implementation = _inspect_impl, attrs = {
    "dep": attr.label(default = "//:forwarded"),
    "src": attr.label(default = "//:a.txt", allow_single_file = True),
    "out": attr.output(mandatory = True),
    "extras": attr.output_list(),
})

def _empty_impl(ctx):
    out = ctx.actions.declare_file("unused-dynamic.txt")
    ctx.actions.write(out, "must remain unmaterialized\n")
    return [DefaultInfo(files = depset([]))]

empty = rule(implementation = _empty_impl)

def _none_impl(ctx):
    out = ctx.actions.declare_file("unused-none.txt")
    ctx.actions.write(out, "dynamic is not a default\n")

none = rule(implementation = _none_impl)

def _hidden_impl(ctx):
    ctx.actions.write(ctx.outputs.out, "explicit predeclared selection\n")
    return [DefaultInfo(files = depset([]))]

hidden = rule(implementation = _hidden_impl, attrs = {"out": attr.output()})
