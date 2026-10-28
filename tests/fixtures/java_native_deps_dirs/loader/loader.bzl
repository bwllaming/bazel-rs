load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_cc//cc/private/link:create_library_to_link.bzl", "make_library_to_link")
load("@rules_java//java/private:java_common_internal.bzl", "collect_native_deps_dirs")

def _library(ctx):
    link = ctx.actions.declare_file("links/libfixture_native.so")
    ctx.actions.symlink(output = link, target_file = ctx.file.actual)
    lib = make_library_to_link(dynamic_library = link, resolved_symlink_dynamic_library = ctx.file.actual, _library_identifier = "fixture")
    linker_input = cc_common.create_linker_input(owner = ctx.label, libraries = depset([lib]))
    linking = cc_common.create_linking_context(linker_inputs = depset([linker_input]))
    return [CcInfo(linking_context = linking), DefaultInfo(files = depset([link]), runfiles = ctx.runfiles(files = [link]))]

library = rule(implementation = _library, attrs = {"actual": attr.label(allow_single_file = True)})

def _loader(ctx):
    linking = ctx.attr.library[CcInfo].linking_context
    libs = depset(transitive = [depset(i.libraries) for i in linking.linker_inputs.to_list()])
    dirs = collect_native_deps_dirs(libs)
    files = [lib.dynamic_library for lib in libs.to_list()]
    report = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(report, json.encode({
        "dirs": dirs,
        "dynamic": [lib.dynamic_library.short_path for lib in libs.to_list()],
        "resolved": [lib.resolved_symlink_dynamic_library.short_path for lib in libs.to_list()],
    }))
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    library_path = ":".join(["${rf}/" + ctx.workspace_name + "/" + d for d in dirs])
    script = '#!/bin/sh\nset -eu\nrf="${RUNFILES_DIR:-$0.runfiles}"\nexec "%s" "-Djava.library.path=%s" -cp "${rf}/%s/%s" jni.NativeProbe\n' % (ctx.attr.java, library_path, ctx.workspace_name, ctx.file.jar.short_path)
    ctx.actions.write(executable, script, is_executable = True)
    return [DefaultInfo(executable = executable, files = depset([executable, report]), runfiles = ctx.runfiles(files = files + [ctx.file.jar]))]

loader = rule(implementation = _loader, executable = True, attrs = {
    "library": attr.label(providers = [CcInfo]),
    "jar": attr.label(allow_single_file = True),
    "java": attr.string(mandatory = True),
})
