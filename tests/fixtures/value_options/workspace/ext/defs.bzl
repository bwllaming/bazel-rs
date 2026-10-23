load(":native_bool_flag.bzl", "BuildSettingInfo")

def _flag_probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [
        "%s=%s" % (flag.label.name, flag[BuildSettingInfo].value)
        for flag in ctx.attr.flags
    ]
    files = [out]
    if ctx.file.tool:
        lines.append("tool=" + ctx.file.tool.path)
        files.append(ctx.file.tool)
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset(files))]

# Writes each flag's value, like protobuf's proto_library reads its
# native_bool_flag attributes (bazel/private/proto_library_rule.bzl:180-226).
flag_probe = rule(
    implementation = _flag_probe_impl,
    attrs = {
        "flags": attr.label_list(providers = [BuildSettingInfo]),
        "tool": attr.label(cfg = "exec", allow_single_file = True),
    },
)
