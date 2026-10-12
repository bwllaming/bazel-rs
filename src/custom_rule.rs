//! Owned custom-rule schemas and trace/freeze-safe Starlark definitions.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Allocative)]
pub enum AttrKind {
    String,
    StringList,
    Int,
    Bool,
    Label,
    LabelList,
    Output,
    OutputList,
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative)]
pub enum AttrValue {
    String(String),
    Strings(Vec<String>),
    Int(i32),
    Bool(bool),
    Label(Option<String>),
    Labels(Vec<String>),
    Output(Option<String>),
    Outputs(Vec<String>),
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative)]
pub struct AttrSpec {
    pub kind: AttrKind,
    pub default: AttrValue,
    pub mandatory: bool,
    pub allow_files: bool,
    pub allow_single_file: bool,
}
#[derive(Clone, Debug)]
pub struct CustomInvocation {
    pub implementation: OwnedFrozenValue,
    pub top_level_functions: Vec<OwnedFrozenValue>,
    pub attrs: BTreeMap<String, AttrValue>,
    pub schema: BTreeMap<String, AttrSpec>,
}

fn string_list(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    ListRef::from_value(value)
        .ok_or_else(|| anyhow!("expected a list of strings"))?
        .iter()
        .map(|v| {
            v.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a list of strings"))
        })
        .collect()
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
    fn empty(&self) -> AttrValue {
        match self {
            Self::String => AttrValue::String(String::new()),
            Self::StringList => AttrValue::Strings(Vec::new()),
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
                ListRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                    .iter()
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
            let resolve = |text: &str| {
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
                        let parsed = spec.kind.parse(*value, Some(&resolve)).map_err(|e| {
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
                    attrs.insert(key.clone(), spec.default.clone());
                }
            }
            crate::package::register_custom(
                eval,
                name,
                visibility,
                self.implementation.to_value(),
                attrs,
                self.schema.clone(),
            )
            .map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }
}
use values::{DefinitionGen, Descriptor};

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
        #[starlark(require = named, default = "target")] cfg: &str,
        #[starlark(require = named)] doc: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if cfg != "target" {
            bail!("attr.label cfg={cfg:?} is unsupported; only target is supported");
        }
        let resolve = |text: &str| crate::loading::resolve_label(eval, text);
        descriptor_in_package(
            AttrKind::Label,
            default,
            mandatory,
            allow_files,
            allow_single_file,
            definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
        )
    }
    fn label_list<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] allow_files: bool,
        #[starlark(require = named, default = "target")] cfg: &str,
        #[starlark(require = named)] doc: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if cfg != "target" {
            bail!("attr.label_list cfg={cfg:?} is unsupported; only target is supported");
        }
        let resolve = |text: &str| crate::loading::resolve_label(eval, text);
        descriptor_in_package(
            AttrKind::LabelList,
            default,
            mandatory,
            allow_files,
            false,
            definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
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
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::OutputList, None, mandatory, false, false)
    }
}

#[starlark_module]
fn definition_globals(builder: &mut GlobalsBuilder) {
    fn rule<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] test: bool,
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
                schema.insert(name.to_owned(), spec.0.clone());
            }
        }
        Ok(eval.heap().alloc(DefinitionGen {
            implementation,
            schema,
        }))
    }
}

/// Install only into .bzl globals; BUILD may invoke loaded definitions, not create them.
pub fn rule_globals(builder: &mut GlobalsBuilder) {
    definition_globals(builder);
    crate::starlark_types::analysis_globals(builder);
    let attrs = GlobalsBuilder::new().with(attr_globals).build();
    builder.frozen_heap().add_reference(attrs.heap());
    builder.set("attr", AllocStruct(attrs.iter()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Module;
    use starlark::syntax::{AstModule, Dialect};

    fn definition_error(expression: &str) -> String {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let source = format!("def impl(ctx):\n    return []\nx = {expression}");
            let ast = AstModule::parse("defs.bzl", source, &Dialect::Standard).unwrap();
            let context = crate::loading::ExtensionContext {
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
