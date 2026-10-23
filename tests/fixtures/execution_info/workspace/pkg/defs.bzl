"""testing.ExecutionInfo (Bazel 9.3 analysis/test/ExecutionInfo.java)."""

E = testing.ExecutionInfo

# Created while this file loads (frozen), returned by `returns_loaded`.
LOADED = E({"requires-loaded": ""}, "loaded_group")

def _write(ctx, lines):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return out

def _probe_impl(ctx):
    a = E({"requires-x": "1", "b": ""})
    projection = a.requirements
    projection["mutated"] = "y"
    lines = [
        "type(E)=" + type(E),
        "repr(E)=" + repr(E),
        "type(a)=" + type(a),
        "dir(a)=" + repr(dir(a)),
        "repr(a)=" + repr(a),
        "str(a)=" + str(a),
        "requirements=" + repr(a.requirements),
        "type(requirements)=" + type(a.requirements),
        "exec_group=" + repr(a.exec_group),
        "projection=" + repr(projection),
        "after_projection=" + repr(a.requirements),
        "default=" + repr(E()),
        "named=" + repr(E(exec_group = "g", requirements = {"k": "v"})),
        "positional=" + repr(E({"k": "v"}, "g")),
        "empty_group=" + repr(E({}, "")),
        "equal=" + repr(E({"k": "v"}) == E(requirements = {"k": "v"})),
        "equal_order=" + repr(E({"a": "1", "b": "2"}) == E({"b": "2", "a": "1"})),
        "differs_group=" + repr(E({"k": "v"}) == E({"k": "v"}, "g")),
        "differs_value=" + repr(E({"k": "v"}) == E({"k": "w"})),
        "differs_type=" + repr(E() == struct(exec_group = "test", requirements = {})),
        "hash=" + repr({a: "found"}.get(E({"b": "", "requires-x": "1"}), "missing")),
        "hasattr=" + repr([hasattr(a, n) for n in ["requirements", "exec_group", "nope"]]),
        "getattr=" + repr(getattr(a, "nope", "default")),
        "loaded=" + repr(LOADED),
        "in_dep=" + repr(E in ctx.attr.dep),
        "dep_requirements=" + repr(ctx.attr.dep[E].requirements),
        "dep_exec_group=" + repr(ctx.attr.dep[E].exec_group),
        "dep_equal=" + repr(ctx.attr.dep[E] == E({"requires-dep": "", "no-remote": "1"})),
        "in_plain=" + repr(E in ctx.attr.plain),
        "loaded_dep=" + repr(ctx.attr.loaded[E] == LOADED),
    ]
    return [DefaultInfo(files = depset([_write(ctx, lines)]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "dep": attr.label(),
        "loaded": attr.label(),
        "plain": attr.label(),
    },
)

def _provider_rule_impl(_ctx):
    return [E({"requires-dep": "", "no-remote": "1"})]

provider_rule = rule(implementation = _provider_rule_impl)

def _plain_impl(_ctx):
    return []

plain = rule(implementation = _plain_impl)

def _returns_loaded_impl(_ctx):
    return [LOADED]

returns_loaded = rule(implementation = _returns_loaded_impl)

# A string list build setting (bazel_skylib's string_list_flag shape).
SettingInfo = provider(fields = ["value"])

def _requirements_setting_impl(ctx):
    return [SettingInfo(value = ctx.build_setting_value)]

requirements_setting = rule(
    implementation = _requirements_setting_impl,
    build_setting = config.string_list(flag = True),
)

# rules_python 1.7.0's py_interpreter_program (python/private/py_interpreter_program.bzl).
def _program_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = executable, target_file = ctx.file.main)
    execution_requirements = {}
    execution_requirements.update([
        value.split("=", 1)
        for value in ctx.attr.execution_requirements[SettingInfo].value
        if value.strip()
    ])
    return [
        DefaultInfo(
            executable = executable,
            files = depset([executable]),
            runfiles = ctx.runfiles(files = [executable]),
        ),
        testing.ExecutionInfo(
            requirements = execution_requirements,
        ),
    ]

program = rule(
    implementation = _program_impl,
    attrs = {
        "execution_requirements": attr.label(providers = [SettingInfo]),
        "main": attr.label(allow_single_file = True),
    },
    executable = False,
)

# rules_python's precompile.bzl: the program's requirements on an action.
def _consumer_impl(ctx):
    program = ctx.attr.program
    execution_requirements = {}
    if testing.ExecutionInfo in program:
        execution_requirements.update(program[testing.ExecutionInfo].requirements)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        tools = [program[DefaultInfo].files_to_run],
        command = "%s from-consumer > %s" % (program[DefaultInfo].files_to_run.executable.path, out.path),
        execution_requirements = execution_requirements,
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(
    implementation = _consumer_impl,
    attrs = {"program": attr.label(cfg = "exec")},
)

def _needs_impl(ctx):
    return [DefaultInfo(files = depset([_write(ctx, ["needs=" + repr(ctx.attr.dep[E].requirements)])]))]

needs = rule(
    implementation = _needs_impl,
    attrs = {"dep": attr.label(providers = [testing.ExecutionInfo])},
)

def _advertises_impl(_ctx):
    return []

# Built-in provider keys in `provides` are never checked.
advertises = rule(
    implementation = _advertises_impl,
    provides = [testing.ExecutionInfo],
)

def _bad_impl(ctx):
    mode = ctx.attr.mode
    if mode == "list":
        E([])
    elif mode == "none":
        E(None)
    elif mode == "int_key":
        E({1: "a"})
    elif mode == "int_value":
        E({"a": 1})
    elif mode == "group_int":
        E({}, 1)
    elif mode == "group_none":
        E(exec_group = None)
    elif mode == "unknown_keyword":
        E(requirement = {})
    elif mode == "three":
        E({}, "a", "b")
    elif mode == "three_bad_type":
        E([], "a", "b")
    elif mode == "duplicate":
        E({}, requirements = {})
    elif mode == "duplicate_bad_type":
        E({}, requirements = [])
    elif mode == "unknown_field":
        E().nope
    elif mode == "set_field":
        info = E()
        info.requirements = {}
    return []

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)

def _with_execution_info_test_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(executable, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [
        DefaultInfo(executable = executable),
        E({"requires-fixture": ""}),
    ]

with_execution_info_test = rule(
    implementation = _with_execution_info_test_impl,
    test = True,
)
