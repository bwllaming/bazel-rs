"""C++ compile actions are not shareable (CppCompileActionBuilder.java:99)."""

load("@rules_cc//cc:find_cc_toolchain.bzl", "find_cc_toolchain", "use_cc_toolchain")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")

def _compile(ctx, name):
    cc_toolchain = find_cc_toolchain(ctx)
    feature_configuration = cc_common.configure_features(
        ctx = ctx,
        cc_toolchain = cc_toolchain,
        requested_features = ctx.features,
        unsupported_features = ctx.disabled_features,
    )
    _, outputs = cc_common.compile(
        name = name,
        actions = ctx.actions,
        feature_configuration = feature_configuration,
        cc_toolchain = cc_toolchain,
        srcs = ctx.files.srcs,
    )
    return outputs.objects + outputs.pic_objects

def _compile_shared_impl(ctx):
    # One compilation name for every target: two targets of the package
    # compiling one source declare the same object.
    objects = _compile(ctx, "shared_compile")
    if ctx.attr.twice:
        # The same compilation again in this target: an identical
        # CppCompileAction of one owner.
        objects = objects + _compile(ctx, "shared_compile")
    return [DefaultInfo(files = depset(objects))]

compile_shared = rule(
    implementation = _compile_shared_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = [".c"]),
        "twice": attr.bool(),
    },
    fragments = ["cpp"],
    toolchains = use_cc_toolchain(),
)
