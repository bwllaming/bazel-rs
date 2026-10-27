//! Function-valued aspect propagation (docs/ASPECTS.md "Propagation
//! functions"): a function `attr_aspects`/`toolchains_aspects` and the
//! `propagation_predicate`, each called with a `propagation_ctx`
//! (analysis/starlark/StarlarkAspectPropagationContext.java;
//! packages/AspectPropagationEdgesSupplier.java,
//! packages/AspectPropagationPredicate.java; paths relative to
//! `src/main/java/com/google/devtools/build/lib/` of the pinned
//! /tmp/opencode/bazel-9.3-reference).
//!
//! `ctx.attr` holds the aspect's public parameters (AspectParameters), and
//! `ctx.rule` the target: `label`, `qualified_kind` (`file_label`,
//! `rule_name`) and `attr`, whose fields are `RuleAttribute`s (`value`,
//! `is_tool`) — for the edge functions every rule attribute with label
//! attributes as their configured dependency Labels
//! (convertAttributeValueForAspectPropagationFunc), for the predicate only
//! `tags`.

use std::fmt;

use allocative::Allocative;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::list::ListRef;
use starlark::values::{Heap, Value, ValueLike};

use crate::aspects::AspectDefinition;
use crate::custom_rule::AttrValue;
use crate::package::Rule;

#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

    /// A record of named fields with Bazel's Starlark type name (the Java
    /// record's @StarlarkBuiltin name or class simple name) and, for a
    /// struct, StructImpl's unknown-field message.
    macro_rules! fields_value {
        ($gen:ident, $name:ident, $type_name:literal) => {
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
            pub(crate) struct $gen<V> {
                pub values: Vec<V>,
                #[trace(unsafe_ignore)]
                #[freeze(identity)]
                pub names: Vec<String>,
                /// `Some` for a struct: the message (`%s` the field).
                #[trace(unsafe_ignore)]
                #[freeze(identity)]
                pub unknown_field: Option<String>,
            }
            starlark_complex_value!(pub(crate) $name);

            impl<'v, V: ValueLike<'v>> fmt::Display for $gen<V> {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    if self.unknown_field.is_none() {
                        return write!(f, "<{}>", $type_name);
                    }
                    // StructImpl.repr: sorted fields.
                    let mut fields = self.names.iter().zip(&self.values).collect::<Vec<_>>();
                    fields.sort_by(|a, b| a.0.cmp(b.0));
                    f.write_str("struct(")?;
                    for (index, (name, value)) in fields.into_iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{name} = {}", value.to_value().to_repr())?;
                    }
                    f.write_str(")")
                }
            }

            #[starlark_value(type = $type_name)]
            impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for $gen<V>
            where
                Self: ProvidesStaticType<'v>,
            {
                fn get_attr(&self, name: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
                    self.names
                        .iter()
                        .position(|candidate| candidate == name)
                        .map(|index| self.values[index].to_value())
                }

                fn dir_attr(&self) -> Vec<String> {
                    let mut names = self.names.clone();
                    names.sort();
                    names
                }

                fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
                    let message = self.unknown_field.as_ref()?;
                    let mut names = self.names.clone();
                    names.sort();
                    Some(format!(
                        "{}\nAvailable attributes: {}",
                        message.replace("%s", attribute),
                        names.join(", ")
                    ))
                }
            }
        };
    }
    fields_value!(PropagationCtxGen, PropagationCtx, "propagation_ctx");
    fields_value!(
        PropagationRuleGen,
        PropagationRule,
        "StarlarkAspectPropagationRule"
    );
    fields_value!(QualifiedKindGen, QualifiedKind, "QualifiedRuleKind");
    fields_value!(RuleAttributeGen, RuleAttribute, "RuleAttribute");
    fields_value!(FieldsStructGen, FieldsStruct, "struct");
}
use values::{
    FieldsStructGen, PropagationCtxGen, PropagationRuleGen, QualifiedKindGen, RuleAttributeGen,
};

fn record<'v, T>(
    heap: Heap<'v>,
    make: impl FnOnce(Vec<Value<'v>>, Vec<String>, Option<String>) -> T,
    fields: Vec<(String, Value<'v>)>,
    unknown_field: Option<String>,
) -> Value<'v>
where
    T: starlark::values::AllocValue<'v>,
{
    let (names, values) = fields.into_iter().unzip();
    heap.alloc(make(values, names, unknown_field))
}

/// What the context's `ctx.rule.attr` holds.
enum RuleAttributes {
    /// createForPropagationEdges: every attribute of the rule.
    All,
    /// createForPropagationPredicate: `tags` only.
    Tags,
}

/// The aspect's public parameters (StarlarkAspectPropagationContext.
/// createAspectPublicParams): the AspectParameters' names — every parameter
/// of a top-level aspect, the rule-extracted ones of an attribute's aspect —
/// with their values.
fn public_parameters(aspect: &AspectDefinition) -> Vec<(String, AttrValue)> {
    let mut parameters = Vec::new();
    for (name, spec) in &aspect.declaration.attrs {
        if !crate::aspects::is_parameter_attribute(name, spec) {
            continue;
        }
        let value = if aspect.top_level {
            aspect.parameters.get(name).unwrap_or(&spec.default)
        } else {
            match aspect.parameters.get(name) {
                Some(value) => value,
                None => continue,
            }
        };
        parameters.push((name.clone(), value.clone()));
    }
    parameters
}

fn context<'v>(
    heap: Heap<'v>,
    aspect: &AspectDefinition,
    rule: &Rule,
    attributes: RuleAttributes,
) -> anyhow::Result<Value<'v>> {
    let convert = |value: &AttrValue| -> anyhow::Result<Value<'v>> {
        Ok(
            crate::custom_rule::attr_value_to_starlark(heap, value, &rule.label)?
                .unwrap_or_else(Value::new_none),
        )
    };
    let mut parameter_fields = Vec::new();
    for (name, value) in public_parameters(aspect) {
        parameter_fields.push((name, convert(&value)?));
    }
    let parameters = record(
        heap,
        |values, names, unknown_field| FieldsStructGen {
            values,
            names,
            unknown_field,
        },
        parameter_fields,
        Some(" '%s' is not a public parameter of the aspect.".to_owned()),
    );
    let rule_attribute = |value: Value<'v>, is_tool: bool| {
        record(
            heap,
            |values, names, unknown_field| RuleAttributeGen {
                values,
                names,
                unknown_field,
            },
            vec![
                ("value".to_owned(), value),
                ("is_tool".to_owned(), Value::new_bool(is_tool)),
            ],
            None,
        )
    };
    let (attribute_fields, unknown) = match attributes {
        RuleAttributes::Tags => (
            vec![(
                "tags".to_owned(),
                rule_attribute(heap.alloc(rule.tags().to_vec()), false),
            )],
            "Only rule's \"tags\" are available in propagation_predicate function.".to_owned(),
        ),
        RuleAttributes::All => {
            let mut fields = Vec::new();
            for (name, value) in
                crate::configured_attributes::transition_attributes(rule, None, &[])
            {
                // An attribute's Attribute.isToolDependency: an exec
                // transition.
                let is_tool = rule
                    .custom
                    .as_ref()
                    .and_then(|custom| custom.schema.get(&name))
                    .is_some_and(|spec| {
                        matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_))
                    });
                let value = match &value {
                    Some(value) => convert(value)?,
                    None => Value::new_none(),
                };
                fields.push((name, rule_attribute(value, is_tool)));
            }
            (
                fields,
                format!(
                    "'%s' is not an attribute of target: {}",
                    crate::rule_declarations::canonical_form(&rule.label)
                ),
            )
        }
    };
    let rule_attributes = record(
        heap,
        |values, names, unknown_field| FieldsStructGen {
            values,
            names,
            unknown_field,
        },
        attribute_fields,
        Some(unknown),
    );
    // QualifiedRuleKind: RuleClass.getRuleDefinitionEnvironmentLabel (None
    // for a native rule class) and the rule class name.
    let file_label = match rule
        .custom
        .as_ref()
        .map(|custom| custom.declaration.definition_environment_label.as_str())
        .filter(|label| !label.is_empty())
    {
        Some(label) => crate::starlark_types::alloc_label(heap, label)?,
        None => Value::new_none(),
    };
    let kind = record(
        heap,
        |values, names, unknown_field| QualifiedKindGen {
            values,
            names,
            unknown_field,
        },
        vec![
            ("file_label".to_owned(), file_label),
            (
                "rule_name".to_owned(),
                heap.alloc(
                    rule.rule_class_name()
                        .rsplit('%')
                        .next()
                        .unwrap_or_default(),
                ),
            ),
        ],
        None,
    );
    let rule_value = record(
        heap,
        |values, names, unknown_field| PropagationRuleGen {
            values,
            names,
            unknown_field,
        },
        vec![
            (
                "label".to_owned(),
                crate::starlark_types::alloc_label(heap, &rule.label.to_string())?,
            ),
            ("qualified_kind".to_owned(), kind),
            ("attr".to_owned(), rule_attributes),
        ],
        None,
    );
    Ok(record(
        heap,
        |values, names, unknown_field| PropagationCtxGen {
            values,
            names,
            unknown_field,
        },
        vec![
            ("attr".to_owned(), parameters),
            ("rule".to_owned(), rule_value),
        ],
        None,
    ))
}

/// Which callback of the aspect value to call.
#[derive(Clone, Copy)]
#[allow(dead_code)] // ToolchainsAspects: used by toolchains_aspects propagation.
enum Callback {
    AttrAspects,
    ToolchainsAspects,
    Predicate,
}

/// Calls `callback` of `aspect` with the context over `rule`
/// (`Starlark.positionalOnlyCall(thread, function, context)`, print()s are
/// DEBUG events) and hands the result to `convert`; `None` when the aspect
/// has no such function. An error is the EvalException's message (with its
/// stack for a failure inside the function).
fn call<T>(
    aspect: &AspectDefinition,
    rule: &Rule,
    callback: Callback,
    convert: impl FnOnce(Value<'_>) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let Some(value) = crate::rule_declarations::Aspect::from_value(aspect.value.value()) else {
        return Ok(None);
    };
    let function = match callback {
        Callback::AttrAspects => &value.attr_aspects_function,
        Callback::ToolchainsAspects => &value.toolchains_aspects_function,
        Callback::Predicate => &value.propagation_predicate,
    };
    if function.is_empty() {
        return Ok(None);
    }
    Module::with_temp_heap(|module| -> Result<Option<T>, String> {
        let heap = module.heap();
        let aspect_value = heap.access_owned_frozen_value(&aspect.value);
        let value = crate::rule_declarations::Aspect::from_value(aspect_value)
            .ok_or_else(|| "internal error: not an aspect".to_owned())?;
        let function = match callback {
            Callback::AttrAspects => value.attr_aspects_function.first(),
            Callback::ToolchainsAspects => value.toolchains_aspects_function.first(),
            Callback::Predicate => value.propagation_predicate.first(),
        }
        .copied()
        .ok_or_else(|| "internal error: no propagation function".to_owned())?;
        let attributes = match callback {
            Callback::Predicate => RuleAttributes::Tags,
            _ => RuleAttributes::All,
        };
        let ctx = context(heap, aspect, rule, attributes).map_err(|error| error.to_string())?;
        let mut eval = Evaluator::new(&module);
        eval.set_max_callstack_size(128)
            .map_err(|error| error.to_string())?;
        eval.set_print_handler(&crate::ui::NotePrints);
        let result = eval
            .eval_function(function, &[ctx], &[])
            .map_err(|error| error.to_string())?;
        convert(result).map(Some)
    })
}

/// A function `attr_aspects` evaluated over `rule`
/// (AttrAspectsFunctionSupplier.computeList: a list, parseAttrAspects
/// without `'*'`); `None` for a fixed list.
pub(crate) fn computed_attr_aspects(
    aspect: &AspectDefinition,
    rule: &Rule,
) -> Result<Option<Vec<String>>, String> {
    call(aspect, rule, Callback::AttrAspects, |result| {
        let list = ListRef::from_value(result).ok_or_else(|| "Expected a list".to_owned())?;
        let mut names = Vec::new();
        for item in list.iter() {
            let name = item.unpack_str().ok_or_else(|| {
                format!(
                    "at index {} of attr_aspects, got element of type {}, want string",
                    names.len(),
                    item.get_type()
                )
            })?;
            if name == "*" {
                return Err("'*' is not allowed in 'attr_aspects' list".to_owned());
            }
            if !names.iter().any(|seen| seen == name) {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    })
}

/// A function `toolchains_aspects` evaluated over `rule`
/// (ToolchainsAspectsFunctionSupplier.computeList: a list of Labels);
/// `None` for a fixed list.
#[allow(dead_code)] // Used by toolchains_aspects propagation.
pub(crate) fn computed_toolchains_aspects(
    aspect: &AspectDefinition,
    rule: &Rule,
) -> Result<Option<Vec<String>>, String> {
    call(aspect, rule, Callback::ToolchainsAspects, |result| {
        let list = ListRef::from_value(result).ok_or_else(|| "Expected a list".to_owned())?;
        let mut labels = Vec::new();
        for item in list.iter() {
            let label = crate::starlark_types::label_canonical_form(item).ok_or_else(|| {
                format!(
                    "at index {} of toolchains_aspects, got element of type {}, want Label",
                    labels.len(),
                    item.get_type()
                )
            })?;
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        Ok(labels)
    })
}

/// The `propagation_predicate` over `rule` (AspectPropagationPredicate.
/// evaluate: a boolean); `true` without a predicate.
pub(crate) fn propagation_predicate(
    aspect: &AspectDefinition,
    rule: &Rule,
) -> Result<bool, String> {
    call(aspect, rule, Callback::Predicate, |result| {
        result
            .unpack_bool()
            .ok_or_else(|| "Expected a boolean".to_owned())
    })
    .map(|result| result.unwrap_or(true))
}
