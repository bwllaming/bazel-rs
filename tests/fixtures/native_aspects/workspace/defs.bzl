NInfo = provider(fields = ["label", "kind", "seen"])

def _one(v):
    t = type(v)
    if t == "Target":
        return "T(%s)" % v.label
    if t == "File":
        return "F(%s)" % v.short_path
    if t == "Label":
        return "L(%s)" % v
    return repr(v)

def _show(v):
    t = type(v)
    if t == "list":
        return "[" + ", ".join([_one(x) for x in v]) + "]"
    if t == "dict":
        return "{" + ", ".join(["%s: %s" % (_one(k), _one(x)) for k, x in v.items()]) + "}"
    return _one(v)

def _asp_impl(target, ctx):
    kind = ctx.rule.kind
    if not str(target.label).startswith("@@//"):
        return [NInfo(label = str(target.label), kind = kind, seen = [])]
    print("K %s kind=%s attrs=%s" % (target.label, kind, sorted(dir(ctx.rule.attr))))
    for name in sorted(dir(ctx.rule.attr)):
        v = getattr(ctx.rule.attr, name)
        print("V %s %s %s %s" % (target.label, name, type(v), _show(v)))
    print("F %s files=%s" % (target.label, sorted(["%s=%s" % (k, _show(getattr(ctx.rule.files, k))) for k in dir(ctx.rule.files)])))
    print("S %s file=%s executable=%s" % (target.label, sorted(["%s=%s" % (k, _show(getattr(ctx.rule.file, k))) for k in dir(ctx.rule.file)]), sorted(["%s=%s" % (k, _show(getattr(ctx.rule.executable, k))) for k in dir(ctx.rule.executable)])))
    seen = []
    for name in dir(ctx.rule.attr):
        v = getattr(ctx.rule.attr, name)
        vs = v if type(v) == "list" else [v]
        for d in vs:
            if type(d) == "Target" and NInfo in d:
                seen.append("%s:%s" % (name, d[NInfo].label))
    print("P %s seen=%s" % (target.label, sorted(seen)))
    out = ctx.actions.declare_file(ctx.label.name + ".asp")
    ctx.actions.write(out, kind)
    return [NInfo(label = str(target.label), kind = kind, seen = seen), OutputGroupInfo(asp = depset([out]))]

asp = aspect(implementation = _asp_impl, attr_aspects = ["*"])

def _root_impl(ctx):
    for d in ctx.attr.pdeps:
        print("O %s pinfo=%s" % (d.label, d[PInfo].kind if PInfo in d else "-"))
    for d in ctx.attr.deps:
        print("R %s info=%s files=%s exe=%s" % (
            d.label,
            d[NInfo].kind if NInfo in d else "-",
            sorted([f.short_path for f in d[DefaultInfo].files.to_list()]),
            d[DefaultInfo].files_to_run.executable.short_path if d[DefaultInfo].files_to_run.executable else None,
        ))
    return []

def _pasp_impl(target, ctx):
    print("Q %s kind=%s" % (target.label, ctx.rule.kind))
    return [PInfo(kind = ctx.rule.kind)]

PInfo = provider(fields = ["kind"])

# Applies only to targets advertising PlatformInfo.
pasp = aspect(
    implementation = _pasp_impl,
    required_providers = [platform_common.PlatformInfo],
)

root = rule(implementation = _root_impl, attrs = {
    "deps": attr.label_list(aspects = [asp], allow_files = True),
    "pdeps": attr.label_list(aspects = [pasp]),
})

def _t_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = out)]

my_test = rule(implementation = _t_impl, test = True)

def _lib_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.srcs))]

lib = rule(implementation = _lib_impl, attrs = {"srcs": attr.label_list(allow_files = True)})

# The rules_multirun shape: an aspect on `commands` reading the base rule's
# `args`/`env` (absent on native rules) and the target's executable.
BinaryArgsEnvInfo = provider(fields = ["args", "env"])

def _binary_args_env_aspect_impl(target, ctx):
    is_executable = target.files_to_run != None and target.files_to_run.executable != None
    args = getattr(ctx.rule.attr, "args", [])
    env = getattr(ctx.rule.attr, "env", {})
    print("M %s kind=%s exe=%s args=%s env=%s data=%s" % (target.label, ctx.rule.kind, is_executable, args, env, hasattr(ctx.rule.attr, "data")))
    return [BinaryArgsEnvInfo(args = args, env = env)]

_binary_args_env_aspect = aspect(implementation = _binary_args_env_aspect_impl)

def _multirun_impl(ctx):
    for c in ctx.attr.commands:
        exe = c[DefaultInfo].files_to_run.executable
        print("N %s exe=%s args=%s files=%s" % (
            c.label,
            exe.short_path if exe else None,
            c[BinaryArgsEnvInfo].args if BinaryArgsEnvInfo in c else "-",
            sorted([f.short_path for f in c[DefaultInfo].files.to_list()]),
        ))
    return []

multirun = rule(
    implementation = _multirun_impl,
    attrs = {"commands": attr.label_list(aspects = [_binary_args_env_aspect], allow_files = True)},
)
