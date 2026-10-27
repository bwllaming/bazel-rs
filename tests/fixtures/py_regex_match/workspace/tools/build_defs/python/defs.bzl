load(":cases.bzl", "CASES")
f = py_internal.regex_match
LOAD_RESULTS = [f(s, p) for s, p in CASES]

def results():
    return "\n".join([str(f(s, p)) for s, p in CASES]) + "\n"

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, results())
    return [DefaultInfo(files = depset([out]))]

probe = rule(implementation = _probe_impl)

def _namespace_impl(ctx):
    names = ["ansible", "_vendor", "9pkg", "é", "中文", "𐐀", "é", "a‌b", "has-hyphen", "ansible.dist-info", "!", "😀"]
    accepted = [name for name in names if f(name, "(?U)\\w+")]
    for name in accepted:
        ctx.file(name + "/__init__.py", "# namespace\n", executable = False)
    ctx.file("accepted.txt", "\n".join(accepted) + "\n", executable = False)
    ctx.file("BUILD.bazel", "exports_files([\"accepted.txt\"])\nfilegroup(name=\"namespaces\", srcs=glob([\"**/__init__.py\"]), visibility=[\"//visibility:public\"])\n")

namespace_repo = repository_rule(implementation = _namespace_impl)
