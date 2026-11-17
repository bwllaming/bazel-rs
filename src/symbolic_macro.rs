//! Exported, closure-owning symbolic macro definitions (packages/MacroClass.java,
//! StarlarkRuleClassFunctions.macro): user attributes, attributes inherited
//! from a rule, a macro or the common attributes (`inherit_attrs`), and their
//! documentation (crate::starlark_doc_extract). Finalizers are retained for
//! documentation but an invocation of one fails explicitly.

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

use crate::custom_rule::{AttrDeclaration, AttrKind, AttrSpec};
use crate::loading::ExtensionContext;

/// One attribute of a symbolic macro besides `name` and `visibility`
/// (MacroClass's AttributeProvider).
#[derive(Clone, Debug, Allocative)]
pub(crate) struct MacroAttr {
    pub name: String,
    /// The attribute type, default and mandatoriness (a native type is
    /// mapped to the Starlark attribute kind with the same conversion).
    pub spec: AttrSpec,
    /// Attribute.isConfigurable: non-None values reach the implementation
    /// as `select()`s.
    pub configurable: bool,
    /// Attribute.starlarkDefined: converted when the macro is instantiated
    /// (BuildType.copyAndLiftStarlarkValue), whose errors stop evaluation;
    /// others only when its attributes are populated, whose errors are
    /// package errors.
    pub starlark_defined: bool,
    /// BuildType.TRISTATE.
    pub tristate: bool,
    /// An optional inherited attribute's default is forced to None
    /// (StarlarkRuleClassFunctions.java:478-486).
    pub default_none: bool,
    /// A NODEP label type (LabelClass.NONDEP_REFERENCE).
    pub nodep: bool,
}

impl MacroAttr {
    /// LabelClass.DEPENDENCY of a public attribute
    /// (MacroInstance.visitExplicitAttributeLabels, :306-319).
    pub(crate) fn dependency(&self) -> bool {
        !self.nodep
            && !self.name.starts_with('_')
            && matches!(
                self.spec.kind,
                AttrKind::Label
                    | AttrKind::LabelList
                    | AttrKind::LabelKeyedStringDict
                    | AttrKind::StringKeyedLabelDict
                    | AttrKind::LabelListDict
            )
    }

    pub(crate) fn value_type(&self) -> crate::custom_rule::MacroAttrType<'_> {
        crate::custom_rule::MacroAttrType {
            spec: &self.spec,
            configurable: self.configurable,
            tristate: self.tristate,
        }
    }
}

fn plain_spec(kind: AttrKind, mandatory: bool, declaration: AttrDeclaration) -> AttrSpec {
    AttrSpec {
        default: kind.type_default(),
        kind,
        mandatory,
        allow_files: false,
        allow_single_file: false,
        non_empty: false,
        required_providers: Vec::new(),
        declaration,
    }
}

/// A built-in (native) attribute of an inherited rule class.
fn native_macro_attr(attribute: &crate::native_rules::NativeAttr) -> MacroAttr {
    use crate::native_rules::NativeType as T;
    let (kind, tristate) = match attribute.ty {
        T::String => (AttrKind::String, false),
        T::StringList | T::License => (AttrKind::StringList, false),
        T::StringDict => (AttrKind::StringDict, false),
        T::Bool => (AttrKind::Bool, false),
        T::Int => (AttrKind::Int, false),
        T::TriState => (AttrKind::Int, true),
        T::Label | T::NodepLabel => (AttrKind::Label, false),
        T::LabelList | T::NodepLabelList => (AttrKind::LabelList, false),
        T::LabelKeyedStringDict => (AttrKind::LabelKeyedStringDict, false),
        T::LabelListDict => (AttrKind::LabelListDict, false),
        T::OutputList => (AttrKind::OutputList, false),
    };
    let mut declaration = AttrDeclaration::default();
    if attribute.order_independent {
        declaration.flags.push("ORDER_INDEPENDENT".to_owned());
    }
    declaration.nonconfigurable = !attribute.configurable;
    MacroAttr {
        name: attribute.name.to_owned(),
        spec: plain_spec(kind, attribute.mandatory, declaration),
        configurable: attribute.configurable,
        starlark_defined: false,
        tristate,
        default_none: !attribute.mandatory,
        nodep: matches!(attribute.ty, T::NodepLabel | T::NodepLabelList),
    }
}

/// StarlarkRuleClassFunctions.getAttrsOf (:507-522): the attributes of the
/// `inherit_attrs` value, before the public/documented/reserved filter.
fn inherited_attrs(value: Value<'_>) -> anyhow::Result<Vec<MacroAttr>> {
    use crate::native_rules::{StarlarkRuleKind, starlark_builtins};
    if value.is_none() {
        return Ok(Vec::new());
    }
    let builtins = |kind: StarlarkRuleKind| {
        starlark_builtins(kind)
            .attrs()
            .filter(|attribute| attribute.documented)
            .map(native_macro_attr)
            .collect::<Vec<_>>()
    };
    if let Some(inherited) = crate::custom_rule::rule_inheritance(value) {
        let (kind, attrs) = inherited?;
        let mut out = builtins(kind);
        for (name, spec, starlark_defined, configurable) in attrs {
            // Starlark `_x` attributes are private (`$x`, Attribute.isPublic).
            if name.starts_with('_') {
                continue;
            }
            let tristate = false;
            out.push(MacroAttr {
                default_none: !spec.mandatory,
                name,
                spec,
                configurable,
                starlark_defined,
                tristate,
                nodep: false,
            });
        }
        return Ok(out);
    }
    if let Some(definition) = values::MacroDefinition::from_value(value) {
        if definition.exported.get().is_none() {
            bail!(
                "Invalid 'inherit_attrs' value: a rule or macro callable must be assigned to a global variable in a .bzl file before it can be inherited from"
            );
        }
        return Ok(definition.attrs.clone());
    }
    if let Some(class) = crate::package::native_rule_class(value) {
        let class = crate::native_rules::rule_class(class)
            .ok_or_else(|| anyhow!("unregistered native rule class {class}"))?;
        return Ok(class
            .attrs
            .iter()
            .filter(|attribute| attribute.documented)
            .map(native_macro_attr)
            .collect());
    }
    match value.unpack_str() {
        // StarlarkRuleClassFunctions.baseRule (:159-194).
        Some("common") => Ok(builtins(StarlarkRuleKind::default())),
        _ => bail!(
            "Invalid 'inherit_attrs' value {}; expected a rule, a macro, or \"common\"",
            value.to_repr()
        ),
    }
}

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
        /// The attributes besides `name` and `visibility`, in MacroClass
        /// order: the declared ones, then the inherited ones.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub attrs: Vec<MacroAttr>,
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
                &self.attrs,
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
            .map(|attr| {
                let mut spec = attr.spec.clone();
                if attr.default_none {
                    spec.default = crate::custom_rule::AttrValue::Label(None);
                }
                crate::starlark_doc_extract::DocAttr {
                    name: attr.name.clone(),
                    spec,
                    starlark_defined: attr.starlark_defined,
                }
            })
            .collect(),
    })
}

/// StarlarkFunction.hasKwargs: the implementation declares `**kwargs`.
fn has_kwargs(implementation: Value<'_>) -> bool {
    implementation
        .parameters_spec()
        .is_some_and(|spec| spec.parameters_str().contains("**"))
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
        // BuiltinFunction @Param allowedTypes (StarlarkRuleFunctionsApi.java:280-300).
        let attrs = DictRef::from_value(attrs).ok_or_else(|| {
            crate::rule_declarations::param_type_error("macro", "attrs", attrs, "dict")
        })?;
        if !(inherit_attrs.is_none()
            || inherit_attrs.unpack_str().is_some()
            || crate::custom_rule::rule_inheritance(inherit_attrs).is_some()
            || values::MacroDefinition::from_value(inherit_attrs).is_some()
            || crate::package::native_rule_class(inherit_attrs).is_some())
        {
            return Err(crate::rule_declarations::param_type_error(
                "macro",
                "inherit_attrs",
                inherit_attrs,
                "rule, macro, string, or NoneType",
            ));
        }
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
        let mut budget = crate::repository_budget::HostBudget::default();
        let mut effective = Vec::new();
        for (name, descriptor) in attrs.iter() {
            let spec = crate::custom_rule::attribute_spec(descriptor);
            let wrong_type = || {
                anyhow!(
                    "got dict<{}, {}> for 'attrs', want dict<string, Attribute|None>",
                    name.get_type(),
                    if descriptor.is_none() || spec.is_some() {
                        "Attribute"
                    } else {
                        descriptor.get_type()
                    }
                )
            };
            let name = name.unpack_str().ok_or_else(wrong_type)?;
            budget.string(name).map_err(anyhow::Error::msg)?;
            // StarlarkRuleClassFunctions.checkAttributeName.
            if !crate::rule_declarations::is_identifier(name) {
                bail!("attribute name `{name}` is not a valid identifier.");
            }
            if spec.is_none() && !descriptor.is_none() {
                return Err(wrong_type());
            }
            if matches!(name, "name" | "visibility") {
                bail!("Cannot declare a macro attribute named '{name}'");
            }
            let Some(spec) = spec else {
                continue;
            };
            if spec.declaration.computed_default || spec.declaration.late_bound {
                bail!(
                    "In macro attribute '{name}': Macros do not support computed defaults or late-bound defaults"
                );
            }
            budget.spec(&spec).map_err(anyhow::Error::msg)?;
            let configurable = !spec.declaration.nonconfigurable
                && !matches!(spec.kind, AttrKind::Output | AttrKind::OutputList);
            effective.push(MacroAttr {
                name: name.to_owned(),
                spec,
                configurable,
                starlark_defined: true,
                tristate: false,
                default_none: false,
                nodep: false,
            });
        }
        for mut attr in inherited_attrs(inherit_attrs)? {
            if matches!(attr.name.as_str(), "name" | "visibility")
                || attrs.get_str(&attr.name).is_some()
            {
                continue;
            }
            budget.spec(&attr.spec).map_err(anyhow::Error::msg)?;
            attr.default_none |= !attr.spec.mandatory;
            effective.push(attr);
        }
        if !inherit_attrs.is_none() && !has_kwargs(implementation) {
            bail!(
                "If inherit_attrs is set, implementation function must have a **kwargs parameter"
            );
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
            (
                "macro(implementation=impl, attrs={1:None})",
                "got dict<int, Attribute> for 'attrs', want dict<string, Attribute|None>",
            ),
            (
                "macro(implementation=impl, attrs={'name':None})",
                "Cannot declare a macro attribute named 'name'",
            ),
            (
                "macro(implementation=impl, attrs={'visibility':None})",
                "Cannot declare a macro attribute named 'visibility'",
            ),
            (
                "macro(implementation=impl, attrs={'x':1})",
                "got dict<string, int> for 'attrs'",
            ),
            (
                "macro(implementation=impl, inherit_attrs=impl)",
                "want 'rule, macro, string, or NoneType'",
            ),
            (
                "macro(implementation=impl, inherit_attrs='common')",
                "implementation function must have a **kwargs parameter",
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
            "macro(implementation=impl, attrs={'ignored':None}, doc='help')",
            "macro(implementation=impl, attrs={'x':attr.string()})",
            // Retained for documentation; invoking it fails explicitly.
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
