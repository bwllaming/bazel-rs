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
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use crate::custom_rule::{AttrKind, AttrSpec, AttrValue, attribute_spec};
use crate::label::Label;
use crate::loading::ExtensionContext;

#[derive(Clone, Debug)]
pub struct RuleDefinition {
    pub implementation: OwnedFrozenValue,
    pub schema: BTreeMap<String, AttrSpec>,
    pub identity: String,
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
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.identity() {
                Some(identity) => write!(f, "<repository_rule {identity}>"),
                None => write!(
                    f,
                    "<unexported repository_rule from {}>",
                    self.definition_label
                ),
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
            _me: Value<'v>,
            _args: &Arguments<'v, '_>,
            _eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            Err(starlark::Error::new_other(anyhow!(
                "repository rule calls are unsupported outside recorded MODULE/direct execution"
            )))
        }
    }
}
use values::{FrozenRepoDefinition, RepoDefinitionGen};

/// Extract an exported frozen definition while retaining its implementation heap.
/// Transitive function registries belong to the loader, not this value.
pub fn unpack(value: &OwnedFrozenValue) -> Result<RuleDefinition, String> {
    let definition = value
        .value()
        .downcast_ref::<FrozenRepoDefinition>()
        .ok_or_else(|| "expected a frozen repository_rule definition".to_owned())?;
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
        identity,
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
        if local {
            bail!("repository_rule(local=True) is unsupported");
        }
        if configure {
            bail!("repository_rule(configure=True) is unsupported");
        }
        let environ = ListRef::from_value(environ)
            .ok_or_else(|| anyhow!("repository_rule environ must be a list of strings"))?;
        if !environ.is_empty() {
            bail!("repository_rule with nonempty environ is unsupported");
        }
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
                let spec = attribute_spec(descriptor).ok_or_else(|| {
                    anyhow!("repository attribute {name:?} must be an attr descriptor")
                })?;
                if matches!(spec.kind, AttrKind::Output | AttrKind::OutputList) {
                    bail!("output attributes are invalid for repository rules: {name:?}");
                }
                schema.insert(name.to_owned(), spec);
            }
        }
        Ok(eval.heap().alloc(RepoDefinitionGen {
            implementation,
            schema,
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
        let parsed = parse_attribute(&spec.kind, value, &resolve)
            .map_err(|e| format!("repository attribute {name:?}: {e}"))?;
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
        let ascii = match value {
            AttrValue::String(value) => value.is_ascii(),
            AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                values.iter().all(|value| value.is_ascii())
            }
            AttrValue::Label(value) | AttrValue::Output(value) => {
                value.as_ref().is_none_or(|value| value.is_ascii())
            }
            AttrValue::Int(_) | AttrValue::Bool(_) => true,
        };
        if !name.is_ascii() || !ascii {
            return Err(format!(
                "non-ASCII repository attribute {name:?} is unsupported in direct repository execution"
            ));
        }
    }
    Ok(attrs)
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
        AttrKind::String => AttrValue::String(string()?.to_owned()),
        AttrKind::StringList => AttrValue::Strings(strings()?),
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
        AttrKind::Output | AttrKind::OutputList => {
            return Err("output attributes are invalid for repository rules".to_owned());
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
    fn execution_boundary_rejects_host_supplied_or_default_unicode_attributes() {
        let strings = definition("{'text':attr.string(), 'items':attr.string_list()}");
        for supplied in [json!({"text":"é"}), json!({"items":["ascii","é"]})] {
            assert!(
                bind(&strings, supplied)
                    .unwrap_err()
                    .contains("non-ASCII repository attribute")
            );
        }
        // Definitions loaded through ordinary APIs can bypass the dedicated
        // source profile. The binding boundary still checks their defaults.
        let unicode_default = definition("{'text':attr.string(default='é')}");
        assert!(
            bind(&unicode_default, json!({}))
                .unwrap_err()
                .contains("non-ASCII repository attribute")
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
        assert!(
            r.implementation
                .value()
                .ptr_eq(other.implementation.value())
        );
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
    fn constructor_rejects_unsupported_options_and_bad_signatures() {
        for (expression, expected) in [
            ("repository_rule(impl, local=True)", "local=True"),
            ("repository_rule(impl, configure=True)", "configure=True"),
            (
                "repository_rule(impl, environ=['HOME'])",
                "nonempty environ",
            ),
            ("repository_rule(impl, environ=None)", "list of strings"),
            ("repository_rule(impl, environ=())", "list of strings"),
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
}
