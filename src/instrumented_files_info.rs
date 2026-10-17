//! `coverage_common.instrumented_files_info` and the InstrumentedFilesInfo it
//! returns (analysis/test/CoverageCommon.java:46-117,
//! analysis/test/InstrumentedFilesInfo.java).
//!
//! The command-line `--collect_code_coverage` is rejected, so outside a
//! coverage configuration (one a Starlark transition enabled it in fails
//! explicitly, N47) InstrumentedFilesCollector.collect returns
//! InstrumentedFilesInfo.EMPTY after the arguments are checked
//! (analysis/test/InstrumentedFilesCollector.java:96-109). The checks run in
//! Bazel's order: parameter types (BuiltinFunction/ParamDescriptor), then the
//! body's casts and the private-API restriction of the undocumented
//! parameters.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;

pub(crate) const PROVIDER_ID: &str = "builtin:InstrumentedFilesInfo";

const FUNCTION: &str = "instrumented_files_info";

// Only derive-generated markers require this exception; the value holds no
// heap references (as crate::run_environment_info).
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// InstrumentedFilesInfo.EMPTY: every nested set is empty.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct InstrumentedFilesInfo;
}
pub(crate) use owned::InstrumentedFilesInfo;

starlark_simple_value!(InstrumentedFilesInfo);

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    InstrumentedFilesInfo::from_value(value).is_some()
}

/// NativeInfo.repr (packages/NativeInfo.java:120-138): the sorted
/// non-method fields; `get_coverage_environment`/`get_coverage_support_files`
/// are methods and omitted.
impl fmt::Display for InstrumentedFilesInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(instrumented_files = depset([]), metadata_files = depset([]))")
    }
}

#[starlark_value(type = "InstrumentedFilesInfo")]
impl<'v> StarlarkValue<'v> for InstrumentedFilesInfo {
    /// InstrumentedFilesInfoApi.getInstrumentedFiles/getMetadataFiles
    /// (starlarkbuildapi/test/InstrumentedFilesInfoApi.java:47-66).
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match name {
            "instrumented_files" | "metadata_files" => {
                Some(crate::starlark_types::alloc_depset(heap, Vec::new()))
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["instrumented_files".to_owned(), "metadata_files".to_owned()]
    }

    /// NativeInfo.equals/hashCode (:96-117): same provider and fields.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_instance(other))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

fn failure(message: String) -> starlark::Error {
    starlark::Error::new_other(anyhow!(message))
}

fn param_type_error(param: &str, value: Value<'_>, want: &str) -> starlark::Error {
    crate::language_modules::type_error(FUNCTION, param, value, want)
}

fn sequence_items(value: Value<'_>) -> Option<Vec<Value<'_>>> {
    ListRef::from_value(value)
        .map(|list| list.iter().collect())
        .or_else(|| TupleRef::from_value(value).map(|tuple| tuple.iter().collect()))
}

/// Sequence.cast (net/starlark/java/eval/Sequence.java:132-150) of an
/// already type-checked sequence.
fn cast_elements(
    items: &[Value<'_>],
    what: &str,
    want: &str,
    accepts: impl Fn(Value<'_>) -> bool,
) -> starlark::Result<()> {
    match items.iter().position(|item| !accepts(*item)) {
        Some(index) => Err(failure(format!(
            "at index {index} of {what}, got element of type {}, want {want}",
            items[index].get_type()
        ))),
        None => Ok(()),
    }
}

fn is_file(value: Value<'_>) -> bool {
    crate::starlark_types::File::from_value(value).is_some()
}

/// `coverage_common.instrumented_files_info(ctx, *, source_attributes=[],
/// dependency_attributes=[], coverage_support_files=[], coverage_environment={},
/// extensions=None, metadata_files=[], reported_to_actual_sources=None,
/// baseline_coverage_files=None)` (starlarkbuildapi/test/CoverageCommonApi.java:44-150).
pub(crate) fn call<'v>(
    style: crate::language_modules::Style,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    use crate::language_modules::param;
    let heap = eval.heap();
    let bound = crate::language_modules::bind_styled(
        style,
        FUNCTION,
        &[
            param("ctx", true, true, true),
            param("source_attributes", false, true, false),
            param("dependency_attributes", false, true, false),
            param("coverage_support_files", false, true, false),
            param("coverage_environment", false, true, false),
            param("extensions", false, true, false),
            param("metadata_files", false, true, false),
            param("reported_to_actual_sources", false, true, false),
            param("baseline_coverage_files", false, true, false),
        ],
        args,
        heap,
    )?;
    let ctx = bound[0].expect("required parameter");
    // BuiltinFunction.checkParamValue in parameter order (ParamDescriptor
    // types: the Java parameter class, or the allowedTypes joined with "or").
    if ctx.get_type() != "ctx" {
        return Err(param_type_error("ctx", ctx, "ctx"));
    }
    let sequence = |index: usize, name: &str| -> starlark::Result<Vec<Value<'v>>> {
        match bound[index] {
            None => Ok(Vec::new()),
            Some(value) => {
                sequence_items(value).ok_or_else(|| param_type_error(name, value, "sequence"))
            }
        }
    };
    let source_attributes = sequence(1, "source_attributes")?;
    let dependency_attributes = sequence(2, "dependency_attributes")?;
    let support_files = bound[3];
    if let Some(value) = support_files
        && sequence_items(value).is_none()
        && !crate::starlark_types::is_depset(value)
    {
        return Err(param_type_error(
            "coverage_support_files",
            value,
            "sequence or depset",
        ));
    }
    let environment = bound[4];
    if let Some(value) = environment
        && DictRef::from_value(value).is_none()
    {
        return Err(param_type_error("coverage_environment", value, "dict"));
    }
    let extensions = bound[5].filter(|value| !value.is_none());
    if let Some(value) = extensions
        && sequence_items(value).is_none()
    {
        return Err(param_type_error(
            "extensions",
            value,
            "sequence or NoneType",
        ));
    }
    let metadata_files = sequence(6, "metadata_files")?;
    let reported = bound[7].filter(|value| !value.is_none());
    if let Some(value) = reported
        && !crate::starlark_types::is_depset(value)
    {
        return Err(param_type_error(
            "reported_to_actual_sources",
            value,
            "depset or NoneType",
        ));
    }
    let baseline = bound[8].filter(|value| !value.is_none());
    if let Some(value) = baseline
        && sequence_items(value).is_none()
    {
        return Err(param_type_error(
            "baseline_coverage_files",
            value,
            "sequence or NoneType",
        ));
    }
    // CoverageCommon.instrumentedFilesInfo body (:60-110).
    if let Some(value) = extensions {
        let items = sequence_items(value).expect("checked");
        cast_elements(&items, "extensions", "string", |item| {
            item.unpack_str().is_some()
        })?;
    }
    let reported_nonempty = match reported {
        // Depset.cast(_, Tuple.class, ...): a non-empty depset must hold tuples.
        Some(value) => match crate::starlark_types::depset_element_type(value) {
            None => false,
            Some(_) => {
                return Err(failure(format!(
                    "coverage_common.{FUNCTION}: non-empty reported_to_actual_sources {} (coverage is never enabled natively)",
                    crate::rule_declarations::NOT_ANALYZED
                )));
            }
        },
        None => false,
    };
    if let Some(value) = baseline {
        let items = sequence_items(value).expect("checked");
        cast_elements(&items, "baseline_coverage_files", "File", is_file)?;
    }
    let mut environment_nonempty = false;
    if let Some(value) = environment {
        let dict = DictRef::from_value(value).expect("checked");
        for (key, item) in dict.iter() {
            if key.unpack_str().is_none() || item.unpack_str().is_none() {
                return Err(failure(format!(
                    "got dict<{}, {}> for 'coverage_environment', want dict<string, string>",
                    key.get_type(),
                    item.get_type()
                )));
            }
        }
        environment_nonempty = !dict.is_empty();
    }
    let mut support_nonempty = false;
    if let Some(value) = support_files {
        if crate::starlark_types::is_depset(value) {
            match crate::starlark_types::depset_element_type(value) {
                None => {}
                Some("File") => support_nonempty = true,
                Some(other) => {
                    return Err(failure(format!(
                        "for 'coverage_support_files', got a depset of '{other}', expected a depset of 'File'"
                    )));
                }
            }
        } else {
            for (index, item) in sequence_items(value)
                .expect("checked")
                .into_iter()
                .enumerate()
            {
                if crate::starlark_types::is_depset(item) {
                    match crate::starlark_types::depset_element_type(item) {
                        None => {}
                        Some("File") => support_nonempty = true,
                        Some(other) => {
                            return Err(failure(format!(
                                "for 'coverage_support_files', got a depset of '{other}', expected a depset of 'File'"
                            )));
                        }
                    }
                } else if is_file(item) {
                    support_nonempty = true;
                } else {
                    return Err(failure(format!(
                        "at index {index} of coverage_support_files, got element of type {}, want one of depset, File or FilesToRunProvider",
                        item.get_type()
                    )));
                }
            }
        }
    }
    if support_nonempty || reported_nonempty || environment_nonempty {
        // BuiltinRestriction.failIfCalledOutsideDefaultAllowlist (:94-98).
        crate::subrule_fragments::private_api(eval)?;
    }
    cast_elements(&source_attributes, "source_attributes", "string", |item| {
        item.unpack_str().is_some()
    })?;
    cast_elements(
        &dependency_attributes,
        "dependency_attributes",
        "string",
        |item| item.unpack_str().is_some(),
    )?;
    cast_elements(&metadata_files, "metadata_files", "File", is_file)?;
    // InstrumentedFilesCollector.collect without coverage (:107-109). With
    // `--collect_code_coverage` set by a Starlark transition (N47) Bazel
    // collects instrumented files (--instrumentation_filter etc.), which the
    // native profile does not model: explicit, like ctx.coverage_instrumented().
    if crate::analysis_ctx::current_surface(ctx)
        .is_ok_and(|surface| surface.config.coverage_enabled)
    {
        return Err(failure(crate::ctx_surface::coverage_gap(&format!(
            "coverage_common.{FUNCTION}"
        ))));
    }
    Ok(eval.heap().alloc(InstrumentedFilesInfo))
}
