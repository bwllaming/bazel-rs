//! `mod show_repo`, `mod show_extension` and `mod dump_repo_mapping`
//! (ModCommand.getReposToShow/dumpRepoMappings, ModExecutor.displayExtension,
//! RepoOutputFormatter).
use super::executor::Executor;
use super::{Code, Context, ExtensionId, Failure, ModOptions, ModuleKey, OutputFormat};
use crate::custom_rule::AttrValue;
use crate::repository::{RepoDefinitionValue, RepositorySet};
use serde_json::Value;

/// Starlark.repr of a string.
pub(crate) fn repr_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn repr_label(label: &str) -> String {
    let canonical = if label.starts_with("@@") {
        label.to_owned()
    } else {
        format!("@@{label}")
    };
    format!("Label({})", repr_string(&canonical))
}

/// Starlark.repr of a type-checked attribute value.
pub(crate) fn repr_attribute(value: &AttrValue) -> String {
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    let strings = |items: &[String]| list(items.iter().map(|item| repr_string(item)).collect());
    let labels = |items: &[String]| list(items.iter().map(|item| repr_label(item)).collect());
    let dict = |entries: Vec<(String, String)>| {
        let entries: Vec<String> = entries
            .into_iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        format!("{{{}}}", entries.join(", "))
    };
    match value {
        AttrValue::String(text) => repr_string(text),
        AttrValue::Strings(items) => strings(items),
        AttrValue::StringListDict(entries) => dict(
            entries
                .iter()
                .map(|(key, items)| (repr_string(key), strings(items)))
                .collect(),
        ),
        AttrValue::StringDict(entries) => dict(
            entries
                .iter()
                .map(|(key, item)| (repr_string(key), repr_string(item)))
                .collect(),
        ),
        AttrValue::Int(int) => int.to_string(),
        AttrValue::Bool(true) => "True".to_owned(),
        AttrValue::Bool(false) => "False".to_owned(),
        AttrValue::Label(Some(label)) | AttrValue::Output(Some(label)) => repr_label(label),
        AttrValue::Label(None) | AttrValue::Output(None) => "None".to_owned(),
        AttrValue::Labels(items) | AttrValue::Outputs(items) => labels(items),
        AttrValue::Ints(items) => list(items.iter().map(ToString::to_string).collect()),
        AttrValue::LabelKeyedStringDict(entries) => dict(
            entries
                .iter()
                .map(|(key, item)| (repr_label(key), repr_string(item)))
                .collect(),
        ),
        AttrValue::StringKeyedLabelDict(entries) => dict(
            entries
                .iter()
                .map(|(key, item)| (repr_string(key), repr_label(item)))
                .collect(),
        ),
        AttrValue::LabelListDict(entries) => dict(
            entries
                .iter()
                .map(|(key, items)| (repr_string(key), labels(items)))
                .collect(),
        ),
    }
}

/// Starlark.repr of a raw tag attribute value (recorded as JSON).
pub(crate) fn repr_json(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => repr_string(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(repr_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => {
            if let Some(Value::Array(entries)) = map.get(crate::repository_rule::DICT_ENTRIES) {
                let entries: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| match entry.as_array().map(Vec::as_slice) {
                        Some([key, value]) => {
                            let key = match key {
                                Value::Object(marker) => marker
                                    .get(crate::repository_rule::KEY_REPR)
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                other => repr_json(other),
                            };
                            Some(format!("{key}: {}", repr_json(value)))
                        }
                        _ => None,
                    })
                    .collect();
                return format!("{{{}}}", entries.join(", "));
            }
            let entries: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}: {}", repr_string(key), repr_json(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

/// ModCommand's DUMP_REPO_MAPPING branch and dumpRepoMappings.
pub(crate) fn dump_repo_mapping(
    repositories: &RepositorySet,
    names: &[String],
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    if names.is_empty() {
        return Err(Failure::invalid("No repository name(s) specified"));
    }
    for name in names {
        if !name.is_empty() {
            crate::label::validate_repository_name(name).map_err(Failure::invalid)?;
        }
    }
    let mut missing = Vec::new();
    for name in names {
        let defined = repositories
            .repository_defined(name)
            .map_err(Failure::unknown)?;
        if !defined {
            missing.push(name.as_str());
        }
    }
    if !missing.is_empty() {
        return Err(Failure::invalid(format!(
            "Repositories not found: {}",
            missing.join(", ")
        )));
    }
    for name in names {
        let text =
            crate::module_cli::dump_repo_mapping(repositories, name).map_err(Failure::unknown)?;
        out.extend_from_slice(text.as_bytes());
    }
    Ok(())
}

/// ModCommand.getReposToShow: display key -> canonical repository name.
pub(crate) fn repos_to_show(
    context: &Context<'_>,
    options: &ModOptions,
    base_canonical: &str,
    args: &[String],
) -> Result<Vec<(String, String)>, Failure> {
    let mut repos: Vec<(String, String)> = Vec::new();
    let mut put = |key: String, canonical: String| match repos
        .iter_mut()
        .find(|(existing, _)| *existing == key)
    {
        Some(entry) => entry.1 = canonical,
        None => repos.push((key, canonical)),
    };
    let graph = context
        .repositories
        .resolve_graph()
        .map_err(Failure::unknown)?;
    if options.all_repos {
        for module in graph.modules_in_bfs_order() {
            put(
                format!("@@{}", module.canonical_name),
                module.canonical_name.clone(),
            );
        }
        for id in &context.usages.order {
            let names = context.extension_repos(id).map_err(Failure::unknown)?;
            let Some(unique) = context.unique_name(id) else {
                continue;
            };
            for name in names {
                let canonical = format!("{unique}+{name}");
                put(format!("@@{canonical}"), canonical);
            }
        }
    } else if options.all_visible_repos {
        let mapping = context
            .repositories
            .mapping(base_canonical)
            .map_err(Failure::unknown)?;
        for name in context.repositories.mapping_order(base_canonical) {
            if let Some(canonical) = mapping.get(&name)
                && !canonical.is_empty()
            {
                put(format!("@{name}"), canonical.clone());
            }
        }
        for (name, canonical) in mapping {
            if !canonical.is_empty() {
                put(format!("@{name}"), canonical.clone());
            }
        }
    } else {
        let mapping = context
            .repositories
            .mapping(base_canonical)
            .map_err(Failure::unknown)?;
        for arg in args {
            let resolved = super::ModuleArg::parse(arg)
                .map_err(Failure::invalid)
                .and_then(|module| {
                    module.resolve_to_repo_names(&context.inspector, mapping, base_canonical)
                });
            match resolved {
                Ok(names) => {
                    for (key, canonical) in names {
                        put(key, canonical);
                    }
                }
                Err(error) => {
                    let message = error
                        .message
                        .strip_suffix(" Type 'bazel help mod' for syntax and help.")
                        .map(|message| message.strip_suffix('.').unwrap_or(message))
                        .unwrap_or(&error.message)
                        .to_owned();
                    return Err(Failure::invalid(format!(
                        "In repo argument {arg}: {message} (Note that unused modules cannot be used here)"
                    )));
                }
            }
        }
    }
    Ok(repos)
}

/// RepoDefinitionFunction: the definition of `canonical`.
pub(crate) fn definition(
    context: &Context<'_>,
    canonical: &str,
) -> Result<Option<RepoDefinitionValue>, String> {
    if canonical.is_empty() {
        return Ok(None);
    }
    if let Some(definition) = context
        .repositories
        .module_repository_definition(canonical)?
    {
        return Ok(Some(definition));
    }
    Ok(context
        .repositories
        .generated_repository_definition(canonical)?
        .map(RepoDefinitionValue::Found))
}

/// ModExecutor.showRepo with RepoOutputFormatter's text form.
pub(crate) fn show_repo(
    options: &ModOptions,
    definitions: &[(String, RepoDefinitionValue)],
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    if options.output != OutputFormat::Text {
        return Err(Failure::new(
            format!(
                "--output={} for show_repo is not implemented in the native profile (text is)",
                options.output
            ),
            Code::InvalidArguments,
        ));
    }
    let mut text = String::new();
    for (key, definition) in definitions {
        match definition {
            RepoDefinitionValue::Found(definition) => {
                text.push_str(&format!("## {key}:\n"));
                text.push_str(&format!(
                    "load({}, {})\n",
                    repr_string(&definition.rule_bzl),
                    repr_string(&definition.rule_name)
                ));
                text.push_str(&format!("{}(\n", definition.rule_name));
                text.push_str(&format!("  name = {},\n", repr_string(&definition.name)));
                if let Some(original) = &definition.original_name {
                    text.push_str(&format!("  _original_name = {},\n", repr_string(original)));
                }
                for (name, value) in &definition.attributes {
                    text.push_str(&format!("  {name} = {},\n", repr_attribute(value)));
                }
                text.push_str(")\n\n");
            }
            RepoDefinitionValue::Override(path) => {
                text.push_str(&format!(
                    "## {key}:\nBuiltin or overridden repo located at: {path}\n\n"
                ));
            }
        }
    }
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

/// Tag.attributeValues as call arguments, `name` first (tagToFunctionArgs).
fn tag_arguments(tag: &crate::bzlmod::ExtensionTag) -> String {
    let mut names: Vec<&String> = tag.attributes.keys().collect();
    names.sort_by_key(|name| {
        if name.as_str() == "name" {
            ""
        } else {
            name.as_str()
        }
    });
    names
        .into_iter()
        .map(|name| format!("{name}={}", repr_json(&tag.attributes[name])))
        .collect::<Vec<_>>()
        .join(", ")
}

/// ModExecutor.displayExtension.
pub(crate) fn show_extension(
    executor: &mut Executor<'_>,
    id: &ExtensionId,
    from_usages: &[ModuleKey],
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    let mut text = format!("## {id}:\n\nFetched repositories:\n");
    let Some(imports) = executor.repo_imports.get(id) else {
        return Err(Failure::invalid(format!(
            "No extension {id} exists in the dependency graph"
        )));
    };
    let imports = imports.clone();
    let repos = executor
        .context
        .extension_repos(id)
        .map_err(|error| Failure::new(error, Code::ErrorDuringGraphInspection))?;
    for (repo, modules) in &imports {
        let modules: Vec<String> = modules.iter().map(ToString::to_string).collect();
        text.push_str(&format!(
            "  - {repo} (imported by {})\n",
            modules.join(", ")
        ));
    }
    for repo in repos.iter().filter(|repo| !imports.contains_key(*repo)) {
        text.push_str(&format!("  - {repo}\n"));
    }
    text.push('\n');
    let row = &executor.context.usages.table[id];
    let modules: Vec<&ModuleKey> = if from_usages.is_empty() {
        row.keys().collect()
    } else {
        from_usages.iter().collect()
    };
    for module in modules {
        let Some(usage) = row.get(module) else {
            continue;
        };
        let (file, line) = usage
            .proxies
            .first()
            .map_or(("", ""), |proxy| (proxy.file.as_str(), proxy.line.as_str()));
        text.push_str(&format!("## Usage in {module} from {file}:{line}\n"));
        if id.is_innate() {
            let (label, rule) = id.name.rsplit_once(' ').unwrap_or(("", &id.name));
            text.push_str(&format!(
                "{rule} = use_repo_rule(\"{label}\", \"{rule}\")\n"
            ));
            for tag in &usage.tags {
                let callee = if tag.name == "repo" {
                    rule.to_owned()
                } else {
                    format!("{rule}.{}", tag.name)
                };
                text.push_str(&format!("{callee}({})\n", tag_arguments(tag)));
            }
            text.push('\n');
        } else {
            for tag in &usage.tags {
                text.push_str(&format!(
                    "{}.{}({})\n",
                    id.name,
                    tag.name,
                    tag_arguments(tag)
                ));
            }
            text.push_str(&format!("use_repo(\n  {},\n", id.name));
            for proxy in &usage.proxies {
                for (local, exported) in &proxy.imports {
                    if local == exported {
                        text.push_str(&format!("  \"{local}\",\n"));
                    } else {
                        text.push_str(&format!("  {local}=\"{exported}\",\n"));
                    }
                }
            }
            text.push_str(")\n\n");
        }
    }
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reprs_follow_starlark() {
        assert_eq!(repr_string("a\"b\\\n"), "\"a\\\"b\\\\\\n\"");
        assert_eq!(
            repr_attribute(&AttrValue::Strings(vec!["x".into(), "y".into()])),
            "[\"x\", \"y\"]"
        );
        assert_eq!(
            repr_attribute(&AttrValue::StringDict(vec![("k".into(), "v".into())])),
            "{\"k\": \"v\"}"
        );
        assert_eq!(
            repr_attribute(&AttrValue::Label(Some("//pkg:x".into()))),
            "Label(\"@@//pkg:x\")"
        );
        assert_eq!(repr_attribute(&AttrValue::Bool(true)), "True");
        assert_eq!(
            repr_json(&serde_json::json!({"a": [1, null, false]})),
            "{\"a\": [1, None, False]}"
        );
    }
}
