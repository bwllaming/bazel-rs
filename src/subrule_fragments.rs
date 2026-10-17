//! `subrule_ctx.fragments` (StarlarkSubrule.SubruleFragmentCollection,
//! analysis/starlark/StarlarkSubrule.java:562-597) and the `cpp`
//! configuration fragment (rules/cpp/CppConfiguration.java,
//! starlarkbuildapi/cpp/CppConfigurationApi.java). Paths below are relative to
//! `src/main/java/com/google/devtools/build/lib/` of the pinned Bazel 9.3.
use allocative::Allocative;
use anyhow::bail;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::{
    FrozenHeap, FrozenHeapRef, FrozenValue, Heap, NoSerialize, ProvidesStaticType,
    StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLike, starlark_value,
};
use std::fmt;
use std::sync::OnceLock;

use crate::analysis_runtime::ScopeHandle;
use crate::custom_rule::CppConfigurationView;

/// The Starlark-visible fragments of a Bazel 9.3 configuration
/// (BuildConfigurationValue.getStarlarkFragmentByName,
/// analysis/config/BuildConfigurationValue.java:360-370, 980-982): every
/// registered fragment with a StarlarkBuiltin name (bazel/rules/
/// BazelRuleClassProvider.java:253-344) except `py` and `bazel_py`, which
/// Fragment.shouldInclude drops (rules/python/PythonConfiguration.java:84-87,
/// bazel/rules/python/BazelPythonConfiguration.java:119-122; observed absent
/// in the paired capture B5/fragments/undeclared_py).
pub(crate) const STARLARK_FRAGMENTS: [&str; 10] = [
    "android",
    "apple",
    "bazel_android",
    "coverage",
    "cpp",
    "j2objc",
    "java",
    "objc",
    "platform",
    "proto",
];

/// BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST
/// (packages/BuiltinRestriction.java:36-104): (apparent repository, package prefix).
pub(crate) const INTERNAL_STARLARK_API_ALLOWLIST: [(&str, &str); 29] = [
    ("", "test"),
    ("", "bazel_internal/test_rules"),
    ("", "tools/build_defs/build_info"),
    ("bazel_tools", "tools/build_defs/build_info"),
    ("", "bazel_internal/test_rules/cc"),
    ("", "tools/build_defs/android"),
    ("", "third_party/bazel_rules/rules_android"),
    ("rules_android", ""),
    ("build_bazel_rules_android", ""),
    ("", "third_party/apple_crosstool"),
    ("", "third_party/cpptoolchains/portable_llvm/build_defs"),
    ("", "third_party/bazel_rules/rules_apple"),
    ("rules_apple", ""),
    ("build_bazel_rules_apple", ""),
    ("", "third_party/bazel_rules/rules_cc"),
    ("", "tools/build_defs/cc"),
    ("rules_cc", ""),
    ("", "third_party/bazel_rules/rules_java/java"),
    ("rules_java", "java"),
    ("", "third_party/bazel_rules/rules_rust/rust/private"),
    ("", "third_party/crubit"),
    ("rules_rust", "rust/private"),
    ("", "third_party/gpus/cuda"),
    ("", "tools/build_defs/packaging"),
    ("", "tools/build_defs/go"),
    ("", "third_party/protobuf"),
    ("protobuf", ""),
    ("com_google_protobuf", ""),
    ("rules_shell", ""),
];

/// The 49 CppConfiguration Starlark members, in `dir()` order.
#[cfg(test)]
pub(crate) const CPP_MEMBERS: [&str; 49] = [
    "_dont_enable_host_nonhost",
    "_fdo_prefetch_hints_label",
    "apple_generate_dsym",
    "build_test_dwp",
    "compilation_mode",
    "conlyopts",
    "copts",
    "cs_fdo_instrument",
    "cs_fdo_path",
    "custom_malloc",
    "cxxopts",
    "disable_nocopts",
    "do_not_use_macos_set_install_name",
    "dynamic_mode",
    "experimental_cc_implementation_deps",
    "experimental_cpp_modules",
    "experimental_link_static_libraries_once",
    "fdo_instrument",
    "fdo_path",
    "fission_active_for_current_compilation_mode",
    "force_pic",
    "generate_llvm_lcov",
    "grte_top",
    "include_scanning",
    "incompatible_remove_legacy_whole_archive",
    "incompatible_use_specific_tool_files",
    "interface_shared_objects",
    "legacy_whole_archive",
    "linkopts",
    "lto_backend_options",
    "lto_index_options",
    "minimum_os_version",
    "objc_enable_binary_stripping",
    "objc_generate_linkmap",
    "objc_should_generate_dotd_files",
    "objc_should_strip_binary",
    "objccopts",
    "process_headers_in_dependencies",
    "propeller_optimize_absolute_cc_profile",
    "propeller_optimize_absolute_ld_profile",
    "proto_profile",
    "save_feature_state",
    "save_temps",
    "share_native_deps",
    "should_generate_dotd_files",
    "should_strip_binaries",
    "start_end_lib",
    "strip_opts",
    "use_llvm_coverage_map_format",
];

#[allow(unsafe_code)]
mod values {
    use super::*;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct FragmentCollection {
        /// The exported subrule name (StarlarkSubrule.getName).
        #[trace(static)]
        pub subrule: String,
        /// `subrule(fragments=...)`, deduplicated in declaration order.
        #[trace(static)]
        pub declared: Vec<String>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        /// The configuration the consuming target is analyzed in.
        #[allocative(skip)]
        #[trace(static)]
        pub cpp: CppConfigurationView,
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct CppFragment {
        #[allocative(skip)]
        pub view: CppConfigurationView,
    }
}
use values::{CppFragment, FragmentCollection};

starlark::starlark_simple_value!(CppFragment);

// Neither SubruleFragmentCollection nor Fragment overrides StarlarkValue.repr
// (net/starlark/java/eval/StarlarkValue.java:50-52).
impl fmt::Display for FragmentCollection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "<unknown object com.google.devtools.build.lib.analysis.starlark.StarlarkSubrule$SubruleFragmentCollection>",
        )
    }
}

impl fmt::Display for CppFragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<unknown object com.google.devtools.build.lib.rules.cpp.CppConfiguration>")
    }
}

starlark::methods_static!(COLLECTION_METHODS = collection_methods);
starlark::methods_static!(CPP_METHODS = cpp_methods);

#[starlark_value(type = "fragments")]
impl<'v> StarlarkValue<'v> for FragmentCollection {
    fn get_methods() -> Option<&'static Methods> {
        Some(COLLECTION_METHODS.methods())
    }
}

#[starlark_value(type = "cpp")]
impl<'v> StarlarkValue<'v> for CppFragment {
    fn get_methods() -> Option<&'static Methods> {
        Some(CPP_METHODS.methods())
    }
}

fn collection<'v>(this: Value<'v>) -> anyhow::Result<&'v FragmentCollection> {
    let collection = this
        .downcast_ref::<FragmentCollection>()
        .ok_or_else(|| anyhow::anyhow!("expected subrule fragments"))?;
    collection.scope.check_current()?;
    Ok(collection)
}

/// The undeclared-fragment error of SubruleFragmentCollection.getValue
/// (StarlarkSubrule.java:578-584).
fn require_declared(collection: &FragmentCollection, name: &str) -> anyhow::Result<()> {
    if !collection.declared.iter().any(|declared| declared == name) {
        bail!(
            "{} has to declare '{name}' as a required fragment in order to access it. Please update the 'fragments' argument of the subrule definition (for example: fragments = [\"{name}\"])",
            collection.subrule
        );
    }
    Ok(())
}

/// SubruleFragmentCollection.getValue (StarlarkSubrule.java:570-586). In
/// the no-config configuration no fragment class exists, so getValue returns
/// null before the declaration check and the field access fails with
/// FragmentCollectionApi.getErrorMessageForUnknownField
/// (starlarkbuildapi/FragmentCollectionApi.java:39-49), listing the
/// collection's field names (the declared fragments, StarlarkSubrule.java:588-591).
fn fragment<'v>(this: Value<'v>, name: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let collection = collection(this)?;
    if collection.cpp.no_fragments {
        bail!(
            "There is no configuration fragment named '{name}'. Available fragments: '{}'",
            collection.declared.join("', '")
        );
    }
    require_declared(collection, name)?;
    match name {
        "cpp" => Ok(heap.alloc(CppFragment {
            view: collection.cpp,
        })),
        _ => bail!(
            "configuration fragment '{name}' is not implemented in subrule contexts by the native profile"
        ),
    }
}

#[starlark_module]
fn collection_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn android<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "android", heap)
    }
    #[starlark(attribute)]
    fn apple<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "apple", heap)
    }
    #[starlark(attribute)]
    fn bazel_android<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "bazel_android", heap)
    }
    #[starlark(attribute)]
    fn coverage<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "coverage", heap)
    }
    #[starlark(attribute)]
    fn cpp<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "cpp", heap)
    }
    #[starlark(attribute)]
    fn j2objc<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "j2objc", heap)
    }
    #[starlark(attribute)]
    fn java<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "java", heap)
    }
    #[starlark(attribute)]
    fn objc<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "objc", heap)
    }
    #[starlark(attribute)]
    fn platform<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "platform", heap)
    }
    #[starlark(attribute)]
    fn proto<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        fragment(this, "proto", heap)
    }
}

/// `hasattr` and `dir` of a Structure (net/starlark/java/eval/Starlark.java
/// hasattr/dir over Structure.getValue/getFieldNames): the fragment
/// collection's fields are the DECLARED fragments, and `hasattr` of an
/// undeclared Starlark fragment raises getValue's error. The interpreter's
/// `hasattr`/`dir` never call attribute getters, so the `.bzl` globals route
/// these two builtins through here first.
fn structure_hasattr(value: Value<'_>, name: &str) -> Option<anyhow::Result<bool>> {
    if let Some(result) = crate::ctx_surface::fragments_hasattr(value, name) {
        return Some(result);
    }
    value.downcast_ref::<FragmentCollection>()?;
    Some((|| {
        let collection = collection(value)?;
        // No fragment exists in the no-config configuration: getValue is null.
        if !STARLARK_FRAGMENTS.contains(&name) || collection.cpp.no_fragments {
            return Ok(false);
        }
        require_declared(collection, name)?;
        Ok(true)
    })())
}

fn structure_dir(value: Value<'_>) -> Option<Vec<String>> {
    if let Some(names) = crate::ctx_surface::fragments_dir(value) {
        return Some(names);
    }
    let collection = value.downcast_ref::<FragmentCollection>()?;
    let mut names = collection.declared.clone();
    names.sort();
    Some(names)
}

/// `.bzl` overrides of the `hasattr` and `dir` builtins (see structure_hasattr).
#[starlark_module]
pub(crate) fn structure_builtins(builder: &mut GlobalsBuilder) {
    fn hasattr<'v>(
        #[starlark(require = pos)] a: Value<'v>,
        #[starlark(require = pos)] attr: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<bool> {
        match structure_hasattr(a, attr) {
            Some(result) => result,
            None => Ok(a.has_attr(attr, heap)),
        }
    }
    fn dir<'v>(#[starlark(require = pos)] x: Value<'v>) -> anyhow::Result<Vec<String>> {
        Ok(structure_dir(x).unwrap_or_else(|| x.dir_attr()))
    }
}

/// CcModule.checkPrivateStarlarkificationAllowlist (rules/cpp/CcModule.java:189-192):
/// BuiltinRestriction.failIfCalledOutsideDefaultAllowlist for the calling `.bzl`.
pub(crate) fn private_api(eval: &Evaluator<'_, '_, '_>) -> starlark::Result<()> {
    crate::cc_common::check_private_api(eval, 0, &allowlist())
}

/// CppConfiguration.checkInExpandedApiAllowlist (rules/cpp/CppConfiguration.java:899-906).
fn expanded_api(eval: &Evaluator<'_, '_, '_>, feature: &str) -> starlark::Result<()> {
    private_api(eval).map_err(|error| {
        starlark::Error::new_other(anyhow::anyhow!(
            "{} (feature '{feature}' in CppConfiguration)",
            error
        ))
    })
}

fn allowlist() -> Vec<(String, String)> {
    INTERNAL_STARLARK_API_ALLOWLIST
        .iter()
        .map(|(repository, prefix)| ((*repository).to_owned(), (*prefix).to_owned()))
        .collect()
}

fn receiver<'v>(this: Value<'v>) -> anyhow::Result<&'v CppFragment> {
    this.downcast_ref::<CppFragment>()
        .ok_or_else(|| anyhow::anyhow!("expected cpp fragment"))
}

/// The immutable empty list Bazel returns for an empty ImmutableList member
/// (Starlark.fromJava: StarlarkList.immutableCopyOf).
pub(crate) fn empty_list<'v>() -> Value<'v> {
    static EMPTY: OnceLock<(FrozenHeapRef, FrozenValue)> = OnceLock::new();
    let (_, value) = EMPTY.get_or_init(|| {
        let heap = FrozenHeap::new();
        let value = heap.alloc(starlark::values::list::AllocList::EMPTY);
        (heap.into_ref(), value)
    });
    value.to_value()
}

/// The immutable `["-g0"]` of an exec configuration's `--copt`/`--cxxopt`
/// (builtin_exec_platforms.bzl:341-342 on non-Windows hosts).
fn exec_g0<'v>() -> Value<'v> {
    static G0: OnceLock<(FrozenHeapRef, FrozenValue)> = OnceLock::new();
    let (_, value) = G0.get_or_init(|| {
        let heap = FrozenHeap::new();
        let value = heap.alloc(starlark::values::list::AllocList(["-g0"]));
        (heap.into_ref(), value)
    });
    value.to_value()
}

/// The CppConfiguration members (49). Every value is the Bazel 9.3 option
/// default, except those derived from options this profile models
/// (`compilation_mode`, `force_pic`, `should_strip_binaries`).
#[starlark_module]
fn cpp_methods(builder: &mut MethodsBuilder) {
    /// CppConfiguration.java:773-776; CppOptions --incompatible_dont_enable_host_nonhost_crosstool_features default true.
    #[starlark(attribute)]
    fn _dont_enable_host_nonhost<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        receiver(this)?;
        Ok(true)
    }
    /// CppConfiguration.java:683-694; --fdo_prefetch_hints default null.
    #[starlark(attribute)]
    fn _fdo_prefetch_hints_label<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        receiver(this)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:305, 809-812; --apple_generate_dsym default false.
    #[starlark(attribute)]
    fn apple_generate_dsym<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        receiver(this)?;
        Ok(false)
    }
    /// CppConfiguration.java:443-451; --build_test_dwp default false.
    fn build_test_dwp<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:326-330.
    fn compilation_mode<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<&'static str> {
        let cpp = receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(cpp.view.compilation_mode)
    }
    /// CppConfiguration.java:291, 535-537; --conlyopt.
    #[starlark(attribute)]
    fn conlyopts<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        receiver(this)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:292, 515-527; --copt, --experimental_omitfp default false.
    #[starlark(attribute)]
    fn copts<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(if receiver(this)?.view.exec {
            exec_g0()
        } else {
            empty_list()
        })
    }
    /// CppConfiguration.java:637-646; --cs_fdo_instrument default null.
    fn cs_fdo_instrument<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:648-657; --cs_fdo_absolute_path default null.
    fn cs_fdo_path<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:424-430; --custom_malloc default null.
    #[starlark(attribute)]
    fn custom_malloc<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        receiver(this)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:293, 529-531; --cxxopt.
    #[starlark(attribute)]
    fn cxxopts<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(if receiver(this)?.view.exec {
            exec_g0()
        } else {
            empty_list()
        })
    }
    /// CppConfiguration.java:799-807; --incompatible_disable_nocopts default true.
    fn disable_nocopts<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:894-897 (always true).
    #[starlark(attribute)]
    fn do_not_use_macos_set_install_name<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        receiver(this)?;
        Ok(true)
    }
    /// CppConfiguration.java:359-365; --dynamic_mode default 'default' (DynamicMode.name()).
    fn dynamic_mode<'v>(this: Value<'v>) -> anyhow::Result<&'static str> {
        receiver(this)?;
        Ok("DEFAULT")
    }
    /// CppConfiguration.java:866-884; default true.
    fn experimental_cc_implementation_deps<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:876-888; default false.
    fn experimental_cpp_modules<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:474-476, 946-951; default true.
    fn experimental_link_static_libraries_once<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:920-925; --fdo_instrument default null.
    fn fdo_instrument<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        expanded_api(eval, "fdo_instrument")?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:621-630; --fdo_optimize default null.
    fn fdo_path<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:438-440, 939-944; --fission default no.
    fn fission_active_for_current_compilation_mode<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:459-461, 908-912.
    fn force_pic<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<bool> {
        let cpp = receiver(this).map_err(starlark::Error::new_other)?;
        expanded_api(eval, "force_pic")?;
        Ok(cpp.view.force_pic)
    }
    /// CppConfiguration.java:914-918; --experimental_generate_llvm_lcov default false.
    fn generate_llvm_lcov<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        expanded_api(eval, "generate_llvm_lcov")?;
        Ok(false)
    }
    /// CppConfiguration.java:756-765; --grte_top default null.
    fn grte_top<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:823-831; include scanning options default false.
    fn include_scanning<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:484-488; default true.
    fn incompatible_remove_legacy_whole_archive<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:790-794; default true.
    fn incompatible_use_specific_tool_files<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:501-505; --interface_shared_objects default true.
    fn interface_shared_objects<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:478-482; --legacy_whole_archive default true.
    fn legacy_whole_archive<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:174-178, 295, 547-549; --linkopt, --experimental_omitfp default false.
    #[starlark(attribute)]
    fn linkopts<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        receiver(this)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:297, 340-343; --ltobackendopt.
    #[starlark(attribute)]
    fn lto_backend_options<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        receiver(this)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:296, 332-337; --ltoindexopt.
    fn lto_index_options<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:345-352; --minimum_os_version default null.
    fn minimum_os_version<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        receiver(this)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:857-861; default false.
    fn objc_enable_binary_stripping<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:849-851; default false.
    #[starlark(attribute)]
    fn objc_generate_linkmap<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        receiver(this)?;
        Ok(false)
    }
    /// CppConfiguration.java:839-843; --objc_use_dotd_pruning default true.
    fn objc_should_generate_dotd_files<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:953-956 (objc_enable_binary_stripping default false).
    #[starlark(attribute)]
    fn objc_should_strip_binary<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        receiver(this)?;
        Ok(false)
    }
    /// CppConfiguration.java:294, 541-543; --objccopt.
    #[starlark(attribute)]
    fn objccopts<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        receiver(this)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:927-931; default false.
    fn process_headers_in_dependencies<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:659-669.
    fn propeller_optimize_absolute_cc_profile<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:671-681.
    fn propeller_optimize_absolute_ld_profile<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(NoneType)
    }
    /// CppConfiguration.java:963-967; --proto_profile default true.
    fn proto_profile<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:933-937; default false.
    fn save_feature_state<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:395-404; --save_temps default false.
    fn save_temps<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
    /// CppConfiguration.java:608-610, 767-771; --share_native_deps default true.
    fn share_native_deps<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:833-837; --cc_dotd_files default true.
    fn should_generate_dotd_files<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:299-302, 371-379 (--strip, compilation mode).
    fn should_strip_binaries<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let cpp = receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(cpp.view.should_strip_binaries())
    }
    /// CppConfiguration.java:463-472; --start_end_lib default true.
    fn start_end_lib<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(true)
    }
    /// CppConfiguration.java:385-393; --stripopt.
    fn strip_opts<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(empty_list())
    }
    /// CppConfiguration.java:740-751; default false.
    fn use_llvm_coverage_map_format<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        receiver(this).map_err(starlark::Error::new_other)?;
        private_api(eval)?;
        Ok(false)
    }
}

/// A `cpp` fragment of the given configuration (rule `ctx.fragments.cpp`).
pub(crate) fn alloc_cpp(heap: Heap<'_>, view: CppConfigurationView) -> Value<'_> {
    heap.alloc(CppFragment { view })
}

/// A fresh `subrule_ctx.fragments` for one subrule invocation.
pub(crate) fn alloc_collection<'v>(
    heap: Heap<'v>,
    subrule: &str,
    declared: &[String],
    scope: ScopeHandle,
    cpp: CppConfigurationView,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(FragmentCollection {
        subrule: subrule.to_owned(),
        declared: declared.to_vec(),
        scope,
        cpp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn view(mode: &'static str) -> CppConfigurationView {
        CppConfigurationView {
            compilation_mode: mode,
            ..Default::default()
        }
    }

    fn evaluate(file: &str, cpp: CppConfigurationView, expression: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            module.set("cpp", module.heap().alloc(CppFragment { view: cpp }));
            let mut eval = Evaluator::new(&module);
            let source = format!("def f():\n    return repr({expression})\nresult = f()\n");
            eval.eval_module(
                AstModule::parse(file, source, &Dialect::Standard).unwrap(),
                &Globals::standard(),
            )
            .map_err(|error| error.to_string())?;
            Ok(module
                .get("result")
                .unwrap()
                .unpack_str()
                .unwrap()
                .to_owned())
        })
    }

    const PRIVATE: &str = "(type(cpp), cpp.compilation_mode(), cpp.fdo_path(), cpp.cs_fdo_path(), cpp.propeller_optimize_absolute_cc_profile(), cpp.propeller_optimize_absolute_ld_profile(), cpp.proto_profile())";

    #[test]
    fn cpp_members_are_allowlisted_private_api_with_native_defaults() {
        assert_eq!(
            evaluate(
                "@@rules_cc+//cc/private/rules_impl/fdo:fdo_context.bzl",
                view("opt"),
                PRIVATE
            )
            .unwrap(),
            "(\"cpp\", \"opt\", None, None, None, None, True)"
        );
        assert_eq!(
            evaluate("@@_builtins//:common/cc/x.bzl", view("fastbuild"), PRIVATE).unwrap(),
            "(\"cpp\", \"fastbuild\", None, None, None, None, True)"
        );
        for file in ["//pkg:defs.bzl", "@@rules_java+//other:defs.bzl"] {
            let error = evaluate(file, view("dbg"), PRIVATE).unwrap_err();
            assert!(
                error.contains(&format!("file '{file}' cannot use private API")),
                "{error}"
            );
        }
        assert!(evaluate("@@rules_java+//java/common:x.bzl", view("dbg"), PRIVATE).is_ok());
        // CppConfiguration.checkInExpandedApiAllowlist (CppConfiguration.java:899-906).
        let error = evaluate("//pkg:defs.bzl", view("dbg"), "cpp.force_pic()").unwrap_err();
        assert!(
            error.contains("file '//pkg:defs.bzl' cannot use private API (feature 'force_pic' in CppConfiguration)"),
            "{error}"
        );
    }

    #[test]
    fn cpp_dir_public_members_and_option_derived_values() {
        let members = evaluate("//pkg:defs.bzl", view("fastbuild"), "dir(cpp)").unwrap();
        let expected = format!(
            "[{}]",
            CPP_MEMBERS.map(|name| format!("\"{name}\"")).join(", ")
        );
        assert_eq!(members, expected);
        assert_eq!(
            evaluate(
                "//pkg:defs.bzl",
                view("opt"),
                "(cpp.copts, cpp.cxxopts, cpp.conlyopts, cpp.objccopts, cpp.linkopts, cpp.lto_backend_options, cpp.custom_malloc, cpp._fdo_prefetch_hints_label, cpp._dont_enable_host_nonhost, cpp.apple_generate_dsym, cpp.objc_generate_linkmap, cpp.objc_should_strip_binary, cpp.do_not_use_macos_set_install_name, cpp.minimum_os_version(), cpp.dynamic_mode())"
            )
            .unwrap(),
            "([], [], [], [], [], [], None, None, True, False, False, False, True, None, \"DEFAULT\")"
        );
        // Immutable like Bazel's StarlarkList.immutableCopyOf.
        assert!(evaluate("//pkg:defs.bzl", view("opt"), "cpp.copts.append(1)").is_err());
        let rules_cc = "@@rules_cc+//cc:x.bzl";
        for (cpp, expected) in [
            (view("fastbuild"), "(True, False)"),
            (view("opt"), "(False, False)"),
            (
                CppConfigurationView {
                    strip: "always",
                    force_pic: true,
                    ..view("opt")
                },
                "(True, True)",
            ),
            (
                CppConfigurationView {
                    strip: "never",
                    ..view("fastbuild")
                },
                "(False, False)",
            ),
        ] {
            assert_eq!(
                evaluate(
                    rules_cc,
                    cpp,
                    "(cpp.should_strip_binaries(), cpp.force_pic())"
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn collection_fields_are_declared_fragments_with_bazel_hasattr_and_dir() {
        let scope = crate::analysis_runtime::ScopeState::new("rule".into(), Default::default());
        Module::with_temp_heap(|module| {
            let collection = alloc_collection(
                module.heap(),
                "S",
                &["java".into(), "cpp".into()],
                scope.root(),
                view("fastbuild"),
            );
            module.set("f", collection);
            let mut builder = GlobalsBuilder::standard();
            structure_builtins(&mut builder);
            let globals = builder.build();
            let mut eval = Evaluator::new(&module);
            eval.eval_module(
                AstModule::parse(
                    "//pkg:defs.bzl",
                    "r = repr((dir(f), hasattr(f, 'cpp'), hasattr(f, 'nope'), hasattr(f, 'py'), getattr(f, 'nope', 'dflt'), type(f.cpp)))\n".into(),
                    &Dialect::Standard,
                )
                .unwrap(),
                &globals,
            )
            .unwrap();
            assert_eq!(
                module.get("r").unwrap().unpack_str().unwrap(),
                "([\"cpp\", \"java\"], True, False, False, \"dflt\", \"cpp\")"
            );
            for expression in [
                "hasattr(f, 'apple')",
                "f.apple",
                "getattr(f, 'apple', None)",
            ] {
                let error = eval
                    .eval_module(
                        AstModule::parse("//pkg:defs.bzl", expression.into(), &Dialect::Standard)
                            .unwrap(),
                        &globals,
                    )
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("S has to declare 'apple' as a required fragment"),
                    "{expression}: {error}"
                );
            }
        });
        scope.finish();
        // N28c: in the no-config configuration no fragment exists: hasattr is
        // False, and a field access fails with FragmentCollectionApi's
        // unknown-field message over the declared fields, before the
        // declaration check.
        let scope = crate::analysis_runtime::ScopeState::new("rule".into(), Default::default());
        Module::with_temp_heap(|module| {
            let collection = alloc_collection(
                module.heap(),
                "S",
                &["cpp".into()],
                scope.root(),
                CppConfigurationView {
                    no_fragments: true,
                    ..view("fastbuild")
                },
            );
            module.set("f", collection);
            let mut builder = GlobalsBuilder::standard();
            structure_builtins(&mut builder);
            let globals = builder.build();
            let mut eval = Evaluator::new(&module);
            eval.eval_module(
                AstModule::parse(
                    "//pkg:defs.bzl",
                    "r = repr((dir(f), hasattr(f, 'cpp'), hasattr(f, 'apple')))\n".into(),
                    &Dialect::Standard,
                )
                .unwrap(),
                &globals,
            )
            .unwrap();
            assert_eq!(
                module.get("r").unwrap().unpack_str().unwrap(),
                "([\"cpp\"], False, False)"
            );
            for (expression, name) in [("f.cpp", "cpp"), ("f.apple", "apple")] {
                let error = eval
                    .eval_module(
                        AstModule::parse("//pkg:defs.bzl", expression.into(), &Dialect::Standard)
                            .unwrap(),
                        &globals,
                    )
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(&format!(
                        "There is no configuration fragment named '{name}'. Available fragments: 'cpp'"
                    )),
                    "{expression}: {error}"
                );
            }
        });
        scope.finish();
        let mut expected = STARLARK_FRAGMENTS.map(str::to_owned).to_vec();
        expected.sort();
        assert!(!expected.contains(&"py".to_owned()) && expected.len() == 10);
    }
}
