load("@rules_java//java/private:java_info.bzl", "JavaPluginInfo")
load("@rules_java//java/private:native.bzl", "get_internal_java_common")

Headers = provider(fields = ["jar", "tjar", "jdeps", "classpath"])

def _header_impl(ctx):
    tc = ctx.toolchains["@bazel_tools//tools/jdk:toolchain_type"].java
    jar = ctx.actions.declare_file(ctx.label.name + "-hjar.jar")
    tjar = ctx.actions.declare_file(ctx.label.name + "-tjar.jar")
    jdeps = ctx.actions.declare_file(ctx.label.name + "-hjar.jdeps")
    deps = [d[Headers] for d in ctx.attr.deps]
    classpath = depset(order = "preorder", direct = [d.jar for d in deps], transitive = [d.classpath for d in deps])
    opts = tc._javacopts
    if ctx.attr.opts:
        opts = depset(ctx.attr.opts, transitive = [opts], order = "preorder")
    get_internal_java_common().create_header_compilation_action(
        ctx, tc, jar, jdeps, JavaPluginInfo(runtime_deps = []),
        depset(ctx.files.srcs), ctx.files.source_jars, classpath,
        depset([d.jar for d in deps]), None, depset([d.jdeps for d in deps]),
        opts, ctx.attr.strict_deps, ctx.label, "fixture_header", ctx.attr.direct,
        ctx.files.extra, tjar, depset([d.tjar for d in deps]),
    )
    return [DefaultInfo(files = depset([jar, tjar, jdeps])), Headers(jar = jar, tjar = tjar, jdeps = jdeps, classpath = classpath)]

header = rule(
    implementation = _header_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "source_jars": attr.label_list(allow_files = True),
        "deps": attr.label_list(providers = [Headers]),
        "extra": attr.label_list(allow_files = True),
        "opts": attr.string_list(),
        "strict_deps": attr.string(default = "ERROR"),
        "direct": attr.bool(default = True),
    },
    toolchains = ["@bazel_tools//tools/jdk:toolchain_type"],
    fragments = ["java"],
)
