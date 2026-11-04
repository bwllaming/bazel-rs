def _talk_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo out-line; echo err-line >&2; touch " + out.path,
        mnemonic = "Talk",
        progress_message = "Talking %{label}",
    )
    return [DefaultInfo(files = depset([out]))]

talk = rule(implementation = _talk_impl)

def _nomsg_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo only-stderr >&2; touch " + out.path,
        mnemonic = "NoMsg",
    )
    return [DefaultInfo(files = depset([out]))]

nomsg = rule(implementation = _nomsg_impl)

def _toolrule_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo exec-config-talk >&2; touch " + out.path,
        mnemonic = "ToolTalk",
        progress_message = "Tooling %{output} for %{label}",
    )
    return [DefaultInfo(files = depset([out]), executable = out)]

toolrule = rule(implementation = _toolrule_impl)

def _usetool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs = [out], inputs = ctx.files._tool, command = "touch " + out.path)
    return [DefaultInfo(files = depset([out]))]

usetool = rule(implementation = _usetool_impl, attrs = {"_tool": attr.label(default = "//tool:tr", cfg = "exec", allow_files = True)})

def _failrule_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(outputs = [out], executable = "/bin/sh", arguments = ["-c", "echo starlark-fail >&2; exit 42", "x"], mnemonic = "FailRun")
    return [DefaultInfo(files = depset([out]))]

failrule = rule(implementation = _failrule_impl)
