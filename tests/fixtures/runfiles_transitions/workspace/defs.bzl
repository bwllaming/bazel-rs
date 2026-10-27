K = "//command_line_option:build_runfile_links"
M = "//command_line_option:build_runfile_manifests"
E = "//command_line_option:enable_runfiles"

def _settings(settings, mode):
    out = dict(settings)
    if mode == "off":
        out[K] = False
    elif mode == "on":
        out[K] = True
    elif mode == "no":
        out[E] = "no"
    elif mode == "auto":
        out[E] = "auto"
    elif mode == "no_manifests":
        out[M] = False
    elif mode == "invalid":
        out[K] = 1
    elif mode == "null":
        out[K] = None
    elif mode == "invalid_enum":
        out[E] = True
    elif mode == "null_enum":
        out[E] = None
    elif mode == "invalid_string":
        out[K] = "bad"
    return out

def _change(settings, attr):
    print("RF_INPUT " + repr([(type(settings[k]), str(settings[k])) for k in [K, M, E]]))
    return _settings(settings, attr.mode)

change = transition(implementation = _change, inputs = [K, M, E], outputs = [K, M, E])

def _split(settings, attr):
    return {"on": _settings(settings, "on"), "off": _settings(settings, "off")}

split = transition(implementation = _split, inputs = [K, M, E], outputs = [K, M, E])

_ALLOW = attr.label(default = "//tools/allowlists/function_transition_allowlist")

SCRIPT = """#!/bin/sh
set -eu
tree="${TEST_SRCDIR:-$0.runfiles}"
if [ -f "$tree/_main/input.txt" ]; then
  value=$(cat "$tree/_main/input.txt")
else
  manifest="${RUNFILES_MANIFEST_FILE:-$tree/MANIFEST}"
  [ -f "$manifest" ] || manifest="$0.runfiles_manifest"
  value=$(cat "$(sed -n 's|^_main/input.txt ||p' "$manifest")")
fi
if [ $# -gt 0 ]; then printf '%s\\n' "$value" > "$1"; else printf '%s manifest_only=%s\\n' "$value" "${RUNFILES_MANIFEST_ONLY-unset}"; fi
"""

def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, SCRIPT, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = [ctx.file.data]))]

_attrs = {"data": attr.label(default = "//:input.txt", allow_single_file = True), "mode": attr.string(default = "keep")}
bin = rule(implementation = _bin, executable = True, attrs = _attrs)
changed_bin = rule(implementation = _bin, executable = True, cfg = change, attrs = dict(_attrs, _allowlist_function_transition = _ALLOW))
changed_test = rule(implementation = _bin, test = True, cfg = change, attrs = dict(_attrs, _allowlist_function_transition = _ALLOW))

def _info(dep):
    f = dep[DefaultInfo].files_to_run
    return {"exe": f.executable.path, "manifest": f.runfiles_manifest.path if f.runfiles_manifest else None, "mapping": f.repo_mapping_manifest.path if f.repo_mapping_manifest else None}

def _view(ctx):
    deps = ctx.attr.dep if type(ctx.attr.dep) == "list" else [ctx.attr.dep]
    value = {"files": [_info(dep) for dep in deps], "links_match": ctx.attr.links_match, "native_match": ctx.attr.native_match}
    print("RF_VIEW " + ctx.label.name + " " + json.encode(value))
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode(value))
    return [DefaultInfo(files = depset([out]))]

_view_attrs = {"dep": attr.label(), "mode": attr.string(default = "keep"), "links_match": attr.bool(), "native_match": attr.bool()}
view = rule(implementation = _view, attrs = _view_attrs)
changed_view = rule(implementation = _view, attrs = dict(_view_attrs, dep = attr.label(cfg = change), _allowlist_function_transition = _ALLOW))
exec_view = rule(implementation = _view, attrs = dict(_view_attrs, dep = attr.label(cfg = "exec")))
split_view = rule(implementation = _view, attrs = dict(_view_attrs, dep = attr.label(cfg = split), _allowlist_function_transition = _ALLOW))

def _use(ctx):
    dep = ctx.attr.dep[0] if type(ctx.attr.dep) == "list" else ctx.attr.dep
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(executable = dep[DefaultInfo].files_to_run, outputs = [out], arguments = [out.path])
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use, attrs = {"dep": attr.label(cfg = change, executable = True), "mode": attr.string(), "_allowlist_function_transition": _ALLOW})

def native_setting(**kwargs):
    native.config_setting(**kwargs)

def _aspect_impl(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".aspect.txt")
    ctx.actions.run(executable = ctx.attr._tool[DefaultInfo].files_to_run, outputs = [out], arguments = [out.path])
    return [OutputGroupInfo(proof = depset([out]))]

_tool_aspect = aspect(implementation = _aspect_impl, attrs = {"_tool": attr.label(default = "//:exe", executable = True, cfg = "exec")})

def _aspect_use(ctx):
    return [DefaultInfo(files = ctx.attr.dep[0][OutputGroupInfo].proof)]

aspect_use = rule(implementation = _aspect_use, attrs = {"dep": attr.label(cfg = change, aspects = [_tool_aspect]), "mode": attr.string(), "_allowlist_function_transition": _ALLOW})
