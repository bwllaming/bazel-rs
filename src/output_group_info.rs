//! Native `OutputGroupInfo` (analysis/OutputGroupInfo.java): construction
//! (`OutputGroupInfoProvider.constructor`, :567-586), the struct-like
//! instance (indexing, `in`, iteration, fields, repr) and the framework's
//! per-target merge (RuleConfiguredTargetBuilder.java:155-213, 543-552).
//!
//! An instance is a fully owned snapshot: group names in Bazel's sorted
//! construction order, each with its owned File depset. It holds no Starlark
//! heap references, so it freezes unchanged and crosses dependency boundaries
//! by value.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::starlark_types::{Depset, File, Item, ItemType};

/// The provider key (crate::rule_declarations::OUTPUT_GROUP_INFO_ID).
pub(crate) const PROVIDER_ID: &str = crate::rule_declarations::OUTPUT_GROUP_INFO_ID;
/// OutputGroupInfo.HIDDEN_TOP_LEVEL (:111-112).
pub(crate) const HIDDEN_TOP_LEVEL: &str = "_hidden_top_level_INTERNAL_";
/// OutputGroupInfo.VALIDATION (:126): the output group of validation
/// actions' outputs, built even when `--output_groups` overrides the
/// defaults and unaffected by its `-` subtraction; disabled only by
/// `--run_validations=false`.
pub(crate) const VALIDATION: &str = "_validation";
/// OutputGroupInfo.TEMP_FILES (:140-141).
pub(crate) const TEMP_FILES: &str = "temp_files_INTERNAL_";
/// OutputGroupInfo.DEFAULT (:144).
pub(crate) const DEFAULT: &str = "default";

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

// Only derive-generated marker implementations need the exception. The value
// owns all its data (Arc depset DAGs of owned Files) without heap references.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct OutputGroupInfo {
        pub id: u64,
        #[allocative(skip)]
        pub groups: Vec<(String, Depset)>,
    }
}
pub(crate) use owned::OutputGroupInfo;
starlark_simple_value!(OutputGroupInfo);

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    OutputGroupInfo::from_value(value).is_some()
}

/// The groups of an instance, in field order.
pub(crate) fn groups(value: Value<'_>) -> Option<&[(String, Depset)]> {
    OutputGroupInfo::from_value(value).map(|info| info.groups.as_slice())
}

/// Every File of every group (provider payload traversal).
pub(crate) fn files(value: Value<'_>) -> Option<Vec<File>> {
    let info = OutputGroupInfo::from_value(value)?;
    let mut files = Vec::new();
    for (_, depset) in &info.groups {
        files.extend(depset_files(depset));
    }
    Some(files)
}

fn depset_files(depset: &Depset) -> Vec<File> {
    depset
        .flatten()
        .into_iter()
        .filter_map(|item| match item {
            Item::File(file) => Some(file.clone()),
            _ => None,
        })
        .collect()
}

/// StarlarkRuleConfiguredTargetUtil.convertToOutputGroupValue (:227-236): a
/// sequence of Files becomes a stable-order set; otherwise Depset.cast to File.
fn convert(group: &str, value: Value<'_>) -> anyhow::Result<Depset> {
    let what = format!("output group '{group}'");
    let sequence = ListRef::from_value(value)
        .map(|list| list.content().to_vec())
        .or_else(|| TupleRef::from_value(value).map(|tuple| tuple.content().to_vec()));
    if let Some(elements) = sequence {
        let mut files = Vec::with_capacity(elements.len());
        for (index, element) in elements.into_iter().enumerate() {
            let file = File::from_value(element).ok_or_else(|| {
                // Sequence.cast (net/starlark/java/eval/Sequence.java:139-144).
                anyhow!(
                    "at index {index} of {what}, got element of type {}, want File",
                    element.get_type()
                )
            })?;
            files.push(file.clone());
        }
        return Ok(Depset::from_files(files));
    }
    // Depset.cast (collect/nestedset/Depset.java:262-271, getSet :207-218).
    let depset = Depset::from_value(value).ok_or_else(|| {
        anyhow!(
            "for {what}, got {}, want a depset of File",
            value.get_type()
        )
    })?;
    if !depset.node.empty
        && let Some(kind) = depset.node.element_type
        && kind != ItemType::File
    {
        return Err(anyhow!(
            "for '{what}', got a depset of '{}', expected a depset of 'File'",
            kind.name()
        ));
    }
    Ok(depset.clone())
}

/// `OutputGroupInfo(**kwargs)`: keyword arguments only, sorted by name
/// (OutputGroupInfoProvider.constructor, :575-586).
pub(crate) fn construct<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if args.positions(eval.heap())?.next().is_some() {
        // BuiltinFunction argument binding of a kwargs-only builtin.
        return Err(starlark::Error::new_other(anyhow!(
            "OutputGroupInfo() got unexpected positional argument"
        )));
    }
    let mut named = args
        .names_map()?
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), *value))
        .collect::<Vec<_>>();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    let mut groups = Vec::with_capacity(named.len());
    for (name, value) in named {
        let depset = convert(&name, value).map_err(starlark::Error::new_other)?;
        groups.push((name, depset));
    }
    Ok(eval.heap().alloc(OutputGroupInfo {
        id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
        groups,
    }))
}

/// OutputGroupInfo.VALIDATION_TRANSITIVE (:133-134).
pub(crate) const VALIDATION_TRANSITIVE: &str = "_validation_transitive";

/// The OutputGroupInfo of a configured rule target
/// (RuleConfiguredTargetBuilder.build): the groups of the returned
/// OutputGroupInfo (addStarlarkDeclaredProvider, :543-552) plus the
/// HIDDEN_TOP_LEVEL group of the default runfiles' artifacts (:155-173), in
/// the builders' sorted order; a group named twice merges its sets.
pub(crate) fn merged(
    mut groups: Vec<(String, Depset)>,
    hidden_top_level: Vec<File>,
) -> OutputGroupInfo {
    add_group(
        &mut groups,
        HIDDEN_TOP_LEVEL,
        Depset::from_files(hidden_top_level),
    );
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    OutputGroupInfo {
        id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
        groups,
    }
}

/// RuleConfiguredTargetBuilder.addOutputGroup: a repeated group's sets are
/// merged.
pub(crate) fn add_group(groups: &mut Vec<(String, Depset)>, name: &str, files: Depset) {
    if let Some((_, existing)) = groups.iter_mut().find(|(group, _)| group == name) {
        *existing = Depset::compose(
            vec![],
            vec![existing.node.clone(), files.node],
            crate::starlark_types::Order::Default,
            Some(ItemType::File),
        )
        .expect("File depsets of the default order compose");
    } else {
        groups.push((name.to_owned(), files));
    }
}

/// The frozen framework OutputGroupInfo of a native rule target: the
/// HIDDEN_TOP_LEVEL group of its default runfiles' artifacts plus the
/// `_validation` group RuleConfiguredTargetBuilder propagated from its
/// dependencies (propagateTransitiveValidationOutputGroups, :175-176,
/// 366-388), for its provider map.
pub(crate) fn freeze(
    hidden_top_level: Vec<File>,
    validation: Vec<File>,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        let mut groups = Vec::new();
        if !validation.is_empty() {
            groups.push((VALIDATION.to_owned(), Depset::from_files(validation)));
        }
        let info = merged(groups, hidden_top_level);
        module.set("__info", module.heap().alloc(info));
        module
            .freeze()
            .map_err(|error| format!("cannot freeze OutputGroupInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
}

/// The frozen OutputGroupInfo of an output-file target
/// (OutputFileConfiguredTarget.java:109-118): a single `_validation` group
/// with the generating rule's validation outputs, present only when they are
/// non-empty (the provider is absent otherwise).
pub(crate) fn freeze_validation(
    validation: Vec<File>,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        let info = OutputGroupInfo {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            groups: vec![(VALIDATION.to_owned(), Depset::from_files(validation))],
        };
        module.set("__info", module.heap().alloc(info));
        module
            .freeze()
            .map_err(|error| format!("cannot freeze OutputGroupInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
}

/// The Files of an instance's `_validation` group (empty when the instance
/// has none).
pub(crate) fn validation_files(value: Value<'_>) -> Vec<File> {
    groups(value)
        .and_then(|groups| {
            groups
                .iter()
                .find(|(name, _)| name == VALIDATION)
                .map(|(_, depset)| depset_files(depset))
        })
        .unwrap_or_default()
}

/// The output-group half of a build request's TopLevelArtifactContext
/// (BuildRequest.getTopLevelArtifactContext, buildtool/BuildRequest.java:
/// 412-421): the `--output_groups` tokens (each with its `+`/`-` prefix
/// kept, repeated flags and commas concatenated in order) and whether
/// validation actions run (`--run_validations`, BuildRequestOptions.java:
/// 178-190).
#[derive(Clone, Debug)]
pub struct OutputSelection {
    pub groups: Vec<String>,
    pub run_validations: bool,
}

impl Default for OutputSelection {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            run_validations: true,
        }
    }
}

impl OutputSelection {
    /// One `--output_groups=VALUE` occurrence: CommaSeparatedOptionListConverter
    /// (empty segments drop out) with allowMultiple's accumulation.
    pub fn parse_groups(&mut self, value: &str) {
        self.groups.extend(
            value
                .split(',')
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
        );
    }
}

/// OutputGroupInfo.determineOutputGroups (:207-243) with
/// DEFAULT_GROUPS (:146-147): the `--output_groups` tokens (each optionally
/// `+`/`-` prefixed; an unprefixed token overrides the default set), the
/// `_validation` group added whenever validation actions run (`--run_validations`,
/// the only way to disable it; `-_validation` subtracts nothing), and the
/// `default` group kept for a command that runs tests (BuildRequest.
/// getTopLevelArtifactContext's shouldRunTests).
pub(crate) fn determine_output_groups(
    output_groups: &[String],
    run_validations: bool,
    should_run_tests: bool,
) -> std::collections::BTreeSet<String> {
    let mut current = std::collections::BTreeSet::new();
    // If all of the requested output groups start with "+" or "-", then
    // these are added or subtracted to the set of default output groups;
    // otherwise the requested groups override the default set.
    let add_default_output_groups = output_groups
        .iter()
        .all(|group| group.starts_with('+') || group.starts_with('-'));
    if add_default_output_groups {
        current.insert(DEFAULT.to_owned());
        current.insert(TEMP_FILES.to_owned());
        current.insert(HIDDEN_TOP_LEVEL.to_owned());
    }
    for group in output_groups {
        if let Some(name) = group.strip_prefix('+') {
            current.insert(name.to_owned());
        } else if let Some(name) = group.strip_prefix('-') {
            current.remove(name);
        } else {
            current.insert(group.clone());
        }
    }
    // Add the validation output group regardless of the additions and
    // subtractions above (the OUTPUT_GROUP ValidationMode of --run_validations;
    // the ASPECT mode of --experimental_use_validation_aspect is rejected by
    // the command line).
    if run_validations {
        current.insert(VALIDATION.to_owned());
    }
    if should_run_tests {
        current.insert(DEFAULT.to_owned());
    }
    current
}

impl OutputGroupInfo {
    fn get(&self, name: &str) -> Option<&Depset> {
        self.groups
            .iter()
            .find(|(group, _)| group == name)
            .map(|(_, depset)| depset)
    }
}

/// StructImpl.repr (packages/StructImpl.java:139-153): sorted fields.
impl fmt::Display for OutputGroupInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = self.groups.iter().collect::<Vec<_>>();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        f.write_str("struct(")?;
        for (index, (name, depset)) in names.into_iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name} = {depset}")?;
        }
        f.write_str(")")
    }
}

#[starlark_value(type = "OutputGroupInfo")]
impl<'v> StarlarkValue<'v> for OutputGroupInfo {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.get(name).map(|depset| heap.alloc(depset.clone()))
    }

    fn dir_attr(&self) -> Vec<String> {
        self.groups.iter().map(|(name, _)| name.clone()).collect()
    }

    /// OutputGroupInfo.getIndex (:329-339).
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let Some(name) = index.unpack_str() else {
            return Err(starlark::Error::new_other(anyhow!(
                "Output group names must be strings, got {} instead",
                index.get_type()
            )));
        };
        self.get(name)
            .map(|depset| heap.alloc(depset.clone()))
            .ok_or_else(|| starlark::Error::new_other(anyhow!("Output group {name} not present")))
    }

    /// OutputGroupInfo.containsKey (:342-345).
    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .unpack_str()
            .is_some_and(|name| self.get(name).is_some()))
    }

    fn iterate_collect(&self, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(self
            .groups
            .iter()
            .map(|(name, _)| heap.alloc(name.as_str()))
            .collect())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| other.id == self.id))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        self.id.hash(hasher);
        Ok(())
    }
}
