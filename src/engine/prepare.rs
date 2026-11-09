//! S2, second part (docs/PARALLEL_ANALYSIS.md, "S2, second part"): a
//! Starlark target's prepare off the coordinator.
//!
//! The prepare of a plan entry ([`Engine::prepare_custom`]: dependency
//! views, prerequisite checks, toolchain and exec-group contexts, ctx
//! surface) is one generic function over a context ([`PrepareCx`]) that
//! answers its lookups and takes its writes:
//!
//! * [`CoordPrepare`] — the coordinator's own prepare (`&mut Engine`, the
//!   planning state): exactly the sequential engine's;
//! * [`SharedPrepare`] — a prepare run on a prepare thread while the
//!   coordinator waits ([`Engine::prepare_batch`]): it reads the engine, the
//!   planning state and the configured nodes through shared references
//!   (nothing writes them while it runs) and records what it would write —
//!   the console and aquery owners, the tracker's accesses — for the
//!   coordinator to apply. Its memo fills (dependency views, the shared
//!   `Target` identities, native output groups) go to the batch's
//!   single-flight memos ([`BatchMemos`]): whoever needs an entry first
//!   fills the one value every prepare of the batch shares, and the
//!   coordinator moves them into the planning state after the batch. A
//!   lookup it cannot answer read-only (a package not loaded, a source file
//!   not memoized, an attribute aspect) aborts it: the coordinator prepares
//!   the entry itself (`PERF-PARALLEL prepare_fallback`).

use super::*;

/// Target lookups and the engine, for the prepare's helpers.
pub(super) trait Lookup {
    fn engine(&self) -> &Engine;
    /// [`Engine::target`].
    fn target(&mut self, label: &Label) -> Result<Target, String>;
    /// The no-config configuration (interned on first use).
    fn no_config(&mut self) -> Result<ConfigId, String>;
    /// The resolved attribute aspects of an attribute (Engine::attribute_aspects).
    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String>;
}

/// A dependency view's writes ([`Engine::dependency_view_in`]).
pub(super) trait ViewCx: Lookup {
    /// Work ahead of its turn (P3: what a view registers is noted for an
    /// earlier turn's hit).
    fn ahead(&self) -> bool;
    /// Register an artifact in the build's index (register_artifact).
    fn register(&mut self, file: &File, artifact: &Artifact);
    /// The artifacts the view registered ahead of its turn
    /// (Engine::view_artifacts).
    fn note_view_artifacts(&mut self, key: ConfiguredTargetKey, artifacts: Artifacts);
    /// The framework OutputGroupInfo of a native rule target, built once
    /// (ActionPlanning::native_output_groups).
    fn native_output_group(
        &mut self,
        key: &ConfiguredTargetKey,
        fill: &mut dyn FnMut(&Engine) -> Result<starlark::values::OwnedFrozenValue, String>,
    ) -> Result<starlark::values::OwnedFrozenValue, String>;
    /// The one shared `Target` value of a configured target
    /// (ActionPlanning::target_identities).
    fn identity(
        &mut self,
        key: TargetIdentityKey,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target>;
}

/// A Starlark target's prepare ([`Engine::prepare_custom`]).
pub(super) trait PrepareCx: Lookup {
    /// The results of the targets planned so far.
    fn reads(&self) -> &dyn PlanReads;
    /// The console and aquery owners of the target's actions.
    fn note_owner(
        &mut self,
        action_owner: &str,
        owner: crate::shared_path::SharedPath,
        key: &ConfiguredTargetKey,
    );
    /// Engine::starlark_prerequisite_errors of the prepared views.
    fn prerequisite_errors(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        exec_config: ConfigId,
        targets: &BTreeMap<String, DependencyTarget>,
    ) -> Result<(), String>;
    /// The dependency view of one edge (with its attribute aspects).
    fn edge_view(
        &mut self,
        rule: &Rule,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<DependencyTarget, String>;
}

impl Lookup for Engine {
    fn engine(&self) -> &Engine {
        self
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        Engine::target(self, label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        Ok(self.configurations.intern_no_config())
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        Engine::attribute_aspects(self, rule, identities)
    }
}

/// The coordinator's dependency views (Engine::plan_dependency_view).
pub(super) struct CoordViews<'a> {
    pub(super) engine: &'a mut Engine,
    pub(super) artifact_index: &'a ArtifactIndex,
    pub(super) native_output_groups:
        &'a mut BTreeMap<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
    pub(super) target_identities:
        &'a mut BTreeMap<TargetIdentityKey, Arc<crate::starlark_types::Target>>,
}

impl Lookup for CoordViews<'_> {
    fn engine(&self) -> &Engine {
        self.engine
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.engine.target(label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        Ok(self.engine.configurations.intern_no_config())
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.engine.attribute_aspects(rule, identities)
    }
}

impl ViewCx for CoordViews<'_> {
    fn ahead(&self) -> bool {
        self.engine.ahead
    }

    fn register(&mut self, file: &File, artifact: &Artifact) {
        register_artifact(self.artifact_index, file, artifact);
    }

    fn note_view_artifacts(&mut self, key: ConfiguredTargetKey, artifacts: Artifacts) {
        self.engine.view_artifacts.insert(key, artifacts);
    }

    fn native_output_group(
        &mut self,
        key: &ConfiguredTargetKey,
        fill: &mut dyn FnMut(&Engine) -> Result<starlark::values::OwnedFrozenValue, String>,
    ) -> Result<starlark::values::OwnedFrozenValue, String> {
        if let Some(frozen) = self.engine.frozen_walk.clone() {
            // S3: shared with the workers' prepares.
            if let Some(value) = frozen.output_groups.get(key) {
                return Ok(value.clone());
            }
            let engine = &*self.engine;
            return frozen
                .memos
                .output_groups
                .get_or_fill(key, || fill(engine).map(Some))?
                .map(|value| (*value).clone())
                .ok_or_else(|| "internal error: an output group was not filled".to_owned());
        }
        if let Some(value) = self.native_output_groups.get(key) {
            return Ok(value.clone());
        }
        let value = fill(self.engine)?;
        self.native_output_groups.insert(key.clone(), value.clone());
        Ok(value)
    }

    fn identity(
        &mut self,
        key: TargetIdentityKey,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target> {
        if let Some(frozen) = self.engine.frozen_walk.clone() {
            // S3: shared with the workers' prepares.
            if let Some(identity) = frozen.identities.get(&key) {
                return identity.clone();
            }
            return match frozen
                .memos
                .identities
                .get_or_fill(&key, || Ok::<_, std::convert::Infallible>(Some(fill())))
            {
                Ok(Some(identity)) => (*identity).clone(),
                _ => fill(),
            };
        }
        self.target_identities
            .entry(key)
            .or_insert_with(fill)
            .clone()
    }
}

/// The coordinator's prepare (Engine::prepare_target).
pub(super) struct CoordPrepare<'a> {
    pub(super) engine: &'a mut Engine,
    pub(super) state: &'a mut ActionPlanning,
    pub(super) configured: &'a mut ConfiguredNodes,
}

impl Lookup for CoordPrepare<'_> {
    fn engine(&self) -> &Engine {
        self.engine
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.engine.target(label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        Ok(self.engine.configurations.intern_no_config())
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.engine.attribute_aspects(rule, identities)
    }
}

impl PrepareCx for CoordPrepare<'_> {
    fn prerequisite_errors(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        exec_config: ConfigId,
        targets: &BTreeMap<String, DependencyTarget>,
    ) -> Result<(), String> {
        Engine::starlark_prerequisite_errors(
            self.engine,
            rule,
            key,
            exec_config,
            self.configured,
            targets,
        )
    }

    fn reads(&self) -> &dyn PlanReads {
        &*self.state
    }

    fn note_owner(
        &mut self,
        action_owner: &str,
        owner: crate::shared_path::SharedPath,
        key: &ConfiguredTargetKey,
    ) {
        self.engine
            .console_owners
            .insert(action_owner.to_owned(), owner);
        self.engine.aquery_owner(action_owner, key, Vec::new);
    }

    fn edge_view(
        &mut self,
        rule: &Rule,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<DependencyTarget, String> {
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            native_output_groups,
            validation,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            workspace_status,
            views,
            input_groups,
            input_memos,
            listed_tree_actions,
            ..
        } = &mut *self.state;
        if aspects.is_empty() {
            return self.engine.plan_dependency_view(
                self.configured,
                dry,
                custom_providers,
                provider_keys,
                runfiles_info,
                artifact_index,
                declared,
                native_output_groups,
                validation,
                target_identities,
                views,
                label,
                edge_config,
            );
        }
        // MergedConfiguredTarget.of's MergingException is reported at the
        // consumer's location (MergedConfiguredTarget.java:210-213).
        let location = rule.location.clone();
        let mut planning = PlanState {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            workspace_status,
            views,
            input_groups: &*input_groups,
            input_memos: &*input_memos,
            listed_tree_actions,
        };
        Ok(self
            .engine
            .plan_aspect_dependency(&mut planning, self.configured, label, edge_config, aspects)
            .map_err(|error| located(location.as_deref(), error))?
            .0)
    }
}

/// A dependency view filled by a prepare of a batch: the view, the
/// artifacts it registered ahead of its turn and the lookups its miss path
/// made (Engine::view_artifacts, Engine::view_accesses).
#[derive(Clone)]
pub(super) struct ViewFill {
    pub(super) view: DependencyTarget,
    pub(super) artifacts: Option<Artifacts>,
    pub(super) accesses: Option<Arc<[Label]>>,
}

/// The memos the prepares of one batch fill (single-flight), moved into
/// the planning state after it ([`Engine::apply_batch_memos`]).
#[derive(Default)]
pub(super) struct BatchMemos {
    pub(super) views: delta::SingleFlight<ConfiguredTargetKey, ViewFill>,
    pub(super) identities:
        delta::SingleFlight<TargetIdentityKey, Arc<crate::starlark_types::Target>>,
    pub(super) output_groups:
        delta::SingleFlight<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
}

/// Marker of a prepare off the coordinator that needs the coordinator
/// (never reported: it prepares the entry itself).
pub(super) const SHARED_ABORT: &str = "\u{1}shared-prepare-abort";

/// What a prepare off the coordinator would have written, applied by the
/// coordinator in plan order ([`Engine::apply_shared_record`]).
#[derive(Default)]
pub(super) struct SharedRecord {
    /// The tracker's accesses (Engine::track_access), in order.
    pub(super) accesses: Vec<Label>,
    /// The console and aquery owners of the target's actions.
    pub(super) owner: Option<(String, crate::shared_path::SharedPath, ConfiguredTargetKey)>,
    /// Why it needs the coordinator.
    pub(super) abort: Option<&'static str>,
    /// T3: the source files it resolved itself (not memoized yet), with
    /// whether the path may be memoized and what the lookup records:
    /// applied where the coordinator would have looked them up.
    pub(super) sources: Vec<(Label, PathBuf, bool, SourceSide)>,
}

/// A prepare on a prepare thread ([`Engine::prepare_batch`]), or (S3,
/// eval pass 2) on an analysis worker while the coordinator commits
/// ([`parallel::Frozen`]).
pub(super) struct SharedPrepare<'a> {
    pub(super) engine: &'a Engine,
    /// The planned targets' results.
    reads: &'a dyn PlanReads,
    /// The memos filled before (the planning state's).
    views: &'a DependencyViews,
    identities: &'a BTreeMap<TargetIdentityKey, Arc<crate::starlark_types::Target>>,
    output_groups: &'a BTreeMap<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
    pub(super) configured: &'a ConfiguredNodes,
    pub(super) memos: &'a BatchMemos,
    /// The artifact index read and stamped at the entry's plan index.
    pub(super) index: ArtifactIndex,
    pub(super) record: SharedRecord,
    /// The lookups of the dependency views being built (innermost last).
    capture: Vec<Vec<Label>>,
    /// The artifacts the view being built registered.
    view_artifacts: Option<Artifacts>,
    /// T3: resolve a declared source file not memoized yet here (a native
    /// rule's results; recorded in `record.sources`) instead of aborting.
    pub(super) resolve_sources: bool,
    /// Its packages' input labels, computed once per package.
    inputs: HashMap<PackageId, HashSet<Label>>,
    /// The source files it resolved (each looked up again by the rule's
    /// inputs, prerequisites and runfiles), and the directories it probed
    /// on their way beyond the engine's probes.
    resolved: HashMap<Label, PathBuf>,
    probes: HashMap<PathBuf, SourceDirProbe>,
    /// The link policies of the repositories of the symlinked sources it
    /// resolved (one view of the materialized roots for the job).
    link_policies: HashMap<String, crate::source_links::LinkPolicy>,
}

/// The engine's source-directory probes, then a job's own.
struct ReadThrough<'a> {
    base: &'a HashMap<PathBuf, SourceDirProbe>,
    own: &'a mut HashMap<PathBuf, SourceDirProbe>,
}

impl SourceProbes for ReadThrough<'_> {
    fn get(&self, path: &Path) -> Option<SourceDirProbe> {
        self.base.get(path).or_else(|| self.own.get(path)).cloned()
    }
    fn put(&mut self, path: PathBuf, probe: SourceDirProbe) {
        self.own.insert(path, probe);
    }
}

impl<'a> SharedPrepare<'a> {
    pub(super) fn new(
        engine: &'a Engine,
        state: &'a ActionPlanning,
        configured: &'a ConfiguredNodes,
        memos: &'a BatchMemos,
        index: usize,
    ) -> Self {
        let stamp = u32::try_from(index).unwrap_or(u32::MAX - 1);
        Self {
            engine,
            reads: state,
            views: &state.views,
            identities: &state.target_identities,
            output_groups: &state.native_output_groups,
            configured,
            memos,
            index: ArtifactIndex {
                shared: Arc::clone(&state.artifact_index.shared),
                turn: stamp,
                stamp,
                provisional: false,
            },
            record: SharedRecord::default(),
            capture: Vec::new(),
            view_artifacts: None,
            resolve_sources: false,
            inputs: HashMap::new(),
            resolved: HashMap::new(),
            probes: HashMap::new(),
            link_policies: HashMap::new(),
        }
    }

    /// A prepare of plan entry `index` on an analysis worker (S3, eval
    /// pass 2): what it reads is the walk's frozen engine and the results
    /// committed so far.
    pub(super) fn frozen(frozen: &'a parallel::Frozen, index: usize) -> Self {
        let stamp = u32::try_from(index).unwrap_or(u32::MAX - 1);
        Self {
            engine: &frozen.engine,
            reads: &*frozen.results,
            views: &frozen.views,
            identities: &frozen.identities,
            output_groups: &frozen.output_groups,
            configured: &frozen.configured,
            memos: &frozen.memos,
            index: ArtifactIndex {
                shared: Arc::clone(&frozen.artifacts),
                turn: stamp,
                stamp,
                // Its views register what dependencies' dry hold: their
                // generated outputs are registered (provisionally, if
                // published) already and stay so; a new entry is a source
                // File's (no producer to choose).
                provisional: true,
            },
            record: SharedRecord::default(),
            capture: Vec::new(),
            view_artifacts: None,
            resolve_sources: false,
            inputs: HashMap::new(),
            resolved: HashMap::new(),
            probes: HashMap::new(),
            link_policies: HashMap::new(),
        }
    }

    fn abort(&mut self, why: &'static str) -> String {
        self.record.abort.get_or_insert(why);
        SHARED_ABORT.to_owned()
    }

    /// Engine::track_access, recorded.
    fn access(&mut self, label: &Label) {
        if self.engine.tracking() {
            if let Some(capture) = self.capture.last_mut() {
                capture.push(label.clone());
            }
            self.record.accesses.push(label.clone());
        }
    }

    /// Engine::plan_dependency_view read-only: the planning state's memo,
    /// else the batch's (filled once).
    pub(super) fn plan_view(
        &mut self,
        label: &Label,
        edge_config: ConfigId,
    ) -> Result<DependencyTarget, String> {
        let key = ConfiguredTargetKey::new(label.clone(), edge_config);
        let engine = self.engine;
        let reads = ViewReads {
            configured: self.configured,
            reads: self.reads,
        };
        let hit = self.views.get(&key).map(|view| ViewFill {
            view: view.clone(),
            artifacts: engine.view_artifacts.get(&key).cloned(),
            accesses: engine.view_accesses.get(&key).cloned(),
        });
        let fill = match hit {
            Some(fill) => fill,
            None => {
                let memos = self.memos;
                let mut filled_here = false;
                let filled = memos.views.get_or_fill(&key, || {
                    filled_here = true;
                    crate::plan_timings::note_dependency_view(&(label, edge_config), true);
                    let tracking = engine.tracking();
                    if tracking {
                        self.capture.push(Vec::new());
                    }
                    self.view_artifacts = None;
                    let view = Engine::dependency_view_in(self, &reads, label, edge_config);
                    let accessed = if tracking {
                        let mut accessed = self.capture.pop().unwrap_or_default();
                        if let Some(outer) = self.capture.last_mut() {
                            outer.extend(accessed.iter().cloned());
                        }
                        let mut seen = HashSet::new();
                        accessed.retain(|label| seen.insert(label.clone()));
                        Some(Arc::<[Label]>::from(accessed))
                    } else {
                        None
                    };
                    let view = view?;
                    Ok::<_, String>(Some(ViewFill {
                        view,
                        artifacts: self.view_artifacts.take(),
                        accesses: accessed,
                    }))
                })?;
                let Some(filled) = filled else {
                    return Err("internal error: a dependency view was not filled".to_owned());
                };
                if filled_here {
                    // The miss path registered and looked up for itself.
                    return Ok(filled.view.clone());
                }
                (*filled).clone()
            }
        };
        crate::plan_timings::note_dependency_view(&(label, edge_config), false);
        if verify_views() {
            let rebuilt = Engine::dependency_view_in(self, &reads, label, edge_config);
            verify_view(&key, &fill.view, rebuilt);
        }
        // What the miss path registered is visible from this turn on (as
        // a hit ahead of its turn on the coordinator).
        if let Some(artifacts) = &fill.artifacts {
            for artifact in artifacts.iter() {
                register_artifact(&self.index, &artifact.file, artifact);
            }
        }
        if let Some(accessed) = &fill.accesses {
            for label in accessed.iter() {
                self.access(label);
            }
        }
        Ok(fill.view)
    }
}

impl SharedPrepare<'_> {
    /// [`Engine::target`]'s source-file path for a declared source of a
    /// loaded package not memoized yet (`None`: only the coordinator can
    /// answer — not declared, not loaded, or its lookup recorded
    /// something only an ordered lookup may).
    fn source(&mut self, label: &Label) -> Option<Result<Target, String>> {
        if let Some(path) = self.resolved.get(label) {
            return Some(Ok(Target::Source(path.clone())));
        }
        let package_id = label.package_id();
        let package = self.engine.packages.get(&package_id)?;
        let declared = package.exports.contains_key(label)
            || (!package.macro_names.contains(&label.name)
                && self
                    .inputs
                    .entry(package_id.clone())
                    .or_insert_with(|| package_input_labels_in(&self.engine.repositories, package))
                    .contains(label));
        if !declared {
            return None;
        }
        let mut side = SourceSide::default();
        let mut probes = ReadThrough {
            base: &self.engine.source_dir_probes,
            own: &mut self.probes,
        };
        let repositories = &self.engine.repositories;
        let policies = &mut self.link_policies;
        let found = source_target_with(repositories, label, &mut probes, &mut side, &mut |name| {
            policies
                .entry(name.to_owned())
                .or_insert_with(|| repositories.link_policy(name))
                .clone()
        });
        let (path, stable) = found.ok()?;
        // Once per job, a symlinked source too (the same policy: the same
        // answer; the merge memoizes only a stable one).
        self.resolved.insert(label.clone(), path.clone());
        self.record
            .sources
            .push((label.clone(), path.clone(), stable, side));
        Some(Ok(Target::Source(path)))
    }
}

impl Lookup for SharedPrepare<'_> {
    fn engine(&self) -> &Engine {
        self.engine
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.access(label);
        match self.engine.target_readonly(label) {
            Some(target) => target,
            None if self.resolve_sources => match self.source(label) {
                Some(target) => target,
                None => Err(self.abort("target_lookup")),
            },
            None => Err(self.abort("target_lookup")),
        }
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        let id = crate::config_registry::no_config().id;
        if self.engine.configurations.is_interned(id) {
            Ok(id)
        } else {
            Err(self.abort("no_config"))
        }
    }

    fn attribute_aspects(
        &mut self,
        _rule: &Rule,
        _identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        Err(self.abort("attribute_aspects"))
    }
}

impl ViewCx for SharedPrepare<'_> {
    fn ahead(&self) -> bool {
        true
    }

    fn register(&mut self, file: &File, artifact: &Artifact) {
        register_artifact(&self.index, file, artifact);
    }

    fn note_view_artifacts(&mut self, _key: ConfiguredTargetKey, artifacts: Artifacts) {
        self.view_artifacts = Some(artifacts);
    }

    fn native_output_group(
        &mut self,
        key: &ConfiguredTargetKey,
        fill: &mut dyn FnMut(&Engine) -> Result<starlark::values::OwnedFrozenValue, String>,
    ) -> Result<starlark::values::OwnedFrozenValue, String> {
        if let Some(value) = self.output_groups.get(key) {
            return Ok(value.clone());
        }
        let engine = self.engine;
        self.memos
            .output_groups
            .get_or_fill(key, || fill(engine).map(Some))?
            .map(|value| (*value).clone())
            .ok_or_else(|| "internal error: an output group was not filled".to_owned())
    }

    fn identity(
        &mut self,
        key: TargetIdentityKey,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target> {
        if let Some(identity) = self.identities.get(&key) {
            return identity.clone();
        }
        match self
            .memos
            .identities
            .get_or_fill(&key, || Ok::<_, std::convert::Infallible>(Some(fill())))
        {
            Ok(Some(identity)) => (*identity).clone(),
            _ => fill(),
        }
    }
}

impl PrepareCx for SharedPrepare<'_> {
    fn prerequisite_errors(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        exec_config: ConfigId,
        targets: &BTreeMap<String, DependencyTarget>,
    ) -> Result<(), String> {
        let configured = self.configured;
        Engine::starlark_prerequisite_errors(self, rule, key, exec_config, configured, targets)
    }

    fn reads(&self) -> &dyn PlanReads {
        self.reads
    }

    fn note_owner(
        &mut self,
        action_owner: &str,
        owner: crate::shared_path::SharedPath,
        key: &ConfiguredTargetKey,
    ) {
        self.record.owner = Some((action_owner.to_owned(), owner, key.clone()));
    }

    fn edge_view(
        &mut self,
        _rule: &Rule,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<DependencyTarget, String> {
        if !aspects.is_empty() {
            return Err(self.abort("attribute_aspects"));
        }
        self.plan_view(label, edge_config)
    }
}

impl Engine {
    /// [`Engine::target`] without loading, memoizing or tracking: `None`
    /// when only the coordinator can answer (a package not loaded, a source
    /// file not memoized, no such target).
    pub(super) fn target_readonly(&self, label: &Label) -> Option<Result<Target, String>> {
        let hash = {
            use std::hash::BuildHasher;
            crate::fast_hash::FxBuildHasher::default().hash_one(label)
        };
        if let Some(rule) = self.rule_targets.get(&hash)
            && rule.label == *label
        {
            return Some(Ok(Target::Rule(rule.clone())));
        }
        if let Some(path) = self.source_targets.get(label) {
            return Some(Ok(Target::Source(path.clone())));
        }
        let package_id = label.package_id();
        let package = self.packages.get(&package_id)?;
        if let Some(rule) = package.get(label) {
            return Some(Ok(Target::Rule(rule.clone())));
        }
        if let Some((owner, output)) = self.generated.get(label) {
            let rule = self.packages.get(&owner.package_id())?.get(owner)?;
            return Some(Ok(Target::Output(rule.clone(), output.clone())));
        }
        None
    }

    /// Move a batch's memo fills into the planning state (every one a key
    /// the planning state did not hold: the batch filled only misses).
    pub(super) fn apply_batch_memos(&mut self, state: &mut ActionPlanning, memos: BatchMemos) {
        self.apply_memos(state, memos, true);
    }

    /// [`Self::apply_batch_memos`]; `ahead`: the fills were made ahead of
    /// their turn within a walk that goes on (their artifacts are
    /// registered again by a hit at an earlier index); a frozen walk that
    /// ended merged every entry: they are registered for good.
    pub(super) fn apply_memos(
        &mut self,
        state: &mut ActionPlanning,
        memos: BatchMemos,
        ahead: bool,
    ) {
        let BatchMemos {
            views,
            identities,
            output_groups,
        } = memos;
        let tracking = self.tracking();
        for (key, fill) in views.into_filled() {
            let ViewFill {
                view,
                artifacts,
                accesses,
            } = (*fill).clone();
            if ahead && let Some(artifacts) = artifacts {
                self.view_artifacts.insert(key.clone(), artifacts);
            }
            if tracking && let Some(accesses) = accesses {
                self.view_accesses.insert(key.clone(), accesses);
            }
            state.views.insert(key, view);
        }
        for (key, identity) in identities.into_filled() {
            state
                .target_identities
                .entry(key)
                .or_insert_with(|| (*identity).clone());
        }
        for (key, value) in output_groups.into_filled() {
            state
                .native_output_groups
                .entry(key)
                .or_insert_with(|| (*value).clone());
        }
    }
}
