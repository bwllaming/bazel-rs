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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::starlark_types::{Depset, DepsetNode, File, Item, ItemType, Order};

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

pub(crate) fn depset_files(depset: &Depset) -> Vec<File> {
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
/// HIDDEN_TOP_LEVEL group of the default runfiles' artifacts (:155-173,
/// crate::runfiles::Runfiles::all_artifacts_depset), in the builders'
/// sorted order; a group named twice merges its sets.
pub(crate) fn merged(
    mut groups: Vec<(String, Depset)>,
    hidden_top_level: Depset,
) -> OutputGroupInfo {
    add_group(&mut groups, HIDDEN_TOP_LEVEL, hidden_top_level);
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    OutputGroupInfo {
        id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
        groups,
    }
}

/// RuleConfiguredTargetBuilder.addOutputGroup: a repeated group's sets are
/// merged (a framework composition, never depth-checked:
/// [`Depset::compose_framework`]).
pub(crate) fn add_group(groups: &mut Vec<(String, Depset)>, name: &str, files: Depset) {
    if let Some((_, existing)) = groups.iter_mut().find(|(group, _)| group == name) {
        *existing = Depset::compose_framework(
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

/// The non-empty `_validation` group of an OutputGroupInfo instance
/// (`getOutputGroup(VALIDATION)` with RuleConfiguredTargetBuilder's
/// `!validationArtifacts.isEmpty()` filter, :418-423).
pub(crate) fn validation(value: Value<'_>) -> Option<&Depset> {
    groups(value)?
        .iter()
        .find(|(name, _)| name == VALIDATION)
        .map(|(_, depset)| depset)
        .filter(|depset| !depset.node.empty)
}

/// A prerequisite's `_validation` set as a member of a composed group
/// ([`compose_validation`]): the set's own node, except that a topological
/// set — nested in a stable-order set it walks unreversed, but its own
/// `to_list` is reversed — joins as its `to_list` in a fresh stable-order
/// set, keeping the order each prerequisite's flattened group gave before
/// W1c (crate::starlark_types::Depset::flatten).
pub(crate) fn validation_member(depset: &Depset) -> Arc<DepsetNode> {
    if depset.node.order == Order::Topological {
        Depset::from_files(depset_files(depset)).node
    } else {
        Arc::clone(&depset.node)
    }
}

/// RuleConfiguredTargetBuilder's `_validation` group of collected
/// prerequisite sets (collectTransitiveValidationOutputGroups, :397-425,
/// each consumed by addOutputGroup into ONE stable-order NestedSetBuilder,
/// :636-656) as one composed node over `members`
/// ([`validation_member`]s in collection order), without flattening them
/// (W1c, PLAN-L1 §2.3). The builder skips empty sets and deduplicates the
/// others by identity (NestedSetBuilder.addTransitive, :143-155, 297-302);
/// a single member is the group itself (buildInternal, :181-202 — the
/// [`Depset::compose_framework`] reuse of a lone same-order child). `to_list`
/// walks the members left to right keeping first occurrences: the order of
/// the flat first-occurrence list collected before. `None` when every
/// member is empty.
pub(crate) fn compose_validation(members: Vec<Arc<DepsetNode>>) -> Option<Depset> {
    let mut seen = std::collections::HashSet::new();
    let members: Vec<_> = members
        .into_iter()
        .filter(|member| !member.empty && seen.insert(Arc::as_ptr(member)))
        .collect();
    if members.is_empty() {
        return None;
    }
    Some(
        Depset::compose_framework(vec![], members, Order::Default, Some(ItemType::File))
            .expect("File sets compose in the default order"),
    )
}

/// A target's non-empty `_validation` group as the set its consumers share
/// (W1c): the group itself when it is stable-order — what Bazel shares
/// (OutputFileConfiguredTarget.java:109-118) and what a builder holding
/// only that set returns (ValidateTarget.java:69-76 over
/// NestedSetBuilder.buildInternal, :181-202) — else a stable-order set over
/// its [`validation_member`], whose `to_list` and repr are those of the
/// flattened stable-order copy every consumer received before W1c.
pub(crate) fn shared_validation(group: &Depset) -> Depset {
    compose_validation(vec![validation_member(group)]).unwrap_or_else(|| group.clone())
}

/// RuleConfiguredTargetBuilder.addOutputGroup of each collected set
/// (`members`, [`validation_member`]s) into group `name`: ONE composed node
/// `[own?, member, ...]` — the target's own group first, as the builder's
/// first transitive set (addStarlarkDeclaredProvider runs before
/// propagateTransitiveValidationOutputGroups) — or, without an own group,
/// [`compose_validation`] of the members. No member: unchanged.
pub(crate) fn add_transitive(
    groups: &mut Vec<(String, Depset)>,
    name: &str,
    members: Vec<Arc<DepsetNode>>,
) {
    if members.iter().all(|member| member.empty) {
        return;
    }
    match groups.iter_mut().find(|(group, _)| group == name) {
        Some((_, own)) => {
            let mut all = Vec::with_capacity(members.len() + 1);
            all.push(Arc::clone(&own.node));
            all.extend(members);
            if let Some(composed) = compose_validation(all) {
                *own = composed;
            }
        }
        None => {
            if let Some(composed) = compose_validation(members) {
                groups.push((name.to_owned(), composed));
            }
        }
    }
}

/// The frozen framework OutputGroupInfo of a native rule target: the
/// HIDDEN_TOP_LEVEL group of its default runfiles' artifacts plus the
/// `_validation` group RuleConfiguredTargetBuilder propagated from its
/// dependencies (propagateTransitiveValidationOutputGroups, :175-176,
/// 366-388), for its provider map.
pub(crate) fn freeze(
    hidden_top_level: Depset,
    validation: Option<Depset>,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        let mut groups = Vec::new();
        if let Some(validation) = validation {
            groups.push((VALIDATION.to_owned(), validation));
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
/// with the generating rule's validation outputs (its group, shared), present
/// only when they are non-empty (the provider is absent otherwise).
pub(crate) fn freeze_validation(
    validation: Depset,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        let info = OutputGroupInfo {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            groups: vec![(VALIDATION.to_owned(), validation)],
        };
        module.set("__info", module.heap().alloc(info));
        module
            .freeze()
            .map_err(|error| format!("cannot freeze OutputGroupInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
}

/// MergedConfiguredTarget.mergeOutputGroupProviders
/// (configuredtargets/MergedConfiguredTarget.java:234-284): the base
/// target's and every aspect's OutputGroupInfo merge per group, a group
/// named twice unions its files (a duplicate group with no new name is not
/// an error). Returns the merged groups, sorted by name
/// (createInternal's ImmutableSortedMap), or `None` when no set has any
/// group.
pub(crate) fn merge_group_sets(sets: &[Vec<(String, Depset)>]) -> Option<Vec<(String, Depset)>> {
    let mut merged: Vec<(String, Depset)> = Vec::new();
    for set in sets {
        for (name, depset) in set {
            add_group(&mut merged, name, depset.clone());
        }
    }
    if merged.is_empty() {
        return None;
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    Some(merged)
}

/// A frozen OutputGroupInfo of exactly `groups` (the merged view of an
/// aspect-configured target's provider map).
pub(crate) fn frozen_of_groups(
    groups: Vec<(String, Depset)>,
) -> starlark::values::OwnedFrozenValue {
    starlark::environment::Module::with_temp_heap(|module| {
        let info = OutputGroupInfo {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            groups,
        };
        module.set("__info", module.heap().alloc(info));
        module
            .freeze()
            .expect("an owned OutputGroupInfo freezes")
            .get("__info")
            .expect("__info was set")
    })
}

/// The frozen OutputGroupInfo the native ValidateTarget aspect returns
/// (rules/core/ValidateTarget.java:69-76): a single `_validation_top_level`
/// group with the promoted validation outputs (the target's `_validation`
/// set added to the aspect's group builder, [`compose_validation`]).
pub(crate) fn freeze_validation_top_level(
    validation: Depset,
) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        let info = OutputGroupInfo {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            groups: vec![(VALIDATION_TOP_LEVEL.to_owned(), validation)],
        };
        module.set("__info", module.heap().alloc(info));
        module
            .freeze()
            .map_err(|error| format!("cannot freeze OutputGroupInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
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
    /// `--experimental_use_validation_aspect` (ValidationMode.ASPECT,
    /// BuildRequest.validationMode, BuildRequest.java:452-457).
    pub use_validation_aspect: bool,
}

impl Default for OutputSelection {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            run_validations: true,
            use_validation_aspect: false,
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
/// validation group added whenever validation actions run (`--run_validations`,
/// the only way to disable it; `-_validation` subtracts nothing), and the
/// `default` group kept for a command that runs tests (BuildRequest.
/// getTopLevelArtifactContext's shouldRunTests).
pub(crate) fn determine_output_groups(
    output_groups: &[String],
    run_validations: bool,
    use_validation_aspect: bool,
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
    // subtractions above (determineOutputGroups's switch, :246-250):
    // OUTPUT_GROUP adds `_validation` (--run_validations without
    // --experimental_use_validation_aspect), ASPECT adds
    // `_validation_top_level` (the ValidateTarget aspect promotes the
    // outputs, rules/core/ValidateTarget.java:45-48), OFF neither
    // (--run_validations=false).
    match (run_validations, use_validation_aspect) {
        (false, _) => {}
        (true, false) => {
            current.insert(VALIDATION.to_owned());
        }
        (true, true) => {
            current.insert(VALIDATION_TOP_LEVEL.to_owned());
        }
    }
    if should_run_tests {
        current.insert(DEFAULT.to_owned());
    }
    current
}

/// OutputGroupInfo.VALIDATION_TOP_LEVEL (:126-128): the group the
/// ValidateTarget aspect promotes `_validation` outputs to, requested by
/// the ASPECT ValidationMode of `--experimental_use_validation_aspect`.
pub(crate) const VALIDATION_TOP_LEVEL: &str = "_validation_top_level";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn file(name: &str) -> File {
        File {
            path: format!("bazel-out/k8-fastbuild/bin/{name}"),
            short_path: name.to_owned(),
            owner: "//p:r".to_owned(),
            is_source: false,
            is_directory: false,
        }
    }

    fn set(direct: &[&str], children: &[&Depset], order: Order) -> Depset {
        Depset::compose(
            direct.iter().map(|name| Item::File(file(name))).collect(),
            children
                .iter()
                .map(|child| Arc::clone(&child.node))
                .collect(),
            order,
            Some(ItemType::File),
        )
        .unwrap()
    }

    fn names(depset: &Depset) -> Vec<String> {
        depset_files(depset)
            .into_iter()
            .map(|file| file.short_path)
            .collect()
    }

    /// The pre-W1c `_validation` group: every non-empty prerequisite group
    /// flattened (its own `to_list`), the first occurrences collected into a
    /// fresh stable-order set and merged into the own group.
    fn flattened(own: Option<&Depset>, groups: &[&Depset]) -> Option<Depset> {
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for group in groups {
            for file in depset_files(group) {
                if seen.insert(file.clone()) {
                    files.push(file);
                }
            }
        }
        let mut out = own
            .map(|own| vec![(VALIDATION.to_owned(), own.clone())])
            .unwrap_or_default();
        if !files.is_empty() {
            add_group(&mut out, VALIDATION, Depset::from_files(files));
        }
        out.pop().map(|(_, group)| group)
    }

    /// The W1c group: the prerequisites' sets composed into one node.
    fn composed(own: Option<&Depset>, groups: &[&Depset]) -> Option<Depset> {
        let mut out = own
            .map(|own| vec![(VALIDATION.to_owned(), own.clone())])
            .unwrap_or_default();
        let members = groups
            .iter()
            .filter(|group| !group.node.empty)
            .map(|group| validation_member(group))
            .collect();
        add_transitive(&mut out, VALIDATION, members);
        out.pop().map(|(_, group)| group)
    }

    #[test]
    fn composed_validation_keeps_the_flattened_to_list_and_repr() {
        let shared = set(&["s1", "s2"], &[], Order::Default);
        let topo_inner = set(&["t2", "s2"], &[&shared], Order::Topological);
        let topo = set(&["t1"], &[&topo_inner], Order::Topological);
        let pre = set(&["p1", "s1"], &[&shared], Order::Preorder);
        let post = set(&["q1"], &[&shared], Order::Postorder);
        let mixed = set(&["d1"], &[&pre, &topo, &post], Order::Default);
        let empty = set(&[], &[], Order::Default);
        let owns = [
            None,
            Some(empty.clone()),
            Some(set(&["o1", "s2"], &[], Order::Default)),
            Some(topo.clone()),
            Some(pre.clone()),
            Some(shared.clone()),
        ];
        let member_lists: Vec<Vec<&Depset>> = vec![
            vec![],
            vec![&empty],
            vec![&shared],
            vec![&topo],
            vec![&pre],
            vec![&post],
            vec![&pre, &post],
            vec![&mixed, &shared, &topo],
            vec![&topo, &topo],
            vec![&shared, &pre, &shared],
            vec![&topo_inner, &topo, &mixed, &post, &pre],
        ];
        for own in &owns {
            for members in &member_lists {
                let before = flattened(own.as_ref(), members);
                let after = composed(own.as_ref(), members);
                let context = format!("own {own:?} members {members:?}");
                assert_eq!(before.is_some(), after.is_some(), "{context}");
                let (Some(before), Some(after)) = (before, after) else {
                    continue;
                };
                assert_eq!(names(&before), names(&after), "{context}");
                assert_eq!(before.to_string(), after.to_string(), "{context}");
                assert_eq!(before.node.order, after.node.order, "{context}");
            }
        }
    }

    #[test]
    fn a_lone_stable_order_contributor_is_reused() {
        // NestedSetBuilder.buildInternal (:181-202): one distinct non-empty
        // stable-order transitive set and no direct element is that set.
        let leaf = set(&["leaf"], &[], Order::Default);
        let other = set(&["other"], &[], Order::Default);
        let empty = set(&[], &[], Order::Default);
        let reused = composed(None, &[&leaf]).unwrap();
        assert!(Arc::ptr_eq(&reused.node, &leaf.node));
        // An empty own group and repeated members do not count.
        let reused = composed(Some(&empty), &[&leaf, &leaf]).unwrap();
        assert!(Arc::ptr_eq(&reused.node, &leaf.node));
        // A rule forwarding its dependency's group as its own.
        let reused = composed(Some(&leaf), &[&leaf]).unwrap();
        assert!(Arc::ptr_eq(&reused.node, &leaf.node));
        // An own group plus a contributor, or two contributors: one new
        // node over both, own first.
        let both = composed(Some(&other), &[&leaf]).unwrap();
        assert!(!Arc::ptr_eq(&both.node, &leaf.node) && !Arc::ptr_eq(&both.node, &other.node));
        assert_eq!(names(&both), ["other", "leaf"]);
        assert_eq!(both.node.children.len(), 2);
        assert!(both.node.direct.is_empty());
        let both = composed(None, &[&leaf, &other]).unwrap();
        assert_eq!(names(&both), ["leaf", "other"]);
        // A lone set of another order joins a stable-order set (same
        // to_list, stable-order repr); a topological one as its to_list.
        let pre = set(&["p1"], &[&leaf], Order::Preorder);
        let wrapped = composed(None, &[&pre]).unwrap();
        assert_eq!(wrapped.node.order, Order::Default);
        assert!(Arc::ptr_eq(&wrapped.node.children[0], &pre.node));
        assert_eq!(names(&wrapped), names(&pre));
        let topo = set(&["t1"], &[&leaf], Order::Topological);
        let wrapped = composed(None, &[&topo]).unwrap();
        assert_eq!(wrapped.node.order, Order::Default);
        assert_eq!(names(&wrapped), ["t1", "leaf"]);
        assert_eq!(names(&wrapped), names(&topo));
    }

    #[test]
    fn shared_validation_is_the_group_when_stable_order() {
        let leaf = set(&["a", "b"], &[], Order::Default);
        assert!(Arc::ptr_eq(&shared_validation(&leaf).node, &leaf.node));
        for order in [Order::Preorder, Order::Postorder, Order::Topological] {
            let group = set(&["x"], &[&leaf], order);
            let shared = shared_validation(&group);
            assert_eq!(shared.node.order, Order::Default);
            assert_eq!(names(&shared), names(&group), "{order:?}");
            assert_eq!(
                shared.to_string(),
                Depset::from_files(depset_files(&group)).to_string()
            );
        }
    }

    #[test]
    fn framework_compositions_never_check_the_depth() {
        // A chain of 1,100 targets, each with its own validation output and
        // its predecessor's group: one level per target, beyond Starlark's
        // depset() limit (1024), without a panic.
        let mut group = set(&["v0"], &[], Order::Default);
        for index in 1..1100 {
            let own = set(&[&format!("v{index}")], &[], Order::Default);
            group = composed(Some(&own), &[&group]).unwrap();
        }
        assert!(group.node.depth > 1024, "{}", group.node.depth);
        let expected = (0..1100)
            .rev()
            .map(|index| format!("v{index}"))
            .collect::<Vec<_>>();
        assert_eq!(names(&group), expected);
        // Merging deep groups (mergeOutputGroupProviders, the
        // HIDDEN_TOP_LEVEL merge) composes without a depth check too.
        let merged_sets = merge_group_sets(&[
            vec![(VALIDATION.to_owned(), group.clone())],
            vec![(VALIDATION.to_owned(), set(&["extra"], &[], Order::Default))],
        ])
        .unwrap();
        assert_eq!(names(&merged_sets[0].1).len(), 1101);
        let own_hidden = set(&["hidden"], &[], Order::Default);
        let info = merged(vec![(HIDDEN_TOP_LEVEL.to_owned(), own_hidden)], group);
        assert_eq!(info.groups.len(), 1);
        assert_eq!(names(&info.groups[0].1).len(), 1101);
        assert_eq!(names(&info.groups[0].1)[0], "hidden");
    }
}
