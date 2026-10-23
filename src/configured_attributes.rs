//! Configured attribute resolution: `select()` against analyzed
//! `config_setting` conditions (ConfiguredAttributeMapper).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use std::collections::{BTreeMap, BTreeSet};

use crate::custom_rule::{AttrValue, ConfigurablePart};
use crate::label::Label;
use crate::package::Rule;

/// BuildType.Selector.DEFAULT_CONDITION_KEY.
pub const DEFAULT_CONDITION: &str = "//conditions:default";

/// One difference of a non-matching condition (ConfigMatchingProvider.MatchResult.NoMatch.Diff,
/// analysis/config/ConfigMatchingProvider.java:83-101).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub what: Label,
    pub got: String,
    pub want: String,
}

/// ConfigMatchingProvider.MatchResult (ConfigMatchingProvider.java:49-141).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchResult {
    Match,
    NoMatch(Vec<Diff>),
    InError(Vec<String>),
}

impl MatchResult {
    /// MatchResult.combine (ConfigMatchingProvider.java:114-140).
    pub fn combine(self, current: MatchResult) -> MatchResult {
        match (self, current) {
            (MatchResult::InError(mut previous), MatchResult::InError(current)) => {
                previous.extend(current);
                MatchResult::InError(previous)
            }
            (previous @ MatchResult::InError(_), _) => previous,
            (MatchResult::NoMatch(_), current @ MatchResult::InError(_)) => current,
            (MatchResult::NoMatch(mut previous), MatchResult::NoMatch(current)) => {
                previous.extend(current);
                MatchResult::NoMatch(previous)
            }
            (previous @ MatchResult::NoMatch(_), MatchResult::Match) => previous,
            (MatchResult::Match, current) => current,
        }
    }
}

/// ConfigMatchingProvider (ConfigMatchingProvider.java:37-234).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigMatching {
    pub label: Label,
    /// `settingsMap` multimap entries in iteration order.
    pub settings: Vec<(String, String)>,
    /// `flagSettingsMap` (N16b).
    pub flags: BTreeMap<Label, String>,
    /// `constraintValuesSetting`: constraint_values labels and their
    /// refinement chains (crate::config_setting).
    pub constraints: BTreeSet<Label>,
    pub result: MatchResult,
}

impl ConfigMatching {
    /// ConfigMatchingProvider.refines (ConfigMatchingProvider.java:204-227): this
    /// matcher's conditions are a proper superset of the other's.
    pub fn refines(&self, other: &ConfigMatching) -> bool {
        let settings = self.settings.iter().collect::<BTreeSet<_>>();
        let other_settings = other.settings.iter().collect::<BTreeSet<_>>();
        let flags = self.flags.iter().collect::<BTreeSet<_>>();
        let other_flags = other.flags.iter().collect::<BTreeSet<_>>();
        if !settings.is_superset(&other_settings)
            || !flags.is_superset(&other_flags)
            || !self.constraints.is_superset(&other.constraints)
        {
            return false;
        }
        settings.len() > other_settings.len()
            || flags.len() > other_flags.len()
            || self.constraints.len() > other.constraints.len()
    }
}

/// The value type of a configurable attribute, for `Type.concat` and
/// `Type.getDefaultValue` (packages/Type.java, packages/BuildType.java).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// Labels, booleans, tristates, outputs and licenses: no concatenation.
    Scalar,
    String,
    Int,
    List,
    Dict,
}

impl ValueType {
    pub fn of(value: &AttrValue) -> Self {
        match value {
            AttrValue::String(_) => Self::String,
            AttrValue::Int(_) => Self::Int,
            AttrValue::Strings(_)
            | AttrValue::Labels(_)
            | AttrValue::Outputs(_)
            | AttrValue::Ints(_) => Self::List,
            AttrValue::StringDict(_)
            | AttrValue::StringListDict(_)
            | AttrValue::LabelKeyedStringDict(_)
            | AttrValue::StringKeyedLabelDict(_)
            | AttrValue::LabelListDict(_) => Self::Dict,
            AttrValue::Bool(_) | AttrValue::Label(_) | AttrValue::Output(_) => Self::Scalar,
        }
    }
}

/// `Type.concat` (packages/Type.java:245-247): list append (:774-780), string
/// join (:478-480), int sum truncated to signed 32 bits (:378-387), dict union
/// keeping the last value (:604-610, ImmutableMap.Builder.buildKeepingLast,
/// whose key order is the first insertion's). `None` when unsupported.
pub fn concat(values: Vec<AttrValue>) -> Option<AttrValue> {
    fn union<V>(target: &mut Vec<(String, V)>, entries: Vec<(String, V)>) {
        for (key, value) in entries {
            match target.iter_mut().find(|(seen, _)| *seen == key) {
                Some(entry) => entry.1 = value,
                None => target.push((key, value)),
            }
        }
    }
    let mut iter = values.into_iter();
    let mut result = iter.next()?;
    for value in iter {
        result = match (result, value) {
            (AttrValue::String(a), AttrValue::String(b)) => AttrValue::String(a + &b),
            (AttrValue::Int(a), AttrValue::Int(b)) => AttrValue::Int(a.wrapping_add(b)),
            (AttrValue::Strings(mut a), AttrValue::Strings(b)) => {
                a.extend(b);
                AttrValue::Strings(a)
            }
            (AttrValue::Labels(mut a), AttrValue::Labels(b)) => {
                a.extend(b);
                AttrValue::Labels(a)
            }
            (AttrValue::Outputs(mut a), AttrValue::Outputs(b)) => {
                a.extend(b);
                AttrValue::Outputs(a)
            }
            (AttrValue::Ints(mut a), AttrValue::Ints(b)) => {
                a.extend(b);
                AttrValue::Ints(a)
            }
            (AttrValue::StringDict(mut a), AttrValue::StringDict(b)) => {
                union(&mut a, b);
                AttrValue::StringDict(a)
            }
            (AttrValue::StringListDict(mut a), AttrValue::StringListDict(b)) => {
                union(&mut a, b);
                AttrValue::StringListDict(a)
            }
            (AttrValue::LabelKeyedStringDict(mut a), AttrValue::LabelKeyedStringDict(b)) => {
                union(&mut a, b);
                AttrValue::LabelKeyedStringDict(a)
            }
            (AttrValue::StringKeyedLabelDict(mut a), AttrValue::StringKeyedLabelDict(b)) => {
                union(&mut a, b);
                AttrValue::StringKeyedLabelDict(a)
            }
            (AttrValue::LabelListDict(mut a), AttrValue::LabelListDict(b)) => {
                union(&mut a, b);
                AttrValue::LabelListDict(a)
            }
            _ => return None,
        };
    }
    Some(result)
}

/// A BUILD `select()` value after BuildType.selectableConvert's
/// unconditional-select simplification (packages/BuildType.java:165-211,
/// `--incompatible_simplify_unconditional_selects_in_rule_attrs` defaults to
/// true, BuildLanguageOptions.java:755-756).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Simplified {
    /// Some select has a non-default condition: resolved at analysis.
    Retained(Vec<ConfigurablePart>),
    /// Every select is `{"//conditions:default": v}`: the plain value, a
    /// `None` branch being the type's default (Selector, BuildType.java:722-725).
    Plain(AttrValue),
    /// `select({"//conditions:default": None})` of a type whose default is
    /// null: the attribute is not set at all (AttributeProvider.java:269-272).
    Unset,
}

/// Simplify `parts` (`type_default` is `Type.getDefaultValue()`, `None` for
/// label types).
pub fn simplify_unconditional(
    parts: Vec<ConfigurablePart>,
    type_default: Option<AttrValue>,
) -> Simplified {
    let unconditional = parts.iter().all(|part| match part {
        ConfigurablePart::Plain(_) => true,
        ConfigurablePart::Select { branches, .. } => {
            branches.len() == 1 && branches[0].0 == DEFAULT_CONDITION
        }
    });
    if !unconditional {
        return Simplified::Retained(parts);
    }
    let mut values = parts
        .into_iter()
        .map(|part| match part {
            ConfigurablePart::Plain(value) => Some(value),
            ConfigurablePart::Select { mut branches, .. } => branches
                .pop()
                .and_then(|(_, value)| value)
                .or_else(|| type_default.clone()),
        })
        .collect::<Vec<_>>();
    if values.len() == 1 {
        return values
            .pop()
            .flatten()
            .map_or(Simplified::Unset, Simplified::Plain);
    }
    // Null-default types reject concatenation at loading (BuildType.java:177-180).
    match values
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .and_then(concat)
    {
        Some(value) => Simplified::Plain(value),
        None => Simplified::Unset,
    }
}

/// How an attribute's default is obtained when a branch is `None`.
pub struct AttributeInfo<'a> {
    pub name: &'a str,
    pub mandatory: bool,
    /// `Type.getDefaultValue()`: the value a `None` branch holds inside the
    /// Selector (BuildType.java:722-725), used for duplicate-value matching.
    pub type_default: Option<AttrValue>,
    /// `Attribute.getDefaultValue(rule)`: substituted for a chosen `None`
    /// (ConfiguredAttributeMapper.java:170-185).
    pub default: Option<AttrValue>,
}

/// A resolved configurable attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// `None` only when the resolved value is null (a label attribute's default).
    pub value: Option<AttrValue>,
    /// ConfiguredAttributeMapper.isAttributeValueExplicitlySpecified
    /// (ConfiguredAttributeMapper.java:435-453): some select chose a non-None branch
    /// or the expression has a plain element.
    pub explicit: bool,
}

/// Inputs shared by every attribute of one configured target.
pub struct Resolver<'a> {
    pub target: &'a Label,
    /// Analyzed conditions by canonical label text.
    pub conditions: &'a BTreeMap<String, ConfigMatching>,
    /// Configuration identifier (`BuildConfigurationValue.shortId` in Bazel).
    pub config_hash: String,
}

fn shown(key: &str) -> String {
    Label::parse_canonical(key)
        .map(|label| crate::rule_declarations::canonical_form(&label))
        .unwrap_or_else(|_| key.to_owned())
}

impl Resolver<'_> {
    /// ConfiguredAttributeMapper.resolveSelector (ConfiguredAttributeMapper.java:277-381):
    /// the chosen branch key and value, or the failure message.
    pub fn resolve_selector<'b>(
        &self,
        attribute: &AttributeInfo<'_>,
        branches: &'b [(String, Option<AttrValue>)],
        no_match_error: &str,
    ) -> Result<&'b (String, Option<AttrValue>), String> {
        let effective =
            |value: &'b Option<AttrValue>| value.as_ref().or(attribute.type_default.as_ref());
        let mut matching: Vec<(&'b (String, Option<AttrValue>), &ConfigMatching)> = Vec::new();
        let mut condition_labels: Vec<&str> = Vec::new();
        let mut errors = Vec::new();
        for branch in branches {
            let key = branch.0.as_str();
            if key == DEFAULT_CONDITION {
                continue;
            }
            // A missing condition means the rule is in error (:296-300).
            let Some(condition) = self.conditions.get(key) else {
                continue;
            };
            if !condition_labels.contains(&key) {
                condition_labels.push(key);
            }
            match &condition.result {
                MatchResult::InError(messages) => errors.push(format!(
                    "config_setting {} is unresolvable because: {}",
                    shown(key),
                    messages.join(", ")
                )),
                MatchResult::Match => {
                    // Keep only matches not strictly less precise than another (:313-334).
                    let mut suppressed = false;
                    let mut index = 0;
                    while index < matching.len() {
                        let existing = matching[index].1;
                        if condition.refines(existing) {
                            matching.remove(index);
                        } else if existing.refines(condition) {
                            suppressed = true;
                            break;
                        } else {
                            index += 1;
                        }
                    }
                    if !suppressed {
                        // LinkedHashMap.put keeps an existing key's position.
                        match matching.iter_mut().find(|(seen, _)| seen.0 == key) {
                            Some(entry) => *entry = (branch, condition),
                            None => matching.push((branch, condition)),
                        }
                    }
                }
                MatchResult::NoMatch(_) => {}
            }
        }
        let label = crate::rule_declarations::canonical_form(self.target);
        let name = attribute.name;
        if !errors.is_empty() {
            return Err(format!(
                "Unresolvable config_settings for configurable attribute \"{name}\" in {label}:\n{}",
                errors.join("\n")
            ));
        }
        // Stream.distinct uses Java equals: lists compare in order, maps
        // (Map.equals) as entry sets regardless of order.
        let mut distinct: Vec<Option<&AttrValue>> = Vec::new();
        for (branch, _) in &matching {
            let value = effective(&branch.1);
            if !distinct.iter().any(|seen| java_equals(*seen, value)) {
                distinct.push(value);
            }
        }
        if distinct.len() > 1 {
            return Err(format!(
                "Illegal ambiguous match on configurable attribute \"{name}\" in {label}:\n{}\nMultiple matches are not allowed unless one is unambiguously more specialized or they resolve to the same value. See https://bazel.build/reference/be/functions#select.",
                matching
                    .iter()
                    .map(|(branch, _)| shown(&branch.0))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if let Some((branch, _)) = matching.first() {
            return Ok(branch);
        }
        if let Some(default) = branches.iter().find(|(key, _)| key == DEFAULT_CONDITION) {
            return Ok(default);
        }
        // --debug_select_always_succeed is not a modelled option (always false).
        Err(no_match_error_message(
            name,
            no_match_error,
            &condition_labels
                .iter()
                .map(|key| shown(key))
                .collect::<Vec<_>>(),
            &label,
            &self.config_hash,
        ))
    }

    /// ConfiguredAttributeMapper.getResolvedAttribute (ConfiguredAttributeMapper.java:157-193).
    pub fn resolve_attribute(
        &self,
        attribute: &AttributeInfo<'_>,
        parts: &[ConfigurablePart],
    ) -> Result<Resolved, String> {
        let mut values = Vec::with_capacity(parts.len());
        let mut explicit = false;
        for part in parts {
            match part {
                // A plain element is an unconditional Selector whose value is set.
                ConfigurablePart::Plain(value) => {
                    explicit = true;
                    values.push(Some(value.clone()));
                }
                ConfigurablePart::Select {
                    branches,
                    no_match_error,
                } => {
                    let (_, value) = self.resolve_selector(attribute, branches, no_match_error)?;
                    match value {
                        Some(value) => {
                            explicit = true;
                            values.push(Some(value.clone()));
                        }
                        None => {
                            if attribute.mandatory {
                                return Err(format!(
                                    "Mandatory attribute '{}' resolved to 'None' after evaluating 'select' expression",
                                    attribute.name
                                ));
                            }
                            values.push(attribute.default.clone());
                        }
                    }
                }
            }
        }
        let value = if values.len() == 1 {
            values.pop().expect("one value")
        } else {
            // Concatenation of a null (label) value cannot occur: such types
            // reject `select() + ...` at loading (BuildType.java:177-180).
            let values = values
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    format!(
                        "attribute '{}': null value in a select() concatenation",
                        attribute.name
                    )
                })?;
            Some(concat(values).ok_or_else(|| {
                format!(
                    "attribute '{}': type doesn't support select concatenation",
                    attribute.name
                )
            })?)
        };
        Ok(Resolved { value, explicit })
    }
}

/// Java `equals` of two attribute values: dictionaries are equal as entry
/// sets (AbstractMap.equals), everything else structurally.
fn java_equals(a: Option<&AttrValue>, b: Option<&AttrValue>) -> bool {
    fn sorted<V: Ord + Clone>(entries: &[(String, V)]) -> Vec<(String, V)> {
        let mut entries = entries.to_vec();
        entries.sort();
        entries
    }
    match (a, b) {
        (Some(AttrValue::StringDict(a)), Some(AttrValue::StringDict(b)))
        | (Some(AttrValue::LabelKeyedStringDict(a)), Some(AttrValue::LabelKeyedStringDict(b)))
        | (Some(AttrValue::StringKeyedLabelDict(a)), Some(AttrValue::StringKeyedLabelDict(b))) => {
            sorted(a) == sorted(b)
        }
        (Some(AttrValue::StringListDict(a)), Some(AttrValue::StringListDict(b)))
        | (Some(AttrValue::LabelListDict(a)), Some(AttrValue::LabelListDict(b))) => {
            sorted(a) == sorted(b)
        }
        _ => a == b,
    }
}

/// ConfiguredAttributeMapper.noMatchError (ConfiguredAttributeMapper.java:387-420).
pub fn no_match_error_message(
    attribute: &str,
    custom: &str,
    conditions: &[String],
    target: &str,
    config_hash: &str,
) -> String {
    let mut error = format!(
        "configurable attribute \"{attribute}\" in {target} doesn't match this configuration"
    );
    if !custom.is_empty() {
        error.push_str(&format!(": {custom}\n"));
    } else {
        error.push_str(&format!(
            ". Would a default condition help?\n\nConditions checked:\n {}\n\nTo see a condition's definition, run: bazel query --output=build <condition label>.\n",
            conditions.join("\n ")
        ));
    }
    let short = &config_hash[..config_hash.len().min(7)];
    error.push_str(&format!(
        "\nThis instance of {target} has configuration identifier {short}. To inspect its configuration, run: bazel config {short}.\n"
    ));
    error.push_str("\nFor more help, see https://bazel.build/docs/configurable-attributes#faq-select-choose-condition.\n\n");
    error
}

/// `$config_dependencies` (AttributeProvider.populateConfigDependenciesAttribute,
/// packages/AttributeProvider.java:443-466): select keys of every attribute,
/// in attribute order, deduplicated, without `//conditions:default`
/// (SelectorList.getKeyLabels, packages/BuildType.java:627-638).
pub fn config_dependency_labels<'a>(
    attributes: impl IntoIterator<Item = &'a [ConfigurablePart]>,
) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for parts in attributes {
        for part in parts {
            if let ConfigurablePart::Select { branches, .. } = part {
                for (key, _) in branches {
                    if key != DEFAULT_CONDITION && !labels.contains(key) {
                        labels.push(key.clone());
                    }
                }
            }
        }
    }
    labels
}

/// Every configurable attribute of a rule in rule-class attribute order:
/// `name`, then the class's (or Starlark base rule's) built-in attributes,
/// then Starlark attributes in `attrs` order (AttributeProvider.getAttributes,
/// packages/AttributeProvider.java:142; RuleClass.Builder insertion order,
/// packages/RuleClass.java:979, 1352).
fn configurable_attributes(rule: &Rule) -> Vec<(AttributeSource<'_>, &[ConfigurablePart])> {
    let mut attributes = Vec::new();
    if let Some(native) = &rule.native
        && let Some(class) = crate::native_rules::rule_class(&native.rule_class)
    {
        for attribute in class.attrs {
            if let Some(parts) = native.configurable.get(attribute.name) {
                attributes.push((AttributeSource::Native(attribute), parts.as_slice()));
            }
        }
    }
    if let Some(custom) = &rule.custom {
        let builtin = &custom.declaration.builtin;
        for attribute in crate::native_rules::starlark_builtins(builtin.kind).attrs() {
            if let Some(parts) = builtin.target.configurable.get(attribute.name) {
                attributes.push((AttributeSource::Native(attribute.clone()), parts.as_slice()));
            }
        }
        for name in &custom.declaration.attr_order {
            if let (Some(parts), Some(spec)) = (
                custom.declaration.configurable.get(name),
                custom.schema.get(name),
            ) {
                attributes.push((AttributeSource::Starlark(name, spec), parts.as_slice()));
            }
        }
    }
    attributes
}

enum AttributeSource<'a> {
    Native(crate::native_rules::NativeAttr),
    Starlark(&'a str, &'a crate::custom_rule::AttrSpec),
}

impl AttributeSource<'_> {
    fn info(&self) -> AttributeInfo<'_> {
        match self {
            Self::Native(attribute) => {
                let type_default = crate::native_rules::type_default(attribute.ty);
                AttributeInfo {
                    name: attribute.name,
                    mandatory: attribute.mandatory,
                    // Native attribute defaults are the type defaults, except
                    // shard_count (-1, StarlarkRuleClassFunctions.java:281).
                    default: if attribute.name == "shard_count" {
                        Some(AttrValue::Int(-1))
                    } else {
                        type_default.clone()
                    },
                    type_default,
                }
            }
            Self::Starlark(name, spec) => AttributeInfo {
                name,
                mandatory: spec.mandatory,
                type_default: Some(spec.kind.type_default()),
                default: Some(spec.default.clone()),
            },
        }
    }
}

/// The `$config_dependencies` value of a rule (see [`config_dependency_labels`]).
pub fn rule_config_dependencies(rule: &Rule) -> Vec<String> {
    config_dependency_labels(
        configurable_attributes(rule)
            .into_iter()
            .map(|(_, parts)| parts),
    )
}

/// RuleContext.Builder.checkAttributesForDuplicateLabels
/// (analysis/RuleContext.java:1553-1568) with
/// ConfiguredAttributeMapper.checkForDuplicateLabels
/// (packages/ConfiguredAttributeMapper.java:456-467): a LABEL_LIST attribute
/// whose value concatenates several elements (`[...] + select(...)`, more than
/// one selector; single selectors and plain values were checked at loading)
/// must not contain a label twice once resolved. LABEL_LIST_DICT is skipped
/// (concatenated dicts keep the last list per key, RuleContext.java:1557-1560).
/// The attribute errors `in A attribute of KIND rule L: Label 'X' is
/// duplicated`, in rule-class attribute order; `rule` is the unconfigured
/// rule and `configured` its configure() result.
pub fn concatenated_duplicate_errors(rule: &Rule, configured: &Rule) -> Vec<String> {
    let shown = |text: &str| {
        Label::parse_canonical(text)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or_else(|_| text.to_owned())
    };
    let label = crate::rule_declarations::canonical_form(&rule.label);
    let mut errors = Vec::new();
    for (source, parts) in configurable_attributes(rule) {
        if parts.len() < 2 {
            continue;
        }
        let (name, resolved) = match &source {
            AttributeSource::Native(attribute) => {
                if attribute.ty != crate::native_rules::NativeType::LabelList {
                    continue;
                }
                let target = match (&configured.native, &configured.custom) {
                    (Some(native), _) => native.as_ref(),
                    (None, Some(custom)) => &custom.declaration.builtin.target,
                    (None, None) => continue,
                };
                (attribute.name, target.attrs.get(attribute.name))
            }
            AttributeSource::Starlark(name, spec) => {
                if spec.kind != crate::custom_rule::AttrKind::LabelList {
                    continue;
                }
                (
                    *name,
                    configured
                        .custom
                        .as_ref()
                        .and_then(|custom| custom.attrs.get(*name)),
                )
            }
        };
        let Some(AttrValue::Labels(labels)) = resolved else {
            continue;
        };
        let mut seen = BTreeSet::new();
        let mut duplicates: Vec<&String> = Vec::new();
        for label in labels {
            if !seen.insert(label) && !duplicates.contains(&label) {
                duplicates.push(label);
            }
        }
        for duplicate in duplicates {
            errors.push(format!(
                "in {name} attribute of {} rule {label}: Label '{}' is duplicated",
                rule.rule_class_name(),
                shown(duplicate)
            ));
        }
    }
    errors
}

/// The `attr` struct values of a Starlark transition attached to `rule`:
/// every attribute of the rule (`rule.getAttributes()`, under its Starlark
/// name `Attribute.getStarlarkName(publicName)`, so `$x`/`:x` are `_x`):
/// `name`, the built-in attributes of the Starlark rule class and the
/// Starlark attributes. For a configured (resolved) rule — an attribute
/// transition (StarlarkAttributeTransitionProvider.java:105-128) — these are
/// its values; for an unconfigured rule — a rule transition
/// (StarlarkRuleTransitionProvider.createTransition, :94-200) — `select()`
/// values are resolved with `resolver` (the pre-transition configuration's
/// conditions) unless one of their keys reads a setting the transition
/// writes (`outputs`, in their given forms: `//command_line_option:NAME`
/// for `values` keys, the canonical label of `flag_values` keys,
/// :176-206) or resolution fails (:160-174): such attributes are omitted.
/// Natively the private built-in attributes have the value `None` except
/// `_config_dependencies` (the rule's select() keys).
pub fn transition_attributes(
    rule: &Rule,
    resolver: Option<&Resolver<'_>>,
    outputs: &[String],
) -> Vec<(String, Option<AttrValue>)> {
    let mut values = vec![(
        "name".to_owned(),
        Some(AttrValue::String(rule.label.name.clone())),
    )];
    let Some(custom) = &rule.custom else {
        return values;
    };
    let reads_outputs =
        |parts: &[ConfigurablePart], resolver: &Resolver<'_>| {
            parts.iter().any(|part| match part {
                ConfigurablePart::Select { branches, .. } => branches.iter().any(|(key, _)| {
                    resolver.conditions.get(key).is_some_and(|condition| {
                        condition.settings.iter().any(|(name, _)| {
                            outputs.contains(&format!("//command_line_option:{name}"))
                        }) || condition.flags.keys().any(|flag| {
                            outputs.contains(&crate::rule_declarations::canonical_form(flag))
                        })
                    })
                }),
                ConfigurablePart::Plain(_) => false,
            })
        };
    // A select() value: resolved, or None when the attribute is omitted.
    let resolve =
        |info: AttributeInfo<'_>, parts: &[ConfigurablePart]| -> Option<Option<AttrValue>> {
            let resolver = resolver?;
            if resolver.conditions.is_empty() || reads_outputs(parts, resolver) {
                return None;
            }
            resolver
                .resolve_attribute(&info, parts)
                .ok()
                .map(|resolved| resolved.value)
        };
    let builtin = &custom.declaration.builtin;
    let target = &builtin.target;
    for (native, attribute) in crate::native_rules::starlark_builtins(builtin.kind).entries() {
        let name = native.trim_start_matches(['$', ':']);
        let starlark_name = if name.len() == native.len() {
            name.to_owned()
        } else {
            format!("_{name}")
        };
        let Some(attribute) = attribute else {
            let value = (native == "$config_dependencies").then(|| {
                AttrValue::Labels(
                    rule_config_dependencies(rule)
                        .iter()
                        .map(|text| {
                            Label::parse_canonical(text)
                                .map(|label| label.to_string())
                                .unwrap_or_else(|_| text.clone())
                        })
                        .collect(),
                )
            });
            values.push((starlark_name, value));
            continue;
        };
        if let Some(parts) = target.configurable.get(native) {
            let info = AttributeSource::Native(attribute.clone());
            if let Some(value) = resolve(info.info(), parts) {
                values.push((starlark_name, value));
            }
            continue;
        }
        let explicit = target.attrs.get(native).cloned();
        let value = match native {
            "tags" => Some(AttrValue::Strings(target.tags.clone())),
            "testonly" => Some(AttrValue::Bool(builtin.testonly)),
            "deprecation" => builtin.deprecation.clone().map(AttrValue::String),
            "visibility" => Some(AttrValue::Labels(
                builtin.visibility.clone().unwrap_or_default(),
            )),
            "generator_name" | "generator_function" | "generator_location"
                if !matches!(explicit, Some(AttrValue::String(_))) =>
            {
                Some(AttrValue::String(
                    builtin
                        .generator
                        .as_ref()
                        .and_then(|generator| match native {
                            "generator_name" => Some(generator.name.clone()),
                            "generator_function" => Some(generator.function.clone()),
                            _ => generator.location.clone(),
                        })
                        .unwrap_or_default(),
                ))
            }
            _ => explicit.or_else(|| crate::native_rules::type_default(attribute.ty)),
        };
        values.push((starlark_name, value));
    }
    for name in &custom.declaration.attr_order {
        let Some(spec) = custom.schema.get(name) else {
            continue;
        };
        match custom.declaration.configurable.get(name) {
            Some(parts) => {
                let info = AttributeSource::Starlark(name, spec);
                if let Some(value) = resolve(info.info(), parts) {
                    values.push((name.clone(), value));
                }
            }
            None => values.push((
                name.clone(),
                Some(
                    custom
                        .attrs
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| spec.default.clone()),
                ),
            )),
        }
    }
    values
}

/// The configured rule: every `select()` resolved by
/// ConfiguredAttributeMapper.validateAttributes (ConfiguredAttributeMapper.java:121-125,
/// called by DependencyResolutionHelpers.visitRule, analysis/DependencyResolutionHelpers.java:309-313)
/// in rule-class attribute order, the first failure winning. Derived
/// dependency lists (`srcs`, genrule `cmd`) are recomputed from resolved
/// values only; no unresolved select remains.
pub fn configure(rule: &Rule, resolver: &Resolver<'_>) -> Result<Rule, String> {
    let mut native_values = Vec::new();
    let mut builtin_values = Vec::new();
    let mut starlark_values = Vec::new();
    for (source, parts) in configurable_attributes(rule) {
        let info = source.info();
        let resolved = resolver.resolve_attribute(&info, parts)?;
        match source {
            AttributeSource::Native(attribute) if rule.native.is_some() => {
                native_values.push((attribute.name, resolved))
            }
            AttributeSource::Native(attribute) => builtin_values.push((attribute.name, resolved)),
            AttributeSource::Starlark(name, spec) => starlark_values.push((
                name.to_owned(),
                resolved.value.unwrap_or_else(|| spec.default.clone()),
            )),
        }
    }
    // A resolved native value is stored only when explicitly specified; an
    // absent value is the attribute default, as for an omitted argument.
    let store = |target: &mut crate::native_rules::NativeTarget, values: Vec<(&str, Resolved)>| {
        target.configurable.clear();
        for (name, resolved) in values {
            if resolved.explicit
                && let Some(value) = resolved.value
            {
                target.attrs.insert(name.to_owned(), value);
            }
        }
    };
    let mut configured = rule.clone();
    if let Some(native) = &rule.native {
        let mut target = (**native).clone();
        store(&mut target, native_values);
        match configured.kind {
            crate::package::RuleKind::Filegroup => configured.srcs = target.labels("srcs"),
            crate::package::RuleKind::Genrule => {
                configured.srcs = target.labels("srcs");
                configured.cmd = target.string("cmd").map(str::to_owned);
            }
            _ => {}
        }
        configured.native = Some(std::sync::Arc::new(target));
    }
    if let Some(custom) = configured.custom.as_mut() {
        store(&mut custom.declaration.builtin.target, builtin_values);
        custom.declaration.configurable.clear();
        for (name, value) in starlark_values {
            custom.attrs.insert(name, value);
        }
        // Late-bound defaults take their configured value (LateBoundDefault
        // resolution in ConfiguredAttributeMapper's configuration).
        crate::late_bound::configure(&custom.schema, &mut custom.attrs);
        configured.srcs = crate::package::custom_dependency_labels(&custom.attrs);
    }
    Ok(configured)
}

/// The configured value of the built-in (native) attribute `name` of
/// `rule`, without configuring its other attributes: its select() resolved
/// as [`configure`] resolves it (a non-explicit result is the unset
/// default, `None`), else its plain value. IncompatibleTargetChecker reads
/// `target_compatible_with` through ConfiguredAttributeMapper.getAndValidate
/// before toolchain and dependency resolution
/// (analysis/constraints/IncompatibleTargetChecker.java:147-170).
pub fn configured_native_value(
    rule: &Rule,
    resolver: &Resolver<'_>,
    name: &str,
) -> Result<Option<AttrValue>, String> {
    for (source, parts) in configurable_attributes(rule) {
        if let AttributeSource::Native(attribute) = &source
            && attribute.name == name
        {
            let resolved = resolver.resolve_attribute(&source.info(), parts)?;
            return Ok(resolved.value.filter(|_| resolved.explicit));
        }
    }
    let target = match (&rule.native, &rule.custom) {
        (Some(native), _) => Some(&**native),
        (None, Some(custom)) => Some(&custom.declaration.builtin.target),
        (None, None) => None,
    };
    Ok(target.and_then(|target| target.attrs.get(name).cloned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn condition(name: &str, settings: &[(&str, &str)], result: MatchResult) -> ConfigMatching {
        ConfigMatching {
            label: label(&format!("//c:{name}")),
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            flags: BTreeMap::new(),
            constraints: BTreeSet::new(),
            result,
        }
    }

    fn strings(values: &[&str]) -> AttrValue {
        AttrValue::Strings(values.iter().map(|v| (*v).to_owned()).collect())
    }

    fn select(branches: &[(&str, Option<AttrValue>)], no_match: &str) -> ConfigurablePart {
        ConfigurablePart::Select {
            branches: branches
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
            no_match_error: no_match.to_owned(),
        }
    }

    fn list_attribute(name: &str) -> AttributeInfo<'_> {
        AttributeInfo {
            name,
            mandatory: false,
            type_default: Some(strings(&[])),
            default: Some(strings(&["dflt"])),
        }
    }

    #[test]
    fn refines_truth_table_matches_config_matching_provider() {
        let base = condition("a", &[("compilation_mode", "opt")], MatchResult::Match);
        let more = condition(
            "b",
            &[("compilation_mode", "opt"), ("stamp", "1")],
            MatchResult::Match,
        );
        let other = condition("c", &[("stamp", "1")], MatchResult::Match);
        let same = condition("d", &[("compilation_mode", "opt")], MatchResult::Match);
        assert!(more.refines(&base));
        assert!(!base.refines(&more));
        assert!(!other.refines(&base) && !base.refines(&other));
        // Equal settings: neither refines (not a proper superset).
        assert!(!same.refines(&base) && !base.refines(&same));
        let mut flagged = base.clone();
        flagged.flags.insert(label("//f:x"), "1".into());
        assert!(flagged.refines(&base) && !base.refines(&flagged));
        let mut constrained = base.clone();
        constrained.constraints.insert(label("//os:linux"));
        assert!(constrained.refines(&base));
        // A larger flag set does not compensate for missing settings.
        let mut flags_only = condition("e", &[], MatchResult::Match);
        flags_only.flags.insert(label("//f:x"), "1".into());
        assert!(!flags_only.refines(&base));
        // Duplicate multimap entries count once (ImmutableSet.copyOf).
        let duplicate = condition(
            "f",
            &[("define", "a=1"), ("define", "a=1")],
            MatchResult::Match,
        );
        let single = condition("g", &[("define", "a=1")], MatchResult::Match);
        assert!(!duplicate.refines(&single));
    }

    #[test]
    fn match_results_combine_with_error_precedence() {
        let diff = |n: &str| Diff {
            what: label(&format!("//command_line_option:{n}")),
            got: "a".into(),
            want: "b".into(),
        };
        assert_eq!(
            MatchResult::Match.combine(MatchResult::NoMatch(vec![diff("x")])),
            MatchResult::NoMatch(vec![diff("x")])
        );
        assert_eq!(
            MatchResult::NoMatch(vec![diff("x")]).combine(MatchResult::NoMatch(vec![diff("y")])),
            MatchResult::NoMatch(vec![diff("x"), diff("y")])
        );
        assert_eq!(
            MatchResult::NoMatch(vec![diff("x")]).combine(MatchResult::InError(vec!["e".into()])),
            MatchResult::InError(vec!["e".into()])
        );
        assert_eq!(
            MatchResult::InError(vec!["a".into()]).combine(MatchResult::InError(vec!["b".into()])),
            MatchResult::InError(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            MatchResult::InError(vec!["a".into()]).combine(MatchResult::Match),
            MatchResult::InError(vec!["a".into()])
        );
    }

    fn resolver<'a>(
        target: &'a Label,
        conditions: &'a BTreeMap<String, ConfigMatching>,
    ) -> Resolver<'a> {
        Resolver {
            target,
            conditions,
            config_hash: "0123456789abcdef".into(),
        }
    }

    #[test]
    fn resolution_specializes_tolerates_duplicates_and_reports_ambiguity() {
        let target = label("//p:t");
        let conditions = BTreeMap::from([
            (
                "//c:opt".to_owned(),
                condition("opt", &[("compilation_mode", "opt")], MatchResult::Match),
            ),
            (
                "//c:opt_stamp".to_owned(),
                condition(
                    "opt_stamp",
                    &[("compilation_mode", "opt"), ("stamp", "1")],
                    MatchResult::Match,
                ),
            ),
            (
                "//c:stamp".to_owned(),
                condition("stamp", &[("stamp", "1")], MatchResult::Match),
            ),
            (
                "//c:dbg".to_owned(),
                condition(
                    "dbg",
                    &[("compilation_mode", "dbg")],
                    MatchResult::NoMatch(vec![]),
                ),
            ),
        ]);
        let resolver = resolver(&target, &conditions);
        let attribute = list_attribute("srcs");
        // Specialization, in either order.
        for parts in [
            select(
                &[
                    ("//c:opt", Some(strings(&["o"]))),
                    ("//c:opt_stamp", Some(strings(&["os"]))),
                ],
                "",
            ),
            select(
                &[
                    ("//c:opt_stamp", Some(strings(&["os"]))),
                    ("//c:opt", Some(strings(&["o"]))),
                ],
                "",
            ),
        ] {
            assert_eq!(
                resolver.resolve_attribute(&attribute, &[parts]).unwrap(),
                Resolved {
                    value: Some(strings(&["os"])),
                    explicit: true
                }
            );
        }
        // Equal values of unrelated matches are tolerated; first wins.
        let same = select(
            &[
                ("//c:opt", Some(strings(&["x"]))),
                ("//c:stamp", Some(strings(&["x"]))),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[same])
                .unwrap()
                .value,
            Some(strings(&["x"]))
        );
        // None (type default []) equals an explicit [] for duplicate matching.
        let none_and_empty = select(&[("//c:opt", None), ("//c:stamp", Some(strings(&[])))], "");
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[none_and_empty])
                .unwrap(),
            Resolved {
                value: Some(strings(&["dflt"])),
                explicit: false
            }
        );
        let ambiguous = select(
            &[
                ("//c:opt", Some(strings(&["a"]))),
                ("//c:stamp", Some(strings(&["b"]))),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[ambiguous])
                .unwrap_err(),
            "Illegal ambiguous match on configurable attribute \"srcs\" in //p:t:\n//c:opt\n//c:stamp\nMultiple matches are not allowed unless one is unambiguously more specialized or they resolve to the same value. See https://bazel.build/reference/be/functions#select."
        );
        // Maps compare as entry sets (Java Map.equals): order is irrelevant.
        let dict = |entries: &[(&str, &str)]| {
            AttrValue::StringDict(
                entries
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        };
        let dict_attribute = AttributeInfo {
            name: "env",
            mandatory: false,
            type_default: Some(dict(&[])),
            default: Some(dict(&[])),
        };
        let reordered = select(
            &[
                ("//c:opt", Some(dict(&[("a", "1"), ("b", "2")]))),
                ("//c:stamp", Some(dict(&[("b", "2"), ("a", "1")]))),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&dict_attribute, &[reordered])
                .unwrap()
                .value,
            Some(dict(&[("a", "1"), ("b", "2")]))
        );
        let reordered_list = select(
            &[
                ("//c:opt", Some(strings(&["a", "b"]))),
                ("//c:stamp", Some(strings(&["b", "a"]))),
            ],
            "",
        );
        assert!(
            resolver
                .resolve_attribute(&attribute, &[reordered_list])
                .is_err()
        );
        // Default chosen when nothing matches.
        let default = select(
            &[
                ("//c:dbg", Some(strings(&["d"]))),
                (DEFAULT_CONDITION, Some(strings(&["z"]))),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[default])
                .unwrap()
                .value,
            Some(strings(&["z"]))
        );
    }

    #[test]
    fn resolution_errors_and_none_branches() {
        let target = label("//p:t");
        let conditions = BTreeMap::from([
            (
                "//c:dbg".to_owned(),
                condition("dbg", &[], MatchResult::NoMatch(vec![])),
            ),
            (
                "//c:broken".to_owned(),
                condition(
                    "broken",
                    &[],
                    MatchResult::InError(vec!["x".into(), "y".into()]),
                ),
            ),
            (
                "//c:opt".to_owned(),
                condition("opt", &[], MatchResult::Match),
            ),
        ]);
        let resolver = resolver(&target, &conditions);
        let attribute = list_attribute("srcs");
        let no_match = select(&[("//c:dbg", Some(strings(&["d"])))], "");
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[no_match])
                .unwrap_err(),
            "configurable attribute \"srcs\" in //p:t doesn't match this configuration. Would a default condition help?\n\nConditions checked:\n //c:dbg\n\nTo see a condition's definition, run: bazel query --output=build <condition label>.\n\nThis instance of //p:t has configuration identifier 0123456. To inspect its configuration, run: bazel config 0123456.\n\nFor more help, see https://bazel.build/docs/configurable-attributes#faq-select-choose-condition.\n\n"
        );
        let custom = select(&[("//c:dbg", Some(strings(&["d"])))], "pick one");
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[custom])
                .unwrap_err(),
            "configurable attribute \"srcs\" in //p:t doesn't match this configuration: pick one\n\nThis instance of //p:t has configuration identifier 0123456. To inspect its configuration, run: bazel config 0123456.\n\nFor more help, see https://bazel.build/docs/configurable-attributes#faq-select-choose-condition.\n\n"
        );
        let broken = select(
            &[
                ("//c:broken", Some(strings(&["b"]))),
                (DEFAULT_CONDITION, None),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, &[broken])
                .unwrap_err(),
            "Unresolvable config_settings for configurable attribute \"srcs\" in //p:t:\nconfig_setting //c:broken is unresolvable because: x, y"
        );
        // A chosen None is the attribute default and not explicit.
        let none = select(
            &[
                ("//c:opt", None),
                (DEFAULT_CONDITION, Some(strings(&["z"]))),
            ],
            "",
        );
        assert_eq!(
            resolver
                .resolve_attribute(&attribute, std::slice::from_ref(&none))
                .unwrap(),
            Resolved {
                value: Some(strings(&["dflt"])),
                explicit: false
            }
        );
        let mandatory = AttributeInfo {
            mandatory: true,
            ..list_attribute("srcs")
        };
        assert_eq!(
            resolver.resolve_attribute(&mandatory, &[none]).unwrap_err(),
            "Mandatory attribute 'srcs' resolved to 'None' after evaluating 'select' expression"
        );
        // Label attributes resolve to null.
        let label_attribute = AttributeInfo {
            name: "dep",
            mandatory: false,
            type_default: None,
            default: None,
        };
        let label_none = select(&[(DEFAULT_CONDITION, None), ("//c:opt", None)], "");
        assert_eq!(
            resolver
                .resolve_attribute(&label_attribute, &[label_none])
                .unwrap(),
            Resolved {
                value: None,
                explicit: false
            }
        );
    }

    #[test]
    fn concatenation_follows_type_concat() {
        let target = label("//p:t");
        let conditions = BTreeMap::from([(
            "//c:opt".to_owned(),
            condition("opt", &[], MatchResult::Match),
        )]);
        let resolver = resolver(&target, &conditions);
        let attribute = list_attribute("srcs");
        let parts = [
            ConfigurablePart::Plain(strings(&["a"])),
            select(
                &[
                    ("//c:opt", Some(strings(&["b"]))),
                    (DEFAULT_CONDITION, Some(strings(&["c"]))),
                ],
                "",
            ),
            select(&[("//c:opt", None)], ""),
        ];
        assert_eq!(
            resolver.resolve_attribute(&attribute, &parts).unwrap(),
            Resolved {
                value: Some(strings(&["a", "b", "dflt"])),
                explicit: true
            }
        );
        assert_eq!(
            concat(vec![
                AttrValue::String("a".into()),
                AttrValue::String("b".into())
            ]),
            Some(AttrValue::String("ab".into()))
        );
        assert_eq!(
            concat(vec![AttrValue::Int(i32::MAX), AttrValue::Int(i32::MAX)]),
            Some(AttrValue::Int(-2))
        );
        assert_eq!(
            concat(vec![
                AttrValue::StringDict(vec![("a".into(), "1".into()), ("b".into(), "2".into())]),
                AttrValue::StringDict(vec![("c".into(), "3".into()), ("a".into(), "4".into())]),
            ]),
            Some(AttrValue::StringDict(vec![
                ("a".into(), "4".into()),
                ("b".into(), "2".into()),
                ("c".into(), "3".into())
            ]))
        );
        assert_eq!(
            concat(vec![AttrValue::Bool(true), AttrValue::Bool(false)]),
            None
        );
    }

    #[test]
    fn unconditional_selects_simplify_to_type_defaults() {
        let default_only = |value: Option<AttrValue>| select(&[(DEFAULT_CONDITION, value)], "");
        let string = || Some(AttrValue::String(String::new()));
        // select({"//conditions:default": None}) is the type default, not the
        // attribute default (BuildType.java:165-211, 722-725).
        assert_eq!(
            simplify_unconditional(vec![default_only(None)], string()),
            Simplified::Plain(AttrValue::String(String::new()))
        );
        assert_eq!(
            simplify_unconditional(vec![default_only(None)], None),
            Simplified::Unset
        );
        assert_eq!(
            simplify_unconditional(
                vec![
                    ConfigurablePart::Plain(strings(&["a"])),
                    default_only(Some(strings(&["b"]))),
                    default_only(None)
                ],
                Some(strings(&[]))
            ),
            Simplified::Plain(strings(&["a", "b"]))
        );
        let conditional = vec![select(&[("//c:opt", None)], "")];
        assert_eq!(
            simplify_unconditional(conditional.clone(), string()),
            Simplified::Retained(conditional)
        );
    }

    #[test]
    fn config_dependency_labels_are_ordered_and_deduplicated() {
        let first = [
            select(
                &[("//c:b", None), (DEFAULT_CONDITION, None), ("//c:a", None)],
                "",
            ),
            ConfigurablePart::Plain(strings(&[])),
            select(&[("//c:a", None), ("//c:c", None)], ""),
        ];
        let second = [select(&[("//c:d", None), ("//c:b", None)], "")];
        assert_eq!(
            config_dependency_labels([&first[..], &second[..]]),
            ["//c:b", "//c:a", "//c:c", "//c:d"]
        );
    }
}
