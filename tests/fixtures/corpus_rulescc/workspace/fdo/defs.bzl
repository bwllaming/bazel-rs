# B32: the CppOptions rules_cc's own tests transition on (FDO, Propeller,
# memprof, custom malloc, libc, C++20 modules, the ThinLTO option lists),
# the CppConfiguration fields late-bound `configuration_field` defaults read
# (StarlarkLateBoundDefault), output roots and select().
_OPTS = [
    "//command_line_option:fdo_instrument",
    "//command_line_option:fdo_optimize",
    "//command_line_option:cs_fdo_instrument",
    "//command_line_option:cs_fdo_absolute_path",
    "//command_line_option:xbinary_fdo",
    "//command_line_option:fdo_prefetch_hints",
    "//command_line_option:fdo_profile",
    "//command_line_option:cs_fdo_profile",
    "//command_line_option:propeller_optimize_absolute_cc_profile",
    "//command_line_option:propeller_optimize_absolute_ld_profile",
    "//command_line_option:propeller_optimize",
    "//command_line_option:memprof_profile",
    "//command_line_option:custom_malloc",
    "//command_line_option:grte_top",
    "//command_line_option:host_grte_top",
    "//command_line_option:ltobackendopt",
    "//command_line_option:ltoindexopt",
    "//command_line_option:per_file_ltobackendopt",
]

_LISTS = ["ltobackendopt", "ltoindexopt", "per_file_ltobackendopt"]

def _value(name, values):
    if name in _LISTS:
        return values
    value = values[0]
    if value == "None":
        return None
    if value == "True":
        return True
    if value.startswith("L:"):
        return Label(value[2:])
    return value

def _set_impl(settings, attr):
    result = {}
    for option in _OPTS:
        name = option.split(":")[1]
        if name in attr.settings:
            result[option] = _value(name, attr.settings[name])
        else:
            result[option] = settings[option]
    return result

_set = transition(implementation = _set_impl, inputs = _OPTS, outputs = _OPTS)

def _read_impl(settings, attr):
    print("READ", [settings[option] for option in _OPTS])
    return {}

_read = transition(implementation = _read_impl, inputs = _OPTS, outputs = [])

_LATE = {
    "_malloc": "custom_malloc",
    "_fdo_optimize": "fdo_optimize",
    "_fdo_profile": "fdo_profile",
    "_xfdo": "xbinary_fdo",
    "_csfdo": "cs_fdo_profile",
    "_prefetch": "fdo_prefetch_hints",
    "_propeller": "propeller_optimize",
    "_memprof": "memprof_profile",
    "_libc": "libc_top",
}

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [out.path, str(ctx.attr.flags)]
    for attribute in sorted(_LATE):
        dep = getattr(ctx.attr, attribute)
        lines.append("%s=%s" % (_LATE[attribute], dep.label if dep else None))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

_leaf_attrs = {"flags": attr.string_list()}
_leaf_attrs.update({
    attribute: attr.label(default = configuration_field(fragment = "cpp", name = field))
    for attribute, field in _LATE.items()
})

leaf = rule(implementation = _leaf_impl, attrs = _leaf_attrs)

# The leaf in its own configuration, the options read back.
read_leaf = rule(implementation = _leaf_impl, attrs = _leaf_attrs, cfg = _read)

def _top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

top = rule(implementation = _top_impl, attrs = {
    "dep": attr.label(cfg = _set),
    "settings": attr.string_list_dict(),
})

def _exec_user_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.tool))]

exec_user = rule(implementation = _exec_user_impl, attrs = {
    "tool": attr.label(cfg = "exec"),
})

# CoreOptions.actionEnvironment (`--action_env`, List<EnvVar>) written and
# read by a transition: the output root and the default shell environment.
def _env_impl(settings, attr):
    print("READ_ENV", settings["//command_line_option:action_env"])
    return {"//command_line_option:action_env": attr.env}

_env = transition(
    implementation = _env_impl,
    inputs = ["//command_line_option:action_env"],
    outputs = ["//command_line_option:action_env"],
)

def _env_leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = 'echo "$PATH $X" > ' + out.path,
        use_default_shell_env = True,
    )
    return [DefaultInfo(files = depset([out]))]

env_leaf = rule(implementation = _env_leaf_impl)

def _env_top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

env_top = rule(implementation = _env_top_impl, attrs = {
    "dep": attr.label(cfg = _env),
    "env": attr.string_list(),
})
