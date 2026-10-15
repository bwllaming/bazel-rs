//! `cc_common` / `java_common` injected globals and the private `cc_internal`
//! object (builtins_bzl/common/cc/cc_common_bazel.bzl,
//! builtins_bzl/common/java/java_common.bzl, CcStarlarkInternal.java).
//!
//! Only `.bzl`-initialization behaviour is implemented: the private-API
//! allowlist check of `internal_DO_NOT_USE()` (BuiltinRestriction.java), the
//! `cc_internal` attribute surface, and the no-artifact
//! `cc_internal.create_header_info()` used by rules_cc's empty CcInfo
//! constants. C++/Java analysis, actions and toolchains are unsupported and
//! fail explicitly.

use crate::label::Label;
use crate::language_modules::{
    Behavior, BuiltinMethod, CcInternal, Domain, bind, builtins_function, failure, param, sequence,
    sorted_struct, type_error,
};
use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    FrozenValue, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// `_internal_exports` in cc_common_bazel.bzl.
const CC_ALLOWLIST: [(&str, &str); 2] =
    [("", "third_party/bazel_rules/rules_cc"), ("rules_cc", "")];
/// `_internal_exports` in java_common.bzl.
const JAVA_ALLOWLIST: [(&str, &str); 4] = [
    ("", "bazel_internal/test_rules"),
    (
        "",
        "javatests/com/google/devtools/grok/kythe/analyzers/build/testdata/pkg",
    ),
    ("", "third_party/bazel_rules/rules_java"),
    ("rules_java", ""),
];

/// cc_common_bazel.bzl `cc_common` (14 fields): these 12 delegate to
/// `_builtins.internal.cc_common` and are analysis-time.
macro_rules! cc_functions {
    ($($field:literal),* $(,)?) => {
        const CC_COMMON_FUNCTIONS: &[(&str, &str, &str)] =
            &[$(($field, concat!("_", $field), concat!("cc_common.", $field))),*];
    };
}
cc_functions!(
    "action_is_enabled",
    "add_go_exec_groups_to_binary_rules",
    "check_experimental_cc_shared_library",
    "empty_variables",
    "get_environment_variables",
    "get_execution_requirements",
    "get_memory_inefficient_command_line",
    "get_tool_for_action",
    "get_tool_requirement_for_action",
    "implementation_deps_allowed_by_allowlist",
    "incompatible_disable_objc_library_transition",
    "legacy_cc_flags_make_variable_do_not_use",
);

/// java_common.bzl `_internal_exports()` struct: (field, Java method name).
const JAVA_INTERNAL_METHODS: [(&str, &str); 11] = [
    (
        "check_java_toolchain_is_declared_on_rule",
        "_check_java_toolchain_is_declared_on_rule",
    ),
    ("check_provider_instances", "check_provider_instances"),
    ("collect_native_deps_dirs", "collect_native_deps_dirs"),
    ("create_compilation_action", "create_compilation_action"),
    (
        "create_header_compilation_action",
        "create_header_compilation_action",
    ),
    ("expand_java_opts", "expand_java_opts"),
    (
        "get_runtime_classpath_for_archive",
        "get_runtime_classpath_for_archive",
    ),
    ("google_legacy_api_enabled", "_google_legacy_api_enabled"),
    (
        "incompatible_disable_non_executable_java_binary",
        "incompatible_disable_non_executable_java_binary",
    ),
    (
        "incompatible_java_info_merge_runtime_module_flags",
        "_incompatible_java_info_merge_runtime_module_flags",
    ),
    ("target_kind", "target_kind"),
];

static NEXT_HEADER_INFO: AtomicU64 = AtomicU64::new(1);

#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// CcCompilationContext.HeaderInfo: immutable, identity-compared,
    /// unannotated (type is the Java simple class name).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct HeaderInfo {
        pub id: u64,
    }
}
pub(crate) use owned::HeaderInfo;
starlark_simple_value!(HeaderInfo);

impl fmt::Display for HeaderInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "<unknown object com.google.devtools.build.lib.rules.cpp.CcCompilationContext$HeaderInfo>",
        )
    }
}

/// HeaderInfo's structField getters (CcCompilationContext.java), sorted as
/// `dir()` lists them. Only the artifact-free form exists natively: the
/// nullable artifact getters return None (allowReturnNones) and the list
/// getters `StarlarkList.immutableCopyOf` of an empty list.
const HEADER_INFO_FIELDS: [(&str, bool); 8] = [
    ("header_module", false),
    ("modular_private_headers", true),
    ("modular_public_headers", true),
    ("pic_header_module", false),
    ("separate_module", false),
    ("separate_module_headers", true),
    ("separate_pic_module", false),
    ("textual_headers", true),
];

#[starlark_value(type = "HeaderInfo")]
impl<'v> StarlarkValue<'v> for HeaderInfo {
    fn get_attr(&self, name: &str, _heap: starlark::values::Heap<'v>) -> Option<Value<'v>> {
        let (_, list) = HEADER_INFO_FIELDS
            .iter()
            .find(|(field, _)| *field == name)?;
        Some(if *list {
            FrozenValue::new_empty_list().to_value()
        } else {
            Value::new_none()
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        HEADER_INFO_FIELDS
            .iter()
            .map(|(field, _)| (*field).to_owned())
            .collect()
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| other.id == self.id))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        "HeaderInfo".hash(hasher);
        self.id.hash(hasher);
        Ok(())
    }
}

pub(crate) fn install(builder: &mut GlobalsBuilder) {
    let (cc_common, java_common) = {
        let heap = builder.frozen_heap();
        const CC_MODULE: &str = "common/cc/cc_common_bazel.bzl";
        let mut fields: Vec<(&'static str, FrozenValue)> = CC_COMMON_FUNCTIONS
            .iter()
            .map(|(field, def, qualified)| {
                (
                    *field,
                    builtins_function(
                        heap,
                        def,
                        CC_MODULE,
                        qualified,
                        Behavior::Unimplemented(Domain::Cc),
                    ),
                )
            })
            .collect();
        fields.push((
            "internal_DO_NOT_USE",
            builtins_function(
                heap,
                "_internal_exports",
                CC_MODULE,
                "cc_common.internal_DO_NOT_USE",
                Behavior::CcInternalExports,
            ),
        ));
        // CcModuleApi.compilerFlagExists: a void structField, i.e. None.
        fields.push((
            "do_not_use_tools_cpp_compiler_present",
            FrozenValue::new_none(),
        ));
        let cc_common = sorted_struct(heap, fields);
        let java_common = sorted_struct(
            heap,
            vec![(
                "internal_DO_NOT_USE",
                builtins_function(
                    heap,
                    "_internal_exports",
                    "common/java/java_common.bzl",
                    "java_common.internal_DO_NOT_USE",
                    Behavior::JavaInternalExports,
                ),
            )],
        );
        (cc_common, java_common)
    };
    builder.set("cc_common", cc_common);
    builder.set("java_common", java_common);
}

/// The `.bzl` module whose code is `depth` Starlark frames below the native
/// callee (0 = the immediate caller). None mirrors Bazel's null module.
fn module_at(eval: &Evaluator<'_, '_, '_>, depth: usize) -> Option<Label> {
    let location = eval.call_stack_nth_location(depth)?;
    // parse_canonical accepts only `//...` and `@@...` module file names.
    if let Ok(label) = Label::parse_canonical(location.filename()) {
        return Some(label);
    }
    eval.extra
        .and_then(|extra| extra.downcast_ref::<crate::loading::ExtensionContext>())
        .map(|context| context.label.clone())
}

/// BuiltinRestriction.AllowlistEntry.allows (reposMatch + package prefix).
pub(crate) fn allowlist_allows(
    label: &Label,
    main_mapping_target: impl Fn(&str) -> Option<String>,
    apparent: &str,
    package_prefix: &str,
) -> bool {
    let repos_match = if label.repository.is_empty() {
        apparent.is_empty() || main_mapping_target(apparent).is_some_and(|c| c.is_empty())
    } else if label.repository == "bazel_tools" {
        apparent == "bazel_tools"
    } else {
        label.repository.starts_with(&format!("{apparent}+"))
    };
    let package_matches = package_prefix.is_empty()
        || label.package == package_prefix
        || label
            .package
            .strip_prefix(package_prefix)
            .is_some_and(|rest| rest.starts_with('/'));
    repos_match && package_matches
}

/// BuiltinRestriction.failIfModuleOutsideAllowlist for the module `depth`
/// Starlark frames below the native callee.
fn check_private_api(
    eval: &Evaluator<'_, '_, '_>,
    depth: usize,
    allowlist: &[(String, String)],
) -> starlark::Result<()> {
    let Some(label) = module_at(eval, depth) else {
        // CcStarlarkInternal.checkPrivateApi: no module means no restriction.
        return Ok(());
    };
    if label.repository == "_builtins" {
        return Ok(());
    }
    let repositories = crate::loading::repositories(eval);
    let main_mapping_target = |apparent: &str| {
        repositories
            .as_ref()
            .and_then(|repositories| repositories.mapping("").ok())
            .and_then(|mapping| mapping.get(apparent).cloned())
    };
    if allowlist
        .iter()
        .any(|(apparent, prefix)| allowlist_allows(&label, main_mapping_target, apparent, prefix))
    {
        return Ok(());
    }
    Err(failure(format!("file '{label}' cannot use private API")))
}

fn owned_allowlist(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(repo, prefix)| ((*repo).to_owned(), (*prefix).to_owned()))
        .collect()
}

/// `cc_common.internal_DO_NOT_USE()`: the Starlark wrapper frame Bazel skips
/// with depth=1 does not exist natively, so the caller is frame 0.
pub(crate) fn internal_exports<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    check_private_api(eval, 0, &owned_allowlist(&CC_ALLOWLIST))?;
    Ok(eval.heap().alloc(CcInternal))
}

/// `java_common.internal_DO_NOT_USE()`.
pub(crate) fn java_internal_exports<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    check_private_api(eval, 0, &owned_allowlist(&JAVA_ALLOWLIST))?;
    let heap = eval.heap();
    Ok(heap.alloc(AllocStruct(JAVA_INTERNAL_METHODS.iter().map(
        |(field, method)| {
            (
                *field,
                heap.alloc(BuiltinMethod {
                    receiver: "java_common",
                    name: method,
                    qualified_owner: "java_common",
                    behavior: Behavior::Unimplemented(Domain::Java),
                }),
            )
        },
    ))))
}

/// `cc_internal.check_private_api(allowlist=[(repo, prefix), ...], depth=1)`.
pub(crate) fn check_private_api_call<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    const F: &str = "check_private_api";
    let bound = bind(
        F,
        &[
            param("allowlist", false, true, true),
            param("depth", false, true, false),
        ],
        args,
        eval.heap(),
    )?;
    let mut allowlist = Vec::new();
    for entry in sequence(F, "allowlist", bound[0].expect("required"))? {
        let tuple = TupleRef::from_value(entry).ok_or_else(|| {
            failure(format!(
                "at index {} of allowlist, got element of type {}, want tuple",
                allowlist.len(),
                entry.get_type()
            ))
        })?;
        let parts: Vec<&str> = tuple.iter().filter_map(|v| v.unpack_str()).collect();
        if tuple.len() != 2 || parts.len() != 2 {
            return Err(failure(
                "check_private_api() allowlist entries must be (repository, package prefix) string pairs"
                    .to_owned(),
            ));
        }
        allowlist.push((parts[0].to_owned(), parts[1].to_owned()));
    }
    let depth = match bound[1] {
        None => 1,
        Some(value) => value
            .unpack_i32()
            .and_then(|depth| usize::try_from(depth).ok())
            .ok_or_else(|| type_error(F, "depth", value, "non-negative int"))?,
    };
    check_private_api(eval, depth, &allowlist)?;
    Ok(Value::new_none())
}

/// CcStarlarkInternal.createHeaderInfo. Only the artifact-free form (all
/// defaults, as in rules_cc's `_EMPTY_COMPILATION_CONTEXT`) is supported.
/// `cc_internal.freeze(value)` (CcStarlarkInternal.java:949-960): a dict becomes
/// an immutable dict copy, any other Java `Iterable` (list, tuple, range, set)
/// an immutable list copy, and everything else (e.g. depset, struct) is returned
/// unchanged. starlark-rust has no immutable-but-unfrozen containers, so the
/// copies here are fresh values that become immutable only when their module
/// freezes; mutating a `freeze()` result during analysis is a known divergence.
pub(crate) fn freeze<'v>(
    value: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Value<'v>> {
    use starlark::values::dict::{AllocDict, DictRef};
    use starlark::values::list::AllocList;
    if let Some(dict) = DictRef::from_value(value) {
        let entries: Vec<(Value<'v>, Value<'v>)> = dict.iter().collect();
        return Ok(heap.alloc(AllocDict(entries)));
    }
    if matches!(value.get_type(), "list" | "tuple" | "range" | "set") {
        let items: Vec<Value<'v>> = value.iterate(heap)?.collect();
        return Ok(heap.alloc(AllocList(items)));
    }
    Ok(value)
}

pub(crate) fn create_header_info<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    const F: &str = "create_header_info";
    const SINGLE: [&str; 4] = [
        "header_module",
        "pic_header_module",
        "separate_module",
        "separate_pic_module",
    ];
    const LISTS: [&str; 4] = [
        "modular_public_headers",
        "modular_private_headers",
        "textual_headers",
        "separate_module_headers",
    ];
    let params: Vec<_> = [
        "header_module",
        "pic_header_module",
        "modular_public_headers",
        "modular_private_headers",
        "textual_headers",
        "separate_module_headers",
        "separate_module",
        "separate_pic_module",
    ]
    .into_iter()
    .map(|name| param(name, false, true, false))
    .collect();
    let bound = bind(F, &params, args, eval.heap())?;
    let mut with_artifacts = false;
    for (p, value) in params.iter().zip(&bound) {
        let Some(value) = value else { continue };
        if SINGLE.contains(&p.name) {
            with_artifacts |= !value.is_none();
        } else if LISTS.contains(&p.name) {
            with_artifacts |= !sequence(F, p.name, *value)?.is_empty();
        }
    }
    if with_artifacts {
        return Err(failure(format!(
            "cc_internal.create_header_info with header artifacts {} (analysis-time C++ API)",
            crate::language_modules::NOT_IMPLEMENTED
        )));
    }
    Ok(eval.heap().alloc(HeaderInfo {
        id: NEXT_HEADER_INFO.fetch_add(1, Ordering::Relaxed),
    }))
}

#[cfg(test)]
mod tests {
    use crate::language_modules::tests::{fails, pass, run};
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    /// rules_cc cc/private/cc_internal.bzl:17, verbatim.
    const CC_INTERNAL_BZL: &str = "cc_internal = cc_common.internal_DO_NOT_USE() if hasattr(cc_common, \"internal_DO_NOT_USE\") else struct()\n";

    #[test]
    fn cc_common_struct_shape_and_explicit_failures() {
        pass(
            "@@rules_cc+//cc/private/rules_impl:native_cc_common.bzl",
            r#"
native_cc_common = cc_common
(type(cc_common) == "struct") or fail("type")
(dir(cc_common) == ["action_is_enabled", "add_go_exec_groups_to_binary_rules", "check_experimental_cc_shared_library", "do_not_use_tools_cpp_compiler_present", "empty_variables", "get_environment_variables", "get_execution_requirements", "get_memory_inefficient_command_line", "get_tool_for_action", "get_tool_requirement_for_action", "implementation_deps_allowed_by_allowlist", "incompatible_disable_objc_library_transition", "internal_DO_NOT_USE", "legacy_cc_flags_make_variable_do_not_use"]) or fail("dir")
(hasattr(cc_common, "internal_DO_NOT_USE") and not hasattr(cc_common, "compile") and not hasattr(cc_common, "CcToolchainInfo")) or fail("hasattr")
(cc_common.do_not_use_tools_cpp_compiler_present == None) or fail("compiler_present")
(type(cc_common.get_tool_for_action) == "function" and type(cc_common.internal_DO_NOT_USE) == "function") or fail("types")
(repr(cc_common.internal_DO_NOT_USE) == "<function _internal_exports from @@_builtins//:common/cc/cc_common_bazel.bzl>") or fail("repr")
(cc_common.empty_variables == native_cc_common.empty_variables) or fail("identity")
_cc_common_internal = native_cc_common
x = struct(do_not_use_tools_cpp_compiler_present = _cc_common_internal.do_not_use_tools_cpp_compiler_present)
"#,
        );
        for (call, message) in [
            (
                "cc_common.get_tool_for_action(feature_configuration = None, action_name = 'c')",
                "cc_common.get_tool_for_action is not implemented in the native profile (analysis-time C++ API)",
            ),
            (
                "cc_common.empty_variables()",
                "cc_common.empty_variables is not implemented in the native profile",
            ),
            (
                "cc_common.check_experimental_cc_shared_library()",
                "cc_common.check_experimental_cc_shared_library is not implemented",
            ),
            (
                "cc_common.internal_DO_NOT_USE(1)",
                "_internal_exports() does not accept positional arguments, but got 1",
            ),
        ] {
            fails("@@rules_cc+//cc:x.bzl", call, message);
        }
    }

    #[test]
    fn internal_do_not_use_private_api_allowlist() {
        for allowed in [
            "@@rules_cc+//cc/private:cc_internal.bzl",
            "@@rules_cc+//:root.bzl",
            "@@rules_cc+0.2.0//cc:x.bzl",
            "@@rules_cc++ext+repo//cc:x.bzl",
            "//third_party/bazel_rules/rules_cc:x.bzl",
            "//third_party/bazel_rules/rules_cc/cc/private:x.bzl",
        ] {
            let module = run(
                allowed,
                &format!("{CC_INTERNAL_BZL}t = type(cc_internal)\n"),
            )
            .unwrap_or_else(|e| panic!("{allowed}: {e}"));
            assert_eq!(
                module.get("t").unwrap().value().unpack_str(),
                Some("cc_internal")
            );
        }
        for denied in [
            "@@rules_go+//go/private:x.bzl",
            "@@my_rules_cc+//cc:x.bzl",
            "@@rules_cc//cc:x.bzl",
            "@@bazel_tools//tools/cpp:x.bzl",
            "//cc/private:cc_internal.bzl",
            "//third_party/bazel_rules/rules_ccx:x.bzl",
            "@@other+//third_party/bazel_rules/rules_cc:x.bzl",
        ] {
            fails(
                denied,
                CC_INTERNAL_BZL,
                &format!("file '{denied}' cannot use private API"),
            );
        }
    }

    #[test]
    fn private_api_uses_the_calling_function_module_not_the_entry_file() {
        // Bazel checks the module of the Starlark function that called
        // internal_DO_NOT_USE (depth=1 below `_internal_exports`).
        let globals = crate::package::bzl_globals();
        let helper = run(
            "@@rules_cc+//cc/private:helper.bzl",
            "def get():\n    return cc_common.internal_DO_NOT_USE()\ndef direct(f):\n    return f()\n",
        )
        .unwrap();
        Module::with_temp_heap(|module| {
            for name in ["get", "direct"] {
                let owned = helper.get(name).unwrap();
                module.set(name, module.heap().access_owned_frozen_value(&owned));
            }
            let ast = AstModule::parse(
                "@@rules_go+//go:consumer.bzl",
                "allowed = type(get())\n".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            assert_eq!(
                module.get("allowed").unwrap().unpack_str(),
                Some("cc_internal")
            );
            let ast = AstModule::parse(
                "@@rules_go+//go:consumer2.bzl",
                "denied = direct(lambda: cc_common.internal_DO_NOT_USE())\n".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            let error = Evaluator::new(&module)
                .eval_module(ast, &globals)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("file '@@rules_go+//go:consumer2.bzl' cannot use private API"),
                "{error}"
            );
        });
    }

    #[test]
    fn cc_internal_surface_and_check_private_api() {
        let source = format!(
            "{CC_INTERNAL_BZL}{}",
            r#"
(dir(cc_internal) == ["absolute_symlink", "actions2ctx_cheat", "aspect_class", "cc_toolchain_features", "cc_toolchain_variables", "check_private_api", "check_toplevel", "collect_per_file_lto_backend_opts", "combine_cc_toolchain_variables", "compute_output_name_prefix_dir", "create_cc_compile_action", "create_cc_compile_action_template", "create_header_info", "create_header_info_with_deps", "create_lto_backend_action", "create_lto_backend_action_template", "declare_compile_output_file", "declare_other_output_file", "dynamic_library_soname", "dynamic_library_symlink", "dynamic_library_symlink2", "exec_os", "expand_and_tokenize", "freeze", "get_artifact_name_extension_for_category", "get_artifact_name_for_category", "get_link_args", "intern_seq", "intern_string_sequence_variable_value", "is_package_headers_checking_mode_set", "is_package_headers_checking_mode_set_for_aspect", "is_tree_artifact", "package_headers_checking_mode", "package_headers_checking_mode_for_aspect", "per_file_copts", "rule_class", "solib_symlink_action", "stl_computed_default", "wrap_link_actions"]) or fail("dir")
(len(dir(cc_internal)) == 39 and not hasattr(cc_internal, "declare_shareable_directory")) or fail("count")
(repr(cc_internal) == "<unknown object com.google.devtools.build.lib.rules.cpp.CcStarlarkInternal>") or fail("repr")
(type(cc_internal.freeze) == "builtin_function_or_method") or fail("method type")
(repr(cc_internal.freeze) == "<built-in method freeze of cc_internal value>") or fail("method repr")
(cc_internal == cc_common.internal_DO_NOT_USE()) or fail("equality")
# Bazel: depth=1 above the top-level frame is no module, so no restriction.
cc_internal.check_private_api(allowlist = [("rules_go", "")])
cc_internal.check_private_api(allowlist = [("rules_cc", "")], depth = 0)
cc_internal.check_private_api(allowlist = [("", "x"), ("rules_cc", "cc")], depth = 0)
def nested():
    cc_internal.check_private_api(allowlist = [("rules_cc", "")], depth = 0)
nested()
_PRIVATE_STARLARKIFICATION_ALLOWLIST = [("_builtins", ""), ("rules_cc", "")]
def _cc_toolchain_variables(*, vars):
    cc_internal.check_private_api(allowlist = _PRIVATE_STARLARKIFICATION_ALLOWLIST)
    return vars
_cc_toolchain_variables(vars = 1)
# CcStarlarkInternal.freeze: dict -> dict copy, iterable -> list, else unchanged.
_src = [1, [2]]
_frozen = cc_internal.freeze(_src)
(type(_frozen) == "list" and _frozen == _src) or fail("freeze list")
(type(cc_internal.freeze((1, 2))) == "list" and cc_internal.freeze((1, 2)) == [1, 2]) or fail("freeze tuple")
(cc_internal.freeze(range(3)) == [0, 1, 2]) or fail("freeze range")
(cc_internal.freeze({"a": 1}) == {"a": 1} and type(cc_internal.freeze({"a": 1})) == "dict") or fail("freeze dict")
_d = depset([1])
(cc_internal.freeze(_d) == _d and cc_internal.freeze("s") == "s" and cc_internal.freeze(None) == None) or fail("freeze passthrough")
_src.append(3)
(_frozen == [1, [2]]) or fail("freeze copies")
EMPTY = cc_internal.freeze([])
"#
        );
        pass("@@rules_cc+//cc/private:cc_common.bzl", &source);
        for (call, message) in [
            (
                "cc_internal.freeze()",
                "missing 1 required positional argument",
            ),
            (
                "cc_internal.freeze(value = [])",
                "freeze() got named argument for positional-only parameter 'value'",
            ),
            (
                "cc_internal.create_cc_compile_action()",
                "cc_internal.create_cc_compile_action is not implemented",
            ),
            (
                "cc_internal.check_private_api(allowlist = [(\"rules_go\", \"\")], depth = 0)",
                "file '@@rules_cc+//cc/private:cc_common.bzl' cannot use private API",
            ),
            (
                "cc_internal.check_private_api(allowlist = [(\"rules_cc\", \"go\")], depth = 0)",
                "cannot use private API",
            ),
            (
                "def f():\n    cc_internal.check_private_api(allowlist = [(\"rules_go\", \"\")])\nf()",
                "cannot use private API",
            ),
            (
                "cc_internal.check_private_api([])",
                "check_private_api() got unexpected positional argument",
            ),
            (
                "cc_internal.check_private_api(allowlist = ['x'])",
                "want tuple",
            ),
            (
                "cc_internal.check_private_api(allowlist = [], depth = 'x')",
                "parameter 'depth'",
            ),
        ] {
            fails(
                "@@rules_cc+//cc/private:cc_common.bzl",
                &format!("{CC_INTERNAL_BZL}{call}\n"),
                message,
            );
        }
    }

    #[test]
    fn create_header_info_is_frozen_into_module_globals() {
        let source = format!(
            "{CC_INTERNAL_BZL}{}",
            r#"
CcCompilationContextInfo = provider(fields = ["headers", "_header_info"])
_EMPTY_COMPILATION_CONTEXT = CcCompilationContextInfo(
    headers = depset(),
    _header_info = cc_internal.create_header_info(),
)
h = cc_internal.create_header_info()
explicit = cc_internal.create_header_info(header_module = None, modular_public_headers = [], textual_headers = (), separate_module = None)
(type(h) == "HeaderInfo") or fail("type")
(dir(h) == ["header_module", "modular_private_headers", "modular_public_headers", "pic_header_module", "separate_module", "separate_module_headers", "separate_pic_module", "textual_headers"]) or fail("dir")
(h.header_module == None and h.pic_header_module == None and h.separate_module == None and h.separate_pic_module == None) or fail("artifact fields")
(h.modular_public_headers == [] and h.modular_private_headers == [] and h.textual_headers == [] and h.separate_module_headers == []) or fail("list fields")
(type(h.textual_headers) == "list" and not hasattr(h, "deps")) or fail("list type")
(repr(h) == "<unknown object com.google.devtools.build.lib.rules.cpp.CcCompilationContext$HeaderInfo>") or fail("repr")
(h == h and h != explicit and h != _EMPTY_COMPILATION_CONTEXT._header_info) or fail("identity")
({h: 1, explicit: 2}[h] == 1) or fail("hashable")
"#
        );
        let frozen = run("@@rules_cc+//cc/private:cc_info.bzl", &source).unwrap();
        // The frozen module keeps the opaque value, also inside a provider.
        Module::with_temp_heap(|module| {
            for name in ["_EMPTY_COMPILATION_CONTEXT", "h"] {
                let (owned, _) = frozen.get_any_visibility(name).unwrap();
                module.set(name, module.heap().access_owned_frozen_value(&owned));
            }
            let ast = AstModule::parse(
                "@@rules_cc+//cc/private:user.bzl",
                "t = type(_EMPTY_COMPILATION_CONTEXT._header_info)\nsame = h == h\n".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &crate::package::bzl_globals())
                .unwrap();
            assert_eq!(module.get("t").unwrap().unpack_str(), Some("HeaderInfo"));
            assert_eq!(module.get("same").unwrap().unpack_bool(), Some(true));
        });
        for (call, message) in [
            (
                "cc_internal.create_header_info().textual_headers.append('a.h')",
                // Bazel: immutable StarlarkList; native: the frozen empty list.
                "Immutable",
            ),
            (
                "cc_internal.create_header_info(textual_headers = ['a.h'])",
                "cc_internal.create_header_info with header artifacts is not implemented in the native profile",
            ),
            (
                "cc_internal.create_header_info(header_module = 1)",
                "not implemented",
            ),
            (
                "cc_internal.create_header_info(1)",
                "create_header_info() got unexpected positional argument",
            ),
            (
                "cc_internal.create_header_info(foo = 1)",
                "create_header_info() got unexpected keyword argument 'foo'",
            ),
            (
                "cc_internal.create_header_info(textual_headers = 'x')",
                "parameter 'textual_headers' got value of type 'string', want 'sequence'",
            ),
        ] {
            fails(
                "@@rules_cc+//cc/private:cc_info.bzl",
                &format!("{CC_INTERNAL_BZL}{call}\n"),
                message,
            );
        }
    }

    #[test]
    fn java_common_internal_do_not_use_allowlist() {
        pass(
            "@@rules_java+//java/private:native.bzl",
            r#"
def get_internal_java_common():
    return java_common.internal_DO_NOT_USE()
j = get_internal_java_common()
(type(j) == "struct" and len(dir(j)) == 11) or fail("fields")
(dir(j)[0] == "check_java_toolchain_is_declared_on_rule" and dir(j)[-1] == "target_kind") or fail("sorted")
(repr(j.google_legacy_api_enabled) == "<built-in method _google_legacy_api_enabled of java_common value>") or fail("repr")
"#,
        );
        fails(
            "@@rules_java+//java/private:native.bzl",
            "java_common.internal_DO_NOT_USE().target_kind(None)",
            "java_common.target_kind is not implemented in the native profile (analysis-time Java API)",
        );
        fails(
            "@@rules_cc+//cc:x.bzl",
            "java_common.internal_DO_NOT_USE()",
            "file '@@rules_cc+//cc:x.bzl' cannot use private API",
        );
    }
}
