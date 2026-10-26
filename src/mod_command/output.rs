//! The graph output formatters (bzlmod/modcommand/OutputFormatters.java,
//! TextOutputFormatter.java, JsonOutputFormatter.java,
//! GraphvizOutputFormatter.java).
use super::executor::{Executor, ResultGraph, ResultNode};
use super::{Charset, ExtensionId, ExtensionShow, ModuleKey, OutputFormat};
use crate::module_graph::ResolutionReason;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet, VecDeque};

/// OutputFormatter.Explanation.
struct Explanation {
    changed_version: String,
    reason: ResolutionReason,
    requested_by: Option<Vec<ModuleKey>>,
}

impl Explanation {
    fn text(&self, unused: bool) -> String {
        let version = if self.changed_version.is_empty() {
            "_"
        } else {
            &self.changed_version
        };
        let reason = match &self.requested_by {
            Some(modules) => modules
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            None => self.reason.name().to_owned(),
        };
        format!(
            "({} {version}, cause {reason})",
            if unused { "to" } else { "was" }
        )
    }
}

struct Formatter<'e, 'a> {
    executor: &'e Executor<'a>,
    result: &'e ResultGraph,
}

impl Formatter<'_, '_> {
    fn module(&self, key: &ModuleKey) -> &super::Augmented {
        self.executor.context.inspector.module(key)
    }

    fn node(&self, key: &ModuleKey) -> ResultNode {
        self.result.get(key).cloned().unwrap_or_default()
    }

    /// The extensions whose repositories `key` imports, sorted.
    fn extensions_used(&self, key: &ModuleKey) -> Vec<&ExtensionId> {
        self.executor
            .repo_imports
            .iter()
            .filter(|(_, imports)| imports.values().any(|modules| modules.contains(key)))
            .map(|(id, _)| id)
            .collect()
    }

    /// The repositories of `id` that `key` imports, sorted.
    fn imports_of(&self, id: &ExtensionId, key: &ModuleKey) -> Vec<&str> {
        self.executor.repo_imports[id]
            .iter()
            .filter(|(_, modules)| modules.contains(key))
            .map(|(repo, _)| repo.as_str())
            .collect()
    }

    /// The repositories of `id` no module imports, sorted.
    fn unused_repos(&self, id: &ExtensionId) -> Vec<&str> {
        let imported = &self.executor.repo_imports[id];
        self.executor
            .extension_repos
            .get(id)
            .map(|repos| {
                repos
                    .iter()
                    .filter(|repo| !imported.contains_key(*repo))
                    .map(String::as_str)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// getExtraResolutionExplanation.
    fn explanation(&self, key: &ModuleKey, parent: &ModuleKey) -> Option<Explanation> {
        if key.is_root() {
            return None;
        }
        let graph = &self.executor.context.inspector.graph;
        let module = graph.get(key)?;
        let parent = graph.get(parent)?;
        let all = parent.all_deps(self.executor.options.include_unused);
        let repo_name = all.get(key)?;
        let reason = *parent.dep_reasons.get(repo_name)?;
        if reason == ResolutionReason::Original {
            return None;
        }
        let replacement = if module.is_used() {
            module
        } else {
            graph.get(parent.dep(repo_name)?)?
        };
        let changed_version = if !module.is_used() {
            replacement.version.clone()
        } else {
            graph.get(parent.unused_dep(repo_name)?)?.version.clone()
        };
        let requested_by = (reason == ResolutionReason::MinimalVersionSelection)
            .then(|| replacement.original_dependants.clone());
        Some(Explanation {
            changed_version,
            reason,
            requested_by,
        })
    }
}

pub(crate) fn format(executor: &Executor<'_>, result: &ResultGraph, out: &mut Vec<u8>) {
    let formatter = Formatter { executor, result };
    let text = match executor.options.output {
        OutputFormat::Json => json_output(&formatter),
        OutputFormat::Graph => graph_output(&formatter),
        _ => text_output(&formatter),
    };
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

/// TextOutputFormatter.DrawCharset.
struct Draw {
    empty: &'static str,
    previous: &'static str,
    child: &'static str,
    indirect_child: &'static str,
    last_child: &'static str,
    last_indirect_child: &'static str,
}

const ASCII: Draw = Draw {
    empty: "    ",
    previous: "|   ",
    child: "|___",
    indirect_child: "|...",
    last_child: "|___",
    last_indirect_child: "|...",
};

const UTF8: Draw = Draw {
    empty: "    ",
    previous: "│   ",
    child: "├───",
    indirect_child: "├╌╌╌",
    last_child: "└───",
    last_indirect_child: "└╌╌╌",
};

struct Text<'f, 'e, 'a> {
    formatter: &'f Formatter<'e, 'a>,
    draw: &'static Draw,
    /// isLastChildStack, oldest first.
    last: Vec<bool>,
    seen_extensions: HashSet<ExtensionId>,
    visited: HashSet<ModuleKey>,
    out: String,
}

impl Text<'_, '_, '_> {
    fn drawing(&mut self, indirect: bool, depth: usize) {
        if depth == 0 {
            return;
        }
        let indents = self.last.len() - 1;
        for index in 0..indents {
            self.out.push_str(if self.last[index] {
                self.draw.empty
            } else {
                self.draw.previous
            });
        }
        let last = *self.last.last().expect("a child");
        self.out.push_str(match (indirect, last) {
            (true, true) => self.draw.last_indirect_child,
            (true, false) => self.draw.indirect_child,
            (false, true) => self.draw.last_child,
            (false, false) => self.draw.child,
        });
    }

    fn extension(&mut self, key: &ModuleKey, id: &ExtensionId, unexpanded: bool, depth: usize) {
        let info = self.formatter.executor.options.extension_info;
        self.drawing(false, depth);
        self.out.push('$');
        self.out.push_str(&id.to_string());
        self.out.push(' ');
        if unexpanded && info == ExtensionShow::All {
            self.out.push_str("... ");
        }
        self.out.push('\n');
        if info == ExtensionShow::Usages {
            return;
        }
        let imports: Vec<String> = self
            .formatter
            .imports_of(id, key)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let unused: Vec<String> = if !unexpanded && info == ExtensionShow::All {
            self.formatter
                .unused_repos(id)
                .into_iter()
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
        let total = imports.len() + unused.len();
        let mut current = 1;
        for repo in &imports {
            self.last.push(current == total);
            current += 1;
            self.drawing(false, depth + 1);
            self.out.push_str(repo);
            self.out.push('\n');
            self.last.pop();
        }
        if unexpanded || info == ExtensionShow::Repos {
            return;
        }
        for repo in &unused {
            self.last.push(current == total);
            current += 1;
            self.drawing(true, depth + 1);
            self.out.push_str(repo);
            self.out.push('\n');
            self.last.pop();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn module(
        &mut self,
        key: &ModuleKey,
        parent: Option<&ModuleKey>,
        expanded: bool,
        indirect: bool,
        cycle: bool,
        depth: usize,
    ) {
        let options = self.formatter.executor.options;
        self.drawing(indirect, depth);
        let added = self.visited.insert(key.clone());
        let node = self.formatter.node(key);
        let module = self.formatter.module(key);
        if key.is_root() {
            let version = if module.version.is_empty() {
                "_"
            } else {
                &module.version
            };
            self.out
                .push_str(&format!("<root> ({}@{version})", module.name));
        } else {
            self.out.push_str(&format!("{key} "));
        }
        let mut total = node.size();
        let used: Vec<ExtensionId> = self
            .formatter
            .extensions_used(key)
            .into_iter()
            .cloned()
            .collect();
        if options.extension_info != ExtensionShow::Hidden {
            total += used.len();
        }
        let is_cycle = cycle || !added;
        if is_cycle {
            self.out.push_str("(cycle) ");
        } else if !expanded {
            self.out.push_str("(*) ");
        } else if node.target {
            self.out.push_str("# ");
        }
        if !options.verbose && !module.is_used() {
            self.out.push_str("(unused) ");
        }
        if !indirect
            && options.verbose
            && let Some(parent) = parent
            && let Some(explanation) = self.formatter.explanation(key, parent)
        {
            self.out.push_str(&explanation.text(!module.is_used()));
        }
        self.out.push('\n');
        if expanded && !is_cycle {
            let mut current = 1;
            if options.extension_info != ExtensionShow::Hidden {
                for id in &used {
                    let unexpanded = !self.seen_extensions.insert(id.clone());
                    self.last.push(current == total);
                    current += 1;
                    self.extension(key, id, unexpanded, depth + 1);
                    self.last.pop();
                }
            }
            for (child, edge) in node.sorted_by_edge_type() {
                self.last.push(current == total);
                current += 1;
                self.module(
                    &child,
                    Some(key),
                    edge.expanded,
                    edge.indirect,
                    edge.cycle,
                    depth + 1,
                );
                self.last.pop();
            }
        }
        if added {
            self.visited.remove(key);
        }
    }
}

fn text_output(formatter: &Formatter<'_, '_>) -> String {
    let mut text = Text {
        formatter,
        draw: if formatter.executor.options.charset == Charset::Ascii {
            &ASCII
        } else {
            &UTF8
        },
        last: Vec::new(),
        seen_extensions: HashSet::new(),
        visited: HashSet::new(),
        out: String::new(),
    };
    text.module(&ModuleKey::root(), None, true, false, false, 0);
    text.out
}

struct Json<'f, 'e, 'a> {
    formatter: &'f Formatter<'e, 'a>,
    seen_extensions: HashSet<ExtensionId>,
}

impl Json<'_, '_, '_> {
    fn key(key: &ModuleKey) -> String {
        key.to_string()
    }

    fn extension(&self, key: &ModuleKey, id: &ExtensionId, unexpanded: bool) -> Value {
        let info = self.formatter.executor.options.extension_info;
        let mut json = Map::new();
        json.insert("key".into(), json!(id.to_string()));
        json.insert("unexpanded".into(), json!(unexpanded));
        if info == ExtensionShow::Usages {
            return Value::Object(json);
        }
        json.insert(
            "used_repos".into(),
            json!(self.formatter.imports_of(id, key)),
        );
        if unexpanded || info == ExtensionShow::Repos {
            return Value::Object(json);
        }
        json.insert(
            "unused_repos".into(),
            json!(self.formatter.unused_repos(id)),
        );
        Value::Object(json)
    }

    fn module(
        &mut self,
        key: &ModuleKey,
        parent: Option<&ModuleKey>,
        expanded: bool,
        indirect: bool,
    ) -> Value {
        let options = self.formatter.executor.options;
        let node = self.formatter.node(key);
        let module = self.formatter.module(key);
        let mut json = Map::new();
        json.insert("key".into(), json!(Self::key(key)));
        json.insert("name".into(), json!(module.name));
        json.insert("version".into(), json!(module.version));
        let apparent = match parent {
            Some(parent) => self
                .formatter
                .module(parent)
                .dep_alias(key)
                .map(str::to_owned),
            None => Some(module.repo_name.clone()),
        };
        if let Some(apparent) = apparent {
            json.insert("apparentName".into(), json!(apparent));
        }
        if !indirect
            && options.verbose
            && let Some(parent) = parent
            && let Some(explanation) = self.formatter.explanation(key, parent)
        {
            if !module.is_used() {
                json.insert("unused".into(), json!(true));
                json.insert("resolvedVersion".into(), json!(explanation.changed_version));
            } else {
                json.insert("originalVersion".into(), json!(explanation.changed_version));
            }
            // Genuine writes the changed version here (JsonOutputFormatter.java:119).
            json.insert(
                "resolutionReason".into(),
                json!(explanation.changed_version),
            );
            if let Some(requested_by) = &explanation.requested_by {
                let keys: Vec<String> = requested_by.iter().map(Self::key).collect();
                json.insert("resolvedRequestedBy".into(), json!(keys));
            }
        }
        if !expanded {
            json.insert("unexpanded".into(), json!(true));
            return Value::Object(json);
        }
        let mut dependencies = Vec::new();
        let mut indirect_dependencies = Vec::new();
        let mut cycles = Vec::new();
        for (child, edge) in node.sorted_by_edge_type() {
            if edge.cycle {
                cycles.push(self.module(&child, Some(key), false, false));
            } else if edge.indirect {
                indirect_dependencies.push(self.module(&child, Some(key), edge.expanded, true));
            } else {
                dependencies.push(self.module(&child, Some(key), edge.expanded, false));
            }
        }
        json.insert("dependencies".into(), Value::Array(dependencies));
        json.insert(
            "indirectDependencies".into(),
            Value::Array(indirect_dependencies),
        );
        json.insert("cycles".into(), Value::Array(cycles));
        if options.extension_info == ExtensionShow::Hidden {
            return Value::Object(json);
        }
        let used: Vec<ExtensionId> = self
            .formatter
            .extensions_used(key)
            .into_iter()
            .cloned()
            .collect();
        let mut usages = Vec::new();
        for id in &used {
            let unexpanded = !self.seen_extensions.insert(id.clone());
            usages.push(self.extension(key, id, unexpanded));
        }
        json.insert("extensionUsages".into(), Value::Array(usages));
        Value::Object(json)
    }
}

/// Gson's pretty printer: two-space indentation, `"key": value`, `[]` and
/// `{}` for empty containers, HTML characters unescaped.
fn gson_pretty(value: &Value) -> String {
    let mut out = String::new();
    gson_write(value, 0, &mut out);
    out
}

fn gson_write(value: &Value, indent: usize, out: &mut String) {
    let pad = |depth: usize| "  ".repeat(depth);
    match value {
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                gson_write(item, indent + 1, out);
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        Value::Object(map) => {
            out.push_str("{\n");
            for (index, (key, item)) in map.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                out.push_str(&gson_string(key));
                out.push_str(": ");
                gson_write(item, indent + 1, out);
                if index + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
        Value::String(text) => out.push_str(&gson_string(text)),
        other => out.push_str(&other.to_string()),
    }
}

/// JsonWriter.string without HTML escaping.
pub(crate) fn gson_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_output(formatter: &Formatter<'_, '_>) -> String {
    let mut json = Json {
        formatter,
        seen_extensions: HashSet::new(),
    };
    let mut root = json.module(&ModuleKey::root(), None, true, false);
    if let Value::Object(map) = &mut root {
        map.insert("root".into(), json!(true));
    }
    gson_pretty(&root)
}

fn graph_output(formatter: &Formatter<'_, '_>) -> String {
    let options = formatter.executor.options;
    let id = |key: &ModuleKey| {
        if key.is_root() {
            "\"<root>\"".to_owned()
        } else {
            format!("\"{key}\"")
        }
    };
    let repo_id = |extension: &ExtensionId, repo: &str| format!("\"{extension}%{repo}\"");
    let mut out = String::from("digraph mygraph {\n  node [ shape=box ]\n  edge [ fontsize=8 ]\n");
    let mut seen: HashSet<ModuleKey> = HashSet::from([ModuleKey::root()]);
    let mut seen_extensions: HashSet<ExtensionId> = HashSet::new();
    let mut queue = VecDeque::from([ModuleKey::root()]);
    while let Some(key) = queue.pop_front() {
        let module = formatter.module(&key);
        let node = formatter.node(&key);
        let source = id(&key);
        if key.is_root() {
            out.push_str(&format!(
                "  \"<root>\" [ label=\"<root> ({}@{})\" ]\n",
                module.name, module.version
            ));
        } else if node.target || !module.is_used() {
            out.push_str(&format!(
                "  {} [ shape={} style={} ]\n",
                id(&key),
                if node.target { "diamond" } else { "box" },
                if module.is_used() { "solid" } else { "dotted" }
            ));
        }
        if options.extension_info != ExtensionShow::Hidden {
            for extension in formatter.extensions_used(&key) {
                if options.extension_info == ExtensionShow::Usages {
                    out.push_str(&format!("  {} -> \"{extension}\"\n", id(&key)));
                    continue;
                }
                if seen_extensions.insert(extension.clone()) {
                    graph_extension(formatter, extension, &mut out);
                }
                for repo in formatter.imports_of(extension, &key) {
                    out.push_str(&format!("  {} -> {}\n", id(&key), repo_id(extension, repo)));
                }
            }
        }
        for (child, edge) in node.sorted_by_key() {
            if !edge.indirect {
                let label = if options.verbose {
                    formatter
                        .explanation(&child, &key)
                        .map(|explanation| explanation.reason.label())
                        .filter(|label| !label.is_empty())
                        .map(|label| format!("label={label}"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                out.push_str(&format!("  {source} -> {} [ {label} ]\n", id(&child)));
            } else {
                out.push_str(&format!("  {source} -> {} [ style=dashed ]\n", id(&child)));
            }
            if seen.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    }
    out.push('}');
    out
}

fn graph_extension(formatter: &Formatter<'_, '_>, extension: &ExtensionId, out: &mut String) {
    let info = formatter.executor.options.extension_info;
    out.push_str(&format!("  subgraph \"cluster_{extension}\" {{\n"));
    out.push_str(&format!("    label=\"{extension}\"\n"));
    if info == ExtensionShow::Usages {
        return;
    }
    let used: BTreeSet<&String> = formatter.executor.repo_imports[extension].keys().collect();
    for repo in &used {
        out.push_str(&format!(
            "    \"{extension}%{repo}\" [ label=\"{repo}\" ]\n"
        ));
    }
    // Genuine leaves the cluster open here (GraphvizOutputFormatter.java:141-143).
    if info == ExtensionShow::Repos {
        return;
    }
    for repo in formatter.unused_repos(extension) {
        out.push_str(&format!(
            "    \"{extension}%{repo}\" [ label=\"{repo}\" style=dotted ]\n"
        ));
    }
    out.push_str("  }\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gson_pretty_matches_the_genuine_layout() {
        let value = json!({"key": "<root>", "a": [], "b": [{"x": true}], "c": "q\"<"});
        assert_eq!(
            gson_pretty(&value),
            "{\n  \"key\": \"<root>\",\n  \"a\": [],\n  \"b\": [\n    {\n      \"x\": true\n    }\n  ],\n  \"c\": \"q\\\"<\"\n}"
        );
    }
}
