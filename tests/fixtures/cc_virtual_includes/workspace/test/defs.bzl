"""Probes of ctx.actions.declare_shareable_artifact.

`test` is a main-repository entry of Bazel's internal Starlark API allowlist
(BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST, allowlistEntry("",
"test")), so rules defined in this package may call it.
"""

def _describe(name, f):
    return [
        name + ".path=" + f.path,
        name + ".short_path=" + f.short_path,
        name + ".root=" + f.root.path,
        name + ".owner=" + str(f.owner),
        name + ".basename=" + f.basename,
        name + ".dirname=" + f.dirname,
        name + ".extension=" + f.extension,
        name + ".is_source=" + str(f.is_source),
        name + ".is_directory=" + str(f.is_directory),
        name + ".repr=" + repr(f),
    ]

def _probe_impl(ctx):
    mode = ctx.attr.mode
    path = ctx.attr.path
    actions = ctx.actions
    lines = []
    files = []
    if mode == "default":
        a = actions.declare_shareable_artifact(path)
        actions.write(a, ctx.attr.content)
        lines += _describe("a", a)
        files.append(a)
    elif mode == "bin_dir":
        a = actions.declare_shareable_artifact(path, ctx.bin_dir)
        actions.write(a, ctx.attr.content)
        lines += _describe("a", a)
        files.append(a)
    elif mode == "genfiles_dir":
        a = actions.declare_shareable_artifact(path, ctx.genfiles_dir)
        actions.write(a, ctx.attr.content)
        lines += _describe("a", a)
        files.append(a)
    elif mode == "file_root":
        anchor = actions.declare_file(ctx.label.name + ".anchor")
        actions.write(anchor, "")
        a = actions.declare_shareable_artifact(path, anchor.root)
        actions.write(a, ctx.attr.content)
        lines += _describe("a", a)
        lines.append("same_root=" + str(a.root == anchor.root))
        files += [anchor, a]
    elif mode == "twice":
        a = actions.declare_shareable_artifact(path)
        b = actions.declare_shareable_artifact(path)
        actions.write(a, ctx.attr.content)
        lines.append("equal=" + str(a == b))
        lines += _describe("b", b)
        files.append(b)
    elif mode == "declare_file_alias":
        a = actions.declare_file(ctx.attr.package_name)
        b = actions.declare_shareable_artifact(path)
        actions.write(b, ctx.attr.content)
        lines.append("equal=" + str(a == b))
        lines.append("in_set=" + str(len(depset([a, b]).to_list())))
        lines += _describe("a", a)
        files.append(a)
    elif mode == "symlink":
        a = actions.declare_shareable_artifact(path)
        actions.symlink(
            output = a,
            target_file = ctx.file.src,
            progress_message = "Symlinking virtual headers for %{label}",
            use_exec_root_for_source = ctx.attr.use_exec_root,
        )
        lines += _describe("a", a)
        files.append(a)
    elif mode == "orphan":
        actions.declare_shareable_artifact(path)
    elif mode == "same_writes":
        a = actions.declare_shareable_artifact(path)
        actions.write(a, ctx.attr.content)
        actions.write(a, ctx.attr.content)
        lines += _describe("a", a)
        files.append(a)
    elif mode == "two_writes":
        a = actions.declare_shareable_artifact(path)
        actions.write(a, "one\n")
        actions.write(a, "two\n")
    elif mode == "named":
        actions.declare_shareable_artifact(path = path)
    elif mode == "int_path":
        actions.declare_shareable_artifact(1)
    elif mode == "too_many":
        actions.declare_shareable_artifact(path, ctx.bin_dir, 3)
    elif mode == "root_string":
        actions.declare_shareable_artifact(path, "bazel-out")
    elif mode == "root_none":
        actions.declare_shareable_artifact(path, None)
    elif mode == "source_root":
        actions.declare_shareable_artifact(path, ctx.file.src.root)
    else:
        fail("unknown mode " + mode)
    report = actions.declare_file(ctx.label.name + ".report")
    actions.write(report, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([report] + files))]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "content": attr.string(default = "shared\n"),
        "mode": attr.string(default = "default"),
        "package_name": attr.string(),
        "path": attr.string(),
        "src": attr.label(allow_single_file = True),
        "use_exec_root": attr.bool(),
    },
)

SharedInfo = provider(doc = "The producer's own File of its artifact.", fields = ["file"])

def _shared_producer_impl(ctx):
    a = ctx.actions.declare_shareable_artifact(ctx.attr.path)
    ctx.actions.write(a, ctx.attr.content)
    return [DefaultInfo(files = depset([a])), SharedInfo(file = a)]

shared_producer = rule(
    implementation = _shared_producer_impl,
    attrs = {
        "content": attr.string(default = "shared\n"),
        "path": attr.string(),
    },
)

def _shared_consumer_impl(ctx):
    # One artifact reaching a consumer through ctx.files, the dependency's
    # DefaultInfo and a provider field holding the producer's own File: the
    # same File every way.
    via_files = ctx.files.dep[0]
    via_info = ctx.attr.dep[DefaultInfo].files.to_list()[0]
    via_provider = ctx.attr.dep[SharedInfo].file
    lines = (
        _describe("files", via_files) +
        _describe("info", via_info) +
        _describe("provider", via_provider) +
        [
            "files_equal_info=" + str(via_files == via_info),
            "files_equal_provider=" + str(via_files == via_provider),
            "in_set=" + str(len(depset([via_files, via_info, via_provider]).to_list())),
        ]
    )
    copy = ctx.actions.declare_file(ctx.label.name + ".copy")
    ctx.actions.run_shell(
        inputs = [via_files, via_info, via_provider],
        outputs = [copy],
        command = "cat \"$1\" \"$2\" \"$3\" > \"$4\"",
        arguments = [via_files.path, via_info.path, via_provider.path, copy.path],
    )
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([report, copy]))]

shared_consumer = rule(
    implementation = _shared_consumer_impl,
    attrs = {"dep": attr.label(providers = [SharedInfo])},
)
