# B31: rules_android's top-level rules use the feature flag transition at
# load time (android_binary/rule.bzl:93).
_t = config_common.config_feature_flag_transition("feature_flags")
_same = config_common.config_feature_flag_transition("feature_flags")
_other = config_common.config_feature_flag_transition("other")

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [
        type(_t),
        str(_t),
        repr(_t == _same),
        repr(_t == _other),
        repr(dir(_t)),
        str(ctx.attr._whitelist_config_feature_flag_setter.label),
        str(ctx.attr._whitelist_config_feature_flag.label) if ctx.attr._whitelist_config_feature_flag else "None",
        repr(ctx.attr.feature_flags),
        ctx.bin_dir.path,
    ]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

flagged = rule(
    implementation = _impl,
    cfg = _t,
    attrs = {"feature_flags": attr.label_keyed_string_dict()},
)

def _dep_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, " ".join([f.short_path for f in ctx.files.deps]) + "\n")
    return [DefaultInfo(files = depset([out]))]

uses = rule(implementation = _dep_impl, attrs = {"deps": attr.label_list()})
