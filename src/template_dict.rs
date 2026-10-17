//! `ctx.actions.template_dict()` (analysis/starlark/TemplateDict.java): the
//! computed substitutions of `ctx.actions.expand_template`. Bazel computes an
//! add_joined value when the template action executes; the native profile
//! calls map_each eagerly at add_joined (like Args, crate::args), so mapper
//! failures surface during analysis. Only owned strings are retained.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLike,
};
use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct TemplateDict {
        #[allocative(skip)]
        #[trace(static)]
        pub(super) substitutions: RefCell<Vec<(String, String)>>,
        #[allocative(skip)]
        #[trace(static)]
        pub(super) active: Arc<AtomicBool>,
    }
}
use owned::TemplateDict;

impl TemplateDict {
    fn check_active(&self) -> anyhow::Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "TemplateDict is no longer active: rule analysis has ended"
        );
        Ok(())
    }
}

impl fmt::Display for TemplateDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<TemplateDict>")
    }
}

starlark::methods_static!(TEMPLATE_DICT_METHODS = template_dict_methods);

#[starlark_value(type = "TemplateDict")]
impl<'v> StarlarkValue<'v> for TemplateDict {
    fn get_methods() -> Option<&'static Methods> {
        Some(TEMPLATE_DICT_METHODS.methods())
    }
}

pub(crate) fn alloc_template_dict<'v>(heap: Heap<'v>, active: Arc<AtomicBool>) -> Value<'v> {
    heap.alloc_complex_no_freeze(TemplateDict {
        substitutions: RefCell::new(Vec::new()),
        active,
    })
}

/// The substitutions of a genuine TemplateDict, in insertion order.
pub(crate) fn substitutions(value: Value<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let dict = value.downcast_ref::<TemplateDict>().ok_or_else(|| {
        anyhow!(
            "in call to expand_template(), parameter 'computed_substitutions' got value of type '{}', want 'TemplateDict'",
            value.get_type()
        )
    })?;
    dict.check_active()?;
    Ok(dict.substitutions.borrow().clone())
}

#[starlark_module]
fn template_dict_methods(builder: &mut MethodsBuilder) {
    /// TemplateDict.addArgument (TemplateDict.java:43-49).
    fn add<'v>(this: Value<'v>, key: &str, value: &str) -> anyhow::Result<Value<'v>> {
        let dict = this
            .downcast_ref::<TemplateDict>()
            .ok_or_else(|| anyhow!("expected TemplateDict"))?;
        dict.check_active()?;
        dict.substitutions
            .borrow_mut()
            .push((key.to_owned(), value.to_owned()));
        Ok(this)
    }

    /// TemplateDict.addJoined and LazySubstitution.getValue
    /// (TemplateDict.java:51-160).
    fn add_joined<'v>(
        this: Value<'v>,
        key: &str,
        values: Value<'v>,
        #[starlark(require = named)] join_with: &str,
        #[starlark(require = named)] map_each: Value<'v>,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = NoneOr::None)] format_joined: NoneOr<&str>,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let dict = this
            .downcast_ref::<TemplateDict>()
            .ok_or_else(|| anyhow!("expected TemplateDict"))?;
        dict.check_active()?;
        if values.get_type() != "depset" {
            bail!(
                "in call to add_joined(), parameter 'values' got value of type '{}', want 'depset'",
                values.get_type()
            );
        }
        let name = map_each.function_name().ok_or_else(|| {
            anyhow!(
                "in call to add_joined(), parameter 'map_each' got value of type '{}', want 'callable'",
                map_each.get_type()
            )
        })?;
        // TemplateDict.addJoined (analysis/starlark/TemplateDict.java:70-78):
        // only a StarlarkFunction must be global; built-in callables pass.
        let starlark_function = {
            let repr = map_each.to_repr();
            repr.starts_with("<function ") && repr.contains(" from ")
        };
        if !allow_closure && starlark_function {
            ensure!(
                !name.contains("lambda")
                    && (crate::analysis_runtime::is_top_level_mapper(eval, map_each)
                        || eval
                            .module()
                            .get(name)
                            .is_some_and(|global| global.ptr_eq(map_each))),
                "to avoid unintended retention of analysis data structures, the map_each function must be declared by a top-level def statement"
            );
        }
        let to_list = values
            .get_attr_error("to_list", eval.heap())
            .map_err(|e| anyhow!(e.to_string()))?;
        let list = eval
            .eval_function(to_list, &[], &[])
            .map_err(|e| anyhow!(e.to_string()))?;
        let items: Vec<Value<'v>> = ListRef::from_value(list)
            .ok_or_else(|| anyhow!("depset.to_list() must return a list"))?
            .iter()
            .collect();
        let mut parts: Vec<String> = Vec::new();
        for item in items {
            // Bazel calls map_each while expanding the template, in its own
            // Starlark thread (TemplateDict.java:125), not within the rule
            // implementation.
            let mapped = eval
                .with_new_recursion_scope(|eval| eval.eval_function(map_each, &[item], &[]))
                .map_err(|e| anyhow!("{e}"))?;
            if let Some(text) = mapped.unpack_str() {
                parts.push(text.to_owned());
            } else if let Some(list) = ListRef::from_value(mapped) {
                for element in list.iter() {
                    let text = element.unpack_str().ok_or_else(|| {
                        anyhow!(
                            "Function provided to map_each must return string, None, or list of strings, but returned list containing element '{}' of type {} for key '{key}' and value: {}",
                            element.to_str(),
                            element.get_type(),
                            item.to_str()
                        )
                    })?;
                    parts.push(text.to_owned());
                }
            } else if !mapped.is_none() {
                bail!(
                    "Function provided to map_each must return string, None, or list of strings, but returned type {} for key '{key}' and value: {}",
                    mapped.get_type(),
                    item.to_str()
                );
            }
        }
        if uniquify {
            let mut seen = std::collections::BTreeSet::new();
            parts.retain(|part| seen.insert(part.clone()));
        }
        let joined = parts.join(join_with);
        let value = match format_joined.into_option() {
            // Starlark.format: Starlark's `%` operator with one argument.
            Some(format) => {
                let heap = eval.heap();
                heap.alloc(format)
                    .percent(heap.alloc(joined.as_str()), heap)
                    .map_err(|e| anyhow!("{e}"))?
                    .unpack_str()
                    .ok_or_else(|| anyhow!("format_joined must produce a string"))?
                    .to_owned()
            }
            None => joined,
        };
        dict.substitutions
            .borrow_mut()
            .push((key.to_owned(), value));
        Ok(this)
    }
}
