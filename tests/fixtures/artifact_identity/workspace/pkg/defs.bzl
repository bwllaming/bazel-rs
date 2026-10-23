# W1b (PLAN-L1 §6): every File resolves through the build's one artifact
# index, as Bazel resolves an action input, a DefaultInfo file or a runfile
# through its Artifact - however the File reached the consumer.
Info = provider(fields = ["f"])
Holder = provider(fields = ["inner"])
Box = provider(fields = ["get", "payload"])

def _leaf(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "leaf:" + ctx.label.name + "\n")
    return [Info(f = out)]

leaf = rule(_leaf)

def _hold(ctx):
    return [Holder(inner = ctx.attr.dep[Info])]

hold = rule(_hold, attrs = {"dep": attr.label()})

def _forward(ctx):
    return [ctx.attr.dep[Holder]]

forward = rule(_forward, attrs = {"dep": attr.label()})

def _use(ctx):
    # The leaf's File, three hops away (use -> forward -> hold -> leaf).
    f = ctx.attr.dep[Holder].inner.f
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(inputs = [f], outputs = [out], command = "cat $1 > $2", arguments = [f.path, out.path])
    return [DefaultInfo(files = depset([out]))]

use = rule(_use, attrs = {"dep": attr.label()})

def _payload(kind):
    # Provider payload values the pre-W1b provider-file walk rejected.
    if kind == "range":
        return range(3)
    if kind == "deep":
        deep = []
        for _ in range(200):
            deep = [deep]
        return deep
    if kind == "cyclic":
        cyclic = []
        cyclic.append(cyclic)
        return cyclic
    if kind == "method":
        return "payload".startswith
    return None

def _closure(ctx):
    hidden = ctx.actions.declare_file(ctx.label.name + ".hidden")
    ctx.actions.write(hidden, "hidden:" + ctx.label.name + "\n")
    return [Box(get = lambda: hidden, payload = _payload(ctx.attr.kind))]

closure = rule(_closure, attrs = {"kind": attr.string()})

def _bound(ctx):
    # Values bound to this analysis: Bazel keeps them in the provider; the
    # native profile freezes providers with the analysis and rejects them.
    if ctx.attr.kind == "closure":
        return [Box(get = lambda: ctx, payload = None)]
    payload = {"ctx": ctx, "actions": ctx.actions, "args": ctx.actions.args()}[ctx.attr.kind]
    return [Box(get = None, payload = [payload])]

bound = rule(_bound, attrs = {"kind": attr.string()})

def _via_closure(ctx):
    # A File reachable only through a closure is an input, and a sibling.
    f = ctx.attr.dep[Box].get()
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(inputs = [f], outputs = [out], command = "cat $1 > $2", arguments = [f.path, out.path])
    files = [out]
    if ctx.attr.sibling:
        sibling = ctx.actions.declare_file(ctx.label.name + ".sibling", sibling = f)
        ctx.actions.write(sibling, "sibling of " + f.basename + "\n")
        files.append(sibling)
    return [DefaultInfo(files = depset(files))]

via_closure = rule(_via_closure, attrs = {"dep": attr.label(), "sibling": attr.bool()})

def _gen(ctx):
    out = ctx.actions.declare_file(ctx.label.name + "_data.txt")
    ctx.actions.write(out, "generated data\n")
    return [DefaultInfo(files = depset([out]))]

gen = rule(_gen)

def _fwd(ctx):
    # The dependency's DefaultInfo, forwarded as is.
    return [ctx.attr.dep[DefaultInfo]]

fwd = rule(_fwd, attrs = {"dep": attr.label()})

def _cat(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = ctx.files.deps,
        outputs = [out],
        command = "cat \"$@\" > " + out.path,
        arguments = [f.path for f in ctx.files.deps],
    )
    return [DefaultInfo(files = depset([out]))]

cat = rule(_cat, attrs = {"deps": attr.label_list(allow_files = True)})

def _status(ctx):
    # A workspace status file to build, consumed by no action.
    return [DefaultInfo(files = depset([ctx.version_file]))]

status = rule(_status)

def _decl(ctx):
    out = ctx.actions.declare_file("same.txt")
    ctx.actions.write(out, ctx.label.name)
    return [DefaultInfo(files = depset([out]))]

decl = rule(_decl, attrs = {"dep": attr.label()})

def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\ncat $0.runfiles/_main/pkg/gen_data.txt > $1\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(_tool, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _run_tool(ctx):
    # The tool's generated runfile is staged into the spawn from its producer.
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(executable = ctx.executable.tool, arguments = [out.path], outputs = [out])
    return [DefaultInfo(files = depset([out]))]

run_tool = rule(_run_tool, attrs = {"tool": attr.label(executable = True, cfg = "target")})

# The workspace status files outside any action input or DefaultInfo: they
# resolve wherever a File can reach, whichever other targets the build has.
def _status_exe(ctx):
    # An executable whose own runfiles hold a status file.
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho " + ctx.label.name + "\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = [ctx.version_file]))]

status_exe = rule(_status_exe, executable = True)

def _status_lib(ctx):
    # A library whose runfiles hold a status file.
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "lib:" + ctx.label.name + "\n")
    return [DefaultInfo(files = depset([out]), runfiles = ctx.runfiles(files = [ctx.info_file]))]

status_lib = rule(_status_lib)

def _status_wrap(ctx):
    # An executable merging its dependencies' runfiles.
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho " + ctx.label.name + "\n", is_executable = True)
    runfiles = ctx.runfiles()
    for dep in ctx.attr.deps:
        runfiles = runfiles.merge(dep[DefaultInfo].default_runfiles)
    return [DefaultInfo(executable = exe, runfiles = runfiles)]

status_wrap = rule(_status_wrap, executable = True, attrs = {"deps": attr.label_list()})

def _status_group(ctx):
    # A status file in an output group only.
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "group:" + ctx.label.name + "\n")
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(status = depset([ctx.info_file]))]

status_group = rule(_status_group)

def _status_aspect(target, ctx):
    # A status file in a top-level aspect's output group.
    return [OutputGroupInfo(aspect_status = depset([ctx.version_file]))]

status_aspect = aspect(_status_aspect)

def _use_manifest(ctx):
    # A tool's runfiles manifest as an action input: the File of the
    # planned manifest node, None without runfile manifests (as in Bazel).
    manifest = ctx.attr.tool[DefaultInfo].files_to_run.runfiles_manifest
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(inputs = [manifest], outputs = [out], command = "cp $1 $2", arguments = [manifest.path, out.path])
    return [DefaultInfo(files = depset([out]))]

use_manifest = rule(_use_manifest, attrs = {"tool": attr.label(executable = True, cfg = "target")})

# A dependency's workspace status files are the consumer's own ctx.info_file
# and ctx.version_file: one File each (short_path `stable-status.txt`, a
# derived File of the `bazel-out` root, no owner), however the consumer
# reaches them - ctx.files, the dependency's DefaultInfo or its Target.files,
# from a Starlark rule or a filegroup forwarding it - under build and
# --nobuild alike.
def _status_files(ctx):
    # Both status files as files to build.
    return [DefaultInfo(files = depset([ctx.info_file, ctx.version_file]))]

status_files = rule(_status_files)

def _describe(how, f, own):
    return " ".join([
        how,
        "path=" + f.path,
        "short_path=" + f.short_path,
        "is_source=" + str(f.is_source),
        "root=" + f.root.path,
        "owner=" + str(f.owner),
        "equal_info_file=" + str(f == own[0]),
        "equal_version_file=" + str(f == own[1]),
        "in_own_set=" + str(len(depset([f] + own).to_list()) == 2),
    ])

def _status_report(ctx):
    # The dependencies' status files as this target sees them, against its
    # own: printed (what --nobuild shows) and written by an action taking
    # every one of them as an input.
    own = [ctx.info_file, ctx.version_file]
    views = []
    for name, dep, files in [("dep", ctx.attr.dep, ctx.files.dep), ("group", ctx.attr.group, ctx.files.group)]:
        views.append(("ctx.files." + name, files))
        views.append((name + "[DefaultInfo]", dep[DefaultInfo].files.to_list()))
        views.append((name + ".files", dep.files.to_list()))
    reached = [f for _, files in views for f in files]
    lines = [_describe("own", f, own) for f in own]
    lines += [_describe(how, f, own) for how, files in views for f in files]
    lines.append("set_size=%d" % len(depset(own + reached).to_list()))
    for line in lines:
        print("status_report " + line)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = own + reached,
        outputs = [out],
        command = "printf '%s\\n' \"$1\" > \"$2\"",
        arguments = ["\n".join(lines), out.path],
    )
    return [DefaultInfo(files = depset([out]))]

status_report = rule(_status_report, attrs = {"dep": attr.label(), "group": attr.label()})
