//! Bazel 9.3 language-module `.bzl` globals: load-time shapes, explicit
//! analysis-time failures.
//!
//! Pinned Bazel injects `cc_common`, `java_common`, `apple_common` and
//! `py_internal` from `@_builtins` (`builtins_bzl/bazel/exports.bzl`) and
//! `proto_common_do_not_use` from `builtins_bzl/common/exports.bzl`, into every
//! `.bzl` environment (BUILD- and MODULE-loaded). `platform_common`,
//! `config_common`, `coverage_common`, `testing`, `exec_group` and
//! `configuration_field` are Java builtins. This module reproduces the
//! attributes visible during `.bzl` initialization (type, `dir`, `hasattr`,
//! `repr`, the pure values such as Apple platform tables) and implements the
//! load-time constructors (`config_common.toolchain_type`, `exec_group`,
//! `configuration_field`). Members that need configured targets, toolchains or
//! C++/Java/Apple analysis exist but fail explicitly when called; see
//! `docs/CC_COMMON_LOADING.md`.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    FrozenHeap, FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic,
    StarlarkValue, Value, starlark_value,
};
use std::fmt;
use std::hash::Hash;

/// Shared suffix of every explicit analysis-time failure.
pub(crate) const NOT_IMPLEMENTED: &str = "is not implemented in the native profile";

/// The analysis-time API family named in explicit failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Allocative)]
pub(crate) enum Domain {
    Cc,
    Java,
    Apple,
    Python,
    Platform,
    Config,
    Coverage,
    Testing,
}

impl Domain {
    fn describe(self) -> &'static str {
        match self {
            Domain::Cc => "analysis-time C++ API",
            Domain::Java => "analysis-time Java API",
            Domain::Apple => "analysis-time Apple API",
            Domain::Python => "analysis-time Python API",
            Domain::Platform => "analysis-time platform/toolchain API",
            Domain::Config => "analysis-time configuration API",
            Domain::Coverage => "analysis-time coverage API",
            Domain::Testing => "analysis-time testing API",
        }
    }
}

/// A Java Starlark semantics flag read by `java_common.internal_DO_NOT_USE()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Allocative)]
pub(crate) enum JavaFlag {
    /// isLegacyGoogleApiEnabled (:383-387): checkPrivateAccess, then
    /// `--experimental_google_legacy_api`.
    GoogleLegacyApi,
    /// isJavaInfoMergeRuntimeModuleFlagsEnabled (:389-395): checkPrivateAccess,
    /// then `--incompatible_java_info_merge_runtime_module_flags`.
    MergeRuntimeModuleFlags,
    /// incompatibleDisableNonExecutableJavaBinary (:397-402): no check.
    DisableNonExecutableJavaBinary,
}

/// What a native callable does when invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Allocative)]
pub(crate) enum Behavior {
    Unimplemented(Domain),
    CcInternalExports,
    JavaInternalExports,
    JavaCheckProviderInstances,
    /// The Starlark-semantics getters of JavaStarlarkCommon
    /// (rules/java/JavaStarlarkCommon.java:383-402).
    JavaSemanticsFlag(JavaFlag),
    CheckPrivateApi,
    CreateHeaderInfo,
    CreateHeaderInfoWithDeps,
    Freeze,
    AppleToolchain,
    AppleDeveloperDir,
    AppleSdkDir,
    ApplePlatformDeveloperFrameworkDir,
    /// AppleStarlarkCommon.dottedVersion; also the `apple_common_bazel`
    /// lambda wrapper (Style::Def binds `version` positionally or by name).
    DottedVersion,
    ToolchainType,
    ExecGroup,
    ConfigurationField,
    TestEnvironment,
    /// coverage_common.instrumented_files_info (crate::instrumented_files).
    InstrumentedFilesInfo,
    /// C++ toolchain features, variables and tools (`crate::cc_features`).
    CcFeatures(crate::cc_features::Op),
    /// The cc_internal analysis-time natives of the C++ compile/link surface
    /// (`crate::cc_actions`, N60).
    CcActions(&'static str),
    /// PyBuiltins.mergeRunfilesWithGeneratedInitsEmptyFilesSupplier
    /// (rules/python/PyBuiltins.java:223-250).
    PyMergeGeneratedInits,
    /// PyBuiltins.getLabelRepoRunfilesPath (rules/python/PyBuiltins.java:120-128).
    PyLabelRepoRunfilesPath,
    /// `_is_tool_configuration(ctx)` (builtins_bzl/common/python/py_internal.bzl:73-74):
    /// `ctx.configuration.is_tool_configuration()`.
    PyIsToolConfiguration,
    /// `_get_legacy_external_runfiles(*args, **kwargs)` (py_internal.bzl:58-59):
    /// `False`.
    PyLegacyExternalRunfiles,
    /// PyBuiltins.isBzlmodEnabled (rules/python/PyBuiltins.java:66-74): `True`.
    PyIsBzlmodEnabled,
    /// `_make_runfiles_respect_legacy_external_runfiles(_ctx, runfiles)`
    /// (py_internal.bzl:86-87): `runfiles`.
    PyRespectLegacyExternalRunfiles,
    /// PyBuiltins.repoMappingAction (rules/python/PyBuiltins.java:157-181):
    /// a RepoMappingManifestAction of `runfiles` to `output`.
    PyCreateRepoMappingManifest,
    /// `_is_valid_shared_library_artifact(shared_library)`
    /// (py_internal.bzl:101-131, `py_internal.cc_helper`).
    PyIsValidSharedLibraryArtifact,
    /// Pure String.matches helper; also callable during repository execution.
    PyRegexMatch,
}

/// The four Java builtin namespaces plus `cc_internal`; each has its own
/// Starlark type name, so each gets a distinct Rust value type below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Allocative)]
pub(crate) enum Namespace {
    PlatformCommon,
    ConfigCommon,
    CoverageCommon,
    Testing,
    CcInternal,
}

/// `CcStarlarkInternal` @StarlarkMethod names (CcStarlarkInternal.java), in
/// Bazel's `dir()` order (sorted). `declare_shareable_directory` belongs to the
/// nested WrappedStarlarkActionFactory and is deliberately absent.
pub(crate) const CC_INTERNAL_METHODS: [&str; 39] = [
    "absolute_symlink",
    "actions2ctx_cheat",
    "aspect_class",
    "cc_toolchain_features",
    "cc_toolchain_variables",
    "check_private_api",
    "check_toplevel",
    "collect_per_file_lto_backend_opts",
    "combine_cc_toolchain_variables",
    "compute_output_name_prefix_dir",
    "create_cc_compile_action",
    "create_cc_compile_action_template",
    "create_header_info",
    "create_header_info_with_deps",
    "create_lto_backend_action",
    "create_lto_backend_action_template",
    "declare_compile_output_file",
    "declare_other_output_file",
    "dynamic_library_soname",
    "dynamic_library_symlink",
    "dynamic_library_symlink2",
    "exec_os",
    "expand_and_tokenize",
    "freeze",
    "get_artifact_name_extension_for_category",
    "get_artifact_name_for_category",
    "get_link_args",
    "intern_seq",
    "intern_string_sequence_variable_value",
    "is_package_headers_checking_mode_set",
    "is_package_headers_checking_mode_set_for_aspect",
    "is_tree_artifact",
    "package_headers_checking_mode",
    "package_headers_checking_mode_for_aspect",
    "per_file_copts",
    "rule_class",
    "solib_symlink_action",
    "stl_computed_default",
    "wrap_link_actions",
];

impl Namespace {
    fn java_class(self) -> &'static str {
        match self {
            Namespace::PlatformCommon => {
                "com.google.devtools.build.lib.rules.platform.PlatformCommon"
            }
            Namespace::ConfigCommon => {
                "com.google.devtools.build.lib.rules.config.ConfigStarlarkCommon"
            }
            Namespace::CoverageCommon => {
                "com.google.devtools.build.lib.analysis.test.CoverageCommon"
            }
            Namespace::Testing => "com.google.devtools.build.lib.rules.test.StarlarkTestingModule",
            Namespace::CcInternal => "com.google.devtools.build.lib.rules.cpp.CcStarlarkInternal",
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            Namespace::PlatformCommon => "platform_common",
            Namespace::ConfigCommon => "config_common",
            Namespace::CoverageCommon => "coverage_common",
            Namespace::Testing => "testing",
            Namespace::CcInternal => "cc_internal",
        }
    }

    /// Bazel `dir()` sorts annotated names (Starlark.dir uses a TreeSet).
    pub(crate) fn members(self) -> &'static [&'static str] {
        match self {
            Namespace::PlatformCommon => &[
                "ConstraintSettingInfo",
                "ConstraintValueInfo",
                "PlatformInfo",
                "TemplateVariableInfo",
                "ToolchainInfo",
            ],
            Namespace::ConfigCommon => &[
                "FeatureFlagInfo",
                "config_feature_flag_transition",
                "toolchain_type",
            ],
            Namespace::CoverageCommon => &["instrumented_files_info"],
            Namespace::Testing => &["ExecutionInfo", "TestEnvironment", "analysis_test"],
            Namespace::CcInternal => &CC_INTERNAL_METHODS,
        }
    }

    fn member<'v>(self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let name: &'static str = self.members().iter().copied().find(|m| *m == name)?;
        let owner = self.type_name();
        let provider = |domain| {
            heap.alloc(BuiltinProvider {
                name,
                owner,
                starlark_defined: None,
                domain,
            })
        };
        let method = |behavior| {
            heap.alloc(BuiltinMethod {
                receiver: owner,
                name,
                qualified_owner: owner,
                behavior,
            })
        };
        Some(match (self, name) {
            (Namespace::PlatformCommon, "TemplateVariableInfo") => {
                crate::template_variable_info::alloc_constructor(heap)
            }
            (Namespace::PlatformCommon, _) => provider(Domain::Platform),
            // ConfigFeatureFlagProvider.Constructor carries its own
            // @StarlarkBuiltin(name = "FeatureFlagInfo"), so type() differs.
            (Namespace::ConfigCommon, "FeatureFlagInfo") => {
                heap.alloc(FeatureFlagInfoProvider(BuiltinProvider {
                    name,
                    owner,
                    starlark_defined: None,
                    domain: Domain::Config,
                }))
            }
            (Namespace::ConfigCommon, "toolchain_type") => method(Behavior::ToolchainType),
            (Namespace::ConfigCommon, _) => method(Behavior::Unimplemented(Domain::Config)),
            (Namespace::CoverageCommon, "instrumented_files_info") => {
                method(Behavior::InstrumentedFilesInfo)
            }
            (Namespace::CoverageCommon, _) => method(Behavior::Unimplemented(Domain::Coverage)),
            (Namespace::Testing, "ExecutionInfo") => provider(Domain::Testing),
            (Namespace::Testing, "TestEnvironment") => method(Behavior::TestEnvironment),
            (Namespace::Testing, _) => method(Behavior::Unimplemented(Domain::Testing)),
            (Namespace::CcInternal, "check_private_api") => method(Behavior::CheckPrivateApi),
            (Namespace::CcInternal, "create_header_info") => method(Behavior::CreateHeaderInfo),
            (Namespace::CcInternal, "create_header_info_with_deps") => {
                method(Behavior::CreateHeaderInfoWithDeps)
            }
            (Namespace::CcInternal, "freeze") => method(Behavior::Freeze),
            (Namespace::CcInternal, _) if crate::cc_actions::is_cc_actions_method(name) => {
                // The name is one of CC_INTERNAL_METHODS.
                method(Behavior::CcActions(
                    CC_INTERNAL_METHODS
                        .iter()
                        .find(|candidate| **candidate == name)
                        .copied()
                        .expect("checked by is_cc_actions_method"),
                ))
            }
            (Namespace::CcInternal, _)
                if crate::cc_features::Op::for_cc_internal(name).is_some() =>
            {
                method(Behavior::CcFeatures(
                    crate::cc_features::Op::for_cc_internal(name).expect("checked"),
                ))
            }
            (Namespace::CcInternal, _) => method(Behavior::Unimplemented(Domain::Cc)),
        })
    }
}

// Only derive-generated marker implementations need the exception. Every value
// below owns static or plain data and freezes unchanged without heap references.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct PlatformCommon;
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ConfigCommon;
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct CoverageCommon;
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct Testing;
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct CcInternal;

    /// A Java `@StarlarkMethod` (type `builtin_function_or_method`).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct BuiltinMethod {
        /// Starlark type of the receiver, or "" for a top-level builtin.
        #[allocative(skip)]
        pub receiver: &'static str,
        #[allocative(skip)]
        pub name: &'static str,
        /// Spelling used in explicit failures (`<owner>.<member>`).
        #[allocative(skip)]
        pub qualified_owner: &'static str,
        pub behavior: Behavior,
    }

    /// A function defined in an `@_builtins` `.bzl` (type `function`).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct BuiltinsFunction {
        /// The Starlark `def` name (or `lambda`), shown by `repr`.
        #[allocative(skip)]
        pub name: &'static str,
        /// Path within `@_builtins//:` of the defining file.
        #[allocative(skip)]
        pub module: &'static str,
        /// Public spelling used in explicit failures.
        #[allocative(skip)]
        pub qualified: &'static str,
        pub behavior: Behavior,
    }

    /// A Bazel-owned provider key (BuiltinProvider or `@_builtins` provider).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct BuiltinProvider {
        #[allocative(skip)]
        pub name: &'static str,
        #[allocative(skip)]
        pub owner: &'static str,
        /// `@_builtins` file for Starlark-defined providers (repr `<provider>`).
        #[allocative(skip)]
        pub starlark_defined: Option<&'static str>,
        pub domain: Domain,
    }

    /// `config_common.FeatureFlagInfo`: a BuiltinProvider whose Java class is
    /// annotated `@StarlarkBuiltin(name = "FeatureFlagInfo")`.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct FeatureFlagInfoProvider(pub BuiltinProvider);

    /// `config_common.toolchain_type(...)` (Java ToolchainTypeRequirement).
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub(crate) struct ToolchainType {
        pub label: String,
        pub mandatory: bool,
    }

    /// `exec_group(...)` (Java DeclaredExecGroup).
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub(crate) struct ExecGroup {
        pub toolchains: Vec<(String, bool)>,
        pub exec_compatible_with: Vec<String>,
    }

    /// `configuration_field(...)` (Java StarlarkLateBoundDefault).
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub(crate) struct LateBoundDefault {
        pub fragment: String,
        pub name: String,
    }
}
pub(crate) use owned::{
    BuiltinMethod, BuiltinProvider, BuiltinsFunction, CcInternal, ConfigCommon, CoverageCommon,
    ExecGroup, FeatureFlagInfoProvider, LateBoundDefault, PlatformCommon, Testing, ToolchainType,
};

starlark_simple_value!(PlatformCommon);
starlark_simple_value!(ConfigCommon);
starlark_simple_value!(CoverageCommon);
starlark_simple_value!(Testing);
starlark_simple_value!(CcInternal);
starlark_simple_value!(BuiltinMethod);
starlark_simple_value!(BuiltinsFunction);
starlark_simple_value!(BuiltinProvider);
starlark_simple_value!(FeatureFlagInfoProvider);
starlark_simple_value!(ToolchainType);
starlark_simple_value!(ExecGroup);
starlark_simple_value!(LateBoundDefault);

// ---------------------------------------------------------------------------
// Namespace values. Bazel's classes do not override repr, so the default
// StarlarkValue repr `<unknown object CLASS>` applies (coverage_common
// overrides it).

macro_rules! namespace_display {
    ($ty:ident, $ns:expr) => {
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if matches!($ns, Namespace::CoverageCommon) {
                    return f.write_str("<coverage_common>");
                }
                write!(f, "<unknown object {}>", $ns.java_class())
            }
        }
    };
}
namespace_display!(PlatformCommon, Namespace::PlatformCommon);
namespace_display!(ConfigCommon, Namespace::ConfigCommon);
namespace_display!(CoverageCommon, Namespace::CoverageCommon);
namespace_display!(Testing, Namespace::Testing);
namespace_display!(CcInternal, Namespace::CcInternal);

fn dir_of(namespace: Namespace) -> Vec<String> {
    namespace
        .members()
        .iter()
        .map(|m| (*m).to_owned())
        .collect()
}

#[starlark_value(type = "platform_common")]
impl<'v> StarlarkValue<'v> for PlatformCommon {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Namespace::PlatformCommon.member(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        dir_of(Namespace::PlatformCommon)
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }
}

#[starlark_value(type = "config_common")]
impl<'v> StarlarkValue<'v> for ConfigCommon {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Namespace::ConfigCommon.member(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        dir_of(Namespace::ConfigCommon)
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }
}

#[starlark_value(type = "coverage_common")]
impl<'v> StarlarkValue<'v> for CoverageCommon {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Namespace::CoverageCommon.member(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        dir_of(Namespace::CoverageCommon)
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }
}

#[starlark_value(type = "testing")]
impl<'v> StarlarkValue<'v> for Testing {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Namespace::Testing.member(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        dir_of(Namespace::Testing)
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }
}

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for CcInternal {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Namespace::CcInternal.member(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        dir_of(Namespace::CcInternal)
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }
}

// ---------------------------------------------------------------------------
// Callables and providers.

impl fmt::Display for BuiltinMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.receiver.is_empty() {
            write!(f, "<built-in function {}>", self.name)
        } else {
            write!(
                f,
                "<built-in method {} of {} value>",
                self.name, self.receiver
            )
        }
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for BuiltinMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let qualified = if self.qualified_owner.is_empty() {
            self.name.to_owned()
        } else {
            format!(
                "{}.{}",
                self.qualified_owner,
                self.name.trim_start_matches('_')
            )
        };
        call(
            Style::Builtin,
            self.name,
            &qualified,
            self.behavior,
            args,
            eval,
        )
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| {
            other.receiver == self.receiver
                && other.name == self.name
                && other.behavior == self.behavior
        }))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.receiver.hash(hasher);
        self.name.hash(hasher);
        Ok(())
    }
}

impl fmt::Display for BuiltinsFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<function {} from @@_builtins//:{}>",
            self.name, self.module
        )
    }
}

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for BuiltinsFunction {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        call(
            Style::Def,
            self.name,
            self.qualified,
            self.behavior,
            args,
            eval,
        )
    }

    // Bazel returns the same function objects (e.g. apple_toolchain's struct);
    // natively they may be re-allocated, so compare by definition identity.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| {
            other.name == self.name
                && other.module == self.module
                && other.qualified == self.qualified
                && other.behavior == self.behavior
        }))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.module.hash(hasher);
        self.qualified.hash(hasher);
        Ok(())
    }
}

impl BuiltinProvider {
    /// Requirement key: Starlark-defined builtins use the custom-provider
    /// `<extension>%<name>` spelling; Java providers the `builtin:` namespace.
    pub(crate) fn requirement_id(&self) -> String {
        match self.starlark_defined {
            Some(module) => format!("@@_builtins//:{module}%{}", self.name),
            None => format!("builtin:{}", self.name),
        }
    }
}

impl fmt::Display for BuiltinProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.starlark_defined.is_some() {
            f.write_str("<provider>")
        } else {
            write!(f, "<function {}>", self.name)
        }
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for BuiltinProvider {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        // ToolchainInfo.Provider.toolchainInfo (analysis/platform/ToolchainInfo.java:49-60).
        if self.owner == "platform_common" && self.name == "ToolchainInfo" {
            return crate::toolchain_info::construct(args, eval);
        }
        // ExecutionInfo.ExecutionInfoProvider.constructor
        // (analysis/test/ExecutionInfo.java:75-86).
        if self.owner == "testing" && self.name == "ExecutionInfo" {
            return crate::execution_info::construct(args, eval);
        }
        Err(failure(format!(
            "{}.{} {NOT_IMPLEMENTED} ({})",
            self.owner,
            self.name,
            self.domain.describe()
        )))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other)
            .is_some_and(|other| other.requirement_id() == self.requirement_id()))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.requirement_id().hash(hasher);
        Ok(())
    }
}

impl fmt::Display for FeatureFlagInfoProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[starlark_value(type = "FeatureFlagInfo")]
impl<'v> StarlarkValue<'v> for FeatureFlagInfoProvider {
    fn invoke(
        &self,
        me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        // ConfigFeatureFlagProvider.Constructor.selfcall
        // (rules/config/ConfigFeatureFlagProvider.java:84-90).
        let _ = me;
        crate::feature_flags::construct(args, eval)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.0.requirement_id().hash(hasher);
        Ok(())
    }
}

/// Provider requirement identity for `attr.label(providers=[...])`.
pub(crate) fn builtin_provider_requirement(value: Value<'_>) -> Option<(String, &'static str)> {
    BuiltinProvider::from_value(value)
        .or_else(|| FeatureFlagInfoProvider::from_value(value).map(|p| &p.0))
        .map(|p| (p.requirement_id(), p.name))
}

impl fmt::Display for ToolchainType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "<unknown object com.google.devtools.build.lib.analysis.config.ToolchainTypeRequirement>",
        )
    }
}

#[starlark_value(type = "toolchain_type")]
impl<'v> StarlarkValue<'v> for ToolchainType {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match name {
            "toolchain_type" => crate::starlark_types::alloc_label(heap, &self.label).ok(),
            "mandatory" => Some(Value::new_bool(self.mandatory)),
            _ => None,
        }
    }
    fn dir_attr(&self) -> Vec<String> {
        vec!["mandatory".to_owned(), "toolchain_type".to_owned()]
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other) == Some(self))
    }
}

impl fmt::Display for ExecGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<unknown object com.google.devtools.build.lib.packages.DeclaredExecGroup>")
    }
}

#[starlark_value(type = "exec_group")]
impl<'v> StarlarkValue<'v> for ExecGroup {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other) == Some(self))
    }
}

impl fmt::Display for LateBoundDefault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<late-bound default>")
    }
}

#[starlark_value(type = "LateBoundDefault")]
impl<'v> StarlarkValue<'v> for LateBoundDefault {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other) == Some(self))
    }
}

pub(crate) fn is_late_bound_default(value: Value<'_>) -> bool {
    LateBoundDefault::from_value(value).is_some()
}

/// The fragment and field name of a `configuration_field(...)` value.
pub(crate) fn configuration_field_of(
    value: Value<'_>,
) -> Option<crate::custom_rule::ConfigurationField> {
    LateBoundDefault::from_value(value).map(|value| crate::custom_rule::ConfigurationField {
        fragment: value.fragment.clone(),
        name: value.name.clone(),
    })
}

// ---------------------------------------------------------------------------
// Argument binding with Bazel's BuiltinFunction diagnostics.

pub(crate) fn failure(message: String) -> starlark::Error {
    starlark::Error::new_other(anyhow!(message))
}

#[derive(Clone, Copy)]
pub(crate) struct Param {
    pub name: &'static str,
    pub positional: bool,
    pub named: bool,
    pub required: bool,
}

pub(crate) const fn param(
    name: &'static str,
    positional: bool,
    named: bool,
    required: bool,
) -> Param {
    Param {
        name,
        positional,
        named,
        required,
    }
}

/// Bazel words argument errors differently for Java builtins
/// (BuiltinFunction) and `@_builtins` Starlark functions (StarlarkFunction).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Builtin,
    Def,
}

pub(crate) fn bind<'v>(
    function: &str,
    params: &[Param],
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Option<Value<'v>>>> {
    bind_styled(Style::Builtin, function, params, args, heap)
}

pub(crate) fn bind_styled<'v>(
    style: Style,
    function: &str,
    params: &[Param],
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Option<Value<'v>>>> {
    let mut bound: Vec<Option<Value<'v>>> = vec![None; params.len()];
    let slots: Vec<usize> = (0..params.len())
        .filter(|&i| params[i].positional)
        .collect();
    let positions: Vec<Value<'v>> = args.positions(heap)?.collect();
    for (value, &slot) in positions.iter().zip(&slots) {
        bound[slot] = Some(*value);
    }
    let names: Vec<&str> = params.iter().map(|p| p.name).collect();
    let mut unexpected: Vec<String> = Vec::new();
    for (name, value) in args.names_map()?.iter() {
        let name = name.as_str();
        match params.iter().position(|p| p.name == name) {
            // StarlarkFunction.java:487-504: a `def` collects every unknown
            // keyword and reports them after binding.
            None if style == Style::Def => unexpected.push(name.to_owned()),
            None => {
                // BuiltinFunction.java:322-337 fails at the first unknown
                // keyword, with SpellChecker.didYouMean over the parameters.
                let hint = crate::native_rules::did_you_mean(name, &names);
                return Err(failure(format!(
                    "{function}() got unexpected keyword argument '{name}'{hint}"
                )));
            }
            Some(i) if !params[i].named => {
                return Err(failure(format!(
                    "{function}() got named argument for positional-only parameter '{name}'"
                )));
            }
            Some(i) if bound[i].is_some() => {
                // StarlarkFunction.java:482-485 says "parameter".
                let noun = match style {
                    Style::Builtin => "argument",
                    Style::Def => "parameter",
                };
                return Err(failure(format!(
                    "{function}() got multiple values for {noun} '{name}'"
                )));
            }
            Some(i) => bound[i] = Some(*value),
        }
    }
    // Bazel adds every argument before call() checks the positional count
    // (BuiltinFunction.java:420, StarlarkFunction.java:507): an unknown
    // keyword or a duplicate of a builtin, and a duplicate of a `def`, are
    // reported before surplus positional arguments.
    if positions.len() > slots.len() {
        if slots.is_empty() {
            return Err(failure(match style {
                Style::Builtin => format!("{function}() got unexpected positional argument"),
                Style::Def => format!(
                    "{function}() does not accept positional arguments, but got {}",
                    positions.len()
                ),
            }));
        }
        return Err(failure(format!(
            "{function}() accepts no more than {} positional argument{} but got {}",
            slots.len(),
            if slots.len() == 1 { "" } else { "s" },
            positions.len()
        )));
    }
    // StarlarkFunction.java:389-403: plural for several, and a spelling
    // hint only when exactly one keyword is unknown.
    if !unexpected.is_empty() {
        let hint = match unexpected.as_slice() {
            [only] => crate::native_rules::did_you_mean(only, &names),
            _ => String::new(),
        };
        return Err(failure(format!(
            "{function}() got unexpected keyword argument{}: {}{hint}",
            if unexpected.len() == 1 { "" } else { "s" },
            unexpected.join(", ")
        )));
    }
    let named_kind = match style {
        Style::Builtin => "named",
        Style::Def => "keyword-only",
    };
    for (kind, positional) in [("positional", true), (named_kind, false)] {
        let missing: Vec<&str> = params
            .iter()
            .zip(&bound)
            .filter(|(p, v)| p.required && v.is_none() && p.positional == positional)
            .map(|(p, _)| p.name)
            .collect();
        if !missing.is_empty() {
            return Err(failure(format!(
                "{function}() missing {} required {kind} argument{}: {}",
                missing.len(),
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            )));
        }
    }
    Ok(bound)
}

pub(crate) fn type_error(
    function: &str,
    param: &str,
    value: Value<'_>,
    want: &str,
) -> starlark::Error {
    failure(format!(
        "in call to {function}(), parameter '{param}' got value of type '{}', want '{want}'",
        value.get_type()
    ))
}

/// Starlark `Sequence` arguments: lists and tuples.
pub(crate) fn sequence<'v>(
    function: &str,
    param: &str,
    value: Value<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    Err(type_error(function, param, value, "sequence"))
}

fn string_arg<'v>(function: &str, param: &str, value: Value<'v>) -> starlark::Result<&'v str> {
    value
        .unpack_str()
        .ok_or_else(|| type_error(function, param, value, "string"))
}

// ---------------------------------------------------------------------------
// Behaviours.

fn call<'v>(
    style: Style,
    name: &str,
    qualified: &str,
    behavior: Behavior,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    match behavior {
        Behavior::Unimplemented(domain) => Err(failure(format!(
            "{qualified} {NOT_IMPLEMENTED} ({})",
            domain.describe()
        ))),
        Behavior::PyMergeGeneratedInits => {
            // `_py_builtins.merge_runfiles_with_generated_inits_empty_files_supplier(ctx, runfiles)`:
            // both parameters are named-only (PyBuiltins.java:223-236).
            const FUNCTION: &str = "merge_runfiles_with_generated_inits_empty_files_supplier";
            let bound = bind_styled(
                Style::Builtin,
                FUNCTION,
                &[
                    param("ctx", false, true, true),
                    param("runfiles", false, true, true),
                ],
                args,
                heap,
            )?;
            let ctx = bound[0].expect("required parameter");
            if ctx.get_type() != "ctx" {
                return Err(type_error(FUNCTION, "ctx", ctx, "ctx"));
            }
            let runfiles = bound[1].expect("required parameter");
            crate::runfiles::merge_with_generated_inits(runfiles, heap).map_err(|error| match error
            {
                Some(message) => failure(message),
                None => type_error(FUNCTION, "runfiles", runfiles, "runfiles"),
            })
        }
        Behavior::PyLabelRepoRunfilesPath => {
            let label = py_label_argument(args, heap, qualified)?;
            Ok(heap.alloc(package_runfiles_path(&label)))
        }
        Behavior::PyRegexMatch => crate::py_regex::call(args, heap),
        Behavior::PyIsToolConfiguration => {
            // The `def`'s one parameter, then its body
            // `ctx.configuration.is_tool_configuration()`: the field and the
            // method of whatever value was passed, as Starlark evaluates them
            // (BuildConfigurationValue.isToolConfiguration = isExecConfiguration,
            // analysis/config/BuildConfigurationValue.java:750-758).
            let bound = bind_styled(style, name, &[param("ctx", true, true, true)], args, heap)?;
            let ctx = bound[0].expect("required parameter");
            let configuration = ctx.get_attr_error("configuration", heap)?;
            let method = configuration.get_attr_error("is_tool_configuration", heap)?;
            eval.eval_function(method, &[], &[])
        }
        Behavior::PyLegacyExternalRunfiles => {
            // `*args, **kwargs`: any arguments (their unpacking still fails
            // at the call, like any call's).
            args.positions(heap)?.for_each(drop);
            args.names_map()?;
            Ok(Value::new_bool(false))
        }
        Behavior::PyIsBzlmodEnabled => {
            // `_is_bzlmod_enabled(*args, **kwargs)` forwards to the Java
            // method: its StarlarkRuleContext parameter (type `ctx`).
            const FUNCTION: &str = "is_bzlmod_enabled";
            let check = |value: Value<'v>| {
                if value.get_type() == "ctx" {
                    Ok(())
                } else {
                    Err(type_error(FUNCTION, "ctx", value, "ctx"))
                }
            };
            py_unbound_argument(args, heap, FUNCTION, "ctx", check)?.ok_or_else(|| {
                failure(format!(
                    "{qualified}() without a ctx: genuine Bazel 9.3 crashes on it (IllegalArgumentException, argument type mismatch: the `unbound` default reaches PyBuiltins.isBzlmodEnabled), which the native profile reports instead"
                ))
            })?;
            Ok(Value::new_bool(true))
        }
        Behavior::PyCreateRepoMappingManifest => {
            // `_create_repo_mapping_manifest(*args, **kwargs)` forwards to the
            // Java method: three named-only parameters (StarlarkRuleContext,
            // Runfiles, Artifact) whose `unbound` defaults reach it when
            // absent (Bazel 9.3 crashes; an explicit error natively).
            const FUNCTION: &str = "create_repo_mapping_manifest";
            let bound = bind_styled(
                Style::Builtin,
                FUNCTION,
                &[
                    param("ctx", false, true, false),
                    param("runfiles", false, true, false),
                    param("output", false, true, false),
                ],
                args,
                heap,
            )?;
            if let Some(ctx) = bound[0]
                && ctx.get_type() != "ctx"
            {
                return Err(type_error(FUNCTION, "ctx", ctx, "ctx"));
            }
            if let Some(runfiles) = bound[1]
                && crate::runfiles::Runfiles::from_value(runfiles).is_none()
            {
                return Err(type_error(FUNCTION, "runfiles", runfiles, "runfiles"));
            }
            if let Some(output) = bound[2]
                && crate::starlark_types::unpack_file(output).is_err()
            {
                return Err(type_error(FUNCTION, "output", output, "File"));
            }
            let (Some(ctx), Some(runfiles), Some(output)) = (bound[0], bound[1], bound[2]) else {
                return Err(failure(format!(
                    "{qualified}() without each of ctx, runfiles and output: genuine Bazel 9.3 crashes on it (IllegalArgumentException, argument type mismatch: an `unbound` default reaches PyBuiltins.repoMappingAction), which the native profile reports instead"
                )));
            };
            let runfiles = crate::runfiles::Runfiles::from_value(runfiles).expect("checked");
            crate::analysis_runtime::create_repo_mapping_manifest(ctx, runfiles, output, eval)?;
            Ok(Value::new_none())
        }
        Behavior::PyIsValidSharedLibraryArtifact => {
            let bound = bind_styled(
                style,
                name,
                &[param("shared_library", true, true, true)],
                args,
                heap,
            )?;
            let library = bound[0].expect("required parameter");
            is_valid_shared_library_artifact(library, eval).map(Value::new_bool)
        }
        Behavior::PyRespectLegacyExternalRunfiles => {
            let bound = bind_styled(
                style,
                name,
                &[
                    param("_ctx", true, true, true),
                    param("runfiles", true, true, true),
                ],
                args,
                heap,
            )?;
            Ok(bound[1].expect("required parameter"))
        }
        Behavior::CcInternalExports => {
            bind_styled(style, name, &[], args, heap)?;
            crate::cc_common::internal_exports(eval)
        }
        Behavior::JavaInternalExports => {
            bind_styled(style, name, &[], args, heap)?;
            crate::cc_common::java_internal_exports(eval)
        }
        Behavior::JavaSemanticsFlag(flag) => {
            bind_styled(style, name, &[], args, heap)?;
            crate::cc_common::java_semantics_flag(eval, flag)
        }
        Behavior::JavaCheckProviderInstances => {
            crate::cc_common::java_check_provider_instances(args, eval)
        }
        Behavior::CheckPrivateApi => crate::cc_common::check_private_api_call(args, eval),
        Behavior::CreateHeaderInfo => crate::cc_common::create_header_info(args, eval),
        Behavior::CreateHeaderInfoWithDeps => {
            crate::cc_common::create_header_info_with_deps(args, eval)
        }
        Behavior::Freeze => {
            // CcStarlarkInternal.freeze(value): @Param defaults are positional-only.
            let bound = bind_styled(
                style,
                name,
                &[param("value", true, false, true)],
                args,
                heap,
            )?;
            crate::cc_common::freeze(bound[0].expect("required parameter"), eval)
        }
        Behavior::AppleToolchain => {
            bind_styled(style, name, &[], args, heap)?;
            let function = |name, qualified, behavior| {
                heap.alloc(BuiltinsFunction {
                    name,
                    module: "common/objc/apple_toolchain.bzl",
                    qualified,
                    behavior,
                })
            };
            Ok(heap.alloc(AllocStruct([
                (
                    "developer_dir",
                    function(
                        "_developer_dir",
                        "apple_toolchain.developer_dir",
                        Behavior::AppleDeveloperDir,
                    ),
                ),
                (
                    "platform_developer_framework_dir",
                    function(
                        "_platform_developer_framework_dir",
                        "apple_toolchain.platform_developer_framework_dir",
                        Behavior::ApplePlatformDeveloperFrameworkDir,
                    ),
                ),
                (
                    "sdk_dir",
                    function("_sdk_dir", "apple_toolchain.sdk_dir", Behavior::AppleSdkDir),
                ),
            ])))
        }
        Behavior::AppleDeveloperDir => {
            bind_styled(style, name, &[], args, heap)?;
            Ok(heap.alloc(APPLE_DEVELOPER_DIR))
        }
        Behavior::AppleSdkDir => {
            bind_styled(style, name, &[], args, heap)?;
            Ok(heap.alloc("__BAZEL_XCODE_SDKROOT__"))
        }
        Behavior::ApplePlatformDeveloperFrameworkDir => {
            let bound = bind_styled(
                style,
                name,
                &[param("apple_fragment", true, true, true)],
                args,
                heap,
            )?;
            let fragment = bound[0].expect("required");
            let platform = fragment.get_attr_error("single_arch_platform", heap)?;
            let plist = platform.get_attr_error("name_in_plist", heap)?;
            Ok(heap.alloc(format!(
                "{APPLE_DEVELOPER_DIR}/Platforms/{}.platform/Developer/Library/Frameworks",
                plist.to_str()
            )))
        }
        Behavior::CcFeatures(op) => crate::cc_features::call(op, style, name, args, eval),
        Behavior::CcActions(method) => crate::cc_actions::call(method, args, eval),
        Behavior::DottedVersion => {
            // The lambda's `version` binds by name too; the Java @Param
            // (AppleCommonApi.java:146-152) is positional-only.
            let bound = bind_styled(
                style,
                name,
                &[param("version", true, style == Style::Def, true)],
                args,
                heap,
            )?;
            crate::apple_xcode::dotted_version(bound[0].expect("required"), heap)
        }
        Behavior::ToolchainType => toolchain_type(args, eval),
        Behavior::ExecGroup => exec_group(args, eval),
        Behavior::ConfigurationField => configuration_field(args, eval),
        Behavior::InstrumentedFilesInfo => {
            crate::instrumented_files::instrumented_files_info(style, args, eval)
        }
        Behavior::TestEnvironment => {
            let bound = bind_styled(
                style,
                name,
                &[
                    param("environment", true, true, true),
                    param("inherited_environment", true, true, false),
                ],
                args,
                heap,
            )?;
            crate::run_environment_info::alloc_test_environment(
                heap,
                bound[0].expect("required"),
                bound[1],
            )
            .map_err(starlark::Error::new_other)
        }
    }
}

const APPLE_DEVELOPER_DIR: &str = "__BAZEL_XCODE_DEVELOPER_DIR__";

/// ConfigStarlarkCommon.toolchainType: string names use the evaluating
/// `.bzl` file's repository mapping (LabelConverter.forBzlEvaluatingThread).
fn toolchain_type<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let bound = bind(
        "toolchain_type",
        &[
            param("name", true, false, true),
            param("mandatory", false, true, false),
        ],
        args,
        heap,
    )?;
    let mandatory = match bound[1] {
        None => true,
        Some(value) => value
            .unpack_bool()
            .ok_or_else(|| type_error("toolchain_type", "mandatory", value, "bool"))?,
    };
    let name = bound[0].expect("required");
    // BuiltinFunction checks @Param allowedTypes {String, Label} before
    // ConfigStarlarkCommon.toolchainType's own (unreachable) type message.
    let label = toolchain_label(name, eval).map_err(|kind| match kind {
        ToolchainLabelError::Type => type_error("toolchain_type", "name", name, "string or Label"),
        ToolchainLabelError::Parse(message) => failure(message),
    })?;
    Ok(eval.heap().alloc(ToolchainType { label, mandatory }))
}

enum ToolchainLabelError {
    Type,
    Parse(String),
}

fn toolchain_label(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
) -> Result<String, ToolchainLabelError> {
    if let Some(label) = crate::starlark_types::unpack_label(value) {
        return Ok(label);
    }
    let text = value.unpack_str().ok_or(ToolchainLabelError::Type)?;
    crate::loading::resolve_label(eval, text)
        .map(|label| label.to_string())
        .map_err(|e| {
            ToolchainLabelError::Parse(format!(
                "Unable to parse toolchain_type label '{text}': {e}"
            ))
        })
}

/// Java `getClass().getSimpleName()` of the value Bazel would hold, for the
/// diagnostics that print it. Unlisted native types fall back to the
/// Starlark type name.
pub(crate) fn java_class_simple_name(value: Value<'_>) -> String {
    if value.is_none() {
        return "NoneType".to_owned();
    }
    if value.unpack_bool().is_some() {
        return "Boolean".to_owned();
    }
    if value.unpack_str().is_some() {
        return "String".to_owned();
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        // Tuple.wrap: SingletonTuple for one element, else RegularTuple.
        return if tuple.len() == 1 {
            "SingletonTuple"
        } else {
            "RegularTuple"
        }
        .to_owned();
    }
    let name = match value.get_type() {
        // StarlarkInt.of: Int32 within int range, Int64, then Big.
        "int" => match value.unpack_i32() {
            Some(_) => "Int32",
            None if value.to_str().parse::<i64>().is_ok() => "Int64",
            None => "Big",
        },
        "float" => "StarlarkFloat",
        // A list literal is StarlarkList.wrap(mutable) -> MutableStarlarkList.
        "list" => "MutableStarlarkList",
        "dict" => "Dict",
        "set" => "StarlarkSet",
        "range" => "RangeList",
        "depset" => "Depset",
        "struct" => "StarlarkInfoNoSchema",
        "function" => "StarlarkFunction",
        "builtin_function_or_method" => "BuiltinFunction",
        other => other,
    };
    name.to_owned()
}

/// The DeclaredExecGroup `name` of an `exec_group(...)` value.
pub(crate) fn declared_exec_group(
    name: String,
    value: Value<'_>,
) -> Option<crate::rule_declarations::DeclaredExecGroup> {
    ExecGroup::from_value(value).map(|group| crate::rule_declarations::DeclaredExecGroup {
        name,
        toolchains: group.toolchains.clone(),
        exec_compatible_with: group.exec_compatible_with.clone(),
    })
}

/// StarlarkRuleClassFunctions.execGroup / parseToolchainTypes: duplicate
/// types keep the first position and the strictest (mandatory) requirement.
fn exec_group<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let bound = bind(
        "exec_group",
        &[
            param("toolchains", false, true, false),
            param("exec_compatible_with", false, true, false),
        ],
        args,
        heap,
    )?;
    let mut toolchains: Vec<(String, bool)> = Vec::new();
    if let Some(value) = bound[0] {
        for item in sequence("exec_group", "toolchains", value)? {
            let (label, mandatory) = if let Some(requirement) = ToolchainType::from_value(item) {
                (requirement.label.clone(), requirement.mandatory)
            } else {
                let label = toolchain_label(item, eval).map_err(|kind| match kind {
                    // parseToolchainType formats getClass().getSimpleName().
                    ToolchainLabelError::Type => failure(format!(
                        "'toolchains' takes a toolchain_type, Label, or String, but instead got a {}",
                        java_class_simple_name(item)
                    )),
                    ToolchainLabelError::Parse(message) => failure(message),
                })?;
                (label, true)
            };
            match toolchains
                .iter_mut()
                .find(|(existing, _)| *existing == label)
            {
                Some((_, existing)) => *existing |= mandatory,
                None => toolchains.push((label, mandatory)),
            }
        }
    }
    // StarlarkRuleClassFunctions.parseLabels: Sequence.cast(inputs, String.class,
    // "exec_compatible_with") checks every element before any is parsed.
    let mut exec_compatible_with = Vec::new();
    if let Some(value) = bound[1] {
        let items = sequence("exec_group", "exec_compatible_with", value)?;
        let mut texts = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            texts.push(item.unpack_str().ok_or_else(|| {
                failure(format!(
                    "at index {index} of exec_compatible_with, got element of type {}, want string",
                    item.get_type()
                ))
            })?);
        }
        for text in texts {
            let label = crate::loading::resolve_label(eval, text)
                .map_err(|e| {
                    failure(format!(
                        "Unable to parse label '{text}' in attribute 'exec_compatible_with': {e}"
                    ))
                })?
                .to_string();
            if !exec_compatible_with.contains(&label) {
                exec_compatible_with.push(label);
            }
        }
    }
    Ok(eval.heap().alloc(ExecGroup {
        toolchains,
        exec_compatible_with,
    }))
}

/// `@StarlarkConfigurationField` names of every fragment registered by Bazel
/// 9.3's BazelRuleClassProvider whose class has a `@StarlarkBuiltin` name
/// (ConfiguredRuleClassProvider.createFragmentMap). Fragments without
/// configuration fields are still valid fragment names.
const CONFIGURATION_FIELDS: [(&str, &[&str]); 12] = [
    (
        "android",
        &["legacy_main_dex_list_generator", "optimizing_dexer"],
    ),
    ("apple", &["xcode_config_label"]),
    ("bazel_android", &[]),
    ("bazel_py", &["python_top"]),
    ("coverage", &["output_generator"]),
    (
        "cpp",
        &[
            "cs_fdo_profile",
            "custom_malloc",
            "fdo_optimize",
            "fdo_prefetch_hints",
            "fdo_profile",
            "libc_top",
            "memprof_profile",
            "propeller_optimize",
            "proto_profile_path",
            "xbinary_fdo",
            "zipper",
        ],
    ),
    ("j2objc", &["dead_code_report"]),
    (
        "java",
        &[
            "bytecode_optimizer",
            "java_toolchain_bytecode_optimizer",
            "launcher",
            "local_java_optimization_configuration",
            "proguard_top",
        ],
    ),
    ("objc", &[]),
    ("platform", &[]),
    (
        "proto",
        &[
            "proto_compiler",
            "proto_toolchain_for_cc",
            "proto_toolchain_for_j2objc",
            "proto_toolchain_for_java",
            "proto_toolchain_for_java_lite",
        ],
    ),
    ("py", &["native_rules_allowlist"]),
];

/// BazelBuildApiGlobals.configurationField: only during `.bzl` initialization
/// (BzlInitThreadContext), validated against the fragment's annotated fields.
fn configuration_field<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let bound = bind(
        "configuration_field",
        &[
            param("fragment", true, true, true),
            param("name", true, true, true),
        ],
        args,
        heap,
    )?;
    let fragment = string_arg(
        "configuration_field",
        "fragment",
        bound[0].expect("required"),
    )?;
    let name = string_arg("configuration_field", "name", bound[1].expect("required"))?;
    let initializing = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<crate::loading::ExtensionContext>())
        .is_some();
    if !initializing {
        return Err(failure(
            "configuration_field() can only be used during .bzl initialization (top-level evaluation)"
                .to_owned(),
        ));
    }
    let fields = CONFIGURATION_FIELDS
        .iter()
        .find(|(known, _)| *known == fragment)
        .map(|(_, fields)| *fields)
        .ok_or_else(|| failure(format!("invalid configuration fragment name '{fragment}'")))?;
    if !fields.contains(&name) {
        return Err(failure(format!(
            "invalid configuration field name '{name}' on fragment '{fragment}'"
        )));
    }
    Ok(heap.alloc(LateBoundDefault {
        fragment: fragment.to_owned(),
        name: name.to_owned(),
    }))
}

// ---------------------------------------------------------------------------
// Struct-valued builtins. Bazel's struct (StarlarkInfo) sorts its fields, so
// declaration order is unobservable; fields are inserted pre-sorted here so
// the native insertion-ordered struct prints and lists them like Bazel.

pub(crate) fn sorted_struct(
    heap: &FrozenHeap,
    mut fields: Vec<(&'static str, FrozenValue)>,
) -> FrozenValue {
    fields.sort_by(|a, b| a.0.cmp(b.0));
    heap.alloc(AllocStruct(fields))
}

pub(crate) fn builtins_function(
    heap: &FrozenHeap,
    name: &'static str,
    module: &'static str,
    qualified: &'static str,
    behavior: Behavior,
) -> FrozenValue {
    heap.alloc(BuiltinsFunction {
        name,
        module,
        qualified,
        behavior,
    })
}

/// builtins_bzl/common/objc/apple_platform.bzl PLATFORM_TYPE / PLATFORM.
const APPLE_PLATFORM_TYPES: [&str; 6] = ["catalyst", "ios", "macos", "tvos", "visionos", "watchos"];
const APPLE_PLATFORMS: [(&str, &str, &str, bool); 10] = [
    ("ios_device", "iPhoneOS", "ios", true),
    ("ios_simulator", "iPhoneSimulator", "ios", false),
    ("macos", "MacOSX", "macos", true),
    ("tvos_device", "AppleTVOS", "tvos", true),
    ("tvos_simulator", "AppleTVSimulator", "tvos", false),
    ("visionos_device", "XROS", "visionos", true),
    ("visionos_simulator", "XRSimulator", "visionos", false),
    ("watchos_device", "WatchOS", "watchos", true),
    ("watchos_simulator", "WatchSimulator", "watchos", false),
    ("catalyst", "MacOSX", "catalyst", true),
];

/// builtins_bzl/common/objc/apple_platform.bzl `(PLATFORM_TYPE, PLATFORM)`.
pub(crate) fn apple_platform_tables(heap: &FrozenHeap) -> (FrozenValue, FrozenValue) {
    let platform_type = sorted_struct(
        heap,
        APPLE_PLATFORM_TYPES
            .iter()
            .map(|name| (*name, heap.alloc(*name)))
            .collect(),
    );
    let platform = sorted_struct(
        heap,
        APPLE_PLATFORMS
            .iter()
            .map(|(name, plist, platform_type, is_device)| {
                (
                    *name,
                    sorted_struct(
                        heap,
                        vec![
                            ("name", heap.alloc(*name)),
                            ("name_in_plist", heap.alloc(*plist)),
                            ("platform_type", heap.alloc(*platform_type)),
                            ("is_device", heap.alloc(*is_device)),
                        ],
                    ),
                )
            })
            .collect(),
    );
    (platform_type, platform)
}

/// builtins_bzl/common/objc/apple_common.bzl `apple_common_bazel`.
fn apple_common(heap: &FrozenHeap) -> FrozenValue {
    let (platform_type, platform) = apple_platform_tables(heap);
    let provider = |name, module| {
        heap.alloc(BuiltinProvider {
            name,
            owner: "apple_common",
            starlark_defined: Some(module),
            domain: Domain::Apple,
        })
    };
    let objc = provider("ObjcInfo", "common/objc/objc_info.bzl");
    let unimplemented = Behavior::Unimplemented(Domain::Apple);
    sorted_struct(
        heap,
        vec![
            (
                "apple_toolchain",
                builtins_function(
                    heap,
                    "lambda",
                    "common/objc/apple_common.bzl",
                    "apple_common.apple_toolchain",
                    Behavior::AppleToolchain,
                ),
            ),
            ("platform_type", platform_type),
            ("platform", platform),
            ("XcodeProperties", FrozenValue::new_none()),
            // The verbatim @_builtins provider (crate::apple_xcode).
            (
                "XcodeVersionConfig",
                crate::apple_xcode::xcode_version_info(heap),
            ),
            (
                "apple_host_system_env",
                builtins_function(
                    heap,
                    "apple_host_system_env",
                    "common/objc/apple_env.bzl",
                    "apple_common.apple_host_system_env",
                    unimplemented,
                ),
            ),
            (
                "target_apple_env",
                builtins_function(
                    heap,
                    "target_apple_env",
                    "common/objc/apple_env.bzl",
                    "apple_common.target_apple_env",
                    unimplemented,
                ),
            ),
            (
                "dotted_version",
                builtins_function(
                    heap,
                    "lambda",
                    "common/objc/apple_common.bzl",
                    "apple_common.dotted_version",
                    Behavior::DottedVersion,
                ),
            ),
            ("Objc", objc),
            ("new_objc_provider", objc),
        ],
    )
}

/// builtins_bzl/common/python/py_internal.bzl `py_internal`: 24 functions,
/// the nested `cc_helper` struct and `PackageSpecificationInfo` (26 fields).
macro_rules! py_functions {
    ($($field:literal),* $(,)?) => {
        const PY_INTERNAL_FUNCTIONS: &[(&str, &str, &str)] =
            &[$(($field, concat!("_", $field), concat!("py_internal.", $field))),*];
    };
}
py_functions!(
    "add_py_extra_pseudo_action",
    "are_action_listeners_enabled",
    "copy_without_caching",
    "create_repo_mapping_manifest",
    "create_sources_only_manifest",
    "declare_constant_metadata_file",
    "declare_shareable_artifact",
    "expand_location_and_make_variables",
    "get_current_os_name",
    "get_label_repo_runfiles_path",
    "get_legacy_external_runfiles",
    "get_rule_name",
    "is_available_for",
    "is_bzlmod_enabled",
    "is_singleton_depset",
    "is_tool_configuration",
    "linkstamp_file",
    "make_runfiles_respect_legacy_external_runfiles",
    "merge_runfiles_with_generated_inits_empty_files_supplier",
    "regex_match",
    "runfiles_enabled",
    "share_native_deps",
    "stamp_binaries",
    "strip_opts",
);

fn py_internal(heap: &FrozenHeap) -> FrozenValue {
    const MODULE: &str = "common/python/py_internal.bzl";
    let unimplemented = Behavior::Unimplemented(Domain::Python);
    let mut fields: Vec<(&'static str, FrozenValue)> = PY_INTERNAL_FUNCTIONS
        .iter()
        .map(|(field, def, qualified)| {
            let behavior = match *field {
                "merge_runfiles_with_generated_inits_empty_files_supplier" => {
                    Behavior::PyMergeGeneratedInits
                }
                "get_label_repo_runfiles_path" => Behavior::PyLabelRepoRunfilesPath,
                "regex_match" => Behavior::PyRegexMatch,
                "is_tool_configuration" => Behavior::PyIsToolConfiguration,
                "get_legacy_external_runfiles" => Behavior::PyLegacyExternalRunfiles,
                "is_bzlmod_enabled" => Behavior::PyIsBzlmodEnabled,
                "make_runfiles_respect_legacy_external_runfiles" => {
                    Behavior::PyRespectLegacyExternalRunfiles
                }
                "create_repo_mapping_manifest" => Behavior::PyCreateRepoMappingManifest,
                _ => unimplemented,
            };
            (
                *field,
                builtins_function(heap, def, MODULE, qualified, behavior),
            )
        })
        .collect();
    fields.push((
        "cc_helper",
        sorted_struct(
            heap,
            vec![(
                "is_valid_shared_library_artifact",
                builtins_function(
                    heap,
                    "_is_valid_shared_library_artifact",
                    MODULE,
                    "py_internal.cc_helper.is_valid_shared_library_artifact",
                    Behavior::PyIsValidSharedLibraryArtifact,
                ),
            )],
        ),
    ));
    fields.push((
        "PackageSpecificationInfo",
        crate::package_specification::alloc_frozen_constructor(heap),
    ));
    sorted_struct(heap, fields)
}

/// `_is_valid_shared_library_artifact(shared_library)` and
/// `_is_versioned_shared_library_extension_valid`
/// (builtins_bzl/common/python/py_internal.bzl:101-131), as Starlark
/// evaluates them: `shared_library.extension in ["so", "dll", "dylib",
/// "pyd", "wasm"]`, else the basename's last `.so.` or `.dylib.` split has
/// a non-empty name and version whose `.`-separated parts start with a
/// digit and continue with `[a-zA-Z0-9_]` (Bazel's ASCII isdigit/isalnum,
/// StringModule.java:762-779). Fields come from any value; an empty part
/// is Bazel's `part[0]` index error.
fn is_valid_shared_library_artifact<'v>(
    library: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<bool> {
    const EXTENSIONS: [&str; 5] = ["so", "dll", "dylib", "pyd", "wasm"];
    let heap = eval.heap();
    let extension = library.get_attr_error("extension", heap)?;
    if extension
        .unpack_str()
        .is_some_and(|extension| EXTENSIONS.contains(&extension))
    {
        return Ok(true);
    }
    let basename = library.get_attr_error("basename", heap)?;
    let Some(name) = basename.unpack_str() else {
        // `shared_library_name.rpartition(ext)` on another value.
        basename.get_attr_error("rpartition", heap)?;
        return Err(failure(format!(
            "'{}' value has no field or method 'rpartition'",
            basename.get_type()
        )));
    };
    for separator in [".so.", ".dylib."] {
        let Some(index) = name.rfind(separator) else {
            continue;
        };
        let (before, version) = (&name[..index], &name[index + separator.len()..]);
        if before.is_empty() || version.is_empty() {
            continue;
        }
        for part in version.split('.') {
            let Some(first) = part.bytes().next() else {
                return Err(failure(
                    "index out of range (index is 0, but sequence has 0 elements)".to_owned(),
                ));
            };
            if !first.is_ascii_digit() {
                return Ok(false);
            }
            if !part[1..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// The `label` argument of `_py_builtins.get_label_repo_runfiles_path`
/// (PyBuiltins.java:120-126: positional or named, default `unbound`, Java
/// type Label) with BuiltinFunction's binding order
/// (net/starlark/java/eval/BuiltinFunction.java:308-445): the positional
/// value's type, then named arguments (unknown keyword, type, duplicate),
/// then the surplus positional count. Without an argument the `unbound`
/// default reaches the Java method and Bazel 9.3 crashes.
fn py_label_argument<'v>(
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
    qualified: &str,
) -> starlark::Result<crate::label::Label> {
    const FUNCTION: &str = "get_label_repo_runfiles_path";
    let check = |value: Value<'v>| {
        crate::starlark_types::unpack_label(value)
            .and_then(|text| crate::label::Label::parse_canonical(&text).ok())
            .ok_or_else(|| type_error(FUNCTION, "label", value, "Label"))
    };
    py_unbound_argument(args, heap, FUNCTION, "label", check)?.ok_or_else(|| {
        failure(format!(
            "{qualified}() without a label: genuine Bazel 9.3 crashes on it (IllegalArgumentException, argument type mismatch: the `unbound` default reaches PyBuiltins.getLabelRepoRunfilesPath), which the native profile reports instead"
        ))
    })
}

/// The one `@Param(positional = true, named = true, defaultValue =
/// "unbound")` of a PyBuiltins method (rules/python/PyBuiltins.java), bound
/// in BuiltinFunction's order (net/starlark/java/eval/BuiltinFunction.java
/// :308-445): the positional value's type (`check`), then named arguments
/// (unknown keyword, type, duplicate), then the surplus positional count.
/// `None`: no argument, so the `unbound` default would reach the Java
/// method (each caller names its declared divergence).
fn py_unbound_argument<'v, T>(
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
    function: &str,
    parameter: &str,
    check: impl Fn(Value<'v>) -> starlark::Result<T>,
) -> starlark::Result<Option<T>> {
    let positional: Vec<Value<'v>> = args.positions(heap)?.collect();
    let mut bound = match positional.first() {
        Some(value) => Some(check(*value)?),
        None => None,
    };
    for (name, value) in args.names_map()?.iter() {
        let name = name.as_str();
        if name != parameter {
            return Err(failure(format!(
                "{function}() got unexpected keyword argument '{name}'{}",
                crate::native_rules::did_you_mean(name, &[parameter])
            )));
        }
        let value = check(*value)?;
        if bound.replace(value).is_some() {
            return Err(failure(format!(
                "{function}() got multiple values for argument '{parameter}'"
            )));
        }
    }
    if positional.len() > 1 {
        return Err(failure(format!(
            "{function}() accepts no more than 1 positional argument but got {}",
            positional.len()
        )));
    }
    Ok(bound)
}

/// PackageIdentifier.getRunfilesPath (cmdline/PackageIdentifier.java): the
/// package below `../REPO` for an external repository
/// (RepositoryName.getRunfilesPath), the package itself in the main one.
fn package_runfiles_path(label: &crate::label::Label) -> String {
    match (label.repository.is_empty(), label.package.is_empty()) {
        (true, _) => label.package.clone(),
        (false, true) => format!("../{}", label.repository),
        (false, false) => format!("../{}/{}", label.repository, label.package),
    }
}

/// Install every language-module global into a `.bzl` environment. Pinned
/// Bazel exposes these identically to BUILD- and MODULE-loaded `.bzl` files.
pub fn install(builder: &mut GlobalsBuilder) {
    builder.set("config_common", ConfigCommon);
    builder.set("coverage_common", CoverageCommon);
    builder.set("testing", Testing);
    builder.set(
        "exec_group",
        BuiltinMethod {
            receiver: "",
            name: "exec_group",
            qualified_owner: "",
            behavior: Behavior::ExecGroup,
        },
    );
    builder.set(
        "configuration_field",
        BuiltinMethod {
            receiver: "",
            name: "configuration_field",
            qualified_owner: "",
            behavior: Behavior::ConfigurationField,
        },
    );
    // CcBootstrap: flag-guarded Starlark.NONE that exports.bzl never overrides.
    builder.set("CcToolchainConfigInfo", FrozenValue::new_none());
    let (apple, python, proto) = {
        let heap = builder.frozen_heap();
        let proto = sorted_struct(
            heap,
            // builtins_bzl/common/exports.bzl; the flag defaults to true
            // (FlagConstants.DEFAULT_INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION).
            vec![(
                "INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION",
                FrozenValue::new_bool(true),
            )],
        );
        (apple_common(heap), py_internal(heap), proto)
    };
    builder.set("apple_common", apple);
    builder.set("py_internal", python);
    builder.set("proto_common_do_not_use", proto);
    crate::cc_common::install(builder);
}

/// The complete `platform_common` namespace value.
pub(crate) fn platform_common() -> PlatformCommon {
    PlatformCommon
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::label::Label;
    use crate::loading::ExtensionContext;
    use starlark::environment::{FrozenModule, Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use std::sync::Arc;

    /// Evaluate `source` as the `.bzl` file `filename` (a canonical label, so
    /// call-stack locations identify it), optionally during `.bzl`
    /// initialization (ExtensionContext), with the production BUILD-phase
    /// or MODULE-phase `.bzl` globals.
    pub(crate) fn run_with(
        globals: &Globals,
        filename: &str,
        source: &str,
        init: bool,
    ) -> Result<FrozenModule, String> {
        let context = ExtensionContext {
            label: Label::parse_canonical(filename).unwrap(),
            repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
            byte_strings: false,
        };
        Module::with_temp_heap(|module| {
            let dialect = Dialect {
                enable_keyword_only_arguments: true,
                ..Dialect::Standard
            };
            let ast = AstModule::parse(filename, source.to_owned(), &dialect)
                .map_err(|e| e.to_string())?;
            {
                let mut eval = Evaluator::new(&module);
                if init {
                    eval.extra = Some(&context);
                }
                eval.eval_module(ast, globals).map_err(|e| format!("{e}"))?;
            }
            module.freeze().map_err(|e| format!("{e:?}"))
        })
    }

    pub(crate) fn run(filename: &str, source: &str) -> Result<FrozenModule, String> {
        run_with(&crate::package::bzl_globals(), filename, source, true)
    }

    pub(crate) fn pass(filename: &str, source: &str) {
        if let Err(error) = run(filename, source) {
            panic!("{source}\n=> {error}");
        }
    }

    pub(crate) fn fails(filename: &str, source: &str, expected: &str) {
        match run(filename, source) {
            Ok(_) => panic!("unexpected success: {source}"),
            Err(error) => assert!(
                error.contains(expected),
                "{source}\n=> {error}\nwant {expected}"
            ),
        }
    }

    const FILE: &str = "@@some_rules+//pkg:defs.bzl";

    #[test]
    fn every_language_global_is_in_both_bzl_phases() {
        let source = r#"
names = [cc_common, java_common, apple_common, py_internal, proto_common_do_not_use,
         platform_common, config_common, coverage_common, testing, exec_group,
         configuration_field, CcToolchainConfigInfo]
(CcToolchainConfigInfo == None) or fail("CcToolchainConfigInfo")
"#;
        for globals in [
            crate::package::bzl_globals(),
            crate::package::module_bzl_globals(),
        ] {
            run_with(&globals, FILE, source, true).unwrap();
        }
    }

    #[test]
    fn java_namespaces_types_dir_repr_and_hasattr() {
        pass(
            FILE,
            r#"
(type(platform_common) == "platform_common") or fail("pc type")
(dir(platform_common) == ["ConstraintSettingInfo", "ConstraintValueInfo", "PlatformInfo", "TemplateVariableInfo", "ToolchainInfo"]) or fail("pc dir")
(repr(platform_common) == "<unknown object com.google.devtools.build.lib.rules.platform.PlatformCommon>") or fail("pc repr")
(type(config_common) == "config_common" and dir(config_common) == ["FeatureFlagInfo", "config_feature_flag_transition", "toolchain_type"]) or fail("cfg")
(repr(config_common) == "<unknown object com.google.devtools.build.lib.rules.config.ConfigStarlarkCommon>") or fail("cfg repr")
(type(coverage_common) == "coverage_common" and dir(coverage_common) == ["instrumented_files_info"] and repr(coverage_common) == "<coverage_common>") or fail("coverage")
(type(testing) == "testing" and dir(testing) == ["ExecutionInfo", "TestEnvironment", "analysis_test"]) or fail("testing")
(repr(testing) == "<unknown object com.google.devtools.build.lib.rules.test.StarlarkTestingModule>") or fail("testing repr")
def _all_providers():
    for name in dir(platform_common):
        (hasattr(platform_common, name) and type(getattr(platform_common, name)) == "Provider") or fail(name)
_all_providers()
(not hasattr(platform_common, "nope") and not hasattr(testing, "TestInfo")) or fail("unknown")
(repr(platform_common.ToolchainInfo) == "<function ToolchainInfo>" and repr(config_common.FeatureFlagInfo) == "<function FeatureFlagInfo>") or fail("provider repr")
# ConfigFeatureFlagProvider.Constructor is @StarlarkBuiltin(name = "FeatureFlagInfo").
(type(config_common.FeatureFlagInfo) == "FeatureFlagInfo" and dir(config_common.FeatureFlagInfo) == []) or fail("FeatureFlagInfo type")
(config_common.FeatureFlagInfo == config_common.FeatureFlagInfo and config_common.FeatureFlagInfo != platform_common.ToolchainInfo) or fail("FeatureFlagInfo identity")
f = config_common.FeatureFlagInfo(value = "x")
(type(f) == "FeatureFlagInfo" and dir(f) == ["error", "is_valid_value", "value"] and f.value == "x" and f.error == None) or fail("FeatureFlagInfo instance")
(repr(f) == 'struct(error = None, value = "x")' and str(f) == repr(f) and f.is_valid_value("y") and type(f.is_valid_value) == "builtin_function_or_method") or fail("FeatureFlagInfo repr")
(f == f and f != config_common.FeatureFlagInfo("x") and {f: 1}[f] == 1) or fail("FeatureFlagInfo identity equality")
({config_common.FeatureFlagInfo: 1}[config_common.FeatureFlagInfo] == 1) or fail("FeatureFlagInfo hash")
(type(testing.ExecutionInfo) == "Provider" and type(apple_common.XcodeVersionConfig) == "Provider") or fail("other provider types")
(repr(testing.ExecutionInfo) == "<function ExecutionInfo>") or fail("ExecutionInfo")
(platform_common.ToolchainInfo == platform_common.ToolchainInfo and platform_common.ToolchainInfo != platform_common.PlatformInfo) or fail("identity")
({platform_common.ToolchainInfo: 1}[platform_common.ToolchainInfo] == 1) or fail("hash")
(type(config_common.toolchain_type) == "builtin_function_or_method") or fail("method type")
(repr(config_common.toolchain_type) == "<built-in method toolchain_type of config_common value>") or fail("method repr")
(repr(exec_group) == "<built-in function exec_group>" and repr(configuration_field) == "<built-in function configuration_field>") or fail("top-level repr")
(type(platform_common.TemplateVariableInfo({})) == "TemplateVariableInfo") or fail("template still implemented")
# ToolchainInfo (analysis/platform/ToolchainInfo.java:41-106): a struct with
# sorted keyword fields.
t = platform_common.ToolchainInfo(z = [1], a = "x", m = None)
(type(t) == "ToolchainInfo" and dir(t) == ["a", "m", "z"] and t.a == "x" and t.z == [1] and t.m == None) or fail("toolchain info")
(repr(t) == 'struct(a = "x", m = None, z = [1])' and str(platform_common.ToolchainInfo()) == "struct()") or fail(repr(t))
(t == platform_common.ToolchainInfo(a = "x", m = None, z = [1]) and t != platform_common.ToolchainInfo(a = "x")) or fail("equality")
(not hasattr(t, "b") and getattr(t, "b", 3) == 3) or fail("missing field")
(proto_common_do_not_use.INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION == True and dir(proto_common_do_not_use) == ["INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION"]) or fail("proto")
"#,
        );
        for (call, message) in [
            ("platform_common.ToolchainInfo(1)", "positional"),
            (
                "platform_common.ConstraintValueInfo()",
                "platform_common.ConstraintValueInfo is not implemented in the native profile",
            ),
            // ConfigFeatureFlagProvider.Constructor (crate::feature_flags).
            (
                "config_common.FeatureFlagInfo(value = 1)",
                "in call to FeatureFlagInfo(), parameter 'value' got value of type 'int', want 'string'",
            ),
            (
                "config_common.FeatureFlagInfo(valu = 'x')",
                "FeatureFlagInfo() got unexpected keyword argument 'valu' (did you mean 'value'?)",
            ),
            (
                "config_common.FeatureFlagInfo('a', 'b')",
                "FeatureFlagInfo() accepts no more than 1 positional argument but got 2",
            ),
            (
                "config_common.FeatureFlagInfo('a', value = 'b')",
                "FeatureFlagInfo() got multiple values for argument 'value'",
            ),
            // BuiltinFunction.java:420: every argument is added (an unknown or
            // duplicate keyword fails there) before the positional count.
            (
                "config_common.FeatureFlagInfo('a', 'b', valu = 'x')",
                "FeatureFlagInfo() got unexpected keyword argument 'valu' (did you mean 'value'?)",
            ),
            (
                "config_common.FeatureFlagInfo('a', 'b', value = 'x')",
                "FeatureFlagInfo() got multiple values for argument 'value'",
            ),
            (
                "config_common.config_feature_flag_transition('attr')",
                "config_common.config_feature_flag_transition is not implemented in the native profile",
            ),
            (
                "coverage_common.instrumented_files_info(None)",
                "in call to instrumented_files_info(), parameter 'ctx' got value of type 'NoneType', want 'ctx'",
            ),
            // ExecutionInfoProvider.constructor (crate::execution_info).
            (
                "testing.ExecutionInfo([])",
                "in call to ExecutionInfo(), parameter 'requirements' got value of type 'list', want 'dict'",
            ),
            (
                "testing.analysis_test(name = 'x', implementation = len)",
                "testing.analysis_test is not implemented in the native profile",
            ),
        ] {
            fails(FILE, call, message);
        }
    }

    #[test]
    fn testing_test_environment_is_a_run_environment_info() {
        pass(
            FILE,
            r#"
env = testing.TestEnvironment({"A": "1"}, ["HOME"])
(type(env) == "RunEnvironmentInfo" and env == RunEnvironmentInfo({"A": "1"}, ["HOME"])) or fail("value")
(testing.TestEnvironment(environment = {}).inherited_environment == []) or fail("default")
"#,
        );
        Module::with_temp_heap(|module| {
            let globals = crate::package::bzl_globals();
            let ast = AstModule::parse(
                "e.bzl",
                "t = testing.TestEnvironment({'A': '1'})\nr = RunEnvironmentInfo({'A': '1'})"
                    .to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            // StarlarkTestingModule passes shouldErrorOnNonExecutableRule=false.
            assert!(!crate::run_environment_info::errors_on_non_executable_rule(
                module.get("t").unwrap()
            ));
            assert!(crate::run_environment_info::errors_on_non_executable_rule(
                module.get("r").unwrap()
            ));
        });
        fails(
            FILE,
            "testing.TestEnvironment()",
            "TestEnvironment() missing 1 required positional argument: environment",
        );
        fails(
            FILE,
            "testing.TestEnvironment({}, [], 1)",
            "TestEnvironment() accepts no more than 2 positional arguments but got 3",
        );
    }

    #[test]
    fn builtin_provider_keys_are_attribute_requirements() {
        Module::with_temp_heap(|module| {
            let globals = crate::package::bzl_globals();
            let mut eval = Evaluator::new(&module);
            let ast = AstModule::parse(
                "p.bzl",
                "t = platform_common.ToolchainInfo\nx = apple_common.XcodeVersionConfig\nf = config_common.FeatureFlagInfo".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            eval.eval_module(ast, &globals).unwrap();
            let toolchain = crate::providers::requirement_info(module.get("t").unwrap()).unwrap();
            assert_eq!(toolchain.id, "builtin:ToolchainInfo");
            assert_eq!(toolchain.display_name, "ToolchainInfo");
            let xcode = crate::providers::requirement_info(module.get("x").unwrap()).unwrap();
            assert_eq!(
                xcode.id,
                "@@_builtins//:common/xcode/providers.bzl%XcodeVersionInfo"
            );
            assert_eq!(xcode.display_name, "XcodeVersionInfo");
            let flag = crate::providers::requirement_info(module.get("f").unwrap()).unwrap();
            assert_eq!(flag.id, "builtin:FeatureFlagInfo");
        });
    }

    #[test]
    fn apple_common_matches_apple_common_bazel() {
        pass(
            "@@rules_go+//go/platform:apple.bzl",
            r#"
(type(apple_common) == "struct") or fail("type")
(dir(apple_common) == ["Objc", "XcodeProperties", "XcodeVersionConfig", "apple_host_system_env", "apple_toolchain", "dotted_version", "new_objc_provider", "platform", "platform_type", "target_apple_env"]) or fail("dir")
(apple_common.XcodeProperties == None and apple_common.Objc == apple_common.new_objc_provider) or fail("aliases")
(type(apple_common.XcodeVersionConfig) == "Provider" and repr(apple_common.XcodeVersionConfig) == "<provider>") or fail("xcode provider")
(dir(apple_common.platform_type) == ["catalyst", "ios", "macos", "tvos", "visionos", "watchos"]) or fail("platform_type dir")
(apple_common.platform_type.ios == "ios" and apple_common.platform_type.watchos == "watchos") or fail("platform_type values")
(len(dir(apple_common.platform)) == 10) or fail("platform count")
_PLATFORMS = {
    "armv7-apple-ios": (apple_common.platform.ios_device, apple_common.platform_type.ios),
    "i386-apple-watchos": (apple_common.platform.watchos_simulator, apple_common.platform_type.watchos),
    "x86_64-apple-tvos": (apple_common.platform.tvos_simulator, apple_common.platform_type.tvos),
}
d = apple_common.platform.ios_device
(d == struct(is_device = True, name = "ios_device", name_in_plist = "iPhoneOS", platform_type = "ios")) or fail("ios_device")
# Field order is Bazel's sorted StarlarkInfo order (native struct repr spacing differs).
(dir(d) == ["is_device", "name", "name_in_plist", "platform_type"]) or fail(repr(d))
(apple_common.platform.watchos_simulator.name_in_plist == "WatchSimulator" and not apple_common.platform.tvos_simulator.is_device) or fail("simulators")
(apple_common.platform.catalyst.platform_type == "catalyst" and apple_common.platform.visionos_device.name_in_plist == "XROS") or fail("catalyst/visionos")
(type(apple_common.apple_toolchain) == "function") or fail("apple_toolchain type")
(repr(apple_common.apple_toolchain) == "<function lambda from @@_builtins//:common/objc/apple_common.bzl>") or fail("lambda repr")
apple_toolchain = apple_common.apple_toolchain()
(dir(apple_toolchain) == ["developer_dir", "platform_developer_framework_dir", "sdk_dir"]) or fail("toolchain dir")
(apple_toolchain.developer_dir() == "__BAZEL_XCODE_DEVELOPER_DIR__" and apple_toolchain.sdk_dir() == "__BAZEL_XCODE_SDKROOT__") or fail("dirs")
fragment = struct(single_arch_platform = apple_common.platform.ios_simulator)
(apple_toolchain.platform_developer_framework_dir(fragment) == "__BAZEL_XCODE_DEVELOPER_DIR__/Platforms/iPhoneSimulator.platform/Developer/Library/Frameworks") or fail("framework dir")
(apple_common.apple_toolchain() == apple_toolchain) or fail("struct equality")
"#,
        );
        for (call, message) in [
            (
                "apple_common.target_apple_env(None, apple_common.platform.macos)",
                "apple_common.target_apple_env is not implemented in the native profile (analysis-time Apple API)",
            ),
            (
                "apple_common.apple_host_system_env(None)",
                "apple_common.apple_host_system_env is not implemented",
            ),
            (
                "apple_common.apple_toolchain(1)",
                "lambda() does not accept positional arguments, but got 1",
            ),
            (
                "apple_common.apple_toolchain().platform_developer_framework_dir(struct())",
                "single_arch_platform",
            ),
        ] {
            fails(FILE, call, message);
        }
    }

    /// PackageIdentifier.getRunfilesPath (paired capture
    /// tests/fixtures/py_label_runfiles_path).
    #[test]
    fn py_label_repo_runfiles_path_is_the_package_runfiles_path() {
        for (label, expected) in [
            ("//tools/build_defs/python:probe", "tools/build_defs/python"),
            ("//:BUILD.bazel", ""),
            ("@@ext+//data:e.txt", "../ext+/data"),
            ("@@ext+//:root", "../ext+"),
            ("@@bazel_tools//tools/test:x", "../bazel_tools/tools/test"),
        ] {
            let label = crate::label::Label::parse_canonical(label).unwrap();
            assert_eq!(super::package_runfiles_path(&label), expected);
        }
    }

    #[test]
    fn py_internal_java_common_and_package_specification_shapes() {
        pass(
            "@@rules_python+//python/private:py_internal.bzl",
            r#"
(type(py_internal) == "struct" and len(dir(py_internal)) == 26) or fail("py_internal fields")
(dir(py_internal)[:4] == ["PackageSpecificationInfo", "add_py_extra_pseudo_action", "are_action_listeners_enabled", "cc_helper"]) or fail("sorted")
(py_internal.PackageSpecificationInfo == PackageSpecificationInfo) or fail("PackageSpecificationInfo identity")
(getattr(py_internal, "PackageSpecificationInfo", None) == PackageSpecificationInfo) or fail("getattr")
(type(py_internal.cc_helper.is_valid_shared_library_artifact) == "function") or fail("cc_helper")
(repr(py_internal.regex_match) == "<function _regex_match from @@_builtins//:common/python/py_internal.bzl>") or fail("repr")
py_internal_renamed = py_internal
(type(java_common) == "struct" and dir(java_common) == ["internal_DO_NOT_USE"]) or fail("java_common")
"#,
        );
        fails(
            "@@rules_python+//python/private:py_internal.bzl",
            "py_internal.get_rule_name(None)",
            "py_internal.get_rule_name is not implemented in the native profile (analysis-time Python API)",
        );
    }

    #[test]
    fn toolchain_type_and_exec_group_values() {
        pass(
            "@@rules_cc+//cc:find_cc_toolchain.bzl",
            r#"
t = config_common.toolchain_type("//cc:toolchain_type", mandatory = False)
(type(t) == "toolchain_type" and dir(t) == ["mandatory", "toolchain_type"]) or fail("shape")
(t.toolchain_type == Label("@@rules_cc+//cc:toolchain_type") and t.mandatory == False) or fail(str(t.toolchain_type))
(repr(t) == "<unknown object com.google.devtools.build.lib.analysis.config.ToolchainTypeRequirement>") or fail("repr")
m = config_common.toolchain_type(Label("@@bazel_tools//tools/cpp:toolchain_type"))
(m.mandatory and str(m.toolchain_type) == "@@bazel_tools//tools/cpp:toolchain_type") or fail("label + default")
(t == config_common.toolchain_type("//cc:toolchain_type", mandatory = False) and t != m) or fail("equality")
g = exec_group(toolchains = [t, "//cc:toolchain_type", m], exec_compatible_with = ["//cc:constraint"])
(type(g) == "exec_group" and dir(g) == []) or fail("exec_group")
(repr(g) == "<unknown object com.google.devtools.build.lib.packages.DeclaredExecGroup>") or fail("exec_group repr")
(g == exec_group(toolchains = ["//cc:toolchain_type", m], exec_compatible_with = ["//cc:constraint", ":constraint"])) or fail("dedupe keeps strictest")
(exec_group() == exec_group(toolchains = [], exec_compatible_with = [])) or fail("defaults")
groups = {"test": exec_group(), "cpp_link": exec_group(toolchains = [t])}
"#,
        );
        for (call, message) in [
            (
                "config_common.toolchain_type(1)",
                "in call to toolchain_type(), parameter 'name' got value of type 'int', want 'string or Label'",
            ),
            (
                "config_common.toolchain_type(None)",
                "in call to toolchain_type(), parameter 'name' got value of type 'NoneType', want 'string or Label'",
            ),
            (
                "config_common.toolchain_type(name = '//x')",
                "toolchain_type() got named argument for positional-only parameter 'name'",
            ),
            (
                "config_common.toolchain_type('//x', 'y')",
                "toolchain_type() accepts no more than 1 positional argument but got 2",
            ),
            (
                "config_common.toolchain_type('//x', mandatory = 'y')",
                "in call to toolchain_type(), parameter 'mandatory' got value of type 'string', want 'bool'",
            ),
            (
                "config_common.toolchain_type('//x:a:b')",
                "Unable to parse toolchain_type label '//x:a:b'",
            ),
            (
                "exec_group([])",
                "exec_group() got unexpected positional argument",
            ),
            (
                "exec_group(toolchains = [1])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a Int32",
            ),
            (
                "exec_group(toolchains = [None])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a NoneType",
            ),
            (
                "exec_group(toolchains = [[]])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a MutableStarlarkList",
            ),
            (
                "exec_group(toolchains = [struct()])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a StarlarkInfoNoSchema",
            ),
            (
                "exec_group(toolchains = [True])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a Boolean",
            ),
            (
                "exec_group(toolchains = [(1,)])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a SingletonTuple",
            ),
            (
                "exec_group(toolchains = [4294967296])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a Int64",
            ),
            (
                "exec_group(exec_compatible_with = ['//x:c', Label('//x:d')])",
                "at index 1 of exec_compatible_with, got element of type Label, want string",
            ),
            (
                "exec_group(exec_compatible_with = [1, '//x:a:b'])",
                "at index 0 of exec_compatible_with, got element of type int, want string",
            ),
            (
                "exec_group(exec_compatible_with = ['//x:c', '//x:a:b'])",
                "Unable to parse label '//x:a:b' in attribute 'exec_compatible_with': ",
            ),
            (
                "exec_group(exec_compatible_with = 'x')",
                "in call to exec_group(), parameter 'exec_compatible_with' got value of type 'string', want 'sequence'",
            ),
            (
                "exec_group(toolchains = 'x')",
                "in call to exec_group(), parameter 'toolchains' got value of type 'string', want 'sequence'",
            ),
            (
                "exec_group(tools = [])",
                "exec_group() got unexpected keyword argument 'tools'",
            ),
        ] {
            fails(FILE, call, message);
        }
    }

    #[test]
    fn configuration_field_accepts_registered_fields_and_rejects_others() {
        let mut accepted = String::new();
        for (fragment, fields) in super::CONFIGURATION_FIELDS {
            for field in fields {
                accepted.push_str(&format!(
                    "x = configuration_field(fragment = {fragment:?}, name = {field:?})\n(type(x) == \"LateBoundDefault\" and repr(x) == \"<late-bound default>\") or fail({field:?})\n"
                ));
            }
        }
        accepted.push_str(
            "(configuration_field('cpp', 'libc_top') == configuration_field(fragment = 'cpp', name = 'libc_top')) or fail('eq')\n",
        );
        pass(FILE, &accepted);
        // Every field rules_cc, rules_java and protobuf request in the app closure.
        let required = [
            ("cpp", "cs_fdo_profile"),
            ("cpp", "custom_malloc"),
            ("cpp", "fdo_optimize"),
            ("cpp", "fdo_prefetch_hints"),
            ("cpp", "fdo_profile"),
            ("cpp", "libc_top"),
            ("cpp", "memprof_profile"),
            ("cpp", "propeller_optimize"),
            ("cpp", "proto_profile_path"),
            ("cpp", "xbinary_fdo"),
            ("cpp", "zipper"),
            ("coverage", "output_generator"),
            ("apple", "xcode_config_label"),
            ("java", "java_toolchain_bytecode_optimizer"),
            ("java", "local_java_optimization_configuration"),
            ("proto", "proto_compiler"),
            ("proto", "proto_toolchain_for_java"),
            ("proto", "proto_toolchain_for_java_lite"),
        ];
        for (fragment, name) in required {
            pass(
                FILE,
                &format!("configuration_field({fragment:?}, {name:?})"),
            );
        }
        for (call, message) in [
            (
                "configuration_field('nope', 'libc_top')",
                "invalid configuration fragment name 'nope'",
            ),
            (
                "configuration_field('cpp', 'nope')",
                "invalid configuration field name 'nope' on fragment 'cpp'",
            ),
            (
                "configuration_field('objc', 'xcode_config_label')",
                "invalid configuration field name 'xcode_config_label' on fragment 'objc'",
            ),
            (
                "configuration_field('test', 'x')",
                "invalid configuration fragment name 'test'",
            ),
            (
                "configuration_field(fragment = 'cpp')",
                "configuration_field() missing 1 required positional argument: name",
            ),
            (
                "configuration_field(1, 'x')",
                "in call to configuration_field(), parameter 'fragment' got value of type 'int', want 'string'",
            ),
        ] {
            fails(FILE, call, message);
        }
        // Late-bound defaults are accepted and retained at load time (the
        // load-declaration surface); targets using them fail at analysis.
        pass(
            FILE,
            "_libc = attr.label(default = configuration_field(fragment = 'cpp', name = 'libc_top'))",
        );
        // StarlarkRuleClassFunctions attribute check: public late-bound attrs fail.
        fails(
            FILE,
            "def _i(ctx):\n    pass\nr = rule(_i, attrs = {'late': attr.label(default = configuration_field('cpp', 'libc_top'))})",
            "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found 'late'",
        );
        let outside = run_with(
            &crate::package::bzl_globals(),
            FILE,
            "configuration_field('cpp', 'libc_top')",
            false,
        )
        .unwrap_err();
        assert!(
            outside.contains("configuration_field() can only be used during .bzl initialization (top-level evaluation)"),
            "{outside}"
        );
    }

    #[test]
    fn module_phase_bazel_version_selects_the_rules_cc_compatibility_proxy() {
        // rules_cc cc/extensions.bzl: _bazel_version_ge("9.0.0-pre.20250911")
        // via bazel_features private/parse.bzl on native.bazel_version.
        let source = r#"
def _partition(s):
    for i in range(len(s)):
        c = s[i]
        if c == "-":
            return s[:i], s[i + 1:]
        if not c.isdigit() and c != ".":
            return s[:i], s[i:]
    return s, ""
def parse_version(v):
    release, prerelease = _partition(v.partition(" ")[0])
    return [int(s) for s in release.split(".")], not prerelease, prerelease
(parse_version(native.bazel_version) >= parse_version("9.0.0-pre.20250911")) or fail(native.bazel_version)
"#;
        run_with(&crate::package::module_bzl_globals(), FILE, source, true).unwrap();
        assert!(crate::COMPATIBILITY_VERSION.starts_with("9."));
    }
}
