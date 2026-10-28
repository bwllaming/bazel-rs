load("@rules_java//java/private:java_info.bzl", "JavaInfo")
load("@rules_java//java/private:java_common.bzl", "JavaRuntimeClasspathInfo")
load("@rules_java//java/private:java_common_internal.bzl", "get_runtime_classpath_for_archive")
load("@rules_java//java/common/rules/impl:java_binary_deploy_jar.bzl", "create_deploy_archive")

def _prebuilt(ctx):
    jar = ctx.actions.declare_file(ctx.label.name + ".jar")
    if ctx.attr.poison:
        ctx.actions.run_shell(outputs = [jar], command = "echo EXCLUDED_JAR_PRODUCER_RAN >&2; exit 90")
    else:
        ctx.actions.run_shell(inputs = [ctx.file.jar], outputs = [jar], command = '/bin/cp "$1" "$2"', arguments = [ctx.file.jar.path, jar.path])
    info = JavaInfo(output_jar = jar, compile_jar = jar, neverlink = ctx.attr.neverlink)
    return [DefaultInfo(files = depset([jar])), info, JavaRuntimeClasspathInfo(runtime_classpath = info.transitive_runtime_jars)]

prebuilt = rule(implementation = _prebuilt, attrs = {
    "jar": attr.label(allow_single_file = True),
    "neverlink": attr.bool(),
    "poison": attr.bool(),
})

def _archive(ctx):
    runtime = depset(order = "preorder", transitive = [dep[JavaInfo].transitive_runtime_jars for dep in ctx.attr.deps])
    deploy_env = depset(transitive = [dep[JavaRuntimeClasspathInfo].runtime_classpath for dep in ctx.attr.deploy_env])
    selected = get_runtime_classpath_for_archive(runtime, deploy_env)
    jars = depset([ctx.file.main], transitive = [selected], order = "preorder")
    out = ctx.actions.declare_file(ctx.label.name + ".jar")
    report = ctx.actions.declare_file(ctx.label.name + ".classpath")
    ctx.actions.write(report, json.encode({
        "runtime": [f.path for f in runtime.to_list()],
        "deploy_env": [f.path for f in deploy_env.to_list()],
        "selected": [f.path for f in selected.to_list()],
        "archive": [f.path for f in jars.to_list()],
    }))
    # This is the actual rules_java java_binary deploy-archive implementation.
    create_deploy_archive(ctx, None, "cpfixture.Main", None, depset(), depset(), jars,
                          ["Classpath-Fixture: filtered"], [], str(ctx.label), out)
    return [DefaultInfo(files = depset([out, report]))]

archive = rule(implementation = _archive, attrs = {
    "main": attr.label(allow_single_file = True),
    "deps": attr.label_list(providers = [JavaInfo]),
    "deploy_env": attr.label_list(providers = [JavaRuntimeClasspathInfo]),
}, toolchains = ["@bazel_tools//tools/jdk:toolchain_type"])
