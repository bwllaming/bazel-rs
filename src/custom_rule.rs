//! Owned custom-rule schemas and trace/freeze-safe Starlark definitions.
// Starlark's fixed attribute signatures also generate module-level helpers.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use crate::build_settings::{BuildSetting, BuildSettingKind};
use crate::providers::ProviderRequirement;

#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub enum AttrKind {
    String,
    StringList,
    StringListDict,
    StringDict,
    Int,
    Bool,
    Label,
    LabelList,
    Output,
    OutputList,
    /// `attr.int_list` (StarlarkAttrModuleApi.java:559).
    IntList,
    /// `attr.label_keyed_string_dict` (StarlarkAttrModuleApi.java:889).
    LabelKeyedStringDict,
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub enum AttrValue {
    String(String),
    Strings(Vec<String>),
    StringListDict(Vec<(String, Vec<String>)>),
    StringDict(Vec<(String, String)>),
    Int(i32),
    Bool(bool),
    Label(Option<String>),
    Labels(Vec<String>),
    Output(Option<String>),
    Outputs(Vec<String>),
    Ints(Vec<i32>),
    /// Canonical label keys with their string values, in insertion order.
    LabelKeyedStringDict(Vec<(String, String)>),
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AttrSpec {
    pub kind: AttrKind,
    pub default: AttrValue,
    pub mandatory: bool,
    pub allow_files: bool,
    pub allow_single_file: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub non_empty: bool,
    #[allow(private_interfaces)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_providers: Vec<Vec<ProviderRequirement>>,
    /// Load-time declaration metadata (docs/LOAD_DECLARATIONS.md); empty
    /// metadata preserves the previous schema serialization.
    #[serde(skip_serializing_if = "AttrDeclaration::is_default")]
    pub declaration: AttrDeclaration,
}
fn is_false(value: &bool) -> bool {
    !*value
}

/// One `attr.string/int(values=[...])` entry (Attribute.AllowedValueSet,
/// packages/Attribute.java:210-243). `value` is `None` when the entry has a
/// different type and therefore can never match.
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AllowedValue {
    pub value: Option<AttrValue>,
    pub display: String,
}

/// Retained Bazel attribute options that the native analysis does not model.
/// See StarlarkAttrModule.createAttribute (StarlarkAttrModule.java:317-513).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AttrDeclaration {
    /// `allow_files`/`allow_single_file` given as a list of file extensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "crate::rule_declarations::Cfg::is_target")]
    pub cfg: crate::rule_declarations::Cfg,
    #[serde(skip_serializing_if = "is_false")]
    pub executable: bool,
    /// Exported aspect identities (`<extension>%<name>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aspects: Vec<String>,
    /// `default = configuration_field(...)` (StarlarkLateBoundDefault).
    #[serde(skip_serializing_if = "is_false")]
    pub late_bound: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_rules: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<AllowedValue>,
    #[serde(skip_serializing_if = "is_false")]
    pub skip_validations: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_dependency_resolution: Option<bool>,
    /// Whether `default` was supplied (Descriptor.hasDefault, used by aspect()).
    #[serde(skip)]
    pub explicit_default: bool,
}

impl AttrDeclaration {
    pub(crate) fn is_default(&self) -> bool {
        Self {
            explicit_default: false,
            ..self.clone()
        } == Self::default()
    }

    /// AllowedValueSet.apply; an empty set allows everything.
    pub(crate) fn allows(&self, value: &AttrValue) -> bool {
        self.values.is_empty()
            || self
                .values
                .iter()
                .any(|allowed| allowed.value.as_ref() == Some(value))
    }

    /// AllowedValueSet.getErrorReason with StringUtil.joinEnglishListSingleQuoted
    /// (util/StringUtil.java:54-88).
    pub(crate) fn allowed_values_reason(&self, value: &AttrValue) -> String {
        let choices = self
            .values
            .iter()
            .map(|allowed| allowed.display.as_str())
            .collect::<Vec<_>>();
        let mut joined = String::new();
        for (index, choice) in choices.iter().enumerate() {
            if index > 0 {
                let last = index + 1 == choices.len();
                if !last || choices.len() > 2 {
                    joined.push(',');
                }
                if last {
                    joined.push_str(" or");
                }
                joined.push(' ');
            }
            joined.push('\'');
            joined.push_str(choice);
            joined.push('\'');
        }
        if joined.is_empty() {
            joined.push_str("nothing");
        }
        let shown = match value {
            AttrValue::String(text) => text.clone(),
            AttrValue::Int(number) => number.to_string(),
            other => format!("{other:?}"),
        };
        format!("has to be one of {joined} instead of '{shown}'")
    }

    /// Features a target exercises at analysis when this attribute has a value.
    pub(crate) fn unanalyzed(&self, name: &str) -> Vec<String> {
        let mut features = Vec::new();
        if !self.cfg.is_target() {
            features.push(format!(
                "attribute '{name}' cfg={} (configuration transition)",
                self.cfg.describe()
            ));
        }
        if self.executable {
            features.push(format!("attribute '{name}' executable=True"));
        }
        if !self.aspects.is_empty() {
            features.push(format!(
                "attribute '{name}' aspects=[{}] (aspect propagation)",
                self.aspects.join(", ")
            ));
        }
        if self.late_bound {
            features.push(format!(
                "attribute '{name}' late-bound default (configuration_field)"
            ));
        }
        if let Some(types) = &self.file_types {
            features.push(format!(
                "attribute '{name}' allowed file types [{}]",
                types.join(", ")
            ));
        }
        if let Some(rules) = &self.allow_rules {
            features.push(format!(
                "attribute '{name}' allow_rules=[{}]",
                rules.join(", ")
            ));
        }
        if !self.flags.is_empty() {
            features.push(format!(
                "attribute '{name}' flags=[{}]",
                self.flags.join(", ")
            ));
        }
        if self.skip_validations {
            features.push(format!("attribute '{name}' skip_validations=True"));
        }
        features
    }
}

/// An element of a `select()`-valued BUILD attribute (BuildType.SelectorList).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ConfigurablePart {
    Plain(AttrValue),
    /// Condition label (or `//conditions:default`) and branch value (`None` = default).
    Select {
        branches: Vec<(String, Option<AttrValue>)>,
        no_match_error: String,
    },
}

/// Load-time invocation metadata beyond ordinary attribute values.
#[derive(Clone, Debug, Default)]
pub struct InvocationDeclaration {
    /// Implicit outputs from `rule(outputs={...})`, already in `Rule::outs`.
    pub implicit_outputs: Vec<(String, String)>,
    /// Exercised features that native analysis does not implement; analysis
    /// fails explicitly when nonempty.
    pub unanalyzed: Vec<String>,
    /// `rule(provides=[...])` identities checked against returned providers.
    #[allow(private_interfaces)]
    pub provides: Vec<ProviderRequirement>,
    /// Retained `select()` attribute values.
    pub configurable: BTreeMap<String, Vec<ConfigurablePart>>,
    /// Rule class name for Bazel-form diagnostics (the exported rule name).
    pub rule_class: String,
}
#[derive(Clone, Debug)]
pub struct CustomInvocation {
    pub build_setting: Option<BuildSetting>,
    pub implementation: OwnedFrozenValue,
    /// Strong frozen roots for direct declarations, including factory closures.
    pub subrules: Vec<OwnedFrozenValue>,
    pub top_level_functions: Vec<OwnedFrozenValue>,
    pub attrs: BTreeMap<String, AttrValue>,
    pub schema: BTreeMap<String, AttrSpec>,
    pub declaration: InvocationDeclaration,
}

/// Attribute sequences accept only lists and tuples, without copying their elements.
pub(crate) fn sequence_ref<'v>(value: Value<'v>) -> Option<&'v [Value<'v>]> {
    ListRef::from_value(value)
        .map(ListRef::content)
        .or_else(|| TupleRef::from_value(value).map(TupleRef::content))
}

fn string_list(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    sequence_ref(value)
        .ok_or_else(|| anyhow!("expected a list of strings"))?
        .iter()
        .map(|v| {
            v.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a list of strings"))
        })
        .collect()
}

/// Normalize OR-of-AND requirements without copying an unbounded input sequence.
/// Every constructor is validated, even after an empty alternative was seen.
fn required_providers(value: Value<'_>) -> anyhow::Result<Vec<Vec<ProviderRequirement>>> {
    let outer = sequence_ref(value)
        .ok_or_else(|| anyhow!("providers must be a list or tuple of Provider constructors"))?;
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    if outer.is_empty() {
        return Ok(Vec::new());
    }
    let nested = sequence_ref(outer[0]).is_some();
    let mut clauses = Vec::new();
    let mut unrestricted = false;
    let mut clause = |items: &[Value<'_>]| -> anyhow::Result<Vec<ProviderRequirement>> {
        budget.node().map_err(anyhow::Error::msg)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut requirements = Vec::new();
        for &item in items {
            budget.node().map_err(anyhow::Error::msg)?;
            // requirement_info bounds its transient owned result before cloning;
            // this cumulative budget charges it before either retained insertion.
            let requirement = crate::providers::requirement_info(item)
                .map_err(|error| anyhow!("providers: {error}"))?;
            budget.string(&requirement.id).map_err(anyhow::Error::msg)?;
            budget
                .string(&requirement.display_name)
                .map_err(anyhow::Error::msg)?;
            if seen.insert(requirement.id.clone()) {
                requirements.push(requirement);
            }
        }
        Ok(requirements)
    };
    if nested {
        for &item in outer {
            let items = sequence_ref(item).ok_or_else(|| {
                anyhow!("providers must not mix Provider constructors and sequences")
            })?;
            let requirements = clause(items)?;
            unrestricted |= requirements.is_empty();
            clauses.push(requirements);
        }
    } else {
        clauses.push(clause(outer)?);
    }
    if unrestricted {
        clauses.clear();
    }
    Ok(clauses)
}

fn with_required_providers(
    mut descriptor: Descriptor,
    value: Value<'_>,
) -> anyhow::Result<Descriptor> {
    descriptor.0.required_providers = required_providers(value)?;
    // Defaults and provider metadata must fit together, not just independently.
    crate::repository_budget::HostBudget::default()
        .spec(&descriptor.0)
        .map_err(anyhow::Error::msg)?;
    Ok(descriptor)
}

/// Attribute defaults use the innermost executing extension's lexical package.
/// A loaded helper can execute while another extension is being initialized,
/// so its canonical call-site filename takes precedence over the phase context.
/// This mirrors Bazel's LabelConverter.forBzlEvaluatingThread semantics.
fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<crate::label::Label> {
    eval.extra?
        .downcast_ref::<crate::loading::ExtensionContext>()?;
    crate::loading::definition_label(eval)
}

type LabelResolver<'a> = dyn Fn(&str) -> anyhow::Result<crate::label::Label> + 'a;

fn label_value(value: Value<'_>, resolver: Option<&LabelResolver<'_>>) -> anyhow::Result<String> {
    if let Some(label) = crate::starlark_types::unpack_label(value) {
        return Ok(label);
    }
    let text = value
        .unpack_str()
        .ok_or_else(|| anyhow!("expected a string or Label"))?;
    if let Some(resolve) = resolver {
        return resolve(text).map(|label| label.to_string());
    }
    if !text.starts_with("@@")
        && !text.starts_with("//")
        && !text.starts_with("@//")
        && !text.starts_with("@@//")
    {
        bail!("relative label default requires an extension definition context");
    }
    crate::label::Label::parse_canonical(text.strip_prefix("@//").map_or(text, |_| &text[1..]))
        .map(|label| label.to_string())
        .map_err(anyhow::Error::msg)
}

impl AttrKind {
    /// The `attr` module function name (Descriptor repr `<attr.NAME>`).
    pub(crate) fn starlark_name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::StringList => "string_list",
            Self::StringListDict => "string_list_dict",
            Self::StringDict => "string_dict",
            Self::Int => "int",
            Self::Bool => "bool",
            Self::Label => "label",
            Self::LabelList => "label_list",
            Self::Output => "output",
            Self::OutputList => "output_list",
            Self::IntList => "int_list",
            Self::LabelKeyedStringDict => "label_keyed_string_dict",
        }
    }

    /// Bazel `Type.toString` (packages/Type.java, BuildType.java).
    fn type_name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::StringList => "list(string)",
            Self::StringListDict => "dict(string, list(string))",
            Self::StringDict => "dict(string, string)",
            Self::Int => "int",
            Self::Bool => "boolean",
            Self::Label => "label",
            Self::LabelList => "list(label)",
            Self::Output => "output",
            Self::OutputList => "list(output)",
            Self::IntList => "list(int)",
            Self::LabelKeyedStringDict => "dict(label, string)",
        }
    }

    /// Bazel `Type.convert` type failure in ConversionException form
    /// (packages/Type.java:296-321): `expected value of type 'T' for WHAT, but
    /// got REPR (TYPE)`; list elements use `element I of WHAT` (Type.java:689)
    /// and dict entries `dict key element` / `dict value element` (:576-577).
    /// Label-typed values that are not strings report type 'string'
    /// (BuildType.LabelType.convert). Shape only; label syntax is checked later.
    pub(crate) fn conversion_error(&self, value: Value<'_>, what: &str) -> Option<String> {
        fn expected(ty: &str, value: Value<'_>, what: &str) -> String {
            format!(
                "expected value of type '{ty}' for {what}, but got {} ({})",
                value.to_repr(),
                value.get_type()
            )
        }
        let string = |value: Value<'_>, what: &str| {
            value
                .unpack_str()
                .is_none()
                .then(|| expected("string", value, what))
        };
        let label = |value: Value<'_>, what: &str| {
            (value.unpack_str().is_none() && crate::starlark_types::label_parts(value).is_none())
                .then(|| expected("string", value, what))
        };
        let int = |value: Value<'_>, what: &str| {
            value
                .unpack_i32()
                .is_none()
                .then(|| expected("int", value, what))
        };
        let list = |value: Value<'_>, element: &dyn Fn(Value<'_>, &str) -> Option<String>| {
            let Some(items) = sequence_ref(value) else {
                return Some(expected(self.type_name(), value, what));
            };
            items
                .iter()
                .enumerate()
                .find_map(|(index, item)| element(*item, &format!("element {index} of {what}")))
        };
        let dict = |key: &dyn Fn(Value<'_>, &str) -> Option<String>,
                    item: &dyn Fn(Value<'_>, &str) -> Option<String>| {
            let Some(dict) = DictRef::from_value(value) else {
                return Some(expected(self.type_name(), value, what));
            };
            dict.iter().find_map(|(k, v)| {
                key(k, "dict key element").or_else(|| item(v, "dict value element"))
            })
        };
        match self {
            Self::String => string(value, what),
            Self::Int => int(value, what),
            Self::Bool => value
                .unpack_bool()
                .is_none()
                .then(|| expected("boolean", value, what)),
            Self::Label => label(value, what),
            Self::Output => string(value, what),
            Self::StringList | Self::OutputList => list(value, &string),
            Self::LabelList => list(value, &label),
            Self::IntList => list(value, &int),
            Self::StringDict => dict(&string, &string),
            Self::LabelKeyedStringDict => dict(&label, &string),
            Self::StringListDict => dict(&string, &|value, what| {
                let Some(items) = sequence_ref(value) else {
                    return Some(expected("list(string)", value, what));
                };
                items
                    .iter()
                    .enumerate()
                    .find_map(|(index, item)| string(*item, &format!("element {index} of {what}")))
            }),
        }
    }

    /// Inspect borrowed defaults before parse copies shared Starlark payloads.
    /// Validate the complete type shape first, preserving ordinary type errors.
    /// This raw-copy phase and lexical normalization each have a cumulative
    /// per-descriptor budget; neither relies on later schema-wrapper checks.
    fn preflight_default(&self, value: Value<'_>) -> anyhow::Result<()> {
        self.preflight_value_with_budget(
            value,
            &mut crate::repository_budget::HostBudget::default(),
        )
    }

    pub(crate) fn preflight_value_with_budget(
        &self,
        value: Value<'_>,
        budget: &mut crate::repository_budget::HostBudget,
    ) -> anyhow::Result<()> {
        let is_label = |value: Value<'_>| {
            value.unpack_str().is_some() || crate::starlark_types::label_parts(value).is_some()
        };
        if matches!(self, Self::Label) && crate::language_modules::is_late_bound_default(value) {
            bail!(
                "late-bound label defaults (configuration_field) are {} (analysis-time configuration API)",
                crate::language_modules::NOT_IMPLEMENTED.trim_start_matches("is ")
            );
        }
        let expected = match self {
            Self::String if value.unpack_str().is_none() => Some("a string"),
            Self::StringList | Self::OutputList
                if !sequence_ref(value)
                    .is_some_and(|list| list.iter().all(|v| v.unpack_str().is_some())) =>
            {
                Some("a list of strings")
            }
            Self::StringListDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter().all(|(key, values)| {
                        key.unpack_str().is_some()
                            && sequence_ref(values).is_some_and(|values| {
                                values.iter().all(|value| value.unpack_str().is_some())
                            })
                    })
                }) =>
            {
                Some("a dict of strings to a list of strings (or tuple of strings)")
            }
            Self::StringDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter().all(|(key, value)| {
                        key.unpack_str().is_some() && value.unpack_str().is_some()
                    })
                }) =>
            {
                Some("a dict of strings to strings")
            }
            Self::Int if value.unpack_i32().is_none() => Some("a 32-bit int"),
            Self::IntList
                if !sequence_ref(value)
                    .is_some_and(|list| list.iter().all(|v| v.unpack_i32().is_some())) =>
            {
                Some("a list of 32-bit ints")
            }
            Self::LabelKeyedStringDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter()
                        .all(|(key, value)| is_label(key) && value.unpack_str().is_some())
                }) =>
            {
                Some("a dict of strings or Labels to strings")
            }
            Self::Bool if value.unpack_bool().is_none() => Some("a bool"),
            Self::Label if !value.is_none() && !is_label(value) => Some("a string or Label"),
            Self::LabelList
                if !sequence_ref(value).is_some_and(|list| list.iter().copied().all(is_label)) =>
            {
                Some("a list of strings or Labels")
            }
            Self::Output if !value.is_none() && value.unpack_str().is_none() => Some("a string"),
            _ => None,
        };
        if let Some(expected) = expected {
            bail!("expected {expected}");
        }
        let mut charge = |value: Value<'_>| -> Result<(), String> {
            budget.node()?;
            if let Some(text) = value.unpack_str() {
                let bytes = if matches!(
                    self,
                    Self::Label | Self::LabelList | Self::LabelKeyedStringDict
                ) {
                    // Covers canonical parsing/formatting even without a resolver.
                    crate::label::Label::resolution_size_bound(text, "", "", &BTreeMap::new())
                } else {
                    text.len()
                };
                budget.bytes(bytes)?;
            } else if let Some((repository, package, name)) =
                crate::starlark_types::label_parts(value)
            {
                budget.bytes(
                    repository
                        .len()
                        .saturating_add(package.len())
                        .saturating_add(name.len())
                        .saturating_add(5),
                )?;
            }
            Ok(())
        };
        charge(value).map_err(anyhow::Error::msg)?;
        if let Some(list) = sequence_ref(value) {
            for &value in list {
                charge(value).map_err(anyhow::Error::msg)?;
            }
        }
        if let Some(dict) = DictRef::from_value(value) {
            for (key, values) in dict.iter() {
                charge(key).map_err(anyhow::Error::msg)?;
                charge(values).map_err(anyhow::Error::msg)?;
                // The complete shape was checked above, before any budget errors.
                if matches!(self, Self::StringListDict) {
                    for &value in sequence_ref(values).expect("validated attribute sequence") {
                        charge(value).map_err(anyhow::Error::msg)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn empty(&self) -> AttrValue {
        match self {
            Self::String => AttrValue::String(String::new()),
            Self::StringList => AttrValue::Strings(Vec::new()),
            Self::StringListDict => AttrValue::StringListDict(Vec::new()),
            Self::StringDict => AttrValue::StringDict(Vec::new()),
            Self::Int => AttrValue::Int(0),
            Self::Bool => AttrValue::Bool(false),
            Self::Label => AttrValue::Label(None),
            Self::LabelList => AttrValue::Labels(Vec::new()),
            Self::Output => AttrValue::Output(None),
            Self::OutputList => AttrValue::Outputs(Vec::new()),
            Self::IntList => AttrValue::Ints(Vec::new()),
            Self::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
        }
    }
    fn parse(
        &self,
        value: Value<'_>,
        resolver: Option<&LabelResolver<'_>>,
    ) -> anyhow::Result<AttrValue> {
        let string = || {
            value
                .unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a string"))
        };
        Ok(match self {
            Self::String => AttrValue::String(string()?),
            Self::StringList => AttrValue::Strings(string_list(value)?),
            Self::StringDict => AttrValue::StringDict(
                DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings to strings"))?
                    .iter()
                    .map(|(key, value)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        let value = value
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict values to be strings"))?;
                        Ok((key.to_owned(), value.to_owned()))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::StringListDict => AttrValue::StringListDict(
                DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings to a list of strings"))?
                    .iter()
                    .map(|(key, values)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        Ok((key.to_owned(), string_list(values)?))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Int => AttrValue::Int(
                value
                    .unpack_i32()
                    .ok_or_else(|| anyhow!("expected a 32-bit int"))?,
            ),
            Self::Bool => AttrValue::Bool(
                value
                    .unpack_bool()
                    .ok_or_else(|| anyhow!("expected a bool"))?,
            ),
            Self::Label => AttrValue::Label(if value.is_none() {
                None
            } else {
                Some(label_value(value, resolver)?)
            }),
            Self::LabelList => AttrValue::Labels(
                sequence_ref(value)
                    .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                    .iter()
                    .copied()
                    .map(|value| {
                        label_value(value, resolver)
                            .map_err(|e| anyhow!("expected a list of strings or Labels: {e}"))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Output => AttrValue::Output(if value.is_none() {
                None
            } else {
                Some(string()?)
            }),
            Self::OutputList => AttrValue::Outputs(string_list(value)?),
            Self::IntList => AttrValue::Ints(
                sequence_ref(value)
                    .ok_or_else(|| anyhow!("expected a list of 32-bit ints"))?
                    .iter()
                    .map(|value| {
                        value
                            .unpack_i32()
                            .ok_or_else(|| anyhow!("expected a list of 32-bit ints"))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::LabelKeyedStringDict => {
                let mut entries: Vec<(String, String)> = Vec::new();
                for (key, value) in DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings or Labels to strings"))?
                    .iter()
                {
                    let key = label_value(key, resolver)?;
                    let value = value
                        .unpack_str()
                        .ok_or_else(|| anyhow!("expected dict values to be strings"))?;
                    // BuildType.LabelKeyedDictType rejects keys that resolve equally.
                    if entries.iter().any(|(seen, _)| *seen == key) {
                        bail!("duplicate key {key} in label-keyed dict");
                    }
                    entries.push((key, value.to_owned()));
                }
                AttrValue::LabelKeyedStringDict(entries)
            }
        })
    }

    pub(crate) fn is_empty_value(value: &AttrValue) -> bool {
        match value {
            AttrValue::String(_) | AttrValue::Int(_) | AttrValue::Bool(_) => false,
            AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                values.is_empty()
            }
            AttrValue::StringListDict(entries) => entries.is_empty(),
            AttrValue::StringDict(entries) | AttrValue::LabelKeyedStringDict(entries) => {
                entries.is_empty()
            }
            AttrValue::Label(value) | AttrValue::Output(value) => value.is_none(),
            AttrValue::Ints(values) => values.is_empty(),
        }
    }
}

// Derives generate marker/coercion/trace implementations. Only implementation
// and the retained callbacks contain heap references; the ignored schema and
// declaration consist solely of owned data.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };
    use starlark::{starlark_complex_value, starlark_simple_value};
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Descriptor(pub AttrSpec);
    impl fmt::Display for Descriptor {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Descriptor.repr (StarlarkAttrModule.java:1218): <attr.KIND>.
            write!(f, "<attr.{}>", self.0.kind.starlark_name())
        }
    }
    starlark_simple_value!(Descriptor);
    // StarlarkAttrModuleApi.Descriptor: @StarlarkBuiltin(name = "Attribute").
    #[starlark_value(type = "Attribute")]
    impl<'v> StarlarkValue<'v> for Descriptor {}

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct DefinitionGen<V> {
        pub implementation: V,
        pub subrules: Vec<V>,
        /// `rule(initializer=...)`: zero or one Starlark function.
        pub initializer: Vec<V>,
        /// `rule(outputs=<function>)`: zero or one retained, never-called function.
        pub outputs_function: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub build_setting: Option<BuildSetting>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub schema: BTreeMap<String, AttrSpec>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub declaration: RuleDeclaration,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
    }
    impl<V> fmt::Display for DefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // StarlarkRuleFunction.repr (StarlarkRuleClassFunctions.java:1999-2005).
            match self.exported.get() {
                Some(name) => write!(f, "<rule {name}>"),
                None => f.write_str("<rule>"),
            }
        }
    }
    starlark_complex_value!(pub(super) Definition);
    #[starlark_value(type = "rule")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn export_as(&self, name: &str, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
            if self.exported.get().is_none() {
                // StarlarkRuleFunction.export (StarlarkRuleClassFunctions.java:1931-1937),
                // TargetUtils.isTestRuleName (packages/TargetUtils.java:67).
                if self.declaration.test != name.ends_with("_test") {
                    return Err(starlark::Error::new_other(anyhow!(
                        "Invalid rule class name '{name}', test rule class names must end with '_test' and other rule classes must not"
                    )));
                }
                let _ = self.exported.set(name.to_owned());
            }
            Ok(())
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            args.no_positional_args(eval.heap())?;
            let named = args.names_map()?;
            let (repositories, package) =
                crate::package::custom_label_context(eval).map_err(starlark::Error::new_other)?;
            // One budget covers the retained schema and every supplied/default value.
            // Charge the schema before register_custom clones it, including defaults
            // that will be overridden by explicit BUILD arguments.
            let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
            budget
                .borrow_mut()
                .schema(&self.schema)
                .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
            crate::subrule::preflight(
                self.subrules.iter().map(|value| value.to_value()),
                &mut budget.borrow_mut(),
            )
            .map_err(starlark::Error::new_other)?;
            let resolve_in = |text: &str, repository: &str, package: &str| {
                let bytes = repositories
                    .resolution_size_bound(text, repository, package)
                    .map_err(anyhow::Error::msg)?;
                budget
                    .borrow_mut()
                    .bytes(bytes)
                    .map_err(anyhow::Error::msg)?;
                repositories
                    .parse(text, repository, package)
                    .map_err(anyhow::Error::msg)
            };
            let resolve = |text: &str| resolve_in(text, &package.repository, &package.package);
            let definition =
                crate::label::Label::parse_canonical(&self.declaration.definition_label)
                    .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
            // Initializer results are reified relative to the rule definition
            // (StarlarkRuleClassFunctions.java:1871-1880), except outputs.
            let resolve_definition =
                |text: &str| resolve_in(text, &definition.repository, &definition.package);
            let mut arguments = named
                .iter()
                .map(|(key, value)| (key.as_str().to_owned(), *value, Origin::Call))
                .collect::<Vec<_>>();
            if let Some(initializer) = self.initializer.first() {
                arguments = run_initializer(
                    initializer.to_value(),
                    arguments,
                    &self.schema,
                    &self.declaration,
                    &definition,
                    &resolve,
                    eval,
                )
                .map_err(starlark::Error::new_other)?;
            }
            let mut name = None;
            let mut visibility = None;
            let mut attrs = BTreeMap::new();
            let mut configurable = BTreeMap::new();
            let rule_class = self
                .exported
                .get()
                .cloned()
                .unwrap_or_else(|| "unexported rule".to_owned());
            // Package-loading errors are prefixed by the target label
            // (RuleClass.populateRuleAttributeValues reportError).
            let target_text = arguments
                .iter()
                .find(|(key, _, _)| key == "name")
                .and_then(|(_, value, _)| value.unpack_str())
                .map(|name| {
                    crate::rule_declarations::canonical_form(&crate::label::Label {
                        repository: package.repository.clone(),
                        package: package.package.clone(),
                        name: name.to_owned(),
                    })
                })
                .unwrap_or_default();
            for (key, value, origin) in &arguments {
                let key = key.as_str();
                match key {
                    "name" => {
                        name = Some(value.unpack_str().ok_or_else(|| {
                            starlark::Error::new_other(anyhow!("name must be a string"))
                        })?)
                    }
                    "visibility" => visibility = Some(*value),
                    _ => {
                        if key.starts_with('_') && *origin != Origin::Initializer {
                            return Err(starlark::Error::new_other(anyhow!(
                                "private attribute {key:?} cannot be set explicitly"
                            )));
                        }
                        let spec = self.schema.get(key).ok_or_else(|| {
                            starlark::Error::new_other(anyhow!("unknown attribute {key:?}"))
                        })?;
                        // Bazel treats an explicit None like an omitted value:
                        // defaults and mandatory checks still apply.
                        if value.is_none() {
                            continue;
                        }
                        let resolver: &LabelResolver<'_> = if *origin == Origin::Initializer
                            && !matches!(spec.kind, AttrKind::Output | AttrKind::OutputList)
                        {
                            &resolve_definition
                        } else {
                            &resolve
                        };
                        if let Some(pieces) = crate::rule_declarations::select_pieces(*value) {
                            // build_setting_default and help are nonconfigurable
                            // (RuleClass.java:904-914).
                            let nonconfigurable = self.build_setting.is_some()
                                && matches!(key, "build_setting_default" | "help");
                            let parts = configurable_parts(
                                key,
                                spec,
                                nonconfigurable,
                                &rule_class,
                                pieces,
                                resolver,
                                &budget,
                            )
                            .map_err(|e| {
                                starlark::Error::new_other(anyhow!("{target_text}: {e}"))
                            })?;
                            configurable.insert(key.to_owned(), parts);
                            continue;
                        }
                        // Bazel's build-setting boolean conversion accepts 0/1;
                        // ordinary attr.bool attributes remain strictly boolean.
                        let parsed = (|| {
                            budget
                                .borrow_mut()
                                .string(key)
                                .map_err(anyhow::Error::msg)?;
                            if key == "build_setting_default"
                                && self
                                    .build_setting
                                    .is_some_and(|s| s.kind == BuildSettingKind::Bool)
                                && matches!(value.unpack_i32(), Some(0 | 1))
                            {
                                budget.borrow_mut().node().map_err(anyhow::Error::msg)?;
                                Ok(AttrValue::Bool(value.unpack_i32() == Some(1)))
                            } else {
                                spec.kind.preflight_value_with_budget(
                                    *value,
                                    &mut budget.borrow_mut(),
                                )?;
                                spec.kind.parse(*value, Some(resolver))
                            }
                        })()
                        .map_err(|e| {
                            starlark::Error::new_other(anyhow!("attribute {key:?}: {e}"))
                        })?;
                        attrs.insert(key.to_owned(), parsed);
                    }
                }
            }
            let name = name.ok_or_else(|| {
                starlark::Error::new_other(anyhow!("missing required attribute name"))
            })?;
            let target = crate::label::Label {
                repository: package.repository.clone(),
                package: package.package.clone(),
                name: name.to_owned(),
            };
            // Only explicitly supplied values (call or initializer) and select()
            // branches are checked; defaults never are (AttributeProvider.java:288-290).
            check_allowed_values(&target, &self.schema, &attrs, &configurable)
                .map_err(starlark::Error::new_other)?;
            for (key, spec) in &self.schema {
                if !attrs.contains_key(key) {
                    if spec.mandatory && !configurable.contains_key(key) {
                        return Err(starlark::Error::new_other(anyhow!(
                            "missing mandatory attribute {key:?}"
                        )));
                    }
                    {
                        let mut budget = budget.borrow_mut();
                        budget
                            .string(key)
                            .and_then(|()| budget.attr_value(&spec.default))
                            .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
                    }
                    attrs.insert(key.clone(), spec.default.clone());
                }
            }
            let implicit_outputs = implicit_outputs(&self.declaration.implicit_outputs, name)
                .map_err(starlark::Error::new_other)?;
            let unanalyzed = unanalyzed(&self.declaration, &self.schema, &attrs, &configurable);
            crate::package::register_custom(
                eval,
                name,
                visibility,
                self.implementation.to_value(),
                self.subrules.iter().map(|value| value.to_value()).collect(),
                attrs,
                self.schema.clone(),
                self.build_setting,
                InvocationDeclaration {
                    implicit_outputs,
                    unanalyzed,
                    provides: self.declaration.provides.clone(),
                    configurable,
                    rule_class,
                },
            )
            .map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }
}
use values::{Definition, DefinitionGen, Descriptor};

/// Owned `rule()` metadata (StarlarkRuleClassFunctions.createRule, :717-1171).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) struct RuleDeclaration {
    /// Canonical `.bzl` label whose evaluation called rule().
    pub definition_label: String,
    pub executable: bool,
    /// `test=True` or `analysis_test=True` (createRule: "analysis_test=true implies test=true").
    pub test: bool,
    pub analysis_test: bool,
    pub skylark_testable: bool,
    pub output_to_genfiles: bool,
    pub dependency_resolution_rule: bool,
    pub fragments: Vec<String>,
    pub host_fragments: Vec<String>,
    pub toolchains: Vec<(String, bool)>,
    pub exec_groups: Vec<String>,
    pub exec_compatible_with: Vec<String>,
    pub cfg: crate::rule_declarations::Cfg,
    pub provides: Vec<ProviderRequirement>,
    pub implicit_outputs: ImplicitOutputs,
    /// `extendable=` as given: "True"/"False" or a canonical allowlist label.
    pub extendable: Option<String>,
}

/// `rule(outputs=...)` (StarlarkRuleClassFunctions.java:1102-1122).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) enum ImplicitOutputs {
    #[default]
    None,
    Templates(Vec<(String, String)>),
    Function,
}

impl RuleDeclaration {
    /// Rule-level features any analysis of a target of this rule exercises.
    fn unanalyzed(&self) -> Vec<String> {
        let mut features = Vec::new();
        if self.analysis_test {
            features.push("rule(analysis_test=True)".to_owned());
        } else if self.test {
            features.push("rule(test=True) (test rule semantics)".to_owned());
        }
        if self.executable {
            features.push("rule(executable=True) (executable rule semantics)".to_owned());
        }
        if self.skylark_testable {
            features.push("rule(_skylark_testable=True)".to_owned());
        }
        if self.output_to_genfiles {
            features.push("rule(output_to_genfiles=True)".to_owned());
        }
        if self.dependency_resolution_rule {
            features.push("rule(dependency_resolution_rule=True)".to_owned());
        }
        if !self.toolchains.is_empty() {
            features.push(format!(
                "rule(toolchains=[{}]) (toolchain resolution)",
                self.toolchains
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.exec_groups.is_empty() {
            features.push(format!(
                "rule(exec_groups=[{}]) (execution platform resolution)",
                self.exec_groups.join(", ")
            ));
        }
        if !self.exec_compatible_with.is_empty() {
            features.push(format!(
                "rule(exec_compatible_with=[{}]) (execution platform resolution)",
                self.exec_compatible_with.join(", ")
            ));
        }
        if !self.cfg.is_target() {
            features.push(format!(
                "rule(cfg={}) (configuration transition)",
                self.cfg.describe()
            ));
        }
        if let ImplicitOutputs::Templates(templates) = &self.implicit_outputs
            && !templates.is_empty()
        {
            features.push("rule(outputs={...}) implicit outputs in ctx.outputs".to_owned());
        }
        features
    }
}

/// Argument provenance during rule instantiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Call,
    Initializer,
}

/// Attributes Bazel defines natively (never Starlark-defined); the initializer
/// may not set them (StarlarkRuleClassFunctions.java:1866-1869).
fn native_attribute(name: &str, test: bool) -> bool {
    matches!(
        name,
        "visibility"
            | "tags"
            | "testonly"
            | "deprecation"
            | "features"
            | "compatible_with"
            | "restricted_to"
            | "target_compatible_with"
            | "exec_compatible_with"
            | "exec_group_compatible_with"
            | "exec_properties"
            | "package_metadata"
            | "toolchains"
            | "licenses"
            | "distribs"
            | "applicable_licenses"
            | "aspect_hints"
            | "transitive_configs"
            | "generator_name"
            | "generator_function"
            | "generator_location"
    ) || (test
        && matches!(
            name,
            "size" | "timeout" | "flaky" | "shard_count" | "local" | "args" | "env" | "env_inherit"
        ))
}

/// ALLOWLIST_RULE_EXTENSION_API_EXPERIMENTAL (StarlarkRuleClassFunctions.java:249-253)
/// with BuiltinRestriction.AllowlistEntry.allows (BuiltinRestriction.java:124-150).
fn private_initializer_allowed(definition: &crate::label::Label) -> bool {
    if definition.repository.is_empty() {
        [
            "initializer_testing/builtins",
            "third_party/bazel_rules/rules_cc",
        ]
        .iter()
        .any(|prefix| {
            definition.package == *prefix || definition.package.starts_with(&format!("{prefix}/"))
        })
    } else {
        definition.repository.starts_with("rules_cc+") || definition.repository == "_builtins"
    }
}

/// Lift a BUILD value like BuildType.copyAndLiftStarlarkValue: label strings
/// become Label objects resolved in the target's package; select() is kept.
fn lift<'v>(
    kind: &AttrKind,
    value: Value<'v>,
    resolve: &LabelResolver<'_>,
    heap: starlark::values::Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    if crate::rule_declarations::select_pieces(value).is_some() {
        return Ok(value);
    }
    let label = |value: Value<'v>| -> anyhow::Result<Value<'v>> {
        if crate::starlark_types::label_parts(value).is_some() {
            return Ok(value);
        }
        let text = value
            .unpack_str()
            .ok_or_else(|| anyhow!("expected a string or Label"))?;
        crate::starlark_types::alloc_label(heap, &resolve(text)?.to_string())
    };
    Ok(match kind {
        AttrKind::Label => label(value)?,
        AttrKind::LabelList => heap.alloc(
            sequence_ref(value)
                .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                .iter()
                .map(|value| label(*value))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        AttrKind::LabelKeyedStringDict => {
            let dict = DictRef::from_value(value)
                .ok_or_else(|| anyhow!("expected a dict of strings or Labels to strings"))?;
            let mut entries = Vec::new();
            for (key, value) in dict.iter() {
                entries.push((label(key)?, value));
            }
            heap.alloc(starlark::values::dict::AllocDict(entries))
        }
        _ => value,
    })
}

/// StarlarkRuleFunction.call initializer loop (StarlarkRuleClassFunctions.java:1810-1885).
fn run_initializer<'v>(
    initializer: Value<'v>,
    mut arguments: Vec<(String, Value<'v>, Origin)>,
    schema: &BTreeMap<String, AttrSpec>,
    declaration: &RuleDeclaration,
    definition: &crate::label::Label,
    resolve: &LabelResolver<'_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<(String, Value<'v>, Origin)>> {
    let heap = eval.heap();
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut kwargs = Vec::new();
    for (key, value, _) in &arguments {
        budget.node().map_err(anyhow::Error::msg)?;
        if value.is_none() {
            continue;
        }
        if key == "name" {
            kwargs.push((key.clone(), *value));
        } else if let Some(spec) = schema.get(key)
            && !key.starts_with('_')
        {
            let lifted = lift(&spec.kind, *value, resolve, heap)
                .map_err(|e| anyhow!("attribute {key:?}: {e}"))?;
            kwargs.push((key.clone(), lifted));
        }
    }
    let named = kwargs
        .iter()
        .map(|(key, value)| (key.as_str(), *value))
        .collect::<Vec<_>>();
    let returned = eval
        .eval_function(initializer, &[], &named)
        .map_err(|e| anyhow!(e.to_string()))?;
    if returned.is_none() {
        return Ok(arguments);
    }
    let dict = DictRef::from_value(returned).ok_or_else(|| {
        anyhow!(
            "got {} for 'rule's initializer return value', want dict",
            returned.get_type()
        )
    })?;
    for (key, value) in dict.iter() {
        if key.unpack_str().is_none() {
            bail!(
                "got dict<{}, {}> for 'rule's initializer return value', want dict<string, object>",
                key.get_type(),
                value.get_type()
            );
        }
    }
    let original_name = arguments
        .iter()
        .find(|(key, _, _)| key == "name")
        .map(|(_, value, _)| *value);
    for (key, value) in dict.iter() {
        budget.node().map_err(anyhow::Error::msg)?;
        let key = key.unpack_str().expect("checked initializer key");
        if key == "name" {
            if !original_name
                .map(|name| name.equals(value))
                .transpose()
                .map_err(|e| anyhow!(e.to_string()))?
                .unwrap_or(false)
            {
                bail!("Initializer can't change the name of the target");
            }
            continue;
        }
        if !crate::rule_declarations::is_identifier(key) {
            bail!("attribute name `{key}` is not a valid identifier.");
        }
        if key.starts_with('_') && !private_initializer_allowed(definition) {
            bail!(
                "file '{}' cannot use private API",
                crate::rule_declarations::canonical_form(definition)
            );
        }
        if !schema.contains_key(key) && native_attribute(key, declaration.test) {
            bail!("Initializer can only set Starlark defined attributes, not '{key}'");
        }
        budget.string(key).map_err(anyhow::Error::msg)?;
        if let Some(entry) = arguments.iter_mut().find(|(seen, _, _)| seen == key) {
            entry.1 = value;
            entry.2 = Origin::Initializer;
        } else {
            arguments.push((key.to_owned(), value, Origin::Initializer));
        }
    }
    Ok(arguments)
}

/// Coerce a `select()` attribute value (BuildType.selectableConvert,
/// packages/BuildType.java:220-330); every branch is type-checked at loading.
fn configurable_parts(
    key: &str,
    spec: &AttrSpec,
    nonconfigurable: bool,
    rule_class: &str,
    pieces: Vec<crate::rule_declarations::SelectPiece<'_>>,
    resolve: &LabelResolver<'_>,
    budget: &std::cell::RefCell<crate::repository_budget::HostBudget>,
) -> anyhow::Result<Vec<ConfigurablePart>> {
    use crate::rule_declarations::SelectPiece;
    // BuildType.convertFromBuildLangType (packages/BuildType.java:234-238).
    if nonconfigurable || matches!(spec.kind, AttrKind::Output | AttrKind::OutputList) {
        bail!("attribute \"{key}\" is not configurable");
    }
    let parse = |value, what: &str| -> anyhow::Result<AttrValue> {
        if let Some(error) = spec.kind.conversion_error(value, what) {
            bail!("{error}");
        }
        spec.kind
            .preflight_value_with_budget(value, &mut budget.borrow_mut())?;
        let parsed = spec.kind.parse(value, Some(resolve))?;
        budget
            .borrow_mut()
            .attr_value(&parsed)
            .map_err(anyhow::Error::msg)?;
        Ok(parsed)
    };
    // AttributeConversionContext.toString (BuildType.java:374-376).
    let attribute = format!("attribute '{key}' of '{rule_class}'");
    let mut parts = Vec::new();
    for piece in pieces {
        budget.borrow_mut().node().map_err(anyhow::Error::msg)?;
        parts.push(match piece {
            SelectPiece::Plain(value) => ConfigurablePart::Plain(parse(value, &attribute)?),
            SelectPiece::Select {
                branches,
                no_match_error,
            } => {
                let mut owned = Vec::new();
                for (condition, value) in branches {
                    let condition =
                        if let Some(label) = crate::starlark_types::unpack_label(condition) {
                            label
                        } else {
                            let text = condition.unpack_str().expect("validated select key");
                            // BuildType.Selector.DEFAULT_CONDITION_KEY.
                            if text == "//conditions:default" {
                                text.to_owned()
                            } else {
                                resolve(text)?.to_string()
                            }
                        };
                    let shown = crate::label::Label::parse_canonical(&condition)
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_else(|_| condition.clone());
                    // SelectBranchMessage (BuildType.java:654-668).
                    let what = format!(
                        "each branch in select expression of {attribute} (including '{shown}')"
                    );
                    let value = if value.is_none() {
                        None
                    } else {
                        Some(parse(value, &what)?)
                    };
                    owned.push((condition, value));
                }
                ConfigurablePart::Select {
                    branches: owned,
                    no_match_error,
                }
            }
        });
    }
    Ok(parts)
}

/// AttributeProvider.checkAllowedValues (packages/AttributeProvider.java:478-497),
/// over plain values and every select() branch.
fn check_allowed_values(
    target: &crate::label::Label,
    schema: &BTreeMap<String, AttrSpec>,
    attrs: &BTreeMap<String, AttrValue>,
    configurable: &BTreeMap<String, Vec<ConfigurablePart>>,
) -> anyhow::Result<()> {
    for (name, spec) in schema {
        if spec.declaration.values.is_empty() {
            continue;
        }
        let mut values = Vec::new();
        if let Some(parts) = configurable.get(name) {
            for part in parts {
                match part {
                    ConfigurablePart::Plain(value) => values.push(value),
                    ConfigurablePart::Select { branches, .. } => {
                        values.extend(
                            branches
                                .iter()
                                .map(|(_, value)| value.as_ref().unwrap_or(&spec.default)),
                        );
                    }
                }
            }
        } else if let Some(value) = attrs.get(name) {
            values.push(value);
        }
        for value in values {
            if !spec.declaration.allows(value) {
                bail!(
                    "{}: invalid value in '{name}' attribute: {}",
                    crate::rule_declarations::canonical_form(target),
                    spec.declaration.allowed_values_reason(value)
                );
            }
        }
    }
    Ok(())
}

/// StarlarkImplicitOutputsFunctionWithMap with the `%{name}` placeholder only.
fn implicit_outputs(
    outputs: &ImplicitOutputs,
    name: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    match outputs {
        ImplicitOutputs::None => Ok(Vec::new()),
        ImplicitOutputs::Function => bail!(
            "rule(outputs=<function>) implicit outputs functions are not supported by the native profile"
        ),
        ImplicitOutputs::Templates(templates) => templates
            .iter()
            .map(|(key, template)| {
                let output = template.replace("%{name}", name);
                if output.contains("%{") {
                    bail!(
                        "implicit output {key:?} template {template:?} uses placeholders other than %{{name}}, which are not supported by the native profile"
                    );
                }
                Ok((key.clone(), output))
            })
            .collect(),
    }
}

/// Every declared-but-unanalyzed feature this target would exercise.
fn unanalyzed(
    declaration: &RuleDeclaration,
    schema: &BTreeMap<String, AttrSpec>,
    attrs: &BTreeMap<String, AttrValue>,
    configurable: &BTreeMap<String, Vec<ConfigurablePart>>,
) -> Vec<String> {
    let mut features = declaration.unanalyzed();
    for (name, spec) in schema {
        let present = configurable.contains_key(name)
            || attrs
                .get(name)
                .is_some_and(|value| !AttrKind::is_empty_value(value));
        if spec.declaration.late_bound || present {
            features.extend(spec.declaration.unanalyzed(name));
        }
        if present && spec.kind == AttrKind::LabelKeyedStringDict {
            features.push(format!(
                "attribute '{name}' attr.label_keyed_string_dict value"
            ));
        }
        if configurable.contains_key(name) {
            features.push(format!(
                "attribute '{name}' select() (configured attribute resolution)"
            ));
        }
    }
    features
}

/// Owned view of a `rule()` definition's retained declaration (tests/diagnostics).
#[cfg(test)]
pub(crate) struct RuleDeclarationView {
    pub declaration: RuleDeclaration,
    pub schema: BTreeMap<String, AttrSpec>,
    pub has_initializer: bool,
}

#[cfg(test)]
pub(crate) fn rule_declaration(value: Value<'_>) -> Option<RuleDeclarationView> {
    let definition = Definition::from_value(value)?;
    Some(RuleDeclarationView {
        declaration: definition.declaration.clone(),
        schema: definition.schema.clone(),
        has_initializer: !definition.initializer.is_empty(),
    })
}

/// Copy the owned schema without exposing the private Starlark descriptor type.
pub(crate) fn attribute_spec(value: Value<'_>) -> Option<AttrSpec> {
    value
        .downcast_ref::<Descriptor>()
        .map(|value| value.0.clone())
}

/// Borrow a descriptor so host preparation can charge defaults before copying.
pub(crate) fn attribute_spec_ref(value: Value<'_>) -> Option<&AttrSpec> {
    value.downcast_ref::<Descriptor>().map(|value| &value.0)
}

/// `Dict.cast(attrs, String, Descriptor, "attrs")` plus checkAttributeName
/// (StarlarkRuleClassFunctions.java:1280-1298), for aspect() attributes.
pub(crate) fn attribute_dict(value: Value<'_>) -> anyhow::Result<Vec<(String, AttrSpec)>> {
    let dict = DictRef::from_value(value)
        .ok_or_else(|| anyhow!("got {} for 'attrs', want dict", value.get_type()))?;
    for (name, spec) in dict.iter() {
        if name.unpack_str().is_none() || spec.downcast_ref::<Descriptor>().is_none() {
            bail!(
                "got dict<{}, {}> for 'attrs', want dict<string, Attribute>",
                name.get_type(),
                spec.get_type()
            );
        }
    }
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut attributes = Vec::new();
    for (name, spec) in dict.iter() {
        let name = name.unpack_str().expect("checked attribute name");
        if !crate::rule_declarations::is_identifier(name) {
            bail!("attribute name `{name}` is not a valid identifier.");
        }
        let spec = &spec.downcast_ref::<Descriptor>().expect("checked").0;
        budget.string(name).map_err(anyhow::Error::msg)?;
        budget.spec(spec).map_err(anyhow::Error::msg)?;
        attributes.push((name.to_owned(), spec.clone()));
    }
    Ok(attributes)
}

/// buildProviderPredicate for aspect required_providers/required_aspect_providers.
pub(crate) fn provider_predicate(
    value: Value<'_>,
    what: &str,
) -> anyhow::Result<Vec<Vec<ProviderRequirement>>> {
    required_providers(value).map_err(|e| anyhow!("{what}: {e}"))
}

fn descriptor(
    kind: AttrKind,
    default: Option<Value<'_>>,
    mandatory: bool,
    allow_files: bool,
    allow_single_file: bool,
) -> anyhow::Result<Descriptor> {
    descriptor_in_package(
        kind,
        default,
        mandatory,
        allow_files,
        allow_single_file,
        None,
    )
}

fn descriptor_in_package(
    kind: AttrKind,
    default: Option<Value<'_>>,
    mandatory: bool,
    allow_files: bool,
    allow_single_file: bool,
    resolver: Option<&LabelResolver<'_>>,
) -> anyhow::Result<Descriptor> {
    if let Some(value) = default {
        kind.preflight_default(value)?;
    }
    let explicit_default = default.is_some_and(|value| !value.is_none());
    let default = default
        .map(|v| kind.parse(v, resolver))
        .transpose()?
        .unwrap_or_else(|| kind.empty());
    Ok(Descriptor(AttrSpec {
        kind,
        default,
        mandatory,
        allow_files: allow_files || allow_single_file,
        allow_single_file,
        non_empty: false,
        required_providers: Vec::new(),
        declaration: AttrDeclaration {
            explicit_default,
            ..AttrDeclaration::default()
        },
    }))
}

/// `values=` (StarlarkAttrModule.java:476-482): Sequence.noneableCast to Object.
fn allowed_values(kind: &AttrKind, values: Value<'_>) -> anyhow::Result<Vec<AllowedValue>> {
    // @Param(name = VALUES_ARG) is Sequence-typed (StarlarkAttrModuleApi.java:228-233, 283-290).
    let items = sequence_ref(values).ok_or_else(|| {
        crate::rule_declarations::param_type_error(
            kind.starlark_name(),
            "values",
            values,
            "sequence",
        )
    })?;
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut allowed: Vec<AllowedValue> = Vec::new();
    for &item in items {
        budget.node().map_err(anyhow::Error::msg)?;
        let value = kind.parse(item, None).ok().filter(|_| match kind {
            AttrKind::String => item.unpack_str().is_some(),
            _ => true,
        });
        let display = item
            .unpack_str()
            .map_or_else(|| item.to_str(), str::to_owned);
        budget.string(&display).map_err(anyhow::Error::msg)?;
        // ImmutableSet.copyOf deduplicates equal values.
        if !allowed
            .iter()
            .any(|seen| seen.display == display && seen.value == value)
        {
            allowed.push(AllowedValue { value, display });
        }
    }
    Ok(allowed)
}

/// LabelKeyedDictType.convert collision message (packages/BuildType.java:449-490).
fn label_keyed_duplicates(
    value: Value<'_>,
    what: &str,
    resolve: &LabelResolver<'_>,
) -> anyhow::Result<()> {
    let dict = DictRef::from_value(value).ok_or_else(|| anyhow!("expected a dict"))?;
    let mut converted: Vec<(String, Vec<String>)> = Vec::new();
    for (key, _) in dict.iter() {
        let canonical = match crate::starlark_types::unpack_label(key) {
            Some(label) => label,
            None => resolve(key.unpack_str().expect("checked label key"))?.to_string(),
        };
        let shown = crate::label::Label::parse_canonical(&canonical)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or(canonical);
        match converted.iter_mut().find(|(seen, _)| *seen == shown) {
            Some((_, originals)) => originals.push(key.to_repr()),
            None => converted.push((shown, vec![key.to_repr()])),
        }
    }
    let duplicates = converted
        .iter()
        .filter(|(_, originals)| originals.len() > 1)
        .map(|(label, originals)| format!(" {label} (as [{}])", originals.join(", ")))
        .collect::<Vec<_>>();
    if !duplicates.is_empty() {
        bail!("duplicate labels in {what}:{}", duplicates.join(","));
    }
    Ok(())
}

/// setAllowedFileTypes (StarlarkAttrModule.java:128-140): (files allowed, extension list).
fn file_types(param: &str, value: Value<'_>) -> anyhow::Result<(bool, Option<Vec<String>>)> {
    if let Some(flag) = value.unpack_bool() {
        return Ok((flag, None));
    }
    if sequence_ref(value).is_some() {
        let types = crate::rule_declarations::cast_strings(value, "allow_files argument")?;
        return Ok((true, Some(types)));
    }
    bail!("{param} should be a boolean or a string list")
}

/// Dependency-attribute options shared by label, label_list and label_keyed_string_dict.
struct DependencyOptions<'v> {
    /// `attr` function name for parameter type errors.
    function: &'static str,
    executable: bool,
    allow_files: Value<'v>,
    allow_single_file: Value<'v>,
    skip_validations: bool,
    providers: Value<'v>,
    for_dependency_resolution: Option<bool>,
    allow_rules: Value<'v>,
    cfg: Value<'v>,
    aspects: Value<'v>,
    flags: Value<'v>,
}

/// StarlarkAttrModule.createAttribute for dependency attributes, in Bazel's
/// validation order (StarlarkAttrModule.java:317-513).
fn dependency_descriptor<'v>(
    kind: AttrKind,
    default: Option<Value<'v>>,
    mandatory: bool,
    allow_empty: bool,
    options: DependencyOptions<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Descriptor> {
    use crate::rule_declarations::{param_type_error, require_sequence};
    let function = options.function;
    // BuiltinFunction @Param allowedTypes, in parameter order (StarlarkAttrModuleApi.java).
    if kind == AttrKind::LabelKeyedStringDict
        && let Some(value) = default
        && DictRef::from_value(value).is_none()
        && value.parameters_spec().is_none()
    {
        return Err(param_type_error(
            function,
            "default",
            value,
            "dict or function",
        ));
    }
    if !options.allow_files.is_none()
        && options.allow_files.unpack_bool().is_none()
        && sequence_ref(options.allow_files).is_none()
    {
        return Err(param_type_error(
            function,
            "allow_files",
            options.allow_files,
            "bool, sequence, or NoneType",
        ));
    }
    require_sequence(function, "providers", options.providers)?;
    if !options.allow_rules.is_none() && sequence_ref(options.allow_rules).is_none() {
        return Err(param_type_error(
            function,
            "allow_rules",
            options.allow_rules,
            "sequence or NoneType",
        ));
    }
    require_sequence(function, "aspects", options.aspects)?;
    require_sequence(function, "flags", options.flags)?;
    let late_bound = kind == AttrKind::Label
        && default.is_some_and(crate::rule_declarations::is_late_bound_default);
    if default.is_some_and(|value| value.parameters_spec().is_some()) {
        bail!("computed (function) attribute defaults are not supported by the native profile");
    }
    // A late-bound default is retained as metadata only; it never reaches the
    // ordinary default coercion and resolves (explicitly unsupported) at analysis.
    let default = if late_bound { None } else { default };
    let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
    let resolve =
        |text: &str| crate::loading::resolve_label_bounded(eval, text, &mut budget.borrow_mut());
    if kind == AttrKind::LabelKeyedStringDict
        && let Some(value) = default
    {
        // Attribute.Builder.defaultValue: what = "parameter 'default' of attribute
        // 'label_keyed_string_dict'" (Attribute.java:700-708).
        let what = "parameter 'default' of attribute 'label_keyed_string_dict'";
        if let Some(error) = kind.conversion_error(value, what) {
            bail!("{error}");
        }
        label_keyed_duplicates(value, what, &resolve)?;
    }
    let mut descriptor = descriptor_in_package(
        kind,
        default,
        mandatory,
        false,
        false,
        definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
    )?;
    let declaration = &mut descriptor.0.declaration;
    declaration.late_bound = late_bound;
    declaration.explicit_default |= late_bound;
    declaration.flags = crate::rule_declarations::noneable_strings(options.flags, "flags")?;
    declaration.for_dependency_resolution = options.for_dependency_resolution;
    declaration.skip_validations = options.skip_validations;
    descriptor.0.non_empty = !allow_empty;
    if options.executable {
        declaration.executable = true;
        if options.cfg.is_none() {
            bail!(
                "cfg parameter is mandatory when executable=True is provided. Please see https://bazel.build/extending/rules#configurations for more details."
            );
        }
    }
    if !options.allow_files.is_none() && !options.allow_single_file.is_none() {
        bail!("Cannot specify both allow_files and allow_single_file");
    }
    if !options.allow_files.is_none() {
        let (allowed, types) = file_types("allow_files", options.allow_files)?;
        descriptor.0.allow_files = allowed;
        declaration.file_types = types;
    } else if !options.allow_single_file.is_none() {
        let (allowed, types) = file_types("allow_single_file", options.allow_single_file)?;
        descriptor.0.allow_files = allowed;
        descriptor.0.allow_single_file = allowed;
        declaration.file_types = types;
    }
    if !options.allow_rules.is_none() {
        declaration.allow_rules = Some(crate::rule_declarations::cast_strings(
            options.allow_rules,
            "allowed rule classes for attribute definition",
        )?);
    }
    declaration.cfg = crate::rule_declarations::attribute_cfg(options.cfg)?;
    declaration.aspects = crate::rule_declarations::aspect_identities(options.aspects, "aspects")?;
    let declaration = declaration.clone();
    let mut descriptor = with_required_providers(descriptor, options.providers)?;
    descriptor.0.declaration = declaration;
    crate::repository_budget::HostBudget::default()
        .spec(&descriptor.0)
        .map_err(anyhow::Error::msg)?;
    Ok(descriptor)
}

#[starlark_module]
fn attr_globals(builder: &mut GlobalsBuilder) {
    fn string<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = AllocList::EMPTY)] values: Value<'v>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::String, default, mandatory, false, false)?;
        descriptor.0.declaration.values = allowed_values(&AttrKind::String, values)?;
        Ok(descriptor)
    }
    fn string_list(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::StringList, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn int_list(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if let Some(value) = default {
            // @Param allowedTypes Sequence<StarlarkInt> (StarlarkAttrModuleApi.java:577-582),
            // then IntegerListType conversion of each element.
            if sequence_ref(value).is_none() {
                return Err(crate::rule_declarations::param_type_error(
                    "int_list", "default", value, "sequence",
                ));
            }
            if let Some(error) =
                AttrKind::IntList.conversion_error(value, "parameter 'default' of attribute ''")
            {
                bail!("{error}");
            }
        }
        let mut descriptor = descriptor(AttrKind::IntList, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn string_dict(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] configurable: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'_>,
    ) -> anyhow::Result<Descriptor> {
        if configurable.is_some() {
            bail!("attr.string_dict configurable is unsupported");
        }
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("attr.string_dict doc must be a string or None");
        }
        let mut descriptor = descriptor(AttrKind::StringDict, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn string_list_dict(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] configurable: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'_>,
    ) -> anyhow::Result<Descriptor> {
        if configurable.is_some() {
            bail!("attr.string_list_dict configurable is unsupported");
        }
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("attr.string_list_dict doc must be a string or None");
        }
        let mut descriptor =
            descriptor(AttrKind::StringListDict, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn int<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = AllocList::EMPTY)] values: Value<'v>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::Int, default, mandatory, false, false)?;
        descriptor.0.declaration.values = allowed_values(&AttrKind::Int, values)?;
        Ok(descriptor)
    }
    fn bool(
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::Bool, default, mandatory, false, false)
    }
    fn label<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_single_file: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label";
        dependency_descriptor(
            AttrKind::Label,
            default,
            mandatory,
            true,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable,
                allow_files,
                allow_single_file,
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
            },
            eval,
        )
    }
    fn label_list<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label_list";
        dependency_descriptor(
            AttrKind::LabelList,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
            },
            eval,
        )
    }
    fn label_keyed_string_dict<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label_keyed_string_dict";
        dependency_descriptor(
            AttrKind::LabelKeyedStringDict,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
            },
            eval,
        )
    }
    fn output(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::Output, None, mandatory, false, false)
    }
    fn output_list(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::OutputList, None, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
}

/// Raw `rule()` arguments (StarlarkRuleFunctionsApi.java:428-788).
struct RuleArguments<'v> {
    implementation: Value<'v>,
    test: Option<bool>,
    attrs: Option<Value<'v>>,
    outputs: Value<'v>,
    executable: Option<bool>,
    output_to_genfiles: bool,
    fragments: Value<'v>,
    host_fragments: Value<'v>,
    skylark_testable: bool,
    toolchains: Value<'v>,
    provides: Value<'v>,
    dependency_resolution_rule: bool,
    exec_compatible_with: Value<'v>,
    analysis_test: bool,
    build_setting: Value<'v>,
    cfg: Value<'v>,
    exec_groups: Value<'v>,
    initializer: Value<'v>,
    parent: Value<'v>,
    extendable: Value<'v>,
    subrules: Value<'v>,
}

/// StarlarkRuleClassFunctions.rule + createRule (StarlarkRuleClassFunctions.java:548-1171).
fn define_rule<'v>(
    args: RuleArguments<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    use crate::rule_declarations::{param_type_error, require_sequence};
    // BuiltinFunction @Param allowedTypes, in parameter order, before the body
    // (StarlarkRuleFunctionsApi.java:428-762).
    if let Some(attrs) = args.attrs
        && DictRef::from_value(attrs).is_none()
    {
        return Err(param_type_error("rule", "attrs", attrs, "dict"));
    }
    for (param, value) in [
        ("fragments", args.fragments),
        ("host_fragments", args.host_fragments),
        ("toolchains", args.toolchains),
        ("provides", args.provides),
        ("exec_compatible_with", args.exec_compatible_with),
    ] {
        require_sequence("rule", param, value)?;
    }
    if !args.exec_groups.is_none() && DictRef::from_value(args.exec_groups).is_none() {
        return Err(param_type_error(
            "rule",
            "exec_groups",
            args.exec_groups,
            "dict or NoneType",
        ));
    }
    if !args.extendable.is_none()
        && args.extendable.unpack_bool().is_none()
        && args.extendable.unpack_str().is_none()
        && crate::starlark_types::label_parts(args.extendable).is_none()
    {
        return Err(param_type_error(
            "rule",
            "extendable",
            args.extendable,
            "bool, Label, string, or NoneType",
        ));
    }
    require_sequence("rule", "subrules", args.subrules)?;
    let Some(definition) = definition_package(eval) else {
        bail!(
            "rule() definitions are only allowed during extension initialization, not BUILD evaluation"
        );
    };
    let RuleArguments {
        implementation,
        attrs,
        build_setting,
        cfg,
        ..
    } = args;
    if !args.parent.is_none() {
        // StarlarkRuleClassFunctions.java:585-622.
        let Some(parent) = Definition::from_value(args.parent) else {
            bail!(
                "Parent needs to be a Starlark rule, was {}",
                args.parent.get_type()
            );
        };
        if parent.exported.get().is_none() {
            bail!("Please export the parent rule before extending it.");
        }
        for (failed, message) in [
            (
                args.executable.is_some(),
                "Omit executable parameter when extending rules.",
            ),
            (
                args.test.is_some(),
                "Omit test parameter when extending rules.",
            ),
            (
                !args.outputs.is_none(),
                "implicit_outputs is not supported when extending rules (deprecated).",
            ),
            (
                crate::custom_rule::sequence_ref(args.host_fragments)
                    .is_some_and(|items| !items.is_empty()),
                "host_fragments are not supported when extending rules (deprecated).",
            ),
            (
                args.output_to_genfiles,
                "output_to_genfiles are not supported when extending rules (deprecated).",
            ),
            (
                args.skylark_testable,
                "_skylark_testable is not supported when extending rules.",
            ),
            (
                args.analysis_test,
                "analysis_test is not supported when extending rules.",
            ),
            (
                !build_setting.is_none(),
                "build_setting is not supported when extending rules.",
            ),
        ] {
            if failed {
                bail!("{message}");
            }
        }
        bail!("rule(parent=...) rule extension is not supported by the native profile");
    }
    let executable = args.executable.unwrap_or(false);
    let test = args.test.unwrap_or(false) || args.analysis_test;
    let build_setting = if build_setting.is_none() {
        None
    } else {
        Some(*BuildSetting::from_value(build_setting).ok_or_else(|| {
            anyhow!(
                "rule build_setting must be a BuildSetting, got {}",
                build_setting.get_type()
            )
        })?)
    };
    implementation
        .check_callable_with(
            [&starlark::typing::Ty::any()],
            [],
            None,
            None,
            &starlark::typing::Ty::any(),
        )
        .map_err(|e| anyhow!("rule implementation must be callable with ctx: {e}"))?;
    // extendable (StarlarkRuleClassFunctions.java:791-808).
    let extendable = if args.extendable.is_none() {
        None
    } else if let Some(flag) = args.extendable.unpack_bool() {
        Some(if flag { "True" } else { "False" }.to_owned())
    } else if let Some(label) = crate::starlark_types::unpack_label(args.extendable) {
        Some(label)
    } else if let Some(text) = args.extendable.unpack_str() {
        Some(
            crate::bazel_label::Parts::parse(text)
                .map_err(anyhow::Error::msg)
                .and_then(|_| {
                    crate::loading::resolve_label_bounded(
                        eval,
                        text,
                        &mut crate::repository_budget::HostBudget::default(),
                    )
                })
                .map_err(|e| anyhow!("Unable to parse label '{text}': {e}"))?
                .to_string(),
        )
    } else {
        bail!(
            "parameter 'extendable': expected bool, str or Label, but got '{}'",
            args.extendable.get_type()
        );
    };
    let initializer = if args.initializer.is_none() {
        Vec::new()
    } else if args.initializer.parameters_spec().is_some() {
        vec![args.initializer]
    } else {
        bail!(
            "in call to rule(), parameter 'initializer' got value of type '{}', want 'function'",
            args.initializer.get_type()
        );
    };
    let mut schema = BTreeMap::new();
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    let subrules = crate::subrule::snapshot(args.subrules, &mut budget)?;
    crate::subrule::validate_exports(&subrules, &mut budget)?;
    let mut propagates_aspects = false;
    if let Some(attrs) = attrs {
        let dict = DictRef::from_value(attrs)
            .ok_or_else(|| anyhow!("rule attrs must be a dict of attribute descriptors"))?;
        for (name, spec) in dict.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| anyhow!("attribute names must be strings"))?;
            if matches!(
                name,
                "name"
                    | "visibility"
                    | "tags"
                    | "testonly"
                    | "deprecation"
                    | "features"
                    | "compatible_with"
                    | "restricted_to"
                    | "target_compatible_with"
                    | "exec_compatible_with"
                    | "exec_group_compatible_with"
                    | "exec_properties"
                    | "package_metadata"
                    | "toolchains"
                    | "licenses"
                    | "distribs"
            ) {
                bail!("attribute {name:?} is reserved");
            }
            if name.is_empty()
                || !name.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                })
            {
                bail!("invalid attribute name {name:?}");
            }
            let spec = spec
                .downcast_ref::<Descriptor>()
                .ok_or_else(|| anyhow!("attribute {name:?} must be an attr descriptor"))?;
            // AttributeValueSource.LATE_BOUND requires the Starlark '_' prefix
            // (packages/AttributeValueSource.java:47-58).
            if spec.0.declaration.late_bound && !name.starts_with('_') {
                bail!(
                    "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found '{name}'"
                );
            }
            if name.starts_with('_') && spec.0.mandatory {
                bail!(
                    "private attribute {name:?} cannot be mandatory because it cannot be set explicitly"
                );
            }
            propagates_aspects |= !spec.0.declaration.aspects.is_empty();
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.spec(&spec.0).map_err(anyhow::Error::msg)?;
            schema.insert(name.to_owned(), spec.0.clone());
        }
    }
    // Implicit outputs (StarlarkRuleClassFunctions.java:1102-1122).
    let mut outputs_function = Vec::new();
    let implicit_outputs = if args.outputs.is_none() {
        ImplicitOutputs::None
    } else if args.outputs.parameters_spec().is_some() {
        outputs_function.push(args.outputs);
        ImplicitOutputs::Function
    } else if let Some(dict) = DictRef::from_value(args.outputs) {
        let mut templates = Vec::new();
        for (key, value) in dict.iter() {
            let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str()) else {
                bail!(
                    "got dict<{}, {}> for 'implicit outputs of the rule class', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                );
            };
            budget.string(key).map_err(anyhow::Error::msg)?;
            budget.string(value).map_err(anyhow::Error::msg)?;
            templates.push((key.to_owned(), value.to_owned()));
        }
        ImplicitOutputs::Templates(templates)
    } else {
        bail!(
            "in call to rule(), parameter 'outputs' got value of type '{}', want 'dict, NoneType, or function'",
            args.outputs.get_type()
        );
    };
    let fragments = crate::rule_declarations::cast_strings(args.fragments, "fragments")?;
    let host_fragments =
        crate::rule_declarations::cast_strings(args.host_fragments, "host_fragments")?;
    let toolchains = crate::rule_declarations::toolchains(eval, args.toolchains, eval.heap())?;
    let exec_groups = crate::rule_declarations::exec_groups(args.exec_groups)?;
    if build_setting.is_some() && !cfg.is_none() {
        bail!("Build setting rules cannot use the `cfg` param to apply transitions to themselves.");
    }
    let rule_cfg = crate::rule_declarations::rule_cfg(cfg)?;
    if args.dependency_resolution_rule {
        // StarlarkRuleClassFunctions.java:1133-1146.
        if !subrules.is_empty() {
            bail!("Rules that can be required for materializers cannot have subrules");
        }
        if !toolchains.is_empty() {
            bail!("Rules that can be required for materializers cannot depend on toolchains");
        }
        if propagates_aspects {
            bail!("Rules that can be required for materializes cannot propagate aspects");
        }
    }
    let provides = crate::rule_declarations::provider_identifiers(args.provides, "provides")?;
    let exec_compatible_with =
        crate::rule_declarations::exec_compatible_with(eval, args.exec_compatible_with)?;
    if let Some(setting) = build_setting {
        for name in ["build_setting_default", "help"] {
            if schema.contains_key(name) {
                bail!("attribute {name:?} is reserved for build-setting rules");
            }
        }
        let kind = match setting.kind {
            BuildSettingKind::Bool => AttrKind::Bool,
            BuildSettingKind::Int => AttrKind::Int,
            BuildSettingKind::String => AttrKind::String,
            BuildSettingKind::StringList => AttrKind::StringList,
        };
        for (name, spec) in [
            (
                "build_setting_default",
                descriptor(kind, None, true, false, false)?.0,
            ),
            (
                "help",
                descriptor(AttrKind::String, None, false, false, false)?.0,
            ),
        ] {
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.spec(&spec).map_err(anyhow::Error::msg)?;
            schema.insert(name.to_owned(), spec);
        }
    }
    let declaration = RuleDeclaration {
        definition_label: definition.to_string(),
        executable,
        test,
        analysis_test: args.analysis_test,
        skylark_testable: args.skylark_testable,
        output_to_genfiles: args.output_to_genfiles,
        dependency_resolution_rule: args.dependency_resolution_rule,
        fragments,
        host_fragments,
        toolchains,
        exec_groups,
        exec_compatible_with,
        cfg: rule_cfg,
        provides,
        implicit_outputs,
        extendable,
    };
    budget
        .string(&declaration.definition_label)
        .map_err(anyhow::Error::msg)?;
    Ok(eval.heap().alloc(DefinitionGen {
        implementation,
        subrules,
        initializer,
        outputs_function,
        schema,
        build_setting,
        declaration,
        exported: OnceLock::new(),
    }))
}

// The public Starlark rule signature has independently named Bazel options.
// Scope this allowance to the generated constructor and its argument wrappers.
#[allow(clippy::too_many_arguments)]
mod definitions {
    use super::*;
    #[starlark_module]
    pub(super) fn definition_globals(builder: &mut GlobalsBuilder) {
        fn rule<'v>(
            implementation: Value<'v>,
            #[starlark(require = named)] test: Option<bool>,
            #[starlark(require = named)] attrs: Option<Value<'v>>,
            #[starlark(require = named, default = NoneType)] outputs: Value<'v>,
            #[starlark(require = named)] executable: Option<bool>,
            #[starlark(require = named, default = false)] output_to_genfiles: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] fragments: Value<'v>,
            #[starlark(require = named, default = AllocList::EMPTY)] host_fragments: Value<'v>,
            #[starlark(require = named, default = false)] _skylark_testable: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] toolchains: Value<'v>,
            #[starlark(require = named)] doc: Option<&str>,
            #[starlark(require = named, default = AllocList::EMPTY)] provides: Value<'v>,
            #[starlark(require = named, default = false)] dependency_resolution_rule: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] exec_compatible_with: Value<
                'v,
            >,
            #[starlark(require = named, default = false)] analysis_test: bool,
            #[starlark(require = named, default = NoneType)] build_setting: Value<'v>,
            #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
            #[starlark(require = named, default = NoneType)] exec_groups: Value<'v>,
            #[starlark(require = named, default = NoneType)] initializer: Value<'v>,
            #[starlark(require = named, default = NoneType)] parent: Value<'v>,
            #[starlark(require = named, default = NoneType)] extendable: Value<'v>,
            #[starlark(require = named, default = AllocList::EMPTY)] subrules: Value<'v>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let _ = doc;
            define_rule(
                RuleArguments {
                    implementation,
                    test,
                    attrs,
                    outputs,
                    executable,
                    output_to_genfiles,
                    fragments,
                    host_fragments,
                    skylark_testable: _skylark_testable,
                    toolchains,
                    provides,
                    dependency_resolution_rule,
                    exec_compatible_with,
                    analysis_test,
                    build_setting,
                    cfg,
                    exec_groups,
                    initializer,
                    parent,
                    extendable,
                    subrules,
                },
                eval,
            )
        }
    }
}

/// Install only into .bzl globals; BUILD may invoke loaded definitions, not create them.
pub fn rule_globals(builder: &mut GlobalsBuilder) {
    definitions::definition_globals(builder);
    crate::starlark_types::analysis_globals(builder);
    let attrs = GlobalsBuilder::new().with(attr_globals).build();
    builder.frozen_heap().add_reference(attrs.heap());
    builder.set("attr", AllocStruct(attrs.iter()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{FrozenModule, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn provider_extension(label: &str, source: &str) -> anyhow::Result<FrozenModule> {
        let globals = GlobalsBuilder::standard()
            .with(rule_globals)
            .with(crate::providers::globals)
            .with(crate::template_variable_info::install)
            .build();
        Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse(label, "").unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    std::path::PathBuf::from("."),
                )),
            };
            let ast = AstModule::parse(label, source.to_owned(), &Dialect::Standard).unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals)
                    .map_err(|error| anyhow!("{error}"))?;
            }
            module.freeze().map_err(|error| anyhow!("{error:?}"))
        })
    }

    #[test]
    fn provider_constraints_normalize_sequences_and_preserve_export_identity() {
        let frozen = provider_extension(
            "//defs:rules.bzl",
            r#"
P = provider()
Alias = P
Q = provider()
T = platform_common.TemplateVariableInfo
items = [P, Alias, DefaultInfo, T, P]
x = attr.label(providers=items)
items.append(Q)
y = attr.label_list(providers=((P, P), [T, Q]))
a = attr.label()
b = attr.label(providers=())
c = attr.label(providers=[[], [P]])
d = attr.label_list(providers=((P,), ()))
"#,
        )
        .unwrap();
        let x = frozen.get("x").unwrap();
        let x = attribute_spec_ref(x.value()).unwrap();
        assert_eq!(x.required_providers.len(), 1);
        assert_eq!(
            x.required_providers[0]
                .iter()
                .map(|p| p.display_name.as_str())
                .collect::<Vec<_>>(),
            ["P", "DefaultInfo", "TemplateVariableInfo"]
        );
        assert_eq!(x.required_providers[0][0].id, "//defs:rules.bzl%P");
        let y = frozen.get("y").unwrap();
        let y = attribute_spec_ref(y.value()).unwrap();
        assert_eq!(
            y.required_providers
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        for name in ["a", "b", "c", "d"] {
            let value = frozen.get(name).unwrap();
            let spec = attribute_spec_ref(value.value()).unwrap();
            assert!(spec.required_providers.is_empty());
            assert!(
                !serde_json::to_string(spec)
                    .unwrap()
                    .contains("required_providers")
            );
        }

        let other = provider_extension("//other:rules.bzl", "P = provider()\n").unwrap();
        Module::with_temp_heap(|module| {
            let first = frozen.get("P").unwrap();
            let second = other.get("P").unwrap();
            let first = module.heap().access_owned_frozen_value(&first);
            let second = module.heap().access_owned_frozen_value(&second);
            let clauses =
                required_providers(module.heap().alloc(vec![first, second, first])).unwrap();
            assert_eq!(clauses[0].len(), 2);
            assert_eq!(clauses[0][0].display_name, clauses[0][1].display_name);
            assert_ne!(clauses[0][0].id, clauses[0][1].id);
        });
    }

    #[test]
    fn provider_constraints_reject_malformed_and_unexported_even_after_empty_clause() {
        for kind in ["label", "label_list"] {
            for expression in [
                "None",
                "'legacy'",
                "[\"PackageSpecificationInfo\"]",
                "[['legacy']]",
                "[None]",
                "[True]",
                "[1]",
                "[len]",
                "[DefaultInfo()]",
                "[P()]",
                "[P, [DefaultInfo]]",
                "[[P], DefaultInfo]",
                "[[[P]]]",
                "[provider()]",
                "[[], [provider()]]",
                "[[], ['legacy']]",
                "[[], [[P]]]",
                "[[], P]",
                "{}",
            ] {
                let source = format!("P = provider()\nx = attr.{kind}(providers={expression})\n");
                let error = provider_extension("//defs:rules.bzl", &source)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("providers"), "{source}: {error}");
            }
        }
        for kind in [
            "string",
            "string_list",
            "string_list_dict",
            "bool",
            "int",
            "output",
            "output_list",
        ] {
            let error = definition_error(&format!("attr.{kind}(providers=[])"));
            assert!(error.contains("providers"), "{kind}: {error}");
        }
        assert!(
            definition_error("rule(implementation=impl, provides=[1])")
                .contains("at index 0 of provides, got element of type int, want Provider")
        );
    }

    #[test]
    fn provider_constraints_budget_input_before_deduplication_and_empty_normalization() {
        for expression in [
            "[DefaultInfo]*100000",
            "[[DefaultInfo]]*40000",
            "[[]]*100000",
            "[[]] + [[DefaultInfo]]*40000",
        ] {
            let error = definition_error(&format!("attr.label(providers={expression})"));
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{expression}: {error}"
            );
        }
        let source = "P = provider()\nx = attr.label(providers=[[P]]*10000)\ndef impl(ctx):\n    return []\nr = rule(implementation=impl, attrs={name: x for name in ['a', 'b', 'c']})\n";
        let error = provider_extension("//defs:rules.bzl", source)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        let label = format!("//{}:rules.bzl", "p".repeat(65536));
        let error = provider_extension(
            &label,
            "P = provider()\nx = attr.label(providers=[P]*129)\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        let error = definition_error(
            "attr.label(default='//:'+'x'*(7*1024*1024), providers=[[DefaultInfo]]*15000)",
        );
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
    }

    fn definition_error(expression: &str) -> String {
        let globals = GlobalsBuilder::standard()
            .with(rule_globals)
            .with(crate::build_settings::install)
            .build();
        Module::with_temp_heap(|module| {
            let source = format!("def impl(ctx):\n    return []\nx = {expression}");
            let ast = AstModule::parse("defs.bzl", source, &Dialect::Standard).unwrap();
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse("//definitions:defs.bzl", "").unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    std::path::PathBuf::from("."),
                )),
            };
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(ast, &globals).unwrap_err().to_string()
        })
    }

    #[test]
    fn string_list_dict_snapshots_ordered_defaults_without_freezing_callers() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                r#"
items = ["first", "first"]
defaults = {"z": items, "a": ("last",), "empty": []}
x = attr.string_list_dict(False, default=defaults, mandatory=True, doc=None)
items.append("later")
defaults["new"] = ["later"]
y = attr.string_list_dict(allow_empty=False, doc="documented")
z = attr.string_list_dict()
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            let spec = attribute_spec_ref(module.get("x").unwrap()).unwrap();
            assert_eq!(spec.kind, AttrKind::StringListDict);
            assert!(spec.non_empty && spec.mandatory);
            assert_eq!(
                spec.default,
                AttrValue::StringListDict(vec![
                    ("z".into(), vec!["first".into(), "first".into()]),
                    ("a".into(), vec!["last".into()]),
                    ("empty".into(), vec![]),
                ])
            );
            let empty = attribute_spec_ref(module.get("y").unwrap()).unwrap();
            assert!(empty.non_empty);
            assert_eq!(empty.default, AttrValue::StringListDict(vec![]));
            assert!(
                !attribute_spec_ref(module.get("z").unwrap())
                    .unwrap()
                    .non_empty
            );
            assert_eq!(
                DictRef::from_value(module.get("defaults").unwrap())
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(sequence_ref(module.get("items").unwrap()).unwrap().len(), 3);
        });
    }

    #[test]
    fn string_dict_snapshots_ordered_defaults_without_phase_guard() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                r#"
defaults = {"z": "before", "a": "café☃", "literal": "@missing//pkg:target"}
x = attr.string_dict(False, default=defaults, mandatory=True, doc=None)
defaults["z"] = "after"
defaults["new"] = "later"
y = attr.string_dict(allow_empty=False, doc="documented")
z = attr.string_dict()
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            // Like neighboring constructors, pure metadata does not need eval.extra.
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            let spec = attribute_spec(module.get("x").unwrap()).unwrap();
            assert_eq!(spec.kind, AttrKind::StringDict);
            assert!(spec.non_empty && spec.mandatory);
            assert_eq!(
                spec.default,
                AttrValue::StringDict(vec![
                    ("z".into(), "before".into()),
                    ("a".into(), "café☃".into()),
                    ("literal".into(), "@missing//pkg:target".into()),
                ])
            );
            let empty = attribute_spec_ref(module.get("y").unwrap()).unwrap();
            assert!(empty.non_empty);
            assert_eq!(empty.default, AttrValue::StringDict(vec![]));
            assert!(
                !attribute_spec_ref(module.get("z").unwrap())
                    .unwrap()
                    .non_empty
            );
            assert_eq!(
                DictRef::from_value(module.get("defaults").unwrap())
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(
                serde_json::to_string(&empty.default).unwrap(),
                r#"{"StringDict":[]}"#
            );
            assert_eq!(
                serde_json::to_value(spec.clone()).unwrap(),
                serde_json::to_value(spec).unwrap()
            );
        });
        // Loaded constructor aliases and helpers may construct unused BUILD metadata.
        build_fixture("r = attr.string_dict", "unused = r(default={'z':'v'})").unwrap();
        build_fixture(
            "def r():\n    return attr.string_dict(default={'z':'v'})",
            "unused = r()",
        )
        .unwrap();
    }

    #[test]
    fn string_dict_rejects_invalid_shapes_and_parameters() {
        for default in [
            "None",
            "[]",
            "()",
            "'text'",
            "{1: 's'}",
            "{False: 's'}",
            "{'x': []}",
            "{'x': ()}",
            "{'x': 1}",
            "{'x': False}",
            "{'x': None}",
            "{'x': {}}",
            "{'x': Label('//:x')}",
            "{Label('//:x'): 's'}",
            "{'x': DefaultInfo()}",
        ] {
            let error = definition_error(&format!("attr.string_dict(default={default})"));
            assert!(
                error.contains("expected a dict of strings to strings"),
                "{default}: {error}"
            );
        }
        for value in ["None", "True", "False", "1"] {
            let error = definition_error(&format!("attr.string_dict(configurable={value})"));
            assert!(error.contains("configurable is unsupported"), "{error}");
        }
        for (expression, expected) in [
            ("attr.string_dict(doc=1)", "doc must be a string or None"),
            ("attr.string_dict(allow_empty=None)", "bool"),
            ("attr.string_dict(allow_empty=1)", "bool"),
            ("attr.string_dict(mandatory=1)", "bool"),
            ("attr.string_dict(True, {})", "positional"),
            ("attr.string_dict(values=[])", "values"),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[allow(unsafe_code)]
    mod string_dict_spoof {
        use super::*;
        use starlark::values::{
            NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
        };
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct FakeString;
        starlark::starlark_simple_value!(FakeString);
        impl fmt::Display for FakeString {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("attribute preflight must not format arbitrary values")
            }
        }
        #[starlark_value(type = "string")]
        impl<'v> StarlarkValue<'v> for FakeString {
            fn dir_attr(&self) -> Vec<String> {
                panic!("attribute preflight must not reflect arbitrary values")
            }
            fn get_attr(&self, _: &str, _: starlark::values::Heap<'v>) -> Option<Value<'v>> {
                panic!("attribute preflight must not call arbitrary attributes")
            }
        }
    }

    #[test]
    fn string_dict_preflight_rejects_spoofed_string_type_without_reflection() {
        Module::with_temp_heap(|module| {
            let fake = module.heap().alloc(string_dict_spoof::FakeString);
            assert_eq!(fake.get_type(), "string");
            let value = module
                .heap()
                .alloc(starlark::values::dict::AllocDict([("k", fake)]));
            let error = AttrKind::StringDict.preflight_default(value).unwrap_err();
            assert_eq!(error.to_string(), "expected a dict of strings to strings");
            assert!(AttrKind::StringDict.preflight_default(fake).is_err());
        });
    }

    #[test]
    fn string_dict_preflight_bounds_shared_payloads_before_copying() {
        for expression in [
            "attr.string_dict(default={'x'*(9*1024*1024): ''})",
            "attr.string_dict(default={'x': 'x'*(9*1024*1024)})",
            "attr.string_dict(default={str(i): 'x'*(1024*1024) for i in range(9)})",
            "attr.string_dict(default={str(i): '' for i in range(50000)})",
            "attr.string_dict(default={'x': 'x'*(3*1024*1024)})\nr = rule(implementation=impl, attrs={name:x for name in ['a','b','c']})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        // Validate the entire shape before reporting a resource error, without printing keys.
        for default in [
            "{'x': 'x'*(9*1024*1024), 'bad': []}",
            "{'x'*(9*1024*1024): []}",
        ] {
            let error = definition_error(&format!("attr.string_dict(default={default})"));
            assert!(
                error.contains("expected a dict of strings to strings"),
                "{error}"
            );
            assert!(!error.contains("host preparation"), "{error}");
            assert!(error.len() < 4096);
        }
        for (definition, call) in [
            (
                "r = rule(implementation=impl, attrs={'x':attr.string_dict()})",
                "s='x'*(1024*1024)\nr(name='x', x={str(i):s for i in range(9)})",
            ),
            (
                "r = rule(implementation=impl, attrs={'a':attr.string_dict(), 'b':attr.string_dict()})",
                "s='x'*(5*1024*1024)\nr(name='x', a={'k':s}, b={'k':s})",
            ),
            (
                "r = rule(implementation=impl, attrs={'x':attr.string_dict(default={'k':'x'*(5*1024*1024)})})",
                "r(name='x')",
            ),
        ] {
            assert!(
                build_fixture(definition, call)
                    .unwrap_err()
                    .contains("host preparation")
            );
        }
    }

    #[test]
    fn string_dict_build_snapshots_none_mandatory_and_literal_values() {
        let definition = "d={'z':'before', 'a':'alpha'}\nspec=attr.string_dict(default=d)\nd['z']='between'\nr=rule(implementation=impl, attrs={'x':spec})\nd['z']='after'";
        let package = build_fixture(definition, "d={'z':'café☃', 'a':'@missing//p:t'}\nr(name='x', x=d)\nd['z']='late'\nd['new']='later'\nr(name='none', x=None)").unwrap();
        let rule = &package[&crate::label::Label::parse("//:x", "").unwrap()];
        assert!(rule.srcs.is_empty());
        assert_eq!(
            rule.custom.as_ref().unwrap().attrs["x"],
            AttrValue::StringDict(vec![
                ("z".into(), "café☃".into()),
                ("a".into(), "@missing//p:t".into()),
            ])
        );
        assert_eq!(
            package[&crate::label::Label::parse("//:none", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringDict(vec![
                ("z".into(), "before".into()),
                ("a".into(), "alpha".into())
            ])
        );
        let required = "r=rule(implementation=impl, attrs={'x':attr.string_dict(default={'k':'v'}, mandatory=True)})";
        for call in ["r(name='x')", "r(name='x', x=None)"] {
            assert!(
                build_fixture(required, call)
                    .unwrap_err()
                    .contains("missing mandatory attribute")
            );
        }
        build_fixture(required, "r(name='x', x={})").unwrap();
        for input in [
            "{1:'s'}",
            "{'x':[]}",
            "{'x':()}",
            "{'x':False}",
            "{'x':None}",
            "{'x':{}}",
        ] {
            assert!(
                build_fixture(definition, &format!("r(name='x', x={input})"))
                    .unwrap_err()
                    .contains("expected a dict of strings to strings")
            );
        }
    }

    #[test]
    fn string_list_dict_rejects_invalid_shapes_and_present_configurable() {
        for expression in [
            "attr.string_list_dict(default=None)",
            "attr.string_list_dict(default=[])",
            "attr.string_list_dict(default={1: []})",
            "attr.string_list_dict(default={'x': 'text'})",
            "attr.string_list_dict(default={'x': {}})",
            "attr.string_list_dict(default={'x': [1]})",
            "attr.string_list_dict(default={'x': (True,)})",
            "attr.string_list_dict(default={'x': [Label('//:x')]})",
            "attr.string_list_dict(default={Label('//:x'): []})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("expected a dict of strings to a list of strings"),
                "{expression}: {error}"
            );
        }
        for value in ["None", "True", "False", "1"] {
            let error = definition_error(&format!("attr.string_list_dict(configurable={value})"));
            assert!(error.contains("configurable is unsupported"), "{error}");
        }
        for (expression, expected) in [
            (
                "attr.string_list_dict(doc=1)",
                "doc must be a string or None",
            ),
            ("attr.string_list_dict(allow_empty=None)", "bool"),
            ("attr.string_list_dict(mandatory=1)", "bool"),
            ("attr.string_list_dict(True, {})", "positional"),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[test]
    fn string_list_dict_preflight_counts_keys_sequences_and_shared_strings() {
        for expression in [
            "attr.string_list_dict(default={'x'*(9*1024*1024): []})",
            "attr.string_list_dict(default={'x': ['x'*(1024*1024)]*9})",
            "attr.string_list_dict(default={'x': ('x'*(1024*1024),)*9})",
            "attr.string_list_dict(default={str(i): [] for i in range(50000)})",
            "attr.string_list_dict(default={'x': ['']*100000})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{expression}: {error}"
            );
        }
        let error = definition_error(
            "attr.string_list_dict(default={'x': ['x'*(1024*1024)]*9, 'bad': [1]})",
        );
        assert!(error.contains("list of strings"), "{error}");
        assert!(!error.contains("host preparation"), "{error}");
    }

    #[test]
    fn attribute_sequences_accept_tuples_and_keep_strict_elements() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                "ss = ('a', 'a')\nlabels = ('//:a', Label('//:b'))\nbad = (1,)".into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            for (kind, name, expected) in [
                (
                    AttrKind::StringList,
                    "ss",
                    AttrValue::Strings(vec!["a".into(), "a".into()]),
                ),
                (
                    AttrKind::OutputList,
                    "ss",
                    AttrValue::Outputs(vec!["a".into(), "a".into()]),
                ),
                (
                    AttrKind::LabelList,
                    "labels",
                    AttrValue::Labels(vec!["//:a".into(), "//:b".into()]),
                ),
            ] {
                let value = module.get(name).unwrap();
                kind.preflight_default(value).unwrap();
                assert_eq!(kind.parse(value, None).unwrap(), expected);
                assert!(kind.preflight_default(module.get("bad").unwrap()).is_err());
            }
        });
        for expression in [
            "attr.string_list(default=('x'*(1024*1024),)*9)",
            "attr.label_list(default=('//:'+'x'*65536,)*129)",
        ] {
            assert!(definition_error(expression).contains("host preparation"));
        }
    }

    #[test]
    fn non_empty_metadata_preserves_existing_schema_serialization() {
        let spec = descriptor(AttrKind::String, None, false, false, false)
            .unwrap()
            .0;
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"kind":"String","default":{"String":""},"mandatory":false,"allow_files":false,"allow_single_file":false}"#
        );
        let mut spec = descriptor(AttrKind::StringListDict, None, false, false, false)
            .unwrap()
            .0;
        spec.default = AttrValue::StringListDict(vec![
            ("z".into(), vec!["b".into(), "a".into()]),
            ("a".into(), vec![]),
        ]);
        assert!(!serde_json::to_string(&spec).unwrap().contains("non_empty"));
        spec.non_empty = true;
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"kind":"StringListDict","default":{"StringListDict":[["z",["b","a"]],["a",[]]]},"mandatory":false,"allow_files":false,"allow_single_file":false,"non_empty":true}"#
        );
    }

    #[test]
    fn rule_schema_preflight_charges_repeated_descriptor_aliases() {
        let error = definition_error(
            "attr.string_list_dict(default={'x': ['x'*(3*1024*1024)]})\nr = rule(implementation=impl, attrs={name: x for name in ['a','b','c']})",
        );
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
    }

    fn build_fixture(definition: &str, call: &str) -> Result<crate::package::Package, String> {
        let root = tempfile::Builder::new()
            .prefix("attribute-budget-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            format!("def impl(ctx):\n    return []\n{definition}\n"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("BUILD.bazel"),
            format!("load(':defs.bzl', 'r')\n{call}\n"),
        )
        .unwrap();
        crate::package::evaluate(root.path(), "")
    }

    #[test]
    fn build_attribute_preflight_aggregates_schema_explicit_values_and_defaults() {
        for (definition, call) in [
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict()})",
                "r(name='x', x={'k': ['x'*(1024*1024)]*9})",
            ),
            (
                "r = rule(implementation=impl, attrs={'a': attr.string_list_dict(), 'b': attr.string_list_dict()})",
                "r(name='x', a={'k': ['x'*(5*1024*1024)]}, b={'k': ['x'*(5*1024*1024)]})",
            ),
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})",
                "r(name='x')",
            ),
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})",
                "r(name='x', x={'k': ['x'*(4*1024*1024)]})",
            ),
        ] {
            let error = build_fixture(definition, call).unwrap_err();
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        build_fixture("r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})", "r(name='x', x={})").unwrap();
    }

    #[test]
    fn build_string_list_dict_snapshots_values_and_preserves_none_mandatory_semantics() {
        let definition = "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'default': ['value']})})";
        let package = build_fixture(definition, "items=['a']\nd={'z': items, 'a': ('b',)}\nr(name='x', x=d)\nitems.append('late')\nd['new']=[]\nr(name='none', x=None)").unwrap();
        assert_eq!(
            package[&crate::label::Label::parse("//:x", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringListDict(vec![
                ("z".into(), vec!["a".into()]),
                ("a".into(), vec!["b".into()])
            ])
        );
        assert_eq!(
            package[&crate::label::Label::parse("//:none", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringListDict(vec![("default".into(), vec!["value".into()])])
        );
        let definition =
            "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(mandatory=True)})";
        for call in ["r(name='x')", "r(name='x', x=None)"] {
            assert!(
                build_fixture(definition, call)
                    .unwrap_err()
                    .contains("missing mandatory attribute")
            );
        }
        build_fixture(definition, "r(name='x', x={})").unwrap();
    }

    #[test]
    fn descriptor_preflight_counts_shared_payloads_and_preserves_type_errors() {
        for expression in [
            "attr.string_list(default=['x'*(1024*1024)]*9)",
            "attr.label_list(default=[Label('//:'+'x'*65536)]*129)",
            "attr.label_list(default=['//:'+'x'*65536]*129)",
            "attr.string_list(default=['']*100000)",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        for (expression, expected) in [
            (
                "attr.string_list(default=['x'*(1024*1024)]*9+[1])",
                "expected a list of strings",
            ),
            (
                "attr.label_list(default=['//:'+'x'*65536]*129+[1])",
                "expected a list of strings or Labels",
            ),
            ("attr.string(default=None)", "expected a string"),
            (
                "attr.label_list(default=None)",
                "expected a list of strings or Labels",
            ),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("host preparation"), "{error}");
        }
    }

    #[test]
    fn descriptor_normalization_charges_mapped_labels_cumulatively() {
        let root = tempfile::Builder::new()
            .prefix("descriptor-mapping-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let symbol = "e".repeat(65_536);
        std::fs::write(root.path().join("MODULE.bazel"), format!(
            "module(name='main')\ne=use_extension('//:ext.bzl','{symbol}')\nuse_repo(e,chosen='made')\n"
        )).unwrap();
        let context = crate::loading::ExtensionContext {
            byte_strings: false,
            label: crate::label::Label::parse("//:ext.bzl", "").unwrap(),
            repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                root.path().to_owned(),
            )),
        };
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for count in [1, 200] {
            Module::with_temp_heap(|module| {
                let ast = AstModule::parse(
                    "//:ext.bzl",
                    format!("x=attr.label_list(default=['@chosen//:data']*{count})"),
                    &Dialect::Standard,
                )
                .unwrap();
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let result = eval.eval_module(ast, &globals);
                if count == 1 {
                    result.unwrap();
                    let value = module.get("x").unwrap();
                    assert_eq!(
                        attribute_spec_ref(value).unwrap().default,
                        AttrValue::Labels(vec![format!("@@+{symbol}+made//:data")])
                    );
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains("host preparation") && error.contains("limit"),
                        "{error}"
                    );
                }
            });
        }
    }

    #[test]
    fn invalid_descriptors_and_rule_options_are_explicit_errors() {
        for (expression, expected) in [
            ("attr.string(default=1)", "string"),
            ("attr.string_list(default=[1])", "list of strings"),
            ("attr.int(default=True)", "int"),
            ("attr.bool(default=1)", "bool"),
            (
                "attr.label(cfg='bogus')",
                "cfg must be either 'target', 'exec' or a starlark defined transition defined by the exec() or transition() functions.",
            ),
            (
                "attr.label_list(cfg=1)",
                "cfg must be either 'target', 'exec'",
            ),
            (
                "attr.label(allow_files=1)",
                "in call to label(), parameter 'allow_files' got value of type 'int', want 'bool, sequence, or NoneType'",
            ),
            (
                "attr.label(allow_single_file=1)",
                "allow_single_file should be a boolean or a string list",
            ),
            (
                "attr.label(allow_single_file=[1])",
                "at index 0 of allow_files argument, got element of type int, want string",
            ),
            ("attr.string(unknown=True)", "unknown"),
            ("attr.output(default='x')", "default"),
            ("attr.output_list(default=['x'])", "default"),
            ("rule(implementation=1)", "callable"),
            (
                "rule(implementation=impl, test=True)",
                "Invalid rule class name 'x', test rule class names must end with '_test' and other rule classes must not",
            ),
            (
                "rule(implementation=impl, analysis_test=True)",
                "Invalid rule class name 'x', test rule class names must end with '_test'",
            ),
            ("rule(implementation=impl, unknown=True)", "unknown"),
            (
                "rule(implementation=impl, build_setting=True)",
                "BuildSetting, got bool",
            ),
            (
                "rule(implementation=impl, build_setting='string')",
                "BuildSetting, got string",
            ),
            (
                "rule(implementation=impl, build_setting=attr.bool())",
                "BuildSetting, got Attribute",
            ),
            (
                "rule(implementation=impl, cfg='target')",
                "`cfg` must be set to a transition object initialized by the transition() function.",
            ),
            (
                "rule(implementation=impl, build_setting=config.bool(), cfg='target')",
                "Build setting rules cannot use the `cfg` param to apply transitions to themselves.",
            ),
            (
                "rule(implementation=impl, build_setting=config.string(), attrs={'help': attr.string()})",
                "reserved for build-setting rules",
            ),
            (
                "rule(implementation=impl, build_setting=config.bool(), attrs={'build_setting_default': attr.bool()})",
                "reserved for build-setting rules",
            ),
            ("rule(implementation=impl, attrs={'x': 1})", "descriptor"),
            (
                "rule(implementation=impl, attrs={'name': attr.string()})",
                "reserved",
            ),
            (
                "rule(implementation=impl, attrs={'visibility': attr.string()})",
                "reserved",
            ),
            (
                "rule(implementation=impl, attrs={'testonly': attr.bool()})",
                "reserved",
            ),
            (
                "rule(implementation=impl, attrs={'tags': attr.string_list()})",
                "reserved",
            ),
            (
                "rule(implementation=impl, attrs={'bad-name': attr.string()})",
                "invalid attribute name",
            ),
            (
                "rule(implementation=impl, attrs={'_tool': attr.label(default=':tool', mandatory=True)})",
                "private attribute",
            ),
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains(expected),
                "{expression}: expected {expected:?}: {error}"
            );
        }
    }

    #[test]
    fn relative_defaults_and_rules_require_extension_context() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for (source, expected) in [
            ("x = attr.label(default=':relative')", "definition context"),
            (
                "x = attr.label_list(default=['relative'])",
                "definition context",
            ),
            (
                "def impl(ctx):\n    return []\nx = rule(implementation=impl)",
                "extension initialization",
            ),
        ] {
            Module::with_temp_heap(|module| {
                let ast =
                    AstModule::parse("//pkg:defs.bzl", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{error}");
            });
        }
    }

    #[test]
    fn relative_defaults_use_context_fallback_without_filesystem_lookups() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for (extension, expected) in [
            ("//:defs.bzl", "//:target"),
            ("//does/not/exist:defs.bzl", "//does/not/exist:target"),
        ] {
            Module::with_temp_heap(|module| {
                let context = crate::loading::ExtensionContext {
                    byte_strings: false,
                    label: crate::label::Label::parse(extension, "").unwrap(),
                    repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                        std::path::PathBuf::from("."),
                    )),
                };
                let ast = AstModule::parse(
                    "defs.bzl",
                    "x = attr.label(default=':target')".into(),
                    &Dialect::Standard,
                )
                .unwrap();
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals).unwrap();
                let descriptor = module
                    .get("x")
                    .unwrap()
                    .downcast_ref::<Descriptor>()
                    .unwrap();
                assert_eq!(
                    descriptor.0.default,
                    AttrValue::Label(Some(expected.into()))
                );
            });
        }
    }
}
