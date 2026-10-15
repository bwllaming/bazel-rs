//! Frozen repository-rule definitions and repository-specific attribute binding.
//! Repository execution and publication deliberately live outside this module.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::tuple::TupleRef;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use crate::custom_rule::{AttrKind, AttrSpec, AttrValue, attribute_spec, attribute_spec_ref};
use crate::extension_calls::RecordedValue;
use crate::label::Label;
use crate::loading::ExtensionContext;
use crate::repository_budget::HostBudget;
use crate::repository_environment::DeclaredEnvironment;

#[derive(Clone, Debug)]
pub struct RuleDefinition {
    pub implementation: OwnedFrozenValue,
    pub schema: BTreeMap<String, AttrSpec>,
    /// Attribute names in declaration order (the `attrs` dict order;
    /// RepoRule.Builder.addAttribute, RepoRule.java:147-152).
    pub attribute_order: Vec<String>,
    pub identity: String,
    pub local: bool,
    /// Bazel `repository_rule(configure=...)`: only selects repositories for
    /// `fetch --configure` and vendoring exclusion
    /// (RepositoryFetchFunction.java:511-527); no effect on ordinary fetches.
    pub configure: bool,
    pub environ: DeclaredEnvironment,
}

// Compile-time assertion: prepared definitions can cross worker boundaries.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RuleDefinition>();
};

// Only generated marker/coercion/trace implementations require this exception.
// All ignored fields own their data; implementation is traced and frozen.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

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
    pub(super) struct RepoDefinitionGen<V> {
        pub implementation: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub schema: BTreeMap<String, AttrSpec>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub attribute_order: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub local: bool,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub configure: bool,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub environ: DeclaredEnvironment,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub definition_label: String,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
    }

    impl<V> RepoDefinitionGen<V> {
        pub fn identity(&self) -> Option<String> {
            self.exported
                .get()
                .map(|name| format!("{}%{name}", self.definition_label))
        }
    }

    impl<V> fmt::Display for RepoDefinitionGen<V> {
        // StarlarkRepoRule.repr (bazel/repository/starlark/StarlarkRepositoryModule.java:152-159)
        // with RepoRuleId.toString, the unambiguous canonical .bzl label
        // (bazel/bzlmod/RepoRuleId.java:31-34).
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.exported.get() {
                Some(name) => {
                    let label = Label::parse_canonical(&self.definition_label)
                        .map(|label| label.canonical_starlark())
                        .unwrap_or_else(|_| self.definition_label.clone());
                    write!(f, "<starlark repository rule {label}%{name}>")
                }
                None => f.write_str("<anonymous starlark repository rule>"),
            }
        }
    }

    starlark_complex_value!(pub(super) RepoDefinition);

    #[starlark_value(type = "repository_rule")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepoDefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn export_as(&self, name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
            if self.exported.get().is_none() {
                let context = eval
                    .extra
                    .and_then(|extra| extra.downcast_ref::<ExtensionContext>());
                if context.is_none_or(|context| context.label.to_string() != self.definition_label)
                {
                    return Err(starlark::Error::new_other(anyhow!(
                        "repository rule must be exported in its defining extension {}",
                        self.definition_label
                    )));
                }
                let _ = self.exported.set(name.to_owned());
            }
            Ok(())
        }

        fn invoke(
            &self,
            me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            crate::extension_ctx::record_repository_call(me, args, eval)
                .map_err(starlark::Error::new_other)
        }
    }
}
use values::{FrozenRepoDefinition, RepoDefinitionGen};

/// Called after the execution-phase guard, before recording any arguments.
pub(crate) fn validate_invokable_value(value: Value<'_>) -> anyhow::Result<()> {
    let definition = values::RepoDefinition::from_value(value)
        .ok_or_else(|| anyhow!("expected a repository_rule definition"))?;
    if definition.identity().is_none() {
        bail!("repository_rule must have an exported name before it is called");
    }
    Ok(())
}

/// Cost estimation only: do not bind or validate callback arguments here.
/// Every invocation is charged, even when many calls share the same callee.
pub(crate) fn charge_schema(value: Value<'_>, budget: &mut HostBudget) -> Result<(), String> {
    let definition = values::RepoDefinition::from_value(value)
        .ok_or_else(|| "expected a repository_rule definition".to_owned())?;
    budget.string(&definition.definition_label)?;
    if let Some(name) = definition.exported.get() {
        budget.string(name)?;
    }
    budget.schema(&definition.schema)?;
    definition.environ.charge(budget)
}

/// Extract an exported frozen definition while retaining its implementation heap.
/// Transitive function registries belong to the loader, not this value.
pub fn unpack(value: &OwnedFrozenValue) -> Result<RuleDefinition, String> {
    let definition = value
        .value()
        .downcast_ref::<FrozenRepoDefinition>()
        .ok_or_else(|| "expected a frozen repository_rule definition".to_owned())?;
    // The ordinary recorder also charges this cumulatively before rooting calls.
    let mut budget = HostBudget::default();
    budget.schema(&definition.schema)?;
    definition.environ.charge(&mut budget)?;
    let identity = definition
        .identity()
        .ok_or_else(|| "repository_rule must have an exported name".to_owned())?;
    let implementation = value.map(|value| {
        value
            .downcast_ref::<FrozenRepoDefinition>()
            .expect("frozen definition checked above")
            .implementation
    });
    Ok(RuleDefinition {
        implementation,
        schema: definition.schema.clone(),
        attribute_order: definition.attribute_order.clone(),
        identity,
        local: definition.local,
        configure: definition.configure,
        environ: definition.environ.clone(),
    })
}

/// Install only the constructor, for shared `.bzl` initialization globals.
#[starlark_module]
pub fn globals(builder: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        implementation: Value<'v>,
        #[starlark(require = named, default = NoneType)] attrs: Value<'v>,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = AllocList::EMPTY)] environ: Value<'v>,
        #[starlark(require = named, default = false)] configure: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
            .ok_or_else(|| anyhow!(
                "repository_rule() definitions are only allowed during .bzl extension initialization, not BUILD evaluation"
            ))?;
        let mut budget = HostBudget::default();
        let string = |value: Value<'v>| {
            value.unpack_str().ok_or_else(|| {
                "repository_rule environ must be a list or tuple of strings".to_owned()
            })
        };
        let environ = if let Some(list) = ListRef::from_value(environ) {
            DeclaredEnvironment::from_names(list.iter().map(string), &mut budget)
        } else if let Some(tuple) = TupleRef::from_value(environ) {
            DeclaredEnvironment::from_names(tuple.iter().map(string), &mut budget)
        } else {
            bail!("repository_rule environ must be a list or tuple of strings");
        }
        .map_err(anyhow::Error::msg)?;
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("repository_rule doc must be a string or None");
        }
        // Unlike arbitrary callable native values, def/lambda values expose a
        // parameter spec. This also accepts implementations from frozen loads.
        if implementation.parameters_spec().is_none() {
            bail!("repository_rule implementation must be a callable Starlark function");
        }
        implementation
            .check_callable_with(
                [&starlark::typing::Ty::any()],
                [],
                None,
                None,
                &starlark::typing::Ty::any(),
            )
            .map_err(|e| {
                anyhow!("repository_rule implementation must be callable with ctx: {e}")
            })?;
        let mut schema = BTreeMap::new();
        let mut attribute_order = Vec::new();
        if !attrs.is_none() {
            let attrs = DictRef::from_value(attrs).ok_or_else(|| {
                anyhow!("repository_rule attrs must be a dict of attribute descriptors")
            })?;
            for (name, descriptor) in attrs.iter() {
                let name = name
                    .unpack_str()
                    .ok_or_else(|| anyhow!("attribute names must be strings"))?;
                if matches!(name, "name" | "tags" | "deprecation" | "visibility") {
                    bail!("repository attribute {name:?} is reserved");
                }
                if name.is_empty()
                    || !name.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                    })
                {
                    bail!("invalid repository attribute name {name:?}");
                }
                let spec = attribute_spec_ref(descriptor).ok_or_else(|| {
                    anyhow!("repository attribute {name:?} must be an attr descriptor")
                })?;
                if matches!(spec.kind, AttrKind::Output | AttrKind::OutputList) {
                    bail!("output attributes are invalid for repository rules: {name:?}");
                }
                // StarlarkRepositoryModule.java:80-90 (convertToNativeName).
                crate::custom_rule::check_value_source_name(name, spec)?;
                if spec.declaration.computed_default {
                    bail!(
                        "computed (function) default of repository attribute {name:?} is not supported by the native profile"
                    );
                }
                budget.string(name).map_err(anyhow::Error::msg)?;
                budget.spec(spec).map_err(anyhow::Error::msg)?;
                if !schema.contains_key(name) {
                    attribute_order.push(name.to_owned());
                }
                schema.insert(
                    name.to_owned(),
                    attribute_spec(descriptor).expect("descriptor checked and charged above"),
                );
            }
        }
        Ok(eval.heap().alloc(RepoDefinitionGen {
            implementation,
            schema,
            attribute_order,
            local,
            configure,
            environ,
            // The initializing extension owns the rule's export. Attribute
            // defaults independently use the innermost helper's lexical label.
            definition_label: context.label.to_string(),
            exported: OnceLock::new(),
        }))
    }
}

/// Bind recorded JSON attributes without BUILD-rule private-attribute semantics.
/// `raw` may include the original invocation's `name`; it is checked as a
/// nonempty string, then replaced by `canonical_name` in the returned attributes.
/// Nulls mean omitted, including unknown keys. Legacy metadata is ignored.
/// Repository attributes are type-checked without enforcing analysis-only non_empty.
// The caller must cumulatively preflight schema/default expansion and raw
// arguments before calling this copying binder for a declaration group.
pub fn bind_attributes(
    definition: &RuleDefinition,
    raw: &BTreeMap<String, serde_json::Value>,
    canonical_name: &str,
    resolve: impl Fn(&str) -> Result<Label, String>,
) -> Result<BTreeMap<String, AttrValue>, String> {
    let mut attrs = BTreeMap::new();
    for (name, value) in raw {
        if value.is_null() || matches!(name.as_str(), "tags" | "deprecation" | "visibility") {
            continue;
        }
        if name == "name" {
            if value.as_str().is_none_or(str::is_empty) {
                return Err("repository name must be a nonempty string".to_owned());
            }
            continue;
        }
        let spec = definition
            .schema
            .get(name)
            .ok_or_else(|| format!("unknown repository attribute {name:?}"))?;
        let parsed = parse_attribute(&spec.kind, value, &resolve).map_err(|e| {
            if matches!(
                spec.kind,
                AttrKind::StringKeyedLabelDict | AttrKind::LabelListDict
            ) && e.starts_with("expected value of type '")
            {
                // AttributeUtils.typeCheckAttrValues where-text
                // (bazel/repository/RepoRule.java:95-105).
                format!(
                    "in call to '{}' repo rule{}, {e}",
                    rule_display_name(definition),
                    with_name(raw)
                )
            } else {
                format!("repository attribute {name:?}: {e}")
            }
        })?;
        // AttributeUtils.typeCheckAttrValues allowed-values check
        // (bazel/repository/AttributeUtils.java:90-98), where-text from
        // RepoRule.instantiate (bazel/repository/RepoRule.java:95-105).
        if !spec.declaration.allows(&parsed) {
            return Err(format!(
                "in call to '{}' repo rule{}, the value for attribute '{name}' {}",
                rule_display_name(definition),
                with_name(raw),
                spec.declaration.allowed_values_reason(&parsed)
            ));
        }
        attrs.insert(name.clone(), parsed);
    }
    for (name, spec) in &definition.schema {
        if !attrs.contains_key(name) {
            if spec.mandatory {
                return Err(format!("missing mandatory repository attribute {name:?}"));
            }
            attrs.insert(name.clone(), spec.default.clone());
        }
    }
    attrs.insert(
        "name".to_owned(),
        AttrValue::String(canonical_name.to_owned()),
    );
    for (name, value) in &attrs {
        // String-valued attributes may carry runtime byte data (U+0000..U+00FF),
        // matching Bazel's Latin-1 internal strings. Executed sources are already
        // ASCII-literal-only, so such values cannot originate from UTF-8 literals.
        let representable = match value {
            AttrValue::String(value) => is_latin1(value),
            AttrValue::Strings(values) => values.iter().all(|value| is_latin1(value)),
            AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                values.iter().all(|value| value.is_ascii())
            }
            AttrValue::StringDict(entries) => entries
                .iter()
                .all(|(key, value)| is_latin1(key) && is_latin1(value)),
            AttrValue::StringListDict(entries) => entries
                .iter()
                .all(|(key, values)| is_latin1(key) && values.iter().all(|value| is_latin1(value))),
            AttrValue::Label(value) | AttrValue::Output(value) => {
                value.as_ref().is_none_or(|value| value.is_ascii())
            }
            AttrValue::Int(_) | AttrValue::Bool(_) | AttrValue::Ints(_) => true,
            AttrValue::LabelKeyedStringDict(entries) => entries
                .iter()
                .all(|(key, value)| key.is_ascii() && is_latin1(value)),
            AttrValue::StringKeyedLabelDict(entries) => entries
                .iter()
                .all(|(key, value)| is_latin1(key) && value.is_ascii()),
            AttrValue::LabelListDict(entries) => entries
                .iter()
                .all(|(key, values)| is_latin1(key) && values.iter().all(|value| value.is_ascii())),
        };
        if !name.is_ascii() || !representable {
            return Err(format!(
                "repository attribute {name:?} requires ASCII names/labels and Latin-1 \
                 (U+0000..U+00FF) string values in direct repository execution"
            ));
        }
    }
    Ok(attrs)
}

/// The exported rule name of a definition identity (`<bzl>%<name>`).
fn rule_display_name(definition: &RuleDefinition) -> &str {
    definition
        .identity
        .rsplit_once('%')
        .map_or(definition.identity.as_str(), |(_, name)| name)
}

/// RepoRule.instantiate's ` with name 'N'` (bazel/repository/RepoRule.java:95-105).
fn with_name(raw: &BTreeMap<String, serde_json::Value>) -> String {
    raw.get("name")
        .and_then(|value| value.as_str())
        .map(|name| format!(" with name '{name}'"))
        .unwrap_or_default()
}

/// Bazel represents Starlark strings internally as Latin-1 byte strings. Native
/// runtime byte data (module reads/downloads) maps each byte to U+0000..U+00FF.
pub(crate) fn is_latin1(text: &str) -> bool {
    text.chars().all(|c| u32::from(c) <= 0xFF)
}

/// Encode a Latin-1-representable native string as Bazel's internal bytes.
pub(crate) fn latin1_bytes(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// Bind call-time snapshots without erasing the distinction between genuine
/// Label values and strings. Raw strings use the completed sibling mapping;
/// genuine Labels retain their previously resolved canonical identity.
pub fn bind_recorded_attributes(
    definition: &RuleDefinition,
    raw: &BTreeMap<String, RecordedValue>,
    canonical_name: &str,
    resolve: impl Fn(&str) -> Result<Label, String>,
) -> Result<BTreeMap<String, AttrValue>, String> {
    // The ordinary recorder supplies a cumulative reservation. Also guard this
    // public host entry point before converting snapshots or expanding defaults.
    let mut budget = HostBudget::default();
    budget.schema(&definition.schema)?;
    budget.string(canonical_name)?;
    for (name, value) in raw {
        budget.string(name)?;
        budget.recorded(value, 0)?;
    }
    /// Where genuine Label values may appear in a recorded attribute value.
    #[derive(Clone, Copy)]
    struct Shape {
        label: bool,
        label_list: bool,
        /// Dictionary values: `dict(string, label)` / `dict(string, list(label))`.
        dict_values: (bool, bool),
    }
    const PLAIN: Shape = Shape {
        label: false,
        label_list: false,
        dict_values: (false, false),
    };
    fn json_value(value: &RecordedValue, shape: Shape) -> Result<serde_json::Value, String> {
        let label_allowed = shape.label;
        Ok(match value {
            RecordedValue::None => serde_json::Value::Null,
            RecordedValue::Bool(value) => (*value).into(),
            RecordedValue::Int(value) => (*value).into(),
            RecordedValue::String(value) => value.clone().into(),
            RecordedValue::Label(value) if label_allowed => {
                // Display normally omits '@@' for main-repository labels.
                // Restore it so caller-relative resolution cannot reanchor them.
                Label::parse_canonical(value)?.canonical_starlark().into()
            }
            RecordedValue::Label(_) => {
                return Err("expected an attribute-compatible value, got Label".to_owned());
            }
            RecordedValue::List(values) => serde_json::Value::Array(
                values
                    .iter()
                    .map(|value| {
                        json_value(
                            value,
                            Shape {
                                label: shape.label_list,
                                ..PLAIN
                            },
                        )
                    })
                    .collect::<Result<_, _>>()?,
            ),
            RecordedValue::Dict(values) => {
                let (label, label_list) = shape.dict_values;
                let inner = Shape {
                    label,
                    label_list,
                    ..PLAIN
                };
                serde_json::Value::Object(
                    values
                        .iter()
                        .map(|(key, value)| Ok((key.clone(), json_value(value, inner)?)))
                        .collect::<Result<_, String>>()?,
                )
            }
        })
    }

    let mut converted = BTreeMap::new();
    for (name, value) in raw {
        if matches!(value, RecordedValue::None)
            || matches!(name.as_str(), "tags" | "deprecation" | "visibility")
        {
            continue;
        }
        let kind = if name == "name" {
            &AttrKind::String
        } else {
            &definition
                .schema
                .get(name)
                .ok_or_else(|| format!("unknown repository attribute {name:?}"))?
                .kind
        };
        let value = json_value(
            value,
            Shape {
                label: matches!(kind, AttrKind::Label),
                label_list: matches!(kind, AttrKind::LabelList),
                dict_values: (
                    matches!(kind, AttrKind::StringKeyedLabelDict),
                    matches!(kind, AttrKind::LabelListDict),
                ),
            },
        )
        .map_err(|e| format!("repository attribute {name:?}: {e}"))?;
        converted.insert(name.clone(), value);
    }
    bind_attributes(definition, &converted, canonical_name, resolve)
}

fn parse_attribute(
    kind: &AttrKind,
    value: &serde_json::Value,
    resolve: &impl Fn(&str) -> Result<Label, String>,
) -> Result<AttrValue, String> {
    let string = || value.as_str().ok_or_else(|| "expected a string".to_owned());
    let strings = || {
        value
            .as_array()
            .ok_or_else(|| "expected a list of strings".to_owned())?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "expected a list of strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match kind {
        AttrKind::IntList | AttrKind::LabelKeyedStringDict => {
            return Err(format!(
                "attribute kind {kind:?} is not supported for repository rules by the native profile"
            ));
        }
        AttrKind::String => AttrValue::String(string()?.to_owned()),
        AttrKind::StringList => AttrValue::Strings(strings()?),
        AttrKind::StringDict => {
            let entries = value
                .as_object()
                .ok_or_else(|| "expected a dict of strings to strings".to_owned())?;
            if !entries.values().all(|value| value.as_str().is_some()) {
                return Err("expected dict values to be strings".to_owned());
            }
            AttrValue::StringDict(
                entries
                    .iter()
                    .map(|(key, value)| {
                        let value = value
                            .as_str()
                            .ok_or_else(|| "expected dict values to be strings".to_owned())?;
                        Ok((key.clone(), value.to_owned()))
                    })
                    .collect::<Result<_, String>>()?,
            )
        }
        AttrKind::StringListDict => AttrValue::StringListDict(
            value
                .as_object()
                .ok_or_else(|| "expected a dict of string lists".to_owned())?
                .iter()
                .map(|(key, value)| {
                    // Transport normalizes Starlark lists and tuples to JSON arrays.
                    let values = value
                        .as_array()
                        .ok_or_else(|| format!("dict key {key:?}: expected a list of strings"))?
                        .iter()
                        .map(|value| {
                            value.as_str().map(str::to_owned).ok_or_else(|| {
                                format!("dict key {key:?}: expected a list of strings")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok((key.clone(), values))
                })
                .collect::<Result<_, String>>()?,
        ),
        AttrKind::Int => AttrValue::Int(
            value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| "expected a 32-bit int".to_owned())?,
        ),
        AttrKind::Bool => AttrValue::Bool(
            value
                .as_bool()
                .ok_or_else(|| "expected a bool".to_owned())?,
        ),
        AttrKind::Label => AttrValue::Label(Some(resolve(string()?)?.to_string())),
        AttrKind::LabelList => AttrValue::Labels(
            strings()?
                .iter()
                .map(|text| resolve(text).map(|label| label.to_string()))
                .collect::<Result<_, _>>()?,
        ),
        AttrKind::StringKeyedLabelDict | AttrKind::LabelListDict => {
            label_dict_json(kind, value, resolve)?
        }
        AttrKind::Output | AttrKind::OutputList => {
            return Err("output attributes are invalid for repository rules".to_owned());
        }
    })
}

/// Bind a JSON-transported `dict(string, label)` / `dict(string, list(label))`
/// value (DictType.convert, packages/Type.java:565-580): each label string is
/// converted with the binding's repository mapping (canonical `@@` forms of
/// genuine Label values keep their identity). Shared with module extension
/// tags. Type failures use Type.ConversionException's text
/// (packages/Type.java:296-321); callers add the where-text.
pub(crate) fn label_dict_json(
    kind: &AttrKind,
    value: &serde_json::Value,
    resolve: &impl Fn(&str) -> Result<Label, String>,
) -> Result<AttrValue, String> {
    let lists = matches!(kind, AttrKind::LabelListDict);
    let ty = if lists {
        "dict(string, list(label))"
    } else {
        "dict(string, label)"
    };
    let entries = value
        .as_object()
        .ok_or_else(|| conversion_error(ty, value, "attribute value"))?;
    let label = |value: &serde_json::Value, what: &str| -> Result<String, String> {
        let text = value
            .as_str()
            .ok_or_else(|| conversion_error("string", value, what))?;
        resolve(text).map(|label| label.to_string())
    };
    if lists {
        let mut bound = Vec::new();
        for (key, values) in entries {
            let items = values
                .as_array()
                .ok_or_else(|| conversion_error("list(label)", values, "dict value element"))?;
            let labels = items
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    label(value, &format!("element {index} of dict value element"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            bound.push((key.clone(), labels));
        }
        Ok(AttrValue::LabelListDict(bound))
    } else {
        let mut bound = Vec::new();
        for (key, value) in entries {
            bound.push((key.clone(), label(value, "dict value element")?));
        }
        Ok(AttrValue::StringKeyedLabelDict(bound))
    }
}

/// `expected value of type 'T' for WHAT, but got REPR (TYPE)` of a
/// JSON-transported Starlark value (Type.ConversionException, packages/Type.java:296-321).
fn conversion_error(ty: &str, value: &serde_json::Value, what: &str) -> String {
    let (repr, type_name) = starlark_repr(value);
    format!("expected value of type '{ty}' for {what}, but got {repr} ({type_name})")
}

/// Starlark `repr` and `type` of a transported attribute value.
fn starlark_repr(value: &serde_json::Value) -> (String, &'static str) {
    use serde_json::Value as Json;
    match value {
        Json::Null => ("None".to_owned(), "NoneType"),
        Json::Bool(flag) => (if *flag { "True" } else { "False" }.to_owned(), "bool"),
        Json::Number(number) => (number.to_string(), "int"),
        Json::String(text) => (
            serde_json::to_string(text).unwrap_or_else(|_| format!("{text:?}")),
            "string",
        ),
        Json::Array(items) => (
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| starlark_repr(item).0)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "list",
        ),
        Json::Object(entries) => (
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!(
                        "{}: {}",
                        starlark_repr(&Json::String(key.clone())).0,
                        starlark_repr(item).0
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "dict",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use starlark::environment::{FrozenModule, Globals, Module};
    use starlark::eval::ReturnFileLoader;
    use starlark::syntax::{AstModule, Dialect};
    use std::collections::HashMap;

    fn test_globals() -> Globals {
        GlobalsBuilder::standard()
            .with(crate::custom_rule::rule_globals)
            .with(globals)
            .build()
    }

    fn context(label: &str) -> ExtensionContext {
        ExtensionContext {
            byte_strings: false,
            label: Label::parse_canonical(label).unwrap(),
            repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                std::path::PathBuf::from("."),
            )),
        }
    }

    fn extension_in(
        label: &str,
        source: &str,
        loads: &HashMap<&str, &FrozenModule>,
    ) -> Result<FrozenModule, String> {
        Module::with_temp_heap(|module| {
            let context = context(label);
            let loader = ReturnFileLoader { modules: loads };
            let ast = AstModule::parse(label, source.to_owned(), &Dialect::Standard)
                .map_err(|e| e.to_string())?;
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.set_loader(&loader);
                eval.eval_module(ast, &test_globals())
                    .map_err(|e| e.to_string())?;
            }
            module.freeze().map_err(|e| format!("{e:?}"))
        })
    }

    fn extension(source: &str) -> Result<FrozenModule, String> {
        extension_in("//defs:repos.bzl", source, &HashMap::new())
    }

    fn definition(attrs: &str) -> RuleDefinition {
        let module = extension(&format!(
            "def impl(ctx):\n    return None\nR = repository_rule(impl, attrs={attrs})"
        ))
        .unwrap();
        unpack(&module.get("R").unwrap()).unwrap()
    }

    #[test]
    fn repository_rule_reprs_follow_repository_rule_function() {
        // StarlarkRepositoryModule.java:153-159 with RepoRuleId.java:32-34.
        extension(
            "def impl(ctx):\n    return None\nanon = repr(repository_rule(impl))\nR = repository_rule(impl)\n(anon == '<anonymous starlark repository rule>') or fail(anon)\n(repr(R) == '<starlark repository rule @@//defs:repos.bzl%R>' and str(R) == repr(R)) or fail(repr(R))",
        )
        .unwrap();
        extension_in(
            "@@dep+//defs:repos.bzl",
            "def impl(ctx):\n    return None\nR = repository_rule(impl)\n(repr(R) == '<starlark repository rule @@dep+//defs:repos.bzl%R>') or fail(repr(R))",
            &HashMap::new(),
        )
        .unwrap();
    }

    #[test]
    fn supplied_values_are_checked_against_values_like_attribute_utils() {
        let definition = definition(
            "{'mode': attr.string(default = 'z', values = ['a', 'b']), 'n': attr.int(values = [1, 2])}",
        );
        // Defaults are never checked; only supplied values (AttributeUtils.java:90-98).
        let bound = bind_attributes(&definition, &raw(json!({"name": "x"})), "repo", |_| {
            panic!("not a label")
        })
        .unwrap();
        assert_eq!(bound["mode"], AttrValue::String("z".to_owned()));
        for (value, expected) in [
            (
                json!({"name": "x", "mode": "c"}),
                "in call to 'R' repo rule with name 'x', the value for attribute 'mode' has to be one of 'a' or 'b' instead of 'c'",
            ),
            (
                json!({"n": 3}),
                "in call to 'R' repo rule, the value for attribute 'n' has to be one of '1' or '2' instead of '3'",
            ),
        ] {
            let error =
                bind_attributes(&definition, &raw(value), "repo", |_| panic!("not a label"))
                    .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    fn raw(value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        value
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn bind(
        definition: &RuleDefinition,
        value: serde_json::Value,
    ) -> Result<BTreeMap<String, AttrValue>, String> {
        bind_attributes(definition, &raw(value), "canonical+repo", |text| {
            Label::parse(text, "caller")
        })
    }

    #[test]
    fn string_list_dict_defaults_are_owned_ordered_and_serialize_flags_only_when_true() {
        let module = extension(
            "items = ['before', 'before']\nd = {'z': items, 'a': ()}\nspec = attr.string_list_dict(default=d, allow_empty=False)\nattrs = {'d': spec, 'old': attr.string()}\ndef impl(ctx):\n    return ctx\nR = repository_rule(impl, attrs=attrs)\nitems.append('after')\nd['later'] = []\nattrs['extra'] = attr.int()\n(d['z'] == ['before', 'before', 'after']) or fail('source mutated')",
        ).unwrap();
        let definition = unpack(&module.get("R").unwrap()).unwrap();
        drop(module);
        let expected = AttrValue::StringListDict(vec![
            (
                "z".to_owned(),
                vec!["before".to_owned(), "before".to_owned()],
            ),
            ("a".to_owned(), vec![]),
        ]);
        assert_eq!(definition.schema.len(), 2);
        assert_eq!(definition.schema["d"].kind, AttrKind::StringListDict);
        assert!(definition.schema["d"].non_empty);
        assert_eq!(definition.schema["d"].default, expected);
        let mut bound = bind(&definition, json!({"d":null})).unwrap();
        assert_eq!(bound["d"], expected);
        if let AttrValue::StringListDict(entries) = bound.get_mut("d").unwrap() {
            entries[0].1.push("bound-only".to_owned());
        }
        assert_eq!(definition.schema["d"].default, expected);
        assert_eq!(bind(&definition, json!({})).unwrap()["d"], expected);
        let serialized = serde_json::to_value(&definition.schema["d"]).unwrap();
        assert_eq!(serialized["non_empty"], true);
        assert_eq!(
            serialized["default"],
            json!({"StringListDict":[["z",["before","before"]],["a",[]]]})
        );
        assert_eq!(
            serde_json::to_value(&definition.schema["old"]).unwrap(),
            json!({
                "kind":"String", "default":{"String":""}, "mandatory":false,
                "allow_files":false, "allow_single_file":false
            })
        );
        Module::with_temp_heap(|module| {
            let implementation = module
                .heap()
                .access_owned_frozen_value(&definition.implementation);
            let result = Evaluator::new(&module)
                .eval_function(implementation, &[module.heap().alloc(42)], &[])
                .unwrap();
            assert_eq!(result.unpack_i32(), Some(42));
        });
    }

    #[test]
    fn string_dict_repository_defaults_survive_mutation_and_owner_drop() {
        let module = extension("d={'z':'before', 'a':'alpha'}\ns=attr.string_dict(default=d, allow_empty=False)\nd['z']='between'\ndef impl(ctx):\n    return ctx\nR=repository_rule(impl, attrs={'d':s})\nd['z']='after'\nd['new']='later'").unwrap();
        let definition = unpack(&module.get("R").unwrap()).unwrap();
        drop(module);
        let expected = AttrValue::StringDict(vec![
            ("z".into(), "before".into()),
            ("a".into(), "alpha".into()),
        ]);
        assert_eq!(definition.schema["d"].kind, AttrKind::StringDict);
        assert!(definition.schema["d"].non_empty);
        assert_eq!(definition.schema["d"].default, expected);
        assert_eq!(bind(&definition, json!({})).unwrap()["d"], expected);
        assert_eq!(bind(&definition, json!({"d":null})).unwrap()["d"], expected);
        assert_eq!(
            serde_json::to_value(&definition.schema["d"]).unwrap()["default"],
            json!({"StringDict":[["z","before"],["a","alpha"]]})
        );
    }

    #[test]
    fn string_dict_repository_binding_order_none_mandatory_nonempty_and_ascii() {
        let definition = definition("{'d':attr.string_dict(allow_empty=False)}");
        let supplied = raw(json!({"d":{"z":"@missing//p:t", "a":""}}));
        assert_eq!(
            bind_attributes(&definition, &supplied, "repo", |_| panic!("not a label")).unwrap()["d"],
            AttrValue::StringDict(vec![
                ("z".into(), "@missing//p:t".into()),
                ("a".into(), "".into())
            ])
        );
        for value in [json!({}), json!({"d":null}), json!({"d":{}})] {
            assert_eq!(
                bind(&definition, value).unwrap()["d"],
                AttrValue::StringDict(vec![])
            );
        }
        let required = self::definition(
            "{'d':attr.string_dict(default={'z':'fallback'}, mandatory=True, allow_empty=False)}",
        );
        for value in [json!({}), json!({"d":null})] {
            assert!(
                bind(&required, value)
                    .unwrap_err()
                    .contains("missing mandatory repository attribute")
            );
        }
        assert_eq!(
            bind(&required, json!({"d":{}})).unwrap()["d"],
            AttrValue::StringDict(vec![])
        );
        for value in [
            json!([]),
            json!("text"),
            json!({"k":[]}),
            json!({"k":1}),
            json!({"k":false}),
            json!({"k":null}),
            json!({"k":{}}),
        ] {
            assert!(
                bind(&definition, json!({"d":value}))
                    .unwrap_err()
                    .contains("expected")
            );
        }
        for value in [json!({"Ā":"ok"}), json!({"k":"Ā"})] {
            assert!(
                bind(&definition, json!({"d":value}))
                    .unwrap_err()
                    .contains("Latin-1 (U+0000..U+00FF) string values")
            );
        }
        for default in ["{'Ā':'ok'}", "{'k':'Ā'}"] {
            let definition =
                self::definition(&format!("{{'d':attr.string_dict(default={default})}}"));
            assert!(
                bind(&definition, json!({}))
                    .unwrap_err()
                    .contains("Latin-1 (U+0000..U+00FF) string values")
            );
        }
    }

    #[test]
    fn string_dict_recorded_order_preserves_literals_but_rejects_labels() {
        let definition = definition("{'d':attr.string_dict()}");
        let raw = BTreeMap::from([(
            "d".into(),
            RecordedValue::Dict(vec![
                ("z".into(), RecordedValue::String("@missing//p:t".into())),
                ("a".into(), RecordedValue::String("".into())),
            ]),
        )]);
        assert_eq!(
            bind_recorded_attributes(&definition, &raw, "repo", |_| panic!("not a label")).unwrap()
                ["d"],
            AttrValue::StringDict(vec![
                ("z".into(), "@missing//p:t".into()),
                ("a".into(), "".into())
            ])
        );
        for value in [
            RecordedValue::Label("//:target".into()),
            RecordedValue::List(vec![]),
        ] {
            let raw =
                BTreeMap::from([("d".into(), RecordedValue::Dict(vec![("k".into(), value)]))]);
            assert!(
                bind_recorded_attributes(&definition, &raw, "repo", |_| panic!("not a label"))
                    .is_err()
            );
        }
        for (key, value) in [("Ā", "ok"), ("k", "Ā")] {
            let raw = BTreeMap::from([(
                "d".into(),
                RecordedValue::Dict(vec![(key.into(), RecordedValue::String(value.into()))]),
            )]);
            assert!(
                bind_recorded_attributes(&definition, &raw, "repo", |_| panic!("not a label"))
                    .unwrap_err()
                    .contains("Latin-1")
            );
        }
    }

    #[test]
    fn string_list_dict_binding_preserves_order_and_ignores_non_empty() {
        let definition = definition("{'d':attr.string_list_dict(allow_empty=False)}");
        let raw = json!({"d":{"z":["same","same"],"a":[]}});
        let original = raw.clone();
        assert_eq!(
            bind(&definition, raw.clone()).unwrap()["d"],
            AttrValue::StringListDict(vec![
                ("z".to_owned(), vec!["same".to_owned(), "same".to_owned()]),
                ("a".to_owned(), vec![]),
            ])
        );
        assert_eq!(raw, original);
        for value in [json!({}), json!({"d":null}), json!({"d":{}})] {
            assert_eq!(
                bind(&definition, value).unwrap()["d"],
                AttrValue::StringListDict(vec![])
            );
        }
        let required = self::definition(
            "{'d':attr.string_list_dict(default={'z':['fallback']}, mandatory=True, allow_empty=False)}",
        );
        for value in [json!({}), json!({"d":null})] {
            assert!(
                bind(&required, value)
                    .unwrap_err()
                    .contains("missing mandatory repository attribute \"d\"")
            );
        }
        assert_eq!(
            bind(&required, json!({"d":{}})).unwrap()["d"],
            AttrValue::StringListDict(vec![])
        );
    }

    #[test]
    fn string_list_dict_rejects_invalid_shapes_and_unicode_without_partial_binding() {
        let definition = definition("{'a':attr.string_list_dict(), 'z':attr.string_list_dict()}");
        for value in [
            json!([]),
            json!("text"),
            json!(1),
            json!({"k":"text"}),
            json!({"k":1}),
            json!({"k":null}),
            json!({"k":{}}),
            json!({"k":[1]}),
            json!({"k":[true]}),
            json!({"k":[null]}),
            json!({"k":[[]]}),
            json!({"k":[{}]}),
        ] {
            let supplied = raw(json!({"a":{"ok":["first"]}, "z":value}));
            let original = supplied.clone();
            let error = bind_attributes(&definition, &supplied, "repo", |_| panic!("not a label"))
                .unwrap_err();
            assert!(
                error.contains("repository attribute \"z\": expected")
                    || error.contains("repository attribute \"z\": dict key"),
                "{error}"
            );
            assert_eq!(supplied, original);
            assert_eq!(
                definition.schema["a"].default,
                AttrValue::StringListDict(vec![])
            );
        }
        for value in [json!({"Ā":[]}), json!({"k":["ascii","Ā"]})] {
            assert!(
                bind(&definition, json!({"z":value}))
                    .unwrap_err()
                    .contains("repository attribute \"z\" requires")
            );
        }
        for default in ["{'Ā':[]}", "{'k':('ascii', 'Ā')}"] {
            let definition =
                self::definition(&format!("{{'d':attr.string_list_dict(default={default})}}"));
            assert!(
                bind(&definition, json!({}))
                    .unwrap_err()
                    .contains("repository attribute \"d\" requires")
            );
        }
    }

    #[test]
    fn recorded_string_list_dict_preserves_normalized_sequences_and_rejects_labels() {
        let definition = definition("{'d':attr.string_list_dict()}");
        // Both list and tuple input arrive as RecordedValue::List from transport.
        let value = RecordedValue::Dict(vec![
            (
                "z".to_owned(),
                RecordedValue::List(vec![
                    RecordedValue::String("same".to_owned()),
                    RecordedValue::String("same".to_owned()),
                ]),
            ),
            ("a".to_owned(), RecordedValue::List(vec![])),
        ]);
        let raw = BTreeMap::from([("d".to_owned(), value)]);
        let original = raw.clone();
        let bound =
            bind_recorded_attributes(&definition, &raw, "repo", |_| panic!("not a label")).unwrap();
        assert_eq!(
            bound["d"],
            AttrValue::StringListDict(vec![
                ("z".to_owned(), vec!["same".to_owned(), "same".to_owned()]),
                ("a".to_owned(), vec![]),
            ])
        );
        assert_eq!(raw, original);
        for value in [
            RecordedValue::Label("//:target".to_owned()),
            RecordedValue::List(vec![RecordedValue::Label("//:target".to_owned())]),
        ] {
            let raw = BTreeMap::from([(
                "d".to_owned(),
                RecordedValue::Dict(vec![("k".to_owned(), value)]),
            )]);
            let error = bind_recorded_attributes(&definition, &raw, "repo", |_| {
                panic!("must reject genuine Label")
            })
            .unwrap_err();
            assert!(
                error.contains("repository attribute \"d\"") && error.contains("got Label"),
                "{error}"
            );
        }
        for (key, value) in [("Ā", "ascii"), ("ascii", "Ā")] {
            let raw = BTreeMap::from([(
                "d".to_owned(),
                RecordedValue::Dict(vec![(
                    key.to_owned(),
                    RecordedValue::List(vec![RecordedValue::String(value.to_owned())]),
                )]),
            )]);
            assert!(
                bind_recorded_attributes(&definition, &raw, "repo", |_| panic!("not a label"))
                    .unwrap_err()
                    .contains("repository attribute \"d\" requires")
            );
        }
    }

    #[test]
    fn repeated_descriptor_defaults_are_bounded_before_schema_clones() {
        for descriptor in [
            "attr.string(default='x' * 1048576)",
            "attr.string_list_dict(default={'z':['x' * 1048576], 'a':[]})",
            "attr.string_dict(default={'z':'x' * 1048576, 'a':''})",
        ] {
            let error = extension(&format!("def impl(ctx):\n    return None\nd = {descriptor}\nR = repository_rule(impl, attrs={{'a' + str(i):d for i in range(600)}})")).unwrap_err();
            assert!(
                error.contains("host preparation weighted byte limit"),
                "{error}"
            );
        }
    }

    #[test]
    fn execution_boundary_rejects_host_supplied_or_default_unicode_attributes() {
        let strings = definition("{'text':attr.string(), 'items':attr.string_list()}");
        for supplied in [json!({"text":"Ā"}), json!({"items":["ascii","Ā"]})] {
            assert!(
                bind(&strings, supplied)
                    .unwrap_err()
                    .contains("Latin-1 (U+0000..U+00FF) string values")
            );
        }
        // Definitions loaded through ordinary APIs can bypass the dedicated
        // source profile. The binding boundary still checks their defaults.
        let unicode_default = definition("{'text':attr.string(default='Ā')}");
        assert!(
            bind(&unicode_default, json!({}))
                .unwrap_err()
                .contains("Latin-1 (U+0000..U+00FF) string values")
        );
        assert!(bind(&strings, json!({"text":"hello", "items":["one"]})).is_ok());
    }

    #[test]
    fn definitions_freeze_with_first_export_identity_and_distinct_objects() {
        let module = extension(
            "def impl(ctx):\n    return None\nR = repository_rule(impl)\nAlias = R\nOther = repository_rule(implementation=impl, attrs=None, local=False, environ=[], configure=False, doc=None)\n(type(R) == 'repository_rule') or fail('type')\n(R == Alias and R != Other) or fail('identity')"
        ).unwrap();
        let r = unpack(&module.get("R").unwrap()).unwrap();
        let alias = unpack(&module.get("Alias").unwrap()).unwrap();
        let other = unpack(&module.get("Other").unwrap()).unwrap();
        assert_eq!(r.identity, "//defs:repos.bzl%R");
        assert_eq!(alias.identity, r.identity);
        assert_ne!(other.identity, r.identity);
        assert!(!r.local);
        assert!(!alias.local);
        assert!(!other.local);
        assert!(
            r.implementation
                .value()
                .ptr_eq(other.implementation.value())
        );
    }

    #[test]
    fn environ_declarations_copy_sort_deduplicate_and_freeze_without_becoming_attrs() {
        let module = extension(
            "def impl(ctx):\n    return None\nnames = ['Z', 'A', 'Z', '', '=', 'A=B', 'A\\x00B', '?']\nR = repository_rule(impl, environ=names)\nnames.append('LATER')\nnames[0] = 'CHANGED'\nTuple = repository_rule(impl, environ=('B', 'A', 'B'))\nEmpty = repository_rule(impl, environ=())"
        ).unwrap();
        let r = unpack(&module.get("R").unwrap()).unwrap();
        assert_eq!(r.environ.names(), ["", "=", "?", "A", "A\0B", "A=B", "Z"]);
        assert!(r.schema.is_empty());
        assert!(!bind(&r, json!({})).unwrap().contains_key("environ"));
        assert_eq!(
            unpack(&module.get("Tuple").unwrap())
                .unwrap()
                .environ
                .names(),
            ["A", "B"]
        );
        assert!(
            unpack(&module.get("Empty").unwrap())
                .unwrap()
                .environ
                .is_empty()
        );
        let owned = r.clone();
        drop(module);
        assert_eq!(owned.environ.names(), r.environ.names());
    }

    #[test]
    fn duplicate_environment_declarations_are_charged_before_deduplication() {
        let error = extension("def impl(ctx):\n    return None\nR = repository_rule(impl, environ=['SYNTHETIC'] * 100001)").unwrap_err();
        assert!(error.contains("host preparation"), "{error}");
        for expression in ["['é']", "['x' * 4097]", "[True]", "[None]", "{'A': 1}"] {
            assert!(extension(&format!("def impl(ctx):\n    return None\nR = repository_rule(impl, environ={expression})")).is_err());
        }
    }

    #[test]
    fn reexport_preserves_original_identity() {
        let original =
            extension("def impl(ctx):\n    return None\nR = repository_rule(impl)").unwrap();
        let reexport = extension_in(
            "//other:alias.bzl",
            "load('//defs:repos.bzl', 'R')\nRenamed = R",
            &HashMap::from([("//defs:repos.bzl", &original)]),
        )
        .unwrap();
        let value = reexport.get("Renamed").unwrap();
        drop(original);
        drop(reexport);
        assert_eq!(unpack(&value).unwrap().identity, "//defs:repos.bzl%R");
    }

    #[test]
    fn local_metadata_survives_alias_reexport_clone_and_implementation_owner_drop() {
        for (option, local) in [
            ("", false),
            (", local=False", false),
            (", local=True", true),
        ] {
            let helper = extension_in(
                "//helper:helper.bzl",
                "def _private(x):\n    return x + 1\ndef run(x):\n    return _private(x)\nImplementationAlias = run",
                &HashMap::new(),
            )
            .unwrap();
            let original = extension_in(
                "//defs:repos.bzl",
                &format!(
                    "load('//helper:helper.bzl', 'ImplementationAlias')\nR = repository_rule(ImplementationAlias, attrs={{'text': attr.string(default='kept')}}{option})\nAlias = R"
                ),
                &HashMap::from([("//helper:helper.bzl", &helper)]),
            )
            .unwrap();
            let reexport = extension_in(
                "//other:alias.bzl",
                "load('//defs:repos.bzl', 'Alias')\nRenamed = Alias",
                &HashMap::from([("//defs:repos.bzl", &original)]),
            )
            .unwrap();
            for name in ["R", "Alias"] {
                assert_eq!(unpack(&original.get(name).unwrap()).unwrap().local, local);
                assert!(!unpack(&original.get(name).unwrap()).unwrap().configure);
            }
            let value = reexport.get("Renamed").unwrap();
            drop(helper);
            drop(original);
            drop(reexport);
            let definition = unpack(&value).unwrap();
            drop(value);
            let cloned = definition.clone();
            drop(definition);
            assert_eq!(cloned.local, local);
            assert_eq!(cloned.identity, "//defs:repos.bzl%R");
            let attrs = bind(&cloned, json!({})).unwrap();
            assert_eq!(attrs["text"], AttrValue::String("kept".to_owned()));
            assert!(!attrs.contains_key("local"));
            assert_eq!(cloned.schema.len(), 1);
            Module::with_temp_heap(|module| {
                let implementation = module
                    .heap()
                    .access_owned_frozen_value(&cloned.implementation);
                drop(cloned);
                let result = Evaluator::new(&module)
                    .eval_function(implementation, &[module.heap().alloc(41)], &[])
                    .unwrap();
                assert_eq!(result.unpack_i32(), Some(42));
            });
        }
    }

    #[test]
    fn implementation_keeps_private_transitive_helpers_alive() {
        let helper = extension_in(
            "//helper:helper.bzl",
            "def _private(x):\n    return x + 1\ndef run(x):\n    return _private(x)",
            &HashMap::new(),
        )
        .unwrap();
        let module = extension_in(
            "//defs:repos.bzl",
            "load('//helper:helper.bzl', 'run')\ndef _impl(ctx):\n    return run(ctx)\nR = repository_rule(_impl)",
            &HashMap::from([("//helper:helper.bzl", &helper)]),
        ).unwrap();
        let owned = module.get("R").unwrap();
        let definition = unpack(&owned).unwrap();
        drop(owned);
        drop(module);
        drop(helper);
        Module::with_temp_heap(|module| {
            let implementation = module
                .heap()
                .access_owned_frozen_value(&definition.implementation);
            let result = Evaluator::new(&module)
                .eval_function(implementation, &[module.heap().alloc(41)], &[])
                .unwrap();
            assert_eq!(result.unpack_i32(), Some(42));
        });
    }

    #[test]
    fn factory_export_owner_differs_from_lexical_default_owner() {
        let helper = extension_in(
            "//helper:helper.bzl",
            "def impl(ctx):\n    return None\ndef make():\n    return repository_rule(impl, attrs={'tool': attr.label(default=':tool')})",
            &HashMap::new(),
        ).unwrap();
        let module = extension_in(
            "//defs:repos.bzl",
            "load('//helper:helper.bzl', 'make')\nR = make()",
            &HashMap::from([("//helper:helper.bzl", &helper)]),
        )
        .unwrap();
        let definition = unpack(&module.get("R").unwrap()).unwrap();
        assert_eq!(definition.identity, "//defs:repos.bzl%R");
        assert_eq!(
            definition.schema["tool"].default,
            AttrValue::Label(Some("//helper:tool".to_owned()))
        );
    }

    #[test]
    fn unpack_rejects_non_rules_and_unexported_rules() {
        let module =
            extension("def impl(ctx):\n    return None\nx = 1\nR = rule(implementation=impl)")
                .unwrap();
        for name in ["x", "R", "impl"] {
            assert!(
                unpack(&module.get(name).unwrap())
                    .unwrap_err()
                    .contains("repository_rule")
            );
        }
        let unexported = Module::with_temp_heap(|module| {
            let context = context("//defs:repos.bzl");
            let ast = AstModule::parse(
                "//defs:repos.bzl",
                "def impl(ctx):\n    return None\nrepository_rule(impl)".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let result = eval.eval_module(ast, &test_globals()).unwrap();
                // Native set deliberately does not run Starlark export_as.
                module.set("Hidden", result);
            }
            module.freeze().unwrap().get("Hidden").unwrap()
        });
        assert!(unpack(&unexported).unwrap_err().contains("exported name"));
        assert!(
            validate_invokable_value(unexported.value())
                .unwrap_err()
                .to_string()
                .contains("exported name")
        );
    }

    #[test]
    fn definitions_require_extension_context_and_invocations_never_create_repos() {
        for filename in ["BUILD", "other.star"] {
            Module::with_temp_heap(|module| {
                let ast = AstModule::parse(
                    filename,
                    "def impl(ctx):\n    return None\nR = repository_rule(impl)".to_owned(),
                    &Dialect::Standard,
                )
                .unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("extension initialization"), "{error}");
            });
        }
        let error = extension(
            "def impl(ctx):\n    fail('must not run')\nR = repository_rule(impl)\nR(name='repo')",
        )
        .unwrap_err();
        assert!(
            error.contains("unsupported outside recorded MODULE/direct execution"),
            "{error}"
        );
        let definition =
            extension("def impl(ctx):\n    fail('must not run')\nR = repository_rule(impl)")
                .unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definition);
            let ast =
                AstModule::parse("BUILD", "R(name='repo')".to_owned(), &Dialect::Standard).unwrap();
            let error = Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unsupported outside recorded MODULE/direct execution"),
                "{error}"
            );
        });
    }

    #[test]
    fn configure_flag_is_retained_metadata_through_aliases_and_reexports() {
        for (option, configure) in [
            ("", false),
            (", configure=False", false),
            (", configure=True", true),
        ] {
            let original = extension_in(
                "//defs:repos.bzl",
                &format!("def impl(ctx):\n    return None\nR = repository_rule(impl, environ=['CC']{option})\nAlias = R"),
                &HashMap::new(),
            )
            .unwrap();
            let reexport = extension_in(
                "//other:alias.bzl",
                "load('//defs:repos.bzl', 'Alias')\nRenamed = Alias",
                &HashMap::from([("//defs:repos.bzl", &original)]),
            )
            .unwrap();
            for name in ["R", "Alias"] {
                let definition = unpack(&original.get(name).unwrap()).unwrap();
                assert_eq!(definition.configure, configure);
                assert!(!definition.local);
            }
            let renamed = unpack(&reexport.get("Renamed").unwrap()).unwrap();
            assert_eq!(renamed.clone().configure, configure);
            assert_eq!(renamed.identity, "//defs:repos.bzl%R");
        }
    }

    #[test]
    fn constructor_rejects_unsupported_options_and_bad_signatures() {
        for (expression, expected) in [
            ("repository_rule(impl, local=1)", "bool"),
            ("repository_rule(impl, local=0)", "bool"),
            ("repository_rule(impl, local=None)", "bool"),
            ("repository_rule(impl, local='True')", "bool"),
            ("repository_rule(impl, configure=1)", "bool"),
            ("repository_rule(impl, configure=None)", "bool"),
            (
                "repository_rule(impl, environ=None)",
                "list or tuple of strings",
            ),
            (
                "repository_rule(impl, environ=[1])",
                "list or tuple of strings",
            ),
            (
                "repository_rule(impl, environ='HOME')",
                "list or tuple of strings",
            ),
            ("repository_rule(impl, remotable=False)", "remotable"),
            ("repository_rule(impl, unknown=False)", "unknown"),
            ("repository_rule(impl, {})", "positional"),
            ("repository_rule(1)", "Starlark function"),
            ("repository_rule(len)", "Starlark function"),
            ("repository_rule(impl, doc=1)", "doc"),
        ] {
            let error = extension(&format!(
                "def impl(ctx):\n    return None\nR = {expression}"
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[test]
    fn schemas_reject_reserved_names_outputs_and_non_descriptors() {
        for (attrs, expected) in [
            ("[]", "dict"),
            ("{'x': 1}", "descriptor"),
            ("{1: attr.string()}", "strings"),
            ("{'name': attr.string()}", "reserved"),
            ("{'tags': attr.string_list()}", "reserved"),
            ("{'deprecation': attr.string()}", "reserved"),
            ("{'visibility': attr.string_list()}", "reserved"),
            ("{'bad-name': attr.string()}", "invalid"),
            ("{'é': attr.string()}", "invalid"),
            ("{'1bad': attr.string()}", "invalid"),
            ("{'': attr.string()}", "invalid"),
            ("{'out': attr.output()}", "output attributes"),
            ("{'out': attr.output_list()}", "output attributes"),
        ] {
            let error = extension(&format!(
                "def impl(ctx):\n    return None\nR = repository_rule(impl, attrs={attrs})"
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{attrs}: {error}");
        }
    }

    #[test]
    fn binder_ignores_null_unknowns_and_legacy_metadata_but_not_repo_mapping() {
        let definition = definition("{'x': attr.string(default='default')}");
        let attrs = bind(&definition, json!({"name": "original", "x": null, "unknown": null, "repo_mapping": null, "tags": 42, "deprecation": {}, "visibility": false})).unwrap();
        assert_eq!(
            attrs,
            BTreeMap::from([
                (
                    "name".to_owned(),
                    AttrValue::String("canonical+repo".to_owned())
                ),
                ("x".to_owned(), AttrValue::String("default".to_owned())),
            ])
        );
        assert!(
            bind(&definition, json!({"unknown": false}))
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            bind(&definition, json!({"repo_mapping": {}}))
                .unwrap_err()
                .contains("repo_mapping")
        );
        assert!(bind(&definition, json!({"name": null})).is_ok());
        for name in [json!(1), json!(""), json!([])] {
            assert!(
                bind(&definition, json!({"name": name}))
                    .unwrap_err()
                    .contains("nonempty string")
            );
        }
    }

    #[test]
    fn binder_allows_private_overrides_and_checks_mandatory_before_defaults() {
        let definition = definition(
            "{'_private': attr.string(default='fallback', mandatory=True), 'required': attr.int(default=7, mandatory=True), 'optional': attr.bool(default=True)}",
        );
        assert!(
            bind(&definition, json!({}))
                .unwrap_err()
                .contains("_private")
        );
        assert!(
            bind(
                &definition,
                json!({"_private": "override", "required": null})
            )
            .unwrap_err()
            .contains("required")
        );
        let attrs = bind(
            &definition,
            json!({"_private": "override", "required": 0, "optional": null}),
        )
        .unwrap();
        assert_eq!(attrs["_private"], AttrValue::String("override".to_owned()));
        assert_eq!(attrs["required"], AttrValue::Int(0));
        assert_eq!(attrs["optional"], AttrValue::Bool(true));
    }

    #[test]
    fn binder_checks_json_types_and_int32_bounds() {
        let definition = definition(
            "{'s': attr.string(), 'ss': attr.string_list(), 'i': attr.int(), 'b': attr.bool(), 'l': attr.label(), 'ls': attr.label_list()}",
        );
        for value in [
            json!({"s": 1}),
            json!({"ss": "x"}),
            json!({"ss": ["x", 1]}),
            json!({"i": true}),
            json!({"i": 1.0}),
            json!({"i": 2147483648_i64}),
            json!({"i": -2147483649_i64}),
            json!({"i": u64::MAX}),
            json!({"b": 1}),
            json!({"l": []}),
            json!({"ls": ["//:ok", false]}),
        ] {
            assert!(bind(&definition, value.clone()).is_err(), "{value}");
        }
        for i in [i32::MIN, i32::MAX] {
            let attrs = bind(
                &definition,
                json!({"s": "", "ss": ["a", "b"], "i": i, "b": false}),
            )
            .unwrap();
            assert_eq!(attrs["i"], AttrValue::Int(i));
            assert_eq!(attrs["b"], AttrValue::Bool(false));
            assert_eq!(attrs["l"], AttrValue::Label(None));
            assert_eq!(attrs["ls"], AttrValue::Labels(vec![]));
        }
    }

    #[test]
    fn binder_preserves_definition_defaults_but_resolves_supplied_labels_at_caller() {
        let definition = definition(
            "{'l': attr.label(default=':tool'), 'ls': attr.label_list(default=[':one', '//other:two'])}",
        );
        let defaults = bind_attributes(&definition, &raw(json!({"l": null})), "repo", |_| {
            panic!("canonical defaults must not be resolved again")
        })
        .unwrap();
        assert_eq!(
            defaults["l"],
            AttrValue::Label(Some("//defs:tool".to_owned()))
        );
        assert_eq!(
            defaults["ls"],
            AttrValue::Labels(vec!["//defs:one".to_owned(), "//other:two".to_owned()])
        );
        let supplied = bind(
            &definition,
            json!({"l": ":tool", "ls": [":one", "//other:two"]}),
        )
        .unwrap();
        assert_eq!(
            supplied["l"],
            AttrValue::Label(Some("//caller:tool".to_owned()))
        );
        assert_eq!(
            supplied["ls"],
            AttrValue::Labels(vec!["//caller:one".to_owned(), "//other:two".to_owned()])
        );
        let error = bind_attributes(
            &definition,
            &raw(json!({"l": "@missing//:tool"})),
            "repo",
            |_| Err("repository is not visible from the caller".to_owned()),
        )
        .unwrap_err();
        assert!(
            error.contains("repository is not visible from the caller"),
            "{error}"
        );
    }

    #[test]
    fn recorded_labels_preserve_main_and_external_identity_while_strings_use_caller() {
        let definition = definition("{'labels': attr.label_list()}");
        let values = BTreeMap::from([(
            "labels".to_owned(),
            RecordedValue::List(vec![
                RecordedValue::Label("//original:data".to_owned()),
                RecordedValue::Label("@@original//pkg:data".to_owned()),
                RecordedValue::String("//original:data".to_owned()),
                RecordedValue::String(":local".to_owned()),
                RecordedValue::String("@original//pkg:data".to_owned()),
            ]),
        )]);
        let attrs = bind_recorded_attributes(&definition, &values, "canonical", |text| {
            Label::parse_in(
                text,
                "extension",
                "export",
                &BTreeMap::from([("original".to_owned(), "remapped".to_owned())]),
            )
        })
        .unwrap();
        assert_eq!(
            attrs["labels"],
            AttrValue::Labels(vec![
                "//original:data".to_owned(),
                "@@original//pkg:data".to_owned(),
                "@@extension//original:data".to_owned(),
                "@@extension//export:local".to_owned(),
                "@@remapped//pkg:data".to_owned(),
            ])
        );
    }

    #[test]
    fn label_dicts_bind_strings_at_caller_and_preserve_recorded_label_identity() {
        let definition = definition(
            "{'files': attr.string_keyed_label_dict(default={'BUILD': ':build'}), 'groups': attr.label_list_dict()}",
        );
        let defaults = bind_attributes(&definition, &raw(json!({})), "repo", |_| {
            panic!("canonical defaults must not be resolved again")
        })
        .unwrap();
        assert_eq!(
            defaults["files"],
            AttrValue::StringKeyedLabelDict(vec![("BUILD".to_owned(), "//defs:build".to_owned())])
        );
        assert_eq!(defaults["groups"], AttrValue::LabelListDict(vec![]));
        let supplied = bind(
            &definition,
            json!({"files": {"a": ":x"}, "groups": {"g": [":one", "//other:two"], "e": []}}),
        )
        .unwrap();
        assert_eq!(
            supplied["files"],
            AttrValue::StringKeyedLabelDict(vec![("a".to_owned(), "//caller:x".to_owned())])
        );
        assert_eq!(
            supplied["groups"],
            AttrValue::LabelListDict(vec![
                (
                    "g".to_owned(),
                    vec!["//caller:one".to_owned(), "//other:two".to_owned()]
                ),
                ("e".to_owned(), vec![]),
            ])
        );
        for (value, expected) in [
            (json!({"files": [":x"]}), "dict(string, label)"),
            (
                json!({"files": {"a": 1}}),
                "in call to 'R' repo rule, expected value of type 'string' for dict value element, but got 1 (int)",
            ),
            (
                json!({"files": {"a": ["//:x", 2]}}),
                "expected value of type 'string' for dict value element, but got [\"//:x\", 2] (list)",
            ),
            (
                json!({"name": "n", "groups": {"g": ":x"}}),
                "in call to 'R' repo rule with name 'n', expected value of type 'list(label)' for dict value element, but got \":x\" (string)",
            ),
            (
                json!({"groups": {"g": [1]}}),
                "expected value of type 'string' for element 0 of dict value element, but got 1 (int)",
            ),
        ] {
            let error = bind(&definition, value.clone()).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
        let recorded = BTreeMap::from([
            (
                "files".to_owned(),
                RecordedValue::Dict(vec![
                    (
                        "a".to_owned(),
                        RecordedValue::Label("@@original//pkg:a".to_owned()),
                    ),
                    (
                        "b".to_owned(),
                        RecordedValue::String("@original//pkg:b".to_owned()),
                    ),
                ]),
            ),
            (
                "groups".to_owned(),
                RecordedValue::Dict(vec![(
                    "g".to_owned(),
                    RecordedValue::List(vec![
                        RecordedValue::Label("//original:data".to_owned()),
                        RecordedValue::String(":local".to_owned()),
                    ]),
                )]),
            ),
        ]);
        let attrs = bind_recorded_attributes(&definition, &recorded, "canonical", |text| {
            Label::parse_in(
                text,
                "extension",
                "export",
                &BTreeMap::from([("original".to_owned(), "remapped".to_owned())]),
            )
        })
        .unwrap();
        assert_eq!(
            attrs["files"],
            AttrValue::StringKeyedLabelDict(vec![
                ("a".to_owned(), "@@original//pkg:a".to_owned()),
                ("b".to_owned(), "@@remapped//pkg:b".to_owned()),
            ])
        );
        assert_eq!(
            attrs["groups"],
            AttrValue::LabelListDict(vec![(
                "g".to_owned(),
                vec![
                    "//original:data".to_owned(),
                    "@@extension//export:local".to_owned()
                ]
            )])
        );
        // Labels are values of these dictionaries only, never keys or nested lists.
        for (name, value) in [
            (
                "files",
                RecordedValue::Dict(vec![(
                    "a".to_owned(),
                    RecordedValue::List(vec![RecordedValue::Label("//:x".to_owned())]),
                )]),
            ),
            (
                "groups",
                RecordedValue::Dict(vec![(
                    "g".to_owned(),
                    RecordedValue::Label("//:x".to_owned()),
                )]),
            ),
        ] {
            let error = bind_recorded_attributes(
                &definition,
                &BTreeMap::from([(name.to_owned(), value)]),
                "canonical",
                Label::parse_canonical,
            )
            .unwrap_err();
            assert!(error.contains("Label"), "{name}: {error}");
        }
    }

    #[test]
    fn computed_defaults_are_private_and_explicitly_unsupported_for_repository_rules() {
        for (attrs, expected) in [
            (
                "{'p': attr.label(default=lambda name: None)}",
                "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found 'p'",
            ),
            (
                "{'_p': attr.label(default=lambda name: None)}",
                "computed (function) default of repository attribute \"_p\" is not supported by the native profile",
            ),
        ] {
            let error = extension(&format!("def impl(ctx):\n    pass\nr = repository_rule(implementation=impl, attrs={attrs})\n")).unwrap_err();
            assert!(error.contains(expected), "{attrs}: {error}");
        }
    }

    #[test]
    fn recorded_labels_are_not_strings_or_nested_label_lists() {
        let definition = definition(
            "{'s': attr.string(), 'ss': attr.string_list(), 'l': attr.label(), 'ls': attr.label_list()}",
        );
        let label = RecordedValue::Label("//:data".to_owned());
        for (name, value) in [
            ("s", label.clone()),
            ("ss", RecordedValue::List(vec![label.clone()])),
            ("l", RecordedValue::List(vec![label.clone()])),
            (
                "ls",
                RecordedValue::List(vec![RecordedValue::List(vec![label.clone()])]),
            ),
        ] {
            let error = bind_recorded_attributes(
                &definition,
                &BTreeMap::from([(name.to_owned(), value)]),
                "canonical",
                Label::parse_canonical,
            )
            .unwrap_err();
            assert!(error.contains("Label"), "{name}: {error}");
        }
        let attrs = bind_recorded_attributes(
            &definition,
            &BTreeMap::from([("l".to_owned(), label)]),
            "canonical",
            |text| Label::parse(text, "elsewhere"),
        )
        .unwrap();
        assert_eq!(attrs["l"], AttrValue::Label(Some("//:data".to_owned())));
    }

    #[test]
    fn recorded_string_sibling_resolution_is_deferred_and_defaults_are_not_rebound() {
        let definition = definition("{'l': attr.label(), 'default': attr.label(default=':tool')}");
        let raw = BTreeMap::from([(
            "l".to_owned(),
            RecordedValue::String("@later//:data".to_owned()),
        )]);
        let siblings = BTreeMap::from([("later".to_owned(), "ext+later".to_owned())]);
        let attrs = bind_recorded_attributes(&definition, &raw, "ext+first", |text| {
            assert_eq!(text, "@later//:data", "only raw explicit labels resolve");
            Label::parse_in(text, "extension", "public", &siblings)
        })
        .unwrap();
        assert_eq!(
            attrs["l"],
            AttrValue::Label(Some("@@ext+later//:data".to_owned()))
        );
        assert_eq!(
            attrs["default"],
            AttrValue::Label(Some("//defs:tool".to_owned()))
        );
        assert_eq!(attrs["name"], AttrValue::String("ext+first".to_owned()));
    }

    #[test]
    fn recorded_binding_skips_none_and_metadata_but_keeps_schema_validation() {
        let definition = definition(
            "{'required': attr.int(mandatory=True), 'text': attr.string(default='default')}",
        );
        let raw = BTreeMap::from([
            ("unknown".to_owned(), RecordedValue::None),
            ("required".to_owned(), RecordedValue::Int(7)),
            ("text".to_owned(), RecordedValue::None),
            (
                "tags".to_owned(),
                RecordedValue::Label("//:not_a_string".to_owned()),
            ),
            ("visibility".to_owned(), RecordedValue::Dict(vec![])),
            (
                "name".to_owned(),
                RecordedValue::String("original".to_owned()),
            ),
        ]);
        let attrs = bind_recorded_attributes(&definition, &raw, "ext+made", Label::parse_canonical)
            .unwrap();
        assert_eq!(attrs["required"], AttrValue::Int(7));
        assert_eq!(attrs["text"], AttrValue::String("default".to_owned()));
        assert_eq!(attrs["name"], AttrValue::String("ext+made".to_owned()));
        for (name, value, message) in [
            ("required", RecordedValue::None, "missing mandatory"),
            ("text", RecordedValue::Dict(vec![]), "expected a string"),
            ("text", RecordedValue::String("Ā".to_owned()), "Latin-1"),
            (
                "unknown",
                RecordedValue::Bool(false),
                "unknown repository attribute",
            ),
        ] {
            let mut raw = raw.clone();
            raw.insert(name.to_owned(), value);
            let error =
                bind_recorded_attributes(&definition, &raw, "ext+made", Label::parse_canonical)
                    .unwrap_err();
            assert!(error.contains(message), "{name}: {error}");
        }
    }
}
