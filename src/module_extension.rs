//! Ordinary module-extension definitions and tag binding.
//! Requested-label/symbol identity, callback contexts, and execution live elsewhere.
// Macro-generated bindings retain the exact pinned eight-argument API.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::typing::StarlarkCallable;
use starlark::values::{OwnedFrozenValue, UnpackValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;

use crate::custom_rule::{AttrKind, AttrSpec, AttrValue, attribute_spec_ref};
use crate::label::Label;
use crate::loading::ExtensionContext;
use crate::repository_budget::HostBudget;

#[derive(Debug, Clone)]
pub struct ExtensionDefinition {
    pub implementation: OwnedFrozenValue,
    pub tag_classes: BTreeMap<String, BTreeMap<String, AttrSpec>>,
    pub os_dependent: bool,
    pub arch_dependent: bool,
    /// `module_extension(environ=...)`: names observed (recorded) before the
    /// callback, like repository_rule environ (ModuleExtension.getEnvVariables).
    pub environ: crate::repository_environment::DeclaredEnvironment,
}

const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<ExtensionDefinition>();
};

// Derives generate marker/coercion/trace implementations. Ignored fields are
// entirely owned; the sole heap reference is traced and frozen normally.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };
    use starlark::{starlark_complex_value, starlark_simple_value};

    #[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct TagClass {
        pub schema: BTreeMap<String, AttrSpec>,
        pub doc: Option<String>,
    }

    impl fmt::Display for TagClass {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // TagClass is a record without a repr override
            // (bazel/bzlmod/TagClass.java:33-36; StarlarkValue.java:50-52).
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.bzlmod.TagClass>")
        }
    }

    starlark_simple_value!(TagClass);

    // No writable/exposed fields, structural equality, or hash implementation.
    #[starlark_value(type = "tag_class")]
    impl<'v> StarlarkValue<'v> for TagClass {}

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
    pub(super) struct ModuleExtensionGen<V> {
        pub implementation: V,
        pub os_dependent: bool,
        pub arch_dependent: bool,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub tag_classes: BTreeMap<String, TagClass>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub doc: Option<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub environ: crate::repository_environment::DeclaredEnvironment,
    }

    impl<V> fmt::Display for ModuleExtensionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // ModuleExtension is a record without a repr override
            // (bazel/bzlmod/ModuleExtension.java:37; StarlarkValue.java:50-52).
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleExtension>",
            )
        }
    }

    starlark_complex_value!(pub(super) ModuleExtension);

    // Bazel's ModuleExtension is not StarlarkExportable. In particular, aliases
    // and private globals do not acquire a first-export identity here.
    // The record has no @StarlarkBuiltin, so type() is its simple class name.
    #[starlark_value(type = "ModuleExtension")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ModuleExtensionGen<V> where
        Self: ProvidesStaticType<'v>
    {
    }
}
use values::{FrozenModuleExtension, ModuleExtensionGen, TagClass};

/// Retain the implementation's owning heap, without requiring any export name.
pub fn unpack(value: &OwnedFrozenValue) -> Result<ExtensionDefinition, String> {
    let definition = value
        .value()
        .downcast_ref::<FrozenModuleExtension>()
        .ok_or_else(|| "expected a frozen module_extension definition".to_owned())?;
    let mut budget = HostBudget::default();
    for (name, class) in &definition.tag_classes {
        budget.string(name)?;
        budget.schema(&class.schema)?;
    }
    definition.environ.charge(&mut budget)?;
    let implementation = value.map(|value| {
        value
            .downcast_ref::<FrozenModuleExtension>()
            .expect("frozen definition checked above")
            .implementation
    });
    Ok(ExtensionDefinition {
        implementation,
        os_dependent: definition.os_dependent,
        arch_dependent: definition.arch_dependent,
        environ: definition.environ.clone(),
        tag_classes: definition
            .tag_classes
            .iter()
            .map(|(name, class)| (name.clone(), class.schema.clone()))
            .collect(),
    })
}

fn require_initialization(eval: &Evaluator<'_, '_, '_>, constructor: &str) -> anyhow::Result<()> {
    if eval
        .extra
        .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
        .is_none()
    {
        bail!("{constructor}() definitions are only allowed during .bzl extension initialization");
    }
    Ok(())
}

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

fn documentation(
    doc: Value<'_>,
    constructor: &str,
    budget: &mut HostBudget,
) -> anyhow::Result<Option<String>> {
    if doc.is_none() {
        Ok(None)
    } else {
        let doc = doc
            .unpack_str()
            .ok_or_else(|| anyhow!("{constructor} doc must be a string or None"))?;
        budget.string(doc).map_err(anyhow::Error::msg)?;
        Ok(Some(doc.to_owned()))
    }
}

/// Install the constructors only into shared `.bzl` initialization globals.
// This signature follows the pinned Starlark API, including gated options.
#[allow(clippy::too_many_arguments)]
#[starlark_module]
pub fn globals(builder: &mut GlobalsBuilder) {
    fn tag_class<'v>(
        #[starlark(default = AllocDict::EMPTY)] attrs: Value<'v>,
        #[starlark(require = named, default = NoneType)] doc: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        require_initialization(eval, "tag_class")?;
        let mut budget = HostBudget::default();
        let doc = documentation(doc, "tag_class", &mut budget)?;
        let attrs = DictRef::from_value(attrs)
            .ok_or_else(|| anyhow!("tag_class attrs must be a dict of attribute descriptors"))?;
        let mut schema = BTreeMap::new();
        for (name, descriptor) in attrs.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| anyhow!("tag attribute names must be strings"))?;
            if !identifier(name) {
                bail!("invalid tag attribute name {name:?}");
            }
            let spec = attribute_spec_ref(descriptor)
                .ok_or_else(|| anyhow!("tag attribute {name:?} must be an attr descriptor"))?;
            if matches!(spec.kind, AttrKind::Output | AttrKind::OutputList) {
                bail!("output tag attributes are unsupported: {name:?}");
            }
            // StarlarkRepositoryModule.tagClass (StarlarkRepositoryModule.java:240-256).
            crate::custom_rule::check_value_source_name(name, spec)?;
            if spec.declaration.computed_default {
                bail!(
                    "computed (function) default of tag attribute {name:?} is not supported by the native profile"
                );
            }
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.spec(spec).map_err(anyhow::Error::msg)?;
            schema.insert(name.to_owned(), spec.clone());
        }
        Ok(eval.heap().alloc(TagClass { schema, doc }))
    }

    fn module_extension<'v>(
        implementation: Value<'v>,
        #[starlark(require = named, default = AllocDict::EMPTY)] tag_classes: Value<'v>,
        #[starlark(require = named, default = NoneType)] doc: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] environ: Value<'v>,
        #[starlark(require = named, default = false)] os_dependent: bool,
        #[starlark(require = named, default = false)] arch_dependent: bool,
        #[starlark(require = named, default = 0)] facts_version: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        require_initialization(eval, "module_extension")?;
        // Unpacking StarlarkCallable checks HAS_invoke only. A builtin or a
        // zero-argument function is legal here; ctx arity is a runtime concern.
        if <StarlarkCallable<'v>>::unpack_value_opt(implementation).is_none() {
            bail!("module_extension implementation must be callable");
        }
        let mut budget = HostBudget::default();
        let doc = documentation(doc, "module_extension", &mut budget)?;
        let environ = ListRef::from_value(environ)
            .ok_or_else(|| anyhow!("module_extension environ must be a list of strings"))?;
        if environ.iter().any(|value| value.unpack_str().is_none()) {
            bail!("module_extension environ must be a list of strings");
        }
        let environ = crate::repository_environment::DeclaredEnvironment::from_names(
            environ
                .iter()
                .map(|value| Ok(value.unpack_str().expect("checked above"))),
            &mut budget,
        )
        .map_err(anyhow::Error::msg)?;
        if facts_version < 0 {
            bail!("module_extension facts_version must be a nonnegative 32-bit int");
        }
        if facts_version != 0 {
            bail!("module_extension with nonzero facts_version is unsupported");
        }
        let classes = DictRef::from_value(tag_classes)
            .ok_or_else(|| anyhow!("module_extension tag_classes must be a dict"))?;
        let mut tag_classes = BTreeMap::new();
        for (name, class) in classes.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| anyhow!("tag class names must be strings"))?;
            if !identifier(name) {
                bail!("invalid tag class name {name:?}");
            }
            let class = class
                .downcast_ref::<TagClass>()
                .ok_or_else(|| anyhow!("tag class {name:?} must be a tag_class value"))?;
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.schema(&class.schema).map_err(anyhow::Error::msg)?;
            if let Some(doc) = &class.doc {
                budget.string(doc).map_err(anyhow::Error::msg)?;
            }
            tag_classes.insert(name.to_owned(), class.clone());
        }
        Ok(eval.heap().alloc(ModuleExtensionGen {
            implementation,
            os_dependent,
            arch_dependent,
            tag_classes,
            doc,
            environ,
        }))
    }
}

/// Bind a tag independently of repository-rule naming and metadata semantics.
/// None is omission even for unknown names. Defaults already have lexical label
/// ownership; only supplied strings are resolved in the MODULE caller's mapping.
/// Like repository binding, tag type-checking does not enforce non_empty.
/// Callers preflight cumulative schema/default expansion and raw JSON before
/// using this copying binder across a module projection.
pub fn bind_tag(
    schema: &BTreeMap<String, AttrSpec>,
    tag_name: &str,
    raw: &BTreeMap<String, serde_json::Value>,
    resolve: impl Fn(&str) -> Result<Label, String>,
) -> Result<BTreeMap<String, AttrValue>, String> {
    bind_tag_where(schema, tag_name, raw, resolve, "to the root module", &[])
}

/// [`bind_tag`] with TypeCheckedTag's `repoMappingWhere` (`to the root
/// module` / `to the module 'NAME@VERSION'`, bzlmod/TypeCheckedTag.java:104-114)
/// and the tag class's attribute declaration order for the visibility check.
pub fn bind_tag_where(
    schema: &BTreeMap<String, AttrSpec>,
    tag_name: &str,
    raw: &BTreeMap<String, serde_json::Value>,
    resolve: impl Fn(&str) -> Result<Label, String>,
    repo_mapping_where: &str,
    attribute_order: &[String],
) -> Result<BTreeMap<String, AttrValue>, String> {
    let mut attrs = BTreeMap::new();
    for (name, value) in raw {
        if value.is_null() {
            continue;
        }
        let spec = schema
            .get(name)
            .ok_or_else(|| format!("unknown tag attribute {name:?}"))?;
        let parsed = parse_attribute(name, &spec.kind, value, &resolve).map_err(|e| {
            if crate::repository_rule::bazel_conversion_error(&spec.kind, &e) {
                // TypeCheckedTag where-text (bazel/bzlmod/TypeCheckedTag.java:106-114).
                format!("in '{tag_name}' tag, {e}")
            } else {
                format!("tag attribute {name:?}: {e}")
            }
        })?;
        // AttributeUtils.java:90-98 with TypeCheckedTag's where-text
        // "'%s' tag" (bazel/bzlmod/TypeCheckedTag.java:106-114).
        if !spec.declaration.allows(&parsed) {
            return Err(format!(
                "in '{tag_name}' tag, the value for attribute '{name}' {}",
                spec.declaration.allowed_values_reason(&parsed)
            ));
        }
        attrs.insert(name.clone(), parsed);
    }
    for (name, spec) in schema {
        if !attrs.contains_key(name) {
            if spec.mandatory {
                return Err(format!("missing mandatory tag attribute {name:?}"));
            }
            attrs.insert(name.clone(), spec.default.clone());
        }
    }
    // AttributeUtils.java:104-135 (declaration order when known).
    let ordered: Vec<&String> = if attribute_order.is_empty() {
        attrs.keys().collect()
    } else {
        attribute_order.iter().collect()
    };
    if let Some(error) = crate::repository_rule::unknown_repository_error(
        ordered
            .into_iter()
            .filter_map(|name| attrs.get(name).map(|value| (name.as_str(), value))),
        repo_mapping_where,
    ) {
        return Err(format!("in '{tag_name}' tag, {error}"));
    }
    for (name, value) in &attrs {
        let ascii = match value {
            AttrValue::String(value) => value.is_ascii(),
            AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                values.iter().all(|value| value.is_ascii())
            }
            AttrValue::StringDict(entries) => entries
                .iter()
                .all(|(key, value)| key.is_ascii() && value.is_ascii()),
            AttrValue::StringListDict(entries) => entries
                .iter()
                .all(|(key, values)| key.is_ascii() && values.iter().all(|value| value.is_ascii())),
            AttrValue::Label(value) | AttrValue::Output(value) => {
                value.as_ref().is_none_or(|value| value.is_ascii())
            }
            AttrValue::Int(_) | AttrValue::Bool(_) | AttrValue::Ints(_) => true,
            AttrValue::LabelKeyedStringDict(entries) | AttrValue::StringKeyedLabelDict(entries) => {
                entries
                    .iter()
                    .all(|(key, value)| key.is_ascii() && value.is_ascii())
            }
            AttrValue::LabelListDict(entries) => entries
                .iter()
                .all(|(key, values)| key.is_ascii() && values.iter().all(|value| value.is_ascii())),
        };
        if !name.is_ascii() || !ascii {
            return Err(format!(
                "non-ASCII tag attribute {name:?} is unsupported in module extension execution"
            ));
        }
    }
    Ok(attrs)
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
                "attribute kind {kind:?} is not supported for module extension tags by the native profile"
            ));
        }
        AttrKind::String => AttrValue::String(string()?.to_owned()),
        AttrKind::StringList => AttrValue::Strings(strings()?),
        AttrKind::StringDict | AttrKind::StringListDict
            if crate::repository_rule::reject_non_string_keys(value).is_err() =>
        {
            return crate::repository_rule::reject_non_string_keys(value)
                .map(|()| AttrValue::Bool(false));
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
        AttrKind::Bool => AttrValue::Bool(crate::repository_rule::convert_bool(name, value)?),
        AttrKind::Label => AttrValue::Label(Some(resolve(string()?)?.to_string())),
        AttrKind::LabelList => AttrValue::Labels(
            strings()?
                .iter()
                .map(|text| resolve(text).map(|label| label.to_string()))
                .collect::<Result<_, _>>()?,
        ),
        AttrKind::StringKeyedLabelDict
        | AttrKind::LabelListDict
        | AttrKind::LabelKeyedStringDict => {
            crate::repository_rule::label_dict_json(name, kind, value, resolve)?
        }
        AttrKind::Output | AttrKind::OutputList => {
            return Err("output tag attributes are unsupported".to_owned());
        }
    })
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
        extension_in("//defs:extensions.bzl", source, &HashMap::new())
    }

    fn schema(attrs: &str) -> BTreeMap<String, AttrSpec> {
        let module = extension(&format!(
            "def impl(ctx):\n    return None\nE = module_extension(impl, tag_classes={{'t': tag_class({attrs})}})"
        ))
        .unwrap();
        unpack(&module.get("E").unwrap()).unwrap().tag_classes["t"].clone()
    }

    fn raw(value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        value
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    #[test]
    fn supplied_tag_values_are_checked_against_values_like_attribute_utils() {
        let schema = schema("{'mode': attr.string(default = 'z', values = ['a', 'b'])}");
        assert_eq!(
            bind(&schema, json!({})).unwrap()["mode"],
            AttrValue::String("z".to_owned())
        );
        assert_eq!(
            bind(&schema, json!({"mode": "c"})).unwrap_err(),
            "in 't' tag, the value for attribute 'mode' has to be one of 'a' or 'b' instead of 'c'"
        );
    }

    fn bind(
        schema: &BTreeMap<String, AttrSpec>,
        value: serde_json::Value,
    ) -> Result<BTreeMap<String, AttrValue>, String> {
        bind_tag(schema, "t", &raw(value), |text| {
            Label::parse(text, "caller")
        })
    }

    #[test]
    fn label_keyed_string_dict_tags_resolve_keys_at_caller() {
        let schema = schema("{'platforms': attr.label_keyed_string_dict()}");
        assert_eq!(
            bind(&schema, json!({})).unwrap()["platforms"],
            AttrValue::LabelKeyedStringDict(vec![])
        );
        assert_eq!(
            bind(&schema, json!({"platforms": {":p": "linux/amd64"}})).unwrap()["platforms"],
            AttrValue::LabelKeyedStringDict(vec![(
                "//caller:p".to_owned(),
                "linux/amd64".to_owned()
            )])
        );
        assert_eq!(
            bind(
                &schema,
                json!({"platforms": {":p": "a", "//caller:p": "b"}})
            )
            .unwrap_err(),
            "in 't' tag, duplicate labels in attribute 'platforms': //caller:p (as [\":p\", \"//caller:p\"])"
        );
        assert_eq!(
            bind(&schema, json!({"platforms": {":p": ["a"]}})).unwrap_err(),
            "in 't' tag, expected value of type 'string' for dict value element, but got [\"a\"] (list)"
        );
    }

    #[test]
    fn label_keyed_tag_conversion_errors_use_bazel_texts() {
        use crate::repository_rule::{DICT_ENTRIES, KEY_REPR, KEY_TYPE};
        let schema = schema("{'platforms': attr.label_keyed_string_dict()}");
        assert_eq!(
            bind(
                &schema,
                json!({"platforms": {DICT_ENTRIES: [[{KEY_REPR: "1", KEY_TYPE: "int"}, "x"]]}})
            )
            .unwrap_err(),
            "in 't' tag, expected value of type 'string' for dict key element, but got 1 (int)"
        );
        assert_eq!(
            bind(&schema, json!({"platforms": {"//:a:b": "x"}})).unwrap_err(),
            "in 't' tag, invalid label '//:a:b' in dict key element: invalid target name 'a:b': target names may not contain ':'"
        );
        assert_eq!(
            bind_tag_where(
                &schema,
                "t",
                &raw(json!({"platforms": {"@nope//:a": "x"}})),
                |text| Label::parse_in(text, "", "", &BTreeMap::new()),
                "to the module 'dep@1.0'",
                &[],
            )
            .unwrap_err(),
            "in 't' tag, no repository visible as '@nope' to the module 'dep@1.0', but referenced by label '@nope//:a' in attribute 'platforms'"
        );
    }

    #[test]
    fn label_dict_tags_resolve_at_caller_and_reject_computed_defaults() {
        let schema = schema(
            "{'files': attr.string_keyed_label_dict(default={'k': ':d'}), 'groups': attr.label_list_dict()}",
        );
        let defaults = bind(&schema, json!({})).unwrap();
        assert_eq!(
            defaults["files"],
            AttrValue::StringKeyedLabelDict(vec![("k".to_owned(), "//defs:d".to_owned())])
        );
        let attrs = bind(
            &schema,
            json!({"files": {"BUILD": ":b"}, "groups": {"g": [":x", "//y:z"]}}),
        )
        .unwrap();
        assert_eq!(
            attrs["files"],
            AttrValue::StringKeyedLabelDict(vec![("BUILD".to_owned(), "//caller:b".to_owned())])
        );
        assert_eq!(
            attrs["groups"],
            AttrValue::LabelListDict(vec![(
                "g".to_owned(),
                vec!["//caller:x".to_owned(), "//y:z".to_owned()]
            )])
        );
        assert_eq!(
            bind(&schema, json!({"groups": {"g": ":x"}})).unwrap_err(),
            "in 't' tag, expected value of type 'list(label)' for dict value element, but got \":x\" (string)"
        );
        for (attrs, expected) in [
            (
                "{'p': attr.label(default=lambda name: None)}",
                "When an attribute value is a function, the attribute must be private",
            ),
            (
                "{'_p': attr.label(default=lambda name: None)}",
                "computed (function) default of tag attribute \"_p\" is not supported by the native profile",
            ),
        ] {
            let error = extension(&format!(
                "E = module_extension(len, tag_classes={{'t': tag_class({attrs})}})"
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{attrs}: {error}");
        }
    }

    #[test]
    fn string_list_dict_schema_snapshot_survives_mutation_and_owner_drop() {
        let module = extension(
            "items = ['before', 'before']\nd = {'z': items, 'a': ()}\nattrs = {'d':attr.string_list_dict(default=d, allow_empty=False)}\nT = tag_class(attrs)\nclasses = {'t':T}\ndef impl(ctx):\n    return ctx\nE = module_extension(impl, tag_classes=classes)\nitems.append('after')\nd['later'] = []\nattrs['extra'] = attr.int()\nclasses['extra'] = tag_class()\n(d['z'] == ['before', 'before', 'after']) or fail('source mutated')",
        ).unwrap();
        let definition = unpack(&module.get("E").unwrap()).unwrap();
        drop(module);
        assert_eq!(definition.tag_classes.len(), 1);
        let schema = &definition.tag_classes["t"];
        assert_eq!(schema.len(), 1);
        assert_eq!(schema["d"].kind, AttrKind::StringListDict);
        assert!(schema["d"].non_empty);
        let expected = AttrValue::StringListDict(vec![
            (
                "z".to_owned(),
                vec!["before".to_owned(), "before".to_owned()],
            ),
            ("a".to_owned(), vec![]),
        ]);
        assert_eq!(schema["d"].default, expected);
        let mut bound = bind(schema, json!({"d":null})).unwrap();
        assert_eq!(bound["d"], expected);
        if let AttrValue::StringListDict(entries) = bound.get_mut("d").unwrap() {
            entries[0].1.push("bound-only".to_owned());
        }
        assert_eq!(schema["d"].default, expected);
        assert_eq!(bind(schema, json!({})).unwrap()["d"], expected);
        let serialized = serde_json::to_value(&schema["d"]).unwrap();
        assert_eq!(serialized["non_empty"], true);
        assert_eq!(
            serialized["default"],
            json!({"StringListDict":[["z",["before","before"]],["a",[]]]})
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
    fn string_dict_tag_schema_snapshot_survives_mutation_and_owner_drop() {
        let module = extension("d={'z':'before', 'a':'alpha'}\ns=attr.string_dict(default=d, allow_empty=False)\nd['z']='between'\nT=tag_class({'d':s})\ndef impl(ctx):\n    return ctx\nE=module_extension(impl, tag_classes={'t':T})\nd['z']='after'\nd['new']='later'").unwrap();
        let definition = unpack(&module.get("E").unwrap()).unwrap();
        drop(module);
        let schema = &definition.tag_classes["t"];
        let expected = AttrValue::StringDict(vec![
            ("z".into(), "before".into()),
            ("a".into(), "alpha".into()),
        ]);
        assert_eq!(schema["d"].kind, AttrKind::StringDict);
        assert!(schema["d"].non_empty);
        assert_eq!(schema["d"].default, expected);
        assert_eq!(bind(schema, json!({})).unwrap()["d"], expected);
        assert_eq!(bind(schema, json!({"d":null})).unwrap()["d"], expected);
        assert_eq!(
            serde_json::to_value(&schema["d"]).unwrap()["default"],
            json!({"StringDict":[["z","before"],["a","alpha"]]})
        );
    }

    #[test]
    fn string_dict_tag_binding_order_none_mandatory_nonempty_and_ascii() {
        let schema = schema("{'d':attr.string_dict(allow_empty=False)}");
        let supplied = raw(json!({"d":{"z":"@missing//p:t", "a":""}}));
        assert_eq!(
            bind_tag(&schema, "t", &supplied, |_| panic!("not a label")).unwrap()["d"],
            AttrValue::StringDict(vec![
                ("z".into(), "@missing//p:t".into()),
                ("a".into(), "".into())
            ])
        );
        for value in [json!({}), json!({"d":null}), json!({"d":{}})] {
            assert_eq!(
                bind(&schema, value).unwrap()["d"],
                AttrValue::StringDict(vec![])
            );
        }
        let required = self::schema(
            "{'d':attr.string_dict(default={'z':'fallback'}, mandatory=True, allow_empty=False)}",
        );
        for value in [json!({}), json!({"d":null})] {
            assert!(
                bind(&required, value)
                    .unwrap_err()
                    .contains("missing mandatory tag attribute")
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
                bind(&schema, json!({"d":value}))
                    .unwrap_err()
                    .contains("expected")
            );
        }
        for value in [json!({"é":"ok"}), json!({"k":"é"})] {
            assert!(
                bind(&schema, json!({"d":value}))
                    .unwrap_err()
                    .contains("non-ASCII tag attribute")
            );
        }
        for default in ["{'é':'ok'}", "{'k':'é'}"] {
            let schema = self::schema(&format!("{{'d':attr.string_dict(default={default})}}"));
            assert!(
                bind(&schema, json!({}))
                    .unwrap_err()
                    .contains("non-ASCII tag attribute")
            );
        }
    }

    #[test]
    fn string_list_dict_binding_preserves_order_and_ignores_non_empty() {
        let schema = schema("{'d':attr.string_list_dict(allow_empty=False)}");
        assert_eq!(
            bind(&schema, json!({"d":{"z":["same","same"],"a":[]}})).unwrap()["d"],
            AttrValue::StringListDict(vec![
                ("z".to_owned(), vec!["same".to_owned(), "same".to_owned()]),
                ("a".to_owned(), vec![]),
            ])
        );
        for value in [json!({}), json!({"d":null}), json!({"d":{}})] {
            assert_eq!(
                bind(&schema, value).unwrap()["d"],
                AttrValue::StringListDict(vec![])
            );
        }
        let required = self::schema(
            "{'d':attr.string_list_dict(default={'z':['fallback']}, mandatory=True, allow_empty=False)}",
        );
        for value in [json!({}), json!({"d":null})] {
            assert!(
                bind(&required, value)
                    .unwrap_err()
                    .contains("missing mandatory tag attribute \"d\"")
            );
        }
        assert_eq!(
            bind(&required, json!({"d":{}})).unwrap()["d"],
            AttrValue::StringListDict(vec![])
        );
    }

    #[test]
    fn string_list_dict_rejects_invalid_shapes_and_unicode_without_partial_binding() {
        let schema = schema("{'a':attr.string_list_dict(), 'z':attr.string_list_dict()}");
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
            let error = bind_tag(&schema, "t", &supplied, |_| panic!("not a label")).unwrap_err();
            assert!(
                error.contains("tag attribute \"z\": expected")
                    || error.contains("tag attribute \"z\": dict key"),
                "{error}"
            );
            assert_eq!(supplied, original);
            assert_eq!(schema["a"].default, AttrValue::StringListDict(vec![]));
        }
        for value in [json!({"é":[]}), json!({"k":["ascii","é"]})] {
            assert!(
                bind(&schema, json!({"z":value}))
                    .unwrap_err()
                    .contains("non-ASCII tag attribute \"z\"")
            );
        }
        for default in ["{'é':[]}", "{'k':('ascii', 'é')}"] {
            let schema = self::schema(&format!("{{'d':attr.string_list_dict(default={default})}}"));
            assert!(
                bind(&schema, json!({}))
                    .unwrap_err()
                    .contains("non-ASCII tag attribute \"d\"")
            );
        }
    }

    #[test]
    fn repeated_descriptor_and_class_defaults_are_bounded_before_cloning() {
        for source in [
            "d = attr.string(default='x' * 1048576)\nT = tag_class({'a' + str(i): d for i in range(600)})",
            "T = tag_class({'value':attr.string(default='x' * 1048576)})\nE = module_extension(len, tag_classes={'t' + str(i):T for i in range(600)})",
            "T = tag_class({}, doc='x' * 1048576)\nE = module_extension(len, tag_classes={'t' + str(i):T for i in range(600)})",
            "d = attr.string_list_dict(default={'z':['x' * 1048576], 'a':[]})\nT = tag_class({'a' + str(i):d for i in range(600)})",
            "T = tag_class({'d':attr.string_list_dict(default={'z':['x' * 1048576], 'a':[]})})\nE = module_extension(len, tag_classes={'t' + str(i):T for i in range(600)})",
            "d = attr.string_dict(default={'z':'x' * 1048576, 'a':''})\nT = tag_class({'a' + str(i):d for i in range(600)})",
            "T = tag_class({'d':attr.string_dict(default={'z':'x' * 1048576, 'a':''})})\nE = module_extension(len, tag_classes={'t' + str(i):T for i in range(600)})",
        ] {
            let error = extension(source).unwrap_err();
            assert!(
                error.contains("host preparation weighted byte limit"),
                "{error}"
            );
        }
    }

    #[test]
    fn private_aliases_reexports_and_unexported_values_unpack_without_identity() {
        let original = extension(
            "def _impl(ctx):\n    return ctx\n_private = module_extension(_impl)\nAlias = _private\nOther = module_extension(implementation=_impl)\n(type(Alias) == 'ModuleExtension') or fail('type')\n(Alias == _private and Alias != Other) or fail('identity')",
        ).unwrap();
        let private = original.get_any_visibility("_private").unwrap().0;
        let reexport = extension_in(
            "//other:alias.bzl",
            "load('//defs:extensions.bzl', 'Alias')\nRenamed = Alias",
            &HashMap::from([("//defs:extensions.bzl", &original)]),
        )
        .unwrap();
        let renamed = reexport.get("Renamed").unwrap();
        drop(original);
        drop(reexport);
        assert!(
            unpack(&private)
                .unwrap()
                .implementation
                .value()
                .ptr_eq(unpack(&renamed).unwrap().implementation.value())
        );
        let unexported = Module::with_temp_heap(|module| {
            let context = context("//defs:extensions.bzl");
            let ast = AstModule::parse(
                "//defs:extensions.bzl",
                "def impl(ctx):\n    return ctx\nmodule_extension(impl)".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let value = eval.eval_module(ast, &test_globals()).unwrap();
                module.set("Hidden", value);
            }
            module.freeze().unwrap().get("Hidden").unwrap()
        });
        assert!(unpack(&unexported).is_ok());
    }

    #[test]
    fn implementation_retains_private_transitive_helpers_after_owners_drop() {
        let helper = extension_in(
            "//helper:helper.bzl",
            "def _private(x):\n    return x + 1\ndef run(x):\n    return _private(x)",
            &HashMap::new(),
        )
        .unwrap();
        let module = extension_in("//defs:extensions.bzl", "load('//helper:helper.bzl', 'run')\ndef _impl(ctx):\n    return run(ctx)\nE = module_extension(_impl)", &HashMap::from([("//helper:helper.bzl", &helper)])).unwrap();
        let definition = unpack(&module.get("E").unwrap()).unwrap();
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
    fn accepts_builtin_and_unchecked_arity_without_running_callback() {
        let module = extension("def noargs():\n    fail('must not run')\nE = module_extension(noargs)\nBuiltin = module_extension(len)").unwrap();
        assert!(unpack(&module.get("E").unwrap()).is_ok());
        assert!(unpack(&module.get("Builtin").unwrap()).is_ok());
        assert!(
            extension("E = module_extension(1)")
                .unwrap_err()
                .contains("must be callable")
        );
        for source in ["E = 1", "E = tag_class()", "def E(ctx):\n    return ctx"] {
            assert!(
                unpack(&extension(source).unwrap().get("E").unwrap())
                    .unwrap_err()
                    .contains("frozen module_extension")
            );
        }
    }

    #[test]
    fn schemas_and_docs_are_owned_opaque_and_unhashable() {
        let module = extension("items = ['before']\na = {'items': attr.string_list(default=items)}\nT = tag_class(a, doc='é')\na['later'] = attr.int()\nitems.append('after')\nclasses = {'_t': T}\ndef impl(ctx):\n    return None\nE = module_extension(impl, tag_classes=classes, doc='é')\nclasses['later'] = tag_class()\n(repr(T) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.TagClass>') or fail('repr')\n(repr(E) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleExtension>') or fail('repr')").unwrap();
        let definition = unpack(&module.get("E").unwrap()).unwrap();
        assert_eq!(definition.tag_classes.len(), 1);
        let schema = &definition.tag_classes["_t"];
        assert_eq!(schema.len(), 1);
        assert_eq!(
            schema["items"].default,
            AttrValue::Strings(vec!["before".to_owned()])
        );
        let class = module.get("T").unwrap();
        assert_eq!(
            class
                .value()
                .downcast_ref::<TagClass>()
                .unwrap()
                .doc
                .as_deref(),
            Some("é")
        );
        for expression in ["{T: 1}", "T.schema", "T.doc", "T.attrs"] {
            assert!(
                extension(&format!("T = tag_class()\nx = {expression}")).is_err(),
                "{expression}"
            );
        }
        assert!(extension("T = tag_class()\nT.doc = 'changed'").is_err());
    }

    #[test]
    fn constructors_require_initialization_not_build_module_or_callback() {
        for filename in ["BUILD", "MODULE.bazel", "callback.star"] {
            for expression in ["tag_class()", "module_extension(len)"] {
                Module::with_temp_heap(|module| {
                    let ast =
                        AstModule::parse(filename, format!("E = {expression}"), &Dialect::Standard)
                            .unwrap();
                    let error = Evaluator::new(&module)
                        .eval_module(ast, &test_globals())
                        .unwrap_err()
                        .to_string();
                    assert!(error.contains("extension initialization"), "{error}");
                });
            }
        }
        assert!(
            extension("def impl(ctx):\n    return None\nE = module_extension(impl)\nE()").is_err()
        );
    }

    #[test]
    fn signatures_and_bounded_options_are_explicit() {
        extension("def impl(ctx):\n    return None\nT = tag_class(attrs={}, doc=None)\nEmpty = tag_class()\nE = module_extension(implementation=impl, tag_classes={}, doc=None, environ=[], os_dependent=False, arch_dependent=False, facts_version=0)").unwrap();
        extension("def impl(ctx):\n    return None\nE = module_extension(impl, environ=['HOME', 'PATH', 'HOME'])").unwrap();
        for (expression, expected) in [
            ("tag_class(None)", "dict"),
            ("tag_class({}, 'doc')", "positional"),
            ("tag_class(unknown=1)", "unknown"),
            ("tag_class(doc=1)", "doc"),
            ("module_extension(impl, {})", "positional"),
            ("module_extension(impl, unknown=False)", "unknown"),
            ("module_extension(impl, doc=1)", "doc"),
            ("module_extension(impl, environ=None)", "list of strings"),
            ("module_extension(impl, environ=())", "list of strings"),
            ("module_extension(impl, environ=[1])", "list of strings"),
            (
                "module_extension(impl, environ=['x' * 4097])",
                "repository environment name exceeds 4096 bytes",
            ),
            ("module_extension(impl, os_dependent=None)", "bool"),
            ("module_extension(impl, arch_dependent=None)", "bool"),
            ("module_extension(impl, os_dependent=1)", "bool"),
            ("module_extension(impl, arch_dependent='yes')", "bool"),
            ("module_extension(impl, facts_version=-1)", "nonnegative"),
            (
                "module_extension(impl, facts_version=1)",
                "nonzero facts_version is unsupported",
            ),
            (
                "module_extension(impl, facts_version=2147483647)",
                "nonzero facts_version is unsupported",
            ),
            ("module_extension(impl, facts_version=2147483648)", "int"),
            ("module_extension(impl, facts_version=-2147483649)", "int"),
            ("module_extension(impl, facts_version=True)", "int"),
        ] {
            let error = extension(&format!(
                "def impl(ctx):\n    return None\nE = {expression}"
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[test]
    fn host_flags_survive_private_factory_alias_freeze_and_owner_drop() {
        for os in [false, true] {
            for arch in [false, true] {
                let helper = extension_in("//helper:factory.bzl", &format!(
                    "def _impl(ctx):\n    return None\ndef make():\n    return module_extension(_impl, os_dependent={}, arch_dependent={})\n",
                    if os { "True" } else { "False" }, if arch { "True" } else { "False" }
                ), &HashMap::new()).unwrap();
                let original = extension_in(
                    "//defs:extensions.bzl",
                    "load('//helper:factory.bzl', 'make')\n_private = make()\nAlias = _private\n",
                    &HashMap::from([("//helper:factory.bzl", &helper)]),
                )
                .unwrap();
                let alias = extension_in(
                    "//other:alias.bzl",
                    "load('//defs:extensions.bzl', 'Alias')\nRenamed = Alias\n",
                    &HashMap::from([("//defs:extensions.bzl", &original)]),
                )
                .unwrap();
                let definition = unpack(&alias.get("Renamed").unwrap()).unwrap();
                drop(alias);
                drop(original);
                drop(helper);
                assert_eq!(
                    (definition.os_dependent, definition.arch_dependent),
                    (os, arch)
                );
                Module::with_temp_heap(|module| {
                    let implementation = module
                        .heap()
                        .access_owned_frozen_value(&definition.implementation);
                    assert!(
                        Evaluator::new(&module)
                            .eval_function(implementation, &[Value::new_none()], &[])
                            .unwrap()
                            .is_none()
                    );
                });
            }
        }
        let default = extension("E = module_extension(len)").unwrap();
        let definition = unpack(&default.get("E").unwrap()).unwrap();
        assert!(!definition.os_dependent && !definition.arch_dependent);
    }

    #[test]
    fn schema_validation_rejects_wrong_descriptors_kinds_and_names() {
        for (attrs, expected) in [
            ("[]", "dict"),
            ("{'x': 1}", "descriptor"),
            ("{1: attr.string()}", "strings"),
            ("{'bad-name': attr.string()}", "invalid"),
            ("{'é': attr.string()}", "invalid"),
            ("{'1bad': attr.string()}", "invalid"),
            ("{'': attr.string()}", "invalid"),
            ("{'out': attr.output()}", "unsupported"),
            ("{'out': attr.output_list()}", "unsupported"),
        ] {
            let error = extension(&format!("T = tag_class({attrs})")).unwrap_err();
            assert!(error.contains(expected), "{attrs}: {error}");
        }
        for (classes, expected) in [
            ("None", "dict"),
            ("[]", "dict"),
            ("{1: tag_class()}", "strings"),
            ("{'t': 1}", "tag_class value"),
            ("{'bad-name': tag_class()}", "invalid"),
            ("{'': tag_class()}", "invalid"),
            ("{'1t': tag_class()}", "invalid"),
            ("{'é': tag_class()}", "invalid"),
        ] {
            let error = extension(&format!("E = module_extension(len, tag_classes={classes})"))
                .unwrap_err();
            assert!(error.contains(expected), "{classes}: {error}");
        }
    }

    #[test]
    fn binder_none_is_omission_even_for_unknown_or_invalid_names() {
        let schema = schema("{'x': attr.string(default='fallback')}");
        assert_eq!(
            bind(
                &schema,
                json!({"x":null,"unknown":null,"bad-name":null,"é":null})
            )
            .unwrap(),
            BTreeMap::from([("x".to_owned(), AttrValue::String("fallback".to_owned()))])
        );
        for value in [
            json!({"unknown":false}),
            json!({"name":"repo"}),
            json!({"tags":[]}),
            json!({"visibility":[]}),
            json!({"dev_dependency":true}),
        ] {
            assert!(
                bind(&schema, value)
                    .unwrap_err()
                    .contains("unknown tag attribute")
            );
        }
    }

    #[test]
    fn binder_keeps_private_and_reserved_names_and_requires_mandatory_before_default() {
        let schema = schema(
            "{'_private': attr.string(default='fallback', mandatory=True), 'name': attr.string(), 'tags': attr.string_list(), 'visibility': attr.string(), 'deprecation': attr.bool(), 'dev_dependency': attr.int(default=7)}",
        );
        for value in [json!({}), json!({"_private":null})] {
            assert!(
                bind(&schema, value)
                    .unwrap_err()
                    .contains("missing mandatory tag attribute \"_private\"")
            );
        }
        let attrs = bind(&schema, json!({"_private":"override", "name":"", "tags":["tag"], "visibility":"private", "deprecation":true})).unwrap();
        assert_eq!(attrs.len(), 6);
        assert_eq!(attrs["_private"], AttrValue::String("override".to_owned()));
        assert_eq!(attrs["name"], AttrValue::String(String::new()));
        assert_eq!(attrs["tags"], AttrValue::Strings(vec!["tag".to_owned()]));
        assert_eq!(attrs["visibility"], AttrValue::String("private".to_owned()));
        assert_eq!(attrs["deprecation"], AttrValue::Bool(true));
        assert_eq!(attrs["dev_dependency"], AttrValue::Int(7));
    }

    #[test]
    fn binder_checks_exact_json_types_and_int32_bounds() {
        let schema = schema(
            "{'s': attr.string(), 'ss': attr.string_list(), 'i': attr.int(), 'b': attr.bool(), 'l': attr.label(), 'ls': attr.label_list()}",
        );
        for value in [
            json!({"s":1}),
            json!({"ss":"x"}),
            json!({"ss":["x",1]}),
            json!({"i":true}),
            json!({"i":1.0}),
            json!({"i":2147483648_i64}),
            json!({"i":-2147483649_i64}),
            json!({"i":u64::MAX}),
            json!({"b":2}),
            json!({"b":-1}),
            json!({"b":"true"}),
            json!({"l":[]}),
            json!({"l":{"label":"//:x"}}),
            json!({"ls":["//:ok",false]}),
        ] {
            assert!(bind(&schema, value.clone()).is_err(), "{value}");
        }
        // Type.BOOLEAN.convert: 0 and 1 convert, anything else fails with
        // Bazel's text (genuine Bazel 9.3.0 via ext.t(flag = 0/1/2)).
        for (raw, flag) in [(json!(0), false), (json!(1), true), (json!(true), true)] {
            let attrs = bind(&schema, json!({"b":raw})).unwrap();
            assert_eq!(attrs["b"], AttrValue::Bool(flag), "{raw}");
        }
        for (raw, shown) in [(json!(2), "2 (int)"), (json!("true"), "\"true\" (string)")] {
            assert_eq!(
                bind(&schema, json!({"b":raw})).unwrap_err(),
                format!(
                    "in 't' tag, expected one of [False, True, 0, 1] for attribute 'b', but got {shown}"
                )
            );
        }
        for i in [i32::MIN, i32::MAX] {
            let attrs = bind(&schema, json!({"s":"", "ss":["a","b"], "i":i, "b":false})).unwrap();
            assert_eq!(attrs["i"], AttrValue::Int(i));
            assert_eq!(attrs["b"], AttrValue::Bool(false));
            assert_eq!(attrs["l"], AttrValue::Label(None));
            assert_eq!(attrs["ls"], AttrValue::Labels(vec![]));
        }
    }

    #[test]
    fn lexical_label_defaults_survive_helper_factory_and_caller_binding() {
        let helper = extension_in("//helper:factory.bzl", "def make():\n    return tag_class({'l': attr.label(default=':tool'), 'ls': attr.label_list(default=[':one', '//other:two'])})", &HashMap::new()).unwrap();
        let module = extension_in("//defs:extensions.bzl", "load('//helper:factory.bzl', 'make')\nE = module_extension(len, tag_classes={'t': make()})", &HashMap::from([("//helper:factory.bzl", &helper)])).unwrap();
        let definition = unpack(&module.get("E").unwrap()).unwrap();
        let schema = &definition.tag_classes["t"];
        let defaults = bind_tag(schema, "t", &raw(json!({"l":null})), |_| {
            panic!("defaults must not be rebound")
        })
        .unwrap();
        assert_eq!(
            defaults["l"],
            AttrValue::Label(Some("//helper:tool".to_owned()))
        );
        assert_eq!(
            defaults["ls"],
            AttrValue::Labels(vec!["//helper:one".to_owned(), "//other:two".to_owned()])
        );
        let supplied = bind(schema, json!({"l":":tool", "ls":[":one","//other:two"]})).unwrap();
        assert_eq!(
            supplied["l"],
            AttrValue::Label(Some("//caller:tool".to_owned()))
        );
        assert_eq!(
            supplied["ls"],
            AttrValue::Labels(vec!["//caller:one".to_owned(), "//other:two".to_owned()])
        );
        assert!(
            bind_tag(schema, "t", &raw(json!({"l":"@missing//:tool"})), |_| Err(
                "not visible from caller".to_owned()
            ))
            .unwrap_err()
            .contains("not visible from caller")
        );
    }

    #[test]
    fn binder_guards_all_bound_strings_and_keys_including_defaults() {
        let schema = schema(
            "{'s':attr.string(), 'ss':attr.string_list(), 'l':attr.label(), 'ls':attr.label_list()}",
        );
        for value in [json!({"s":"é"}), json!({"ss":["ascii","é"]})] {
            assert!(
                bind(&schema, value)
                    .unwrap_err()
                    .contains("non-ASCII tag attribute")
            );
        }
        for attrs in [
            "{'s':attr.string(default='é')}",
            "{'ss':attr.string_list(default=['é'])}",
        ] {
            assert!(
                bind(&self::schema(attrs), json!({}))
                    .unwrap_err()
                    .contains("non-ASCII tag attribute")
            );
        }
        for value in [json!({"l":":x"}), json!({"ls":[":x"]})] {
            assert!(
                bind_tag(&schema, "t", &raw(value), |_| Ok(Label {
                    repository: String::new(),
                    package: "é".to_owned(),
                    name: "x".to_owned()
                }))
                .unwrap_err()
                .contains("non-ASCII tag attribute")
            );
        }
        let mut host_schema = schema.clone();
        host_schema.insert("é".to_owned(), schema["s"].clone());
        assert!(
            bind(&host_schema, json!({}))
                .unwrap_err()
                .contains("non-ASCII tag attribute")
        );
    }
}
