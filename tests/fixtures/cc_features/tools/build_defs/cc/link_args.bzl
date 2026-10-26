"""Direct nullable get_link_args probe; no host compiler/toolchain required."""

load("@rules_cc//cc:cc_toolchain_config_lib.bzl", "action_config", "feature", "flag_group", "flag_set", "tool")
load("@rules_cc//cc/private:cc_internal.bzl", "cc_internal")
load(":probe.bzl", "LINK", "NO_LEGACY", "configure", "toolchain", "variables")

_RECORD = """
import json, pathlib, sys
out, *args = sys.argv[1:]
params = {}
for arg in args:
    if arg.startswith('--params=100%:@') and pathlib.Path(arg.split('@', 1)[1]).is_file():
        params[arg.replace(out, '<OUTPUT>')] = pathlib.Path(arg.split('@', 1)[1]).read_text()
pathlib.Path(out).write_text(json.dumps({'argv': [a.replace(out, '<OUTPUT>') for a in args], 'params': params}, sort_keys=True) + '\\n')
"""

def _impl(ctx):
    groups = [flag_group(flags = ["plain", "space here", "single'quote", "double\"quote", "back\\slash", "100%%", ""])]
    values = {}
    if ctx.attr.placeholder in ["variable", "custom"]:
        values["linker_param_file"] = "LINKER_PARAM_FILE_PLACEHOLDER" if ctx.attr.placeholder == "variable" else "CUSTOM_PARAM"
        groups.append(flag_group(flags = ["--params=100%%:@%{linker_param_file}"]))
    elif ctx.attr.placeholder == "literal":
        # A literal placeholder is NOT filtered when the variable is absent.
        groups.append(flag_group(flags = ["--params=100%%:@LINKER_PARAM_FILE_PLACEHOLDER"]))
    tc = toolchain(ctx, [NO_LEGACY, feature(name = "flags", flag_sets = [flag_set(actions = [LINK], flag_groups = groups)])], [action_config(action_name = LINK, tools = [tool(path = "/usr/bin/unused")])])
    kwargs = {
        "action_name": LINK,
        "feature_configuration": configure(tc, [LINK, "flags"]),
        "build_variables": variables(values),
    }
    if ctx.attr.parameter_type != "MISSING":
        kwargs["parameter_file_type"] = {"NONE": None, "INT": 1, "BOOL": True, "LIST": []}.get(ctx.attr.parameter_type, ctx.attr.parameter_type)
    args = cc_internal.get_link_args(**kwargs)
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.run(
        executable = "/usr/bin/python3",
        arguments = ["-c", _RECORD, out.path, args],
        outputs = [out],
        mnemonic = "LinkArgsProbe",
    )
    return [DefaultInfo(files = depset([out]))]

cc_link_args_probe = rule(
    implementation = _impl,
    attrs = {
        "parameter_type": attr.string(mandatory = True),
        "placeholder": attr.string(default = "variable"),
    },
)
