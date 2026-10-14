//! Owned custom-rule schemas and trace/freeze-safe Starlark definitions.
// Starlark's fixed attribute signatures also generate module-level helpers.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;

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
}
fn is_false(value: &bool) -> bool {
    !*value
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
                let bytes = if matches!(self, Self::Label | Self::LabelList) {
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
        })
    }
}

// Derives generate marker/coercion/trace implementations. Only implementation
// contains heap references; the ignored schema consists solely of owned data.
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
            write!(f, "attr.{:?}", self.0.kind)
        }
    }
    starlark_simple_value!(Descriptor);
    #[starlark_value(type = "attribute")]
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
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub build_setting: Option<BuildSetting>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub schema: BTreeMap<String, AttrSpec>,
    }
    impl<V> fmt::Display for DefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<custom rule>")
        }
    }
    starlark_complex_value!(pub(super) Definition);
    #[starlark_value(type = "rule")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
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
            let resolve = |text: &str| {
                let bytes = repositories
                    .resolution_size_bound(text, &package.repository, &package.package)
                    .map_err(anyhow::Error::msg)?;
                budget
                    .borrow_mut()
                    .bytes(bytes)
                    .map_err(anyhow::Error::msg)?;
                repositories
                    .parse(text, &package.repository, &package.package)
                    .map_err(anyhow::Error::msg)
            };
            let mut name = None;
            let mut visibility = None;
            let mut attrs = BTreeMap::new();
            for (key, value) in named.iter() {
                let key = key.as_str();
                match key {
                    "name" => {
                        name = Some(value.unpack_str().ok_or_else(|| {
                            starlark::Error::new_other(anyhow!("name must be a string"))
                        })?)
                    }
                    "visibility" => visibility = Some(*value),
                    _ => {
                        if key.starts_with('_') {
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
                                spec.kind.parse(*value, Some(&resolve))
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
            for (key, spec) in &self.schema {
                if !attrs.contains_key(key) {
                    if spec.mandatory {
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
            crate::package::register_custom(
                eval,
                name,
                visibility,
                self.implementation.to_value(),
                self.subrules.iter().map(|value| value.to_value()).collect(),
                attrs,
                self.schema.clone(),
                self.build_setting,
            )
            .map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }
}
use values::{DefinitionGen, Descriptor};

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
    }))
}

#[starlark_module]
fn attr_globals(builder: &mut GlobalsBuilder) {
    fn string(
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::String, default, mandatory, false, false)
    }
    fn string_list(
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::StringList, default, mandatory, false, false)
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
    fn int(
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::Int, default, mandatory, false, false)
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
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] allow_files: bool,
        #[starlark(require = named, default = false)] allow_single_file: bool,
        #[starlark(require = named, default = starlark::values::list::AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named, default = "target")] cfg: &str,
        #[starlark(require = named)] doc: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if cfg != "target" {
            bail!("attr.label cfg={cfg:?} is unsupported; only target is supported");
        }
        let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
        let resolve = |text: &str| {
            crate::loading::resolve_label_bounded(eval, text, &mut budget.borrow_mut())
        };
        let descriptor = descriptor_in_package(
            AttrKind::Label,
            default,
            mandatory,
            allow_files,
            allow_single_file,
            definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
        )?;
        with_required_providers(descriptor, providers)
    }
    fn label_list<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] allow_files: bool,
        #[starlark(require = named, default = starlark::values::list::AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named, default = "target")] cfg: &str,
        #[starlark(require = named)] doc: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if cfg != "target" {
            bail!("attr.label_list cfg={cfg:?} is unsupported; only target is supported");
        }
        let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
        let resolve = |text: &str| {
            crate::loading::resolve_label_bounded(eval, text, &mut budget.borrow_mut())
        };
        let descriptor = descriptor_in_package(
            AttrKind::LabelList,
            default,
            mandatory,
            allow_files,
            false,
            definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
        )?;
        with_required_providers(descriptor, providers)
    }
    fn output(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::Output, None, mandatory, false, false)
    }
    fn output_list(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::OutputList, None, mandatory, false, false)
    }
}

// The public Starlark rule signature has independently named Bazel options.
// Scope this allowance to the generated constructor and its argument wrappers.
#[allow(clippy::too_many_arguments)]
mod definitions {
    use super::*;
    #[starlark_module]
    pub(super) fn definition_globals(builder: &mut GlobalsBuilder) {
        fn rule<'v>(
            #[starlark(require = named)] implementation: Value<'v>,
            #[starlark(require = named)] attrs: Option<Value<'v>>,
            #[starlark(require = named, default = starlark::values::list::AllocList::EMPTY)]
            subrules: Value<'v>,
            #[starlark(require = named)] doc: Option<&str>,
            #[starlark(require = named, default = false)] executable: bool,
            #[starlark(require = named, default = false)] test: bool,
            #[starlark(require = named, default = NoneType)] build_setting: Value<'v>,
            #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let _ = doc;
            if definition_package(eval).is_none() {
                bail!(
                    "rule() definitions are only allowed during extension initialization, not BUILD evaluation"
                );
            }
            if executable || test {
                bail!("executable and test custom rules are unsupported");
            }
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
            if !cfg.is_none() {
                if build_setting.is_some() {
                    bail!("rule build_setting is incompatible with cfg");
                }
                bail!("rule cfg configuration transitions are unsupported");
            }
            implementation
                .check_callable_with(
                    [&starlark::typing::Ty::any()],
                    [],
                    None,
                    None,
                    &starlark::typing::Ty::any(),
                )
                .map_err(|e| anyhow!("rule implementation must be callable with ctx: {e}"))?;
            let mut schema = BTreeMap::new();
            let mut budget = crate::repository_budget::HostBudget::default();
            budget.node().map_err(anyhow::Error::msg)?;
            let subrules = crate::subrule::snapshot(subrules, &mut budget)?;
            crate::subrule::validate_exports(&subrules, &mut budget)?;
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
                    if name.starts_with('_') && spec.0.mandatory {
                        bail!(
                            "private attribute {name:?} cannot be mandatory because it cannot be set explicitly"
                        );
                    }
                    budget.string(name).map_err(anyhow::Error::msg)?;
                    budget.spec(&spec.0).map_err(anyhow::Error::msg)?;
                    schema.insert(name.to_owned(), spec.0.clone());
                }
            }
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
            Ok(eval.heap().alloc(DefinitionGen {
                implementation,
                subrules,
                schema,
                build_setting,
            }))
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
        assert!(definition_error("rule(implementation=impl, provides=[])").contains("provides"));
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
            ("attr.label(cfg='exec')", "unsupported"),
            ("attr.label_list(cfg='host')", "unsupported"),
            ("attr.label(allow_files=['.txt'])", "bool"),
            ("attr.label(allow_single_file=['.txt'])", "bool"),
            ("attr.string(unknown=True)", "unknown"),
            ("attr.output(default='x')", "default"),
            ("attr.output_list(default=['x'])", "default"),
            ("rule(implementation=1)", "callable"),
            ("rule(implementation=impl, executable=True)", "unsupported"),
            ("rule(implementation=impl, test=True)", "unsupported"),
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
                "BuildSetting, got attribute",
            ),
            (
                "rule(implementation=impl, cfg='target')",
                "configuration transitions are unsupported",
            ),
            (
                "rule(implementation=impl, build_setting=config.bool(), cfg='target')",
                "build_setting is incompatible with cfg",
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
