//! Pure repository declaration indexing; never loads or executes a rule or extension.
//!
//! Bazel 9.3 source contract: Selection.walk preserves root-first BFS order;
//! ModuleThreadContext's dependency map is insertion-ordered, and
//! BazelModuleResolutionFunction.computeFinalDepGraph preserves that order.
//! BazelDepGraphFunction then visits usages in module/file order, deduplicates
//! resolved ordinary extension identities, and allocates globally unique prefixes.
//! Innate identities remain opaque and MODULE-owned, NOT definition-file-owned.
//!
//! Ordinary extensions record participants, import mappings, and exact prefix
//! routes here, but never claim that generated repositories exist. Isolated usages cannot be
//! named exactly from recording metadata (their exported variable is absent).
//! Their prefixes have an underscore-prefixed extension component, disjoint from
//! public direct-rule symbols. We skip those reservations only in that profile;
//! otherwise catalog construction fails explicitly rather than inventing names.
use crate::bzlmod::{
    ExtensionTag, ExtensionUsage, ModuleFile, RepositoryImport, validate_repo_name,
};
use crate::label::{Label, validate_repository_name};
use crate::module_graph::ResolvedGraph;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectCatalog {
    pub owner_mappings: BTreeMap<String, BTreeMap<String, String>>,
    pub groups: BTreeMap<String, DirectGroup>,
    pub repositories: BTreeMap<String, DirectRepository>,
    pub ordinary_groups: BTreeMap<String, OrdinaryGroup>,
    /// Exact allocated prefix -> ordinary group ID, not a materialization index.
    /// A runtime must split a canonical repository at its LAST '+' and look up
    /// the complete prefix, never select a group using string-prefix matching.
    pub ordinary_routes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrdinaryGroup {
    /// Requested public label, not the implementation object's defining label.
    pub label: Label,
    pub symbol: String,
    pub prefix: String,
    /// Root-first dependency BFS, including active users without any tags.
    pub participants: Vec<OrdinaryParticipant>,
    pub root_has_non_dev_dependency: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrdinaryParticipant {
    pub owner: String,
    pub name: String,
    /// Normalized declared version, including for non-registry overrides.
    pub version: String,
    pub is_root: bool,
    /// Invocation order across all raw spellings of this public identity.
    pub tags: Vec<ExtensionTag>,
    pub imports: Vec<RepositoryImport>,
    pub source_profile_error: Option<String>,
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
    let mut has_isolated = false;
    let mut budget = crate::repository_budget::HostBudget::default();

    for owner in &owners {
        let (module, base_mapping) = owner_data(graph, owner)?;
        let ignore_dev = !owner.is_empty() || ignore_root_dev;
        budget.string(owner)?;
        charge_mapping(&mut budget, base_mapping)?;
        catalog
            .owner_mappings
            .insert(owner.clone(), base_mapping.clone());
        for usage in &module.extension_usages {
            if !active_usage(usage, ignore_dev) {
                continue;
            }
            budget.string(&usage.identity.label)?;
            budget.string(&usage.identity.extension_name)?;
            if !usage.identity.is_repo_rule() {
                if usage.identity.isolation.is_some() {
                    has_isolated = true;
                    continue;
                }
                // Public extension identities use only bazel_dep mappings, not
                // the full generated-repository mapping assembled below.
                // ModuleFileGlobals.normalizeLabelString (Bazel 9.3:486-519)
                // stringifies even @@repo labels as APPARENT @repo labels before
                // BazelDepGraphFunction resolves them. This MODULE-only step
                // must not alter direct rule requests or general label parsing.
                let raw_label = &usage.identity.label;
                let apparent_label = if raw_label.starts_with("@@") {
                    &raw_label[1..]
                } else {
                    raw_label
                };
                let label =
                    Label::parse_in(apparent_label, owner, "", base_mapping).map_err(|e| {
                        format!("invalid ordinary extension label in MODULE owner {owner:?}: {e}")
                    })?;
                record_ordinary(
                    &mut catalog,
                    &mut prefixes,
                    owner,
                    module,
                    usage,
                    label,
                    ignore_dev,
                    &mut budget,
                )?;
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
                budget.string(&group.prefix)?;
                for (key, value) in &tag.attributes {
                    budget.string(key)?;
                    budget.json(value, 0)?;
                }
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
                budget.string(&import.local_name)?;
                budget.string(&declaration.canonical_name)?;
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

    for group in catalog.ordinary_groups.values_mut() {
        for participant in &mut group.participants {
            participant.tags.sort_by_key(|tag| tag.ordinal);
        }
    }

    if has_isolated
        && (catalog.groups.values().any(|group| {
            !public_identifier(
                group
                    .raw_identity
                    .split_once(' ')
                    .expect("innate identity")
                    .1,
            )
        }) || catalog
            .ordinary_groups
            .values()
            .any(|group| !public_identifier(&group.symbol)))
    {
        return Err("isolated extension namespace not supported for direct execution with non-public or invalid rule symbols".to_owned());
    }

    // Only after ALL direct and ordinary imports have been installed do direct repos
    // inherit the complete MODULE-owner mapping. Internal sibling names win,
    // including self (ModuleExtensionRepoMappingEntriesFunction, lines 69-78).
    for (group_id, group) in &catalog.groups {
        charge_mapping(&mut budget, &catalog.owner_mappings[&group.owner])?;
        let mut mapping = catalog.owner_mappings[&group.owner].clone();
        for declaration in &group.declarations {
            budget.string(&declaration.name)?;
            budget.string(&declaration.canonical_name)?;
            mapping.insert(declaration.name.clone(), declaration.canonical_name.clone());
        }
        for (declaration_index, declaration) in group.declarations.iter().enumerate() {
            let repository = catalog
                .repositories
                .get_mut(&declaration.canonical_name)
                .expect("declaration indexed above");
            debug_assert_eq!(repository.group_id, *group_id);
            debug_assert_eq!(repository.declaration_index, declaration_index);
            charge_mapping(&mut budget, &mapping)?;
            repository.mapping = mapping.clone();
        }
    }
    Ok(catalog)
}

fn charge_mapping(
    budget: &mut crate::repository_budget::HostBudget,
    mapping: &BTreeMap<String, String>,
) -> Result<(), String> {
    budget.node()?;
    for (apparent, canonical) in mapping {
        budget.string(apparent)?;
        budget.string(canonical)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_ordinary(
    catalog: &mut DirectCatalog,
    prefixes: &mut BTreeSet<String>,
    owner: &str,
    module: &ModuleFile,
    usage: &ExtensionUsage,
    label: Label,
    ignore_dev: bool,
    budget: &mut crate::repository_budget::HostBudget,
) -> Result<(), String> {
    let symbol = &usage.identity.extension_name;
    let group_id =
        serde_json::to_string(&(label.to_string(), symbol)).map_err(|e| e.to_string())?;
    if !catalog.ordinary_groups.contains_key(&group_id) {
        let prefix = allocate_prefix(prefixes, &label.repository, symbol);
        validate_repository_name(&prefix).map_err(|e| {
            format!("unsupported ordinary extension canonical namespace for {label} {symbol}: {e}")
        })?;
        catalog
            .ordinary_routes
            .insert(prefix.clone(), group_id.clone());
        catalog.ordinary_groups.insert(
            group_id.clone(),
            OrdinaryGroup {
                label,
                symbol: symbol.clone(),
                prefix,
                participants: Vec::new(),
                root_has_non_dev_dependency: false,
            },
        );
    }
    let group = catalog
        .ordinary_groups
        .get_mut(&group_id)
        .expect("ordinary group inserted above");
    group.root_has_non_dev_dependency |= owner.is_empty()
        && usage
            .declarations
            .iter()
            .any(|declaration| !declaration.dev_dependency);
    let participant_index = match group.participants.iter().position(|p| p.owner == owner) {
        Some(index) => index,
        None => {
            budget.string(owner)?;
            if let Some(info) = &module.module {
                budget.string(&info.name)?;
                budget.string(&info.version)?;
            }
            let (name, version) = match &module.module {
                Some(info) => (
                    info.name.clone(),
                    crate::module_graph::normalize_declared_version(&info.version)?,
                ),
                None => (String::new(), String::new()),
            };
            group.participants.push(OrdinaryParticipant {
                owner: owner.to_owned(),
                name,
                version,
                is_root: owner.is_empty(),
                tags: Vec::new(),
                imports: Vec::new(),
                source_profile_error: module.source_profile_error.clone(),
            });
            group.participants.len() - 1
        }
    };
    let participant = &mut group.participants[participant_index];
    for tag in usage
        .tags
        .iter()
        .filter(|tag| !tag.dev_dependency || !ignore_dev)
    {
        budget.string(&tag.name)?;
        for (name, value) in &tag.attributes {
            budget.string(name)?;
            budget.json(value, 0)?;
        }
    }
    participant.tags.extend(
        usage
            .tags
            .iter()
            .filter(|tag| !tag.dev_dependency || !ignore_dev)
            .cloned(),
    );
    let mapping = catalog
        .owner_mappings
        .get_mut(owner)
        .expect("owner inserted above");
    for import in &usage.imports {
        if import.dev_dependency && ignore_dev {
            continue;
        }
        // User-provided import names have stricter rules than canonical names:
        // '+' is forbidden, ensuring exact last-separator routing is unambiguous.
        validate_repo_name(&import.local_name).map_err(|e| e.to_string())?;
        validate_repo_name(&import.exported_name).map_err(|e| e.to_string())?;
        // A short import can expand to a long extension prefix. Reserve that
        // expansion before formatting or retaining each mapping entry.
        budget.string(&import.local_name)?;
        budget.string(&import.exported_name)?;
        budget.string(&group.prefix)?;
        let canonical = format!("{}+{}", group.prefix, import.exported_name);
        validate_repository_name(&canonical)?;
        if mapping.contains_key(&import.local_name) {
            return Err(format!(
                "ordinary repository import {:?} collides with an existing repository name in MODULE owner {owner:?}",
                import.local_name
            ));
        }
        mapping.insert(import.local_name.clone(), canonical);
        participant.imports.push(import.clone());
    }
    Ok(())
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
        for dependency in module.effective_dependencies() {
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

    #[test]
    fn repeated_long_prefix_imports_respect_catalog_host_budget() {
        let symbol = "e".repeat(65_536);
        let imports = (0..256).map(|i| format!(", 'r{i}'")).collect::<String>();
        let graph = graph(&format!(
            "module(name='main')\ne=use_extension('//:ext.bzl','{symbol}')\nuse_repo(e{imports})\n"
        ));
        let error = match build(&graph, false) {
            Err(error) => error,
            Ok(_) => panic!("catalog must enforce a cumulative host budget"),
        };
        assert!(error.contains("host") && error.contains("limit"), "{error}");
    }
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
        assert_eq!(
            catalog.owner_mappings[""]["ordinary_output"],
            "+maker+ordinary_output"
        );
        assert_eq!(
            catalog.repositories["+maker3+direct"].mapping["ordinary_output"],
            "+maker+ordinary_output"
        );
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
            "module(name='z')\nbazel_dep(name='deep')\nbazel_dep(name='main')\ne=use_extension('@main//:z.bzl','maker')",
        );
        dependency(
            &mut graph,
            "a",
            "a+",
            "module(name='a')\nbazel_dep(name='main')\ne=use_extension('@main//:a.bzl','maker')",
        );
        dependency(
            &mut graph,
            "deep",
            "deep+",
            "module(name='deep')\nbazel_dep(name='main')\ne=use_extension('@main//:deep.bzl','maker')",
        );
        graph.root_mapping.remove("deep");
        graph
            .modules
            .get_mut("z+")
            .unwrap()
            .mapping
            .insert("deep".to_owned(), "deep+".to_owned());
        // Each external participant needs a real apparent dependency on main
        // to reserve a root-owned prefix; @@// is not a canonical escape here.
        for owner in ["z+", "a+", "deep+"] {
            graph
                .modules
                .get_mut(owner)
                .unwrap()
                .mapping
                .insert("main".to_owned(), String::new());
        }
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
        let catalog = build(&graph, false).unwrap();
        assert!(catalog.repositories.contains_key("+maker+direct"));
        for (prefix, file) in [
            ("+maker2", "z.bzl"),
            ("+maker3", "a.bzl"),
            ("+maker4", "deep.bzl"),
        ] {
            assert_eq!(
                catalog.ordinary_groups[&catalog.ordinary_routes[prefix]]
                    .label
                    .name,
                file
            );
        }
    }

    #[test]
    fn module_bfs_includes_implicit_builtin_after_declared_deps() {
        let mut graph = graph("bazel_dep(name='z')\nbazel_dep(name='a')");
        for name in ["z", "a"] {
            dependency(
                &mut graph,
                name,
                &format!("{name}+"),
                &format!("module(name='{name}')"),
            );
        }
        dependency(
            &mut graph,
            "bazel_tools",
            "bazel_tools",
            "module(name='bazel_tools')",
        );
        let implicit = bzlmod::evaluate_source_with_builtins(
            "MODULE.bazel",
            String::new(),
            EvaluationContext::default(),
            bzlmod::BuiltinInjection::AppendBazelTools,
        )
        .unwrap()
        .implicit_dependencies;
        graph.root.implicit_dependencies = implicit.clone();
        for name in ["z+", "a+"] {
            let module = graph.modules.get_mut(name).unwrap();
            module.module.implicit_dependencies = implicit.clone();
            module
                .mapping
                .insert("bazel_tools".to_owned(), "bazel_tools".to_owned());
        }
        for ignore_dev in [false, true] {
            assert_eq!(
                owner_order(&graph, ignore_dev).unwrap(),
                ["", "z+", "a+", "bazel_tools"]
            );
            let catalog = build(&graph, ignore_dev).unwrap();
            for owner in ["", "z+", "a+", "bazel_tools"] {
                assert_eq!(catalog.owner_mappings[owner]["bazel_tools"], "bazel_tools");
            }
        }
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

    #[test]
    fn ordinary_alias_spellings_merge_participants_in_global_tag_order() {
        let graph = graph(
            r#"
module(name="root")
a=use_extension("//:ext.bzl", "ext")
b=use_extension("@root//:ext.bzl", "ext")
values=["before"]
a.first(values=values)
b.second(value=2)
a.third(value=3)
use_repo(a, first_alias="one")
use_repo(b, second_alias="two")
values.append("after")
"#,
        );
        let before = graph.clone();
        let catalog = build(&graph, false).unwrap();
        assert_eq!(graph, before);
        assert_eq!(catalog.ordinary_groups.len(), 1);
        let (id, group) = catalog.ordinary_groups.iter().next().unwrap();
        assert_eq!(
            serde_json::from_str::<(String, String)>(id).unwrap(),
            ("//:ext.bzl".to_owned(), "ext".to_owned())
        );
        assert_eq!(group.participants.len(), 1);
        let participant = &group.participants[0];
        assert_eq!(
            participant
                .tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(
            participant
                .tags
                .iter()
                .map(|tag| tag.ordinal)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            participant.tags[0].attributes["values"],
            json!(["before", "after"])
        );
        assert_eq!(
            participant
                .imports
                .iter()
                .map(|import| import.local_name.as_str())
                .collect::<Vec<_>>(),
            ["first_alias", "second_alias"]
        );
        assert_eq!(catalog.owner_mappings[""]["first_alias"], "+ext+one");
        assert_eq!(catalog.owner_mappings[""]["second_alias"], "+ext+two");
    }

    #[test]
    fn ordinary_module_label_double_at_requires_an_apparent_dependency_alias() {
        let mut graph = graph("bazel_dep(name='z')\ne=use_extension('@@z+//:ext.bzl','ext')");
        dependency(&mut graph, "z", "z+", "module(name='z')");
        assert!(
            build(&graph, false)
                .unwrap_err()
                .contains("unknown apparent repository @z+")
        );

        graph.root.extension_usages = evaluate(
            "e=use_extension('@@z//:ext.bzl','ext')\nr=use_repo_rule('@@z+//:rules.bzl','maker')\nr(name='direct')",
            ModuleRole::Root,
            false,
        )
        .extension_usages;
        let before = graph.clone();
        let catalog = build(&graph, false).unwrap();
        assert_eq!(graph, before);
        let ordinary = catalog.ordinary_groups.values().next().unwrap();
        assert_eq!(ordinary.label.repository, "z+");
        assert_eq!(ordinary.prefix, "z++ext");
        // MODULE use_extension normalization must not affect opaque direct
        // requests or the canonical-label grammar used by their runtime loader.
        let direct = catalog.groups.values().next().unwrap();
        let (label, symbol) = direct.definition_request().unwrap();
        assert_eq!((label, symbol), ("@@z+//:rules.bzl", "maker"));
        assert_eq!(
            Label::parse_in(label, "", "", &catalog.owner_mappings[""])
                .unwrap()
                .repository,
            "z+"
        );
    }

    #[test]
    fn ordinary_module_label_double_at_empty_repo_is_visible_only_from_root() {
        for module in ["", "module(name='root')\n"] {
            let graph = graph(&format!("{module}e=use_extension('@@//:ext.bzl','ext')"));
            let catalog = build(&graph, false).unwrap();
            assert_eq!(catalog.ordinary_groups.len(), 1);
            assert_eq!(
                catalog
                    .ordinary_groups
                    .values()
                    .next()
                    .unwrap()
                    .label
                    .repository,
                ""
            );
        }
        let mut graph = graph("bazel_dep(name='z')");
        dependency(
            &mut graph,
            "z",
            "z+",
            "module(name='z')\ne=use_extension('@@//:ext.bzl','ext')",
        );
        let error = build(&graph, false).unwrap_err();
        assert!(error.contains("unknown apparent repository @ from"));
        assert!(error.contains("z+"));
    }

    #[test]
    fn ordinary_module_label_self_spellings_merge_without_mutating_raw_metadata() {
        let mut graph = graph("bazel_dep(name='z')");
        dependency(
            &mut graph,
            "z",
            "z+",
            r#"
module(name="z", repo_name="self")
a=use_extension("//:ext.bzl", "ext")
b=use_extension("@@self//:ext.bzl", "ext")
c=use_extension("@self//:ext.bzl", "ext")
a.first()
b.second()
c.third()
a.fourth()
"#,
        );
        let before = graph.clone();
        let catalog = build(&graph, false).unwrap();
        assert_eq!(graph, before);
        assert_eq!(graph.modules["z+"].module.extension_usages.len(), 3);
        assert_eq!(
            graph.modules["z+"].module.extension_usages[1]
                .identity
                .label,
            "@@self//:ext.bzl"
        );
        assert_eq!(catalog.ordinary_groups.len(), 1);
        let group = catalog.ordinary_groups.values().next().unwrap();
        assert_eq!(group.label.repository, "z+");
        assert_eq!(group.participants.len(), 1);
        assert_eq!(
            group.participants[0]
                .tags
                .iter()
                .map(|tag| (tag.name.as_str(), tag.ordinal))
                .collect::<Vec<_>>(),
            [("first", 0), ("second", 1), ("third", 2), ("fourth", 3)]
        );
    }

    #[test]
    fn ordinary_participants_follow_bfs_include_zero_tags_and_preserve_profile_errors() {
        let mut graph = graph(
            r#"
module(name="root")
bazel_dep(name="z")
bazel_dep(name="a")
bazel_dep(name="nonparticipant")
e=use_extension("@z//:ext.bzl", "ext")
"#,
        );
        dependency(
            &mut graph,
            "z",
            "z+",
            "module(name='z')\ne=use_extension('//:ext.bzl','ext')\ne.tag(value='z')",
        );
        dependency(
            &mut graph,
            "a",
            "a+",
            "module(name='a')\nbazel_dep(name='z')\ne=use_extension('@@z//:ext.bzl','ext')",
        );
        dependency(
            &mut graph,
            "nonparticipant",
            "nonparticipant+",
            "module(name='nonparticipant')",
        );
        graph
            .modules
            .get_mut("a+")
            .unwrap()
            .mapping
            .insert("z".to_owned(), "z+".to_owned());
        graph.root.source_profile_error = Some("root deferred profile".to_owned());
        graph
            .modules
            .get_mut("a+")
            .unwrap()
            .module
            .source_profile_error = Some("a deferred profile".to_owned());
        let catalog = build(&graph, false).unwrap();
        let group = catalog.ordinary_groups.values().next().unwrap();
        assert_eq!(catalog.ordinary_groups.len(), 1);
        assert_eq!(
            group
                .participants
                .iter()
                .map(|p| p.owner.as_str())
                .collect::<Vec<_>>(),
            ["", "z+", "a+"]
        );
        assert_eq!(
            group
                .participants
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["root", "z", "a"]
        );
        assert!(group.participants[0].is_root);
        assert!(!group.participants[1].is_root);
        assert!(group.participants[0].tags.is_empty());
        assert!(group.participants[2].tags.is_empty());
        assert_eq!(group.participants[1].tags[0].attributes["value"], "z");
        assert_eq!(
            group.participants[0].source_profile_error.as_deref(),
            Some("root deferred profile")
        );
        assert_eq!(
            group.participants[2].source_profile_error.as_deref(),
            Some("a deferred profile")
        );
        assert!(group.root_has_non_dev_dependency);
    }

    #[test]
    fn ordinary_root_non_dev_flag_uses_declarations_not_tags_or_imports() {
        let dev = graph("e=use_extension('//:ext.bzl','ext',dev_dependency=True)");
        let catalog = build(&dev, false).unwrap();
        let group = catalog.ordinary_groups.values().next().unwrap();
        assert!(!group.root_has_non_dev_dependency);
        assert_eq!(group.participants.len(), 1);
        assert!(build(&dev, true).unwrap().ordinary_groups.is_empty());

        let mixed = graph(
            "module(name='root')\ne=use_extension('//:ext.bzl','ext',dev_dependency=True)\nf=use_extension('@root//:ext.bzl','ext')",
        );
        for ignore_dev in [false, true] {
            let catalog = build(&mixed, ignore_dev).unwrap();
            let group = catalog.ordinary_groups.values().next().unwrap();
            assert!(group.root_has_non_dev_dependency);
            assert_eq!(group.participants.len(), 1);
            assert!(group.participants[0].tags.is_empty());
            assert!(group.participants[0].imports.is_empty());
        }

        let mut dep_only = graph("bazel_dep(name='dep')");
        dependency(
            &mut dep_only,
            "dep",
            "dep+",
            "module(name='dep')\ne=use_extension('//:ext.bzl','ext')",
        );
        let catalog = build(&dep_only, false).unwrap();
        assert!(
            !catalog
                .ordinary_groups
                .values()
                .next()
                .unwrap()
                .root_has_non_dev_dependency
        );
    }

    #[test]
    fn ordinary_participant_versions_use_normalized_declarations_not_selected_override_versions() {
        let mut graph = graph(
            "module(name='root',version='1.2+root.build')\nbazel_dep(name='dep')\ne=use_extension('@dep//:ext.bzl','ext')",
        );
        dependency(
            &mut graph,
            "dep",
            "dep+",
            "module(name='dep',version='3.4+dep.build')\ne=use_extension('//:ext.bzl','ext')",
        );
        assert_eq!(graph.modules["dep+"].version, "");
        let catalog = build(&graph, false).unwrap();
        let group = catalog.ordinary_groups.values().next().unwrap();
        assert_eq!(group.participants[0].version, "1.2");
        assert_eq!(group.participants[1].version, "3.4");

        let anonymous = super::tests::graph("e=use_extension('//:ext.bzl','ext')");
        let catalog = build(&anonymous, false).unwrap();
        let participant = &catalog
            .ordinary_groups
            .values()
            .next()
            .unwrap()
            .participants[0];
        assert_eq!(participant.name, "");
        assert_eq!(participant.version, "");
        assert!(participant.is_root);
    }

    #[test]
    fn ordinary_imports_are_routes_not_generated_repositories_and_direct_repos_inherit_them() {
        let graph = graph(
            r#"
e=use_extension("//:ext.bzl", "ext")
r=use_repo_rule("//:rule.bzl", "rule")
r(name="direct")
use_repo(e, local="exported")
"#,
        );
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.repositories.len(), 1);
        assert_eq!(catalog.groups.len(), 1);
        assert_eq!(catalog.owner_mappings[""]["local"], "+ext+exported");
        assert_eq!(
            catalog.repositories["+rule+direct"].mapping["local"],
            "+ext+exported"
        );
        assert!(!catalog.repositories.contains_key("+ext+exported"));
        assert!(!catalog.repositories.contains_key("+ext+computed_later"));
        let (prefix, internal) = "+ext+computed_later".rsplit_once('+').unwrap();
        assert_eq!(internal, "computed_later");
        let group = &catalog.ordinary_groups[&catalog.ordinary_routes[prefix]];
        assert_eq!(group.prefix, "+ext");
        assert!(!catalog.ordinary_routes.contains_key("+ext_extra"));
        assert!(!catalog.ordinary_routes.contains_key("+ext+computed_later"));
        assert!(!catalog.ordinary_routes.contains_key("+rule"));
    }

    #[test]
    fn different_public_labels_never_merge_reexports_and_all_prefixes_share_one_allocator() {
        // Whether these public files re-export the same object is deliberately
        // unknowable here; identity is always the requested public label.
        let graph = graph(
            r#"
a=use_extension("//:one.bzl", "ext")
r=use_repo_rule("//:rule.bzl", "ext")
r(name="direct")
b=use_extension("//:two.bzl", "ext")
c=use_extension("//:other.bzl", "ext2")
use_repo(a, first="out")
use_repo(b, second="out")
"#,
        );
        let catalog = build(&graph, false).unwrap();
        assert_eq!(catalog.ordinary_groups.len(), 3);
        assert_eq!(catalog.ordinary_routes.len(), 3);
        assert!(catalog.ordinary_routes.contains_key("+ext"));
        assert!(catalog.ordinary_routes.contains_key("+ext3"));
        assert!(catalog.ordinary_routes.contains_key("+ext22"));
        assert!(catalog.repositories.contains_key("+ext2+direct"));
        assert_eq!(catalog.owner_mappings[""]["first"], "+ext+out");
        assert_eq!(catalog.owner_mappings[""]["second"], "+ext3+out");
        assert_ne!(
            catalog.ordinary_routes["+ext"],
            catalog.ordinary_routes["+ext3"]
        );
    }

    #[test]
    fn ordinary_dev_tags_and_imports_are_filtered_but_kept_tags_retain_dev_metadata() {
        let source = r#"
module(name="root")
d=use_extension("//:ext.bzl", "ext", dev_dependency=True)
n=use_extension("@root//:ext.bzl", "ext")
d.tag(value="dev")
n.tag(value="normal")
use_repo(d, dev_alias="dev_repo")
use_repo(n, normal_alias="normal_repo")
"#;
        let graph = graph(source);
        let catalog = build(&graph, false).unwrap();
        let tags = &catalog
            .ordinary_groups
            .values()
            .next()
            .unwrap()
            .participants[0]
            .tags;
        assert_eq!(tags.len(), 2);
        assert!(tags[0].dev_dependency);
        assert!(!tags[1].dev_dependency);
        let filtered = build(&graph, true).unwrap();
        let participant = &filtered
            .ordinary_groups
            .values()
            .next()
            .unwrap()
            .participants[0];
        assert_eq!(participant.tags.len(), 1);
        assert_eq!(participant.tags[0].attributes["value"], "normal");
        assert_eq!(participant.imports.len(), 1);
        assert!(!filtered.owner_mappings[""].contains_key("dev_alias"));

        let mut dep_graph = super::tests::graph("bazel_dep(name='dep')");
        dependency(
            &mut dep_graph,
            "dep",
            "dep+",
            &source.replace("root", "dep"),
        );
        let catalog = build(&dep_graph, false).unwrap();
        let group = catalog.ordinary_groups.values().next().unwrap();
        assert_eq!(group.participants.len(), 1);
        assert_eq!(group.participants[0].tags.len(), 1);
        assert_eq!(group.participants[0].imports.len(), 1);
        assert!(!catalog.owner_mappings["dep+"].contains_key("dev_alias"));
        assert!(!group.root_has_non_dev_dependency);
    }

    #[test]
    fn ordinary_import_names_are_validated_and_cannot_overwrite_owner_aliases() {
        let original = graph(
            "module(name='root')\ne=use_extension('//:ext.bzl','ext')\nuse_repo(e,alias='exported')",
        );
        for field in ["local", "exported"] {
            for invalid in ["", "contains+separator", "bad/name"] {
                let mut graph = original.clone();
                let import = &mut graph.root.extension_usages[0].imports[0];
                if field == "local" {
                    import.local_name = invalid.to_owned();
                } else {
                    import.exported_name = invalid.to_owned();
                }
                assert!(
                    build(&graph, false).is_err(),
                    "accepted {field} {invalid:?}"
                );
            }
        }
        let mut collision = original.clone();
        collision.root.extension_usages[0].imports[0].local_name = "root".to_owned();
        assert!(build(&collision, false).unwrap_err().contains("collides"));
        assert_eq!(collision.root_mapping["root"], "");
        let mut duplicate = original.clone();
        let import = duplicate.root.extension_usages[0].imports[0].clone();
        duplicate.root.extension_usages[0].imports.push(import);
        assert!(build(&duplicate, false).unwrap_err().contains("collides"));
    }

    #[test]
    fn isolated_imports_do_not_fabricate_ordinary_identities_or_routes() {
        let source = r#"
isolated=use_extension("//:ext.bzl", "ext", isolate=True)
normal=use_extension("//:ext.bzl", "ext")
use_repo(isolated, "hidden")
use_repo(normal, "visible")
r=use_repo_rule("//:rule.bzl", "ext")
r(name="direct")
"#;
        let catalog = build(&graph(source), false).unwrap();
        assert_eq!(catalog.ordinary_groups.len(), 1);
        assert_eq!(catalog.ordinary_routes.len(), 1);
        assert!(!catalog.owner_mappings[""].contains_key("hidden"));
        assert_eq!(catalog.owner_mappings[""]["visible"], "+ext+visible");
        assert!(catalog.repositories.contains_key("+ext2+direct"));
        let private = source.replace(
            "normal=use_extension(\"//:ext.bzl\", \"ext\")",
            "normal=use_extension(\"//:ext.bzl\", \"_private\")",
        );
        assert!(
            build(&graph(&private), false)
                .unwrap_err()
                .contains("isolated extension namespace not supported")
        );
    }

    #[test]
    fn ordinary_definitions_use_only_base_mappings_and_safe_canonical_prefixes() {
        let generated_definition = graph(
            "r=use_repo_rule('//:rule.bzl','rule')\nr(name='generated')\ne=use_extension('@generated//:ext.bzl','ext')",
        );
        assert!(
            build(&generated_definition, false)
                .unwrap_err()
                .contains("invalid ordinary extension label")
        );
        let mut invalid = graph("e=use_extension('//:ext.bzl','ext')");
        invalid.root.extension_usages[0].identity.extension_name = "bad symbol".to_owned();
        assert!(
            build(&invalid, false)
                .unwrap_err()
                .contains("ordinary extension canonical namespace")
        );
        let private = graph("e=use_extension('//:missing.bzl','_private')");
        // Export visibility is a runtime preparation check unless it makes the
        // isolated namespace omission unsafe. No definition file is opened here.
        assert_eq!(build(&private, false).unwrap().ordinary_groups.len(), 1);
    }
}
