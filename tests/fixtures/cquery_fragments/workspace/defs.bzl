def _impl(ctx):
    return [DefaultInfo()]

def frag_rule(fragments):
    return rule(implementation = _impl, fragments = fragments, attrs = {"deps": attr.label_list()})

frag_none = frag_rule([])
frag_cpp = frag_rule(["cpp"])
frag_java = frag_rule(["java"])
frag_py = frag_rule(["py"])
frag_platform = frag_rule(["platform"])
frag_coverage = frag_rule(["coverage"])
frag_proto = frag_rule(["proto"])
frag_apple = frag_rule(["apple"])
frag_objc = frag_rule(["objc"])
frag_j2objc = frag_rule(["j2objc"])
frag_android = frag_rule(["android"])
frag_bogus = frag_rule(["no_such_fragment"])
frag_cpp_java = frag_rule(["java", "cpp"])

def _var_impl(ctx):
    _ = ctx.var.get("FOO")
    return [DefaultInfo()]

reads_var = rule(implementation = _var_impl)

def _flag_impl(ctx):
    return []

string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _set_impl(settings, attr):
    return {"//:flag": str(settings["//command_line_option:compilation_mode"])}

set_flag = transition(implementation = _set_impl, inputs = ["//command_line_option:compilation_mode"], outputs = ["//:flag"])

def _cm_impl(settings, attr):
    return {"//command_line_option:copt": ["-O1"]}

set_copt = transition(implementation = _cm_impl, inputs = [], outputs = ["//command_line_option:copt"])

with_rule_transition = rule(
    implementation = _impl,
    cfg = set_copt,
    attrs = {
        "deps": attr.label_list(),
        "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist"),
    },
)

with_attr_transition = rule(
    implementation = _impl,
    attrs = {
        "dep": attr.label(cfg = set_flag),
        "tool": attr.label(cfg = "exec"),
        "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist"),
    },
)

frag_bazel_py = frag_rule(["bazel_py"])
frag_bazel_android = frag_rule(["bazel_android"])

def _aspect_impl(target, ctx):
    return []

java_aspect = aspect(implementation = _aspect_impl, fragments = ["java"])

with_aspect = rule(
    implementation = _impl,
    attrs = {"dep": attr.label(aspects = [java_aspect])},
)
