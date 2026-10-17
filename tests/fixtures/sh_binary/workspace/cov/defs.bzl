def _impl(ctx):
    m = ctx.attr.mode
    if m == "ok":
        info = coverage_common.instrumented_files_info(ctx, source_attributes = ["srcs"], dependency_attributes = ["deps"], extensions = ["sh"], metadata_files = [], coverage_support_files = depset(), coverage_environment = {}, reported_to_actual_sources = None, baseline_coverage_files = None)
        out = ctx.actions.declare_file(ctx.label.name + ".txt")
        ctx.actions.write(out, "%s\n%s\n%s\n%s\n%s\n" % (type(info), info, info.instrumented_files, info.metadata_files, info == coverage_common.instrumented_files_info(ctx)))
        return [DefaultInfo(files = depset([out])), info]
    if m == "ctx":
        coverage_common.instrumented_files_info(None)
    if m == "source":
        coverage_common.instrumented_files_info(ctx, source_attributes = [1])
    if m == "env":
        coverage_common.instrumented_files_info(ctx, coverage_environment = {"K": "V"})
    if m == "support":
        coverage_common.instrumented_files_info(ctx, coverage_support_files = [1])
    if m == "seq":
        coverage_common.instrumented_files_info(ctx, dependency_attributes = "deps")
    return []

cov = rule(_impl, attrs = {"mode": attr.string(), "srcs": attr.label_list(allow_files = True), "deps": attr.label_list()})
