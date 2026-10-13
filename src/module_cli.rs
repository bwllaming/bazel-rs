//! Native `mod graph --output=json` for explicit modules.
//!
//! This implements Bazel 9.3's hidden-extension, non-verbose, selected-module
//! JSON tree profile, not full `bazel mod` parity. Implicit `bazel_tools` and
//! other built-ins are not injected. Extensions cannot be executed, so active
//! extension usages are errors even with `--extension_info=hidden`. Remote
//! registries and HTTPS Git require explicit --lockfile_mode=off. Git sources are
//! materialized for discovery; registry sources and lockfiles are not read/written.
use crate::bzlmod::ModuleFile;
use crate::module_graph::{self, ResolvedGraph, ResolverOptions};
use crate::registry::Registry;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

/// Run a native module subcommand. `args` begins with `graph`, not `mod`.
/// Only explicit registries and the documented JSON profile work.
pub fn run(root: &Path, args: &[String]) -> Result<String, String> {
    run_with_source_cache(root, args, Path::new("bazel-rs-out/repositories"))
}

/// Run with a caller-selected repository source cache.
pub fn run_with_source_cache(root: &Path, args: &[String], cache: &Path) -> Result<String, String> {
    let options = Options::parse(args)?;
    let graph = module_graph::resolve_with_source_cache(root, &options.resolver, cache)?;
    reject_extensions(
        &graph.root,
        options.resolver.ignore_dev_dependencies,
        "<root>",
    )?;
    for module in graph.modules.values() {
        reject_extensions(&module.module, true, &module.name)?;
    }
    if options.resolver.builtin_profile == module_graph::BuiltinProfile::Bazel93Builtins {
        return Err("builtin module graph presentation is unsupported; the genuine builtin profile currently supports query/build repository access only".to_owned());
    }
    let output = format_graph(
        &graph,
        options.resolver.ignore_dev_dependencies,
        options.cycles,
    )?;
    serde_json::to_string_pretty(&output)
        .map(|s| s + "\n")
        .map_err(|e| format!("cannot format module graph: {e}"))
}

#[derive(Debug)]
struct Options {
    resolver: ResolverOptions,
    cycles: bool,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        if args.first().map(String::as_str) != Some("graph") {
            return Err("native mod supports only 'graph --output=json'".to_owned());
        }
        let mut options = Self {
            resolver: ResolverOptions::default(),
            cycles: true,
        };
        let mut output_json = false;
        let mut args = args[1..].iter();
        while let Some(arg) = args.next() {
            if let Some(profile) = builtin_profile_argument(arg)? {
                options.resolver.builtin_profile = profile;
                continue;
            }
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
            match flag {
                "--registry" | "--output" | "--extension_info" | "--lockfile_mode" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .ok_or_else(|| format!("{flag} requires a value"))?;
                    match (flag, value) {
                        ("--registry", value) => {
                            options.resolver.registries.push(registry_path(value)?)
                        }
                        ("--output", "json") => output_json = true,
                        ("--extension_info", "hidden") => {}
                        ("--lockfile_mode", "off") => options.resolver.allow_remote = true,
                        _ => {
                            return Err(format!(
                                "unsupported native mod graph option {flag}={value}; supported profile: --output=json --extension_info=hidden --lockfile_mode=off"
                            ));
                        }
                    }
                }
                "--ignore_dev_dependency" | "--noignore_dev_dependency" => {
                    options.resolver.ignore_dev_dependencies = boolean(flag, inline)?;
                }
                "--cycles" | "--nocycles" => options.cycles = boolean(flag, inline)?,
                "--include_builtin"
                | "--noinclude_builtin"
                | "--include_unused"
                | "--noinclude_unused"
                | "--verbose"
                | "--noverbose" => {
                    if boolean(flag, inline)? {
                        return Err(format!(
                            "{flag}=true is unsupported by native mod graph; built-ins, unused modules, and verbose explanations are not implemented"
                        ));
                    }
                }
                _ => return Err(format!("unknown native mod graph argument: {arg}")),
            }
        }
        if !output_json {
            return Err("native mod graph requires --output=json; the default text formatter is not implemented".to_owned());
        }
        options.resolver.validate_registries()?;
        Ok(options)
    }
}

fn boolean(flag: &str, value: Option<&str>) -> Result<bool, String> {
    let value = match value {
        None | Some("true") => true,
        Some("false") => false,
        Some(other) => return Err(format!("{flag} expects true or false, got {other:?}")),
    };
    Ok(value != flag.starts_with("--no"))
}

pub fn registry_path(uri: &str) -> Result<Registry, String> {
    Registry::parse(uri)
}

/// Native-only transition switch, independent of graph display/registry policy.
pub fn builtin_profile_argument(arg: &str) -> Result<Option<module_graph::BuiltinProfile>, String> {
    use module_graph::BuiltinProfile;
    match arg {
        "--experimental-builtin-modules" | "--experimental-builtin-modules=true" => {
            Ok(Some(BuiltinProfile::Bazel93Builtins))
        }
        "--experimental-builtin-modules=false" => Ok(Some(BuiltinProfile::ExplicitModules)),
        _ if arg.starts_with("--experimental-builtin-modules=") => {
            Err("--experimental-builtin-modules expects true or false".to_owned())
        }
        _ => Ok(None),
    }
}

fn reject_extensions(module: &ModuleFile, ignore_dev: bool, name: &str) -> Result<(), String> {
    for usage in &module.extension_usages {
        if usage
            .declarations
            .iter()
            .any(|d| !d.dev_dependency || !ignore_dev)
        {
            if usage.identity.is_repo_rule() {
                return Err(format!(
                    "module {name:?} declares repositories with use_repo_rule {:?}: native mod graph cannot execute repository rules",
                    usage.identity.extension_name
                ));
            }
            return Err(format!(
                "module {name:?} uses extension {:?}%{}: native mod graph cannot execute module extensions, even with --extension_info=hidden",
                usage.identity.label, usage.identity.extension_name
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Edge {
    target: String,
    alias: String,
    expanded: bool,
}

fn module_data<'a>(
    graph: &'a ResolvedGraph,
    canonical: &str,
) -> (&'a ModuleFile, &'a BTreeMap<String, String>) {
    if canonical.is_empty() {
        (&graph.root, &graph.root_mapping)
    } else {
        let node = &graph.modules[canonical];
        (&node.module, &node.mapping)
    }
}

fn direct_edges(
    graph: &ResolvedGraph,
    canonical: &str,
    ignore_dev: bool,
) -> Result<Vec<Edge>, String> {
    let (module, mapping) = module_data(graph, canonical);
    let mut edges = Vec::new();
    for dependency in module.effective_dependencies() {
        if dependency.dev_dependency && (!canonical.is_empty() || ignore_dev) {
            continue;
        }
        let Some(alias) = dependency.effective_repo_name() else {
            continue;
        };
        let target = mapping.get(alias).ok_or_else(|| {
            format!("missing resolved repository mapping for {alias:?} in {canonical:?}")
        })?;
        if !target.is_empty() && !graph.modules.contains_key(target) {
            return Err(format!("missing resolved module for repository {target:?}"));
        }
        edges.push(Edge {
            target: target.clone(),
            alias: alias.to_owned(),
            expanded: false,
        });
    }
    edges.sort_by(|a, b| compare_keys(graph, &a.target, &b.target));
    Ok(edges)
}

fn compare_keys(graph: &ResolvedGraph, a: &str, b: &str) -> Ordering {
    match (graph.modules.get(a), graph.modules.get(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => a
            .name
            .cmp(&b.name)
            .then_with(|| compare_versions(&a.version, &b.version)),
    }
}

// Resolved versions are validated and normalized by module_graph. Compare
// numeric identifiers numerically (including spelling ties), not lexically.
fn compare_versions(a: &str, b: &str) -> Ordering {
    fn identifiers(a: &str, b: &str) -> Ordering {
        let key = |s: &str| match s.parse::<u64>() {
            Ok(n) => (false, n, s.to_owned()),
            Err(_) => (true, 0, s.to_owned()),
        };
        a.split('.').map(key).cmp(b.split('.').map(key))
    }
    let (ar, ap) = a.split_once('-').unwrap_or((a, ""));
    let (br, bp) = b.split_once('-').unwrap_or((b, ""));
    a.is_empty()
        .cmp(&b.is_empty())
        .then_with(|| identifiers(ar, br))
        .then_with(|| ap.is_empty().cmp(&bp.is_empty()))
        .then_with(|| identifiers(ap, bp))
}

fn format_graph(graph: &ResolvedGraph, ignore_dev: bool, cycles: bool) -> Result<Value, String> {
    // Expansion is a per-edge BFS snapshot. In particular all root dependencies
    // are marked visited before descending, unlike a recursive first-seen walk.
    let mut visited = BTreeSet::from([String::new()]);
    let mut queue = VecDeque::from([String::new()]);
    let mut nodes = BTreeMap::new();
    while let Some(canonical) = queue.pop_front() {
        let mut edges = direct_edges(graph, &canonical, ignore_dev)?;
        for edge in &mut edges {
            edge.expanded = visited.insert(edge.target.clone());
            if edge.expanded {
                queue.push_back(edge.target.clone());
            }
        }
        edges.sort_by(|a, b| {
            a.expanded
                .cmp(&b.expanded)
                .then_with(|| compare_keys(graph, &a.target, &b.target))
        });
        nodes.insert(canonical, edges);
    }
    let root_alias = graph
        .root
        .module
        .as_ref()
        .map_or("", |m| m.effective_repo_name());
    let mut output = print_node(
        graph,
        &nodes,
        "",
        root_alias,
        true,
        cycles,
        &mut BTreeSet::new(),
    );
    output["root"] = json!(true);
    Ok(output)
}

fn print_node(
    graph: &ResolvedGraph,
    nodes: &BTreeMap<String, Vec<Edge>>,
    canonical: &str,
    alias: &str,
    expanded: bool,
    include_cycles: bool,
    ancestors: &mut BTreeSet<String>,
) -> Value {
    let (module, _) = module_data(graph, canonical);
    let (name, version) = module
        .module
        .as_ref()
        .map_or(("", ""), |m| (m.name.as_str(), m.version.as_str()));
    let key = if canonical.is_empty() {
        "<root>".to_owned()
    } else {
        let node = &graph.modules[canonical];
        format!(
            "{}@{}",
            node.name,
            if node.version.is_empty() {
                "_"
            } else {
                &node.version
            }
        )
    };
    let mut output = json!({"key": key, "name": name, "version": version.split('+').next().unwrap_or(""), "apparentName": alias});
    if !expanded {
        output["unexpanded"] = json!(true);
        return output;
    }
    ancestors.insert(canonical.to_owned());
    let mut dependencies = Vec::new();
    let mut cycles = Vec::new();
    for edge in &nodes[canonical] {
        if ancestors.contains(&edge.target) {
            if include_cycles {
                cycles.push(print_node(
                    graph,
                    nodes,
                    &edge.target,
                    &edge.alias,
                    false,
                    include_cycles,
                    ancestors,
                ));
            }
        } else {
            dependencies.push(print_node(
                graph,
                nodes,
                &edge.target,
                &edge.alias,
                edge.expanded,
                include_cycles,
                ancestors,
            ));
        }
    }
    ancestors.remove(canonical);
    output["dependencies"] = json!(dependencies);
    output["indirectDependencies"] = json!([]);
    output["cycles"] = json!(cycles);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzlmod::{Dependency, DependencyRepoName, ModuleInfo};
    use crate::module_graph::{ModuleSource, ResolvedModule};
    use std::path::PathBuf;

    fn graph(edges: &[(&str, &[&str])]) -> ResolvedGraph {
        let mut graph = ResolvedGraph::default();
        graph.root.module = Some(ModuleInfo {
            name: "root".into(),
            version: "1.0".into(),
            repo_name: Some("root_repo".into()),
            ..Default::default()
        });
        for (name, _) in edges {
            if name.is_empty() {
                continue;
            }
            graph.modules.insert(
                (*name).into(),
                ResolvedModule {
                    name: (*name).into(),
                    version: "1.0".into(),
                    compatibility_level: 0,
                    canonical_name: (*name).into(),
                    mapping: BTreeMap::new(),
                    source: ModuleSource::LocalPath(PathBuf::from("/unused")),
                    module: ModuleFile {
                        module: Some(ModuleInfo {
                            name: (*name).into(),
                            version: "1.0".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            );
        }
        for (name, children) in edges {
            let (module, mapping) = if name.is_empty() {
                (&mut graph.root, &mut graph.root_mapping)
            } else {
                let node = graph.modules.get_mut(*name).unwrap();
                (&mut node.module, &mut node.mapping)
            };
            mapping.insert("self_only".into(), (*name).into());
            for child in *children {
                let alias = if child.is_empty() { "root" } else { child };
                module.dependencies.push(Dependency {
                    name: alias.into(),
                    version: "1.0".into(),
                    ..Default::default()
                });
                mapping.insert(alias.into(), (*child).into());
            }
        }
        graph
    }
    fn leaf(name: &str, expanded: bool) -> Value {
        let mut value = json!({"key": format!("{name}@1.0"), "name":name, "version":"1.0", "apparentName":name});
        if expanded {
            value["dependencies"] = json!([]);
            value["indirectDependencies"] = json!([]);
            value["cycles"] = json!([]);
        } else {
            value["unexpanded"] = json!(true);
        }
        value
    }
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn root_and_chain_have_exact_json_fields() {
        let graph = graph(&[("", &["a"]), ("a", &["b"]), ("b", &[])]);
        let mut a = leaf("a", true);
        a["dependencies"] = json!([leaf("b", true)]);
        assert_eq!(
            format_graph(&graph, false, true).unwrap(),
            json!({
                "key":"<root>", "name":"root", "version":"1.0", "apparentName":"root_repo", "root":true,
                "dependencies":[a], "indirectDependencies":[], "cycles":[]
            })
        );
    }

    #[test]
    fn shared_root_dependency_is_unexpanded_under_child() {
        let graph = graph(&[("", &["b", "a"]), ("a", &["b"]), ("b", &[])]);
        let output = format_graph(&graph, false, true).unwrap();
        assert_eq!(
            output["dependencies"][0]["dependencies"],
            json!([leaf("b", false)])
        );
        assert_eq!(output["dependencies"][1], leaf("b", true));
    }

    #[test]
    fn diamond_expansion_uses_bfs_and_unexpanded_children_sort_first() {
        let graph = graph(&[
            ("", &["b", "a"]),
            ("a", &["c"]),
            ("b", &["c", "aa"]),
            ("c", &[]),
            ("aa", &[]),
        ]);
        let output = format_graph(&graph, false, true).unwrap();
        assert_eq!(
            output["dependencies"][0]["dependencies"],
            json!([leaf("c", true)])
        );
        assert_eq!(
            output["dependencies"][1]["dependencies"],
            json!([leaf("c", false), leaf("aa", true)])
        );
    }

    #[test]
    fn ancestors_are_cycles_but_cross_edges_are_dependencies() {
        let graph = graph(&[("", &["a"]), ("a", &["b"]), ("b", &["a", ""])]);
        let output = format_graph(&graph, false, true).unwrap();
        let b = &output["dependencies"][0]["dependencies"][0];
        assert_eq!(b["dependencies"], json!([]));
        assert_eq!(
            b["cycles"][0],
            json!({"key":"<root>","name":"root","version":"1.0","apparentName":"root","unexpanded":true})
        );
        assert_eq!(b["cycles"][1], leaf("a", false));
        let hidden = format_graph(&graph, false, false).unwrap();
        assert_eq!(
            hidden["dependencies"][0]["dependencies"][0]["cycles"],
            json!([])
        );
    }

    #[test]
    fn local_override_key_keeps_declared_version_and_parent_alias() {
        let mut graph = graph(&[("", &["a"]), ("a", &[])]);
        let node = graph.modules.get_mut("a").unwrap();
        node.version.clear();
        node.module.module.as_mut().unwrap().version = "2.0+metadata".into();
        graph.root.dependencies[0].repo_name = DependencyRepoName::Explicit("alias".into());
        graph.root_mapping.insert("alias".into(), "a".into());
        let output = format_graph(&graph, false, true).unwrap();
        assert_eq!(
            output["dependencies"][0],
            json!({"key":"a@_", "name":"a", "version":"2.0", "apparentName":"alias", "dependencies":[], "indirectDependencies":[], "cycles":[]})
        );
    }

    #[test]
    fn direct_edges_filter_nodeps_and_nonroot_dev_dependencies() {
        let mut graph = graph(&[("", &["a", "b"]), ("a", &["b"]), ("b", &[])]);
        graph.root.dependencies[1].dev_dependency = true;
        graph.modules.get_mut("a").unwrap().module.dependencies[0].dev_dependency = true;
        graph.root.dependencies.push(Dependency {
            name: "nodep".into(),
            repo_name: DependencyRepoName::Nodep,
            ..Default::default()
        });
        let output = format_graph(&graph, true, true).unwrap();
        assert_eq!(output["dependencies"], json!([leaf("a", true)]));
    }

    #[test]
    fn versions_sort_numerically_and_empty_last() {
        for (a, b) in [
            ("1.2", "1.10"),
            ("1.01", "1.1"),
            ("1.0-2", "1.0-10"),
            ("1.0-alpha", "1.0"),
            ("1.0", ""),
        ] {
            assert_eq!(compare_versions(a, b), Ordering::Less);
        }
    }

    #[test]
    fn builtin_profile_is_explicit_and_separate_from_graph_display() {
        use module_graph::BuiltinProfile;
        let selected = Options::parse(&args(&[
            "graph",
            "--output=json",
            "--experimental-builtin-modules",
        ]))
        .unwrap();
        assert_eq!(
            selected.resolver.builtin_profile,
            BuiltinProfile::Bazel93Builtins
        );
        let reset = Options::parse(&args(&[
            "graph",
            "--output=json",
            "--experimental-builtin-modules=true",
            "--experimental-builtin-modules=false",
        ]))
        .unwrap();
        assert_eq!(
            reset.resolver.builtin_profile,
            BuiltinProfile::ExplicitModules
        );
        assert!(builtin_profile_argument("--experimental-builtin-modules=maybe").is_err());
        assert_eq!(builtin_profile_argument("--include_builtin"), Ok(None));
        assert!(
            Options::parse(&args(&[
                "graph",
                "--output=json",
                "--experimental-builtin-modules",
                "--include_builtin"
            ]))
            .is_err()
        );
    }

    #[test]
    fn strict_options_and_registry_order() {
        let options = Options::parse(&args(&[
            "graph",
            "--output=json",
            "--registry=file:///first",
            "--registry",
            "file:///second%20space",
            "--ignore_dev_dependency=true",
            "--noignore_dev_dependency=false",
            "--extension_info=hidden",
            "--include_builtin=false",
            "--noinclude_unused",
            "--verbose=false",
            "--cycles=true",
            "--lockfile_mode=off",
        ]))
        .unwrap();
        assert_eq!(
            options.resolver.registries,
            vec![
                PathBuf::from("/first").into(),
                PathBuf::from("/second space").into()
            ]
        );
        assert!(options.resolver.ignore_dev_dependencies);
        assert!(options.cycles);
        for invalid in [
            "--output=text",
            "--registry=https://example.invalid",
            "--registry=/local",
            "--registry=file://host/local",
            "--registry=file:///bad%",
            "--registry=file:///bad%00",
            "--extension_info=all",
            "--lockfile_mode=update",
            "--include_builtin",
            "--include_unused=true",
            "--verbose",
            "--ignore_dev_dependency=maybe",
            "--unknown",
            "a@1",
            "--depth=1",
        ] {
            assert!(
                Options::parse(&args(&["graph", "--output=json", invalid])).is_err(),
                "{invalid}"
            );
        }
        assert!(
            Options::parse(&args(&["graph"]))
                .unwrap_err()
                .contains("text formatter")
        );
        assert!(Options::parse(&args(&["dump_repo_mapping", "--output=json"])).is_err());
        assert!(
            !Options::parse(&args(&[
                "graph",
                "--output=json",
                "--ignore_dev_dependency",
                "--noignore_dev_dependency"
            ]))
            .unwrap()
            .resolver
            .ignore_dev_dependencies
        );
    }
}
