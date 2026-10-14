# Field-only metadata projection of the pinned genuine Bazel contract.
# Never retrieve environ or serialize the host object itself.
def metadata(ctx):
    host = ctx.os
    name = getattr(host, "name")
    arch = getattr(host, "arch")
    keyed = {host: "safe-key"}
    return {
        "context_type": type(ctx),
        "type": type(host),
        "dir": dir(host),
        "str": str(host),
        "repr": repr(host),
        "same_context_object": host == ctx.os,
        "hashable_lookup": keyed[ctx.os],
        "name": name,
        "arch": arch,
        "name_type": type(name),
        "arch_type": type(arch),
        "name_str": str(name),
        "arch_str": str(arch),
        "name_repr": repr(name),
        "arch_repr": repr(arch),
        "name_json": json.encode(name),
        "arch_json": json.encode(arch),
        "lower_platform": name.lower(),
        "short_ascii_name_ops": [name.startswith("lin"), name.endswith("ux"), name.lower() == "linux", name == "Linux"],
        "arch_spellings": [arch == "amd64", arch == "x86_64"],
    }
