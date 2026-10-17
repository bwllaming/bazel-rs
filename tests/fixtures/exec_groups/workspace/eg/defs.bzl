def _tc(ctx):
    return [platform_common.ToolchainInfo(name = ctx.label.name)]
tc = rule(implementation = _tc)

def _notc(ctx):
    return []
notc = rule(implementation = _notc)

def _dep(ctx):
    print("DEP", ctx.label, ctx.target_platform_has_constraint(ctx.attr._cv[platform_common.ConstraintValueInfo]))
    return []
dep = rule(implementation = _dep, attrs = {"_cv": attr.label(default = "//eg:cv")})

def _actions(ctx, modes):
    outs = []
    for mode in modes:
        out = ctx.actions.declare_file(ctx.label.name + "." + mode)
        outs.append(out)
        script = ctx.actions.declare_file(ctx.label.name + "." + mode + ".sh")
        ctx.actions.write(script, "#!/bin/sh\necho ok > \"$1\"\n", is_executable = True)
        src = ctx.file.src
        if mode == "shell":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path)
        elif mode == "shell_link":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "link")
        elif mode == "shell_empty":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "empty")
        elif mode == "shell_nope":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "nope")
        elif mode == "shell_default":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "default-exec-group")
        elif mode == "shell_auto_name":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "//eg:t1")
        elif mode == "shell_int":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = 1)
        elif mode == "shell_test":
            ctx.actions.run_shell(outputs = [out], command = "echo ok > " + out.path, exec_group = "test")
        elif mode == "shell_tools":
            ctx.actions.run_shell(outputs = [out], inputs = [src], tools = [src], command = "echo ok > " + out.path)
        elif mode == "shell_tools_depset":
            ctx.actions.run_shell(outputs = [out], inputs = [src], tools = depset([src]), command = "echo ok > " + out.path)
        elif mode == "shell_tools_none":
            ctx.actions.run_shell(outputs = [out], inputs = [src], tools = [src], command = "echo ok > " + out.path, toolchain = None)
        elif mode == "shell_tools_t1":
            ctx.actions.run_shell(outputs = [out], inputs = [src], tools = [src], command = "echo ok > " + out.path, toolchain = "//eg:t1")
        elif mode == "shell_tools_link":
            ctx.actions.run_shell(outputs = [out], inputs = [src], tools = [src], command = "echo ok > " + out.path, exec_group = "link")
        elif mode == "run":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path])
        elif mode == "run_none":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = None)
        elif mode == "run_t1":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = "//eg:t1")
        elif mode == "run_t1_label":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = Label("//eg:t1"))
        elif mode == "run_t1_relative":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = ":t1")
        elif mode == "run_t3":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = "//eg:t3")
        elif mode == "run_link_t1":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], exec_group = "link", toolchain = "//eg:t1")
        elif mode == "run_link_t2opt":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], exec_group = "link", toolchain = "//eg:t2opt")
        elif mode == "run_link":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], exec_group = "link")
        elif mode == "run_toolchain_int":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = 1)
        elif mode == "run_toolchain_syntax":
            ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = "//eg:a:b")
        elif mode == "run_string":
            ctx.actions.run(outputs = [out], inputs = [script], executable = script.path, arguments = [out.path])
        else:
            fail("unknown mode " + mode)
    return outs

GROUP_ATTRS = {
    "bad": attr.string(),
    "modes": attr.string_list(),
    "src": attr.label(allow_single_file = True, default = "//eg:src.txt"),
    "tool": attr.label(cfg = config.exec(exec_group = "link")),
    "dtool": attr.label(cfg = "exec"),
    "tdep": attr.label(),
    "nope_tool": attr.label(cfg = config.exec(exec_group = "nope")),
}

def _groups(ctx):
    egs = ctx.exec_groups
    print("EG1", type(egs), repr(egs), "link" in egs, "empty" in egs, "nope" in egs, "default-exec-group" in egs, dir(egs))
    link = egs["link"]
    print("EG2", type(link), repr(link), dir(link), type(link.toolchains), repr(link.toolchains))
    print("EG3", link.toolchains["//eg:t1"].name, "//eg:t1" in link.toolchains, "//eg:t2" in link.toolchains, link.toolchains["//eg:t2opt"])
    print("EG4", repr(egs["empty"].toolchains), repr(ctx.toolchains), egs["empty"].toolchains.toolchain_types(), link.toolchains.toolchain_types())
    b = ctx.attr.bad
    if b == "unknown":
        egs["zzz"]
    if b == "lnk":
        egs["lnk"]
    if b == "int":
        egs[1]
    if b == "int_in":
        print(1 in egs)
    if b == "default":
        egs["default-exec-group"]
    if b == "not_requested":
        link.toolchains["//eg:t2"]
    if b == "default_tc":
        ctx.toolchains["//eg:t1"]
    return [DefaultInfo(files = depset(_actions(ctx, ctx.attr.modes)))]

groups = rule(
    implementation = _groups,
    exec_groups = {
        "link": exec_group(toolchains = ["//eg:t1", config_common.toolchain_type("//eg:t2opt", mandatory = False)]),
        "empty": exec_group(),
    },
    attrs = GROUP_ATTRS,
)

def _strict(ctx):
    print("STRICT", repr(ctx.exec_groups))
    return []

strict = rule(implementation = _strict, exec_groups = {"cvg": exec_group(exec_compatible_with = ["//eg:cv"])})
strict_tc = rule(implementation = _strict, exec_groups = {"cvg": exec_group(toolchains = ["//eg:t1"], exec_compatible_with = ["//eg:cv"])})
missing = rule(implementation = _strict, exec_groups = {"m": exec_group(toolchains = ["//eg:t3"])})
noinfo = rule(implementation = _strict, exec_groups = {"m": exec_group(toolchains = ["//eg:tnoinfo"])})

def _auto(ctx):
    print("AUTO1", repr(ctx.exec_groups), repr(ctx.toolchains), ctx.toolchains["//eg:t1"].name, ctx.toolchains["//eg:t2opt"], "//eg:t1" in ctx.toolchains)
    if "//eg:t1" in ctx.exec_groups:
        print("AUTO2", ctx.exec_groups["//eg:t1"].toolchains["//eg:t1"].name, repr(ctx.exec_groups["//eg:t1"].toolchains), "@@//eg:t1" in ctx.exec_groups, ctx.toolchains.toolchain_types())
    if ctx.attr.bad == "not_requested":
        ctx.toolchains["//eg:t3"]
    return [DefaultInfo(files = depset(_actions(ctx, ctx.attr.modes)))]

AUTO_ATTRS = {
    "bad": attr.string(),
    "modes": attr.string_list(),
    "src": attr.label(allow_single_file = True, default = "//eg:src.txt"),
}
AUTO_KW = dict(
    implementation = _auto,
    toolchains = ["//eg:t1", config_common.toolchain_type("//eg:t2opt", mandatory = False)],
    exec_groups = {"link": exec_group(toolchains = [config_common.toolchain_type("//eg:t2opt", mandatory = False)])},
)
auto = rule(attrs = AUTO_ATTRS | {"_use_auto_exec_groups": attr.bool(default = True)}, **AUTO_KW)
flagged = rule(attrs = AUTO_ATTRS, **AUTO_KW)
optout = rule(attrs = AUTO_ATTRS | {"_use_auto_exec_groups": attr.bool(default = False)}, **AUTO_KW)

def _plain(ctx):
    print("PLAIN", repr(ctx.exec_groups), repr(ctx.toolchains))
    return [DefaultInfo(files = depset(_actions(ctx, ctx.attr.modes)))]

plain = rule(implementation = _plain, attrs = AUTO_ATTRS | {"_use_auto_exec_groups": attr.bool(default = True)})

def _sub(ctx, mode):
    print("SUB", repr(ctx.toolchains), ctx.toolchains["//eg:t1"].name, ctx.toolchains.toolchain_types())
    out = ctx.actions.declare_file(mode + ".sub")
    script = ctx.actions.declare_file(mode + ".sub.sh")
    ctx.actions.write(script, "#!/bin/sh\necho ok > \"$1\"\n", is_executable = True)
    if mode == "run":
        ctx.actions.run(outputs = [out], executable = script, arguments = [out.path])
    elif mode == "exec_group":
        ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], exec_group = "link")
    elif mode == "toolchain":
        ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = "//eg:t1")
    elif mode == "toolchain_none":
        ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], toolchain = None)
    return out

S = subrule(implementation = _sub, toolchains = ["//eg:t1"])

def _withsub(ctx):
    print("RULE", ctx.toolchains["//eg:t1"].name)
    return [DefaultInfo(files = depset([S(ctx.attr.mode)]))]

withsub = rule(implementation = _withsub, subrules = [S], attrs = {"mode": attr.string(), "_use_auto_exec_groups": attr.bool(default = True)})

def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out)]

def _eg_test(ctx):
    egs = ctx.exec_groups
    tcs = egs["test"].toolchains
    print("TEST1", repr(egs), "test" in egs, repr(tcs), tcs["@bazel_tools//tools/test:default_test_toolchain_type"], tcs.toolchain_types())
    return _t(ctx)

eg_test = rule(implementation = _eg_test, test = True)

def _own_test(ctx):
    tcs = ctx.exec_groups["test"].toolchains
    print("OWN1", repr(ctx.exec_groups), repr(tcs), tcs["//eg:t1"].name, "@bazel_tools//tools/test:default_test_toolchain_type" in tcs)
    return _t(ctx)

own_test = rule(implementation = _own_test, test = True, exec_groups = {"test": exec_group(toolchains = ["//eg:t1"])})

def _cov_test(ctx):
    print("COV", repr(ctx.exec_groups))
    return _t(ctx)

cov_test = rule(implementation = _cov_test, test = True, attrs = {"_cov": attr.label(default = "//eg:dep", cfg = config.exec(exec_group = "test")), "tdep": attr.label()})

def _auto_test(ctx):
    print("AUTOTEST", repr(ctx.exec_groups))
    _actions(ctx, ctx.attr.modes)
    return _t(ctx)

auto_test = rule(implementation = _auto_test, test = True, attrs = AUTO_ATTRS | {"_use_auto_exec_groups": attr.bool(default = True)})

# Declaration shapes of genuine rule sets (read-only sources under the external cache):
# rules_python 1.7.0 python/private/attributes.bzl:39-45, 340-370 and py_executable.bzl:1815-1830
# (REQUIRED_EXEC_GROUP_BUILDERS cpp_link/py_precompile, _use_auto_exec_groups = True, coverage
# attributes with cfg = config.exec(exec_group = "test")); rules_go go/private/rules/binary.bzl:500-504
# and go/private/actions/link.bzl:206-216 (go_link group, actions.run(toolchain = ..., exec_group = ...));
# rules_cc cc/private/rules_impl/cc_test.bzl:60, 160-164 (cpp_link and a declared test group).
# A local stand-in for @bazel_tools//tools/cpp:toolchain_type, whose package loads rules_cc (stubbed here).
CPP = "//eg:cpp_toolchain_type"
PY_TOOLCHAINS = ["//eg:t1", config_common.toolchain_type("//eg:t2opt", mandatory = False), config_common.toolchain_type(CPP, mandatory = False)]

def _pyshape(ctx):
    print("PY", repr(ctx.exec_groups), repr(ctx.toolchains), ctx.toolchains["//eg:t1"].name, ctx.toolchains[CPP], ctx.exec_groups["cpp_link"].toolchains.toolchain_types())
    out = ctx.actions.declare_file(ctx.label.name + ".pyc")
    ctx.actions.run_shell(outputs = [out], inputs = [ctx.file.src], tools = [ctx.file.src], command = "echo ok > " + out.path,
                          exec_group = "py_precompile", toolchain = None)
    out2 = ctx.actions.declare_file(ctx.label.name + ".py.out")
    ctx.actions.run_shell(outputs = [out2], inputs = [ctx.file.src], tools = [ctx.file.src], command = "echo ok > " + out2.path, toolchain = "//eg:t1")
    if ctx.attr.is_test:
        return _t(ctx)
    return [DefaultInfo(files = depset([out, out2]))]

PY_ATTRS = {
    "src": attr.label(allow_single_file = True, default = "//eg:src.txt"),
    "is_test": attr.bool(),
    "_use_auto_exec_groups": attr.bool(default = True),
}
PY_GROUPS = {"cpp_link": exec_group(), "py_precompile": exec_group()}
pyshape = rule(implementation = _pyshape, attrs = PY_ATTRS, exec_groups = PY_GROUPS, toolchains = PY_TOOLCHAINS)
pyshape_test = rule(implementation = _pyshape, test = True, exec_groups = PY_GROUPS, toolchains = PY_TOOLCHAINS,
                    attrs = PY_ATTRS | {"_lcov_merger": attr.label(default = "//eg:dep", executable = False, cfg = config.exec(exec_group = "test"))})

def _goshape(ctx):
    print("GO", repr(ctx.exec_groups), repr(ctx.exec_groups["go_link"].toolchains), ctx.exec_groups["go_link"].toolchains["//eg:t1"].name)
    out = ctx.actions.declare_file(ctx.label.name + ".link")
    script = ctx.actions.declare_file(ctx.label.name + ".link.sh")
    ctx.actions.write(script, "#!/bin/sh\necho ok > \"$1\"\n", is_executable = True)
    ctx.actions.run(outputs = [out], executable = script, arguments = [out.path], mnemonic = "GoLink", toolchain = "//eg:t1", exec_group = "go_link")
    return [DefaultInfo(files = depset([out]))]

goshape = rule(implementation = _goshape, toolchains = ["//eg:t1"],
               exec_groups = {"go_link": exec_group(toolchains = ["//eg:t1", config_common.toolchain_type(CPP, mandatory = False)])})

def _ccshape(ctx):
    print("CC", repr(ctx.exec_groups), ctx.exec_groups["test"].toolchains["//eg:t2opt"], repr(ctx.exec_groups["cpp_link"].toolchains), "cpp_link" in ctx.exec_groups)
    return _t(ctx)

ccshape_test = rule(implementation = _ccshape, test = True, attrs = {"_use_auto_exec_groups": attr.bool(default = True)},
                    toolchains = [config_common.toolchain_type(CPP, mandatory = False)],
                    exec_groups = {"cpp_link": exec_group(toolchains = [config_common.toolchain_type(CPP, mandatory = False)]),
                                   "test": exec_group(toolchains = [config_common.toolchain_type("//eg:t2opt", mandatory = False)])})
