Info = provider(fields = ["lines"])

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "content of " + ctx.label.name)
    copied = ctx.actions.declare_file(ctx.label.name + ".copy")
    ctx.actions.run_shell(
        outputs = [copied],
        inputs = [out],
        command = "cp $1 $2",
        arguments = [out.path, copied.path],
        mnemonic = "LibCopy",
    )
    return [DefaultInfo(files = depset([out, copied]))]

lib = rule(_lib_impl, attrs = {"deps": attr.label_list()})

def _flag_impl(ctx):
    return []

flag = rule(_flag_impl, build_setting = config.bool(flag = True))

def describe(target):
    lines = ["%s hasattr=%s" % (target.label.name, hasattr(target, "actions"))]
    for action in target.actions:
        lines.append("%s %s argv=%s -> %s" % (
            target.label.name,
            action.mnemonic,
            action.argv,
            " ".join([f.short_path for f in action.outputs.to_list()]),
        ))
    return lines

def _reader_impl(target, ctx):
    print("READER %s" % target.label)
    lines = describe(target)
    for dep in getattr(ctx.rule.attr, "deps", []):
        lines += dep[Info].lines
    return [Info(lines = lines)]

reader_aspect = aspect(_reader_impl, attr_aspects = ["deps"])

def _collect_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        lines += dep[Info].lines
    out = ctx.actions.declare_file(ctx.label.name + ".actions")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

collect = rule(_collect_impl, attrs = {"deps": attr.label_list(aspects = [reader_aspect])})
