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
    source_cache: PathBuf,
    absolute_source_cache: OnceLock<Result<PathBuf, String>>,
    resolved: OnceLock<Result<ResolvedGraph, String>>,
    registry_roots: OnceLock<BTreeMap<String, OnceLock<Result<PathBuf, String>>>>,
    direct: OnceLock<Result<DirectRuntime, String>>,
}

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
        let mut value = serializer.serialize_struct("RepositorySpec", 6)?;
        value.serialize_field("definition_identity", &self.definition.identity)?;
        value.serialize_field("schema", &self.definition.schema)?;
        value.serialize_field("local", &self.definition.local)?;
        value.serialize_field("attributes", &self.attributes)?;
        value.serialize_field("canonical_name", &self.canonical_name)?;
        value.serialize_field("original_name", &self.original_name)?;
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
        Self {
            state: Arc::new(RepositoryState {
                runtime_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
                source_cache,
                absolute_source_cache: OnceLock::new(),
                root,
                options,
                resolved: OnceLock::new(),
                registry_roots: OnceLock::new(),
                direct: OnceLock::new(),
            }),
        }
    }

    pub fn with_source_cache(self, cache: PathBuf) -> Self {
        Self::configured(self.state.root.clone(), self.state.options.clone(), cache)
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

    pub fn resolve_graph(&self) -> Result<&ResolvedGraph, String> {
        self.state
            .resolved
            .get_or_init(|| {
                module_graph::resolve_with_source_cache(
                    &self.state.root,
                    &self.state.options,
                    self.source_cache_path()?,
                )
            })
            .as_ref()
            .map_err(Clone::clone)
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
                    repository_rule::bind_attributes(
                        &definition,
                        &declaration.attributes,
                        &declaration.canonical_name,
                        |text| self.parse(text, &group.owner, ""),
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
                let provenance = serde_json::to_vec(&serde_json::json!({
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
                }))
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
                        serde_json::to_vec(&inputs).map_err(|error| {
                            format!("cannot serialize repository execution inputs: {error}")
                        })
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
            let execution = crate::extension_ctx::execute(
                Arc::new(self.clone()), &definition, &loaded.functions, &group.label,
                &group.participants, group.root_has_non_dev_dependency,
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
            }
            let mut repositories = BTreeMap::new();
            let resolution_budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
            for recorded in execution.repositories {
                // Charge omitted defaults and supplied values before the binder
                // expands either into another owned attribute map.
                preparation_budget.schema(&recorded.definition.schema)?;
                for (name, value) in &recorded.attributes {
                    preparation_budget.string(name)?;
                    preparation_budget.recorded(value, 0)?;
                }
                let canonical_name = generated_mapping[&recorded.name].clone();
                let attributes = repository_rule::bind_recorded_attributes(
                    &recorded.definition, &recorded.attributes, &canonical_name,
                    |text| {
                        resolution_budget.borrow_mut().bytes(Label::resolution_size_bound(text, &group.label.repository, &group.label.package, &generated_mapping))?;
                        Label::parse_in(text, &group.label.repository, &group.label.package, &generated_mapping)
                    },
                ).map_err(|error| format!("repository @@{canonical_name}: {error}"))?;
                repositories.insert(recorded.name.clone(), OrdinaryRepository {
                    definition: recorded.definition,
                    attributes,
                    original_name: recorded.name,
                    canonical_name,
                    root: DemandSlot::new(),
                });
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
                            serde_json::to_vec(&inputs).map_err(|error| {
                                format!("cannot serialize repository execution inputs: {error}")
                            })
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
) -> Result<PathBuf, String> {
    let relative = format!("modules/{name}/{version}/source.json");
    let descriptor = registry.location(&relative);
    let source = registry
        .read_object(&relative)?
        .ok_or_else(|| format!("missing registry source descriptor {descriptor}"))?;
    match source.get("type") {
        Some(serde_json::Value::String(kind)) if kind == "local_path" => {}
        None => {
            return crate::materializer::materialize_with_patches(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
            );
        }
        Some(serde_json::Value::String(kind)) if kind == "archive" => {
            return crate::materializer::materialize_with_patches(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
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
        }
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
}
