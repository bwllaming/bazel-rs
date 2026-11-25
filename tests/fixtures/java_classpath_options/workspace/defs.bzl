def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [
        "bin=" + ctx.bin_dir.path,
        "lint=" + str(ctx.fragments.java.run_android_lint),
        "flags=" + ",".join(ctx.attr.flags),
    ]
    tool = ctx.attr.tool
    if tool:
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
    },
)

# --experimental_* options: only analysis tests (and builtins) may
# transition on them.
def _show_test_impl(ctx):
    return [AnalysisTestResultInfo(success = False, message = "\n".join(ctx.attr.target[0][Lines].lines))]

show_test = rule(
    _show_test_impl,
    analysis_test = True,
    attrs = {"target": attr.label(cfg = analysis_test_transition(settings = {
        "//command_line_option:experimental_java_classpath": "javabuilder",
        "//command_line_option:experimental_run_android_lint_on_java_rules": True,
    }))},
)
