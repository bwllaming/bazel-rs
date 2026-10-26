Info = provider(fields = ["mode", "file"])
AspectInfo = provider(fields = ["mode", "extras"])

def _setting(ctx):
    return [Info(mode = ctx.build_setting_value, file = None)]

setting = rule(implementation = _setting, build_setting = config.string(flag = True))

def _dep(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    mode = ctx.attr._flag[Info].mode
    ctx.actions.run_shell(inputs = ctx.files.srcs + ctx.files.tool, outputs = [out], command = "printf '" + mode + ":' > " + out.path + "; cat " + " ".join([f.path for f in ctx.files.srcs]) + " >> " + out.path)
    return [Info(mode = mode, file = out), DefaultInfo(files = depset([out]))]

dep = rule(implementation = _dep, attrs = {"_flag": attr.label(default = "//:flag"), "srcs": attr.label_list(allow_files = True), "tool": attr.label(cfg = "exec", allow_files = True)})

def _named(settings, attr):
    return {"z": {"//:flag": "opt"}, "a": {"//:flag": "dbg"}, "m": {"//:flag": "opt"}}

def _list(settings, attr):
    return [{"//:flag": "opt"}, {"//:flag": "dbg"}, {"//:flag": "opt"}]

def _empty(settings, attr):
    return []

def _patch(settings, attr):
    return {"//:flag": "patch"}

named = transition(implementation = _named, inputs = [], outputs = ["//:flag"])
listed = transition(implementation = _list, inputs = [], outputs = ["//:flag"])
empty = transition(implementation = _empty, inputs = [], outputs = ["//:flag"])
patch = transition(implementation = _patch, inputs = [], outputs = ["//:flag"])

def _aspect(target, ctx):
    return [AspectInfo(mode = target[Info].mode, extras = [d[Info].mode for d in ctx.attr._extra])]

probe_aspect = aspect(implementation = _aspect, attrs = {"_extra": attr.label(default = "//:extra", cfg = listed)})

def _consumer(ctx):
    def modes(deps):
        return [d[Info].mode if Info in d else "source" for d in deps]
    summary = {"one": modes(ctx.attr.one), "many": modes(ctx.attr.many), "source": modes(ctx.attr.source), "empty": modes(ctx.attr.empty), "patch": modes(ctx.attr.patch), "unset": modes(ctx.attr.unset), "one_keys": list(ctx.split_attr.one), "many_keys": list(ctx.split_attr.many), "source_keys": list(ctx.split_attr.source), "empty_keys": list(ctx.split_attr.empty), "patch_keys": list(ctx.split_attr.patch), "unset_keys": list(ctx.split_attr.unset), "aspects": [(d[AspectInfo].mode, d[AspectInfo].extras) for d in ctx.attr.aspected]}
    print("SPLIT:" + json.encode(summary))
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode(summary))
    proof = ctx.actions.declare_file(ctx.label.name + ".proof")
    inputs = [d[Info].file for d in ctx.attr.one]
    ctx.actions.run_shell(inputs = inputs, outputs = [proof], command = "cat " + " ".join([f.path for f in inputs]) + " > " + proof.path)
    return [DefaultInfo(files = depset([out, proof]))]

consumer = rule(implementation = _consumer, attrs = {"one": attr.label(cfg = named), "many": attr.label_list(cfg = listed), "source": attr.label(cfg = named, allow_files = True), "empty": attr.label(cfg = empty), "patch": attr.label(cfg = patch), "unset": attr.label(cfg = named), "aspected": attr.label_list(cfg = named, aspects = [probe_aspect]), "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")})
