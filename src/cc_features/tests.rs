//! Unit tests for the C++ feature engine against hand-built
//! `CcToolchainConfigInfo`-shaped structs (the rules_cc helpers produce
//! provider instances with the same fields; tests/cc_features.rs covers the
//! genuine rules_cc path).

use crate::language_modules::tests::{fails, pass};

const FILE: &str = "@@rules_cc+//cc/private:features_test.bzl";

/// Struct builders mirroring cc_toolchain_config_lib.bzl's fields.
const PRELUDE: &str = r#"
cc_internal = cc_common.internal_DO_NOT_USE()
def eq(actual, expected):
    if actual != expected:
        fail("got %r, want %r" % (actual, expected))
def feature(name, enabled = False, implies = [], requires = [], provides = [], flag_sets = [], env_sets = []):
    return struct(type_name = "feature", name = name, enabled = enabled, implies = implies,
                  requires = [struct(type_name = "feature_set", features = r) for r in requires],
                  provides = provides, flag_sets = flag_sets, env_sets = env_sets)
def flag_set(actions = ["c"], flag_groups = [], with_features = []):
    return struct(type_name = "flag_set", actions = actions, flag_groups = flag_groups, with_features = with_features)
def flag_group(flags = [], flag_groups = [], iterate_over = None, expand_if_available = None,
               expand_if_not_available = None, expand_if_true = None, expand_if_false = None,
               expand_if_equal = None):
    return struct(type_name = "flag_group", flags = flags, flag_groups = flag_groups,
                  iterate_over = iterate_over, expand_if_available = expand_if_available,
                  expand_if_not_available = expand_if_not_available, expand_if_true = expand_if_true,
                  expand_if_false = expand_if_false, expand_if_equal = expand_if_equal)
def equal(name, value):
    return struct(type_name = "variable_with_value", name = name, value = value)
def with_feature_set(features = [], not_features = []):
    return struct(type_name = "with_feature_set", features = features, not_features = not_features)
def env_set(actions = ["c"], env_entries = [], with_features = []):
    return struct(type_name = "env_set", actions = actions, env_entries = env_entries, with_features = with_features)
def env_entry(key, value, expand_if_available = None):
    return struct(type_name = "env_entry", key = key, value = value, expand_if_available = expand_if_available)
def action_config(name, enabled = False, tools = [], flag_sets = [], implies = []):
    return struct(type_name = "action_config", action_name = name, enabled = enabled, tools = tools,
                  flag_sets = flag_sets, implies = implies)
def tool(path = None, with_features = [], execution_requirements = []):
    return struct(type_name = "tool", path = path, tool = None, with_features = with_features,
                  execution_requirements = execution_requirements)
def pattern(category, prefix, extension):
    return struct(type_name = "artifact_name_pattern", category_name = category, prefix = prefix, extension = extension)
def toolchain(features = [], action_configs = [], patterns = [], tools_directory = "tc/pkg"):
    return cc_internal.cc_toolchain_features(
        toolchain_config_info = struct(
            _features_DO_NOT_USE = features,
            _action_configs_DO_NOT_USE = action_configs,
            _artifact_name_patterns_DO_NOT_USE = patterns,
            _exec_os_DO_NOT_USE = "LINUX",
        ),
        tools_directory = tools_directory,
    )
def enabled(tc, requested):
    fc = tc.configure_features(requested_features = requested)
    return [f for f in ["a", "b", "c", "d", "e", "x", "y", "z", "ac"] if fc.is_enabled(f)]
def cmd(fc, variables = {}, action = "c"):
    return cc_common.get_memory_inefficient_command_line(
        feature_configuration = fc, action_name = action,
        variables = cc_internal.cc_toolchain_variables(vars = variables))
def flags(groups, variables = {}):
    tc = toolchain([feature("a", flag_sets = [flag_set(flag_groups = groups)])])
    return cmd(tc.configure_features(requested_features = ["a"]), variables)
"#;

fn ok(body: &str) {
    pass(FILE, &format!("{PRELUDE}{body}"));
}

fn err(body: &str, message: &str) {
    fails(FILE, &format!("{PRELUDE}{body}"), message);
}

#[test]
fn selection_follows_implies_requires_and_declaration_order() {
    ok(r#"
# Implied features are enabled transitively and reported in declaration order.
tc = toolchain([feature("c"), feature("b", implies = ["c"]), feature("a", implies = ["b"])])
eq(enabled(tc, ["a"]), ["a", "b", "c"])
fc = tc.configure_features(requested_features = ["a", "unknown"])
eq(str(fc), "<FeatureConfiguration(ENABLED:c, b, a;REQUESTED:a, unknown)>")
eq(repr(fc), "<unknown object com.google.devtools.build.lib.rules.cpp.FeatureConfigurationForStarlark>")
eq(type(fc), "FeatureConfiguration")
eq((fc.is_requested("unknown"), fc.is_requested("b"), fc.is_enabled("unknown")), (True, False, False))
# An implied feature whose requirement fails disables everything implying it.
tc = toolchain([feature("a", implies = ["b"]), feature("b", requires = [["x"]]), feature("x"), feature("c", implies = ["x"])])
eq(enabled(tc, ["a"]), [])
eq(enabled(tc, ["a", "x"]), ["a", "b", "x"])
eq(enabled(tc, ["a", "c"]), ["a", "b", "c", "x"])
# requires is any-of over all-of sets.
tc = toolchain([feature("a", requires = [["b"], ["c", "d"]]), feature("b"), feature("c"), feature("d")])
eq(enabled(tc, ["a", "c"]), ["c"])
eq(enabled(tc, ["a", "c", "d"]), ["a", "c", "d"])
eq(enabled(tc, ["a", "b"]), ["a", "b"])
# Removal propagates: x is only implied by a, which loses its requirement.
tc = toolchain([feature("a", implies = ["x"], requires = [["b"]]), feature("b"), feature("x")])
eq(enabled(tc, ["a"]), [])
eq(enabled(tc, ["a", "x"]), ["x"])
# Cycles of implications are stable.
tc = toolchain([feature("a", implies = ["b"]), feature("b", implies = ["a"])])
eq(enabled(tc, ["b"]), ["a", "b"])
# enabled=True features and action configs are the defaults, features first.
tc = toolchain([feature("a", enabled = True), feature("b"), feature("c", enabled = True)],
               [action_config("ac", enabled = True), action_config("z")])
eq(tc.default_features_and_action_configs(), ["a", "c", "ac"])
eq(type(tc), "CcToolchainFeatures")
eq(dir(tc), ["configure_features", "default_features_and_action_configs"])
eq(dir(fc), ["is_enabled", "is_requested"])
eq(repr(tc.configure_features), "<built-in method configure_features of CcToolchainFeatures value>")
# Action configs can imply features; features see only enabled features.
tc = toolchain([feature("a")], [action_config("ac", implies = ["a"])])
fc = tc.configure_features(requested_features = ["ac"])
eq((fc.is_enabled("a"), fc.is_enabled("ac")), (True, False))
eq(cc_common.action_is_enabled(feature_configuration = fc, action_name = "ac"), True)
eq(cc_common.action_is_enabled(feature_configuration = fc, action_name = "a"), False)
# The cache is keyed by the requested set; the first order is kept.
tc = toolchain([feature("a"), feature("b")])
first = tc.configure_features(requested_features = ["b", "a"])
second = tc.configure_features(requested_features = ["a", "b", "a"])
eq(str(second), "<FeatureConfiguration(ENABLED:a, b;REQUESTED:b, a)>")
eq(first == second, False)
"#);
}

#[test]
fn colliding_provides_and_graph_errors_use_bazel_texts() {
    ok(r#"
tc = toolchain([feature("a", provides = ["x"]), feature("b", provides = ["x"]), feature("c", provides = ["x"])])
eq(enabled(tc, ["a"]), ["a"])
"#);
    for (body, message) in [
        (
            r#"toolchain([feature("a", provides = ["x"]), feature("b", provides = ["x"]), feature("c", provides = ["x"])]).configure_features(requested_features = ["c", "a"])"#,
            "Symbol x is provided by all of the following features: a c",
        ),
        (
            r#"toolchain([feature("a"), feature("a")])"#,
            "Invalid toolchain configuration: feature or action config 'a' was specified multiple times.",
        ),
        (
            r#"toolchain([feature("c")], [action_config("c")])"#,
            "Invalid toolchain configuration: feature or action config 'c' was specified multiple times.",
        ),
        (
            r#"toolchain([feature("c"), feature("c")], [action_config("x"), action_config("x")])"#,
            "Invalid toolchain configuration: multiple action configs for action 'x'",
        ),
        (
            r#"toolchain([feature("a", implies = ["missing"])])"#,
            "Invalid toolchain configuration: feature 'missing', which is referenced from feature 'a', is not defined.",
        ),
        (
            r#"toolchain([feature("a", requires = [["b", "gone"]]), feature("b")])"#,
            "Invalid toolchain configuration: feature 'gone', which is referenced from feature 'a', is not defined.",
        ),
        (
            r#"toolchain([], [action_config("ac", implies = ["nope"])])"#,
            "Invalid toolchain configuration: feature 'nope', which is referenced from feature 'ac', is not defined.",
        ),
    ] {
        err(body, message);
    }
}

#[test]
fn parser_rejects_malformed_structs_with_bazel_texts() {
    for (body, message) in [
        (
            r#"toolchain([struct(type_name = "flag_set")])"#,
            "in struct instantiated at <unknown>: Expected object of type 'feature', received 'flag_set'.",
        ),
        (
            r#"toolchain([struct(name = "a")])"#,
            "Expected object of type 'feature', received 'struct'.",
        ),
        (
            r#"toolchain([struct(type_name = "feature", enabled = True)])"#,
            "Missing mandatory field 'name'",
        ),
        (
            r#"toolchain([feature("")])"#,
            "A feature must either have a nonempty 'name' field or be enabled.",
        ),
        (
            r#"toolchain([feature("Bad")])"#,
            "A feature's name must consist solely of lowercase ASCII letters, digits, '.', '_', '+', and '-', got 'Bad'",
        ),
        (
            r#"toolchain([struct(type_name = "feature", name = 1, enabled = True)])"#,
            "Field 'name' is not of 'java.lang.String' type.",
        ),
        (
            r#"toolchain([struct(type_name = "feature", name = "a", enabled = 1)])"#,
            "Field 'enabled' is not of 'java.lang.Boolean' type.",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(actions = [], flag_groups = [flag_group(flags = ["x"])])])])"#,
            "A flag_set that belongs to a feature must have nonempty 'actions' parameter.",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(flag_groups = [flag_group()])])])"#,
            "Both 'flags' and 'flag_groups' are empty.",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["x"], flag_groups = [flag_group(flags = ["y"])])])])])"#,
            "flag_group must contain either a list of flags or a list of flag_groups.",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["-x%{"])])])])"#,
            "Invalid toolchain configuration: expected variable name at position 4 while parsing a flag containing '-x%{'",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["%y"])])])])"#,
            "Invalid toolchain configuration: expected '{' at position 1 while parsing a flag containing '%y'",
        ),
        (
            r#"toolchain([feature("a", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["x"], expand_if_equal = equal("v", ""))])])])"#,
            "'value' parameter of variable_with_value must be a nonempty string.",
        ),
        (
            r#"toolchain([feature("a", implies = "b")])"#,
            "for implies, got string, want sequence",
        ),
        (
            r#"toolchain([feature("a", implies = [1])])"#,
            "at index 0 of implies, got element of type int, want string",
        ),
        (
            r#"toolchain([feature("a", flag_sets = ["x"])])"#,
            "at index 0 of flag_sets, got element of type string, want struct",
        ),
        (
            r#"toolchain([struct(type_name = "feature", name = "a", enabled = False, requires = [struct(type_name = "x")])])"#,
            "expected object of type 'feature_set'.",
        ),
        (
            r#"toolchain([feature("a", env_sets = [env_set(actions = [])])])"#,
            "actions parameter of env_set must be a nonempty list.",
        ),
        (
            r#"toolchain([feature("a", env_sets = [env_set(env_entries = [env_entry("", "v")])])])"#,
            "'key' parameter of env_entry must be a nonempty string.",
        ),
        (
            r#"toolchain([], [action_config("")])"#,
            "The 'action_name' field of action_config must be a nonempty string.",
        ),
        (
            r#"toolchain([], [action_config("C")])"#,
            "An action_config's name must consist solely of lowercase ASCII letters, digits, '.', '_', '+', and '-', got 'C'",
        ),
        (
            r#"toolchain([], [action_config("ac", flag_sets = [flag_set(actions = ["c"])])])"#,
            "action_config ac specifies actions.  An action_config's flag sets automatically apply to the configured action.  Thus, you must not specify action lists in an action_config's flag set.",
        ),
        (
            r#"toolchain([], [action_config("ac", tools = [tool()])])"#,
            "Exactly one of \"tool\" and \"path\" must be set.",
        ),
        (
            r#"toolchain([], [action_config("ac", tools = [tool(path = "./")])])"#,
            "The 'path' field of tool must be a nonempty string.",
        ),
        (
            r#"toolchain(patterns = [pattern("executable", "", ".bin")])"#,
            "Unrecognized file extension '.bin', allowed extensions are '', '.exe', or '.wasm', please check artifact_name_pattern configuration for executable in your rule.",
        ),
        (
            r#"toolchain(patterns = [pattern("static_library", "", ".x")])"#,
            "allowed extensions are '.a' or '.lib', please check",
        ),
        (
            r#"toolchain(patterns = [pattern("nope", "", "")])"#,
            "Artifact category nope not recognized.",
        ),
        (
            r#"cc_internal.cc_toolchain_features(toolchain_config_info = 1, tools_directory = "")"#,
            "in call to cc_toolchain_features(), parameter 'toolchain_config_info' got value of type 'int', want 'struct'",
        ),
        (
            r#"cc_internal.cc_toolchain_features(toolchain_config_info = struct(), tools_directory = None)"#,
            "in call to cc_toolchain_features(), parameter 'tools_directory' got value of type 'NoneType', want 'string'",
        ),
        (
            r#"cc_internal.cc_toolchain_features(toolchain_config_info = struct(_features_DO_NOT_USE = [], _action_configs_DO_NOT_USE = [], _artifact_name_patterns_DO_NOT_USE = [], _exec_os_DO_NOT_USE = 3), tools_directory = "")"#,
            "for _exec_os_DO_NOT_USE field, got int, want string",
        ),
        (
            r#"toolchain().configure_features(requested_features = ("a",))"#,
            "in call to configure_features(), parameter 'requested_features' got value of type 'tuple', want 'list'",
        ),
        (
            r#"toolchain().configure_features(requested_features = [1])"#,
            "at index 0 of requested_features, got element of type int, want string",
        ),
        (
            r#"toolchain().configure_features(requested_features = []).is_enabled(feature = "a")"#,
            "is_enabled() got named argument for positional-only parameter 'feature'",
        ),
    ] {
        err(body, message);
    }
}

#[test]
fn expansion_gates_iteration_and_variable_lookup() {
    ok(r#"
eq(flags([flag_group(flags = ["-a", "%%lit%%", "x%{v}y", "%{path:a/./b/../c}"])], {"v": "V"}),
   ["-a", "%lit%", "xVy", "a/c"])
# expand_if_available / expand_if_not_available.
g = [flag_group(flags = ["on"], expand_if_available = "v"), flag_group(flags = ["off"], expand_if_not_available = "v")]
eq(flags(g, {"v": "1"}), ["on"])
eq(flags(g, {}), ["off"])
eq(flags(g, {"v": None}), ["off"])
# expand_if_true / expand_if_false over booleans, strings and sequences.
g = [flag_group(flags = ["t"], expand_if_true = "b"), flag_group(flags = ["f"], expand_if_false = "b")]
eq(flags(g, {"b": True}), ["t"])
eq(flags(g, {"b": False}), ["f"])
eq(flags(g, {}), [])
eq(flags(g, {"b": ""}), ["f"])
eq(flags(g, {"b": "x"}), ["t"])
eq(flags(g, {"b": []}), ["f"])
eq(flags(g, {"b": struct()}), ["t"])
# Boolean variables expand to 1 and 0.
eq(flags([flag_group(flags = ["-b=%{b}"])], {"b": True}), ["-b=1"])
eq(flags([flag_group(flags = ["-b=%{b}"])], {"b": False}), ["-b=0"])
# expand_if_equal compares the string value.
g = [flag_group(flags = ["eq"], expand_if_equal = equal("m", "opt"))]
eq(flags(g, {"m": "opt"}), ["eq"])
eq(flags(g, {"m": "dbg"}), [])
eq(flags(g, {}), [])
eq(flags(g, {"m": True}), [])
eq(flags([flag_group(flags = ["one"], expand_if_equal = equal("m", "1"))], {"m": True}), ["one"])
# iterate_over: lists, tuples, depsets, dict keys and structures.
it = [flag_group(flags = ["-I%{d}"], iterate_over = "d")]
eq(flags(it, {"d": ["a", "b"]}), ["-Ia", "-Ib"])
eq(flags(it, {"d": ("x",)}), ["-Ix"])
eq(flags(it, {"d": depset(["p", "q"])}), ["-Ip", "-Iq"])
eq(flags(it, {"d": {"k1": 1, "k2": 2}}), ["-Ik1", "-Ik2"])
eq(flags(it, {"d": []}), [])
libs = [struct(name = "m", kind = "dynamic", deps = ["x", "y"]), struct(name = "z", kind = "static", deps = [])]
g = [flag_group(iterate_over = "libs", flag_groups = [
    flag_group(flags = ["-l%{libs.name}"], expand_if_equal = equal("libs.kind", "dynamic")),
    flag_group(flags = ["%{libs.name}.a"], expand_if_equal = equal("libs.kind", "static")),
    flag_group(flags = ["dep=%{libs.deps}"], iterate_over = "libs.deps"),
])]
eq(flags(g, {"libs": libs}), ["-lm", "dep=x", "dep=y", "z.a"])
# Dotted names reach into structures; nested structures too.
eq(flags([flag_group(flags = ["%{s.a}/%{s.b.c}"])], {"s": struct(a = "A", b = struct(c = "C"))}), ["A/C"])
# A dotted variable name that exists verbatim wins over structure access.
eq(flags([flag_group(flags = ["%{s.a}"])], {"s.a": "flat", "s": struct(a = "deep")}), ["flat"])
# Labels expand to their canonical form.
eq(flags([flag_group(flags = ["%{l}"])], {"l": Label("@@rules_cc+//cc:x")}), ["@@rules_cc+//cc:x"])
# expand_if_available on a structure field.
g = [flag_group(flags = ["has"], expand_if_available = "s.f")]
eq(flags(g, {"s": struct(f = "1")}), ["has"])
eq(flags(g, {"s": struct(g = "1")}), [])
eq(flags(g, {"s": "str"}), [])
eq(flags(g, {"s": struct(f = None)}), [])
"#);
    for (body, message) in [
        (
            r#"flags([flag_group(flags = ["%{missing}"])])"#,
            "Invalid toolchain configuration: Cannot find variable named 'missing'.",
        ),
        (
            r#"flags([flag_group(flags = ["%{v}"], iterate_over = "v")], {"v": "s"})"#,
            "Invalid toolchain configuration: Cannot expand variable 'v': expected sequence, found string",
        ),
        (
            r#"flags([flag_group(flags = ["%{v}"])], {"v": ["a"]})"#,
            "Invalid toolchain configuration: Cannot expand variable 'v': expected string, found sequence",
        ),
        (
            r#"flags([flag_group(flags = ["%{v}"])], {"v": struct()})"#,
            "Invalid toolchain configuration: Cannot expand variable 'v': expected string, found struct",
        ),
        (
            r#"flags([flag_group(flags = ["%{s.x}"])], {"s": struct(a = 1)})"#,
            "Invalid toolchain configuration: Cannot expand variable 's.x': structure s doesn't have a field named 'x'",
        ),
        (
            // Bazel keeps the outermost structure path for nested fields.
            r#"flags([flag_group(flags = ["%{s.b.x}"])], {"s": struct(b = struct(a = 1))})"#,
            "Invalid toolchain configuration: Cannot expand variable 's.x': structure s doesn't have a field named 'x'",
        ),
        (
            r#"flags([flag_group(flags = ["%{s.x}"])], {"s": "str"})"#,
            "Invalid toolchain configuration: Cannot expand variable 's.x': variable 's' is string, expected structure",
        ),
        (
            r#"flags([flag_group(flags = ["%{d.x}"])], {})"#,
            "Invalid toolchain configuration: Cannot find variable named 'd.x'.",
        ),
        (
            r#"flags([flag_group(flags = ["%{v}"])], {"v": 1})"#,
            "a value of type 'int' cannot be used as a C++ toolchain variable (Bazel 9.3 fails with an internal ClassCastException)",
        ),
        (
            r#"flags([flag_group(flags = ["%{v}"], iterate_over = "v")], {"v": [None]})"#,
            "element of sequence variable 'v' is None (Bazel 9.3 fails with an internal NullPointerException)",
        ),
        (
            r#"cc_internal.cc_toolchain_variables(vars = {1: "a"})"#,
            "got dict<int, string> for 'vars', want dict<string, unknown>",
        ),
        (
            r#"cc_internal.cc_toolchain_variables(vars = [])"#,
            "in call to cc_toolchain_variables(), parameter 'vars' got value of type 'list', want 'dict'",
        ),
    ] {
        err(body, message);
    }
}

#[test]
fn variables_values_parents_and_combination() {
    ok(r#"
empty = cc_common.empty_variables()
eq(type(empty), "Variables")
eq(repr(empty), "<unknown object com.google.devtools.build.lib.rules.cpp.CcToolchainVariables$MapVariables>")
parent = cc_internal.cc_toolchain_variables(vars = {"p": "P", "shadow": "parent"})
a = cc_internal.cc_toolchain_variables(vars = {"a": "A", "shadow": None})
b = cc_internal.cc_toolchain_variables(vars = {"b": "B"})
combined = cc_internal.combine_cc_toolchain_variables(parent, a, b)
single = cc_internal.combine_cc_toolchain_variables(parent, b)
eq(repr(single), "<unknown object com.google.devtools.build.lib.rules.cpp.CcToolchainVariables$SingleVariables>")
tc = toolchain([feature("a", flag_sets = [flag_set(flag_groups = [
    flag_group(flags = ["%{p}%{a}%{b}"]),
    flag_group(flags = ["shadowed"], expand_if_available = "shadow"),
])])])
fc = tc.configure_features(requested_features = ["a"])
eq(cc_common.get_memory_inefficient_command_line(feature_configuration = fc, action_name = "c", variables = combined), ["PAB"])
# A None value hides the parent's value of the same name.
eq(cc_common.get_memory_inefficient_command_line(
    feature_configuration = toolchain([feature("x", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["%{shadow}"], expand_if_available = "shadow")])])]).configure_features(requested_features = ["x"]),
    action_name = "c", variables = combined), [])
eq(cc_internal.intern_string_sequence_variable_value(["a", "b"]), ["a", "b"])
eq(cc_internal.exec_os == cc_internal.exec_os, True)
"#);
    for (body, message) in [
        (
            r#"cc_internal.combine_cc_toolchain_variables(cc_common.empty_variables(), cc_internal.cc_toolchain_variables(vars = {"a": 1}), cc_internal.cc_toolchain_variables(vars = {"a": 2}))"#,
            "Cannot overwrite existing variables: [a] (Bazel 9.3 fails with an internal IllegalArgumentException)",
        ),
        (
            r#"cc_internal.combine_cc_toolchain_variables(1)"#,
            "in call to combine_cc_toolchain_variables(), parameter 'parent' got value of type 'int', want 'Variables'",
        ),
        (
            r#"cc_internal.combine_cc_toolchain_variables(cc_common.empty_variables(), 1)"#,
            "at index 0 of variables, got element of type int, want Variables",
        ),
        (
            r#"cc_internal.intern_string_sequence_variable_value(["a"]).append("b")"#,
            "frozen",
        ),
        (
            r#"cc_internal.intern_string_sequence_variable_value([1])"#,
            "at index 0 of string_sequence, got element of type int, want string",
        ),
        (
            r#"cc_internal.exec_os(1)"#,
            "in call to exec_os(), parameter 'ctx' got value of type 'int', want 'ctx'",
        ),
        (
            r#"cc_common.empty_variables(1)"#,
            "_empty_variables() does not accept positional arguments, but got 1",
        ),
    ] {
        err(body, message);
    }
}

#[test]
fn flag_sets_env_sets_and_tools_honour_with_features() {
    ok(r#"
tc = toolchain(
    [
        feature("a", flag_sets = [
            flag_set(flag_groups = [flag_group(flags = ["-a"])]),
            flag_set(flag_groups = [flag_group(flags = ["-with-b"])], with_features = [with_feature_set(features = ["b"])]),
            flag_set(flag_groups = [flag_group(flags = ["-not-b"])], with_features = [with_feature_set(not_features = ["b"])]),
            flag_set(actions = ["other"], flag_groups = [flag_group(flags = ["-other"])]),
            flag_set(flag_groups = [flag_group(flags = ["-ac"])], with_features = [with_feature_set(features = ["ac"])]),
        ], env_sets = [
            env_set(env_entries = [env_entry("A", "1"), env_entry("V", "%{v}", expand_if_available = "v")]),
            env_set(env_entries = [env_entry("B", "b")], with_features = [with_feature_set(features = ["b"])]),
        ]),
        feature("b", flag_sets = [flag_set(flag_groups = [flag_group(flags = ["-b"])])]),
    ],
    [action_config("c", flag_sets = [flag_set(actions = [], flag_groups = [flag_group(flags = ["-config"])])], tools = [
        tool(path = "/abs/b-tool", with_features = [with_feature_set(features = ["b"])], execution_requirements = ["req-b"]),
        tool(path = "rel/../tool", execution_requirements = ["x", "x", "y"]),
    ]), action_config("ac")],
)
fc = tc.configure_features(requested_features = ["a", "c"])
eq(cmd(fc), ["-config", "-a", "-not-b"])
fcb = tc.configure_features(requested_features = ["a", "b", "c", "ac"])
eq(cmd(fcb), ["-config", "-a", "-with-b", "-b"])
eq(cmd(tc.configure_features(requested_features = ["a"])), ["-a", "-not-b"])
eq(cmd(fc, action = "other"), ["-other"])
env = cc_common.get_environment_variables(feature_configuration = fc, action_name = "c", variables = cc_internal.cc_toolchain_variables(vars = {"v": "vv"}))
eq(env, {"A": "1", "V": "vv"})
eq(cc_common.get_environment_variables(feature_configuration = fcb, action_name = "c", variables = cc_common.empty_variables()), {"A": "1", "B": "b"})
eq(cc_common.get_tool_for_action(feature_configuration = fc, action_name = "c"), "tc/pkg/tool")
eq(cc_common.get_tool_for_action(feature_configuration = fcb, action_name = "c"), "/abs/b-tool")
eq(cc_common.get_execution_requirements(feature_configuration = fc, action_name = "c"), ["x", "y"])
eq(cc_common.get_tool_requirement_for_action(feature_configuration = fcb, action_name = "c"), ["req-b"])
# The tool of a configured-but-disabled action config is still reported.
tc2 = toolchain([], [action_config("c", tools = [tool(path = "../up/gcc")])], tools_directory = "a/b")
fc2 = tc2.configure_features(requested_features = [])
eq(cc_common.action_is_enabled(feature_configuration = fc2, action_name = "c"), False)
eq(cc_common.get_tool_for_action(feature_configuration = fc2, action_name = "c"), "a/up/gcc")
tc3 = toolchain([], [action_config("c", tools = [tool(path = "gcc")])], tools_directory = "")
eq(cc_common.get_tool_for_action(feature_configuration = tc3.configure_features(requested_features = []), action_name = "c"), "gcc")
"#);
    let toolchain = r#"tc = toolchain([feature("a", env_sets = [env_set(env_entries = [env_entry("K", "1")]), env_set(env_entries = [env_entry("K", "2")])])], [action_config("c", tools = [tool(path = "t", with_features = [with_feature_set(features = ["a"])])])])
fc = tc.configure_features(requested_features = ["a"])
fc0 = tc.configure_features(requested_features = [])
"#;
    for (call, message) in [
        (
            r#"cc_common.get_tool_for_action(feature_configuration = fc, action_name = "x")"#,
            "Action x does not have an enabled configuration in the toolchain.",
        ),
        (
            r#"cc_common.get_tool_for_action(feature_configuration = fc0, action_name = "c")"#,
            "Matching tool for action c not found for given feature configuration",
        ),
        (
            r#"cc_common.get_execution_requirements(feature_configuration = fc, action_name = "x")"#,
            "Action x does not have an enabled configuration in the toolchain. (Bazel 9.3 fails with an internal IllegalArgumentException)",
        ),
        (
            r#"cc_common.get_environment_variables(feature_configuration = fc, action_name = "c", variables = cc_common.empty_variables())"#,
            "Multiple entries with same key: K=2 and K=1",
        ),
        (
            r#"cc_common.get_tool_for_action(feature_configuration = 1, action_name = "c")"#,
            "in call to get_tool_for_action(), parameter 'feature_configuration' got value of type 'int', want 'FeatureConfiguration'",
        ),
        (
            r#"cc_common.get_tool_for_action(feature_configuration = fc, action_name = None)"#,
            "in call to get_tool_for_action(), parameter 'action_name' got value of type 'NoneType', want 'string'",
        ),
        (
            r#"cc_common.get_memory_inefficient_command_line(feature_configuration = fc, action_name = "c", variables = {})"#,
            "in call to get_memory_inefficient_command_line(), parameter 'variables' got value of type 'dict', want 'Variables'",
        ),
        (
            r#"cc_common.get_tool_for_action(feature_configuration = fc)"#,
            "_get_tool_for_action() missing 1 required keyword-only argument: action_name",
        ),
        (
            r#"cc_common.get_memory_inefficient_command_line(feature_configuration = fc, action_name = "c", variables = cc_common.empty_variables()).append("x")"#,
            "frozen",
        ),
    ] {
        err(&format!("{toolchain}{call}\n"), message);
    }
}

#[test]
fn private_api_checks_follow_the_default_and_builtins_allowlists() {
    let toolchain = format!(
        "{PRELUDE}tc = toolchain([feature(\"a\")], [action_config(\"c\", tools = [tool(path = \"t\")])])\nfc = tc.configure_features(requested_features = [\"a\"])\n"
    );
    let helper = crate::language_modules::tests::run(FILE, &toolchain).unwrap();
    for (file, call, message) in [
        (
            "@@other+//pkg:x.bzl",
            "fc.is_enabled('a')",
            "file '@@other+//pkg:x.bzl' cannot use private API",
        ),
        (
            "@@other+//pkg:x.bzl",
            "tc.configure_features(requested_features = [])",
            "cannot use private API",
        ),
        (
            "@@other+//pkg:x.bzl",
            "tc.default_features_and_action_configs()",
            "cannot use private API",
        ),
        (
            "//pkg:x.bzl",
            "cc_common.get_tool_requirement_for_action(feature_configuration = fc, action_name = 'c')",
            "file '//pkg:x.bzl' cannot use private API",
        ),
    ] {
        let error = eval_with(&helper, file, call).unwrap_err();
        assert!(error.contains(message), "{call}: {error}");
    }
    for (file, call) in [
        ("//tools/build_defs/cc:probe.bzl", "fc.is_requested('a')"),
        (
            "//tools/build_defs/cc/sub:probe.bzl",
            "cc_common.get_tool_requirement_for_action(feature_configuration = fc, action_name = 'c')",
        ),
        (
            "@@other+//pkg:x.bzl",
            "cc_common.get_tool_for_action(feature_configuration = fc, action_name = 'c')",
        ),
    ] {
        eval_with(&helper, file, call).unwrap_or_else(|e| panic!("{call}: {e}"));
    }
}

/// Evaluate `call` in `file` with the frozen `tc`/`fc` of `helper` in scope.
fn eval_with(
    helper: &starlark::environment::FrozenModule,
    file: &str,
    call: &str,
) -> Result<(), String> {
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    Module::with_temp_heap(|module| {
        for name in ["tc", "fc"] {
            let owned = helper.get(name).unwrap();
            module.set(name, module.heap().access_owned_frozen_value(&owned));
        }
        let ast = AstModule::parse(file, format!("x = {call}\n"), &Dialect::Standard)
            .map_err(|e| e.to_string())?;
        Evaluator::new(&module)
            .eval_module(ast, &crate::package::bzl_globals())
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

#[test]
fn variables_equality_and_hashing_follow_map_and_single_variables() {
    ok(r#"
e = cc_common.empty_variables()
v1 = cc_internal.cc_toolchain_variables(vars = {"a": "x"})
v2 = cc_internal.cc_toolchain_variables(vars = {"a": "x"})
comb0 = cc_internal.combine_cc_toolchain_variables(v1)
eq([v1 == v1, v1 == v2, v1 != v2, e == cc_common.empty_variables(), e == cc_internal.cc_toolchain_variables(vars = {})], [True, True, False, True, True])
eq([comb0 == v1, comb0 == cc_internal.combine_cc_toolchain_variables(v1), v1 == cc_internal.cc_toolchain_variables(vars = {"a": "y"}), v2 in [v1]], [False, True, False, True])
# The empty singleton is one parent instance; distinct parents never compare equal.
s1 = cc_internal.combine_cc_toolchain_variables(cc_common.empty_variables(), v1)
s2 = cc_internal.combine_cc_toolchain_variables(cc_common.empty_variables(), v2)
eq([s1 == s2, s1 == cc_internal.combine_cc_toolchain_variables(v1, v2), s1 == v1], [True, False, False])
eq(cc_internal.combine_cc_toolchain_variables(v1, v2) == cc_internal.combine_cc_toolchain_variables(v2, v1), False)
# Lists compare by value, as StarlarkList.equals does.
eq(cc_internal.cc_toolchain_variables(vars = {"l": ["a"]}) == cc_internal.cc_toolchain_variables(vars = {"l": ["a"]}), True)
d = {v1: 1, e: 2}
eq([len(d), d[v2], d[cc_common.empty_variables()]], [2, 1, 2])
"#);
}

#[test]
fn dotted_lookup_cache_is_per_variables_instance() {
    let setup = r#"
avail = flag_group(flags = ["avail"], expand_if_available = "s.x")
use = flag_group(flags = ["%{s.x}"])
tc = toolchain([feature("a", flag_sets = [flag_set(flag_groups = [avail])]), feature("b", flag_sets = [flag_set(flag_groups = [use])]), feature("c", flag_sets = [flag_set(flag_groups = [avail, use])])])
v = cc_internal.cc_toolchain_variables(vars = {"s": struct(y = "1")})
"#;
    // Without a prior availability check, the specific message.
    err(
        &format!(
            "{setup}cmd(tc.configure_features(requested_features = [\"b\"]), {{\"s\": struct(y = \"1\")}})\n"
        ),
        "Invalid toolchain configuration: Cannot expand variable 's.x': structure s doesn't have a field named 'x'",
    );
    // A failed availability check first caches "not found" for the name.
    err(
        &format!(
            "{setup}cc_common.get_memory_inefficient_command_line(feature_configuration = tc.configure_features(requested_features = [\"c\"]), action_name = \"c\", variables = v)\n"
        ),
        "Invalid toolchain configuration: Cannot find variable named 's.x'.",
    );
    // The cache belongs to the Variables instance and outlives one call.
    err(
        &format!(
            "{setup}cc_common.get_memory_inefficient_command_line(feature_configuration = tc.configure_features(requested_features = [\"a\"]), action_name = \"c\", variables = v)\ncc_common.get_memory_inefficient_command_line(feature_configuration = tc.configure_features(requested_features = [\"b\"]), action_name = \"c\", variables = v)\n"
        ),
        "Invalid toolchain configuration: Cannot find variable named 's.x'.",
    );
    // A found dotted name is cached as found.
    ok(&format!(
        "{setup}v2 = cc_internal.cc_toolchain_variables(vars = {{\"s\": struct(x = \"X\")}})
fc = tc.configure_features(requested_features = [\"c\"])
eq(cc_common.get_memory_inefficient_command_line(feature_configuration = fc, action_name = \"c\", variables = v2), [\"avail\", \"X\"])
eq(cc_common.get_memory_inefficient_command_line(feature_configuration = fc, action_name = \"c\", variables = v2), [\"avail\", \"X\"])
"
    ));
}

#[test]
fn duplicate_environment_keys_are_reported_after_all_expansions() {
    err(
        r#"tc = toolchain([feature("a", env_sets = [env_set(env_entries = [env_entry("K", "1"), env_entry("K", "2"), env_entry("M", "%{missing}")])])])
cc_common.get_environment_variables(feature_configuration = tc.configure_features(requested_features = ["a"]), action_name = "c", variables = cc_common.empty_variables())
"#,
        "Invalid toolchain configuration: Cannot find variable named 'missing'.",
    );
    err(
        r#"tc = toolchain([feature("a", env_sets = [env_set(env_entries = [env_entry("K", "1"), env_entry("J", "j"), env_entry("K", "2"), env_entry("K", "3")])])])
cc_common.get_environment_variables(feature_configuration = tc.configure_features(requested_features = ["a"]), action_name = "c", variables = cc_common.empty_variables())
"#,
        "Multiple entries with same key: K=2 and K=1 (Bazel 9.3 fails with an internal IllegalArgumentException)",
    );
}

#[test]
fn provider_instances_report_their_creation_call_site() {
    // Plain struct() values have no recorded location.
    err(
        r#"toolchain([feature("Bad")])"#,
        "in struct instantiated at <unknown>: A feature's name",
    );
    err(
        "FeatureInfo = provider(fields = [\"name\", \"enabled\", \"type_name\"])\n\
         def make():\n    return FeatureInfo(\n        name = \"Bad\", enabled = True, type_name = \"feature\")\n\
         toolchain([make()])\n",
        &format!(
            "in FeatureInfo instantiated at {FILE}:{}:23: A feature's name",
            PRELUDE.lines().count() + 3
        ),
    );
}

// ---------------------------------------------------------------------------
// FeatureSelection: the native worklist against a literal port of Bazel's
// recursive algorithm (FeatureSelection.java:145-300).

use super::{ToolchainData, build_graph};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

fn graph(implies: &[Vec<usize>], requires: &[Vec<Vec<usize>>]) -> Arc<ToolchainData> {
    let name = |i: &usize| format!("f{i}");
    let features = implies
        .iter()
        .zip(requires)
        .enumerate()
        .map(|(i, (implied, required))| super::Feature {
            name: name(&i),
            flag_sets: Vec::new(),
            env_sets: Vec::new(),
            enabled: false,
            requires: required
                .iter()
                .map(|set| set.iter().map(name).collect())
                .collect(),
            implies: implied.iter().map(name).collect(),
            provides: Vec::new(),
        })
        .collect();
    Arc::new(build_graph(features, Vec::new(), "", || Ok(())).unwrap())
}

/// FeatureSelection.run, enableAllImpliedBy, disableUnsupportedActivatables,
/// checkActivatable and isSatisfied, line by line. `reverse` varies the
/// HashSet iteration order of the initial queue.
fn bazel_select(t: &ToolchainData, requested: &[usize], reverse: bool) -> Vec<bool> {
    struct Selection<'a> {
        t: &'a ToolchainData,
        requested: HashSet<usize>,
        enabled: HashSet<usize>,
    }
    impl Selection<'_> {
        fn enable_all_implied_by(&mut self, s: usize) {
            if self.enabled.contains(&s) {
                return;
            }
            self.enabled.insert(s);
            for implied in self.t.implies[s].clone() {
                self.enable_all_implied_by(implied);
            }
        }
        fn check_activatable(&mut self, s: usize) {
            if !self.enabled.contains(&s) || self.is_satisfied(s) {
                return;
            }
            self.enabled.remove(&s);
            for x in self.t.implied_by[s].clone() {
                self.check_activatable(x);
            }
            for x in self.t.required_by[s].clone() {
                self.check_activatable(x);
            }
            for x in self.t.implies[s].clone() {
                self.check_activatable(x);
            }
        }
        fn is_satisfied(&self, s: usize) -> bool {
            (self.requested.contains(&s)
                || self.t.implied_by[s]
                    .iter()
                    .any(|x| self.enabled.contains(x)))
                && self.t.implies[s].iter().all(|x| self.enabled.contains(x))
                && (self.t.requires[s].is_empty()
                    || self.t.requires[s]
                        .iter()
                        .any(|all_of| all_of.iter().all(|x| self.enabled.contains(x))))
        }
    }
    let mut selection = Selection {
        t,
        requested: requested.iter().copied().collect(),
        enabled: HashSet::new(),
    };
    for s in requested {
        selection.enable_all_implied_by(*s);
    }
    let mut initial: Vec<usize> = selection.enabled.iter().copied().collect();
    initial.sort_unstable();
    if reverse {
        initial.reverse();
    }
    let mut queue: VecDeque<usize> = initial.into();
    while let Some(s) = queue.pop_front() {
        selection.check_activatable(s);
    }
    (0..t.names.len())
        .map(|i| selection.enabled.contains(&i))
        .collect()
}

fn subsets(n: usize) -> Vec<Vec<usize>> {
    (0..1usize << n)
        .map(|mask| (0..n).filter(|i| mask & (1 << i) != 0).collect())
        .collect()
}

fn compare(implies: &[Vec<usize>], requires: &[Vec<Vec<usize>>]) -> usize {
    let t = graph(implies, requires);
    let mut checked = 0;
    for requested in subsets(implies.len()) {
        let names: Vec<String> = requested.iter().map(|i| format!("f{i}")).collect();
        let native = t.select(names).unwrap();
        let native: Vec<bool> = (0..t.names.len())
            .map(|i| native.enabled_set.contains(&t.names[i]))
            .collect();
        for reverse in [false, true] {
            assert_eq!(
                native,
                bazel_select(&t, &requested, reverse),
                "implies {implies:?} requires {requires:?} requested {requested:?}"
            );
        }
        checked += 1;
    }
    checked
}

/// Every combination of each feature's `implies` set and `requires` list,
/// with every requested subset, for n <= 3 features (n <= 2: any `implies`
/// subset including itself and up to two `requires` sets; n = 3: `implies`
/// over the other features and none or one `requires` set); n = 4: 20000
/// deterministic pseudo-random graphs (any `implies` subset, up to two
/// `requires` sets) with every requested subset.
#[test]
fn worklist_selection_matches_bazels_recursive_selection() {
    let mut checked = 0;
    for n in 1..=3usize {
        let sets = subsets(n);
        let implies_options: Vec<Vec<usize>> = if n <= 2 {
            sets.clone()
        } else {
            sets.iter().filter(|s| !s.contains(&0)).cloned().collect()
        };
        let mut requires_options: Vec<Vec<Vec<usize>>> = vec![Vec::new()];
        requires_options.extend(sets.iter().map(|s| vec![s.clone()]));
        if n <= 2 {
            for a in &sets {
                for b in &sets {
                    requires_options.push(vec![a.clone(), b.clone()]);
                }
            }
        }
        let per_feature = implies_options.len() * requires_options.len();
        let total = per_feature.pow(n as u32);
        for mut code in 0..total {
            let mut implies = Vec::with_capacity(n);
            let mut requires = Vec::with_capacity(n);
            for feature in 0..n {
                let choice = code % per_feature;
                code /= per_feature;
                // For n = 3 the options exclude feature 0; relabel so each
                // feature's implies ranges over the *other* features.
                let implied = implies_options[choice % implies_options.len()]
                    .iter()
                    .map(|i| if n <= 2 { *i } else { (i + feature) % n })
                    .collect();
                implies.push(implied);
                requires.push(requires_options[choice / implies_options.len()].clone());
            }
            checked += compare(&implies, &requires);
        }
    }
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    for _ in 0..20_000 {
        let implies: Vec<Vec<usize>> = (0..4)
            .map(|_| {
                let mask = next(16) as usize;
                (0..4).filter(|i| mask & (1 << i) != 0).collect()
            })
            .collect();
        let requires: Vec<Vec<Vec<usize>>> = (0..4)
            .map(|_| {
                (0..next(3))
                    .map(|_| {
                        let mask = next(16) as usize;
                        (0..4).filter(|i| mask & (1 << i) != 0).collect()
                    })
                    .collect()
            })
            .collect();
        checked += compare(&implies, &requires);
    }
    assert!(checked > 600_000, "{checked}");
}
