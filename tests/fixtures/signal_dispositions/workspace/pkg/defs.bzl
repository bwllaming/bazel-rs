def _executable(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.symlink(output = out, target_file = ctx.file.src, is_executable = True)
    return [DefaultInfo(executable = out)]

probe_test = rule(implementation = _executable, test = True, attrs = {"src": attr.label(allow_single_file = True)})

probe_bin = rule(implementation = _executable, executable = True, attrs = {"src": attr.label(allow_single_file = True)})

def _probe_repo(ctx):
    result = ctx.execute(["grep", "SigIgn", "/proc/self/status"])
    ctx.file("status.txt", result.stdout)
    ctx.file("BUILD.bazel", 'exports_files(["status.txt"])\n')

probe_repo = repository_rule(implementation = _probe_repo)
