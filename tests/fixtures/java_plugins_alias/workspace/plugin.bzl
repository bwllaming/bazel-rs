load("@rules_java//java/private:java_info.bzl", "JavaInfo", "JavaPluginInfo")

def _plugin(ctx):
    jar = ctx.actions.declare_file(ctx.label.name + ".jar")
    data = ctx.actions.declare_file(ctx.label.name + ".data")
    ctx.actions.write(jar, ctx.attr.processor)
    ctx.actions.write(data, ctx.attr.processor + " data")
    return [
        DefaultInfo(files = depset([jar, data]), runfiles = ctx.runfiles(files = [data])),
        JavaPluginInfo(
            runtime_deps = [JavaInfo(output_jar = jar, compile_jar = jar, deps = [struct()] if ctx.attr.bad else [])],
            processor_class = ctx.attr.processor,
            processor_classes = ["Common", ctx.attr.processor],
            data = depset([data, ctx.file.input]),
            generates_api = ctx.attr.api,
        ),
    ]

plugin = rule(implementation = _plugin, attrs = {
    "processor": attr.string(),
    "api": attr.bool(),
    "bad": attr.bool(),
    "input": attr.label(default = "//:input.txt", allow_single_file = True),
})

def _data(p):
    return {
        "classes": p.processor_classes.to_list(),
        "jars": [f.short_path for f in p.processor_jars.to_list()],
        "data": [f.short_path for f in p.processor_data.to_list()],
    }

def _probe(ctx):
    dep = ctx.attr.dep or ctx.attr.exec_dep
    p = dep[JavaPluginInfo]
    d = dep[DefaultInfo]
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode({
        "plugins": _data(p.plugins),
        "api": _data(p.api_generating_plugins),
        "outputs": len(p.java_outputs),
        "files": [f.short_path for f in d.files.to_list()],
        "default_runfiles": [f.short_path for f in d.default_runfiles.files.to_list()],
        "data_runfiles": [f.short_path for f in d.data_runfiles.files.to_list()],
    }))
    return [DefaultInfo(files = depset([out]))]

probe = rule(implementation = _probe, attrs = {
    "dep": attr.label(providers = [JavaPluginInfo]),
    "exec_dep": attr.label(providers = [JavaPluginInfo], cfg = "exec"),
})
