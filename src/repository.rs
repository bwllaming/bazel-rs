//! Lazy repository roots and strict apparent-name resolution.
//!
//! Main-repository source operations never evaluate MODULE.bazel. External
//! resolution records the graph once; generated import aliases are metadata-only.
//! Ordinary groups prepare on generated-root/mapping demand, while repository
//! rules execute only on root demand. Local source roots remain
//! live inputs; archive sources are materialized only when requested. Git sources
//! are materialized for discovery and revalidated before repository use.
use crate::label::{Label, PackageId, validate_repository_name};
use crate::module_graph::{self, ModuleSource, ResolvedGraph, ResolverOptions};
use crate::patch_inputs::LocalPatchPlan;
use crate::registry::Registry;
use crate::repository_catalog::{self, DirectCatalog};
use crate::repository_demand::DemandSlot;
use crate::{custom_rule::AttrValue, loading, repository_rule};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct RepositorySet {
    state: Arc<RepositoryState>,
}

#[derive(Debug)]
struct RepositoryState {
    runtime_id: u64,
    root: PathBuf,
    options: ResolverOptions,
    repository_downloads_enabled: bool,
    source_cache: PathBuf,
    absolute_source_cache: OnceLock<Result<PathBuf, String>>,
    root_module: OnceLock<Result<module_graph::RootModuleSnapshot, String>>,
    resolved: OnceLock<Result<ResolvedGraph, String>>,
    registry_roots: OnceLock<BTreeMap<String, OnceLock<Result<PathBuf, String>>>>,
    direct: OnceLock<Result<DirectRuntime, String>>,
    environment: Arc<crate::repository_environment::Snapshot>,
    host: Arc<crate::host_os::HostSnapshot>,
    /// IgnoredSubdirectoriesValue per repository, computed once per set.
    ignored: std::sync::Mutex<BTreeMap<String, IgnoredResult>>,
    /// Canonical forms of materialized roots and blocked prefixes for the
    /// link policy, memoized per invocation (published roots never move).
    link_canonical: std::sync::Mutex<BTreeMap<PathBuf, PathBuf>>,
}

type IgnoredResult = Result<Arc<crate::ignored::IgnoredSubdirectories>, String>;

#[derive(Debug)]
struct DirectRuntime {
    catalog: DirectCatalog,
    groups: BTreeMap<String, DemandSlot<PreparedGroup>>,
    roots: BTreeMap<String, DemandSlot<PathBuf>>,
    ordinary: BTreeMap<String, DemandSlot<PreparedOrdinary>>,
}

#[derive(Debug)]
struct PreparedGroup {
    definition: repository_rule::RuleDefinition,
    loaded: loading::LoadedDefinition,
    label: Label,
    attributes: Vec<BTreeMap<String, AttrValue>>,
}

#[derive(Debug)]
struct PreparedOrdinary {
    loaded: loading::LoadedDefinition,
    generated_mapping: BTreeMap<String, String>,
    repositories: BTreeMap<String, OrdinaryRepository>,
    provenance: Vec<u8>,
}

#[derive(Debug)]
struct OrdinaryRepository {
    definition: repository_rule::RuleDefinition,
    attributes: BTreeMap<String, AttrValue>,
    original_name: String,
    canonical_name: String,
    root: DemandSlot<PathBuf>,
}

impl serde::Serialize for OrdinaryRepository {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let has_environ = !self.definition.environ.is_empty();
        let configure = self.definition.configure;
        let mut value = serializer.serialize_struct(
            "RepositorySpec",
            6 + usize::from(has_environ) + usize::from(configure),
        )?;
        value.serialize_field("definition_identity", &self.definition.identity)?;
        value.serialize_field("schema", &self.definition.schema)?;
        value.serialize_field("local", &self.definition.local)?;
        // Omitted when false so pre-existing provenance bytes/keys are unchanged.
        if configure {
            value.serialize_field("configure", &true)?;
        }
        value.serialize_field("attributes", &self.attributes)?;
        value.serialize_field("canonical_name", &self.canonical_name)?;
        value.serialize_field("original_name", &self.original_name)?;
        if has_environ {
            value.serialize_field("environ", &self.definition.environ)?;
        }
        value.end()
    }
}

// Stream borrowed provenance instead of first cloning potentially large
// defaults into serde_json::Value trees. Check the limit before buffer growth.
fn bounded_provenance(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, String> {
    struct Sink {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "ordinary extension host provenance limit exceeded",
                ));
            }
            let required = self.bytes.len() + bytes.len();
            if required > self.bytes.capacity() {
                let capacity = required
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.limit);
                self.bytes
                    .try_reserve_exact(capacity - self.bytes.len())
                    .map_err(std::io::Error::other)?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut sink, value)
        .map_err(|e| format!("cannot serialize ordinary extension provenance: {e}"))?;
    Ok(sink.bytes)
}

const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RepositorySet>();
    send_sync::<PreparedGroup>();
    send_sync::<PreparedOrdinary>();
};

impl RepositorySet {
    pub fn new(root: PathBuf) -> Self {
        Self::with_options(root, ResolverOptions::default())
    }

    pub fn with_options(root: PathBuf, options: ResolverOptions) -> Self {
        Self::configured(root, options, PathBuf::from("bazel-rs-out/repositories"))
    }

    fn configured(root: PathBuf, options: ResolverOptions, source_cache: PathBuf) -> Self {
        Self::configured_environment(
            root,
            options,
            source_cache,
            Arc::default(),
            Arc::default(),
            true,
        )
    }

    fn configured_environment(
        root: PathBuf,
        options: ResolverOptions,
        source_cache: PathBuf,
        environment: Arc<crate::repository_environment::Snapshot>,
        host: Arc<crate::host_os::HostSnapshot>,
        repository_downloads_enabled: bool,
    ) -> Self {
        Self {
            state: Arc::new(RepositoryState {
                runtime_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
                source_cache,
                absolute_source_cache: OnceLock::new(),
                root,
                options,
                repository_downloads_enabled,
                root_module: OnceLock::new(),
                resolved: OnceLock::new(),
                registry_roots: OnceLock::new(),
                direct: OnceLock::new(),
                environment,
                host,
                ignored: std::sync::Mutex::default(),
                link_canonical: std::sync::Mutex::default(),
            }),
        }
    }

    /// The repository's ignored subdirectories (`IgnoredSubdirectoriesValue`),
    /// evaluated once per repository set like a Skyframe value. The lock is
    /// not held while computing: evaluation may resolve repositories.
    pub fn ignored_subdirectories(&self, repository: &str) -> IgnoredResult {
        let cached = self
            .state
            .ignored
            .lock()
            .map_err(|_| "ignored-subdirectories cache poisoned".to_owned())?
            .get(repository)
            .cloned();
        if let Some(result) = cached {
            return result;
        }
        let computed =
            crate::ignored::for_repository(&Arc::new(self.clone()), repository).map(Arc::new);
        self.state
            .ignored
            .lock()
            .map_err(|_| "ignored-subdirectories cache poisoned".to_owned())?
            .entry(repository.to_owned())
            .or_insert(computed)
            .clone()
    }

    pub fn with_source_cache(self, cache: PathBuf) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            cache,
            self.state.environment.clone(),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        )
    }

    /// Configure source acquisition, not registry metadata or Git execution.
    /// Verified completed trees and download blobs remain reusable when disabled.
    /// Like other configuration builders, this starts fresh demand memoization;
    /// the policy is deliberately absent from persistent content identities.
    pub fn with_repository_downloads_enabled(self, enabled: bool) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            self.state.environment.clone(),
            self.state.host.clone(),
            enabled,
        )
    }

    pub fn repository_downloads_enabled(&self) -> bool {
        self.state.repository_downloads_enabled
    }

    /// `--incompatible_disable_starlark_host_transitions` (ResolverOptions).
    pub(crate) fn disable_starlark_host_transitions(&self) -> bool {
        self.state.options.disable_starlark_host_transitions
    }

    /// The Java Starlark semantics options (ResolverOptions::java_semantics).
    pub(crate) fn java_semantics(&self) -> crate::module_graph::JavaSemantics {
        self.state.options.java_semantics
    }

    pub(crate) fn environment(&self) -> &crate::repository_environment::Snapshot {
        &self.state.environment
    }

    pub(crate) fn host(&self) -> Result<Arc<crate::host_os::HostMetadata>, String> {
        self.state.host.get()
    }

    #[cfg(test)]
    fn with_host(self, host: Result<Arc<crate::host_os::HostMetadata>, String>) -> Self {
        // Never rewrite prepared handles or root/environment memoized values.
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            self.state.environment.clone(),
            Arc::new(crate::host_os::HostSnapshot::fixture(host)),
            self.state.repository_downloads_enabled,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_environment_reader(
        self,
        reader: Arc<dyn crate::repository_environment::EnvironmentReader>,
    ) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            Arc::new(crate::repository_environment::Snapshot::with_reader(reader)),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        )
    }

    /// Library-relative cache paths are workspace-relative. The CLI resolves
    /// its cwd-relative option before constructing this set. Both discovery
    /// and later repository access must reuse this exact absolute path.
    pub(crate) fn source_cache_path(&self) -> Result<&Path, String> {
        self.state
            .absolute_source_cache
            .get_or_init(|| {
                if self.state.source_cache.is_absolute() {
                    Ok(self.state.source_cache.clone())
                } else {
                    std::fs::canonicalize(&self.state.root)
                        .map(|root| root.join(&self.state.source_cache))
                        .map_err(|e| {
                            format!(
                                "cannot resolve repository cache workspace {}: {e}",
                                self.state.root.display()
                            )
                        })
                }
            })
            .as_ref()
            .map(PathBuf::as_path)
            .map_err(Clone::clone)
    }

    pub fn main_root(&self) -> &Path {
        &self.state.root
    }

    fn root_module(&self) -> Result<&module_graph::RootModuleSnapshot, String> {
        self.state
            .root_module
            .get_or_init(|| module_graph::read_root_module(&self.state.root, &self.state.options))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Record an optional root MODULE without resolving dependencies or loading
    /// repository/extension definitions. Later graph demand reuses this record,
    /// including cached absence or evaluation errors. Source-profile restrictions
    /// remain deferred to the repository demand paths that enforce them.
    pub fn validate_root_module(&self) -> Result<(), String> {
        self.state.options.validate_registries()?;
        self.root_module().map(|_| ())
    }

    /// The Bzlmod module associated with a repository, for native
    /// `module_name()`/`module_version()` (packages/StarlarkNativeModule.java:669-683,
    /// skyframe/RepositoryMappingFunction.java:100-159). Module-extension repos
    /// report their hosting module in Bazel; that association is not modeled.
    /// ModuleKey.toDisplayString (bazel/bzlmod/ModuleKey.java:70-76) of the
    /// module owning canonical repository `repository`: `root module` or
    /// `module 'NAME@VERSION'` (`_` for an empty key version).
    pub(crate) fn module_display(&self, repository: &str) -> String {
        if repository.is_empty() {
            return "root module".to_owned();
        }
        match self
            .resolve_graph()
            .ok()
            .and_then(|graph| graph.modules.get(repository))
        {
            Some(module) => format!(
                "module '{}@{}'",
                module.name,
                if module.version.is_empty() {
                    "_"
                } else {
                    module.version.as_str()
                }
            ),
            None => format!("module '@@{repository}'"),
        }
    }

    pub(crate) fn associated_module(&self, repository: &str) -> Result<(String, String), String> {
        if repository.is_empty() {
            return self.root_module()?.identity();
        }
        let graph = self.resolve_graph()?;
        if let Some(module) = graph.modules.get(repository) {
            // Module.getVersion is the module() declaration (InterimModule
            // keeps it for non-registry overrides, whose key version is
            // empty; ModuleFileFunction.java:274-279 forces equality otherwise).
            let version = match &module.module.module {
                Some(info) => crate::module_graph::normalize_declared_version(&info.version)?,
                None => module.version.clone(),
            };
            return Ok((module.name.clone(), version));
        }
        Err(format!(
            "module_name()/module_version() in repository @@{repository}: the associated module of a non-module (extension or direct) repository is not implemented by the native profile"
        ))
    }

    /// `register_execution_platforms` label lists in effect without new
    /// module resolution I/O: the root module's (dev registrations unless
    /// dev dependencies are ignored) and, when the module graph has already
    /// been resolved, the non-dev registrations of the other modules
    /// (RegisteredExecutionPlatformsFunction semantics).
    pub(crate) fn known_execution_platform_registrations(&self) -> Vec<Vec<String>> {
        use crate::bzlmod::RegistrationKind;
        let mut found = Vec::new();
        let ignore_dev = self.state.options.ignore_dev_dependencies;
        if let Ok(root) = self.root_module()
            && let Some(module) = root.module_file()
        {
            for registration in &module.registrations {
                if registration.kind == RegistrationKind::ExecutionPlatforms
                    && !(ignore_dev && registration.dev_dependency)
                    && !registration.labels.is_empty()
                {
                    found.push(registration.labels.clone());
                }
            }
        }
        if let Some(Ok(graph)) = self.state.resolved.get() {
            for module in graph.modules.values() {
                for registration in &module.module.registrations {
                    if registration.kind == RegistrationKind::ExecutionPlatforms
                        && !registration.dev_dependency
                        && !registration.labels.is_empty()
                    {
                        found.push(registration.labels.clone());
                    }
                }
            }
        }
        found
    }

    /// `register_toolchains` patterns with the repository they are parsed
    /// in, in RegisteredToolchainsFunction order
    /// (skyframe/toolchains/RegisteredToolchainsFunction.java:96-104, 149-172):
    /// the modules of the dependency graph in breadth-first order (root
    /// first). Non-root dev registrations are dropped, and the root's under
    /// --ignore_dev_dependency (bazel/bzlmod/ModuleFileGlobals.java:390-411).
    /// The module graph is resolved when the root module has dependencies.
    pub(crate) fn toolchain_registrations(&self) -> Result<Vec<(String, Vec<String>)>, String> {
        use crate::bzlmod::RegistrationKind;
        let ignore_dev = self.state.options.ignore_dev_dependencies;
        let mut found = Vec::new();
        let root = self.root_module()?;
        let Some(module) = root.module_file() else {
            return Ok(found);
        };
        for registration in &module.registrations {
            if registration.kind == RegistrationKind::Toolchains
                && !(ignore_dev && registration.dev_dependency)
            {
                found.push((String::new(), registration.labels.clone()));
            }
        }
        if module.effective_dependencies().next().is_none() {
            return Ok(found);
        }
        for module in self.resolve_graph()?.modules_in_bfs_order() {
            for registration in &module.module.registrations {
                if registration.kind == RegistrationKind::Toolchains && !registration.dev_dependency
                {
                    found.push((module.canonical_name.clone(), registration.labels.clone()));
                }
            }
        }
        Ok(found)
    }

    pub fn resolve_graph(&self) -> Result<&ResolvedGraph, String> {
        self.state
            .resolved
            .get_or_init(|| {
                self.state.options.validate_registries()?;
                module_graph::resolve_from_root_snapshot(
                    self.root_module()?,
                    &self.state.options,
                    self.source_cache_path()?,
                )
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Canonical repository roots already materialized by this repository set
    /// (the current invocation): main, local_path modules of a resolved graph,
    /// published registry/Git/built-in trees and generated repositories whose
    /// demand completed. Never resolves, fetches or executes anything; stale
    /// trees and private stages are never listed.
    pub fn materialized_roots(&self) -> Vec<(String, PathBuf)> {
        let mut roots = vec![(String::new(), self.state.root.clone())];
        if let Some(Ok(graph)) = self.state.resolved.get() {
            for (name, module) in &graph.modules {
                if let ModuleSource::LocalPath(root) = &module.source {
                    roots.push((name.clone(), root.clone()));
                }
            }
        }
        if let Some(registry_roots) = self.state.registry_roots.get() {
            for (name, slot) in registry_roots {
                if let Some(Ok(root)) = slot.get() {
                    roots.push((name.clone(), root.clone()));
                }
            }
        }
        if let Some(Ok(runtime)) = self.state.direct.get() {
            for (name, slot) in &runtime.roots {
                if let Some(root) = slot.ready() {
                    roots.push((name.clone(), root.clone()));
                }
            }
            for slot in runtime.ordinary.values() {
                if let Some(prepared) = slot.ready() {
                    for repository in prepared.repositories.values() {
                        if let Some(root) = repository.root.ready() {
                            roots.push((repository.canonical_name.clone(), root.clone()));
                        }
                    }
                }
            }
        }
        roots
    }

    /// Host prefixes no shorter materialized root may claim for links: the
    /// repository store (stale trees, private stages and trash) and, with the
    /// CLI layout `--cache-dir/repositories`, the whole `--cache-dir` root
    /// (action outputs and downloads too).
    pub fn link_blocked_prefixes(&self) -> Vec<PathBuf> {
        let Ok(store) = self.source_cache_path() else {
            return Vec::new();
        };
        let mut blocked = vec![store.to_path_buf()];
        if store.file_name().is_some_and(|name| name == "repositories")
            && let Some(root) = store.parent()
        {
            blocked.push(root.to_path_buf());
        }
        blocked
    }

    /// Whether `repository` is a repository generated by a repository rule
    /// (a `use_repo_rule` declaration or a module extension's repository
    /// call), executed through `repository_ctx`. Never resolves, fetches or
    /// executes anything.
    pub fn is_repository_rule_generated(&self, repository: &str) -> bool {
        if repository.is_empty() {
            return false;
        }
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return false;
        };
        if runtime.catalog.repositories.contains_key(repository) {
            return true;
        }
        runtime.ordinary.values().any(|slot| {
            slot.ready().is_some_and(|prepared| {
                prepared
                    .repositories
                    .values()
                    .any(|generated| generated.canonical_name == repository)
            })
        })
    }

    /// The shared native link policy for reads of `repository`'s sources,
    /// `.bzl` files and globs (`source_links::LinkPolicy`): materialized roots
    /// and blocked store prefixes, plus host-path reads for repositories
    /// generated by repository rules.
    pub fn link_policy(&self, repository: &str) -> crate::source_links::LinkPolicy {
        let mut memo = self
            .state
            .link_canonical
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only existing paths are memoized: a prefix created later in the
        // invocation is canonicalized again.
        let mut canonical = |path: PathBuf| -> Option<PathBuf> {
            if let Some(canonical) = memo.get(&path) {
                return Some(canonical.clone());
            }
            let canonical = std::fs::canonicalize(&path).ok()?;
            memo.insert(path, canonical.clone());
            Some(canonical)
        };
        let others = self
            .materialized_roots()
            .into_iter()
            .filter_map(|(name, path)| canonical(path).map(|path| (name, path)))
            .collect();
        let blocked = self
            .link_blocked_prefixes()
            .into_iter()
            .map(|path| canonical(path.clone()).unwrap_or(path))
            .collect();
        crate::source_links::LinkPolicy {
            others,
            blocked,
            host_reads: self.is_repository_rule_generated(repository),
        }
    }

    pub fn root(&self, repository: &str) -> Result<&Path, String> {
        validate_repository_name(repository)?;
        if repository.is_empty() {
            return Ok(&self.state.root);
        }
        let graph = self.resolve_graph()?;
        let Some(module) = graph.modules.get(repository) else {
            return if self
                .direct_runtime()?
                .catalog
                .repositories
                .contains_key(repository)
            {
                self.direct_root(repository)
            } else {
                self.ordinary_root(repository)
            };
        };
        // MODULE overrides affect discovery metadata, not the upstream special
        // repository filesystem precedence for the canonical built-in name.
        if repository == "bazel_tools"
            && self.state.options.builtin_profile == module_graph::BuiltinProfile::Bazel93Builtins
        {
            let roots = self.state.registry_roots.get_or_init(|| {
                graph
                    .modules
                    .keys()
                    .map(|name| (name.clone(), OnceLock::new()))
                    .collect()
            });
            return roots[repository]
                .get_or_init(|| {
                    crate::builtin_tools::materialize(&self.source_cache_path()?.join("builtins"))
                })
                .as_ref()
                .map(PathBuf::as_path)
                .map_err(Clone::clone);
        }
        match &module.source {
            ModuleSource::Embedded(_) => Err("embedded module source requires the genuine built-in profile and canonical bazel_tools repository".to_owned()),
            ModuleSource::LocalPath(root) => Ok(root),
            ModuleSource::Git {
                spec,
                local_patches,
                tree,
            } => {
                let roots = self.state.registry_roots.get_or_init(|| {
                    graph
                        .modules
                        .keys()
                        .map(|name| (name.clone(), OnceLock::new()))
                        .collect()
                });
                roots[repository].get_or_init(|| {
                    spec.validate(self.state.options.allow_remote)?;
                    if !tree.commit.eq_ignore_ascii_case(&spec.commit) {
                        return Err(format!("Git commit mismatch for repository @@{repository}"));
                    }
                    let (verified, ()) = crate::git_source::materialize(
                        spec,
                        local_patches,
                        self.source_cache_path()?,
                        self.state.options.allow_remote,
                        |_, bytes| {
                            if bytes != tree.module_file {
                                return Err(format!("Git MODULE differs from resolved graph for repository @@{repository}"));
                            }
                            Ok(())
                        },
                    )?;
                    Ok(verified.root)
                }).as_ref().map(PathBuf::as_path).map_err(Clone::clone)
            }
            ModuleSource::Registry {
                registry,
                name,
                version,
                module_file,
                local_patches,
                ..
            } => {
                let roots = self.state.registry_roots.get_or_init(|| {
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
                            self.source_cache_path()?,
                            local_patches,
                            self.repository_downloads_enabled(),
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
        let catalog = &self.direct_runtime()?.catalog;
        if let Some(mapping) = catalog.owner_mappings.get(repository) {
            return Ok(mapping);
        }
        if let Some(entry) = catalog.repositories.get(repository) {
            return Ok(&entry.mapping);
        }
        let (prepared, _) = self.ordinary_repository(repository)?;
        Ok(&prepared.generated_mapping)
    }

    /// Catalog construction is pure metadata work. In particular, its OnceLock
    /// initializer cannot recursively demand a source root or load a definition.
    fn direct_runtime(&self) -> Result<&DirectRuntime, String> {
        self.state
            .direct
            .get_or_init(|| {
                let catalog = repository_catalog::build(
                    self.resolve_graph()?,
                    self.state.options.ignore_dev_dependencies,
                )?;
                let groups = catalog
                    .groups
                    .keys()
                    .map(|id| (id.clone(), DemandSlot::new()))
                    .collect();
                let roots = catalog
                    .repositories
                    .keys()
                    .map(|name| (name.clone(), DemandSlot::new()))
                    .collect();
                let ordinary = catalog
                    .ordinary_groups
                    .keys()
                    .map(|id| (id.clone(), DemandSlot::new()))
                    .collect();
                Ok(DirectRuntime {
                    catalog,
                    groups,
                    roots,
                    ordinary,
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn prepare_direct_group(&self, id: &str) -> Result<&PreparedGroup, String> {
        let runtime = self.direct_runtime()?;
        let group = runtime
            .catalog
            .groups
            .get(id)
            .ok_or_else(|| format!("unknown direct repository group {id:?}"))?;
        let slot = runtime
            .groups
            .get(id)
            .ok_or_else(|| format!("missing direct repository preparation slot {id:?}"))?;
        slot.get_or_try_init(self.state.runtime_id, &format!("prepare:{id}"), || {
            if let Some(error) = &group.owner_profile_error {
                return Err(error.clone());
            }
            let (text, symbol) = group.definition_request()?;
            let label = self.parse(text, &group.owner, "")?;
            let loaded = loading::load_definition_in(
                Arc::new(self.clone()),
                &label,
                symbol,
                &crate::package::module_bzl_globals(),
            )?;
            let definition = repository_rule::unpack(&loaded.value)
                .map_err(|error| format!("repository rule {label}%{symbol}: {error}"))?;
            // An innate usage validates every declaration before executing even
            // its first callback, including declarations not otherwise demanded.
            // Reserve the whole group's copied defaults and supplied payloads
            // before binding any declaration. A budget per invocation would
            // miss amplification by many name-only uses of one large default.
            let mut budget = crate::repository_budget::HostBudget::default();
            for declaration in &group.declarations {
                budget.schema(&definition.schema)?;
                definition.environ.charge(&mut budget)?;
                budget.string(&declaration.canonical_name)?;
                budget.node()?;
                for (name, value) in &declaration.attributes {
                    budget.string(name)?;
                    budget.json(value, 0)?;
                }
            }
            let attributes = group
                .declarations
                .iter()
                .map(|declaration| {
                    repository_rule::bind_attributes_where(
                        &definition,
                        &declaration.attributes,
                        &declaration.canonical_name,
                        |text| self.parse(text, &group.owner, ""),
                        &format!("to the {}", self.module_display(&group.owner)),
                    )
                    .map_err(|error| {
                        format!("repository @@{}: {error}", declaration.canonical_name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(PreparedGroup {
                definition,
                loaded,
                label,
                attributes,
            })
        })
    }

    fn direct_root(&self, canonical: &str) -> Result<&Path, String> {
        let runtime = self.direct_runtime()?;
        let entry = runtime.catalog.repositories.get(canonical).ok_or_else(|| {
            format!("unknown canonical repository @@{canonical}; extension repository not materialized or module not selected")
        })?;
        let slot = runtime
            .roots
            .get(canonical)
            .ok_or_else(|| format!("missing direct repository root slot @@{canonical}"))?;
        // Claim before preparation: definitions can themselves be generated,
        // and a recursive definition demand must encounter our cycle guard.
        slot.get_or_try_init(
            self.state.runtime_id,
            &format!("repository:{canonical}"),
            || {
                let prepared = self.prepare_direct_group(&entry.group_id)?;
                let group = &runtime.catalog.groups[&entry.group_id];
                let declaration = &group.declarations[entry.declaration_index];
                let attributes = &prepared.attributes[entry.declaration_index];
                let mut provenance = serde_json::json!({
                    "profile": "direct-file-template-v1",
                    "owner": group.owner,
                    "raw_identity": group.raw_identity,
                    "public_label": prepared.label.to_string(),
                    "definition_identity": prepared.definition.identity,
                    "sources": prepared.loaded.sources,
                    "attributes": attributes,
                    "schema": prepared.definition.schema,
                    "local": prepared.definition.local,
                    "owner_mapping": runtime.catalog.owner_mappings[&group.owner],
                    "repository_mapping": entry.mapping,
                });
                if prepared.definition.configure {
                    // Omitted when false so pre-existing provenance keys are unchanged.
                    provenance
                        .as_object_mut()
                        .expect("provenance object")
                        .insert("configure".to_owned(), serde_json::Value::Bool(true));
                }
                if !prepared.definition.environ.is_empty() {
                    provenance
                        .as_object_mut()
                        .expect("provenance object")
                        .insert(
                            "environ".to_owned(),
                            serde_json::to_value(&prepared.definition.environ).map_err(|_| {
                                "cannot serialize repository environment declaration".to_owned()
                            })?,
                        );
                }
                let provenance = serde_json::to_vec(&provenance)
                    .map_err(|error| format!("cannot serialize repository provenance: {error}"))?;
                crate::repository_store::materialize(
                    self.source_cache_path()?,
                    &provenance,
                    |stage| {
                        let inputs = crate::repository_ctx::execute(
                            Arc::new(self.clone()),
                            &prepared.definition,
                            &prepared.loaded.functions,
                            attributes,
                            canonical,
                            &declaration.name,
                            &prepared.label,
                            stage,
                        )?;
                        crate::host_os::repository_inputs(self.host()?.as_ref(), &inputs)
                    },
                )
            },
        )
        .map(PathBuf::as_path)
    }

    fn prepare_ordinary_group(&self, id: &str) -> Result<&PreparedOrdinary, String> {
        let runtime = self.direct_runtime()?;
        let group = runtime
            .catalog
            .ordinary_groups
            .get(id)
            .ok_or_else(|| format!("unknown ordinary extension group {id:?}"))?;
        let slot = runtime
            .ordinary
            .get(id)
            .ok_or_else(|| format!("missing ordinary extension preparation slot {id:?}"))?;
        slot.get_or_try_init(self.state.runtime_id, &format!("prepare:ordinary:{id}"), || {
            for participant in &group.participants {
                if let Some(error) = &participant.source_profile_error {
                    return Err(error.clone());
                }
            }
            let loaded = loading::load_module_extension_in(
                Arc::new(self.clone()), &group.label, &group.symbol,
                &crate::package::module_bzl_globals(),
            )?;
            let mut preparation_budget = crate::repository_budget::HostBudget::default();
            for (label, source) in &loaded.sources {
                preparation_budget.string(label)?;
                preparation_budget.bytes(source.bytes.len())?;
                for (apparent, canonical) in &source.mapping {
                    preparation_budget.string(apparent)?;
                    preparation_budget.string(canonical)?;
                }
            }
            let definition = crate::module_extension::unpack(&loaded.value)
                .map_err(|error| format!("module extension {}%{}: {error}", group.label, group.symbol))?;
            let host = self.host()?;
            let host_factors = host.factors(definition.os_dependent, definition.arch_dependent, &mut preparation_budget)?;
            let execution = crate::extension_ctx::execute(
                Arc::new(self.clone()), &definition, &loaded.functions, &group.label,
                &group.symbol, &group.participants, group.root_has_non_dev_dependency,
            )?;

            // The requested extension host owns this mapping, not its first
            // importing MODULE. Install all siblings before binding any spec.
            let host_mapping = self.mapping(&group.label.repository)?;
            for (apparent, canonical) in host_mapping {
                preparation_budget.string(apparent)?;
                preparation_budget.string(canonical)?;
            }
            let mut generated_mapping = host_mapping.clone();
            let mut names = BTreeSet::new();
            // generatedRepoSpecs.keySet(): creation order, for metadata fixups.
            let mut generated_order = Vec::new();
            for recorded in &execution.repositories {
                preparation_budget.string(&recorded.name)?;
                preparation_budget.bytes(group.prefix.len())?;
                crate::bzlmod::validate_repo_name(&recorded.name).map_err(|e| e.to_string())?;
                if !names.insert(recorded.name.clone()) {
                    return Err(format!("duplicate generated repository {:?}", recorded.name));
                }
                let canonical = format!("{}+{}", group.prefix, recorded.name);
                validate_repository_name(&canonical)?;
                let (prefix, _) = canonical.rsplit_once('+')
                    .ok_or_else(|| format!("invalid generated repository @@{canonical}"))?;
                if runtime.catalog.ordinary_routes.get(prefix).map(String::as_str) != Some(id) {
                    return Err(format!("ambiguous ordinary extension route for @@{canonical}"));
                }
                if self.resolve_graph()?.modules.contains_key(&canonical)
                    || runtime.catalog.repositories.contains_key(&canonical) {
                    return Err(format!("generated repository canonical name collision: @@{canonical}"));
                }
                generated_mapping.insert(recorded.name.clone(), canonical);
                generated_order.push(recorded.name.clone());
            }
            let mut repositories = BTreeMap::new();
            let resolution_budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
            for recorded in execution.repositories {
                // Charge omitted defaults and supplied values before the binder
                // expands either into another owned attribute map.
                preparation_budget.schema(&recorded.definition.schema)?;
                recorded.definition.environ.charge(&mut preparation_budget)?;
                for (name, value) in &recorded.attributes {
                    preparation_budget.string(name)?;
                    preparation_budget.recorded(value, 0)?;
                }
                let canonical_name = generated_mapping[&recorded.name].clone();
                let attributes = repository_rule::bind_recorded_attributes_where(
                    &recorded.definition, &recorded.attributes, &canonical_name,
                    |text| {
                        resolution_budget.borrow_mut().bytes(Label::resolution_size_bound(text, &group.label.repository, &group.label.package, &generated_mapping))?;
                        Label::parse_in(text, &group.label.repository, &group.label.package, &generated_mapping)
                    },
                    &format!("in the extension '{}%{}'", group.label.canonical_starlark(), group.symbol),
                ).map_err(|error| format!("repository @@{canonical_name}: {error}"))?;
                repositories.insert(recorded.name.clone(), OrdinaryRepository {
                    definition: recorded.definition,
                    attributes,
                    original_name: recorded.name,
                    canonical_name,
                    root: DemandSlot::new(),
                });
            }
            // SingleExtensionEvalFunction.createSingleExtensionValue (Bazel
            // 9.3:455-487) then SingleExtensionFunction.java:53: metadata errors
            // fail the evaluation; the fixup warning precedes the import check.
            if let (Some(metadata), Some(root_usage)) = (&execution.metadata, &group.root_usage) {
                match crate::extension_metadata::generate_fixup(metadata, root_usage, &generated_order) {
                    Ok(Some(fixup)) => eprintln!("{}", fixup.warning()),
                    Ok(None) => {}
                    Err(message) => return Err(format!(
                        "{message}\nerror evaluating module extension {} in {}",
                        group.symbol, group.label,
                    )),
                }
            }
            // Missing imports invalidate the complete usage even if that alias
            // was never requested. Nothing from this group is published yet.
            let mut imports = Vec::new();
            let mut source_guards = Vec::new();
            for participant in &group.participants {
                preparation_budget.string(&participant.owner)?;
                source_guards.push(serde_json::json!({
                    "owner": participant.owner,
                    "source_profile_error": participant.source_profile_error,
                }));
                for import in &participant.imports {
                    preparation_budget.string(&import.local_name)?;
                    preparation_budget.string(&import.exported_name)?;
                    if !repositories.contains_key(&import.exported_name) {
                        return Err(format!(
                            "module extension {}%{} did not declare imported repository {:?} (alias @{} in MODULE owner {:?})",
                            group.label, group.symbol, import.exported_name, import.local_name, participant.owner,
                        ));
                    }
                    imports.push(serde_json::json!({
                        "owner": participant.owner,
                        "local_name": import.local_name,
                        "exported_name": import.exported_name,
                        "dev_dependency": import.dev_dependency,
                    }));
                }
            }
            #[derive(serde::Serialize)]
            struct Provenance<'a> {
                profile: &'static str,
                public_label: String,
                symbol: &'a str,
                prefix: &'a str,
                sources: &'a BTreeMap<String, loading::SourceProvenance>,
                tag_classes: &'a BTreeMap<String, BTreeMap<String, crate::custom_rule::AttrSpec>>,
                modules: &'a [crate::extension_ctx::BoundModule],
                source_guards: &'a [serde_json::Value],
                root_has_non_dev_dependency: bool,
                generated_mapping: &'a BTreeMap<String, String>,
                repositories: &'a BTreeMap<String, OrdinaryRepository>,
                imports: &'a [serde_json::Value],
                #[serde(skip_serializing_if = "crate::host_os::HostFactors::is_empty")]
                host_factors: crate::host_os::HostFactors<'a>,
                #[serde(skip_serializing_if = "Option::is_none")]
                extension_io: Option<ExtensionIo<'a>>,
                /// module_extension(environ=...) declarations and the digest of
                /// declared/module_ctx.getenv observations; omitted when empty
                /// so existing extension identities are unchanged.
                #[serde(skip_serializing_if = "crate::repository_environment::DeclaredEnvironment::is_empty")]
                environ: &'a crate::repository_environment::DeclaredEnvironment,
                #[serde(skip_serializing_if = "Option::is_none")]
                watched_env: Option<&'a crate::repository_environment::WatchedEnvironment>,
            }
            #[derive(serde::Serialize)]
            struct ExtensionIo<'a> {
                version: u8,
                observations: &'a [crate::extension_io::Observation],
            }
            let provenance = bounded_provenance(&Provenance {
                profile: "ordinary-file-template-v1",
                public_label: group.label.to_string(),
                symbol: &group.symbol, prefix: &group.prefix,
                sources: &loaded.sources, tag_classes: &definition.tag_classes,
                modules: &execution.modules, source_guards: &source_guards,
                root_has_non_dev_dependency: group.root_has_non_dev_dependency,
                generated_mapping: &generated_mapping, repositories: &repositories,
                imports: &imports,
                host_factors,
                extension_io: (!execution.io_observations.is_empty()).then_some(ExtensionIo {
                    version: 1, observations: &execution.io_observations,
                }),
                environ: &definition.environ,
                watched_env: execution.watched_env.as_ref(),
            }, 64 * 1024 * 1024)?;
            Ok(PreparedOrdinary { loaded, generated_mapping, repositories, provenance })
        })
    }

    fn ordinary_repository(
        &self,
        canonical: &str,
    ) -> Result<(&PreparedOrdinary, &OrdinaryRepository), String> {
        let unknown = || {
            format!(
                "unknown canonical repository @@{canonical}; extension repository not materialized or module not selected"
            )
        };
        // Exact last-separator routing avoids executing an unrelated extension
        // for a merely similar canonical name. User names cannot contain '+'.
        let (prefix, name) = canonical.rsplit_once('+').ok_or_else(unknown)?;
        let runtime = self.direct_runtime()?;
        let id = runtime
            .catalog
            .ordinary_routes
            .get(prefix)
            .ok_or_else(unknown)?;
        crate::bzlmod::validate_repo_name(name).map_err(|error| error.to_string())?;
        let prepared = self.prepare_ordinary_group(id)?;
        let repository = prepared.repositories.get(name).ok_or_else(|| {
            format!("generated repository @@{canonical} was not declared by its module extension")
        })?;
        Ok((prepared, repository))
    }

    fn ordinary_root(&self, canonical: &str) -> Result<&Path, String> {
        let (prepared, repository) = self.ordinary_repository(canonical)?;
        repository
            .root
            .get_or_try_init(
                self.state.runtime_id,
                &format!("repository:{canonical}"),
                || {
                    let (label, _) =
                        repository
                            .definition
                            .identity
                            .split_once('%')
                            .ok_or_else(|| {
                                format!(
                                    "invalid repository rule identity {:?}",
                                    repository.definition.identity
                                )
                            })?;
                    let definition_label = Label::parse_canonical(label)?;
                    // Length-frame the already serialized bytes; serializing
                    // Vec<u8> as a JSON number array would amplify them again.
                    let mut provenance = Vec::new();
                    provenance
                        .try_reserve_exact(
                            16 + repository.canonical_name.len() + prepared.provenance.len(),
                        )
                        .map_err(|e| {
                            format!("ordinary extension host provenance allocation limit: {e}")
                        })?;
                    provenance
                        .extend_from_slice(&(repository.canonical_name.len() as u64).to_le_bytes());
                    provenance.extend_from_slice(repository.canonical_name.as_bytes());
                    provenance.extend_from_slice(&(prepared.provenance.len() as u64).to_le_bytes());
                    provenance.extend_from_slice(&prepared.provenance);
                    crate::repository_store::materialize(
                        self.source_cache_path()?,
                        &provenance,
                        |stage| {
                            let inputs = crate::repository_ctx::execute(
                                Arc::new(self.clone()),
                                &repository.definition,
                                &prepared.loaded.functions,
                                &repository.attributes,
                                &repository.canonical_name,
                                &repository.original_name,
                                &definition_label,
                                stage,
                            )?;
                            crate::host_os::repository_inputs(self.host()?.as_ref(), &inputs)
                        },
                    )
                },
            )
            .map(PathBuf::as_path)
    }

    /// Select exactly the mapping parsing needs, without demanding an extension
    /// catalog for base aliases or a module graph for canonical/local labels.
    /// `None` is the local fast path (including the main repository's @// alias).
    fn mapping_for_label(
        &self,
        text: &str,
        repository: &str,
    ) -> Result<Option<&BTreeMap<String, String>>, String> {
        // Apparent aliases require the owning module's mapping. Only the main
        // repository has the empty alias: external @// is NOT self or main.
        let main_empty_alias = repository.is_empty() && text.starts_with("@//");
        if text.starts_with('@') && !text.starts_with("@@") && !main_empty_alias {
            let graph = self.resolve_graph()?;
            let owner = if repository.is_empty() {
                Some((&graph.root, &graph.root_mapping))
            } else {
                graph
                    .modules
                    .get(repository)
                    .map(|module| (&module.module, &module.mapping))
            };
            if let Some((module, base_mapping)) = owner {
                let apparent = text[1..]
                    .split_once("//")
                    .map_or(&text[1..], |(name, _)| name);
                if base_mapping.contains_key(apparent) {
                    return Ok(Some(base_mapping));
                }
                let imported_usage = module.extension_usages.iter().find(|usage| {
                    usage.imports.iter().any(|import| {
                        import.local_name == apparent
                            && !(import.dev_dependency
                                && (!repository.is_empty()
                                    || self.state.options.ignore_dev_dependencies))
                    })
                });
                // Selected-module base aliases remain independent of extension
                // catalog allocation. Only an active generated import needs it.
                if imported_usage.is_some_and(|usage| usage.identity.isolation.is_some()) {
                    return Err("isolated module extensions unsupported".to_owned());
                }
                let supported_generated_import = imported_usage.is_some();
                if !supported_generated_import {
                    return Ok(Some(base_mapping));
                }
            }
            self.mapping(repository).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn resolution_size_bound(
        &self,
        text: &str,
        repository: &str,
        package: &str,
    ) -> Result<usize, String> {
        let empty = BTreeMap::new();
        let mapping = self.mapping_for_label(text, repository)?.unwrap_or(&empty);
        Ok(Label::resolution_size_bound(
            text, repository, package, mapping,
        ))
    }

    pub fn parse(&self, text: &str, repository: &str, package: &str) -> Result<Label, String> {
        if let Some(mapping) = self.mapping_for_label(text, repository)? {
            Label::parse_in(text, repository, package, mapping)
        } else {
            let mapping = if repository.is_empty() && text.starts_with("@//") {
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
    local_patches: &LocalPatchPlan,
    downloads_enabled: bool,
) -> Result<PathBuf, String> {
    let relative = format!("modules/{name}/{version}/source.json");
    let descriptor = registry.location(&relative);
    let source = registry
        .read_object(&relative)?
        .ok_or_else(|| format!("missing registry source descriptor {descriptor}"))?;
    match source.get("type") {
        Some(serde_json::Value::String(kind)) if kind == "local_path" => {}
        None => {
            return crate::materializer::materialize_with_download_policy(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
                downloads_enabled,
            );
        }
        Some(serde_json::Value::String(kind)) if kind == "archive" => {
            return crate::materializer::materialize_with_download_policy(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
                downloads_enabled,
            );
        }
        Some(kind) => return Err(format!("unsupported source type {kind} in {descriptor}")),
    }
    if local_patches.has_transformations() {
        return Err(format!(
            "single_version_override patches and patch_cmds are unsupported for local_path source in {descriptor}; live source checkout is not modified"
        ));
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
    include!("../tests/support/extension_io_boundary_provenance.rs");

    #[test]
    fn provenance_budget_counts_serialized_bytes_and_rejects_before_growth() {
        let input = BTreeMap::from([("nul", "\0\0\0"), ("text", "plain")]);
        let expected = serde_json::to_vec(&input).unwrap();
        assert_eq!(
            bounded_provenance(&input, expected.len()).unwrap(),
            expected
        );
        assert!(
            bounded_provenance(&input, expected.len() - 1)
                .unwrap_err()
                .contains("host provenance limit")
        );
        assert!(
            bounded_provenance(&vec![255_u8; 4096], 4096)
                .unwrap_err()
                .contains("host provenance limit")
        );
        assert!(bounded_provenance(&(), 0).is_err());
    }
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

    fn assert_root_module_demand_is_lazy(repos: &RepositorySet) {
        assert!(repos.state.resolved.get().is_none());
        assert!(repos.state.direct.get().is_none());
        assert!(repos.state.registry_roots.get().is_none());
        assert!(repos.state.absolute_source_cache.get().is_none());
    }

    #[test]
    fn root_module_preflight_records_unused_declarations_without_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nbazel_dep(name='not_downloaded',version='1')\nr=use_repo_rule('//:poison.bzl','r')\nr(name='direct')\ne=use_extension('//:poison.bzl','extension')\ne.tag(value='recorded')\nuse_repo(e,'ordinary')\n");
        f.write("poison.bzl", "fail('definition must not load')\n");
        f.write("BUILD", "fail('BUILD must not load')\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                registries: vec![f.0.join("absent-registry").into()],
                ..Default::default()
            },
        );
        repos.validate_root_module().unwrap();
        assert!(repos.state.root_module.get().unwrap().is_ok());
        assert_root_module_demand_is_lazy(&repos);
        assert!(!f.0.join("bazel-rs-out").exists());
        assert!(!f.0.join("absent-registry").exists());
    }

    #[test]
    fn root_module_success_shared_by_clones_preserves_graph_metadata_after_edit() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='before',version='1')\nbazel_dep(name='dep',version='1',repo_name='original')\n");
        repos.validate_root_module().unwrap();
        let cloned = repos.clone();
        f.write("main/MODULE.bazel", "module(name='after',version='2')\n");
        cloned.validate_root_module().unwrap();
        assert!(std::ptr::eq(
            repos.root_module().unwrap(),
            cloned.root_module().unwrap()
        ));
        assert_root_module_demand_is_lazy(&cloned);
        let graph = cloned.resolve_graph().unwrap();
        let info = graph.root.module.as_ref().unwrap();
        assert_eq!((&*info.name, &*info.version), ("before", "1"));
        assert_eq!(graph.root.dependencies.len(), 1);
        assert_eq!(graph.root_mapping["original"], "dep+");
        let ModuleSource::Registry {
            registry,
            module_file,
            ..
        } = &graph.modules["dep+"].source
        else {
            panic!("expected the original selected registry metadata");
        };
        assert_eq!(registry.local_path().unwrap(), f.0.join("registry"));
        assert_eq!(module_file, b"module(name='dep',version='1')\n");
        assert!(std::ptr::eq(graph, repos.resolve_graph().unwrap()));
        let fresh = RepositorySet::with_options(f.0.join("main"), repos.state.options.clone());
        fresh.validate_root_module().unwrap();
        let graph = fresh.resolve_graph().unwrap();
        assert_eq!(graph.root.module.as_ref().unwrap().name, "after");
        assert_eq!(graph.root.module.as_ref().unwrap().version, "2");
        assert!(graph.modules.is_empty());
    }

    #[test]
    fn root_module_language_errors_are_cached_across_clones_not_fresh_sets() {
        let f = Fixture::new();
        for source in [
            "x=RunEnvironmentInfo()",
            "x=UnknownRootSymbol",
            "module(",
            "x=1//0",
        ] {
            f.write("MODULE.bazel", source);
            let expected = crate::bzlmod::evaluate(&f.0.join("MODULE.bazel")).unwrap_err();
            let repos = RepositorySet::new(f.0.clone());
            let error = repos.validate_root_module().unwrap_err();
            assert_eq!(error, expected);
            assert!(error.contains("MODULE.bazel"), "{error}");
            assert_root_module_demand_is_lazy(&repos);
            let cloned = repos.clone();
            f.write("MODULE.bazel", "module(name='fixed')\n");
            assert_eq!(cloned.validate_root_module().unwrap_err(), error);
            assert_eq!(cloned.resolve_graph().unwrap_err(), error);
            let fresh = RepositorySet::new(f.0.clone());
            fresh.validate_root_module().unwrap();
            assert_eq!(
                fresh
                    .resolve_graph()
                    .unwrap()
                    .root
                    .module
                    .as_ref()
                    .unwrap()
                    .name,
                "fixed"
            );
        }
    }

    #[test]
    fn root_module_absence_is_valid_but_graph_demand_keeps_original_diagnostic() {
        let f = Fixture::new();
        let expected = crate::bzlmod::evaluate(&f.0.join("MODULE.bazel")).unwrap_err();
        let repos = RepositorySet::new(f.0.clone());
        repos.validate_root_module().unwrap();
        assert_root_module_demand_is_lazy(&repos);
        let cloned = repos.clone();
        f.write("MODULE.bazel", "module(name='created')\n");
        cloned.validate_root_module().unwrap();
        assert!(std::ptr::eq(
            repos.root_module().unwrap(),
            cloned.root_module().unwrap()
        ));
        assert_eq!(cloned.resolve_graph().unwrap_err(), expected);
        assert_eq!(repos.resolve_graph().unwrap_err(), expected);
        let fresh = RepositorySet::new(f.0.clone());
        fresh.validate_root_module().unwrap();
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "created"
        );
    }

    #[test]
    fn root_module_source_cache_configuration_starts_a_fresh_runtime() {
        let f = Fixture::new();
        let repos = f.registry().with_source_cache("first".into());
        f.source(serde_json::json!({"type":"local_path", "path":f.0}));
        repos.validate_root_module().unwrap();
        repos.mapping("").unwrap();
        repos.root("dep+").unwrap();
        assert!(repos.state.root_module.get().is_some());
        assert!(repos.state.resolved.get().is_some());
        assert!(repos.state.direct.get().is_some());
        assert!(repos.state.registry_roots.get().is_some());
        assert!(repos.state.absolute_source_cache.get().is_some());
        f.write("main/MODULE.bazel", "module(name='fresh')\n");
        let fresh = repos.clone().with_source_cache("second".into());
        assert_ne!(fresh.state.runtime_id, repos.state.runtime_id);
        assert_eq!(fresh.state.options, repos.state.options);
        assert!(fresh.state.root_module.get().is_none());
        assert_root_module_demand_is_lazy(&fresh);
        fresh.validate_root_module().unwrap();
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "fresh"
        );
        assert_eq!(
            repos
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "main"
        );
        assert_eq!(fresh.source_cache_path().unwrap(), f.0.join("main/second"));
        assert_eq!(repos.source_cache_path().unwrap(), f.0.join("main/first"));
    }

    #[test]
    fn root_module_ignore_dev_context_is_recorded_and_configuration_is_isolated() {
        let f = Fixture::new();
        for source in [
            "r=use_repo_rule('//:absent.bzl','r')\nr(name='same')\nr(name='same',dev_dependency=True)\n",
            "local_path_override(module_name='unused',path='one')\nlocal_path_override(module_name='unused',path='two')\n",
        ] {
            f.write("MODULE.bazel", source);
            let strict = RepositorySet::new(f.0.clone());
            assert!(strict.validate_root_module().is_err());
            let ignored = RepositorySet::with_options(
                f.0.clone(),
                ResolverOptions {
                    ignore_dev_dependencies: true,
                    ..Default::default()
                },
            );
            ignored.validate_root_module().unwrap();
            assert_root_module_demand_is_lazy(&ignored);
            let expected = crate::bzlmod::evaluate_with_context(
                &f.0.join("MODULE.bazel"),
                crate::bzlmod::EvaluationContext {
                    role: crate::bzlmod::ModuleRole::Root,
                    ignore_dev_dependencies: true,
                },
            )
            .unwrap();
            assert_eq!(ignored.resolve_graph().unwrap().root, expected);
            assert!(strict.clone().validate_root_module().is_err());
        }
    }

    #[test]
    fn root_module_builtin_profile_is_isolated_and_retained_by_source_cache_configuration() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='bazel_tools')\n");
        let explicit = RepositorySet::new(f.0.clone());
        explicit.validate_root_module().unwrap();
        let builtin = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                builtin_profile: module_graph::BuiltinProfile::Bazel93Builtins,
                ..Default::default()
            },
        );
        let error = builtin.validate_root_module().unwrap_err();
        assert!(
            error.contains("repository name \"bazel_tools\" is already used"),
            "{error}"
        );
        f.write("MODULE.bazel", "module(name='changed')\n");
        assert_eq!(builtin.clone().validate_root_module().unwrap_err(), error);
        let fresh = builtin.with_source_cache("new-cache".into());
        assert_eq!(
            fresh.state.options.builtin_profile,
            module_graph::BuiltinProfile::Bazel93Builtins
        );
        assert!(fresh.state.root_module.get().is_none());
        fresh.validate_root_module().unwrap();
        assert_root_module_demand_is_lazy(&fresh);
        assert_eq!(
            explicit
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "bazel_tools"
        );
    }

    #[test]
    fn root_module_source_profile_error_remains_deferred_to_repository_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nr=use_repo_rule('//:poison.bzl','r')\nr(name='made',text=str(len('é')))\n");
        f.write("poison.bzl", "fail('definition must not load')\n");
        let repos = RepositorySet::new(f.0.clone());
        repos.validate_root_module().unwrap();
        assert_root_module_demand_is_lazy(&repos);
        let error = repos
            .resolve_graph()
            .unwrap()
            .root
            .source_profile_error
            .as_ref()
            .unwrap()
            .clone();
        assert!(error.contains("non-ASCII"), "{error}");
        assert_eq!(
            repos.parse("@made//:file", "", "").unwrap().repository,
            "+r+made"
        );
        assert_eq!(repos.root("+r+made").unwrap_err(), error);
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn root_module_invalid_registry_options_precede_recording() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "UnknownRootSymbol\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                registries: vec![Registry::parse("https://example.invalid").unwrap()],
                ..Default::default()
            },
        );
        let expected = repos.state.options.validate_registries().unwrap_err();
        assert_eq!(repos.validate_root_module().unwrap_err(), expected);
        assert!(repos.state.root_module.get().is_none());
        assert_root_module_demand_is_lazy(&repos);
        assert_eq!(repos.resolve_graph().unwrap_err(), expected);
        assert!(repos.state.root_module.get().is_none());
    }

    #[test]
    fn ordinary_repository_local_metadata_participates_in_provenance_not_attributes() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'implicit','explicit','enabled')\n",
        );
        f.write("BUILD", "fail('BUILD must not run')\n");
        f.write(
            "rules.bzl",
            "def implementation(ctx):\n    fail('repository callback must not run during preparation')\nImplementationAlias = implementation\nimplicit = repository_rule(ImplementationAlias)\nexplicit = repository_rule(ImplementationAlias, local=False)\nenabled = repository_rule(ImplementationAlias, local=True)\nAlias = enabled\n",
        );
        f.write(
            "reexport.bzl",
            "load(':rules.bzl', 'implicit', 'explicit', 'Alias')\nImplicit = implicit\nExplicit = explicit\nEnabled = Alias\n",
        );
        f.write(
            "extension.bzl",
            "load(':reexport.bzl', 'Implicit', 'Explicit', 'Enabled')\ndef implementation(ctx):\n    Implicit(name='implicit')\n    Explicit(name='explicit')\n    Enabled(name='enabled')\nextension = module_extension(implementation)\n",
        );
        let repos = RepositorySet::new(f.0.clone());
        let (prepared, enabled) = repos.ordinary_repository("+extension+enabled").unwrap();
        let provenance: serde_json::Value = serde_json::from_slice(&prepared.provenance).unwrap();
        for (name, local) in [("implicit", false), ("explicit", false), ("enabled", true)] {
            let repository = &prepared.repositories[name];
            assert_eq!(repository.definition.local, local);
            assert_eq!(
                repository.definition.identity,
                format!("//:rules.bzl%{name}")
            );
            assert!(!repository.attributes.contains_key("local"));
            assert!(repository.definition.schema.is_empty());
            assert_eq!(provenance["repositories"][name]["local"], local);
            // configure=False (default) is omitted, keeping legacy bytes/keys.
            assert!(provenance["repositories"][name].get("configure").is_none());
        }
        let mut configured = OrdinaryRepository {
            definition: enabled.definition.clone(),
            attributes: enabled.attributes.clone(),
            original_name: enabled.original_name.clone(),
            canonical_name: enabled.canonical_name.clone(),
            root: DemandSlot::new(),
        };
        let unconfigured = serde_json::to_string(&configured).unwrap();
        configured.definition.configure = true;
        let with_configure: serde_json::Value = serde_json::to_value(&configured).unwrap();
        assert_eq!(with_configure["configure"], serde_json::Value::Bool(true));
        assert_ne!(serde_json::to_string(&configured).unwrap(), unconfigured);
        // Hold all source, identity, schema and call metadata constant: only the
        // retained local flag changes serialized provenance, including false.
        let mut toggled = OrdinaryRepository {
            definition: enabled.definition.clone(),
            attributes: enabled.attributes.clone(),
            original_name: enabled.original_name.clone(),
            canonical_name: enabled.canonical_name.clone(),
            root: DemandSlot::new(),
        };
        let before = bounded_provenance(&toggled, 4096).unwrap();
        toggled.definition.local = false;
        let after = bounded_provenance(&toggled, 4096).unwrap();
        assert_ne!(before, after);
        let mut before: serde_json::Value = serde_json::from_slice(&before).unwrap();
        let mut after: serde_json::Value = serde_json::from_slice(&after).unwrap();
        assert_eq!(
            before.as_object_mut().unwrap().remove("local"),
            Some(true.into())
        );
        assert_eq!(
            after.as_object_mut().unwrap().remove("local"),
            Some(false.into())
        );
        assert_eq!(before, after);
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn download_policy_is_immutable_and_preserved_by_every_rebuilding_builder() {
        use crate::repository_environment::tests::Reader;
        let f = Fixture::new();
        let original = f.registry();
        assert!(original.repository_downloads_enabled());
        original.validate_root_module().unwrap();
        original.mapping("").unwrap();
        let disabled = original.clone().with_repository_downloads_enabled(false);
        assert!(original.repository_downloads_enabled());
        assert!(!disabled.clone().repository_downloads_enabled());
        assert!(Arc::ptr_eq(&disabled.state, &disabled.clone().state));
        let reader = Reader::new(Ok(None));
        for rebuilt in [
            disabled.clone(),
            disabled.clone().with_source_cache("other-cache".into()),
            disabled.clone().with_environment_reader(reader.clone()),
            disabled.clone().with_host(Err("test host".into())),
        ] {
            assert!(!rebuilt.repository_downloads_enabled());
            assert_ne!(original.state.runtime_id, rebuilt.state.runtime_id);
            assert_eq!(original.state.options, rebuilt.state.options);
            assert!(rebuilt.state.root_module.get().is_none());
            assert!(rebuilt.state.resolved.get().is_none());
            assert!(rebuilt.state.registry_roots.get().is_none());
            assert!(rebuilt.state.direct.get().is_none());
            assert!(rebuilt.state.absolute_source_cache.get().is_none());
        }
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &disabled.state.environment
        ));
        assert!(Arc::ptr_eq(&original.state.host, &disabled.state.host));
        let enabled = disabled.with_repository_downloads_enabled(true);
        assert!(enabled.repository_downloads_enabled());
        assert_ne!(original.state.runtime_id, enabled.state.runtime_id);
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changing_download_policy_does_not_reuse_demand_success_or_failure() {
        for failure in [false, true] {
            let f = environment_fixture(
                false,
                "[]",
                if failure {
                    "fail('old demand failure')"
                } else {
                    "ctx.file('value', 'old')"
                },
            );
            let original = RepositorySet::new(f.0.clone());
            let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
            let old = original.root(&canonical).map(Path::to_owned);
            assert_eq!(old.is_err(), failure);
            let replacement = "def implementation(ctx):\n    ctx.file('value', 'new')\nR = repository_rule(implementation, environ=[])\n";
            f.write("rules.bzl", replacement);
            let fresh = original.clone().with_repository_downloads_enabled(false);
            assert_ne!(fresh.state.runtime_id, original.state.runtime_id);
            assert!(fresh.state.direct.get().is_none());
            let root = fresh.root(&canonical).unwrap();
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"new");
            assert_eq!(original.clone().root(&canonical).map(Path::to_owned), old);
            if let Ok(old) = old {
                assert_eq!(std::fs::read(old.join("value")).unwrap(), b"old");
            }
        }
    }

    #[test]
    fn download_policy_leaves_no_source_io_provenance_and_persistent_keys_unchanged() {
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "[]", "ctx.file('value', 'constant')");
            let original = RepositorySet::new(f.0.clone());
            let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
            let disabled = original.clone().with_repository_downloads_enabled(false);
            if ordinary {
                let before = &original
                    .ordinary_repository(&canonical)
                    .unwrap()
                    .0
                    .provenance;
                let after = &disabled
                    .ordinary_repository(&canonical)
                    .unwrap()
                    .0
                    .provenance;
                assert_eq!(before, after);
            }
            let root = original.root(&canonical).unwrap();
            assert_eq!(root, disabled.root(&canonical).unwrap());
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
        }
    }

    #[test]
    fn extension_io_zero_operations_omit_entire_field_and_preserve_legacy_key_order() {
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        let mut baseline = None;
        for enabled in [true, false] {
            let repos = RepositorySet::new(f.0.clone()).with_repository_downloads_enabled(enabled);
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&prepared.provenance).unwrap();
            let keys: Vec<_> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "profile",
                    "public_label",
                    "symbol",
                    "prefix",
                    "sources",
                    "tag_classes",
                    "modules",
                    "source_guards",
                    "root_has_non_dev_dependency",
                    "generated_mapping",
                    "repositories",
                    "imports"
                ]
            );
            assert_eq!(value["profile"], "ordinary-file-template-v1");
            assert!(!String::from_utf8_lossy(&prepared.provenance).contains("extension_io"));
            let root = repos.root(&canonical).unwrap().to_owned();
            let snapshot = (prepared.provenance.clone(), root);
            if let Some(baseline) = &baseline {
                assert_eq!(&snapshot, baseline);
            } else {
                baseline = Some(snapshot);
            }
        }
    }

    #[test]
    fn unused_failed_download_overwrite_and_reads_rekey_fresh_runtime_not_demand_memoization() {
        use crate::extension_io::tests::{Server, response};
        let a = vec![b'a'; 33000];
        let b = vec![b'b'; 33000];
        let server = Server::new(
            [&a, &a, &b, &b, &a, &a]
                .into_iter()
                .map(|bytes| response(200, bytes, ""))
                .collect(),
        );
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        f.write("extension.bzl", &format!("load(':rules.bzl', 'R')\ndef implementation(ctx):\n    ctx.download({url:?}, 'same', {sha:?}, allow_fail=True)\n    ctx.read('same', watch='no')\n    ctx.download({url:?}, 'same', {sha:?}, allow_fail=True)\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation)\n", url=server.url, sha="00".repeat(32)));
        let mut roots = Vec::new();
        let mut observations = Vec::new();
        for index in 0..3 {
            let repos = RepositorySet::new(f.0.clone());
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            let root = repos.root(&canonical).unwrap().to_owned();
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
            assert_eq!(server.count(), (index + 1) * 2);
            assert_eq!(repos.root(&canonical).unwrap(), root);
            let other = repos.parse("@other//:value", "", "").unwrap().repository;
            repos.root(&other).unwrap();
            assert_eq!(server.count(), (index + 1) * 2);
            let prepared = &repos.ordinary_repository(&canonical).unwrap().0;
            let value: serde_json::Value = serde_json::from_slice(&prepared.provenance).unwrap();
            let io = &value["extension_io"];
            assert_eq!(io["version"], 1);
            assert_eq!(io["observations"].as_array().unwrap().len(), 3);
            assert_eq!(
                io["observations"][0]["result"],
                serde_json::json!({"success":false})
            );
            assert_eq!(io["observations"][0]["state"]["generation"], 1);
            assert_eq!(io["observations"][1]["operation"], "read");
            assert_eq!(io["observations"][2]["state"]["generation"], 2);
            let receipt = serde_json::to_string(io).unwrap();
            assert!(!receipt.contains(&server.url));
            assert!(!receipt.contains("bazel-extension-io-"));
            observations.push(receipt);
            roots.push(root);
        }
        assert_ne!(roots[0], roots[1]);
        assert_eq!(roots[0], roots[2]);
        assert_ne!(observations[0], observations[1]);
        assert_eq!(observations[0], observations[2]);
    }

    #[test]
    fn environment_snapshot_lifetime_follows_owner_not_source_cache_configuration() {
        use crate::repository_environment::{Watch, tests::Reader};
        let f = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let original = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        let clone = original.clone();
        let configured = clone.clone().with_source_cache("another-cache".into());
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &clone.state.environment
        ));
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &configured.state.environment
        ));
        assert_ne!(original.state.runtime_id, configured.state.runtime_id);
        for owner in [&original, &clone, &configured] {
            Watch::default()
                .observe(owner.environment(), "SYNTHETIC")
                .unwrap();
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        let fresh = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        Watch::default()
            .observe(fresh.environment(), "SYNTHETIC")
            .unwrap();
        assert_eq!(reader.calls.load(Ordering::SeqCst), 2);
        assert!(!Arc::ptr_eq(
            &original.state.environment,
            &RepositorySet::new(f.0.clone()).state.environment
        ));
        assert!(!Arc::ptr_eq(
            &original.state.environment,
            &RepositorySet::with_options(f.0.clone(), ResolverOptions::default())
                .state
                .environment
        ));
        assert!(!format!("{original:?}").contains("SyntheticSecret"));
    }

    #[test]
    fn host_snapshot_is_shared_but_test_injection_starts_fresh_demand_state() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let original = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
        let (prepared, _) = original.ordinary_repository(&canonical).unwrap();
        let bytes = prepared.provenance.clone();
        let configured = original.clone().with_source_cache("another-cache".into());
        let replacement_reader = original.clone().with_environment_reader(reader.clone());
        for owner in [original.clone(), configured, replacement_reader] {
            assert!(Arc::ptr_eq(&original.state.host, &owner.state.host));
            assert!(Arc::ptr_eq(
                &original.host().unwrap(),
                &owner.host().unwrap()
            ));
        }
        let injected = original.clone().with_host(Ok(HostMetadata::fixture(
            "native-only",
            "test-arch",
            "test-os",
        )));
        assert!(!Arc::ptr_eq(&original.state.host, &injected.state.host));
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &injected.state.environment
        ));
        assert!(injected.state.direct.get().is_none());
        assert!(injected.state.root_module.get().is_none());
        assert_ne!(original.state.runtime_id, injected.state.runtime_id);
        assert_eq!(
            original
                .ordinary_repository(&canonical)
                .unwrap()
                .0
                .provenance,
            bytes
        );
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_only_host_factor_matrix_is_checked_before_materialization() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        for os in [false, true] {
            for arch in [false, true] {
                let f = environment_fixture(true, "[]", "fail('repository callback must not run')");
                f.write("extension.bzl", &format!("load(':rules.bzl', 'R')\ndef implementation(ctx):\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation, os_dependent={}, arch_dependent={})\n", if os { "True" } else { "False" }, if arch { "True" } else { "False" }));
                let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
                let run = |name, host_arch, factor| {
                    let repos = RepositorySet::new(f.0.clone())
                        .with_environment_reader(reader.clone())
                        .with_host(Ok(HostMetadata::fixture(name, host_arch, factor)));
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
                    assert!(!f.0.join("bazel-rs-out").exists());
                    prepared.provenance.clone()
                };
                let base = run("getter", "arch-a", "os-a");
                assert_eq!(base != run("getter", "arch-a", "os-b"), os);
                assert_eq!(base != run("getter", "arch-b", "os-a"), arch);
                assert_eq!(base != run("getter", "arch-b", "os-b"), os || arch);
                assert_eq!(base, run("another getter", "arch-a", "os-a"));
                let json: serde_json::Value = serde_json::from_slice(&base).unwrap();
                if !os && !arch {
                    assert_eq!(json.as_object().unwrap().len(), 12);
                    assert!(json.get("host_factors").is_none());
                } else {
                    let factors = json["host_factors"].as_object().unwrap();
                    assert_eq!(factors.len(), usize::from(os) + usize::from(arch));
                    assert_eq!(
                        factors.get("os").and_then(|v| v.as_str()),
                        os.then_some("os-a")
                    );
                    assert_eq!(
                        factors.get("arch").and_then(|v| v.as_str()),
                        arch.then_some("arch-a")
                    );
                }
                assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn native_only_unread_host_rekeys_direct_and_generated_repositories_and_restores() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "[]", "ctx.file('value', 'constant')");
            let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
            let run = |name, arch, factor| {
                let repos = RepositorySet::new(f.0.clone())
                    .with_environment_reader(reader.clone())
                    .with_host(Ok(HostMetadata::fixture(name, arch, factor)));
                let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                let root = repos.root(&canonical).unwrap().to_owned();
                assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
                assert_eq!(repos.clone().root(&canonical).unwrap(), root);
                root
            };
            let base = run("getter-a", "arch-a", "os-a");
            assert_ne!(base, run("getter-b", "arch-a", "os-a"));
            assert_ne!(base, run("getter-a", "arch-b", "os-a"));
            // Canonical factor is not an unconditional repository host input.
            assert_eq!(base, run("getter-a", "arch-a", "os-b"));
            assert_eq!(base, run("getter-a", "arch-a", "os-a"));
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn host_getter_reads_never_enable_flags_but_selected_attributes_remain_inputs() {
        use crate::host_os::HostMetadata;
        for select in [false, true] {
            let f = Fixture::new();
            f.write("BUILD", "");
            f.write(
                "MODULE.bazel",
                "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo')\n",
            );
            f.write("rules.bzl", "def implementation(ctx):\n    fail('must not materialize')\nR=repository_rule(implementation, attrs={'selected':attr.string()})\n");
            f.write("extension.bzl", &format!("load(':rules.bzl','R')\ndef implementation(ctx):\n    host = ctx.os\n    name = host.name\n    arch = host.arch\n    R(name='repo', selected={})\nextension=module_extension(implementation)\n", if select { "name + '/' + arch" } else { "'constant'" }));
            let run =
                |name, arch| {
                    let repos = RepositorySet::new(f.0.clone())
                        .with_host(Ok(HostMetadata::fixture(name, arch, "canonical")));
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    let (prepared, repository) = repos.ordinary_repository(&canonical).unwrap();
                    let json: serde_json::Value =
                        serde_json::from_slice(&prepared.provenance).unwrap();
                    assert!(json.get("host_factors").is_none());
                    assert!(!repository.attributes.contains_key("host_factors"));
                    assert!(!f.0.join("bazel-rs-out").exists());
                    prepared.provenance.clone()
                };
            let base = run("getter-a", "arch-a");
            assert_eq!(base != run("getter-b", "arch-a"), select);
            assert_eq!(base != run("getter-a", "arch-b"), select);
        }
    }

    #[test]
    fn unsupported_host_stays_lazy_then_fails_before_callbacks_and_publication() {
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "['SYNTHETIC']", "fail('callback ran')");
            let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
            let repos = RepositorySet::new(f.0.clone())
                .with_environment_reader(reader.clone())
                .with_host(Err("synthetic unsupported host".into()));
            repos.validate_root_module().unwrap();
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            for _ in 0..2 {
                let error = repos.root(&canonical).unwrap_err();
                assert!(error.contains("synthetic unsupported host"), "{error}");
                assert!(!error.contains("callback ran"));
            }
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            let trees = f.0.join("bazel-rs-out/repositories/direct/trees");
            assert!(!trees.exists() || std::fs::read_dir(trees).unwrap().next().is_none());
        }
    }

    #[test]
    fn versioned_host_envelope_intentionally_migrates_old_constant_repository_identity() {
        let cache = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let inputs = crate::repository_ctx::ExecutionInputs::default();
        let legacy = serde_json::to_vec(&inputs).unwrap();
        assert_eq!(legacy, br#"{"templates":[]}"#);
        let host = crate::host_os::HostMetadata::fixture("linux", "amd64", "linux");
        let current = crate::host_os::repository_inputs(&host, &inputs).unwrap();
        let run = |bytes: &[u8]| {
            crate::repository_store::materialize(cache.path(), b"same-provenance", |stage| {
                std::fs::write(stage.join("constant"), b"same-content").unwrap();
                Ok(bytes.to_vec())
            })
            .unwrap()
        };
        let old_root = run(&legacy);
        let new_root = run(&current);
        assert_ne!(old_root, new_root);
        assert_eq!(new_root, run(&current));
        assert_eq!(old_root, run(&legacy));
    }

    fn environment_fixture(ordinary: bool, declaration: &str, body: &str) -> Fixture {
        let f = Fixture::new();
        f.write("BUILD", "");
        f.write("rules.bzl", &format!("def implementation(ctx):\n    {body}\nR = repository_rule(implementation, environ={declaration})\n"));
        if ordinary {
            f.write(
                "MODULE.bazel",
                "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo','other')\n",
            );
            f.write("extension.bzl", "load(':rules.bzl', 'R')\ndef implementation(ctx):\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation)\n");
        } else {
            f.write(
                "MODULE.bazel",
                "r=use_repo_rule('//:rules.bzl','R')\nr(name='repo')\nr(name='other')\n",
            );
        }
        f
    }

    #[test]
    fn direct_and_ordinary_environment_content_identity_restore_and_demand_memoization() {
        use crate::repository_environment::{Watch, tests::Reader};
        for ordinary in [false, true] {
            for (declaration, body, output_changes) in [
                ("['SYNTHETIC']", "ctx.file('value', 'constant')", false),
                (
                    "[]",
                    "ctx.getenv('SYNTHETIC')\n    ctx.file('value', 'constant')",
                    false,
                ),
                (
                    "('SYNTHETIC',)",
                    "ctx.file('value', ctx.getenv('SYNTHETIC'))",
                    true,
                ),
            ] {
                let f = environment_fixture(ordinary, declaration, body);
                let run = |value: &str, unrelated: bool| {
                    let reader = Reader::new(Ok(Some(value.into())));
                    let repos =
                        RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
                    if unrelated {
                        Watch::default()
                            .observe(repos.environment(), "UNRELATED")
                            .unwrap();
                    }
                    let root = repos.root(&canonical).unwrap().to_path_buf();
                    assert_eq!(
                        std::fs::read(root.join("value")).unwrap(),
                        if output_changes {
                            value.as_bytes()
                        } else {
                            b"constant"
                        }
                    );
                    let calls = reader.calls.load(Ordering::SeqCst);
                    assert_eq!(calls, if unrelated { 2 } else { 1 });
                    assert_eq!(repos.clone().root(&canonical).unwrap(), root);
                    assert_eq!(reader.calls.load(Ordering::SeqCst), calls);
                    // Only inventory persists; neither requested names nor values.
                    for entry in
                        std::fs::read_dir(f.0.join("bazel-rs-out/repositories/direct/manifests"))
                            .unwrap()
                    {
                        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
                        let text = String::from_utf8(bytes).unwrap();
                        assert!(!text.contains("SyntheticSecret"));
                        assert!(!text.contains("SYNTHETIC"));
                        let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(manifest.as_object().unwrap().len(), 4);
                        assert_eq!(manifest["profile"], "direct-file-template-v1");
                    }
                    root
                };
                let first = run("SyntheticSecretFirst", false);
                assert_eq!(run("SyntheticSecretFirst", true), first);
                assert_ne!(run("SyntheticSecretChanged", false), first);
                assert_eq!(run("SyntheticSecretFirst", false), first);
            }
        }
    }

    #[test]
    fn unused_and_prepared_declarations_do_not_capture_environment() {
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "['SYNTHETIC']", "fail('must not run')");
            let reader = Reader::new(Ok(None));
            let repos = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            repos.mapping(&canonical).unwrap();
            if ordinary {
                let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
                assert!(String::from_utf8_lossy(&prepared.provenance).contains("environ"));
            } else {
                let runtime = repos.direct_runtime().unwrap();
                repos
                    .prepare_direct_group(&runtime.catalog.repositories[&canonical].group_id)
                    .unwrap();
            }
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            assert!(!f.0.join("bazel-rs-out").exists());
        }
    }

    #[test]
    fn environment_capture_and_callback_errors_never_publish_and_demand_errors_are_cached() {
        use crate::repository_environment::{ReadError, tests::Reader};
        for ordinary in [false, true] {
            for reader_failure in [false, true] {
                let f = environment_fixture(
                    ordinary,
                    "['SYNTHETIC']",
                    "ctx.file('partial', 'safe')\n    ctx.getenv('SYNTHETIC')\n    fail('deliberate failure')",
                );
                let reader = Reader::new(if reader_failure {
                    Err(ReadError)
                } else {
                    Ok(Some("SyntheticSecret".into()))
                });
                let repos = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
                let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                let error = repos.root(&canonical).unwrap_err();
                assert!(!error.contains("SyntheticSecret"));
                assert!(
                    error.contains(if reader_failure {
                        "environment read failed"
                    } else {
                        "deliberate failure"
                    }),
                    "{error}"
                );
                assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
                assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
                for subdir in ["trees", "manifests", "locks"] {
                    assert_eq!(
                        std::fs::read_dir(
                            f.0.join("bazel-rs-out/repositories/direct").join(subdir)
                        )
                        .unwrap()
                        .count(),
                        0
                    );
                }
            }
        }
    }

    #[test]
    fn environment_provenance_omission_keeps_legacy_field_order_and_profile() {
        let f = environment_fixture(true, "[]", "return None");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
        let (prepared, repository) = repos.ordinary_repository(&canonical).unwrap();
        let before = serde_json::to_string(repository).unwrap();
        assert!(!before.contains("environ"));
        let legacy = serde_json::json!({
            "definition_identity": repository.definition.identity,
            "schema": repository.definition.schema,
            "local": repository.definition.local,
            "attributes": repository.attributes,
            "canonical_name": repository.canonical_name,
            "original_name": repository.original_name,
        });
        assert_eq!(before, serde_json::to_string(&legacy).unwrap());
        let mut changed = OrdinaryRepository {
            definition: repository.definition.clone(),
            attributes: repository.attributes.clone(),
            canonical_name: repository.canonical_name.clone(),
            original_name: repository.original_name.clone(),
            root: DemandSlot::new(),
        };
        changed.definition.environ =
            crate::repository_environment::DeclaredEnvironment::from_names(
                [Ok("SYNTHETIC")],
                &mut crate::repository_budget::HostBudget::default(),
            )
            .unwrap();
        let mut with_environ = serde_json::to_value(&changed).unwrap();
        assert_eq!(
            with_environ
                .as_object_mut()
                .unwrap()
                .remove("environ")
                .unwrap(),
            serde_json::json!(["SYNTHETIC"])
        );
        assert_eq!(serde_json::to_string(&with_environ).unwrap(), before);
        let provenance: serde_json::Value = serde_json::from_slice(&prepared.provenance).unwrap();
        assert_eq!(provenance["profile"], "ordinary-file-template-v1");
    }

    #[test]
    fn cloned_repository_handles_share_graph_and_fresh_configuration_does_not_mutate_them() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<RepositorySet>();
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='before')\n");
        let original = RepositorySet::new(f.0.clone()).with_source_cache("first".into());
        let cloned = original.clone();
        let graph = original.resolve_graph().unwrap();
        assert!(std::ptr::eq(graph, cloned.resolve_graph().unwrap()));
        f.write("MODULE.bazel", "module(name='after')\n");
        let fresh = cloned.with_source_cache("second".into());
        assert_ne!(original.state.runtime_id, fresh.state.runtime_id);
        assert!(fresh.state.direct.get().is_none());
        assert_eq!(
            original
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "before"
        );
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "after"
        );
        assert_eq!(original.source_cache_path().unwrap(), f.0.join("first"));
        assert_eq!(fresh.source_cache_path().unwrap(), f.0.join("second"));
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
                let bound = repos.resolution_size_bound(input, "", "p").unwrap();
                assert!(bound >= repos.parse(input, "", "p").unwrap().to_string().len());
            }
            assert!(repos.resolution_size_bound("//p:x", "dep+", "").is_ok());
            assert_eq!(repos.parse("//p:x", "dep+", "").unwrap().repository, "dep+");
            assert!(repos.state.resolved.get().is_none());
            assert!(repos.parse("@//p:x", "dep+", "").is_err());
            assert!(repos.parse("@dep//p:x", "", "").is_err());
            assert!(repos.state.resolved.get().is_some());
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
    fn direct_repository_aliases_resolve_without_definition_io_and_are_owner_scoped() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='root_generated')\n");
        f.write("registry/modules/dep/1/MODULE.bazel", "module(name='dep',version='1')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='dep_generated')\nr(name='ignored',dev_dependency=True)\n");
        assert!(repos.root("").is_ok());
        assert!(repos.state.resolved.get().is_none());
        assert_eq!(
            repos.parse("@dep//:file", "", "").unwrap().repository,
            "dep+"
        );
        for (name, owner, canonical) in [
            ("root_generated", "", "+r+root_generated"),
            ("dep_generated", "dep+", "dep++r+dep_generated"),
        ] {
            let label = repos.parse(&format!("@{name}//:file"), owner, "").unwrap();
            assert_eq!(label.repository, canonical);
            assert_eq!(repos.mapping(owner).unwrap()[name], canonical);
            assert_eq!(
                repos
                    .parse(&format!("@{name}//:file"), canonical, "")
                    .unwrap(),
                label
            );
            assert!(
                repos
                    .parse("@unknown//:file", canonical, "")
                    .unwrap_err()
                    .contains("unknown apparent repository")
            );
        }
        for (name, owner) in [
            ("dep_generated", ""),
            ("root_generated", "dep+"),
            ("ignored", "dep+"),
        ] {
            assert!(
                repos
                    .parse(&format!("@{name}//:file"), owner, "")
                    .unwrap_err()
                    .contains("unknown apparent repository")
            );
        }
        assert!(repos.state.registry_roots.get().is_none());
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn direct_definition_failure_is_demanded_and_cached_only_in_this_runtime() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "module(name='main')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='generated')\n",
        );
        f.write("BUILD", "");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@generated//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+r+generated");
        assert!(!f.0.join("bazel-rs-out").exists());
        let error = repos.root(&canonical).unwrap_err();
        assert!(error.contains("missing.bzl"), "{error}");
        f.write("missing.bzl", "r = 1\n");
        assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
        let fresh = repos.with_source_cache("another-cache".into());
        assert!(
            fresh
                .root(&canonical)
                .unwrap_err()
                .contains("expected a frozen repository_rule")
        );
        assert!(!f.0.join("another-cache").exists());
    }

    #[test]
    fn ordinary_import_aliases_are_metadata_only_and_owner_scoped() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\ne=use_extension('@dep//:absent.bzl','extension')\nuse_repo(e, local='generated')\nr=use_repo_rule('//:also_absent.bzl','r')\nr(name='direct')\n");
        f.write("registry/modules/dep/1/MODULE.bazel", "module(name='dep',version='1')\ne=use_extension('//:absent.bzl','extension')\nuse_repo(e, own='generated')\n");
        let label = repos.parse("@local//:file", "", "").unwrap();
        assert_eq!(label.repository, "dep++extension+generated");
        assert_eq!(repos.parse("@own//:file", "dep+", "").unwrap(), label);
        assert_eq!(repos.mapping("").unwrap()["local"], label.repository);
        assert_eq!(
            repos.mapping("+r+direct").unwrap()["local"],
            label.repository
        );
        assert!(
            repos
                .parse("@local//:file", "dep+", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(
            repos
                .parse("@own//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(repos.state.registry_roots.get().is_none());
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn ordinary_definition_failure_is_demanded_and_shared_by_root_and_mapping() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:absent.bzl','extension')\nuse_repo(e,'generated')\n",
        );
        f.write("BUILD", "");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@generated//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+extension+generated");
        assert!(!f.0.join("bazel-rs-out").exists());
        let error = repos.root(&canonical).unwrap_err();
        assert!(error.contains("absent.bzl"), "{error}");
        f.write("absent.bzl", "extension = 1\n");
        assert_eq!(repos.mapping(&canonical).unwrap_err(), error);
        assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
        let fresh = repos.with_source_cache("fresh-cache".into());
        assert!(
            fresh
                .root(&canonical)
                .unwrap_err()
                .contains("expected a frozen module_extension")
        );
        assert!(!f.0.join("fresh-cache").exists());
    }

    #[test]
    fn unknown_ordinary_prefix_never_loads_or_executes_a_similar_extension() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'generated')\n",
        );
        f.write("BUILD", "");
        f.write("extension.bzl", "def implementation(ctx):\n    fail('callback must not run')\nextension = module_extension(implementation=implementation)\n");
        let repos = RepositorySet::new(f.0.clone());
        for canonical in [
            "+extension2+generated",
            "+extension+generated+suffix",
            "+extension+",
            "+other+generated",
        ] {
            assert!(repos.root(canonical).is_err());
            assert!(repos.mapping(canonical).is_err());
        }
        // Replacing the definition after unknown demands demonstrates that no
        // preparation result was cached for the real exact-prefix route.
        f.write("extension.bzl", "extension = 1\n");
        let error = repos.root("+extension+generated").unwrap_err();
        assert!(
            error.contains("expected a frozen module_extension"),
            "{error}"
        );
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn base_aliases_stay_lazy_and_isolated_imports_fail_only_on_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\ne=use_extension('//:absent.bzl','extension',isolate=True)\nuse_repo(e,'generated')\n");
        let repos = RepositorySet::new(f.0.clone());
        assert!(repos.resolution_size_bound("@main//:file", "", "").is_ok());
        assert_eq!(repos.parse("@main//:file", "", "").unwrap().repository, "");
        assert!(repos.state.direct.get().is_none());
        assert!(
            repos
                .resolution_size_bound("@unrelated//:file", "", "")
                .is_ok()
        );
        assert!(
            repos
                .parse("@unrelated//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(repos.state.direct.get().is_none());
        assert!(
            repos
                .resolution_size_bound("@generated//:file", "", "")
                .unwrap_err()
                .contains("isolated module extensions unsupported")
        );
        assert!(
            repos
                .parse("@generated//:file", "", "")
                .unwrap_err()
                .contains("isolated module extensions unsupported")
        );
        assert!(repos.state.direct.get().is_none());
        let ignored = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                ignore_dev_dependencies: true,
                ..Default::default()
            },
        );
        f.write("MODULE.bazel", "e=use_extension('//:absent.bzl','extension',isolate=True,dev_dependency=True)\nuse_repo(e,'generated')\n");
        assert!(
            ignored
                .parse("@generated//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(ignored.state.direct.get().is_none());
    }

    #[test]
    fn unused_invalid_definition_label_stays_lazy_until_root_demand() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "r=use_repo_rule('//bad/../:defs.bzl','maker')\nr(name='unused')\n",
        );
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@unused//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+maker+unused");
        assert!(!f.0.join("bazel-rs-out").exists());
        assert!(repos.root(&canonical).is_err());
    }

    #[test]
    fn ignored_root_direct_repositories_do_not_enter_namespace() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='ignored',dev_dependency=True)\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                ignore_dev_dependencies: true,
                ..Default::default()
            },
        );
        assert!(
            repos
                .parse("@ignored//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(
            repos
                .resolve_graph()
                .unwrap()
                .root
                .extension_usages
                .is_empty()
        );
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
    fn local_path_sources_reject_override_transformations_without_mutating_live_tree() {
        for options in [
            "patches=[':fix.patch'],patch_strip=1",
            "patch_cmds=['touch command-ran']",
            "patches=['@@external+//:missing.patch']",
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("main/MODULE.bazel", &format!("bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',{options})\n"));
            f.write("main/BUILD", "");
            f.write(
                "main/fix.patch",
                "--- a/data\n+++ b/data\n@@ -1 +1 @@\n-before\n+after\n",
            );
            f.write("live/data", "before\n");
            f.write("live/MODULE.bazel", "invalid live MODULE !!!");
            f.source(serde_json::json!({"type":"local_path", "path":f.0.join("live")}));
            assert_eq!(repos.resolve_graph().unwrap().modules.len(), 1);
            assert!(repos.state.registry_roots.get().is_none());
            let error = repos.root("dep+").unwrap_err();
            assert!(
                error.contains("unsupported for local_path source"),
                "{error}"
            );
            assert_eq!(std::fs::read(f.0.join("live/data")).unwrap(), b"before\n");
            assert_eq!(
                std::fs::read(f.0.join("live/MODULE.bazel")).unwrap(),
                b"invalid live MODULE !!!"
            );
            assert!(!f.0.join("main/command-ran").exists());
            assert!(!f.0.join("main/bazel-rs-out").exists());
        }
    }

    #[test]
    fn strip_only_override_keeps_local_path_sources_live() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',patch_strip=3)\n");
        f.write("live/data", "before");
        f.source(serde_json::json!({"type":"local_path", "path":f.0.join("live")}));
        let root = repos.root("dep+").unwrap();
        f.write("live/data", "after");
        assert_eq!(std::fs::read(root.join("data")).unwrap(), b"after");
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn archive_commands_and_external_patches_fail_before_fetch_or_cache_creation() {
        for (options, diagnostic) in [
            ("patch_cmds=['touch command-ran']", "patch_cmds"),
            (
                "patches=['@@external+//missing:fix.patch']",
                "canonical external patch",
            ),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("main/MODULE.bazel", &format!("bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',{options})\n"));
            // Deliberately invalid descriptor: unsupported local operations must
            // be diagnosed before archive parsing, fetching or cache creation.
            f.source(serde_json::json!({"type":"archive"}));
            assert_eq!(repos.resolve_graph().unwrap().modules.len(), 1);
            let error = repos.root("dep+").unwrap_err();
            assert!(error.contains(diagnostic), "{error}");
            assert!(!f.0.join("main/command-ran").exists());
            assert!(!f.0.join("main/bazel-rs-out").exists());
        }
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
        assert!(repos.state.resolved.get().is_none());
        assert!(repos.root("local+").is_ok());
        assert!(repos.state.registry_roots.get().is_none());
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

    #[test]
    fn link_blocked_prefixes_cover_the_store_and_the_cli_cache_root() {
        let temp = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cli =
            RepositorySet::new(root.clone()).with_source_cache(root.join("cache/repositories"));
        assert_eq!(
            cli.link_blocked_prefixes(),
            [root.join("cache/repositories"), root.join("cache")]
        );
        let other = RepositorySet::new(root.clone()).with_source_cache(root.join("store"));
        assert_eq!(other.link_blocked_prefixes(), [root.join("store")]);
    }
}
