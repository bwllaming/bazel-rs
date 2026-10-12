//! Lazy repository roots and strict apparent-name resolution.
//!
//! Main-repository source operations never evaluate MODULE.bazel. External
//! resolution records the graph once and does not execute extensions or mistake
//! registry metadata for a materialized source tree. Local source roots remain
//! live inputs; archive sources are materialized only when requested.
use crate::label::{Label, PackageId, validate_repository_name};
use crate::module_graph::{self, ModuleSource, ResolvedGraph, ResolverOptions};
use crate::registry::Registry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug)]
pub struct RepositorySet {
    root: PathBuf,
    options: ResolverOptions,
    source_cache: PathBuf,
    resolved: OnceLock<Result<ResolvedGraph, String>>,
    registry_roots: OnceLock<BTreeMap<String, OnceLock<Result<PathBuf, String>>>>,
}

impl RepositorySet {
    pub fn new(root: PathBuf) -> Self {
        Self::with_options(root, ResolverOptions::default())
    }

    pub fn with_options(root: PathBuf, options: ResolverOptions) -> Self {
        Self {
            source_cache: root.join("bazel-rs-out/repositories"),
            root,
            options,
            resolved: OnceLock::new(),
            registry_roots: OnceLock::new(),
        }
    }

    pub fn with_source_cache(mut self, cache: PathBuf) -> Self {
        self.source_cache = cache;
        self
    }

    pub fn main_root(&self) -> &Path {
        &self.root
    }

    pub fn resolve_graph(&self) -> Result<&ResolvedGraph, String> {
        self.resolved
            .get_or_init(|| module_graph::resolve(&self.root, &self.options))
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn root(&self, repository: &str) -> Result<&Path, String> {
        validate_repository_name(repository)?;
        if repository.is_empty() {
            return Ok(&self.root);
        }
        let graph = self.resolve_graph()?;
        let module = graph.modules.get(repository).ok_or_else(|| {
            format!("unknown canonical repository @@{repository}; extension repository not materialized or module not selected")
        })?;
        match &module.source {
            ModuleSource::LocalPath(root) => Ok(root),
            ModuleSource::Registry {
                registry,
                name,
                version,
                module_file,
            } => {
                let roots = self.registry_roots.get_or_init(|| {
                    graph
                        .modules
                        .keys()
                        .map(|name| (name.clone(), OnceLock::new()))
                        .collect()
                });
                roots[repository]
                    .get_or_init(|| {
                        registry_source_root(
                            registry,
                            name,
                            version,
                            module_file,
                            &self.source_cache,
                        )
                        .map_err(|error| {
                            format!("repository @@{repository} is not materialized: {error}")
                        })
                    })
                    .as_ref()
                    .map(PathBuf::as_path)
                    .map_err(Clone::clone)
            }
        }
    }

    /// Return the source repository root, not its package subdirectory.
    pub fn package_root(&self, package: &PackageId) -> Result<&Path, String> {
        self.root(&package.repository)
    }

    pub fn mapping(&self, repository: &str) -> Result<&BTreeMap<String, String>, String> {
        validate_repository_name(repository)?;
        let graph = self.resolve_graph()?;
        if repository.is_empty() {
            return Ok(&graph.root_mapping);
        }
        graph.modules.get(repository).map(|module| &module.mapping).ok_or_else(|| {
            format!("unknown canonical repository @@{repository}; extension repository not materialized or module not selected")
        })
    }

    pub fn parse(&self, text: &str, repository: &str, package: &str) -> Result<Label, String> {
        // Apparent aliases require the owning module's mapping. Only the main
        // repository has the empty alias: external @// is NOT self or main.
        let main_empty_alias = repository.is_empty() && text.starts_with("@//");
        if text.starts_with('@') && !text.starts_with("@@") && !main_empty_alias {
            Label::parse_in(text, repository, package, self.mapping(repository)?)
        } else {
            let mapping = if main_empty_alias {
                BTreeMap::from([(String::new(), String::new())])
            } else {
                BTreeMap::new()
            };
            Label::parse_in(text, repository, package, &mapping)
        }
    }
}

fn registry_source_root(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    cache: &Path,
) -> Result<PathBuf, String> {
    let relative = format!("modules/{name}/{version}/source.json");
    let descriptor = registry.location(&relative);
    let source = registry
        .read_object(&relative)?
        .ok_or_else(|| format!("missing registry source descriptor {descriptor}"))?;
    match source.get("type") {
        Some(serde_json::Value::String(kind)) if kind == "local_path" => {}
        None => {
            return crate::materializer::materialize(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
            );
        }
        Some(serde_json::Value::String(kind)) if kind == "archive" => {
            return crate::materializer::materialize(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
            );
        }
        Some(kind) => return Err(format!("unsupported source type {kind} in {descriptor}")),
    }
    for key in source.keys() {
        if !matches!(key.as_str(), "type" | "path") {
            return Err(format!(
                "unsupported local_path source field {key:?} in {descriptor}; patches and other source transformations are not implemented"
            ));
        }
    }
    let path = source
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!("local_path source requires a nonempty string path in {descriptor}")
        })?;
    if path.is_empty() || path.contains('\0') {
        return Err(format!(
            "invalid local_path path in {descriptor}: must be nonempty and contain no NUL"
        ));
    }
    let path = PathBuf::from(path);
    let chosen = if path.is_absolute() {
        path
    } else {
        let config_path = registry.location("bazel_registry.json");
        let config = registry.read_object("bazel_registry.json").map_err(|error| {
            format!("relative local_path source requires registry module_base_path: {error}")
        })?.ok_or_else(|| format!("relative local_path source requires registry module_base_path: missing {config_path}"))?;
        let base = config
            .get("module_base_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "relative local_path source requires string module_base_path in {config_path}"
                )
            })?;
        if base.contains('\0') {
            return Err(format!(
                "invalid module_base_path in {config_path}: NUL is not allowed"
            ));
        }
        // IndexRegistry.getLocalPathRepoSpec resolves a relative base against
        // the selected registry, not the workspace or source descriptor folder.
        let base = PathBuf::from(base);
        if base.is_absolute() {
            base.join(path)
        } else if let Some(registry) = registry.local_path() {
            registry.join(base).join(path)
        } else {
            return Err(format!(
                "relative module_base_path is unsupported for remote registry {registry}; local_path sources require an absolute path or absolute module_base_path"
            ));
        }
    };
    let root = std::fs::canonicalize(&chosen).map_err(|error| {
        format!(
            "cannot resolve registry local_path source {}: {error}",
            chosen.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "registry local_path source is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "bazel-rs-repository-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn registry(&self) -> RepositorySet {
            self.write(
                "main/MODULE.bazel",
                "module(name='main')\nbazel_dep(name='dep',version='1')\n",
            );
            self.write(
                "registry/modules/dep/1/MODULE.bazel",
                "module(name='dep',version='1')\n",
            );
            RepositorySet::with_options(
                self.0.join("main"),
                ResolverOptions {
                    registries: vec![self.0.join("registry").into()],
                    ..ResolverOptions::default()
                },
            )
        }

        fn source(&self, value: serde_json::Value) {
            self.write("registry/modules/dep/1/source.json", &value.to_string());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn main_and_canonical_parsing_are_lazy_even_with_broken_module() {
        let f = Fixture::new();
        for module in [None, Some("not valid MODULE syntax !!!")] {
            if let Some(module) = module {
                f.write("MODULE.bazel", module);
            }
            let repos = RepositorySet::new(f.0.clone());
            assert_eq!(repos.root("").unwrap(), f.0);
            for input in ["//p:x", ":x", "@//p:x", "@@//p:x", "@@dep+//p:x"] {
                assert!(repos.parse(input, "", "p").is_ok());
            }
            assert_eq!(repos.parse("//p:x", "dep+", "").unwrap().repository, "dep+");
            assert!(repos.resolved.get().is_none());
            assert!(repos.parse("@//p:x", "dep+", "").is_err());
            assert!(repos.parse("@dep//p:x", "", "").is_err());
            assert!(repos.resolved.get().is_some());
        }
    }

    #[test]
    fn local_overrides_have_strict_per_repository_aliases() {
        let f = Fixture::new();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='../dep')\n");
        f.write(
            "dep/MODULE.bazel",
            "module(name='dep',repo_name='self_dep')\n",
        );
        let repos = RepositorySet::new(f.0.join("main"));
        let label = repos.parse("@alias//same:x", "", "").unwrap();
        assert_eq!(label.repository, "dep+");
        assert_eq!(
            repos.root(&label.repository).unwrap(),
            std::fs::canonicalize(f.0.join("dep")).unwrap()
        );
        assert_eq!(repos.parse("@self_dep//same:x", "dep+", "").unwrap(), label);
        assert!(
            repos
                .parse("@//same:x", "dep+", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert_eq!(
            repos.parse("@@//same:x", "dep+", "").unwrap().repository,
            ""
        );
        assert_eq!(repos.parse("//same:x", "dep+", "").unwrap(), label);
        assert!(repos.parse("@alias//same:x", "dep+", "").is_err());
        assert!(repos.parse("@dep+//same:x", "", "").is_err());
        assert_eq!(repos.parse("@@dep+//same:x", "", "").unwrap(), label);
        assert_ne!(
            repos.parse("//same:x", "", "").unwrap().package_id(),
            label.package_id()
        );
        let visibility = crate::visibility::Visibility::parse_in(
            &["@alias//same:__pkg__".into()],
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &repos,
        )
        .unwrap();
        assert!(visibility.allows_in(
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &label.package_id()
        ));
        assert!(!visibility.allows_in(
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &Label::parse("//same:x", "").unwrap().package_id()
        ));
        assert!(
            repos
                .root("unselected+")
                .unwrap_err()
                .contains("not materialized")
        );
    }

    #[test]
    fn graph_failure_is_cached_without_fallback() {
        let f = Fixture::new();
        let repos = RepositorySet::new(f.0.clone());
        let error = repos.resolve_graph().unwrap_err();
        f.write("MODULE.bazel", "module(name='main')\n");
        assert_eq!(repos.resolve_graph().unwrap_err(), error);
        assert!(repos.root("").is_ok());
    }

    #[test]
    fn registry_metadata_is_never_a_repository_root() {
        let f = Fixture::new();
        f.write(
            "main/MODULE.bazel",
            "module(name='main')\nbazel_dep(name='dep',version='1')\n",
        );
        f.write(
            "registry/modules/dep/1/MODULE.bazel",
            "module(name='dep',version='1')\n",
        );
        let repos = RepositorySet::with_options(
            f.0.join("main"),
            ResolverOptions {
                registries: vec![f.0.join("registry").into()],
                ..ResolverOptions::default()
            },
        );
        let label = repos.parse("@dep//p:x", "", "").unwrap();
        assert!(
            repos
                .root(&label.repository)
                .unwrap_err()
                .contains("not materialized")
        );
    }

    #[test]
    fn registry_absolute_local_path_is_live_and_registry_module_is_authoritative() {
        let f = Fixture::new();
        let repos = f.registry();
        // Outside the main workspace, and no valid source MODULE is required.
        f.write(
            "live/MODULE.bazel",
            "this source MODULE must never be evaluated !!!",
        );
        f.write("live/BUILD", "exports_files(['data'])");
        f.write("live/data", "before");
        f.source(serde_json::json!({"type":"local_path", "path": f.0.join("live")}));
        let root = repos.root("dep+").unwrap();
        assert_eq!(root, std::fs::canonicalize(f.0.join("live")).unwrap());
        assert_eq!(repos.resolve_graph().unwrap().modules["dep+"].name, "dep");
        assert_eq!(repos.resolve_graph().unwrap().modules["dep+"].version, "1");
        f.write("live/data", "after");
        assert_eq!(std::fs::read_to_string(root.join("data")).unwrap(), "after");
        // Both success and failure are cached, without reevaluating descriptors.
        f.source(serde_json::json!({"type":"archive"}));
        assert_eq!(repos.root("dep+").unwrap(), root);
    }

    #[test]
    fn registry_relative_paths_use_absolute_or_registry_relative_base() {
        for absolute in [false, true] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("sources/dep/BUILD", "");
            f.source(serde_json::json!({"type":"local_path", "path":"dep"}));
            let base = if absolute {
                f.0.join("sources")
            } else {
                PathBuf::from("../sources")
            };
            f.write(
                "registry/bazel_registry.json",
                &serde_json::json!({
                    "module_base_path": base, "mirrors": ["https://unused.invalid/"]
                })
                .to_string(),
            );
            assert_eq!(
                repos.root("dep+").unwrap(),
                std::fs::canonicalize(f.0.join("sources/dep")).unwrap()
            );
        }
    }

    #[test]
    fn unused_registry_sources_remain_unmaterialized() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nbazel_dep(name='local',version='1')\nlocal_path_override(module_name='local',path='../local')\n");
        f.write("local/MODULE.bazel", "module(name='local')\n");
        // Invalid before any download is attempted, even with archive support.
        f.source(serde_json::json!({"type":"archive", "url":17}));
        assert!(repos.root("").is_ok());
        assert!(repos.resolved.get().is_none());
        assert!(repos.root("local+").is_ok());
        assert!(repos.registry_roots.get().is_none());
        assert!(repos.root("dep+").unwrap_err().contains("url"));
    }

    #[test]
    fn source_descriptors_reject_unsupported_or_malformed_values() {
        for descriptor in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"type":null}),
            serde_json::json!({"type":"archive"}),
            serde_json::json!({"type":"git"}),
            serde_json::json!({"type":"unknown"}),
            serde_json::json!({"type":"local_path"}),
            serde_json::json!({"type":"local_path","path":null}),
            serde_json::json!({"type":"local_path","path":7}),
            serde_json::json!({"type":"local_path","path":""}),
            serde_json::json!({"type":"local_path","path":"bad\u{0000}path"}),
            serde_json::json!({"type":"local_path","path":".","patches":{}}),
            serde_json::json!({"type":"local_path","path":".","unknown":true}),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.source(descriptor.clone());
            assert!(repos.root("dep+").is_err(), "accepted {descriptor}");
        }
        let f = Fixture::new();
        let repos = f.registry();
        f.write("registry/modules/dep/1/source.json", "{invalid JSON");
        assert!(
            repos
                .root("dep+")
                .unwrap_err()
                .contains("invalid registry JSON")
        );
    }

    #[test]
    fn relative_source_requires_valid_registry_base_configuration() {
        for config in [
            None,
            Some("null"),
            Some("[]"),
            Some("{"),
            Some("{}"),
            Some("{\"module_base_path\":null}"),
            Some("{\"module_base_path\":5}"),
            Some("{\"module_base_path\":\"bad\\u0000path\"}"),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.source(serde_json::json!({"type":"local_path","path":"dep"}));
            if let Some(config) = config {
                f.write("registry/bazel_registry.json", config);
            }
            assert!(repos.root("dep+").is_err(), "accepted {config:?}");
        }
    }

    #[test]
    fn local_source_requires_existing_directory_and_caches_errors() {
        for target in ["missing", "file"] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("file", "not a directory");
            f.source(serde_json::json!({"type":"local_path", "path":f.0.join(target)}));
            let error = repos.root("dep+").unwrap_err();
            f.source(serde_json::json!({"type":"local_path", "path":f.0}));
            assert_eq!(repos.root("dep+").unwrap_err(), error);
        }
    }

    #[test]
    fn selected_registry_source_does_not_fall_back_to_another_registry() {
        let f = Fixture::new();
        let _ = f.registry();
        f.write(
            "other/modules/dep/1/MODULE.bazel",
            "module(name='dep',version='1')\n",
        );
        f.write(
            "other/modules/dep/1/source.json",
            &serde_json::json!({"type":"local_path","path":f.0}).to_string(),
        );
        let repos = RepositorySet::with_options(
            f.0.join("main"),
            ResolverOptions {
                registries: vec![f.0.join("registry").into(), f.0.join("other").into()],
                ..ResolverOptions::default()
            },
        );
        let error = repos.root("dep+").unwrap_err();
        assert!(
            error.contains("registry/modules/dep/1/source.json"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn registry_metadata_symlinks_are_rejected_but_declared_source_symlink_is_allowed() {
        use std::os::unix::fs::symlink;
        for config in [false, true] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("elsewhere.json", "{}");
            let path = if config {
                f.source(serde_json::json!({"type":"local_path","path":"dep"}));
                "registry/bazel_registry.json"
            } else {
                "registry/modules/dep/1/source.json"
            };
            symlink(f.0.join("elsewhere.json"), f.0.join(path)).unwrap();
            assert!(
                repos
                    .root("dep+")
                    .unwrap_err()
                    .contains("symlink registry paths")
            );
        }
        let f = Fixture::new();
        let repos = f.registry();
        f.write("live/BUILD", "");
        symlink(f.0.join("live"), f.0.join("live-link")).unwrap();
        f.source(serde_json::json!({"type":"local_path","path":f.0.join("live-link")}));
        assert_eq!(
            repos.root("dep+").unwrap(),
            std::fs::canonicalize(f.0.join("live")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_descriptor_checks_ancestor_components_after_graph_resolution() {
        let f = Fixture::new();
        let repos = f.registry();
        repos.resolve_graph().unwrap();
        std::fs::rename(f.0.join("registry/modules/dep/1"), f.0.join("relocated")).unwrap();
        std::os::unix::fs::symlink(f.0.join("relocated"), f.0.join("registry/modules/dep/1"))
            .unwrap();
        assert!(
            repos
                .root("dep+")
                .unwrap_err()
                .contains("symlink registry paths")
        );
    }
}
