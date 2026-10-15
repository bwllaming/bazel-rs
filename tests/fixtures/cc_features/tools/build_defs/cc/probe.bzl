"""Probe of the C++ toolchain feature engine through the genuine rules_cc API.

This package is on Bazel's INTERNAL_STARLARK_API_ALLOWLIST and on rules_cc's
PRIVATE_STARLARKIFICATION_ALLOWLIST (`("", "tools/build_defs/cc")`), so it may
call the private feature-configuration methods. Each scenario returns a
JSON-encodable dict, or fails with the error under test.
"""

load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "action_config",
    "artifact_name_pattern",
    "env_entry",
    "env_set",
    "feature",
    "feature_set",
    "flag_group",
    "flag_set",
    "tool",
    "tool_path",
    "variable_with_value",
    "with_feature_set",
)
load("@rules_cc//cc/private:cc_common.bzl", "cc_common")
load("@rules_cc//cc/private:cc_internal.bzl", "cc_internal")
load("@rules_cc//cc/private/toolchain_config:configure_features.bzl", "configure_features")

C = "c-compile"
LINK = "c++-link-executable"
NO_LEGACY = feature(name = "no_legacy_features")

def toolchain(ctx, features, action_configs = [], tool_paths = [], artifact_name_patterns = []):
    info = cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "probe",
        compiler = "gcc",
        features = features,
        action_configs = action_configs,
        tool_paths = tool_paths,
        artifact_name_patterns = artifact_name_patterns,
    )
    return cc_common.cc_toolchain_features(
        toolchain_config_info = info,
        tools_directory = "tools/build_defs/cc",
    )

def configure(toolchain_features, requested):
    return toolchain_features.configure_features(requested_features = requested)

def variables(values):
    return cc_common.cc_toolchain_variables(vars = values)

def command_line(feature_configuration, action, build_variables):
    return cc_common.get_memory_inefficient_command_line(
        feature_configuration = feature_configuration,
        action_name = action,
        variables = build_variables,
    )

def environment(feature_configuration, action, build_variables):
    return cc_common.get_environment_variables(
        feature_configuration = feature_configuration,
        action_name = action,
        variables = build_variables,
    )

def tool_for(feature_configuration, action):
    return cc_common.get_tool_for_action(
        feature_configuration = feature_configuration,
        action_name = action,
    )

def rules_cc_configure(toolchain_features, requested, unsupported = []):
    """rules_cc's configure_features with a stub ctx and cc_toolchain.

    The stub carries the members configure_features.bzl reads; the values are
    the Bazel 9.3 defaults of a fastbuild target configuration.
    """
    cpp = struct(
        compilation_mode = lambda: "fastbuild",
        force_pic = lambda: False,
        apple_generate_dsym = False,
        objc_generate_linkmap = False,
        objc_should_strip_binary = False,
        _dont_enable_host_nonhost = True,
        fdo_instrument = lambda: None,
        cs_fdo_instrument = lambda: None,
        _fdo_prefetch_hints_label = None,
        use_llvm_coverage_map_format = lambda: False,
    )
    stub_ctx = struct(
        fragments = struct(cpp = cpp),
        configuration = struct(coverage_enabled = False),
    )
    cc_toolchain = struct(
        _supports_header_parsing = False,
        _cc_info = struct(compilation_context = struct(_module_map = None)),
        _toolchain_features = toolchain_features,
        _is_tool_configuration = False,
        _fdo_context = struct(),
        _toolchain_label = Label("//tools/build_defs/cc:stub_toolchain"),
    )
    return configure_features(
        ctx = stub_ctx,
        cc_toolchain = cc_toolchain,
        requested_features = requested,
        unsupported_features = unsupported,
    )

def _flags(flags, **kwargs):
    return flag_group(flags = flags, **kwargs)

def _selection(ctx):
    tc = toolchain(ctx, [
        NO_LEGACY,
        feature(name = "a", implies = ["b"]),
        feature(name = "b", implies = ["c"]),
        feature(name = "c"),
        feature(name = "d", requires = [
            feature_set(features = ["e"]),
            feature_set(features = ["a", "c"]),
        ]),
        feature(name = "e"),
        feature(name = "on", enabled = True),
        feature(name = "off", enabled = False),
        feature(name = "p1", provides = ["sym"]),
        feature(name = "p2", provides = ["sym"]),
        feature(name = "needs_gate", requires = [feature_set(features = ["gate"])]),
        feature(name = "gate"),
        feature(name = "implies_needs_gate", implies = ["needs_gate"]),
    ], [
        action_config(action_name = C, enabled = True, implies = ["e"]),
        action_config(action_name = LINK),
    ])
    requests = [
        ["a"],
        ["d", "a"],
        ["d", "e"],
        ["d"],
        ["implies_needs_gate"],
        ["implies_needs_gate", "gate"],
        ["p1", "unknown"],
        [C],
        ["off", "on", "d", "c"],
    ]
    fc = configure(tc, ["a", "unknown"])
    return {
        "defaults": tc.default_features_and_action_configs(),
        "configurations": [str(configure(tc, r)) for r in requests],
        "action_is_enabled": [
            cc_common.action_is_enabled(feature_configuration = configure(tc, r), action_name = C)
            for r in [[C], ["a"], ["e"]]
        ],
        "is_enabled": [fc.is_enabled(name) for name in ["a", "b", "c", "unknown"]],
        "is_requested": [fc.is_requested(name) for name in ["a", "b", "unknown"]],
        "types": [type(tc), type(fc), repr(fc)],
        "cached_order": str(configure(tc, ["unknown", "a"])),
    }

def _expansion(ctx):
    groups = [
        _flags(["-lit", "%%pct%%", "pre%{str}post", "%{path:a/./b/../c}"]),
        _flags(["avail-str"], expand_if_available = "str"),
        _flags(["avail-missing"], expand_if_available = "missing"),
        _flags(["avail-none"], expand_if_available = "none"),
        _flags(["not-avail-missing"], expand_if_not_available = "missing"),
        _flags(["not-avail-none"], expand_if_not_available = "none"),
        _flags(["not-avail-str"], expand_if_not_available = "str"),
        _flags(["true-t"], expand_if_true = "t"),
        _flags(["true-f"], expand_if_true = "f"),
        _flags(["true-missing"], expand_if_true = "missing"),
        _flags(["true-empty"], expand_if_true = "empty"),
        _flags(["true-list"], expand_if_true = "list"),
        _flags(["false-f"], expand_if_false = "f"),
        _flags(["false-t"], expand_if_false = "t"),
        _flags(["false-missing"], expand_if_false = "missing"),
        _flags(["false-empty"], expand_if_false = "empty"),
        _flags(["equal-str"], expand_if_equal = variable_with_value(name = "str", value = "S")),
        _flags(["equal-other"], expand_if_equal = variable_with_value(name = "str", value = "T")),
        _flags(["equal-bool"], expand_if_equal = variable_with_value(name = "t", value = "1")),
        _flags(["equal-missing"], expand_if_equal = variable_with_value(name = "missing", value = "x")),
        _flags(["-t=%{t}", "-f=%{f}"]),
        _flags(["-I%{list}"], iterate_over = "list"),
        _flags(["-T%{tuple}"], iterate_over = "tuple"),
        _flags(["-D%{dep}"], iterate_over = "dep"),
        _flags(["-K%{keys}"], iterate_over = "keys"),
        flag_group(iterate_over = "libs", flag_groups = [
            _flags(["-l%{libs.name}"], expand_if_equal = variable_with_value(name = "libs.kind", value = "dynamic")),
            _flags(["%{libs.name}.a"], expand_if_equal = variable_with_value(name = "libs.kind", value = "static")),
            _flags(["dep=%{libs.deps}"], iterate_over = "libs.deps"),
        ]),
        _flags(["nested=%{nested.inner.leaf}"]),
        _flags(["flat=%{flat.name}"]),
        _flags(["label=%{label}"]),
        _flags(["file=%{file}"]),
        _flags(["struct-field"], expand_if_available = "nested.inner"),
        _flags(["struct-no-field"], expand_if_available = "nested.nope"),
        _flags(["parent=%{p}%{q}"], expand_if_available = "p"),
    ]
    tc = toolchain(ctx, [
        NO_LEGACY,
        feature(
            name = "x",
            flag_sets = [
                flag_set(actions = [C], flag_groups = groups),
                flag_set(actions = [LINK], flag_groups = [_flags(["-link-only"])]),
            ],
            env_sets = [env_set(actions = [C], env_entries = [
                env_entry(key = "STR", value = "%{str}"),
                env_entry(key = "OPT", value = "o", expand_if_available = "missing"),
                env_entry(key = "PCT", value = "100%%"),
            ])],
        ),
    ])
    fc = configure(tc, ["x"])
    values = {
        "str": "S",
        "empty": "",
        "t": True,
        "f": False,
        "list": ["x", "y"],
        "tuple": ("t1",),
        "dep": depset(["d1", "d2"]),
        "keys": {"k1": 1, "k2": 2},
        "libs": [
            struct(name = "m", kind = "dynamic", deps = ["dx", "dy"]),
            struct(name = "z", kind = "static", deps = []),
        ],
        "nested": struct(inner = struct(leaf = "L")),
        "flat.name": "flat",
        "label": Label("//tools/build_defs/cc:src.txt"),
        "file": ctx.file.src,
        "none": None,
    }
    combined = cc_internal.combine_cc_toolchain_variables(
        variables({"p": "P"}),
        variables(values),
        variables({"q": "Q"}),
    )
    return {
        "compile": command_line(fc, C, combined),
        "link": command_line(fc, LINK, variables(values)),
        "unconfigured_action": command_line(fc, "objc-compile", variables(values)),
        "env": environment(fc, C, variables(values)),
        "empty_variables": [type(cc_common.empty_variables()), repr(combined)],
    }

def _with_features(ctx):
    tc = toolchain(ctx, [
        NO_LEGACY,
        feature(name = "opt"),
        feature(name = "dbg"),
        feature(
            name = "x",
            flag_sets = [
                flag_set(actions = [C], flag_groups = [_flags(["-always"])]),
                flag_set(
                    actions = [C],
                    with_features = [with_feature_set(features = ["opt"])],
                    flag_groups = [_flags(["-O2"])],
                ),
                flag_set(
                    actions = [C],
                    with_features = [with_feature_set(not_features = ["opt"])],
                    flag_groups = [_flags(["-O0"])],
                ),
                flag_set(
                    actions = [C],
                    with_features = [
                        with_feature_set(features = ["opt", "dbg"]),
                        with_feature_set(features = ["dbg"], not_features = ["opt"]),
                    ],
                    flag_groups = [_flags(["-g"])],
                ),
                flag_set(
                    actions = [C],
                    with_features = [with_feature_set(features = [C])],
                    flag_groups = [_flags(["-never-action-config-name"])],
                ),
            ],
            env_sets = [env_set(
                actions = [C],
                with_features = [with_feature_set(features = ["opt"])],
                env_entries = [env_entry(key = "MODE", value = "opt")],
            )],
        ),
    ], [
        action_config(
            action_name = C,
            tools = [
                tool(
                    path = "/opt/bin/cc",
                    with_features = [with_feature_set(features = ["opt"])],
                    execution_requirements = ["requires-opt"],
                ),
                tool(path = "bin/cc", execution_requirements = ["local", "local", "no-remote"]),
            ],
            flag_sets = [flag_set(flag_groups = [_flags(["-from-action-config"])])],
        ),
        action_config(action_name = LINK, tools = [tool(tool = ctx.file.tool)]),
        action_config(action_name = "strip", tools = [tool(path = "../up/./strip")]),
    ])
    empty = cc_common.empty_variables()
    results = []
    for requested in [["x", C], ["x", C, "opt"], ["x", C, "opt", "dbg"], ["x", C, "dbg"], ["x"]]:
        fc = configure(tc, requested)
        results.append({
            "requested": requested,
            "compile": command_line(fc, C, empty),
            "env": environment(fc, C, empty),
            "tool": tool_for(fc, C),
            "execution_requirements": cc_common.get_execution_requirements(
                feature_configuration = fc,
                action_name = C,
            ),
            "tool_requirement": cc_common.get_tool_requirement_for_action(
                feature_configuration = fc,
                action_name = C,
            ),
            "link_tool": tool_for(fc, LINK),
            "strip_tool": tool_for(fc, "strip"),
            "action_is_enabled": cc_common.action_is_enabled(feature_configuration = fc, action_name = C),
        })
    return {"results": results}

def _legacy(ctx):
    mine = feature(
        name = "mine",
        enabled = True,
        flag_sets = [flag_set(actions = [C], flag_groups = [_flags(["-mine"])])],
    )
    tool_paths = [
        tool_path(name = "gcc", path = "/usr/bin/gcc"),
        tool_path(name = "ar", path = "/usr/bin/ar"),
        tool_path(name = "strip", path = "/usr/bin/strip"),
    ]
    legacy = toolchain(ctx, [mine], tool_paths = tool_paths)
    plain = toolchain(ctx, [NO_LEGACY, mine], tool_paths = tool_paths)
    fc = rules_cc_configure(legacy, ["mine"])
    build_variables = variables({
        "source_file": "src/a.c",
        "output_file": "out/a.o",
        "preprocessor_defines": ["NDEBUG"],
        "include_paths": ["inc"],
        "quote_include_paths": ["."],
        "system_include_paths": ["/usr/include"],
        "framework_include_paths": [],
        "user_compile_flags": ["-Wall"],
        "unfiltered_compile_flags": [],
        "sysroot": "/sysroot",
    })
    plain_fc = rules_cc_configure(plain, ["mine"])
    return {
        "legacy_defaults": legacy.default_features_and_action_configs(),
        "plain_defaults": plain.default_features_and_action_configs(),
        "legacy_configuration": str(fc),
        "plain_configuration": str(plain_fc),
        "compile": command_line(fc, C, build_variables),
        "compile_tool": tool_for(fc, C),
        "link_tool": tool_for(fc, LINK),
        "archive_tool": tool_for(fc, "c++-link-static-library"),
        "plain_compile": command_line(plain_fc, C, build_variables),
        "plain_has_compile_config": cc_common.action_is_enabled(feature_configuration = plain_fc, action_name = C),
    }

def _error_duplicate_feature(ctx):
    toolchain(ctx, [NO_LEGACY, feature(name = "dup"), feature(name = "dup")])

def _error_undefined_implied(ctx):
    toolchain(ctx, [NO_LEGACY, feature(name = "a", implies = ["ghost"])])

def _error_undefined_required(ctx):
    toolchain(ctx, [NO_LEGACY, feature(name = "a", requires = [feature_set(features = ["ghost"])])])

def _error_unknown_action_tool(ctx):
    tc = toolchain(ctx, [NO_LEGACY], [action_config(action_name = C, tools = [tool(path = "cc")])])
    tool_for(configure(tc, [C]), "nope")

def _error_no_matching_tool(ctx):
    tc = toolchain(ctx, [NO_LEGACY, feature(name = "never")], [action_config(
        action_name = C,
        tools = [tool(path = "cc", with_features = [with_feature_set(features = ["never"])])],
    )])
    tool_for(configure(tc, [C]), C)

def _error_missing_variable(ctx):
    tc = toolchain(ctx, [NO_LEGACY, feature(name = "x", flag_sets = [
        flag_set(actions = [C], flag_groups = [_flags(["%{missing}"])]),
    ])])
    command_line(configure(tc, ["x"]), C, cc_common.empty_variables())

def _error_non_sequence(ctx):
    tc = toolchain(ctx, [NO_LEGACY, feature(name = "x", flag_sets = [
        flag_set(actions = [C], flag_groups = [_flags(["%{v}"], iterate_over = "v")]),
    ])])
    command_line(configure(tc, ["x"]), C, variables({"v": "scalar"}))

def _error_missing_field(ctx):
    tc = toolchain(ctx, [NO_LEGACY, feature(name = "x", flag_sets = [
        flag_set(actions = [C], flag_groups = [_flags(["%{s.inner.nope}"])]),
    ])])
    command_line(configure(tc, ["x"]), C, variables({"s": struct(inner = struct(leaf = "L"))}))

def _error_malformed(ctx):
    toolchain(ctx, [NO_LEGACY, feature(name = "x", flag_sets = [
        flag_set(actions = [C], flag_groups = [_flags(["-%{"])]),
    ])])

def _error_provides(ctx):
    tc = toolchain(ctx, [
        NO_LEGACY,
        feature(name = "p1", provides = ["sym"]),
        feature(name = "p2", provides = ["sym"]),
    ])
    configure(tc, ["p2", "p1"])

def _error_unconditional(ctx):
    # "base" is a default feature that implies the unsupported "on".
    tc = toolchain(ctx, [
        NO_LEGACY,
        feature(name = "base", enabled = True, implies = ["on"]),
        feature(name = "on"),
    ])
    rules_cc_configure(tc, [], unsupported = ["on"])

def _error_artifact_pattern(ctx):
    toolchain(ctx, [NO_LEGACY], artifact_name_patterns = [
        artifact_name_pattern(category_name = "executable", prefix = "", extension = ".bin"),
    ])

SCENARIOS = {
    "selection": _selection,
    "expansion": _expansion,
    "with_features": _with_features,
    "legacy": _legacy,
    "error_duplicate_feature": _error_duplicate_feature,
    "error_undefined_implied": _error_undefined_implied,
    "error_undefined_required": _error_undefined_required,
    "error_unknown_action_tool": _error_unknown_action_tool,
    "error_no_matching_tool": _error_no_matching_tool,
    "error_missing_variable": _error_missing_variable,
    "error_non_sequence": _error_non_sequence,
    "error_missing_field": _error_missing_field,
    "error_malformed": _error_malformed,
    "error_provides": _error_provides,
    "error_unconditional": _error_unconditional,
    "error_artifact_pattern": _error_artifact_pattern,
}

def _probe_impl(ctx):
    result = SCENARIOS[ctx.attr.scenario](ctx)
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode_indent(result) + "\n")
    return [DefaultInfo(files = depset([out]))]

cc_feature_probe = rule(
    implementation = _probe_impl,
    attrs = {
        "scenario": attr.string(mandatory = True),
        "src": attr.label(default = "//tools/build_defs/cc:src.txt", allow_single_file = True),
        "tool": attr.label(default = "//tools/build_defs/cc:tool.sh", allow_single_file = True),
    },
)
