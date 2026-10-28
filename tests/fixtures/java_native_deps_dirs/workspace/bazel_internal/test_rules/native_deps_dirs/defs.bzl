f = java_common.internal_DO_NOT_USE().collect_native_deps_dirs
_loading = f(depset([struct(), struct(dynamic_library = "ignored")]))
OtherInfo = provider(fields = ["dynamic_library"])
Dirs = provider(fields = ["directories", "files"])

def _report(ctx):
    a, b, resolved, iface, real_iface, static = ctx.files.srcs
    generated = ctx.actions.declare_file("lib/libgenerated.so")
    ctx.actions.write(generated, ctx.var.get("CONTENT", "generated") + "\n")
    link = ctx.actions.declare_file("links/liblinked.so")
    ctx.actions.symlink(output = link, target_file = resolved)
    interface_link = ctx.actions.declare_file("interface_links/libinterface.so")
    ctx.actions.symlink(output = interface_link, target_file = real_iface)
    tree = ctx.actions.declare_directory("trees/tree.so")
    ctx.actions.run_shell(outputs = [tree], command = 'mkdir -p "$1"', arguments = [tree.path])
    unresolved = ctx.actions.declare_symlink("unresolved/lib.so")
    ctx.actions.symlink(output = unresolved, target_path = "missing")
    result = {"loading": _loading, "type": type(f(depset()))}
    cases = {
        "empty": [],
        "missing": [struct()],
        "static": [struct(static_library = static, pic_static_library = static)],
        "objects": [struct(objects = (static,))],
        "dynamic": [struct(dynamic_library = a)],
        "static_and_dynamic": [struct(static_library = static, dynamic_library = a)],
        "wrong_interface_type": [struct(interface_library = "ignored", dynamic_library = a)],
        "wrong_dynamic_type": [struct(dynamic_library = struct(path = a.path, short_path = a.short_path))],
        "interface_wins": [struct(interface_library = iface, dynamic_library = a)],
        "shared_interface_wins": [struct(interface_library = real_iface, dynamic_library = a)],
        "resolved_only": [struct(resolved_symlink_dynamic_library = resolved)],
        "solib": [struct(dynamic_library = link, resolved_symlink_dynamic_library = resolved)],
        "interface_solib": [struct(interface_library = interface_link, resolved_symlink_interface_library = real_iface, dynamic_library = a)],
        "generic_provider": [OtherInfo(dynamic_library = a)],
        "equal_dirnames": [struct(dynamic_library = a), struct(dynamic_library = b), struct(dynamic_library = generated)],
        "root_directory": [struct(dynamic_library = ctx.file.root)],
        "external": [struct(dynamic_library = ctx.file.external_a), struct(dynamic_library = ctx.file.external_b), struct(dynamic_library = ctx.file.external_generated)],
        "artifact_kinds": [struct(dynamic_library = tree), struct(dynamic_library = unresolved)],
    }
    for name, libraries in cases.items():
        result[name] = f(depset(libraries))
    result["alias_equal"] = ctx.file.external_a == ctx.file.external_canonical
    extensions = []
    for i, ext in enumerate(["so", "dylib", "dll", "pyd", "wasm", "tgt", "vpi", "so.2.10", "dylib.1a_b35", "ifso", "tbd", "lib", "dll.a"]):
        file = ctx.actions.declare_file("extensions/%d/lib.%s" % (i, ext))
        ctx.actions.write(file, "extension fixture")
        extensions.append(struct(dynamic_library = file))
    result["extensions"] = f(depset(extensions))
    for order in ["default", "preorder", "postorder", "topological"]:
        leaf = depset([struct(dynamic_library = ctx.file.external_a), struct(dynamic_library = a)], order = order)
        other = depset([struct(dynamic_library = link), struct(dynamic_library = ctx.file.external_a)], order = order)
        result[order] = f(depset([struct(dynamic_library = ctx.file.root)], transitive = [leaf, leaf, other], order = order))
    selected = [ctx.file.external_a, link, generated]
    if ctx.var.get("EXTERNAL", "a") == "b":
        selected[0] = ctx.file.external_b
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode(result))
    return [DefaultInfo(files = depset([out])), Dirs(directories = f(depset([struct(dynamic_library = lib) for lib in selected])), files = depset(selected))]

report = rule(implementation = _report, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "root": attr.label(default = "//:root.so", allow_single_file = True),
    "external_a": attr.label(default = "@a//:same/libexternal.so", allow_single_file = True),
    "external_b": attr.label(default = "@b//:same/libexternal.so", allow_single_file = True),
    "external_generated": attr.label(default = "@a//:generated", allow_single_file = True),
    "external_canonical": attr.label(default = "@@native_dep_a+//:same/libexternal.so", allow_single_file = True),
})

def _consume(ctx):
    info = ctx.attr.dep[Dirs]
    out = ctx.actions.declare_file(ctx.label.name + ".manifest")
    args = ctx.actions.args().add(out.path).add(":".join(info.directories))
    args.add_all(info.files)
    ctx.actions.run_shell(outputs = [out], inputs = info.files, arguments = [args],
        command = 'out="$1"; shift; printf "%s\\n" "$1" > "$out"; shift; for f in "$@"; do /bin/cat "$f" >> "$out"; done')
    return [DefaultInfo(files = depset([out]))]
consume = rule(implementation = _consume, attrs = {"dep": attr.label(providers = [Dirs])})

def _bad(ctx):
    mode = ctx.attr.mode
    if mode == "missing":
        f()
    elif mode == "extra":
        f(depset(), depset())
    elif mode == "named":
        f(libraries = depset())
    elif mode == "none":
        f(None)
    elif mode == "list":
        f([])
    elif mode == "string":
        f(depset(["bad"]))
    elif mode == "file":
        f(depset([ctx.file.library]))
    elif mode == "mutate_empty":
        f(depset()).append("bad")
    elif mode == "mutate_nonempty":
        f(depset([struct(dynamic_library = ctx.file.library)])).append("bad")
    elif mode == "invalid_dynamic":
        f(depset([struct(dynamic_library = ctx.file.invalid)]))
    elif mode == "invalid_interface":
        f(depset([struct(interface_library = ctx.file.invalid, dynamic_library = ctx.file.library)]))
    return []
bad = rule(implementation = _bad, attrs = {"mode": attr.string(), "library": attr.label(default = "lib/liba.so", allow_single_file = True), "invalid": attr.label(default = "static/libstatic.a", allow_single_file = True)})
