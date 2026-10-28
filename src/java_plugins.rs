//! JavaPluginsFlagAliasRule.java:86-108 and JavaPluginInfo.java:116-244 (9.3).
//! These are Starlark providers defined by rules_java, not new built-in
//! constructors. Match the defining extension AND exported name, never just
//! the name, and publish both identities just as the genuine bridge does.
use std::collections::BTreeMap;

use starlark::environment::Module;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, OwnedFrozenValue, Value, ValueLike};

use crate::starlark_types::{Depset, ItemType, Order};

pub(crate) const PROVIDER_IDS: [&str; 2] = [
    "@@rules_java+//java/private:java_info.bzl%JavaPluginInfo",
    "//java/private:java_info.bzl%JavaPluginInfo",
];

type Providers = BTreeMap<String, OwnedFrozenValue>;

fn field<'v>(value: Value<'v>, name: &str, heap: Heap<'v>) -> Result<Value<'v>, String> {
    value
        .get_attr(name, heap)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("JavaPluginInfo missing field '{name}'"))
}

fn merge_data<'v>(values: &[Value<'v>], name: &str, heap: Heap<'v>) -> Result<Value<'v>, String> {
    let mut fields = Vec::new();
    for (field_name, kind) in [
        ("processor_classes", ItemType::String),
        ("processor_jars", ItemType::File),
        ("processor_data", ItemType::File),
    ] {
        let mut children = Vec::new();
        for value in values {
            let data = field(*value, name, heap)?;
            let value = field(data, field_name, heap)?;
            let set = value
                .downcast_ref::<Depset>()
                .ok_or_else(|| format!("JavaPluginInfo.{name}.{field_name} must be a depset"))?;
            children.push(set.node.clone());
        }
        // NAIVE_LINK_ORDER == Starlark preorder. Reuse DAGs (no flattening),
        // preserving order/type checks and framework's unlimited nesting.
        let set = Depset::compose_framework(Vec::new(), children, Order::Preorder, Some(kind))
            .map_err(|error| format!("JavaPluginInfo.{name}.{field_name}: {error}"))?;
        fields.push((field_name, heap.alloc(set)));
    }
    Ok(heap.alloc(AllocStruct(fields)))
}

pub(crate) fn merge(prerequisites: &[Providers]) -> Result<Providers, String> {
    Module::with_temp_heap(|module| {
        // Genuine chooses all external-identity providers, falling back to
        // all main-repo-identity providers ONLY if the first list is empty.
        let mut values = Vec::new();
        for id in PROVIDER_IDS {
            values = prerequisites
                .iter()
                .filter_map(|p| p.get(id))
                .map(|v| module.heap().access_owned_frozen_value(v))
                .collect();
            if !values.is_empty() {
                break;
            }
        }
        let heap = module.heap();
        for value in &values {
            validate_outputs(*value, heap)?;
        }
        let plugins = merge_data(&values, "plugins", heap)?;
        let api = merge_data(&values, "api_generating_plugins", heap)?;
        for (index, id) in PROVIDER_IDS.iter().enumerate() {
            let value = crate::providers::alloc_instance(
                heap,
                (*id).to_owned(),
                "JavaPluginInfo".to_owned(),
                vec![
                    ("api_generating_plugins".to_owned(), api),
                    ("java_outputs".to_owned(), heap.alloc(Vec::<Value>::new())),
                    ("plugins".to_owned(), plugins),
                ],
                None,
            );
            module.set(&format!("p{index}"), value);
        }
        let module = module
            .freeze()
            .map_err(|error| format!("freezing JavaPluginInfo: {error:?}"))?;
        PROVIDER_IDS
            .iter()
            .enumerate()
            .map(|(index, id)| {
                Ok((
                    (*id).to_owned(),
                    module
                        .get(&format!("p{index}"))
                        .map_err(|error| error.to_string())?,
                ))
            })
            .collect()
    })
}

/// The native wrapper validates JavaOutput even though the bridge subsequently
/// drops it (JavaPluginInfo.Provider.wrap, JavaOutput.fromStarlarkJavaOutput).
pub(crate) fn validate_outputs<'v>(value: Value<'v>, heap: Heap<'v>) -> Result<(), String> {
    let outputs = field(value, "java_outputs", heap)?;
    let outputs = starlark::values::list::ListRef::from_value(outputs)
        .map(|list| list.content())
        .or_else(|| {
            starlark::values::tuple::TupleRef::from_value(outputs).map(|tuple| tuple.content())
        })
        .ok_or("JavaPluginInfo.java_outputs must be a sequence")?;
    for output in outputs {
        for name in [
            "class_jar",
            "compile_jar",
            "header_compilation_jar",
            "compile_jdeps",
            "generated_class_jar",
            "generated_source_jar",
            "native_headers_jar",
            "manifest_proto",
            "jdeps",
        ] {
            let value = output.get_attr(name, heap).map_err(|e| e.to_string())?;
            match value {
                Some(value) if !value.is_none() => {
                    crate::starlark_types::unpack_file(value).map_err(|e| e.to_string())?;
                }
                _ if name == "class_jar" => {
                    return Err("JavaOutput.class_jar must be a File".to_owned());
                }
                _ => {}
            }
        }
        let jars = field(*output, "source_jars", heap)?;
        if !jars.is_none() {
            crate::starlark_types::file_list(jars).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
