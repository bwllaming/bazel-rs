"""Coverage fixture rules (tests/coverage_command.rs)."""

def _library(ctx):
    return [
        DefaultInfo(files = depset(ctx.files.srcs)),
        coverage_common.instrumented_files_info(
            ctx,
            source_attributes = ["srcs"],
            dependency_attributes = ["deps"],
            extensions = ["txt"],
        ),
    ]

cov_library = rule(
    implementation = _library,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
    },
)

def _test(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [
        DefaultInfo(executable = out),
        coverage_common.instrumented_files_info(ctx, dependency_attributes = ["deps"]),
    ]

cov_test = rule(
    implementation = _test,
    test = True,
    attrs = {
        "script": attr.string(),
        "deps": attr.label_list(),
        "_lcov_merger": attr.label(
            default = configuration_field(fragment = "coverage", name = "output_generator"),
            executable = True,
            cfg = "exec",
        ),
    },
)

def _plain_test(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\necho plain\n", is_executable = True)
    return [DefaultInfo(executable = out)]

plain_test = rule(implementation = _plain_test, test = True)
