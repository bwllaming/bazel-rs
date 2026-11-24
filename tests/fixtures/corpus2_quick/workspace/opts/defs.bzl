Flags = provider(fields = ["text"])

def _flags(ctx):
    a = ctx.fragments.apple
    p = ctx.fragments.proto
    text = repr([a.macos_minimum_os_flag, p.experimental_protoc_opts, type(p.experimental_protoc_opts)])
    if ctx.attr.tool:
        text = text + " exec=" + ctx.attr.tool[Flags].text
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, text + "\n")
    return [DefaultInfo(files = depset([out])), Flags(text = text)]

flags = rule(
    implementation = _flags,
    attrs = {"tool": attr.label(cfg = "exec")},
    fragments = ["apple", "proto"],
)

def _env(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs = [out], command = "echo CC=${CC-unset} FOO=${FOO-unset} > " + out.path, use_default_shell_env = True)
    return [DefaultInfo(files = depset([out]))]

env = rule(implementation = _env)

def _tr_impl(settings, attr):
    return {"//command_line_option:protocopt": settings["//command_line_option:protocopt"] + ["--x"]}

tr = transition(implementation = _tr_impl, inputs = ["//command_line_option:protocopt"], outputs = ["//command_line_option:protocopt"])

def _tr_rule(ctx):
    return [DefaultInfo(files = ctx.attr.dep[0][DefaultInfo].files)]

tr_rule = rule(implementation = _tr_rule, attrs = {"dep": attr.label(cfg = tr)})
