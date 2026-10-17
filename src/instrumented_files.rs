//! The analysis-time coverage API in configurations without
//! `--collect_code_coverage` (N54): `coverage_common.instrumented_files_info`
//! and the `InstrumentedFilesInfo` provider.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//!
//! CoverageCommon.instrumentedFilesInfo (analysis/test/CoverageCommon.java:47-157)
//! validates its arguments and then InstrumentedFilesCollector.collect
//! (analysis/test/InstrumentedFilesCollector.java:94-148) returns the shared
//! `InstrumentedFilesInfo.EMPTY` (analysis/test/InstrumentedFilesInfo.java:34-41)
//! whenever coverage is disabled — nothing is collected from the source or
//! dependency attributes, which are not even looked up. Native rules that add
//! the provider do so through the same collector (Filegroup.java:63-108,
//! GenRuleBase.java:234-239), so they carry EMPTY too, and an output file
//! target answers with its generating rule's provider or EMPTY
//! (OutputFileConfiguredTarget.java:100-107). In a configuration a Starlark
//! transition enabled coverage in (N47) the call fails explicitly.
use std::fmt;
use std::hash::Hash;
use std::sync::OnceLock;

use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::values::dict::DictRef;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};

use crate::language_modules::{Param, failure, param, type_error};
use crate::starlark_types::{Depset, File, ItemType};

/// The provider key of InstrumentedFilesInfo (InstrumentedFilesProvider,
/// a BuiltinProvider named `InstrumentedFilesInfo`).
pub(crate) const PROVIDER_ID: &str = "builtin:InstrumentedFilesInfo";

// Only the upstream derives generate unsafe marker implementations; both
// values own no data.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    /// `InstrumentedFilesInfo.EMPTY` (coverage disabled), or the
    /// unmodelled collected instance of a native target in a coverage
    /// configuration (`coverage_of`: the target), whose every member fails
    /// explicitly.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Info {
        pub(super) coverage_of: Option<String>,
    }
}
use owned::{Constructor, Info};

starlark_simple_value!(Constructor);
starlark_simple_value!(Info);

/// TestingBootstrap binds `InstrumentedFilesInfo`
/// (starlarkbuildapi/test/TestingBootstrap.java:46-52).
pub(crate) fn install(builder: &mut GlobalsBuilder) {
    builder.set("InstrumentedFilesInfo", Constructor);
}

pub(crate) fn is_constructor(value: Value<'_>) -> bool {
    Constructor::from_value(value).is_some()
}

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    Info::from_value(value).is_some()
}

/// The frozen `InstrumentedFilesInfo.EMPTY` of native rule and output-file
/// targets.
pub(crate) fn empty_frozen() -> Result<OwnedFrozenValue, String> {
    static EMPTY: OnceLock<Result<OwnedFrozenValue, String>> = OnceLock::new();
    EMPTY
        .get_or_init(|| {
            starlark::environment::Module::with_temp_heap(|module| {
                module.set("__info", module.heap().alloc(Info { coverage_of: None }));
                module
                    .freeze()
                    .map_err(|error| format!("cannot freeze InstrumentedFilesInfo: {error:?}"))?
                    .get("__info")
                    .map_err(|error| error.to_string())
            })
        })
        .clone()
}

/// The provider of a native target (filegroup, genrule, output file) in a
/// coverage configuration: InstrumentedFilesCollector.collect would collect
/// from its dependencies (not modelled); present, but every member fails
/// explicitly.
pub(crate) fn coverage_gap_frozen(target: &str) -> Result<OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        module.set(
            "__info",
            module.heap().alloc(Info {
                coverage_of: Some(target.to_owned()),
            }),
        );
        module
            .freeze()
            .map_err(|error| format!("cannot freeze InstrumentedFilesInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
}

impl Info {
    fn check(&self) -> anyhow::Result<()> {
        match &self.coverage_of {
            None => Ok(()),
            Some(target) => Err(anyhow::anyhow!(
                "{}",
                crate::ctx_surface::coverage_gap(&format!("InstrumentedFilesInfo of {target}"))
            )),
        }
    }
}

/// BuiltinProvider.repr (packages/BuiltinProvider.java:93-96).
impl fmt::Display for Constructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function InstrumentedFilesInfo>")
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for Constructor {
    // A BuiltinProvider without @StarlarkConstructor is not callable.
    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(failure("'Provider' object is not callable".to_owned()))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_constructor(other))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

/// The empty, untyped stable-order depset of the EMPTY instance's fields
/// (`Depset.of(Artifact.class, emptySet)`).
fn empty_depset<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(
        Depset::compose(
            Vec::new(),
            Vec::new(),
            crate::starlark_types::Order::Default,
            None,
        )
        .expect("an empty depset is valid"),
    )
}

/// NativeInfo.repr (packages/NativeInfo.java:120-138): the non-method
/// fields sorted by name.
impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.coverage_of {
            None => {
                f.write_str("struct(instrumented_files = depset([]), metadata_files = depset([]))")
            }
            // Not Bazel's text: the collected value is not modelled.
            Some(target) => write!(
                f,
                "<InstrumentedFilesInfo of {target} in a coverage configuration: not modelled>"
            ),
        }
    }
}

starlark::methods_static!(INFO_METHODS = info_methods);

/// InstrumentedFilesInfoApi (starlarkbuildapi/test/InstrumentedFilesInfoApi.java:47-79).
#[starlark_module]
fn info_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn instrumented_files<'v>(this: &Info, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        this.check()?;
        Ok(empty_depset(heap))
    }

    #[starlark(attribute)]
    fn metadata_files<'v>(this: &Info, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        this.check()?;
        Ok(empty_depset(heap))
    }

    /// InstrumentedFilesInfo.getCoverageEnvironmentForStarlark (:106-111):
    /// BuiltinRestriction.failIfCalledOutsideDefaultAllowlist.
    fn get_coverage_environment<'v>(
        this: &Info,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        this.check().map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(eval
            .heap()
            .alloc(starlark::values::dict::AllocDict(Vec::<(&str, &str)>::new())))
    }

    /// getCoverageSupportFilesForStarlark (:98-103), restricted likewise.
    fn get_coverage_support_files<'v>(
        this: &Info,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        this.check().map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(empty_depset(eval.heap()))
    }
}

#[starlark_value(type = "InstrumentedFilesInfo")]
impl<'v> StarlarkValue<'v> for Info {
    fn get_methods() -> Option<&'static Methods> {
        Some(INFO_METHODS.methods())
    }

    // NativeInfo.equals/hashCode (packages/NativeInfo.java:97-117): the
    // provider and the fields; every native instance is EMPTY.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        self.check().map_err(starlark::Error::new_other)?;
        Ok(Info::from_value(other).is_some_and(|other| other.coverage_of.is_none()))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.check().map_err(starlark::Error::new_other)?;
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

const F: &str = "instrumented_files_info";

/// The parameters of CoverageCommonApi.instrumentedFilesInfo
/// (starlarkbuildapi/test/CoverageCommonApi.java:39-133), in order.
const PARAMS: [Param; 9] = [
    param("ctx", true, true, true),
    param("source_attributes", false, true, false),
    param("dependency_attributes", false, true, false),
    param("coverage_support_files", false, true, false),
    param("coverage_environment", false, true, false),
    param("extensions", false, true, false),
    param("metadata_files", false, true, false),
    param("reported_to_actual_sources", false, true, false),
    param("baseline_coverage_files", false, true, false),
];

fn is_sequence(value: Value<'_>) -> bool {
    matches!(value.get_type(), "list" | "tuple")
}

fn sequence_items<'v>(value: Value<'v>) -> Vec<Value<'v>> {
    crate::custom_rule::sequence_ref(value)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

/// Sequence.cast (net/starlark/java/eval/Sequence.java:132-149).
fn cast_sequence(value: Value<'_>, want: &str, what: &str) -> starlark::Result<()> {
    for (index, item) in sequence_items(value).into_iter().enumerate() {
        let ok = match want {
            "string" => item.unpack_str().is_some(),
            _ => File::from_value(item).is_some(),
        };
        if !ok {
            return Err(failure(format!(
                "at index {index} of {what}, got element of type {}, want {want}",
                item.get_type()
            )));
        }
    }
    Ok(())
}

/// Depset.cast (collect/nestedset/Depset.java:262-272) of a depset value.
fn cast_depset(value: Value<'_>, want: ItemType, what: &str) -> starlark::Result<bool> {
    let depset = Depset::from_value(value).expect("type-checked depset");
    if let Some(kind) = depset.node.element_type
        && !depset.node.empty
        && kind != want
    {
        return Err(failure(format!(
            "for '{what}', got a depset of '{}', expected a depset of '{}'",
            kind.name(),
            want.name()
        )));
    }
    Ok(!depset.node.empty)
}

/// CoverageCommon.instrumentedFilesInfo (analysis/test/CoverageCommon.java:47-111).
pub(crate) fn instrumented_files_info<'v>(
    style: crate::language_modules::Style,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    // The caller's binding style (N33: built-in method vs def-style texts).
    let bound = crate::language_modules::bind_styled(style, F, &PARAMS, args, heap)?;
    let ctx = bound[0].expect("required");
    // The @Param types, in declaration order (Starlark's argument checks).
    if ctx.get_type() != "ctx" {
        return Err(type_error(F, "ctx", ctx, "ctx"));
    }
    let wants: [(usize, &str); 8] = [
        (1, "sequence"),
        (2, "sequence"),
        (3, "sequence or depset"),
        (4, "dict"),
        (5, "sequence or NoneType"),
        (6, "sequence"),
        (7, "depset or NoneType"),
        (8, "sequence or NoneType"),
    ];
    for (index, want) in wants {
        let Some(value) = bound[index] else { continue };
        let ok = match index {
            1 | 2 | 6 => is_sequence(value),
            3 => is_sequence(value) || Depset::from_value(value).is_some(),
            4 => DictRef::from_value(value).is_some(),
            5 | 8 => value.is_none() || is_sequence(value),
            _ => value.is_none() || Depset::from_value(value).is_some(),
        };
        if !ok {
            return Err(type_error(F, PARAMS[index].name, value, want));
        }
    }
    let present = |index: usize| bound[index].filter(|value| !value.is_none());
    // extensions, reported_to_actual_sources, baseline_coverage_files,
    // coverage_environment, coverage_support_files (CoverageCommon.java:62-102).
    if let Some(extensions) = present(5) {
        cast_sequence(extensions, "string", "extensions")?;
    }
    let mut restricted = false;
    if let Some(reported) = present(7) {
        restricted |= cast_depset(reported, ItemType::Tuple, "reported_to_actual_sources")?;
    }
    if let Some(baseline) = present(8) {
        cast_sequence(baseline, "File", "baseline_coverage_files")?;
    }
    if let Some(environment) = bound[4] {
        let dict = DictRef::from_value(environment).expect("type-checked dict");
        for (key, value) in dict.iter() {
            if key.unpack_str().is_none() || value.unpack_str().is_none() {
                return Err(failure(format!(
                    "got dict<{}, {}> for 'coverage_environment', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                )));
            }
        }
        restricted |= !dict.is_empty();
    }
    if let Some(support) = bound[3] {
        if Depset::from_value(support).is_some() {
            restricted |= cast_depset(support, ItemType::File, "coverage_support_files")?;
        } else {
            for (index, item) in sequence_items(support).into_iter().enumerate() {
                if Depset::from_value(item).is_some() {
                    restricted |= cast_depset(item, ItemType::File, "coverage_support_files")?;
                } else if File::from_value(item).is_some() {
                    restricted = true;
                } else {
                    return Err(failure(format!(
                        "at index {index} of coverage_support_files, got element of type {}, want one of depset, File or FilesToRunProvider",
                        item.get_type()
                    )));
                }
            }
        }
    }
    // BuiltinRestriction.failIfCalledOutsideDefaultAllowlist (:103-107).
    if restricted {
        crate::subrule_fragments::private_api(eval)?;
    }
    if let Some(sources) = bound[1] {
        cast_sequence(sources, "string", "source_attributes")?;
    }
    if let Some(dependencies) = bound[2] {
        cast_sequence(dependencies, "string", "dependency_attributes")?;
    }
    if let Some(metadata) = bound[6] {
        cast_sequence(metadata, "File", "metadata_files")?;
    }
    if crate::analysis_ctx::coverage_enabled(ctx).map_err(starlark::Error::new_other)? {
        return Err(failure(crate::ctx_surface::coverage_gap(
            "coverage_common.instrumented_files_info()",
        )));
    }
    Ok(heap.alloc(Info { coverage_of: None }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_instance_is_shared_and_frozen() {
        let a = super::empty_frozen().unwrap();
        let b = super::empty_frozen().unwrap();
        assert!(a.value().ptr_eq(b.value()));
        assert_eq!(a.value().get_type(), "InstrumentedFilesInfo");
        assert_eq!(
            a.value().to_repr(),
            "struct(instrumented_files = depset([]), metadata_files = depset([]))"
        );
    }
}
