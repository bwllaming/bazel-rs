# go_library / go_binary / go_context_data / nogo shaped after rules_go 0.63.0
# (go/private/rules/library.bzl:79-205, binary.bzl, context.bzl:807-845,
# nogo.bzl:94-121) with simplified implementations that report the build
# settings each target sees and the files of its transitioned dependencies.
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("//go/private:providers.bzl", "GoArchive", "GoInfo")
load(
    ":transition.bzl",
    "go_tool_transition",
    "go_transition",
    "non_go_transition",
    "non_request_nogo_transition",
)

_ALLOW = {
    "_allowlist_function_transition": attr.label(
        default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
    ),
}

_SETTINGS = {
    "_race": attr.label(default = "//go/config:race"),
    "_pure": attr.label(default = "//go/config:pure"),
    "_linkmode": attr.label(default = "//go/config:linkmode"),
    "_tags": attr.label(default = "//go/config:tags"),
    "_request_nogo": attr.label(default = "//go/private:request_nogo"),
    "_bootstrap_nogo": attr.label(default = "//go/private:bootstrap_nogo"),
    "_original_race": attr.label(default = "//go/private/rules:original_race"),
}

def _settings(ctx):
    return "race=%s pure=%s linkmode=%s tags=%s request_nogo=%s bootstrap_nogo=%s original_race=%r" % (
        ctx.attr._race[BuildSettingInfo].value,
        ctx.attr._pure[BuildSettingInfo].value,
        ctx.attr._linkmode[BuildSettingInfo].value,
        ctx.attr._tags[BuildSettingInfo].value,
        ctx.attr._request_nogo[BuildSettingInfo].value,
        ctx.attr._bootstrap_nogo[BuildSettingInfo].value,
        ctx.attr._original_race[BuildSettingInfo].value,
    )

def _files(targets):
    return [f.path for t in targets for f in t[DefaultInfo].files.to_list()]

def _report(ctx, lines, deps = []):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(["%s %s" % (ctx.label.name, _settings(ctx))] + lines) + "\n")
    return depset([out], transitive = [d[DefaultInfo].files for d in deps])

def _go_context_data_impl(ctx):
    files = _report(ctx, ["  coverdata=%s" % _files(ctx.attr.coverdata)], ctx.attr.coverdata)
    return [DefaultInfo(files = files)]

go_context_data = rule(
    _go_context_data_impl,
    attrs = dict(_ALLOW, coverdata = attr.label(
        mandatory = True,
        cfg = non_request_nogo_transition,
        providers = [GoArchive],
    ), **_SETTINGS),
)

def _go_library_impl(ctx):
    lines = [
        "  srcs=%s" % _files(ctx.attr.srcs),
        "  data=%s" % _files(ctx.attr.data),
        "  embedsrcs=%s" % _files(ctx.attr.embedsrcs),
        "  cdeps=%s" % _files(ctx.attr.cdeps),
        "  deps=%s" % _files(ctx.attr.deps),
        "  nogo=%s" % _files([ctx.attr._nogo]),
        "  context=%s" % _files([ctx.attr._go_context_data]),
    ]
    files = _report(ctx, lines, ctx.attr.data + ctx.attr.deps + [ctx.attr._go_context_data])
    return [DefaultInfo(files = files), GoInfo(importpath = ctx.attr.importpath), GoArchive(name = ctx.label.name)]

go_library = rule(
    _go_library_impl,
    attrs = dict(_ALLOW, **dict({
        "data": attr.label_list(allow_files = True, cfg = non_go_transition),
        "srcs": attr.label_list(allow_files = True, cfg = non_go_transition),
        "deps": attr.label_list(providers = [GoInfo]),
        "importpath": attr.string(),
        "embedsrcs": attr.label_list(allow_files = True, cfg = non_go_transition),
        "cdeps": attr.label_list(cfg = non_go_transition),
        "_go_context_data": attr.label(default = "//go:go_context_data"),
        "_nogo": attr.label(default = "//go:nogo", cfg = "exec"),
    }, **_SETTINGS)),
    provides = [GoArchive, GoInfo],
)

def _nogo_impl(ctx):
    files = _report(ctx, ["  deps=%s" % _files(ctx.attr.deps)], ctx.attr.deps)
    return [DefaultInfo(files = files)]

nogo = rule(
    _nogo_impl,
    attrs = dict(_ALLOW, deps = attr.label_list(providers = [GoArchive]), **_SETTINGS),
    cfg = go_tool_transition,
)

def _go_binary_impl(ctx):
    lines = [
        "  srcs=%s" % _files(ctx.attr.srcs),
        "  data=%s" % _files(ctx.attr.data),
        "  deps=%s" % _files(ctx.attr.deps),
    ]
    return [DefaultInfo(files = _report(ctx, lines, ctx.attr.data + ctx.attr.deps))]

go_binary = rule(
    _go_binary_impl,
    attrs = dict(_ALLOW, **dict({
        "srcs": attr.label_list(allow_files = True, cfg = non_go_transition),
        "data": attr.label_list(allow_files = True, cfg = non_go_transition),
        "deps": attr.label_list(providers = [GoInfo]),
        "goos": attr.string(default = "auto"),
        "goarch": attr.string(default = "auto"),
        "race": attr.string(default = "auto"),
        "msan": attr.string(default = "auto"),
        "static": attr.string(default = "auto"),
        "pure": attr.string(default = "auto"),
        "gotags": attr.string_list(),
        "linkmode": attr.string(default = "auto"),
        "pgoprofile": attr.label(allow_files = True, default = "//go/config:empty"),
    }, **_SETTINGS)),
    cfg = go_transition,
)

def _go_tool_library_impl(ctx):
    files = _report(ctx, ["  srcs=%s" % _files(ctx.attr.srcs)])
    return [DefaultInfo(files = files), GoInfo(importpath = ctx.attr.importpath), GoArchive(name = ctx.label.name)]

# go_tool_library (library.bzl:216-245): no transitions, no context data.
go_tool_library = rule(
    _go_tool_library_impl,
    attrs = dict({
        "srcs": attr.label_list(allow_files = True),
        "importpath": attr.string(),
    }, **_SETTINGS),
)
