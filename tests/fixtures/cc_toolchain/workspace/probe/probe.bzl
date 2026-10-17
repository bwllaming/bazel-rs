"""F1 probe: json-encodes what a consumer sees of the resolved C++ toolchain."""

load("@rules_cc//cc:action_names.bzl", "ACTION_NAMES")
load("@rules_cc//cc:find_cc_toolchain.bzl", "find_cc_toolchain", "use_cc_toolchain")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")

CcToolchainInfo = cc_common.CcToolchainInfo

PUBLIC_FIELDS = [
    "built_in_include_directories",
    "all_files",
    "sysroot",
    "compiler",
    "libc",
    "cpu",
    "target_gnu_system_name",
    "toolchain_id",
    "dynamic_runtime_solib_dir",
    "objcopy_executable",
    "compiler_executable",
    "preprocessor_executable",
    "nm_executable",
    "objdump_executable",
    "ar_executable",
    "strip_executable",
    "ld_executable",
    "gcov_executable",
    "generate_modmap",
]

PRIVATE_FIELDS = [
    "_as_files",
    "_ar_files",
    "_strip_files",
    "_tool_paths",
    "_solib_dir",
    "_linker_files",
    "_coverage_files",
    "_compiler_files",
    "_dwp_files",
    "_builtin_include_files",
    "_legacy_cc_flags_make_variable",
    "_additional_make_variables",
    "_all_files_including_libc",
    "_abi",
    "_abi_glibc_version",
    "_crosstool_top_path",
    "_build_info_files",
    "_build_variables_dict",
    "_build_variables",
    "_supports_header_parsing",
    "_supports_param_files",
    "_toolchain_features",
    "_toolchain_label",
    "_cpp_configuration",
    "_link_dynamic_library_tool",
    "_grep_includes",
    "_if_so_builder",
    "_is_tool_configuration",
    "_is_sibling_repository_layout",
    "_stamp_binaries",
    "_static_runtime_lib_depset",
    "_dynamic_runtime_lib_depset",
    "_compiler_files_without_includes",
    "_allowlist_for_layering_check",
    "_cc_info",
    "_objcopy_files",
    "_aggregate_ddi",
    "_extra_allowlisted_feature_layering_check_macros",
    "_force_layering_check_features",
]

FEATURES = [
    "default_compile_flags",
    "default_link_flags",
    "opt",
    "dbg",
    "fastbuild",
    "host",
    "nonhost",
    "supports_pic",
    "supports_dynamic_linker",
    "supports_start_end_lib",
    "fixture_env",
    "fixture_requested",
    "fixture_disabled",
    "sysroot_flag",
    "parse_headers",
    "module_maps",
    "no_generate_debug_symbols",
    "dependency_file",
    "random_seed",
    "pic",
    "preprocessor_defines",
    "include_paths",
    "user_compile_flags",
    "sysroot",
    "unfiltered_compile_flags",
    "linker_param_file",
    "output_execpath_flags",
    "runtime_library_search_directories",
    "library_search_directories",
    "user_link_flags",
    "strip_debug_symbols",
    "shared_flag",
    "linkstamps",
    "static_libgcc",
    "fission_support",
    "compiler_input_flags",
    "compiler_output_flags",
    "c-compile",
    "c++-compile",
    "c++-link-executable",
    "c++-link-dynamic-library",
    "strip",
]

ACTIONS = [
    ACTION_NAMES.c_compile,
    ACTION_NAMES.cpp_compile,
    ACTION_NAMES.cpp_link_executable,
    ACTION_NAMES.cpp_link_dynamic_library,
]

def _file(f):
    return {"short_path": f.short_path, "source": f.is_source, "owner": str(f.owner)}

def _leaf(v):
    t = type(v)
    if v == None or t in ("bool", "int", "string"):
        return v
    if t == "File":
        return _file(v)
    if t == "Label":
        return "Label:" + str(v)
    if t == "function":
        return "function"
    return "<" + t + ">"

# Starlark forbids recursion: containers are encoded one level deep.
def enc(v):
    t = type(v)
    if t == "depset":
        return {"depset": [_leaf(x) for x in v.to_list()]}
    if t in ("list", "tuple"):
        return [_leaf(x) for x in v]
    if t == "dict":
        return {str(k): _leaf(x) for k, x in v.items()}
    return _leaf(v)

def _fields(info, fc):
    out = {}
    for name in PUBLIC_FIELDS + PRIVATE_FIELDS:
        out[name] = enc(getattr(info, name))
    out["needs_pic_for_dynamic_libraries"] = info.needs_pic_for_dynamic_libraries(feature_configuration = fc)
    out["static_runtime_lib"] = enc(info.static_runtime_lib(feature_configuration = fc))
    out["dynamic_runtime_lib"] = enc(info.dynamic_runtime_lib(feature_configuration = fc))
    fdo = info._fdo_context
    out["_fdo_context"] = {k: enc(getattr(fdo, k)) for k in dir(fdo)}
    out["_cc_info.compilation_context.module_map"] = enc(info._cc_info.compilation_context._module_map)
    bi = info._build_info_files
    out["_build_info_files.groups"] = {k: enc(getattr(bi, k)) for k in dir(bi)} if bi != None else None
    allow = info._allowlist_for_layering_check
    out["_allowlist_for_layering_check.type"] = type(allow)
    return out

def _actions(info, fc):
    compile_vars = cc_common.create_compile_variables(
        cc_toolchain = info,
        feature_configuration = fc,
        source_file = "probe/a.c",
        output_file = "probe/a.o",
        user_compile_flags = ["-user-copt"],
        include_directories = depset(["probe/inc"]),
        quote_include_directories = depset(["."]),
        system_include_directories = depset(["probe/sys"]),
        preprocessor_defines = depset(["PROBE=1"]),
        use_pic = True,
    )
    exe_vars = cc_common.create_link_variables(
        cc_toolchain = info,
        feature_configuration = fc,
        output_file = "probe/out",
        user_link_flags = ["-user-linkopt"],
        param_file = "probe/out-2.params",
        runtime_library_search_directories = depset(["rt"]),
        library_search_directories = depset(["ls"]),
        is_linking_dynamic_library = False,
        must_keep_debug = False,
    )
    so_vars = cc_common.create_link_variables(
        cc_toolchain = info,
        feature_configuration = fc,
        output_file = "probe/libout.so",
        user_link_flags = ["-user-linkopt"],
        is_linking_dynamic_library = True,
    )
    variables = {
        ACTION_NAMES.c_compile: compile_vars,
        ACTION_NAMES.cpp_compile: compile_vars,
        ACTION_NAMES.cpp_link_executable: exe_vars,
        ACTION_NAMES.cpp_link_dynamic_library: so_vars,
    }
    out = {}
    for action in ACTIONS:
        enabled = cc_common.action_is_enabled(feature_configuration = fc, action_name = action)
        entry = {"enabled": enabled}
        if enabled:
            entry["tool"] = cc_common.get_tool_for_action(feature_configuration = fc, action_name = action)
            entry["execution_requirements"] = cc_common.get_execution_requirements(feature_configuration = fc, action_name = action)
            entry["command_line"] = cc_common.get_memory_inefficient_command_line(
                feature_configuration = fc,
                action_name = action,
                variables = variables[action],
            )
            entry["env"] = cc_common.get_environment_variables(
                feature_configuration = fc,
                action_name = action,
                variables = variables[action],
            )
        out[action] = entry
    return out

def _providers(target):
    out = {"label": str(target.label)}
    if platform_common.TemplateVariableInfo in target:
        out["TemplateVariableInfo"] = target[platform_common.TemplateVariableInfo].variables
    if platform_common.ToolchainInfo in target:
        ti = target[platform_common.ToolchainInfo]
        out["ToolchainInfo.cc_provider_in_toolchain"] = ti.cc_provider_in_toolchain
        out["ToolchainInfo.cc.toolchain_id"] = ti.cc.toolchain_id
        out["ToolchainInfo.cc.type"] = type(ti.cc)
    if CcToolchainInfo in target:
        out["CcToolchainInfo.toolchain_id"] = target[CcToolchainInfo].toolchain_id
        out["CcToolchainInfo._toolchain_label"] = str(target[CcToolchainInfo]._toolchain_label)
    out["DefaultInfo.files"] = enc(target[DefaultInfo].files)
    return out

def _probe_impl(ctx):
    info = ctx.attr.toolchain[CcToolchainInfo] if ctx.attr.toolchain else find_cc_toolchain(ctx)
    fc = cc_common.configure_features(
        ctx = ctx,
        cc_toolchain = info,
        requested_features = ctx.features,
        unsupported_features = ctx.disabled_features,
    )
    doc = {
        "configuration": {
            "is_tool_configuration": ctx.configuration.is_tool_configuration(),
            "compilation_mode": ctx.var["COMPILATION_MODE"],
            "ctx.features": ctx.features,
            "ctx.disabled_features": ctx.disabled_features,
        },
        "type": type(info),
        "fields": _fields(info, fc),
        "feature_configuration": str(fc),
        "enabled": {f: cc_common.is_enabled(feature_configuration = fc, feature_name = f) for f in FEATURES},
        "actions": _actions(info, fc),
        "deps": [_providers(t) for t in ctx.attr.deps],
        "var": {k: ctx.var[k] for k in sorted(ctx.var.keys()) if k in ("CC", "CC_FLAGS", "AR", "NM", "LD", "STRIP", "OBJCOPY", "C_COMPILER", "CROSSTOOLTOP", "ABI", "ABI_GLIBC_VERSION", "TARGET_CPU", "COMPILATION_MODE", "GLIBC_VERSION", "STACK_FRAME_UNLIMITED", "FIXTURE_MAKE_VAR", "BINDIR", "GENDIR")},
    }
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode_indent(doc, indent = " ") + "\n")
    return [DefaultInfo(files = depset([out]))]

cc_toolchain_probe = rule(
    implementation = _probe_impl,
    attrs = {
        "deps": attr.label_list(),
        "toolchain": attr.label(providers = [CcToolchainInfo]),
    },
    fragments = ["cpp"],
    toolchains = use_cc_toolchain(),
)

def _exec_impl(ctx):
    return [DefaultInfo(files = ctx.attr.probe[DefaultInfo].files)]

exec_probe = rule(
    implementation = _exec_impl,
    attrs = {"probe": attr.label(cfg = "exec")},
)
