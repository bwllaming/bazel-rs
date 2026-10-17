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
/// OutputGroupInfo.VALIDATION (:126).
pub(crate) const VALIDATION: &str = "_validation";

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

/// The frozen framework OutputGroupInfo of a native rule target (only the
/// HIDDEN_TOP_LEVEL group), for its provider map.
pub(crate) fn freeze(
    hidden_top_level: Vec<File>,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        module.set(
            "__info",
            module.heap().alloc(merged(Vec::new(), hidden_top_level)),
        );
        module
            .freeze()
            .map_err(|error| format!("cannot freeze OutputGroupInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
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
