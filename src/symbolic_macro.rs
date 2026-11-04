//! Exported, closure-owning symbolic macro definitions. User attributes and
//! finalizers are retained for documentation (crate::starlark_doc_extract)
//! but an invocation of such a macro fails explicitly.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::none::NoneType;
use starlark::values::{Value, ValueLike};
use std::fmt;
use std::sync::OnceLock;

use crate::loading::ExtensionContext;

// Only the generated marker/coercion/trace implementations require unsafe.
// The callback is traced/frozen; the remaining fields own their Rust data.
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
    pub(super) struct MacroDefinitionGen<V> {
        pub implementation: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub definition_label: String,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
        /// The effective user attributes in declaration order, `finalizer`
        /// and `doc` (untrimmed): documented by crate::starlark_doc_extract;
        /// an invocation with attributes or as a finalizer is unsupported.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub attrs: Vec<(String, crate::custom_rule::AttrSpec)>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub finalizer: bool,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub doc: Option<String>,
    }

    impl<V> fmt::Display for MacroDefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.exported.get() {
                Some(name) => write!(f, "<macro {name}>"),
                // MacroFunction.repr (analysis/starlark/StarlarkRuleClassFunctions.java:1683-1689).
                None => f.write_str("<macro>"),
            }
        }
    }

    starlark_complex_value!(pub(super) MacroDefinition);

    #[starlark_value(type = "macro")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for MacroDefinitionGen<V>
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
                        "macro must be exported in its defining extension {}",
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
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            if !self.attrs.is_empty() {
                return Err(starlark::Error::new_other(anyhow!(
                    "macro user attributes are unsupported"
                )));
            }
            if self.finalizer {
                return Err(starlark::Error::new_other(anyhow!(
                    "macro finalizer=True is unsupported"
                )));
            }
            crate::package::invoke_symbolic_macro(
                eval,
                &self.definition_label,
                self.exported.get().map(String::as_str),
                self.implementation.to_value(),
                args,
            )
            .map_err(starlark::Error::new_other)
        }
    }
}

/// What `starlark_doc_extract` documents of a `macro()` value
/// (ModuleInfoExtractor.visitMacroFunction).
pub(crate) fn macro_doc(value: Value<'_>) -> Option<crate::starlark_doc_extract::MacroDoc> {
    let definition = values::MacroDefinition::from_value(value)?;
    Some(crate::starlark_doc_extract::MacroDoc {
        exported: definition.exported.get().cloned(),
        file: definition.definition_label.clone(),
        doc: definition.doc.clone(),
        finalizer: definition.finalizer,
        attrs: definition
            .attrs
            .iter()
            .map(|(name, spec)| crate::starlark_doc_extract::DocAttr {
                name: name.clone(),
                spec: spec.clone(),
                starlark_defined: true,
            })
            .collect(),
    })
}

#[starlark_module]
pub(crate) fn globals(builder: &mut GlobalsBuilder) {
    fn r#macro<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named, default = AllocDict::EMPTY)] attrs: Value<'v>,
        #[starlark(require = named, default = NoneType)] inherit_attrs: Value<'v>,
        #[starlark(require = named, default = false)] finalizer: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
            .ok_or_else(|| {
                anyhow!("macro() definitions are only allowed during .bzl initialization")
            })?;
        // In pinned Starlark 0.14.2 only Def/FrozenDef expose a parameter spec,
        // including private closures. Arity is intentionally checked at invocation.
        if implementation.parameters_spec().is_none() {
            bail!("macro implementation must be a Starlark function");
        }
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("macro doc must be a string or None");
        }
        if !inherit_attrs.is_none() {
            bail!("macro inherit_attrs is unsupported");
        }
        let attrs =
            DictRef::from_value(attrs).ok_or_else(|| anyhow!("macro attrs must be a dict"))?;
        let mut budget = crate::repository_budget::HostBudget::default();
        let mut effective = Vec::new();
        for (name, descriptor) in attrs.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| anyhow!("macro attribute names must be strings"))?;
            budget.string(name).map_err(anyhow::Error::msg)?;
            if matches!(name, "name" | "visibility") {
                bail!("macro attribute {name:?} is reserved");
            }
            if !descriptor.is_none() {
                let Some(spec) = crate::custom_rule::attribute_spec(descriptor) else {
                    bail!("macro attribute {name:?} must be an attr descriptor or None");
                };
                budget.spec(&spec).map_err(anyhow::Error::msg)?;
                effective.push((name.to_owned(), spec));
            }
        }
        Ok(eval.heap().alloc(values::MacroDefinitionGen {
            implementation,
            definition_label: context.label.to_string(),
            exported: OnceLock::new(),
            attrs: effective,
            finalizer,
            doc: doc.unpack_str().map(str::to_owned),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{FrozenModule, Globals, Module};
    use starlark::eval::ReturnFileLoader;
    use starlark::syntax::{AstModule, Dialect};
    use std::collections::HashMap;

    fn extension(
        label: &str,
        source: &str,
        loads: &HashMap<&str, &FrozenModule>,
        globals: &Globals,
    ) -> Result<FrozenModule, String> {
        Module::with_temp_heap(|module| {
            let context = ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse_canonical(label).unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    ".".into(),
                )),
            };
            let loader = ReturnFileLoader { modules: loads };
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.set_loader(&loader);
                eval.eval_module(
                    AstModule::parse(label, source.to_owned(), &Dialect::Standard).unwrap(),
                    globals,
                )
                .map_err(|e| e.to_string())?;
            }
            module.freeze().map_err(|e| format!("{e:?}"))
        })
    }

    #[test]
    fn symbolic_macro_constructor_signature_and_attribute_profile() {
        let globals = crate::package::bzl_globals();
        for (call, expected) in [
            ("macro()", "implementation"),
            // BuiltinFunction.java:420-423: `implementation` is named-only.
            ("macro(impl)", "macro() got unexpected positional argument"),
            ("macro(implementation=impl, unknown=None)", "unknown"),
            ("macro(implementation=impl, attrs=None)", "dict"),
            ("macro(implementation=impl, attrs=[])", "dict"),
            ("macro(implementation=impl, attrs={1:None})", "strings"),
            (
                "macro(implementation=impl, attrs={'name':None})",
                "reserved",
            ),
            (
                "macro(implementation=impl, attrs={'visibility':None})",
                "reserved",
            ),
            ("macro(implementation=impl, attrs={'x':1})", "descriptor"),
            (
                "macro(implementation=impl, inherit_attrs=impl)",
                "unsupported",
            ),
            ("macro(implementation=impl, doc=1)", "string or None"),
        ] {
            let source = format!("def impl():\n    pass\nm = {call}");
            let error =
                extension("//defs:macro.bzl", &source, &HashMap::new(), &globals).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
        for call in [
            "macro(implementation=impl)",
            "macro(implementation=impl, attrs={}, inherit_attrs=None, finalizer=False, doc=None)",
            "macro(implementation=impl, attrs={'ignored':None, '':None}, doc='help')",
            // Retained for documentation; invoking them fails explicitly.
            "macro(implementation=impl, attrs={'x':attr.string()})",
            "macro(implementation=impl, finalizer=True)",
        ] {
            // Zero-argument implementations are accepted at construction.
            extension(
                "//defs:macro.bzl",
                &format!("def impl():\n    pass\nm = {call}"),
                &HashMap::new(),
                &globals,
            )
            .unwrap();
        }
    }

    #[test]
    fn symbolic_macro_implementation_predicate_rejects_native_and_descriptors() {
        for implementation in [
            "str",
            "len",
            "native.filegroup",
            "provider()",
            "rule(implementation=rule_impl)",
            "macro(implementation=impl)",
            "1",
            "None",
        ] {
            let error = extension("//defs:macro.bzl", &format!("def rule_impl(ctx):\n    return []\ndef impl(name, visibility):\n    pass\nm = macro(implementation={implementation})"), &HashMap::new(), &crate::package::bzl_globals()).unwrap_err();
            assert!(
                error.contains("Starlark function"),
                "{implementation}: {error}"
            );
        }
    }

    #[test]
    fn symbolic_macro_private_first_export_alias_reexport_and_factory_owner() {
        let globals = crate::package::bzl_globals();
        let helpers = extension(
            "//helper:defs.bzl",
            r#"
def factory():
    captured = ["alive"]
    def implementation(name, visibility):
        captured[0] == "alive" or fail("lost closure")
    return macro(implementation=implementation)
_first = factory()
alias = _first
def closure_factory():
    def implementation(name, visibility):
        pass
    return implementation
closure = closure_factory()
"#,
            &HashMap::new(),
            &globals,
        )
        .unwrap();
        let defs = extension(
            "//defs:macros.bzl",
            r#"
load("helper", "alias", "factory", "closure")
reexport = alias
local = factory()
frozen_closure = macro(implementation=closure)
"#,
            &HashMap::from([("helper", &helpers)]),
            &globals,
        )
        .unwrap();
        for (name, label, export) in [
            ("reexport", "//helper:defs.bzl", "_first"),
            ("local", "//defs:macros.bzl", "local"),
            ("frozen_closure", "//defs:macros.bzl", "frozen_closure"),
        ] {
            let owned = defs.get(name).unwrap();
            let descriptor = owned
                .value()
                .downcast_ref::<values::FrozenMacroDefinition>()
                .unwrap();
            assert_eq!(descriptor.definition_label, label);
            assert_eq!(descriptor.exported.get().map(String::as_str), Some(export));
            assert!(
                descriptor
                    .implementation
                    .to_value()
                    .parameters_spec()
                    .is_some()
            );
        }
    }

    #[test]
    fn symbolic_macro_constructor_shared_bzl_globals_and_phase_guards() {
        for globals in [
            crate::package::bzl_globals(),
            crate::package::module_bzl_globals(),
        ] {
            let defs = "def impl(name, visibility):\n    pass\nm = macro(implementation=impl)\n";
            extension("//defs:macros.bzl", defs, &HashMap::new(), &globals).unwrap();
            let error = extension(
                "//defs:macros.bzl",
                &format!("{defs}m(name='x')"),
                &HashMap::new(),
                &globals,
            )
            .unwrap_err();
            assert!(error.contains("BUILD evaluation"), "{error}");
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                let error = eval
                    .eval_module(
                        AstModule::parse("outside.bzl", defs.to_owned(), &Dialect::Standard)
                            .unwrap(),
                        &globals,
                    )
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(".bzl initialization"), "{error}");
            });
        }
    }
}
