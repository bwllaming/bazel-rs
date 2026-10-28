load("@rules_java//java/private:java_common_internal.bzl", "compile")
load("@rules_java//java/private:java_info.bzl", "JavaInfo")

def _compile_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".jar")
    info = compile(
        ctx,
        output = output,
        java_toolchain = ctx.toolchains["@bazel_tools//tools/jdk:toolchain_type"].java,
        source_files = ctx.files.srcs,
        source_jars = ctx.files.source_jars,
        deps = [dep[JavaInfo] for dep in ctx.attr.deps],
        strict_deps = ctx.attr.strict,
        resources = ctx.files.resources,
    )
    return [DefaultInfo(files = depset([output])), info]

compile_fixture = rule(
    implementation = _compile_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "source_jars": attr.label_list(allow_files = True),
        "deps": attr.label_list(providers = [JavaInfo]),
        "strict": attr.string(default = "ERROR"),
        "resources": attr.label_list(allow_files = True),
    },
    toolchains = ["@bazel_tools//tools/jdk:toolchain_type"],
    fragments = ["java"],
)
