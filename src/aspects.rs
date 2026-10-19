//! Starlark aspects at analysis (N58, docs/ASPECTS.md): the registry of
//! loaded aspect definitions, the aspect-collection computation of an edge
//! (AspectResolutionHelpers/AspectCollection) and the provider merge of an
//! aspect-configured target (MergedConfiguredTarget.of).
//!
//! A definition is owned data: the load-time `AspectDeclaration`
//! (crate::rule_declarations) plus the frozen implementation function.
//! Applications are keyed per aspect like Bazel's `AspectKey`
//! (skyframe/AspectKeyCreator.java:96-167): the base configured target key,
//! the aspect's identity and the identities of the directly required
//! aspects (`baseKeys`).

use std::collections::BTreeMap;
use std::sync::Arc;

use starlark::values::OwnedFrozenValue;

use crate::analysis::DependencyTarget;
use crate::config_registry::ConfiguredTargetKey;
use crate::custom_rule::AttrValue;
use crate::label::Label;
use crate::package::Rule;
use crate::providers::ProviderRequirement;
use crate::rule_declarations::AspectDeclaration;

/// One loaded, exported aspect (`StarlarkDefinedAspect`): its identity
/// (`{extension}%{name}`, StarlarkAspectClass.java:34-40), its declaration
/// and the frozen aspect value (whose `implementation` field the analysis
/// reads, keeping the owner alive).
#[derive(Clone, Debug)]
pub(crate) struct AspectDefinition {
    /// The aspect's class name: `<canonical .bzl label>%<exported name>`
    /// (StarlarkAspectClass.getName).
    pub identity: String,
    /// The `.bzl` label the aspect is defined in.
    pub definition_label: Label,
    /// The exported global name.
    pub exported_name: String,
    pub declaration: AspectDeclaration,
    /// The exported aspect value itself; the implementation function and
    /// callbacks stay reachable through it (StarlarkDefinedAspect).
    pub value: OwnedFrozenValue,
    /// Parameter values from `--aspects_parameters`
    /// (AspectParameters; string, bool, int or string-list attributes).
    pub parameters: BTreeMap<String, AttrValue>,
}

/// Whether `name`/`spec` is one of the aspect's parameter attributes: a
/// public (not `_`/`$`-prefixed) attribute of a scalar or string-list type
/// (StarlarkRuleClassFunctions.aspect: an aspect's non-implicit attributes
/// are parameters, `Aspect parameter attribute 'X' must use the 'values'
/// restriction` at load).
pub(crate) fn is_parameter_attribute(name: &str, spec: &crate::custom_rule::AttrSpec) -> bool {
    use crate::custom_rule::AttrKind;
    !name.starts_with('_')
        && !name.starts_with('$')
        && matches!(
            spec.kind,
            AttrKind::String | AttrKind::Bool | AttrKind::Int | AttrKind::StringList
        )
}

impl AspectDefinition {
    /// The aspect's advertised providers (`provides`): what other aspects'
    /// `required_aspect_providers` are satisfied by
    /// (AspectDefinition.getAdvertisedProviders).
    fn advertised(&self) -> &[ProviderRequirement] {
        &self.declaration.provides
    }

    /// `AspectDefinition.requires(other)` (AspectDefinition.java:221-224):
    /// this aspect's required_aspect_providers are satisfied by `other`'s
    /// advertised providers.
    fn requires(&self, other: &AspectDefinition) -> bool {
        satisfies(
            &self.declaration.required_aspect_providers,
            other.advertised(),
        )
    }

    /// RequiredProviders.isSatisfiedBy over clauses (each a conjunction).
    fn applies_to_rule(&self, rule: &Rule) -> bool {
        if self.declaration.required_providers.is_empty() {
            return true;
        }
        // AdvertisedProviderSet.ANY: the forwarding rule classes.
        if let Some(native) = &rule.native
            && matches!(
                native.rule_class.as_str(),
                "alias" | "label_flag" | "label_setting"
            )
        {
            return true;
        }
        let provides = rule
            .custom
            .as_ref()
            .map(|custom| custom.declaration.provides.as_slice())
            .unwrap_or_default();
        satisfies(&self.declaration.required_providers, provides)
    }
}

/// RequiredProviders.isSatisfiedBy (packages/RequiredProviders.java): some
/// clause has each of its requirements advertised.
fn satisfies(required: &[Vec<ProviderRequirement>], advertised: &[ProviderRequirement]) -> bool {
    required.iter().any(|clause| {
        clause
            .iter()
            .all(|r| advertised.iter().any(|a| a.id == r.id))
    })
}

/// The registry of every aspect definition the invocation can reach: those
/// exported by loaded `.bzl` modules and the native `ValidateTarget`
/// (rules/core/ValidateTarget.java). Keyed by identity.
#[derive(Default)]
pub(crate) struct AspectRegistry {
    definitions: BTreeMap<String, Arc<AspectDefinition>>,
}

impl AspectRegistry {
    pub fn insert(&mut self, definition: AspectDefinition) {
        self.definitions
            .insert(definition.identity.clone(), Arc::new(definition));
    }

    pub fn get(&self, identity: &str) -> Option<Arc<AspectDefinition>> {
        self.definitions.get(identity).cloned()
    }
}

/// The name of the native aspect that collects validation outputs
/// (AspectCollection.VALIDATION_ASPECT_NAME, ValidateTarget.java:45).
pub const VALIDATION_ASPECT_NAME: &str = "ValidateTarget";

/// The native ValidateTarget aspect's identity (its class name).
pub const VALIDATION_ASPECT_IDENTITY: &str = VALIDATION_ASPECT_NAME;

/// One aspect application: Bazel's AspectKey — the aspect, the base
/// configured target and the directly required aspects
/// (AspectKeyCreator.AspectKey). Identity and equality follow the key: the
/// same aspect over the same base and base aspects is one application.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AspectApplicationKey {
    pub base: ConfiguredTargetKey,
    pub aspect: String,
    /// The directly required aspects' identities in application order
    /// (`baseKeys`, sorted by name; AspectKeyCreator.createAspectKey).
    pub bases: Vec<String>,
}

impl AspectApplicationKey {
    /// The action owner of the application's actions: distinct from the base
    /// configured target's, so actions of the base target and of several
    /// aspects never share an id (AspectKey as ActionLookupKey).
    pub fn action_owner(&self, top: crate::configuration::ConfigId) -> String {
        let base = self.base.action_owner(top);
        if self.bases.is_empty() {
            format!("{base}%{}", self.aspect)
        } else {
            format!("{base}%{}[{}]", self.aspect, self.bases.join(","))
        }
    }
}

/// The aspects applied to one configured target through one dependency edge:
/// the kept aspects in application order, each with the aspects directly
/// visible to it (AspectCollection: getUsedAspects order and AspectDeps).
#[derive(Clone, Debug, Default)]
pub(crate) struct AspectCollection {
    /// (aspect, directly required aspects) in application order.
    pub used: Vec<(Arc<AspectDefinition>, Vec<String>)>,
}

impl AspectCollection {
    pub fn is_empty(&self) -> bool {
        self.used.is_empty()
    }

    /// The aspects' identities in application order.
    pub fn identities(&self) -> Vec<String> {
        self.used
            .iter()
            .map(|(aspect, _)| aspect.identity.clone())
            .collect()
    }

    /// The aspects' descriptions in application order (AspectDescriptor.
    /// getDescription: the class name without parameters).
    pub fn descriptions(&self) -> Vec<String> {
        self.identities()
    }
}

/// The aspects that propagate along one edge of a target
/// (AspectResolutionHelpers.computePropagatingAspects:86-92): the
/// attribute's own `aspects` list, then the incoming aspect path's aspects
/// that propagate along the attribute name (or are required by one already
/// kept), top-down (collectPropagatingAspects:219-243).
pub(crate) fn propagating_aspects(
    registry: &AspectRegistry,
    attribute_aspects: &[String],
    incoming: &[Arc<AspectDefinition>],
    attribute: &str,
) -> Vec<Arc<AspectDefinition>> {
    let mut result = Vec::new();
    for identity in attribute_aspects {
        if let Some(aspect) = registry.get(identity)
            && !result
                .iter()
                .any(|kept: &Arc<AspectDefinition>| kept.identity == *identity)
        {
            result.push(aspect);
        }
    }
    // Iterating the path backwards is top-down: an aspect joins when it
    // propagates along `attribute` or when an already-kept aspect requires
    // it (isAspectRequired).
    let mut kept: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in incoming.iter().rev() {
        if propagates_along(aspect, attribute) || kept.iter().any(|other| other.requires(aspect)) {
            kept.push(aspect.clone());
        }
    }
    kept.reverse();
    for aspect in kept {
        if !result
            .iter()
            .any(|existing| existing.identity == aspect.identity)
        {
            result.push(aspect);
        }
    }
    result
}

/// AspectDefinition.getAttributeAspects: the aspect propagates along every
/// attribute (`'*'`) or the named one.
fn propagates_along(aspect: &AspectDefinition, attribute: &str) -> bool {
    match &aspect.declaration.attr_aspects {
        None => false, // A function: rejected before, at application.
        Some(names) => names.iter().any(|name| name == "*" || name == attribute),
    }
}

/// The aspect collection of a dependency edge: the propagating aspects the
/// target admits, required aspects pulled in, in application order with
/// their directly visible aspects (computeAspectCollection +
/// AspectCollection.create).
pub(crate) fn aspect_collection(
    mut aspects: Vec<Arc<AspectDefinition>>,
    rule: &Rule,
) -> Result<AspectCollection, String> {
    // computeAspectCollection (AspectResolutionHelpers.java:140-170): from
    // the end of the path, keep the aspect if the target satisfies its
    // required providers or a kept aspect requires it; then reverse.
    let mut kept: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in aspects.iter().rev() {
        if aspect.applies_to_rule(rule) || kept.iter().any(|other| other.requires(aspect)) {
            kept.push(aspect.clone());
        }
    }
    kept.reverse();
    aspects = kept;
    // AspectCollection.create (:266-322) over the deduplicated path.
    let mut order: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in &aspects {
        if let Some(first) = order
            .iter()
            .position(|seen| seen.identity == aspect.identity)
        {
            // validateDuplicateAspect (:349-371): a repeated aspect whose
            // second occurrence would see an aspect after its first one (a
            // different visible set) is a cycle on the path.
            for between in &order[first + 1..] {
                if aspect.requires(between) || between.requires(aspect) {
                    return Err(format!(
                        "Aspect path contains a cycle: aspect '{}' twice, seeing aspect '{}'",
                        aspect.identity, between.identity
                    ));
                }
            }
            continue;
        }
        order.push(aspect.clone());
    }
    // deps[aspect] = the earlier aspects the aspect sees (isSatisfiedBy its
    // required_aspect_providers, explicitly required, or the validation
    // aspect's catch-all), in path order.
    let mut used = Vec::new();
    for (index, aspect) in order.iter().enumerate() {
        let mut bases = Vec::new();
        for earlier in &order[..index] {
            let visible = aspect.requires(earlier) || aspect.identity == VALIDATION_ASPECT_IDENTITY;
            if visible && !bases.contains(&earlier.identity) {
                bases.push(earlier.identity.clone());
            }
        }
        used.push((aspect.clone(), bases));
    }
    Ok(AspectCollection { used })
}

/// The provider merge of an aspect-configured target
/// (MergedConfiguredTarget.of, configuredtargets/MergedConfiguredTarget.java:186-310):
/// aspect providers are looked up first (they shadow the base target's),
/// OutputGroupInfo is merged per group, an aspect's DefaultInfo must only
/// set `files` (which join the files to build), and any other provider an
/// aspect returns that the base target or an earlier aspect also provides
/// fails the merge.
pub(crate) fn merge_providers(
    base: &BTreeMap<String, OwnedFrozenValue>,
    base_files: &[crate::starlark_types::File],
    aspects: &[(AspectApplicationKey, BTreeMap<String, OwnedFrozenValue>)],
) -> Result<MergedProviders, String> {
    let mut providers = base.clone();
    let mut files = base_files.to_vec();
    let mut group_sets: Vec<Vec<(String, crate::starlark_types::Depset)>> = Vec::new();
    if let Some(value) = providers.get(crate::output_group_info::PROVIDER_ID) {
        group_sets.push(
            crate::output_group_info::groups(value.value())
                .map(<[_]>::to_vec)
                .unwrap_or_default(),
        );
    }
    // Whether an aspect already contributed the merged target's
    // InstrumentedFilesInfo (the base target's copy, if any, is
    // overwritten by the first aspect's).
    let mut instrumented_from_aspect = false;
    for (_, aspect_providers) in aspects {
        for (id, value) in aspect_providers {
            if id == crate::output_group_info::PROVIDER_ID {
                group_sets.push(
                    crate::output_group_info::groups(value.value())
                        .map(<[_]>::to_vec)
                        .unwrap_or_default(),
                );
                continue;
            }
            if id == crate::providers::DEFAULT_INFO_PROVIDER_ID {
                // DefaultInfo of an aspect: only `files` may be set
                // (MergedConfiguredTarget.java:216-227); the files join the
                // merged files to build (FileProvider, :187-206).
                match crate::starlark_types::returned_default_info(value.value()) {
                    Ok(Some(info)) => {
                        if info.runfiles.is_some()
                            || info.data_runfiles.is_some()
                            || info.default_runfiles.is_some()
                            || info.executable.is_some()
                        {
                            return Err(
                                "Provider 'DefaultInfo' returned by an aspect not at top level must only have the 'files' field set"
                                    .to_owned(),
                            );
                        }
                        for file in info.files.unwrap_or_default() {
                            if !files.contains(&file) {
                                files.push(file);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return Err(error.to_string()),
                }
                continue;
            }
            if id == crate::instrumented_files::PROVIDER_ID {
                if instrumented_from_aspect {
                    // Two aspects both returned InstrumentedFilesInfo: the
                    // ordinary duplicate error.
                    return Err("Provider InstrumentedFilesInfo provided twice".to_owned());
                }
                // When InstrumentedFilesInfo is on both the base target
                // and an aspect, the base's copy below is overwritten — the
                // aspect's wins, without an error, because a rule and a
                // related aspect may legitimately share the instrumentation
                // implementation (MergedConfiguredTarget.java:296-305).
                instrumented_from_aspect = true;
                providers.insert(id.clone(), value.clone());
                continue;
            }
            if providers.contains_key(id) {
                // `Provider <name> provided twice` (MergingException), with
                // the provider's display name (its exported name); two
                // aspects returning the same provider fail here too.
                let name = id.rsplit('%').next().unwrap_or(id.as_str());
                let name = name.strip_prefix("builtin:").unwrap_or(name);
                return Err(format!("Provider {name} provided twice"));
            }
            providers.insert(id.clone(), value.clone());
        }
    }
    let groups = crate::output_group_info::merge_group_sets(&group_sets);
    Ok(MergedProviders {
        providers,
        files,
        groups,
    })
}
/// An aspect-configured target's merged view: providers (aspects shadow the
/// base target), the files to build (base plus every aspect's
/// DefaultInfo.files) and the merged OutputGroupInfo groups.
pub(crate) struct MergedProviders {
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub files: Vec<crate::starlark_types::File>,
    /// `None` when neither the base target nor any aspect has one.
    pub groups: Option<Vec<(String, crate::starlark_types::Depset)>>,
}

impl MergedProviders {
    /// The provider map with the merged OutputGroupInfo substituted in
    /// (None drops the group provider, e.g. no group was ever set).
    pub fn with_groups(self) -> BTreeMap<String, OwnedFrozenValue> {
        let mut providers = self.providers;
        match self.groups {
            Some(groups) if !groups.is_empty() => {
                let merged = crate::output_group_info::frozen_of_groups(groups);
                providers.insert(crate::output_group_info::PROVIDER_ID.to_owned(), merged);
            }
            _ => {
                providers.remove(crate::output_group_info::PROVIDER_ID);
            }
        }
        providers
    }
}

/// The aspect-configured dependency `target` value: providers merged over
/// the base configured target (MergedConfiguredTarget.of) and the merged
/// files to build. First-match lookup means an aspect's providers shadow
/// the base target's of the same name (rawGetStarlarkProvider:
/// nonBaseProviders first, MergedConfiguredTarget.java:153-172).
pub(crate) fn merged_dependency(
    base: &DependencyTarget,
    aspects: &[(AspectApplicationKey, BTreeMap<String, OwnedFrozenValue>)],
) -> Result<DependencyTarget, String> {
    let merged = merge_providers(&base.providers, &base.files, aspects)?;
    let mut provider_keys = base.provider_keys.clone();
    for id in aspects.iter().flat_map(|(_, providers)| providers.keys()) {
        if !provider_keys.contains(id) {
            provider_keys.push(id.clone());
        }
    }
    let mut provider_files = base.provider_files.clone();
    for value in aspects.iter().flat_map(|(_, providers)| providers.values()) {
        for file in crate::providers::provider_files(value.value()).map_err(|e| e.to_string())? {
            if !provider_files.contains(&file) {
                provider_files.push(file);
            }
        }
    }
    let MergedProviders { files, .. } = &merged;
    let files = files.clone();
    let providers = merged.with_groups();
    // A fresh Target value for the merged configured target: its providers
    // are the merged set, never the base target's identity (which carries
    // the base providers only).
    let identity = std::sync::Arc::new(crate::analysis::dependency_target_data(
        &base.label,
        &base.label,
        &files,
        base.is_source,
        base.is_rule,
        base.is_package_group,
        base.is_filegroup,
        &providers,
        &provider_keys,
        &crate::runfiles::TargetRunfiles::default(),
    ));
    Ok(DependencyTarget {
        label: base.label.clone(),
        files,
        is_source: base.is_source,
        is_rule: base.is_rule,
        is_package_group: base.is_package_group,
        is_filegroup: base.is_filegroup,
        providers,
        provider_files,
        provider_keys,
        runfiles: crate::runfiles::TargetRunfiles::default(),
        rule_class: base.rule_class.clone(),
        identity,
    })
}
