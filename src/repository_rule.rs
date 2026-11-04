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

use crate::custom_rule::{AttrKind, AttrValue, attribute_spec, attribute_spec_ref};
use crate::extension_calls::RecordedValue;
use crate::label::Label;
use crate::loading::ExtensionContext;
use crate::repository_budget::HostBudget;
use crate::repository_environment::DeclaredEnvironment;

#[derive(Clone, Debug)]
pub struct RuleDefinition {
    pub implementation: OwnedFrozenValue,
    /// Shared with the rule's definition value and every repository it
    /// generates (MEM2: a module extension's thousands of repositories
    /// each held a copy).
    pub schema: crate::custom_rule::SharedSchema,
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
        pub schema: crate::custom_rule::SharedSchema,
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
        /// `environ` in declaration order, each name once, and `doc`
        /// (untrimmed), for crate::starlark_doc_extract.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub environ_order: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub doc: Option<String>,
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

/// What `starlark_doc_extract` documents of a `repository_rule()` value
/// (ModuleInfoExtractor.visitRepositoryRule).
pub(crate) fn repository_rule_doc(
    value: Value<'_>,
) -> Option<crate::starlark_doc_extract::RepositoryRuleDoc> {
    let definition = values::RepoDefinition::from_value(value)?;
    Some(crate::starlark_doc_extract::RepositoryRuleDoc {
        exported: definition.exported.get().cloned(),
        file: definition.definition_label.clone(),
        doc: definition.doc.clone(),
        attrs: definition
            .attribute_order
            .iter()
            .filter_map(|name| {
                Some(crate::starlark_doc_extract::DocAttr {
                    name: name.clone(),
                    spec: definition.schema.get(name)?.clone(),
                    starlark_defined: true,
                })
            })
            .collect(),
        environ: definition.environ_order.clone(),
    })
}

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
        let mut environ_order: Vec<String> = Vec::new();
        for name in ListRef::from_value(environ)
            .map(|list| list.content().to_vec())
            .or_else(|| TupleRef::from_value(environ).map(|tuple| tuple.content().to_vec()))
            .unwrap_or_default()
        {
            if let Some(name) = name.unpack_str()
                && !environ_order.iter().any(|seen| seen == name)
            {
                environ_order.push(name.to_owned());
            }
        }
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
            schema: crate::custom_rule::SharedSchema::new(schema),
            attribute_order,
            local,
            configure,
            environ,
            environ_order,
            doc: doc.unpack_str().map(str::to_owned),
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
    bind_attributes_where(
        definition,
        raw,
        canonical_name,
        resolve,
        "to the root module",
    )
}

/// [`bind_attributes`] with RepoRule.instantiate's `repoMappingWhere`
/// (`to the root module`, `to the module 'NAME@VERSION'`, `in the extension
/// 'ID'`; InnateRunnableExtension.java:205-210,
/// ModuleExtensionEvalStarlarkThreadContext.java:150-155).
pub fn bind_attributes_where(
    definition: &RuleDefinition,
    raw: &BTreeMap<String, serde_json::Value>,
    canonical_name: &str,
    resolve: impl Fn(&str) -> Result<Label, String>,
    repo_mapping_where: &str,
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
        let parsed = parse_attribute(name, &spec.kind, value, &resolve).map_err(|e| {
            if bazel_conversion_error(&spec.kind, &e) {
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
    for (name, spec) in definition.schema.iter() {
        if !attrs.contains_key(name) {
            if spec.mandatory {
                return Err(format!("missing mandatory repository attribute {name:?}"));
            }
            attrs.insert(name.clone(), spec.default.clone());
        }
    }
    // AttributeUtils.java:104-135: labels of repositories the caller does
    // not see fail after every attribute converted, in declaration order.
    if let Some(error) = unknown_repository_error(
        definition
            .attribute_order
            .iter()
            .filter_map(|name| attrs.get(name).map(|value| (name.as_str(), value))),
        repo_mapping_where,
    ) {
        return Err(format!(
            "in call to '{}' repo rule{}, {error}",
            rule_display_name(definition),
            with_name(raw)
        ));
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

/// Whether a conversion error of an attribute of `kind` carries Bazel's
/// Type.ConversionException / label syntax text, to which the caller adds
/// AttributeUtils' where-text (`in call to 'R' repo rule …, `, `in 'T' tag, `).
pub(crate) fn bazel_conversion_error(kind: &AttrKind, error: &str) -> bool {
    if *kind == AttrKind::Bool {
        return error.starts_with(BOOL_EXPECTED);
    }
    matches!(
        kind,
        AttrKind::StringKeyedLabelDict
            | AttrKind::LabelListDict
            | AttrKind::LabelKeyedStringDict
            | AttrKind::StringDict
            | AttrKind::StringListDict
    ) && (error.starts_with("expected value of type '")
        || error.starts_with("duplicate labels")
        || error.starts_with("invalid label '"))
}

/// A string-keyed dict transported with a non-string key fails on that key
/// (DictType.convert, packages/Type.java:574-577), before any native check.
pub(crate) fn reject_non_string_keys(value: &serde_json::Value) -> Result<(), String> {
    if value
        .as_object()
        .is_some_and(|object| object.contains_key(DICT_ENTRIES))
    {
        for (key, _) in dict_entries(value).unwrap_or_default() {
            string_key(&key)?;
        }
    }
    Ok(())
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
    bind_recorded_attributes_where(
        definition,
        raw,
        canonical_name,
        resolve,
        "to the root module",
    )
}

/// [`bind_recorded_attributes`] with the extension's `repoMappingWhere`
/// (`in the extension 'ID'`, ModuleExtensionEvalStarlarkThreadContext.java:150-155).
pub fn bind_recorded_attributes_where(
    definition: &RuleDefinition,
    raw: &BTreeMap<String, RecordedValue>,
    canonical_name: &str,
    resolve: impl Fn(&str) -> Result<Label, String>,
    repo_mapping_where: &str,
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
        /// Dictionary keys: `dict(label, string)`.
        dict_label_keys: bool,
    }
    const PLAIN: Shape = Shape {
        label: false,
        label_list: false,
        dict_values: (false, false),
        dict_label_keys: false,
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
            RecordedValue::LabelKeyedDict(values) if shape.dict_label_keys => {
                // Genuine Label keys travel as their canonical `@@` form; key
                // collisions were reported by the caller's duplicate check.
                let mut object = serde_json::Map::new();
                for (key, value) in values {
                    let key = match key {
                        RecordedValue::String(text) => text.clone(),
                        RecordedValue::Label(text) => {
                            Label::parse_canonical(text)?.canonical_starlark()
                        }
                        _ => return Err("expected a string or Label dict key".to_owned()),
                    };
                    if object
                        .insert(key.clone(), json_value(value, PLAIN)?)
                        .is_some()
                    {
                        return Err(format!("duplicate label key {key}"));
                    }
                }
                serde_json::Value::Object(object)
            }
            RecordedValue::LabelKeyedDict(_) => {
                return Err(
                    "expected an attribute-compatible value, got a Label dict key".to_owned(),
                );
            }
        })
    }

    /// LabelKeyedDictType's collision check over the recorded keys, which can
    /// mix strings and genuine Labels (packages/BuildType.java:449-490). Runs
    /// only when every key converts and every value is a string, so DictType's
    /// earlier per-entry conversion errors keep their precedence.
    fn label_key_collision(
        name: &str,
        value: &RecordedValue,
        resolve: &impl Fn(&str) -> Result<Label, String>,
    ) -> Option<String> {
        let RecordedValue::LabelKeyedDict(entries) = value else {
            return None;
        };
        let mut keys = Vec::new();
        for (key, item) in entries {
            if !matches!(item, RecordedValue::String(_)) {
                return None;
            }
            match key {
                RecordedValue::String(text) => {
                    keys.push((resolve(text).ok()?, serde_json::to_string(text).ok()?))
                }
                RecordedValue::Label(text) => {
                    let label = Label::parse_canonical(text).ok()?;
                    let repr =
                        format!("Label({})", serde_json::to_string(&label.to_string()).ok()?);
                    keys.push((label, repr));
                }
                _ => return None,
            }
        }
        label_key_duplicates(&format!("attribute '{name}'"), &keys)
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
        if matches!(kind, AttrKind::LabelKeyedStringDict)
            && let Some(error) = label_key_collision(name, value, &resolve)
        {
            let with_name = match raw.get("name") {
                Some(RecordedValue::String(name)) => format!(" with name '{name}'"),
                _ => String::new(),
            };
            return Err(format!(
                "in call to '{}' repo rule{with_name}, {error}",
                rule_display_name(definition)
            ));
        }
        let value = json_value(
            value,
            Shape {
                label: matches!(kind, AttrKind::Label),
                label_list: matches!(kind, AttrKind::LabelList),
                dict_values: (
                    matches!(kind, AttrKind::StringKeyedLabelDict),
                    matches!(kind, AttrKind::LabelListDict),
                ),
                dict_label_keys: matches!(kind, AttrKind::LabelKeyedStringDict),
            },
        )
        .map_err(|e| format!("repository attribute {name:?}: {e}"))?;
        converted.insert(name.clone(), value);
    }
    bind_attributes_where(
        definition,
        &converted,
        canonical_name,
        resolve,
        repo_mapping_where,
    )
}

fn parse_attribute(
    name: &str,
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
        AttrKind::IntList => {
            return Err(format!(
                "attribute kind {kind:?} is not supported for repository rules by the native profile"
            ));
        }
        AttrKind::String => AttrValue::String(string()?.to_owned()),
        AttrKind::StringList => AttrValue::Strings(strings()?),
        AttrKind::StringDict | AttrKind::StringListDict
            if reject_non_string_keys(value).is_err() =>
        {
            return reject_non_string_keys(value).map(|()| AttrValue::Bool(false));
        }
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
        AttrKind::Bool => AttrValue::Bool(convert_bool(name, value)?),
        AttrKind::Label => AttrValue::Label(Some(resolve(string()?)?.to_string())),
        AttrKind::LabelList => AttrValue::Labels(
            strings()?
                .iter()
                .map(|text| resolve(text).map(|label| label.to_string()))
                .collect::<Result<_, _>>()?,
        ),
        AttrKind::StringKeyedLabelDict
        | AttrKind::LabelListDict
        | AttrKind::LabelKeyedStringDict => label_dict_json(name, kind, value, resolve)?,
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
    name: &str,
    kind: &AttrKind,
    value: &serde_json::Value,
    resolve: &impl Fn(&str) -> Result<Label, String>,
) -> Result<AttrValue, String> {
    // AttributeUtils.typeCheckAttrValues converts with what = "attribute 'NAME'"
    // (bazel/repository/AttributeUtils.java:76-83).
    let what = format!("attribute '{name}'");
    if matches!(kind, AttrKind::LabelKeyedStringDict) {
        return label_keyed_string_dict_json(&what, value, resolve);
    }
    let lists = matches!(kind, AttrKind::LabelListDict);
    let ty = if lists {
        "dict(string, list(label))"
    } else {
        "dict(string, label)"
    };
    let entries = dict_entries(value).ok_or_else(|| conversion_error(ty, value, &what))?;
    let label = |value: &serde_json::Value, what: &str| -> Result<String, String> {
        let text = value
            .as_str()
            .ok_or_else(|| conversion_error("string", value, what))?;
        convert_label(text, what, resolve).map(|label| label.to_string())
    };
    if lists {
        let mut bound = Vec::new();
        for (key, values) in entries {
            let key = string_key(&key)?;
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
            bound.push((key, labels));
        }
        Ok(AttrValue::LabelListDict(bound))
    } else {
        let mut bound = Vec::new();
        for (key, value) in entries {
            let key = string_key(&key)?;
            bound.push((key, label(value, "dict value element")?));
        }
        Ok(AttrValue::StringKeyedLabelDict(bound))
    }
}

/// The JSON transport of a Starlark dict with a key that is not a string
/// (`{1: "x"}` in MODULE.bazel): `{DICT_ENTRIES: [[key, value], ...]}` in
/// insertion order, a non-string key being `{KEY_REPR: repr, KEY_TYPE: type}`.
/// Only produced for such dicts, so all other transports are unchanged.
pub(crate) const DICT_ENTRIES: &str = "\u{0}dict_entries";
pub(crate) const KEY_REPR: &str = "\u{0}key_repr";
pub(crate) const KEY_TYPE: &str = "\u{0}key_type";

/// A transported dict's (key, value) entries in insertion order; keys are
/// strings or [`KEY_REPR`] objects. `None` when `value` is not a dict.
pub(crate) fn dict_entries(
    value: &serde_json::Value,
) -> Option<Vec<(serde_json::Value, &serde_json::Value)>> {
    let object = value.as_object()?;
    if let Some(entries) = object
        .get(DICT_ENTRIES)
        .and_then(|entries| entries.as_array())
        && object.len() == 1
    {
        return Some(
            entries
                .iter()
                .filter_map(|entry| match entry.as_array().map(Vec::as_slice) {
                    Some([key, value]) => Some((key.clone(), value)),
                    _ => None,
                })
                .collect(),
        );
    }
    Some(
        object
            .iter()
            .map(|(key, value)| (serde_json::Value::String(key.clone()), value))
            .collect(),
    )
}

/// DictType.convert's string key conversion (`dict key element`,
/// packages/Type.java:574-577).
fn string_key(key: &serde_json::Value) -> Result<String, String> {
    key.as_str()
        .map(str::to_owned)
        .ok_or_else(|| conversion_error("string", key, "dict key element"))
}

/// BuildType.LABEL.convert of a label string (packages/BuildType.java:412-429):
/// Bazel's label syntax errors (`invalid label 'L' in WHAT: …`,
/// cmdline/LabelParser.java via `crate::bazel_label`) and
/// Label.parseWithPackageContext's absolute-package check (Label.java:234-245),
/// then the binding's repository mapping. An apparent repository the mapping
/// does not contain becomes a deferred invalid label
/// (RepositoryName.createUnvalidated), reported by
/// [`unknown_repository_error`] after every attribute converted.
pub(crate) fn convert_label(
    text: &str,
    what: &str,
    resolve: &impl Fn(&str) -> Result<Label, String>,
) -> Result<Label, String> {
    let parts = crate::bazel_label::Parts::parse(text)
        .map_err(|error| format!("invalid label '{text}' in {what}: {error}"))?;
    if parts.pkg_ends_with_triple_dots {
        return Err(format!(
            "invalid label '{text}' in {what}: invalid label '{text}': package name cannot contain '...'"
        ));
    }
    if !parts.pkg.is_empty() && !parts.pkg_is_absolute {
        return Err(format!(
            "invalid label '{text}' in {what}: invalid label '{text}': absolute label must begin with '@' or '//'"
        ));
    }
    match resolve(text) {
        Err(error)
            if error.starts_with("unknown apparent repository @")
                && !parts.repo_is_canonical
                && parts.repo.is_some() =>
        {
            Ok(Label {
                repository: format!(
                    "[unknown repo '{}' requested from @@]",
                    parts.repo.unwrap_or_default()
                ),
                package: parts.pkg.to_owned(),
                name: parts.target.to_owned(),
            })
        }
        result => result,
    }
}

/// AttributeUtils.typeCheckAttrValues' visibility check
/// (bazel/repository/AttributeUtils.java:118-135): the first label of the
/// first attribute in declaration order whose repository the binding's
/// mapping does not contain.
pub(crate) fn unknown_repository_error<'a>(
    attributes: impl Iterator<Item = (&'a str, &'a AttrValue)>,
    repo_mapping_where: &str,
) -> Option<String> {
    for (name, value) in attributes {
        let labels: Vec<&String> = match value {
            AttrValue::LabelKeyedStringDict(entries) => {
                entries.iter().map(|(key, _)| key).collect()
            }
            AttrValue::StringKeyedLabelDict(entries) => {
                entries.iter().map(|(_, value)| value).collect()
            }
            AttrValue::LabelListDict(entries) => {
                entries.iter().flat_map(|(_, values)| values).collect()
            }
            _ => continue,
        };
        for text in labels {
            let Some(rest) = text.strip_prefix("@@[unknown repo '") else {
                continue;
            };
            let Some((repo, rest)) = rest.split_once("' requested from @@]//") else {
                continue;
            };
            let (package, target) = rest.split_once(':').unwrap_or((rest, ""));
            return Some(format!(
                "no repository visible as '@{repo}' {repo_mapping_where}, but referenced by label '@{repo}//{package}:{target}' in attribute '{name}'"
            ));
        }
    }
    None
}

/// Bind a JSON-transported `dict(label, string)` value
/// (BuildType.LABEL_KEYED_STRING_DICT, packages/BuildType.java:67-68): keys
/// are converted with the binding's repository mapping (canonical `@@` forms
/// of genuine Label keys keep their identity), values must be strings, in
/// insertion order. Keys converting to the same label are Bazel's
/// `duplicate labels in WHAT: L (as [k1, k2])` (LabelKeyedDictType.convert,
/// packages/BuildType.java:449-490).
fn label_keyed_string_dict_json(
    what: &str,
    value: &serde_json::Value,
    resolve: &impl Fn(&str) -> Result<Label, String>,
) -> Result<AttrValue, String> {
    let entries =
        dict_entries(value).ok_or_else(|| conversion_error("dict(label, string)", value, what))?;
    // DictType.convert converts each key, then each value, in order
    // (packages/Type.java:565-580).
    let mut bound: Vec<(String, String)> = Vec::new();
    let mut labels = Vec::new();
    for (key, item) in entries {
        let text = key
            .as_str()
            .ok_or_else(|| conversion_error("string", &key, "dict key element"))?;
        let label = convert_label(text, "dict key element", resolve)?;
        let item = item
            .as_str()
            .ok_or_else(|| conversion_error("string", item, "dict value element"))?;
        labels.push((label.clone(), starlark_repr(&key).0));
        bound.push((label.to_string(), item.to_owned()));
    }
    if let Some(error) = label_key_duplicates(what, &labels) {
        return Err(error);
    }
    Ok(AttrValue::LabelKeyedStringDict(bound))
}

/// LabelKeyedDictType.convert's collision report (packages/BuildType.java:458-489)
/// over (converted label, original key repr) pairs in input order.
pub(crate) fn label_key_duplicates(what: &str, keys: &[(Label, String)]) -> Option<String> {
    let mut converted: Vec<(&Label, Vec<&str>)> = Vec::new();
    for (label, original) in keys {
        match converted.iter_mut().find(|(seen, _)| *seen == label) {
            Some((_, originals)) => originals.push(original),
            None => converted.push((label, vec![original])),
        }
    }
    let duplicates = converted
        .iter()
        .filter(|(_, originals)| originals.len() > 1)
        .map(|(label, originals)| {
            format!(
                " {} (as [{}])",
                crate::rule_declarations::canonical_form(label),
                originals.join(", ")
            )
        })
        .collect::<Vec<_>>();
    (!duplicates.is_empty()).then(|| format!("duplicate labels in {what}:{}", duplicates.join(",")))
}

/// Type.BOOLEAN.convert of a JSON-transported value (packages/Type.java:409-428):
/// True/False or the integers 0 and 1; anything else fails with
/// `expected one of [False, True, 0, 1] for attribute 'NAME', but got REPR (TYPE)`
/// (AttributeUtils.typeCheckAttrValues what-text).
pub(crate) fn convert_bool(name: &str, value: &serde_json::Value) -> Result<bool, String> {
    value
        .as_bool()
        .or_else(|| match value.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        })
        .ok_or_else(|| {
            let (repr, type_name) = starlark_repr(value);
            format!("{BOOL_EXPECTED} for attribute '{name}', but got {repr} ({type_name})")
        })
}

/// Type.BOOLEAN's ConversionException expectation (packages/Type.java:427).
pub(crate) const BOOL_EXPECTED: &str = "expected one of [False, True, 0, 1]";

/// `expected value of type 'T' for WHAT, but got REPR (TYPE)` of a
/// JSON-transported Starlark value (Type.ConversionException, packages/Type.java:296-321).
fn conversion_error(ty: &str, value: &serde_json::Value, what: &str) -> String {
    let (repr, type_name) = starlark_repr(value);
    format!("expected value of type '{ty}' for {what}, but got {repr} ({type_name})")
}

/// Starlark `repr` and `type` of a transported attribute value.
fn starlark_repr(value: &serde_json::Value) -> (String, String) {
    use serde_json::Value as Json;
    if let Json::Object(object) = value {
        if let (Some(Json::String(repr)), Some(Json::String(ty))) =
            (object.get(KEY_REPR), object.get(KEY_TYPE))
        {
            return (repr.clone(), ty.clone());
        }
        if object.contains_key(DICT_ENTRIES) {
            let entries = dict_entries(value).unwrap_or_default();
            return (
                format!(
                    "{{{}}}",
                    entries
                        .iter()
                        .map(|(key, item)| format!(
                            "{}: {}",
                            starlark_repr(key).0,
                            starlark_repr(item).0
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "dict".to_owned(),
            );
        }
    }
    match value {
        Json::Null => ("None".to_owned(), "NoneType".to_owned()),
        Json::Bool(flag) => (
            if *flag { "True" } else { "False" }.to_owned(),
            "bool".to_owned(),
        ),
        Json::Number(number) => (number.to_string(), "int".to_owned()),
        Json::String(text) => (
            serde_json::to_string(text).unwrap_or_else(|_| format!("{text:?}")),
            "string".to_owned(),
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
            "list".to_owned(),
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
            "dict".to_owned(),
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
            json!({"b": 2}),
            json!({"b": -1}),
            json!({"b": "true"}),
            json!({"l": []}),
            json!({"ls": ["//:ok", false]}),
        ] {
            assert!(bind(&definition, value.clone()).is_err(), "{value}");
        }
        // Type.BOOLEAN.convert: 0 and 1 convert, anything else fails with
        // Bazel's text (genuine Bazel 9.3.0 via use_repo_rule rr(flag = 0/1/2)).
        for (raw, flag) in [(json!(0), false), (json!(1), true), (json!(true), true)] {
            let attrs = bind(&definition, json!({"b": raw})).unwrap();
            assert_eq!(attrs["b"], AttrValue::Bool(flag), "{raw}");
        }
        for (raw, shown) in [(json!(2), "2 (int)"), (json!("true"), "\"true\" (string)")] {
            assert_eq!(
                bind(&definition, json!({"b": raw})).unwrap_err(),
                format!(
                    "in call to 'R' repo rule, expected one of [False, True, 0, 1] for attribute 'b', but got {shown}"
                )
            );
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
    fn label_keyed_string_dicts_bind_keys_at_caller_and_report_bazel_errors() {
        // rules_oci oci_alias / crane toolchains shape.
        let definition = definition(
            "{'platforms': attr.label_keyed_string_dict(default={':linux': 'linux/amd64'})}",
        );
        let defaults = bind_attributes(&definition, &raw(json!({})), "repo", |_| {
            panic!("canonical defaults must not be resolved again")
        })
        .unwrap();
        assert_eq!(
            defaults["platforms"],
            AttrValue::LabelKeyedStringDict(vec![(
                "//defs:linux".to_owned(),
                "linux/amd64".to_owned()
            )])
        );
        let supplied = bind(
            &definition,
            json!({"platforms": {":b": "linux/arm64", "//other:a": "linux/amd64"}}),
        )
        .unwrap();
        // Insertion order, keys converted with the caller's context.
        assert_eq!(
            supplied["platforms"],
            AttrValue::LabelKeyedStringDict(vec![
                ("//caller:b".to_owned(), "linux/arm64".to_owned()),
                ("//other:a".to_owned(), "linux/amd64".to_owned()),
            ])
        );
        for (value, expected) in [
            (
                json!({"name": "n", "platforms": [":x"]}),
                "in call to 'R' repo rule with name 'n', expected value of type 'dict(label, string)' for attribute 'platforms', but got [\":x\"] (list)",
            ),
            (
                json!({"platforms": {":x": 1}}),
                "in call to 'R' repo rule, expected value of type 'string' for dict value element, but got 1 (int)",
            ),
            (
                json!({"platforms": {":x": "a", "//caller:x": "b"}}),
                "in call to 'R' repo rule, duplicate labels in attribute 'platforms': //caller:x (as [\":x\", \"//caller:x\"])",
            ),
        ] {
            assert_eq!(
                bind(&definition, value.clone()).unwrap_err(),
                expected,
                "{value}"
            );
        }
        // Recorded extension calls may mix string and genuine Label keys.
        let recorded = BTreeMap::from([
            ("name".to_owned(), RecordedValue::String("n".to_owned())),
            (
                "platforms".to_owned(),
                RecordedValue::LabelKeyedDict(vec![
                    (
                        RecordedValue::Label("@@platforms//cpu:x86_64".to_owned()),
                        RecordedValue::String("linux/amd64".to_owned()),
                    ),
                    (
                        RecordedValue::String("@original//cpu:arm64".to_owned()),
                        RecordedValue::String("linux/arm64".to_owned()),
                    ),
                ]),
            ),
        ]);
        let resolve = |text: &str| {
            Label::parse_in(
                text,
                "extension",
                "export",
                &BTreeMap::from([("original".to_owned(), "remapped".to_owned())]),
            )
        };
        let attrs = bind_recorded_attributes(&definition, &recorded, "canonical", resolve).unwrap();
        assert_eq!(
            attrs["platforms"],
            AttrValue::LabelKeyedStringDict(vec![
                (
                    "@@platforms//cpu:x86_64".to_owned(),
                    "linux/amd64".to_owned()
                ),
                ("@@remapped//cpu:arm64".to_owned(), "linux/arm64".to_owned()),
            ])
        );
        // A string key and a Label key naming the same target collide.
        let colliding = BTreeMap::from([
            ("name".to_owned(), RecordedValue::String("n".to_owned())),
            (
                "platforms".to_owned(),
                RecordedValue::LabelKeyedDict(vec![
                    (
                        RecordedValue::String("@@extension//export:x".to_owned()),
                        RecordedValue::String("a".to_owned()),
                    ),
                    (
                        RecordedValue::Label("@@extension//export:x".to_owned()),
                        RecordedValue::String("b".to_owned()),
                    ),
                ]),
            ),
        ]);
        assert_eq!(
            bind_recorded_attributes(&definition, &colliding, "canonical", resolve).unwrap_err(),
            "in call to 'R' repo rule with name 'n', duplicate labels in attribute 'platforms': @@extension//export:x (as [\"@@extension//export:x\", Label(\"@@extension//export:x\")])"
        );
        // Label keys are accepted only by label-keyed dictionaries.
        let other = self::definition("{'d': attr.string_dict()}");
        let error = bind_recorded_attributes(
            &other,
            &BTreeMap::from([(
                "d".to_owned(),
                RecordedValue::LabelKeyedDict(vec![(
                    RecordedValue::Label("//:x".to_owned()),
                    RecordedValue::String("v".to_owned()),
                )]),
            )]),
            "canonical",
            Label::parse_canonical,
        )
        .unwrap_err();
        assert!(error.contains("Label dict key"), "{error}");
    }

    #[test]
    fn label_dict_conversion_errors_use_bazel_texts() {
        let definition = definition(
            "{'platforms': attr.label_keyed_string_dict(), 'skd': attr.string_keyed_label_dict(), 'sd': attr.string_dict()}",
        );
        let int_key = json!({DICT_ENTRIES: [[{KEY_REPR: "1", KEY_TYPE: "int"}, "x"]]});
        for (value, expected) in [
            (
                json!({"name": "e", "platforms": int_key.clone()}),
                "in call to 'R' repo rule with name 'e', expected value of type 'string' for dict key element, but got 1 (int)",
            ),
            (
                json!({"name": "e", "skd": int_key.clone()}),
                "in call to 'R' repo rule with name 'e', expected value of type 'string' for dict key element, but got 1 (int)",
            ),
            (
                json!({"name": "e", "sd": int_key}),
                "in call to 'R' repo rule with name 'e', expected value of type 'string' for dict key element, but got 1 (int)",
            ),
            (
                json!({"name": "e", "platforms": {"//:a:b": "x"}}),
                "in call to 'R' repo rule with name 'e', invalid label '//:a:b' in dict key element: invalid target name 'a:b': target names may not contain ':'",
            ),
            (
                json!({"name": "e", "platforms": {"@@[x]//:a": "x"}}),
                "in call to 'R' repo rule with name 'e', invalid label '@@[x]//:a' in dict key element: invalid repository name '[x]': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'",
            ),
            (
                json!({"name": "e", "platforms": {"sub/pkg:x": "x"}}),
                "in call to 'R' repo rule with name 'e', invalid label 'sub/pkg:x' in dict key element: invalid label 'sub/pkg:x': absolute label must begin with '@' or '//'",
            ),
            (
                json!({"name": "e", "skd": {"k": "//:a:b"}}),
                "in call to 'R' repo rule with name 'e', invalid label '//:a:b' in dict value element: invalid target name 'a:b': target names may not contain ':'",
            ),
        ] {
            assert_eq!(
                bind(&definition, value.clone()).unwrap_err(),
                expected,
                "{value}"
            );
        }
        // A repository the caller does not see fails after every attribute
        // converted (AttributeUtils.java:118-135).
        let resolve = |text: &str| {
            Label::parse_in(
                text,
                "",
                "caller",
                &BTreeMap::from([("seen".to_owned(), "seen+".to_owned())]),
            )
        };
        assert_eq!(
            bind_attributes_where(
                &definition,
                &raw(json!({"name": "e", "platforms": {"@nope//:a": "x", "@seen//:b": "y"}})),
                "repo",
                resolve,
                "to the root module",
            )
            .unwrap_err(),
            "in call to 'R' repo rule with name 'e', no repository visible as '@nope' to the root module, but referenced by label '@nope//:a' in attribute 'platforms'"
        );
        assert_eq!(
            bind_attributes_where(
                &definition,
                &raw(json!({"name": "e", "platforms": {"@seen//:b": "y"}})),
                "repo",
                resolve,
                "to the root module",
            )
            .unwrap()["platforms"],
            AttrValue::LabelKeyedStringDict(vec![("@@seen+//:b".to_owned(), "y".to_owned())])
        );
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
