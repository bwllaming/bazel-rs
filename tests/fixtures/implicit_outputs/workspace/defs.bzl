# N46 fixture (docs/IMPLICIT_OUTPUTS.md): rule(outputs = ...) implicit outputs.

def _write_all(ctx):
    for key in dir(ctx.outputs):
        value = getattr(ctx.outputs, key)
        if type(value) == "File":
            ctx.actions.write(value, key + "\n")
        elif type(value) == "list":
            for file in value:
                ctx.actions.write(file, key + "\n")
    print("OUTPUTS", ctx.outputs, dir(ctx.outputs))

def _impl(ctx):
    _write_all(ctx)

def _exe_impl(ctx):
    _write_all(ctx)
    ctx.actions.write(ctx.outputs.executable, "#!/bin/sh\n", is_executable = True)

ATTRS = {
    "src": attr.label(allow_files = True),
    "srcs": attr.label_list(allow_files = True),
    "s": attr.string(),
    "ss": attr.string_list(),
    "n": attr.int(),
    "b": attr.bool(),
    "d": attr.string_dict(),
    "o": attr.output(),
    "os": attr.output_list(),
    "lbl": attr.label(),
}

def mk(outputs, executable = False):
    return rule(implementation = _exe_impl if executable else _impl, outputs = outputs, attrs = ATTRS, executable = executable)

ok = mk({"a": "%{name}.a", "b": "%{src}-%{s}.b", "c": "%{dirname}%{basename}.c"})
lit = mk({"e": "lit%{name"})
multi = mk({"m": "%{srcs}.m"})
unknown = mk({"u": "%{missing}.u"})
private = rule(implementation = _impl, outputs = {"p": "%{_priv}.p"}, attrs = {"_priv": attr.string(default = "P")})
int_t = mk({"i": "%{n}.i"})
bool_t = mk({"i": "%{b}.i"})
dict_t = mk({"i": "%{d}.i"})
vis_t = mk({"v": "%{visibility}.v"})
tags_t = mk({"t": "%{tags}.t"})
dep_t = mk({"t": "%{deprecation}.t"})
lbl_t = mk({"l": "%{lbl}.l"})
emptylist = mk({"e": "%{ss}.e"})
list_t = mk({"one": "%{srcs}.one", "strs": "%{ss}.strs"})
outs_t = mk({"o2": "%{o}.x", "os2": "%{os}.y"})
collide = mk({"o": "%{name}.o"})
dup = mk({"a": "%{name}.same", "b": "%{name}.same"})
up = mk({"a": "../%{name}"})
empty = mk({"a": ""})
selfname = mk({"a": "%{name}"})
exe = mk({"executable": "%{name}.exe"}, executable = True)
exe_ok = mk({"a": "%{name}.a"}, executable = True)
prefix = mk({"a": "%{name}", "b": "%{name}/sub"})
sel = mk({"s": "%{s}.sel"})

def _f_ok(name, s, src, lbl, tags, _priv):
    return {"k": name + "." + s, "t": "%{name}.tpl", "l": "l-" + src.name + "-" + str(lbl) + "-" + ",".join(tags) + _priv}

def _f_unknown(nope):
    return {}

def _f_list(name):
    return [name]

def _f_fail(name):
    fail("boom " + name)

def _f_multi(name):
    return {"m": "%{srcs}.m"}

def _f_int(name):
    return {"k": 1}

def _f_select(s):
    return {"k": s}

def _f_badtpl(name):
    return {"bad": "%{nothing}"}

def _f_native(name):
    return {"k": native.package_name()}

def _f_glob(name):
    return {"k": native.glob(["*.txt"])[0]}

def _f_kwonly(*, name):
    return {"k": name}

FN_ATTRS = dict(ATTRS, _priv = attr.string(default = "P"))
fn_ok = rule(implementation = _impl, outputs = _f_ok, attrs = FN_ATTRS)
fn_unknown = rule(implementation = _impl, outputs = _f_unknown, attrs = ATTRS)
fn_list = rule(implementation = _impl, outputs = _f_list, attrs = ATTRS)
fn_fail = rule(implementation = _impl, outputs = _f_fail, attrs = ATTRS)
fn_multi = rule(implementation = _impl, outputs = _f_multi, attrs = ATTRS)
fn_int = rule(implementation = _impl, outputs = _f_int, attrs = ATTRS)
fn_select = rule(implementation = _impl, outputs = _f_select, attrs = ATTRS)
fn_badtpl = rule(implementation = _impl, outputs = _f_badtpl, attrs = ATTRS)
fn_native = rule(implementation = _impl, outputs = _f_native, attrs = ATTRS)
fn_glob = rule(implementation = _impl, outputs = _f_glob, attrs = ATTRS)
fn_kwonly = rule(implementation = _impl, outputs = _f_kwonly, attrs = ATTRS)

# rules_cc 0.2.17 cc_binary's declaration shape (cc/private/rules_impl/cc_binary.bzl:899-910):
# executable, outputs = {dwp_file, stripped_binary}, DefaultInfo files = the binary only.
def _cc_like_impl(ctx):
    binary = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(binary, "#!/bin/sh\necho binary\n", is_executable = True)
    ctx.actions.run_shell(
        inputs = [binary],
        outputs = [ctx.outputs.stripped_binary],
        command = "cp \"$1\" \"$2\"",
        arguments = [binary.path, ctx.outputs.stripped_binary.path],
        mnemonic = "CcStrip",
    )
    ctx.actions.write(ctx.outputs.dwp_file, "")
    return [DefaultInfo(files = depset([binary]), executable = binary)]

cc_like = rule(
    implementation = _cc_like_impl,
    executable = True,
    outputs = {"dwp_file": "%{name}.dwp", "stripped_binary": "%{name}.stripped"},
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
