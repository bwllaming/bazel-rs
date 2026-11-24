def _exe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\necho hello\nexit %d\n" % ctx.attr.code, is_executable = True)
    return [DefaultInfo(executable = out)]

exe = rule(_exe_impl, executable = True, attrs = {"code": attr.int()})
exe_test = rule(_exe_impl, test = True, attrs = {"code": attr.int()})

def _mode_impl(settings, attr):
    return {"//command_line_option:compilation_mode": attr.mode}

mode_transition = transition(
    implementation = _mode_impl,
    inputs = [],
    outputs = ["//command_line_option:compilation_mode"],
)

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs = [out], command = "echo x > " + out.path)
    return [DefaultInfo(files = depset([out]))]

mode_gen = rule(_gen_impl, cfg = mode_transition, attrs = {"mode": attr.string()})

def _tree_impl(ctx):
    tree = ctx.actions.declare_directory(ctx.label.name + "_dir")
    ctx.actions.run_shell(outputs = [tree], command = "mkdir -p {0}/sub && echo t1 > {0}/t1.txt && echo t2 > {0}/sub/t2.txt".format(tree.path))
    link = ctx.actions.declare_file(ctx.label.name + "_link.txt")
    ctx.actions.symlink(output = link, target_file = ctx.file.src)
    return [DefaultInfo(files = depset([tree, link]))]

tree = rule(_tree_impl, attrs = {"src": attr.label(allow_single_file = True)})

def _bad_impl(ctx):
    fail("bad analysis")

bad = rule(_bad_impl)
