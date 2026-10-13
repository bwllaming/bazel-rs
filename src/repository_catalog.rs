//! Pure direct-repository declaration indexing; never loads or executes a rule.
//!
//! Bazel 9.3 source contract: Selection.walk preserves root-first BFS order;
//! ModuleThreadContext's dependency map is insertion-ordered, and
//! BazelModuleResolutionFunction.computeFinalDepGraph preserves that order.
//! BazelDepGraphFunction then visits usages in module/file order, deduplicates
//! resolved ordinary extension identities, and allocates globally unique prefixes.
//! Innate identities remain opaque and MODULE-owned, NOT definition-file-owned.
//!
//! Ordinary extensions only reserve namespace here. Isolated usages cannot be
//! named exactly from recording metadata (their exported variable is absent).
//! Their prefixes have an underscore-prefixed extension component, disjoint from
//! public direct-rule symbols. We skip those reservations only in that profile;
//! otherwise catalog construction fails explicitly rather than inventing names.
use crate::bzlmod::{ExtensionUsage, ModuleFile};
use crate::label::{Label, validate_repository_name};
use crate::module_graph::ResolvedGraph;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectCatalog {
    pub owner_mappings: BTreeMap<String, BTreeMap<String, String>>,
    pub groups: BTreeMap<String, DirectGroup>,
    pub repositories: BTreeMap<String, DirectRepository>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectGroup {
    pub owner: String,
    /// Exact recorder identity. Aliases and label spellings are never normalized.
    pub raw_identity: String,
    /// Allocated extension name, without the final repository-name separator.
    pub prefix: String,
    pub declarations: Vec<DirectDeclaration>,
    /// Enforced by the runtime at preparation, not by this metadata catalog.
    pub owner_profile_error: Option<String>,
}

impl DirectGroup {
    /// Preparation-time split, matching InnateRunnableExtension.load. The caller
    /// parses the returned label with the COMPLETE `owner_mappings[owner]`.
    /// In particular, definitions may themselves live in a generated repository.
    pub fn definition_request(&self) -> Result<(&str, &str), String> {
        let (label, symbol) = self.raw_identity.rsplit_once(' ').ok_or_else(|| {
            format!(
                "invalid direct repository rule identity {:?}",
                self.raw_identity
            )
        })?;
        if label.is_empty() || !public_identifier(symbol) {
            return Err(format!(
                "invalid direct repository rule request {:?}: expected a nonempty label and public Starlark identifier",
                self.raw_identity
            ));
        }
        Ok((label, symbol))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDeclaration {
    pub name: String,
    pub canonical_name: String,
    pub attributes: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectRepository {
    pub group_id: String,
    pub declaration_index: usize,
    pub mapping: BTreeMap<String, String>,
}

/// Construct owned immutable metadata from a graph resolved with the same dev
/// policy. No filesystem paths, loader, initializer state, or callbacks are used.
pub fn build(graph: &ResolvedGraph, ignore_root_dev: bool) -> Result<DirectCatalog, String> {
    let owners = owner_order(graph, ignore_root_dev)?;
    let mut catalog = DirectCatalog::default();
    let mut prefixes = BTreeSet::new();
    let mut ordinary = BTreeSet::new();
    let mut has_isolated = false;

    for owner in &owners {
        let (module, base_mapping) = owner_data(graph, owner)?;
        let ignore_dev = !owner.is_empty() || ignore_root_dev;
        catalog
            .owner_mappings
            .insert(owner.clone(), base_mapping.clone());
        for usage in &module.extension_usages {
            if !active_usage(usage, ignore_dev) {
                continue;
            }
            if !usage.identity.is_repo_rule() {
                if usage.identity.isolation.is_some() {
                    has_isolated = true;
                    continue;
                }
                // Public extension identities use only bazel_dep mappings, not
                // the full generated-repository mapping assembled below.
                let label = Label::parse_in(&usage.identity.label, owner, "", base_mapping)
                    .map_err(|e| {
                        format!("invalid ordinary extension label in MODULE owner {owner:?}: {e}")
                    })?;
                let symbol = &usage.identity.extension_name;
                if ordinary.insert((label.clone(), symbol.clone())) {
                    allocate_prefix(&mut prefixes, &label.repository, symbol);
                }
                continue;
            }

            let raw = &usage.identity.extension_name;
            // JSON pair encoding avoids ambiguous owner/raw concatenations.
            let group_id = serde_json::to_string(&(owner, raw)).map_err(|e| e.to_string())?;
            let group = catalog.groups.entry(group_id.clone()).or_insert_with(|| {
                // Deliberately FIRST space, unlike definition_request(). This is
                // BazelDepGraphFunction.makeUniqueNameCandidate's pinned quirk.
                let suffix = raw.split_once(' ').expect("innate identity has a space").1;
                DirectGroup {
                    owner: owner.clone(),
                    raw_identity: raw.clone(),
                    prefix: allocate_prefix(&mut prefixes, owner, suffix),
                    declarations: Vec::new(),
                    owner_profile_error: module.source_profile_error.clone(),
                }
            });
            for tag in &usage.tags {
                if tag.dev_dependency && ignore_dev {
                    continue;
                }
                // The tag class is always "repo"; its name attribute is the
                // repository name (InnateRunnableExtension.run, lines 193-196).
                let name = tag.attributes.get("name").and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| format!("direct repository declaration in {group_id} has no nonempty string name"))?;
                validate_repository_name(name)?;
                let canonical_name = format!("{}+{name}", group.prefix);
                validate_repository_name(&canonical_name).map_err(|e| {
                    format!("unsupported direct repository canonical namespace for {raw:?}: {e}")
                })?;
                let declaration_index = group.declarations.len();
                if catalog
                    .repositories
                    .insert(
                        canonical_name.clone(),
                        DirectRepository {
                            group_id: group_id.clone(),
                            declaration_index,
                            mapping: BTreeMap::new(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "duplicate direct repository canonical name {canonical_name:?}"
                    ));
                }
                group.declarations.push(DirectDeclaration {
                    name: name.to_owned(),
                    canonical_name,
                    attributes: tag.attributes.clone(),
                });
            }
            let mapping = catalog
                .owner_mappings
                .get_mut(owner)
                .expect("owner inserted above");
            for import in &usage.imports {
                if import.dev_dependency && ignore_dev {
                    continue;
                }
                if import.local_name.is_empty() {
                    return Err("direct repository import has an empty local name".to_owned());
                }
                validate_repository_name(&import.local_name)?;
                let declaration = group.declarations.iter()
                    .find(|declaration| declaration.name == import.exported_name)
                    .ok_or_else(|| format!("direct repository import {:?} in MODULE owner {owner:?} names undeclared repository {:?}", import.local_name, import.exported_name))?;
                if mapping
                    .insert(
                        import.local_name.clone(),
                        declaration.canonical_name.clone(),
                    )
                    .is_some()
                {
                    return Err(format!(
                        "direct repository import {:?} collides with an existing repository name in MODULE owner {owner:?}",
                        import.local_name
                    ));
                }
            }
        }
    }

    if has_isolated
        && catalog.groups.values().any(|group| {
            !public_identifier(
                group
                    .raw_identity
                    .split_once(' ')
                    .expect("innate identity")
                    .1,
            )
        })
    {
        return Err("isolated extension namespace not supported for direct execution with non-public or invalid rule symbols".to_owned());
    }

    // Only after ALL direct imports have been installed do generated repos
    // inherit the complete MODULE-owner mapping. Internal sibling names win,
    // including self (ModuleExtensionRepoMappingEntriesFunction, lines 69-78).
    for (group_id, group) in &catalog.groups {
        let mut mapping = catalog.owner_mappings[&group.owner].clone();
        for declaration in &group.declarations {
            mapping.insert(declaration.name.clone(), declaration.canonical_name.clone());
        }
        for (declaration_index, declaration) in group.declarations.iter().enumerate() {
            let repository = catalog
                .repositories
                .get_mut(&declaration.canonical_name)
                .expect("declaration indexed above");
            debug_assert_eq!(repository.group_id, *group_id);
            debug_assert_eq!(repository.declaration_index, declaration_index);
            repository.mapping = mapping.clone();
        }
    }
    Ok(catalog)
}

fn active_usage(usage: &ExtensionUsage, ignore_dev: bool) -> bool {
    usage.declarations.is_empty()
        || usage
            .declarations
            .iter()
            .any(|declaration| !declaration.dev_dependency || !ignore_dev)
}

fn public_identifier(symbol: &str) -> bool {
    symbol
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && symbol
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn allocate_prefix(prefixes: &mut BTreeSet<String>, repository: &str, symbol: &str) -> String {
    let base = format!("{repository}+{symbol}");
    let mut candidate = base.clone();
    let mut attempt = 1;
    while !prefixes.insert(candidate.clone()) {
        attempt += 1;
        candidate = format!("{base}{attempt}");
    }
    candidate
}

fn owner_data<'a>(
    graph: &'a ResolvedGraph,
    owner: &str,
) -> Result<(&'a ModuleFile, &'a BTreeMap<String, String>), String> {
    if owner.is_empty() {
        Ok((&graph.root, &graph.root_mapping))
    } else {
        let module = graph.modules.get(owner).ok_or_else(|| {
            format!("internal direct catalog error: selected MODULE owner {owner:?} is missing")
        })?;
        if module.canonical_name != owner {
            return Err(format!(
                "internal direct catalog error: MODULE owner key {owner:?} disagrees with canonical name {:?}",
                module.canonical_name
            ));
        }
        Ok((&module.module, &module.mapping))
    }
}

fn owner_order(graph: &ResolvedGraph, ignore_root_dev: bool) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::from([String::new()]);
    let mut queue = VecDeque::from([String::new()]);
    let mut result = Vec::new();
    while let Some(owner) = queue.pop_front() {
        let (module, mapping) = owner_data(graph, &owner)?;
        for dependency in &module.dependencies {
            if dependency.dev_dependency && (!owner.is_empty() || ignore_root_dev) {
                continue;
            }
            let Some(alias) = dependency.effective_repo_name() else {
                continue;
            };
            let canonical = mapping.get(alias).ok_or_else(|| format!(
                "internal direct catalog error: active dependency @{alias} has no mapping in MODULE owner {owner:?}"
            ))?;
            if seen.insert(canonical.clone()) {
                queue.push_back(canonical.clone());
            }
        }
        result.push(owner);
    }
    if let Some(missing) = graph.modules.keys().find(|owner| !seen.contains(*owner)) {
        return Err(format!(
            "internal direct catalog error: selected MODULE owner {missing:?} is not reachable in dependency declaration BFS order (graph/dev policy mismatch)"
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzlmod::{self, EvaluationContext, ModuleRole};
    use crate::module_graph::{ModuleSource, ResolvedModule};
    use serde_json::json;
    use std::path::PathBuf;

    fn evaluate(source: &str, role: ModuleRole, ignore_dev_dependencies: bool) -> ModuleFile {
        bzlmod::evaluate_source_with_context(
            "MODULE.bazel",
            source.to_owned(),
            EvaluationContext {
                role,
                ignore_dev_dependencies,
            },
        )
        .unwrap()
    }

    fn mapping(module: &ModuleFile, owner: &str) -> BTreeMap<String, String> {
        let mut mapping = BTreeMap::new();
        if owner.is_empty() {
            mapping.insert(String::new(), String::new());
        }
        if let Some(info) = &module.module
            && !info.effective_repo_name().is_empty()
        {
            mapping.insert(info.effective_repo_name().to_owned(), owner.to_owned());
        }
        mapping
    }

    fn graph(source: &str) -> ResolvedGraph {
        let root = evaluate(source, ModuleRole::Root, false);
        ResolvedGraph {
            root_mapping: mapping(&root, ""),
            root,
            modules: BTreeMap::new(),
        }
    }

    fn dependency(graph: &mut ResolvedGraph, alias: &str, canonical: &str, source: &str) {
        let module = evaluate(source, ModuleRole::Dependency, false);
        graph
            .root_mapping
            .insert(alias.to_owned(), canonical.to_owned());
        graph.modules.insert(
            canonical.to_owned(),
            ResolvedModule {
                name: module.module.as_ref().unwrap().name.clone(),
                version: String::new(),
                compatibility_level: 0,
                canonical_name: canonical.to_owned(),
                mapping: mapping(&module, canonical),
                module,
                // Deliberately nonexistent: successful catalog creation cannot
                // require MODULE sources or definition files to be on disk.
                source: ModuleSource::LocalPath(PathBuf::from("/nonexistent/direct-catalog-test")),
            },
        );
    }

    #[test]
    fn module_owner_names_values_and_complete_mappings_without_io() {
        let mut graph = graph(
            r#"
module(name="main", repo_name="self_alias")
bazel_dep(name="dep", repo_name="tools")
r=use_repo_rule("@tools//:absent.bzl", "maker")
r(name="same", value="root")
r(name="sibling")
s=use_repo_rule("@same//:generated.bzl", "other")
s(name="another")
"#,
        );
        dependency(
            &mut graph,
            "tools",
            "dep+",
            r#"
module(name="dep")
r=use_repo_rule("//:absent.bzl", "maker")
r(name="same", value="dependency")
r(name="sibling")
"#,
        );
        let before = graph.clone();
        let catalog = build(&graph, false).unwrap();
        assert_eq!(graph, before);
        assert_eq!(catalog.repositories.len(), 5);
        let root = &catalog.repositories["+maker+same"];
        let dep = &catalog.repositories["dep++maker+same"];
        assert_ne!(root.group_id, dep.group_id);
        assert_eq!(
            catalog.groups[&root.group_id].declarations[root.declaration_index].attributes["value"],
            "root"
        );
        assert_eq!(
            catalog.groups[&dep.group_id].declarations[dep.declaration_index].attributes["value"],
            "dependency"
        );
        assert_eq!(root.mapping[""], "");
        assert_eq!(root.mapping["self_alias"], "");
        assert_eq!(root.mapping["tools"], "dep+");
        assert_eq!(root.mapping["same"], "+maker+same");
        assert_eq!(root.mapping["sibling"], "+maker+sibling");
        assert_eq!(root.mapping["another"], "+other+another");
        assert!(!dep.mapping.contains_key(""));
        assert!(!dep.mapping.contains_key("another"));
        assert_eq!(dep.mapping["dep"], "dep+");
        assert_eq!(dep.mapping["sibling"], "dep++maker+sibling");
        let generated = &catalog.groups[&catalog.repositories["+other+another"].group_id];
        let (raw, symbol) = generated.definition_request().unwrap();
        assert_eq!(symbol, "other");
        assert_eq!(
            Label::parse_in(
                raw,
                &generated.owner,
                "",
                &catalog.owner_mappings[&generated.owner]
            )
            .unwrap()
            .repository,
            "+maker+same"
        );
    }

    #[test]
    fn opaque_aliases_and_file_names_allocate_distinct_groups_in_file_order() {
        let graph = graph(
            r#"
module(name="main")
a=use_repo_rule("//:same.bzl", "maker")
b=use_repo_rule("@main//:same.bzl", "maker")
c=use_repo_rule("//:different.bzl", "maker")
b(name="first")
a(name="second")
c(name="third")
"#,
        );
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.groups.len(), 3);
        for canonical in ["+maker+first", "+maker2+second", "+maker3+third"] {
            assert!(catalog.repositories.contains_key(canonical));
        }
        let group = &catalog.groups[&catalog.repositories["+maker+first"].group_id];
        assert_eq!(group.raw_identity, "@main//:same.bzl maker");
        assert_eq!(
            serde_json::from_str::<(String, String)>(
                &catalog.repositories["+maker+first"].group_id
            )
            .unwrap(),
            (String::new(), group.raw_identity.clone())
        );
    }

    #[test]
    fn ordinary_reservations_share_resolved_identity_but_not_different_files() {
        let graph = graph(
            r#"
module(name="main")
a=use_extension("//:one.bzl", "maker")
b=use_extension("@main//:one.bzl", "maker")
c=use_extension("//:two.bzl", "maker")
use_repo(a, "ordinary_output")
r=use_repo_rule("//:rules.bzl", "maker")
r(name="direct")
"#,
        );
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.repositories.len(), 1);
        assert!(catalog.repositories.contains_key("+maker3+direct"));
        assert!(!catalog.owner_mappings[""].contains_key("ordinary_output"));
    }

    #[test]
    fn shared_ordinary_identity_is_deduplicated_across_owners() {
        let mut graph = graph(
            r#"
module(name="main")
bazel_dep(name="dep")
e=use_extension("@dep//:ext.bzl", "maker")
"#,
        );
        dependency(
            &mut graph,
            "dep",
            "dep+",
            r#"
module(name="dep")
e=use_extension("//:ext.bzl", "maker")
r=use_repo_rule("//:rules.bzl", "maker")
r(name="direct")
"#,
        );
        assert!(
            build(&graph, false)
                .unwrap()
                .repositories
                .contains_key("dep++maker2+direct")
        );
    }

    #[test]
    fn module_bfs_is_declaration_order_not_lexical_or_depth_first() {
        let mut graph = graph("module(name='main')\nbazel_dep(name='z')\nbazel_dep(name='a')");
        dependency(
            &mut graph,
            "z",
            "z+",
            "module(name='z')\nbazel_dep(name='deep')\ne=use_extension('@@//:z.bzl','maker')",
        );
        dependency(
            &mut graph,
            "a",
            "a+",
            "module(name='a')\ne=use_extension('@@//:a.bzl','maker')",
        );
        dependency(
            &mut graph,
            "deep",
            "deep+",
            "module(name='deep')\nbazel_dep(name='main')\ne=use_extension('@@//:deep.bzl','maker')",
        );
        graph.root_mapping.remove("deep");
        graph
            .modules
            .get_mut("z+")
            .unwrap()
            .mapping
            .insert("deep".to_owned(), "deep+".to_owned());
        graph
            .modules
            .get_mut("deep+")
            .unwrap()
            .mapping
            .insert("main".to_owned(), String::new());
        assert_eq!(
            owner_order(&graph, false).unwrap(),
            ["", "z+", "a+", "deep+"]
        );
        // Reservation by root happens before all dependencies even though its
        // declaration occurs after the bazel_dep statements.
        graph.root.extension_usages = evaluate(
            "r=use_repo_rule('//:rules.bzl','maker')\nr(name='direct')",
            ModuleRole::Root,
            false,
        )
        .extension_usages;
        assert!(
            build(&graph, false)
                .unwrap()
                .repositories
                .contains_key("+maker+direct")
        );
    }

    #[test]
    fn sibling_internal_names_override_owner_aliases_and_all_groups_are_visible() {
        let mut graph = graph(
            r#"
module(name="main")
bazel_dep(name="dep")
r=use_repo_rule("//:rules.bzl", "maker")
r(name="internal")
r(name="sibling")
s=use_repo_rule("//:other.bzl", "other")
s(name="other")
"#,
        );
        dependency(&mut graph, "dep", "dep+", "module(name='dep')");
        // Hand-built import metadata tests the internal/exported distinction.
        // Real use_repo_rule imports normally have identical names.
        let usage = &mut graph.root.extension_usages[0];
        usage.tags[0]
            .attributes
            .insert("name".to_owned(), json!("dep"));
        usage.imports[0].exported_name = "dep".to_owned();
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.owner_mappings[""]["dep"], "dep+");
        assert_eq!(catalog.owner_mappings[""]["internal"], "+maker+dep");
        assert_eq!(
            catalog.repositories["+maker+sibling"].mapping["dep"],
            "+maker+dep"
        );
        assert_eq!(
            catalog.repositories["+maker+sibling"].mapping["other"],
            "+other+other"
        );
        assert_eq!(catalog.repositories["+other+other"].mapping["dep"], "dep+");
    }

    #[test]
    fn dev_filtering_applies_to_declarations_imports_and_ordinary_reservations() {
        let source = r#"
e=use_extension("//:ordinary.bzl", "maker", dev_dependency=True)
r=use_repo_rule("//:rules.bzl", "maker")
r(name="ignored", dev_dependency=True)
r(name="kept")
"#;
        let graph = graph(source);
        assert!(
            build(&graph, false)
                .unwrap()
                .repositories
                .contains_key("+maker2+ignored")
        );
        let filtered = build(&graph, true).unwrap();
        assert_eq!(filtered.repositories.len(), 1);
        assert!(filtered.repositories.contains_key("+maker+kept"));
        assert!(!filtered.owner_mappings[""].contains_key("ignored"));
        let evaluated = evaluate(source, ModuleRole::Root, true);
        let filtered_graph = ResolvedGraph {
            root: evaluated,
            root_mapping: graph.root_mapping.clone(),
            modules: BTreeMap::new(),
        };
        assert_eq!(filtered, build(&filtered_graph, true).unwrap());

        let mut graph = super::tests::graph("bazel_dep(name='dep')");
        dependency(
            &mut graph,
            "dep",
            "dep+",
            &format!("module(name='dep')\n{source}"),
        );
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.repositories.len(), 1);
        assert!(catalog.repositories.contains_key("dep++maker+kept"));
        assert!(!catalog.owner_mappings["dep+"].contains_key("ignored"));
    }

    #[test]
    fn unused_and_invalid_definition_requests_are_not_loaded_or_parsed() {
        let mut graph = graph(
            "unused=use_repo_rule('invalid label','bad symbol')\nr=use_repo_rule('@not_declared//:bad.bzl','maker')\nr(name='repo')",
        );
        graph.root.source_profile_error = Some("deferred source profile failure".to_owned());
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.groups.len(), 1);
        let group = catalog.groups.values().next().unwrap();
        assert_eq!(group.owner_profile_error, graph.root.source_profile_error);
        assert_eq!(
            group.definition_request().unwrap(),
            ("@not_declared//:bad.bzl", "maker")
        );
        assert!(
            Label::parse_in(
                group.definition_request().unwrap().0,
                "",
                "",
                &catalog.owner_mappings[""]
            )
            .is_err()
        );

        // Recorded empty usages reserve a prefix but create no repositories.
        graph.root.extension_usages[0].tags.clear();
        graph.root.extension_usages[0].imports.clear();
        graph.root.extension_usages[0].identity.extension_name = "invalid label maker".to_owned();
        let empty = build(&graph, false).unwrap();
        let group = empty.groups.values().next().unwrap();
        assert_eq!(group.prefix, "+label maker");
        assert_eq!(
            group.definition_request().unwrap(),
            ("invalid label", "maker")
        );
        assert!(empty.repositories.is_empty());
        assert_eq!(empty.owner_mappings[""], graph.root_mapping);
    }

    #[test]
    fn malformed_symbol_errors_are_deferred_unless_canonical_name_is_unsafe() {
        let graph = graph("r=use_repo_rule('//:r.bzl','_private')\nr(name='repo')");
        let catalog = build(&graph, false).unwrap();
        assert!(
            catalog
                .groups
                .values()
                .next()
                .unwrap()
                .definition_request()
                .unwrap_err()
                .contains("public Starlark identifier")
        );
        let graph = super::tests::graph("r=use_repo_rule('invalid label','maker')\nr(name='repo')");
        assert!(
            build(&graph, false)
                .unwrap_err()
                .contains("canonical namespace")
        );
    }

    #[test]
    fn isolated_extensions_are_disjoint_only_in_public_direct_profile() {
        let source = "e=use_extension('//:ext.bzl','maker',isolate=True)\nr=use_repo_rule('//:r.bzl','maker')\nr(name='repo')";
        let catalog = build(&graph(source), false).unwrap();
        assert!(catalog.repositories.contains_key("+maker+repo"));
        let invalid = source.replace("'//:r.bzl','maker'", "'//:r.bzl','_maker'");
        assert!(
            build(&graph(&invalid), false)
                .unwrap_err()
                .contains("isolated extension namespace not supported")
        );
    }

    #[test]
    fn duplicate_missing_imports_and_inconsistent_graphs_fail_explicitly() {
        let mut graph =
            graph("module(name='main')\nr=use_repo_rule('//:r.bzl','maker')\nr(name='repo')");
        graph.root.extension_usages[0].imports[0].local_name = "main".to_owned();
        assert!(build(&graph, false).unwrap_err().contains("collides"));
        graph.root.extension_usages[0].imports[0].local_name = "repo".to_owned();
        graph.root.extension_usages[0].imports[0].exported_name = "missing".to_owned();
        assert!(
            build(&graph, false)
                .unwrap_err()
                .contains("undeclared repository")
        );
        dependency(&mut graph, "dep", "dep+", "module(name='dep')");
        assert!(build(&graph, false).unwrap_err().contains("not reachable"));
    }

    #[test]
    fn bfs_ignores_nodep_and_dev_edges_and_rejects_missing_active_mappings() {
        let graph = graph(
            "bazel_dep(name='nodep',repo_name=None)\nbazel_dep(name='dev',dev_dependency=True)",
        );
        assert_eq!(owner_order(&graph, true).unwrap(), [""]);
        assert!(
            owner_order(&graph, false)
                .unwrap_err()
                .contains("active dependency @dev")
        );
    }
}
