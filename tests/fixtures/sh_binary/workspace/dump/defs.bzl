def _sp(f):
    return f.short_path if f else "None"

def _dump_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        info = dep[DefaultInfo]
        lines.append("target %s" % dep.label)
        lines.append("  files " + " ".join([_sp(f) for f in info.files.to_list()]))
        lines.append("  executable " + _sp(info.files_to_run.executable))
        for name, rf in [("default_runfiles", info.default_runfiles), ("data_runfiles", info.data_runfiles)]:
            lines.append("  %s.files %s" % (name, " ".join([_sp(f) for f in rf.files.to_list()])))
            lines.append("  %s.symlinks %s" % (name, " ".join([s.path + "=" + _sp(s.target_file) for s in rf.symlinks.to_list()])))
            lines.append("  %s.root_symlinks %s" % (name, " ".join([s.path + "=" + _sp(s.target_file) for s in rf.root_symlinks.to_list()])))
            lines.append("  %s.empty_filenames %s" % (name, " ".join(rf.empty_filenames.to_list())))
        if RunEnvironmentInfo in dep:
            lines.append("  env %s inherit %s" % (dep[RunEnvironmentInfo].environment, dep[RunEnvironmentInfo].inherited_environment))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

dump = rule(_dump_impl, attrs = {"deps": attr.label_list()})
