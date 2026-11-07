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
    /// (AspectParameters; string, bool, int or string-list attributes) of
    /// a top-level aspect; empty for an attribute's aspect.
    pub parameters: BTreeMap<String, AttrValue>,
    /// A `--aspects` aspect (or one it requires): its AspectParameters are
    /// every parameter attribute's value, given or default
    /// (StarlarkDefinedAspect.extractTopLevelParameters), which its
    /// description shows.
    pub top_level: bool,
    /// The AspectDescriptor ([`Self::description`]): the identity of an
    /// application, so one aspect with two parameter sets on one target is
    /// two applications (an attribute's aspect with its rule's parameters
    /// and the same aspect from `--aspects`), equal descriptors one.
    pub descriptor: String,
    /// A function `attr_aspects` evaluated over the target this definition
    /// is applied to (crate::aspect_propagation::computed_attr_aspects,
    /// AspectResolutionHelpers.computeAttributeAspects): the attribute
    /// names it propagates along there. `None` for a fixed list.
    pub computed_attr_aspects: Option<Arc<[String]>>,
    /// A function `toolchains_aspects` evaluated over that target
    /// (computeToolchainsAspects): the toolchain type labels it propagates
    /// to. `None` for a fixed list.
    pub computed_toolchains_aspects: Option<Arc<[String]>>,
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
    /// [`Self::description`] into [`Self::descriptor`] (after the
    /// parameters changed).
    pub fn refresh_descriptor(&mut self) {
        self.descriptor = self.description();
    }

    /// This aspect attached to an attribute of `rule` (the rule declaring
    /// `attr.*(aspects = [...])`): its AspectParameters are the values of
    /// `rule`'s same-named, same-typed, non-configurable attributes
    /// (StarlarkDefinedAspect.getDefaultParametersExtractor, :300-343).
    /// `None` when that gives no parameter (the shared definition applies).
    pub fn for_attribute_of(&self, rule: &Rule) -> Option<AspectDefinition> {
        let invocation = rule.custom.as_ref()?;
        let mut parameters = BTreeMap::new();
        for (name, spec) in &self.declaration.attrs {
            if !is_parameter_attribute(name, spec) {
                continue;
            }
            let Some(rule_spec) = invocation.schema.get(name) else {
                continue;
            };
            if rule_spec.kind != spec.kind {
                continue;
            }
            let value = invocation
                .attrs
                .get(name)
                .unwrap_or(&rule_spec.default)
                .clone();
            parameters.insert(name.clone(), value);
        }
        if parameters.is_empty() {
            return None;
        }
        let mut definition = self.clone();
        definition.parameters = parameters;
        definition.top_level = false;
        definition.refresh_descriptor();
        Some(definition)
    }

    /// AspectDescriptor.getDescription (packages/AspectDescriptor.java:
    /// 92-114): the class name, then `[name="value",...]` when the aspect
    /// has parameters, in the aspect's attribute order. A top-level aspect
    /// has every parameter attribute's value; an attribute's aspect those
    /// its rule gave ([`Self::for_attribute_of`]).
    pub fn description(&self) -> String {
        let mut entries = Vec::new();
        for (name, spec) in &self.declaration.attrs {
            if !is_parameter_attribute(name, spec) {
                continue;
            }
            let value = if self.top_level {
                self.parameters.get(name).unwrap_or(&spec.default)
            } else {
                match self.parameters.get(name) {
                    Some(value) => value,
                    None => continue,
                }
            };
            let value = match value {
                AttrValue::String(text) => text.clone(),
                AttrValue::Bool(value) => value.to_string(),
                AttrValue::Int(value) => value.to_string(),
                AttrValue::Strings(items) => items.join(","),
                _ => continue,
            };
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            entries.push(format!("{name}=\"{escaped}\""));
        }
        if entries.is_empty() {
            return self.identity.clone();
        }
        format!("{}[{}]", self.identity, entries.join(","))
    }

    /// The `transition()` object of the aspect attribute `name`'s Starlark
    /// transition (`cfg = some_transition`), kept alive by the aspect
    /// value's owner.
    pub fn attribute_transition(&self, name: &str) -> Option<OwnedFrozenValue> {
        let aspect = crate::rule_declarations::Aspect::from_value(self.value.value())?;
        let index = aspect.transition_keys.iter().position(|key| key == name)?;
        aspect.transitions.get(index)?;
        Some(self.value.map(|value| {
            starlark::values::ValueLike::downcast_ref::<crate::rule_declarations::FrozenAspect>(
                value,
            )
            .map(|aspect| aspect.transitions[index])
            .unwrap_or(value)
        }))
    }

    /// The aspect's advertised providers (`provides`): what other aspects'
    /// `required_aspect_providers` are satisfied by
    /// (AspectDefinition.getAdvertisedProviders).
    fn advertised(&self) -> &[ProviderRequirement] {
        &self.declaration.provides
    }

    /// `AspectDefinition.requires(other)` (AspectDefinition.java:221-224):
    /// `other` is one of this aspect's `requires` (requiredAspectClasses).
    fn requires(&self, other: &AspectDefinition) -> bool {
        self.declaration.requires.contains(&other.identity)
    }

    /// Whether `other`, earlier on the path, is visible to this aspect
    /// (AspectCollection.create :300-308, validateDuplicateAspect
    /// :358-366): this aspect's required_aspect_providers are satisfied by
    /// `other`'s advertised providers, or this aspect requires it.
    fn sees(&self, other: &AspectDefinition) -> bool {
        satisfies(
            &self.declaration.required_aspect_providers,
            other.advertised(),
        ) || self.requires(other)
    }

    /// RequiredProviders.isSatisfiedBy over clauses (each a conjunction).
    pub fn applies_to_rule(&self, rule: &Rule) -> bool {
        if self.declaration.required_providers.is_empty() {
            return true;
        }
        // A native rule class's advertised providers; AdvertisedProviderSet
        // .ANY for the forwarding rule classes.
        if rule.custom.is_none()
            && let Some(native) = &rule.native
        {
            let Some(advertised) = crate::native_rules::advertised_providers(&native.rule_class)
            else {
                return true;
            };
            return self.declaration.required_providers.iter().any(|clause| {
                clause
                    .iter()
                    .all(|required| advertised.contains(&required.id.as_str()))
            });
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The aspects' descriptors in application order.
    pub fn descriptors(&self) -> Vec<String> {
        self.used
            .iter()
            .map(|(aspect, _)| aspect.descriptor.clone())
            .collect()
    }

    /// The descriptions of `identity`'s application path: its base
    /// aspects in topological order (a postorder over the directly
    /// required aspects, AspectFunction.collectAspectKeysInTopologicalOrder
    /// :851-866), then itself — RuleContext.getAspectDescriptors, read by
    /// `ctx.aspect_ids` and the `ASPECTS aspect on KIND` messages.
    pub fn path_descriptions(&self, descriptor: &str) -> Vec<String> {
        self.path_aspects(descriptor)
            .iter()
            .map(|aspect| aspect.description())
            .collect()
    }

    /// The aspects of `descriptor`'s application path (RuleContext.
    /// getAspects: the topological aspect path, AspectFunction.java:342-363):
    /// its base aspects in topological order, then itself. A base aspect
    /// that is a NonApplicableAspect on the target stays on the path.
    pub fn path_aspects(&self, descriptor: &str) -> Vec<Arc<AspectDefinition>> {
        fn visit(
            collection: &AspectCollection,
            descriptor: &str,
            ordered: &mut Vec<Arc<AspectDefinition>>,
        ) {
            let Some((aspect, bases)) = collection
                .used
                .iter()
                .find(|(aspect, _)| aspect.descriptor == descriptor)
            else {
                return;
            };
            // The AspectKey's baseKeys are sorted by aspect class name
            // (AspectKeyCreator.createAspectKey, :127-133), the order the
            // postorder visits them in.
            let mut sorted: Vec<&AspectDefinition> = bases
                .iter()
                .filter_map(|base| {
                    collection
                        .used
                        .iter()
                        .find(|(used, _)| used.descriptor == *base)
                        .map(|(used, _)| used.as_ref())
                })
                .collect();
            sorted.sort_by(|a, b| a.identity.cmp(&b.identity));
            for base in sorted {
                if !ordered
                    .iter()
                    .any(|seen| seen.descriptor == base.descriptor)
                {
                    visit(collection, &base.descriptor, ordered);
                }
            }
            if !ordered
                .iter()
                .any(|seen| seen.descriptor == aspect.descriptor)
            {
                ordered.push(aspect.clone());
            }
        }
        let mut ordered = Vec::new();
        visit(self, descriptor, &mut ordered);
        ordered
    }
}

/// The aspects that propagate along one edge of a target
/// (AspectResolutionHelpers.computePropagatingAspects:86-92): the
/// attribute's own `aspects` list, then the incoming aspect path's aspects
/// that propagate along the attribute name (or are required by one already
/// kept), top-down (collectPropagatingAspects:219-243).
pub(crate) fn propagating_aspects(
    attribute_aspects: &[Arc<AspectDefinition>],
    incoming: &[Arc<AspectDefinition>],
    attribute: &str,
) -> Vec<Arc<AspectDefinition>> {
    let mut result: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in attribute_aspects {
        if !result
            .iter()
            .any(|kept| kept.descriptor == aspect.descriptor)
        {
            result.push(aspect.clone());
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
            .any(|existing| existing.descriptor == aspect.descriptor)
        {
            result.push(aspect);
        }
    }
    result
}

/// The attributes of the base aspects of an application path (`path`, the
/// main aspect last) that `ctx.rule.attr` holds besides the base rule's
/// (StarlarkRuleContext.java:302-316, AspectContext.
/// mergeRuleAndBaseAspectsAttributes, DependencyResolutionHelpers.
/// addAspectAttributes): per base aspect in path order, its attribute names
/// that neither the rule nor an earlier base aspect has (the first
/// attribute of a name wins, StarlarkAttributesCollection.Builder.
/// addAttribute). The main aspect's own attributes are `ctx.attr`'s only.
pub(crate) fn base_aspect_attributes(
    path: &[Arc<AspectDefinition>],
    rule: &Rule,
) -> Vec<(Arc<AspectDefinition>, Vec<String>)> {
    let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    taken.insert("name".to_owned());
    if let Some(invocation) = &rule.custom {
        taken.extend(invocation.schema.keys().cloned());
        taken.extend(invocation.attrs.keys().cloned());
    }
    let mut result = Vec::new();
    for aspect in path.iter().take(path.len().saturating_sub(1)) {
        let mut names = Vec::new();
        for (name, _) in &aspect.declaration.attrs {
            if taken.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        if !names.is_empty() {
            result.push((aspect.clone(), names));
        }
    }
    result
}

/// The aspects of an application path `path` (bottom up, the main aspect
/// last) that propagate along the attribute `attribute` of the aspect
/// `owner` (collectPropagatingAspects, AspectResolutionHelpers.java:
/// 211-243 with `aspectOwningAttribute`): an aspect never propagates over
/// its own attributes; another one does when its `attr_aspects` names the
/// attribute (or `'*'`), or when an already-kept aspect requires it.
pub(crate) fn propagating_over_aspect_attribute(
    path: &[Arc<AspectDefinition>],
    owner: &AspectDefinition,
    attribute: &str,
) -> Vec<Arc<AspectDefinition>> {
    let mut kept: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in path.iter().rev() {
        if aspect.identity == owner.identity {
            continue;
        }
        if propagates_along(aspect, attribute) || kept.iter().any(|other| other.requires(aspect)) {
            kept.push(aspect.clone());
        }
    }
    kept.reverse();
    kept
}

/// The aspects of a target's path (`path`, bottom up) that propagate to
/// the target's resolved toolchain of a type requested as any of
/// `type_labels` (the type label and every requested alias of it;
/// AspectResolutionHelpers.computePropagatingAspectsToToolchainDep:
/// `toolchains_aspects` names one of them or is `'*'`, or an already-kept
/// aspect requires the aspect), top-down then reversed.
pub(crate) fn propagating_to_toolchain(
    path: &[Arc<AspectDefinition>],
    type_labels: &[crate::label::Label],
) -> Vec<Arc<AspectDefinition>> {
    let names_type = |name: &str| {
        name == "*"
            || crate::label::Label::parse_canonical(name)
                .is_ok_and(|label| type_labels.contains(&label))
    };
    let mut kept: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in path.iter().rev() {
        let propagates = match (
            &aspect.computed_toolchains_aspects,
            &aspect.declaration.toolchains_aspects,
        ) {
            (Some(names), _) => names.iter().any(|name| names_type(name)),
            (None, Some(names)) => names.iter().any(|name| names_type(name)),
            (None, None) => false,
        };
        if propagates || kept.iter().any(|other| other.requires(aspect)) {
            kept.push(aspect.clone());
        }
    }
    kept.reverse();
    kept
}

/// AspectDefinition.getAttributeAspects: the aspect propagates along every
/// attribute (`'*'`) or the named one.
fn propagates_along(aspect: &AspectDefinition, attribute: &str) -> bool {
    if let Some(names) = &aspect.computed_attr_aspects {
        // A function's list (never `'*'`, parseAttrAspects allowAll=false).
        return names.iter().any(|name| name == attribute);
    }
    match &aspect.declaration.attr_aspects {
        // A function not evaluated over this target propagates nowhere.
        None => false,
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
    predicate: &mut dyn FnMut(&AspectDefinition) -> Result<bool, String>,
) -> Result<AspectCollection, String> {
    // computeAspectCollection (AspectResolutionHelpers.java:140-170): from
    // the end of the path, keep the aspect if the target satisfies its
    // required providers or a kept aspect requires it, and its
    // propagation_predicate accepts the target
    // (evaluatePropagationPredicate); then reverse.
    let mut kept: Vec<Arc<AspectDefinition>> = Vec::new();
    for aspect in aspects.iter().rev() {
        if (aspect.applies_to_rule(rule) || kept.iter().any(|other| other.requires(aspect)))
            && predicate(aspect)?
        {
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
            .position(|seen| seen.descriptor == aspect.descriptor)
        {
            // validateDuplicateAspect (:349-371): a repeated aspect whose
            // second occurrence would see an aspect after its first one (a
            // different visible set) is a cycle on the path.
            for between in &order[first + 1..] {
                if aspect.sees(between) {
                    return Err(format!(
                        "Aspect {} is applied twice, both before and after aspect {} (when propagating to {})",
                        aspect.descriptor,
                        between.descriptor,
                        crate::rule_declarations::canonical_form(&rule.label)
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
            let visible = aspect.sees(earlier) || aspect.identity == VALIDATION_ASPECT_IDENTITY;
            if visible && !bases.contains(&earlier.descriptor) {
                bases.push(earlier.descriptor.clone());
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
    let MergedProviders { files, .. } = &merged;
    let files = files.clone();
    let providers = merged.with_groups();
    // A fresh Target value for the merged configured target: its providers
    // are the merged set, never the base target's identity (which carries
    // the base providers only). Its FilesToRunProvider and RunfilesProvider
    // are the base target's: an aspect cannot return them, so the lookup
    // falls through to the base (MergedConfiguredTarget.getProvider,
    // MergedConfiguredTarget.java:131-140) and `target[DefaultInfo]`
    // delegates to them (DefaultInfo.DelegatingDefaultInfo).
    let runfiles = std::sync::Arc::clone(&base.runfiles);
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
        &runfiles,
    ));
    Ok(DependencyTarget::new(crate::analysis::DependencyView {
        label: base.label.clone(),
        files,
        is_source: base.is_source,
        is_rule: base.is_rule,
        is_package_group: base.is_package_group,
        is_filegroup: base.is_filegroup,
        providers,
        provider_keys,
        runfiles,
        rule_class: base.rule_class.clone(),
        identity,
    }))
}
