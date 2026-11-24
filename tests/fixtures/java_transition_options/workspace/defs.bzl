_OPTIONS = [
    "//command_line_option:java_header_compilation",
    "//command_line_option:use_ijars",
    "//command_line_option:java_deps",
    "//command_line_option:one_version_enforcement_on_java_tests",
    "//command_line_option:java_launcher",
]

def _java_impl(settings, attr):
    return {
        "//command_line_option:java_header_compilation": False,
        "//command_line_option:use_ijars": False,
        "//command_line_option:java_deps": False,
        "//command_line_option:one_version_enforcement_on_java_tests": False,
        "//command_line_option:java_launcher": Label("//:launcher"),
    }

java_options = transition(implementation = _java_impl, inputs = [], outputs = _OPTIONS)

def _probe_impl(ctx):
    java = ctx.fragments.java
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [
        "bin=" + ctx.bin_dir.path,
        "one_version=" + java.one_version_enforcement_level,
        "on_tests=" + str(java.one_version_enforcement_on_java_tests),
        "strict=" + java.strict_java_deps,
        "launcher=" + (str(ctx.attr._launcher.label) if ctx.attr._launcher else "None"),
        "flags=" + ",".join(ctx.attr.flags),
    ]
    tool = ctx.attr.tool
    if tool:
        lines.append("tool=" + tool[DefaultInfo].files.to_list()[0].path)
        lines.extend(["tool." + line for line in tool[Lines].lines])
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out])), Lines(lines = lines)]

Lines = provider(fields = ["lines"])

probe = rule(
    _probe_impl,
    fragments = ["java"],
    attrs = {
        "flags": attr.string_list(),
        "tool": attr.label(cfg = "exec"),
        "_launcher": attr.label(default = configuration_field(fragment = "java", name = "launcher")),
    },
)

def _set_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.probes))]

set_java_options = rule(_set_impl, attrs = {"probes": attr.label_list(cfg = java_options)})

# The experimental_*/incompatible_* options only analysis tests (and
# builtins) may transition on.
def _show_test_impl(ctx):
    return [AnalysisTestResultInfo(success = False, message = "\n".join(ctx.attr.target[0][Lines].lines))]

show_test = rule(
    _show_test_impl,
    analysis_test = True,
    attrs = {"target": attr.label(cfg = analysis_test_transition(settings = {
        "//command_line_option:experimental_one_version_enforcement": "WARNING",
        "//command_line_option:incompatible_disallow_java_import_exports": True,
        "//command_line_option:experimental_strict_java_deps": "warn",
        "//command_line_option:one_version_enforcement_on_java_tests": False,
    }))},
)
