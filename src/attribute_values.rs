//! Owned, immutable projections of dictionary-valued attributes.
use anyhow::anyhow;
use starlark::environment::Module;
use starlark::values::dict::AllocDict;
use starlark::values::{FrozenHeapName, Heap, Value};

/// Freeze only the newly allocated attribute, never the caller's containers or
/// enclosing context. Transfer its owner to the receiving evaluation heap.
pub(crate) fn alloc_string_list_dict<'v>(
    heap: Heap<'v>,
    entries: &[(String, Vec<String>)],
) -> anyhow::Result<Value<'v>> {
    // Binding and invocation preflight charge aggregate payloads. This local
    // boundary also rejects oversized standalone projections before copying.
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    for (key, values) in entries {
        budget.string(key).map_err(anyhow::Error::msg)?;
        budget.node().map_err(anyhow::Error::msg)?;
        for value in values {
            budget.string(value).map_err(anyhow::Error::msg)?;
        }
    }
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let local = module.heap();
        let value = local.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, values)| (key.as_str(), local.alloc(values.clone()))),
        ));
        module.set("attribute", value);
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "string_list_dict attribute".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze dictionary attribute: {error:?}"))?;
        frozen
            .get("attribute")
            .map_err(|error| anyhow!(error.to_string()))
    })?;
    Ok(heap.access_owned_frozen_value(&owned))
}

/// Scalar dictionary projection owns only its fresh dictionary and strings.
pub(crate) fn alloc_string_dict<'v>(
    heap: Heap<'v>,
    entries: &[(String, String)],
) -> anyhow::Result<Value<'v>> {
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    for (key, value) in entries {
        budget.string(key).map_err(anyhow::Error::msg)?;
        budget.string(value).map_err(anyhow::Error::msg)?;
    }
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        module.set(
            "attribute",
            module.heap().alloc(AllocDict(
                entries
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )),
        );
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "string_dict attribute".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze dictionary attribute: {error:?}"))?;
        frozen
            .get("attribute")
            .map_err(|error| anyhow!(error.to_string()))
    })?;
    Ok(heap.access_owned_frozen_value(&owned))
}

/// Frozen `dict(string, label)` / `dict(string, list(label))` projection
/// (Attribute.valueToStarlark, packages/Attribute.java:2442-2450): fresh
/// immutable dictionary of Label values (lists for label-list dictionaries).
pub(crate) fn alloc_label_dict<'v>(
    heap: Heap<'v>,
    entries: &[(String, Vec<String>)],
    lists: bool,
) -> anyhow::Result<Value<'v>> {
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    for (key, values) in entries {
        budget.string(key).map_err(anyhow::Error::msg)?;
        budget.node().map_err(anyhow::Error::msg)?;
        for value in values {
            budget.string(value).map_err(anyhow::Error::msg)?;
        }
    }
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let local = module.heap();
        let mut items = Vec::new();
        for (key, values) in entries {
            let labels = values
                .iter()
                .map(|label| crate::starlark_types::alloc_label(local, label))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let value = if lists {
                local.alloc(labels)
            } else {
                *labels
                    .first()
                    .ok_or_else(|| anyhow!("label dictionary entry without a label"))?
            };
            items.push((key.as_str(), value));
        }
        module.set("attribute", local.alloc(AllocDict(items)));
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "label dictionary attribute".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze dictionary attribute: {error:?}"))?;
        frozen
            .get("attribute")
            .map_err(|error| anyhow!(error.to_string()))
    })?;
    Ok(heap.access_owned_frozen_value(&owned))
}

/// [`alloc_label_dict`] of a `dict(string, label)` value.
pub(crate) fn alloc_string_keyed_label_dict<'v>(
    heap: Heap<'v>,
    entries: &[(String, String)],
) -> anyhow::Result<Value<'v>> {
    let entries = entries
        .iter()
        .map(|(key, label)| (key.clone(), vec![label.clone()]))
        .collect::<Vec<_>>();
    alloc_label_dict(heap, &entries, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Globals;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    #[test]
    fn string_dict_projection_is_frozen_ordered_and_independent_of_owners() {
        Module::with_temp_heap(|module| {
            let mut original = vec![
                ("z".to_owned(), "café☃".to_owned()),
                ("a".to_owned(), "".to_owned()),
            ];
            module.set(
                "value",
                alloc_string_dict(module.heap(), &original).unwrap(),
            );
            original[0].1 = "after".into();
            original.reverse();
            drop(original);
            let mut eval = Evaluator::new(&module);
            let ast = AstModule::parse("test.bzl", "(value.keys() == ['z', 'a'] and value.values() == ['café☃', ''] and type(value['a']) == 'string') or fail('snapshot lost')".into(), &Dialect::Standard).unwrap();
            eval.eval_module(ast, &Globals::standard()).unwrap();
            let ast = AstModule::parse(
                "mutation.bzl",
                "value['z'] = 'changed'".into(),
                &Dialect::Standard,
            )
            .unwrap();
            let error = eval
                .eval_module(ast, &Globals::standard())
                .unwrap_err()
                .to_string()
                .to_lowercase();
            assert!(
                error.contains("frozen") || error.contains("immutable"),
                "{error}"
            );
        });
    }

    #[test]
    fn string_dict_projection_precharges_standalone_payloads() {
        Module::with_temp_heap(|module| {
            for entries in [
                vec![("x".repeat(9 * 1024 * 1024), "".into())],
                vec![("k".into(), "x".repeat(9 * 1024 * 1024))],
                vec![("".into(), "".into()); 50000],
            ] {
                assert!(
                    alloc_string_dict(module.heap(), &entries)
                        .unwrap_err()
                        .to_string()
                        .contains("limit")
                );
            }
        });
    }

    #[test]
    fn string_list_dict_frozen_projection_retains_order_and_owner() {
        Module::with_temp_heap(|module| {
            let mut original = vec![
                ("z".to_owned(), vec!["before".to_owned()]),
                ("a".to_owned(), vec![]),
            ];
            module.set(
                "value",
                alloc_string_list_dict(module.heap(), &original).unwrap(),
            );
            original[0].1.push("after".to_owned());
            original.reverse();
            drop(original);
            let mut eval = Evaluator::new(&module);
            let ast = AstModule::parse("test.bzl", "(value.keys() == ['z', 'a'] and value['z'] == ['before'] and type(value['a']) == 'list') or fail('snapshot lost')\n".to_owned(), &Dialect::Standard).unwrap();
            eval.eval_module(ast, &Globals::standard()).unwrap();
        });
    }

    #[test]
    fn string_list_dict_dictionary_and_nested_lists_are_frozen() {
        for mutation in ["value['new'] = []", "value['z'].append('changed')"] {
            Module::with_temp_heap(|module| {
                let original = vec![("z".to_owned(), vec!["before".to_owned()])];
                module.set(
                    "value",
                    alloc_string_list_dict(module.heap(), &original).unwrap(),
                );
                let mut eval = Evaluator::new(&module);
                let ast = AstModule::parse("mutation.bzl", mutation.to_owned(), &Dialect::Standard)
                    .unwrap();
                let error = eval
                    .eval_module(ast, &Globals::standard())
                    .unwrap_err()
                    .to_string();
                let diagnostic = error.to_lowercase();
                assert!(
                    diagnostic.contains("frozen") || diagnostic.contains("immutable"),
                    "{error}"
                );
            });
        }
    }
}
