//! Task B (docs/EVALUATOR.md, "Genuine parity, task B"): a Starlark rule
//! with attribute aspects (rules_rust's clippy and rustfmt, cc_binary's
//! and cc_proto_library's aspects) is a worker job like any rule's, its
//! aspect applications planned and analyzed on the workers.
//!
//! - **On the worker** (`BAZEL_RS_T3_ASPECT_JOBS`): the rule's prepare
//!   plans its aspect-applied edges with the coordinator's recursion
//!   (`aspect_plan`) over the frozen view ([`FrozenAspects`]): the
//!   dependency views as any worker's (`SharedPrepare::plan_view`), the
//!   aspect memo as the walk froze it, and every application analyzed once
//!   for the whole walk ([`AspectTable`], single-flight: whichever job
//!   needs it first). An application's entry keeps its providers, its
//!   analysis, its prints and owners, and the applications its own
//!   dependencies met ("subs", in the recursion's order). Nothing is
//!   committed. A dependency whose results are not published yet ends the
//!   job: it is queued again once they are (`Board::requeue`).
//! - **At the rule's merge** (its first dependent in plan order is where
//!   the sequential walk commits each application it is the first to
//!   meet): [`Engine::place_aspects`] walks the applications the job met,
//!   in the recursion's order, each one's subs first, and commits those not
//!   in the aspect memo yet — prints, owners, outputs and actions
//!   (`aspect_plan::commit_application`, the sequential code), the memo.
//!   The rule's own commit follows. An application that could not be used
//!   (failed, panicked, had effects) or an input not registered yet ends
//!   the placement: the rule is prepared at its turn from there (what was
//!   placed is a prefix of the sequential commits; the coordinator's walk
//!   takes the analyses from the table).
//! - **One value per key**: the aspect-configured targets' shared `Target`
//!   values are filled once per walk (single-flight); the rule's merge
//!   checks each against the planning state's (the first in plan order
//!   wins: a different one prepares the rule at its turn).
//!   `BAZEL_RS_T3_ASPECT_JOBS=verify` runs the jobs and then prepares every
//!   such rule at its turn (with `BAZEL_RS_VERIFY_DELTA=1`: each worker
//!   analysis is compared with the coordinator's inputs').

use super::*;
use crate::aspects::AspectApplicationKey;
use crate::engine::aspect_plan::{self, AspectCommit, AspectCx, AspectIdentityKey, Planned};

/// A dependency's results are not published yet (the job is queued again).
pub(in crate::engine) const UNREADY: &str = "\u{1}aspect-job-unready";

/// `BAZEL_RS_T3_ASPECT_JOBS`: on by default; `0` off (the prefetch at the
/// rule's turn, `parallel::aspects`), `verify` (jobs, then every such rule
/// at its turn).
fn mode() -> u8 {
    static MODE: std::sync::LazyLock<u8> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        match std::env::var("BAZEL_RS_T3_ASPECT_JOBS").as_deref() {
            Ok("0") => 0,
            Ok("verify") => 2,
            _ => 1,
        }
    });
    *MODE
}

pub(in crate::engine) fn enabled() -> bool {
    mode() != 0 && !order_audit::enabled()
}

/// `BAZEL_RS_T3_ASPECT_JOBS=verify`.
pub(super) fn verify_mode() -> bool {
    mode() == 2
}

/// BAZEL_RS_TIMING (`PERF-PARALLEL aspect_jobs`).
static STATS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const STAT_NAMES: [&str; 14] = [
    "analyzed",
    "unusable",
    "requeued",
    "placed",
    "placed_rules",
    "fallbacks",
    "identity_mismatch",
    "coordinator_hits",
    "coordinator_fills",
    "place_missing",
    "place_unusable",
    "place_inputs",
    "place_taken",
    "fallback_prepare",
];
pub(super) const S_FALLBACK_PREPARE: usize = 13;
pub(super) const S_REQUEUED: usize = 2;
pub(super) const S_FALLBACK: usize = 5;

pub(super) fn stat(index: usize, by: u64) {
    STATS[index].fetch_add(by, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn report() {
    if STATS
        .iter()
        .all(|value| value.load(std::sync::atomic::Ordering::Relaxed) == 0)
    {
        return;
    }
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL aspect_jobs {}",
        STAT_NAMES
            .iter()
            .zip(&STATS)
            .map(|(name, value)| format!(
                "{name}={}",
                value.load(std::sync::atomic::Ordering::Relaxed)
            ))
            .collect::<Vec<_>>()
            .join(" ")
    ));
}

/// One application of a frozen walk: analyzed once, committed at its first
/// dependent's merge.
pub(in crate::engine) struct Applied {
    providers: AspectProviders,
    /// The applications its dependencies met, in the recursion's order
    /// (committed before it).
    subs: Vec<AspectApplicationKey>,
    kind: AppliedKind,
}

enum AppliedKind {
    /// No implementation (a validation or non-applicable aspect): its
    /// providers only.
    Trivial,
    Analyzed {
        commit: Option<Box<AspectCommit>>,
        owner: Option<(
            String,
            crate::shared_path::SharedPath,
            Vec<crate::aquery::Aspect>,
        )>,
        /// Taken by its commit.
        analysis: std::sync::Mutex<Option<Box<crate::analysis::AnalysisResult>>>,
        printed: Vec<String>,
    },
    /// Failed, panicked or had effects: the coordinator analyzes it.
    Unusable,
}

/// The applications and aspect-configured targets of a frozen walk.
#[derive(Default)]
pub(in crate::engine) struct AspectTable {
    applications: delta::SingleFlight<AspectApplicationKey, Applied>,
    identities: delta::SingleFlight<AspectIdentityKey, Arc<crate::starlark_types::Target>>,
    /// The planning state's as the walk froze them.
    memo: BTreeMap<AspectApplicationKey, AspectProviders>,
    memo_identities: AspectTargetIdentities,
    /// Phase 1's aspect tables the walk's pass-2 ops publish, each with
    /// the plan position before whose turn it applies (an entry at or
    /// after it sees it, as at its turn; `Frozen::platform_at`).
    predicates: HashMap<(Label, String), (usize, bool)>,
    attr_aspects: HashMap<(ConfiguredTargetKey, String), (usize, AspectAttrAspects)>,
    toolchains: HashMap<(ConfigId, String), (usize, Arc<AspectToolchains>)>,
    attribute_configs: HashMap<(ConfiguredTargetKey, String), (usize, AttributeConfigs)>,
}

type AttributeConfigs = Arc<BTreeMap<String, TransitionConfigurations>>;

type AspectAttrAspects = (Option<Arc<[String]>>, Option<Arc<[String]>>);

impl AspectTable {
    pub(in crate::engine) fn new(engine: &Engine, state: &ActionPlanning) -> Self {
        if !enabled() {
            return Self::default();
        }
        let mut table = Self {
            memo: state.aspect_providers.clone(),
            memo_identities: state.aspect_target_identities.clone(),
            ..Self::default()
        };
        use crate::engine::eval::linearize::driver::Published;
        use crate::engine::eval::linearize::pass2::Op;
        let Some(pending) = &engine.eval_pass2 else {
            return table;
        };
        for (position, op) in &pending.ops {
            let Op::Publish(published) = op else {
                continue;
            };
            let position = *position;
            match published {
                Published::AspectPredicate(memo, value) => {
                    table
                        .predicates
                        .entry(memo.clone())
                        .or_insert((position, *value));
                }
                Published::AspectAttrAspects(memo, value) => {
                    table
                        .attr_aspects
                        .entry(memo.clone())
                        .or_insert_with(|| (position, value.clone()));
                }
                Published::AspectToolchains(memo, value) => {
                    table
                        .toolchains
                        .entry(memo.clone())
                        .or_insert_with(|| (position, Arc::clone(value)));
                }
                Published::AspectAttributeConfigs(memo, value) => {
                    table
                        .attribute_configs
                        .entry(memo.clone())
                        .or_insert_with(|| (position, Arc::clone(value)));
                }
                _ => {}
            }
        }
        table
    }
}

/// What a job's aspect planning met, for its merge.
#[derive(Default)]
pub(in crate::engine) struct AspectRecord {
    /// The applications of the rule's aspect-applied edges, in order.
    placements: Vec<AspectApplicationKey>,
    /// The shared `Target` values it filled or took from the walk's table.
    identities: Vec<(AspectIdentityKey, Arc<crate::starlark_types::Target>)>,
    /// A dependency whose results were not published.
    pub(super) unready: Option<ConfiguredTargetKey>,
    /// The source files its lookups resolved (Engine::target's records,
    /// applied at its merge).
    pub(super) sources: Vec<(Label, PathBuf, bool, SourceSide)>,
    /// The dependencies walked (one result per job).
    walked: HashMap<(Label, ConfigId, Vec<String>), (DependencyTarget, AspectApplications)>,
}

/// The aspect planning of a worker's prepare over the frozen view.
struct FrozenAspects<'p, 'a> {
    prepare: &'p mut prepare::SharedPrepare<'a>,
    frozen: &'a Frozen,
    /// The job's plan index.
    index: usize,
    /// The subs of the applications being planned (innermost last).
    frames: Vec<Vec<AspectApplicationKey>>,
    owner: Option<(
        String,
        crate::shared_path::SharedPath,
        Vec<crate::aquery::Aspect>,
    )>,
    /// A toolchain read whose results were not published (reported by the
    /// application's analysis: the reads' answers are not the walk's).
    missed: std::cell::RefCell<Option<ConfiguredTargetKey>>,
}

impl FrozenAspects<'_, '_> {
    fn record(&mut self) -> &mut AspectRecord {
        self.prepare
            .record
            .aspects
            .get_or_insert_with(Default::default)
    }

    fn unready(&mut self, key: ConfiguredTargetKey) -> String {
        self.record().unready.get_or_insert(key);
        UNREADY.to_owned()
    }

    /// The results `label`'s view reads, if they are not published yet.
    fn missing(&self, label: &Label, config: ConfigId) -> Option<ConfiguredTargetKey> {
        let engine = self.prepare.engine;
        let package = engine.packages.get(&label.package_id())?;
        let key = if package.get(label).is_some() {
            ConfiguredTargetKey::new(label.clone(), config)
        } else {
            let (owner, _) = engine.generated.get(label)?;
            ConfiguredTargetKey::new(owner.clone(), config)
        };
        self.frozen.results.get(&key).is_none().then_some(key)
    }
}

impl prepare::Lookup for FrozenAspects<'_, '_> {
    fn engine(&self) -> &Engine {
        self.prepare.engine
    }

    fn platform(&self, target: &ConfiguredTargetKey) -> Option<PlatformResult> {
        prepare::Lookup::platform(&*self.prepare, target)
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        prepare::Lookup::target(self.prepare, label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        prepare::Lookup::no_config(self.prepare)
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        prepare::Lookup::attribute_aspects(self.prepare, rule, identities)
    }
}

/// Engine::attribute_aspects over the definitions loaded already (`None`:
/// one is not).
/// A Starlark rule with attribute aspects (`attr.*(aspects = [...])`).
pub(super) fn has_attribute_aspects(rule: &Rule) -> bool {
    rule.custom.as_ref().is_some_and(|custom| {
        custom
            .schema
            .values()
            .any(|spec| !spec.declaration.aspects.is_empty())
    })
}

pub(in crate::engine) fn loaded_attribute_aspects(
    engine: &Engine,
    rule: &Rule,
    identities: &[String],
) -> Option<Vec<Arc<crate::aspects::AspectDefinition>>> {
    identities
        .iter()
        .map(|identity| {
            let definition = engine.aspects.get(identity)?;
            Some(match definition.for_attribute_of(rule) {
                Some(parameterized) => Arc::new(parameterized),
                None => definition,
            })
        })
        .collect()
}

/// Every PlanReads read, noting a key whose results are not published.
struct MissReads<'r> {
    reads: &'r dyn PlanReads,
    missed: std::sync::Mutex<Option<ConfiguredTargetKey>>,
}

impl MissReads<'_> {
    fn note(&self, key: &ConfiguredTargetKey) {
        if self.reads.providers(key).is_none() {
            self.missed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| key.clone());
        }
    }
}

impl PlanReads for MissReads<'_> {
    fn dry(&self, key: &ConfiguredTargetKey) -> Option<Artifacts> {
        self.reads.dry(key)
    }
    fn providers(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<BTreeMap<String, starlark::values::OwnedFrozenValue>> {
        self.note(key);
        self.reads.providers(key)
    }
    fn provider(
        &self,
        key: &ConfiguredTargetKey,
        id: &str,
    ) -> Option<starlark::values::OwnedFrozenValue> {
        self.note(key);
        self.reads.provider(key, id)
    }
    fn provider_keys(&self, key: &ConfiguredTargetKey) -> Option<Vec<String>> {
        self.reads.provider_keys(key)
    }
    fn runfiles_info(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<Arc<crate::runfiles::TargetRunfiles>> {
        self.reads.runfiles_info(key)
    }
    fn declared(&self, config: ConfigId, path: &Path) -> Option<Artifact> {
        self.reads.declared(config, path)
    }
    fn validation(&self, key: &ConfiguredTargetKey) -> Option<crate::starlark_types::Depset> {
        self.reads.validation(key)
    }
    fn repositories(&self, key: &ConfiguredTargetKey) -> Option<Arc<BTreeSet<String>>> {
        self.reads.repositories(key)
    }
}

impl AspectCx for FrozenAspects<'_, '_> {
    fn configured(&self) -> &ConfiguredNodes {
        self.prepare.configured
    }

    fn view(&mut self, label: &Label, edge_config: ConfigId) -> Result<DependencyTarget, String> {
        if let Some(key) = self.missing(label, edge_config) {
            return Err(self.unready(key));
        }
        self.prepare.plan_view(label, edge_config)
    }

    fn collection_for(
        &mut self,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        let engine = self.prepare.engine;
        let (table, index) = (&self.frozen.aspects, self.index);
        let mut missed = false;
        let collection = crate::aspects::aspect_collection(aspects, rule, &mut |aspect| {
            let memo = (rule.label.clone(), aspect.descriptor.clone());
            let scheduled = table
                .predicates
                .get(&memo)
                .filter(|(position, _)| *position <= index)
                .map(|(_, value)| *value);
            match engine.aspect_predicates.get(&memo).copied().or(scheduled) {
                Some(result) => Ok(result),
                // Phase 1's memo is the evaluator's: computed here (a
                // function of the aspect and the rule), an error left to
                // the coordinator.
                None => {
                    crate::aspect_propagation::propagation_predicate(aspect, rule).map_err(|_| {
                        missed = true;
                        prepare::SHARED_ABORT.to_owned()
                    })
                }
            }
        });
        if missed {
            return Err(self.prepare.abort("aspect_predicate"));
        }
        collection
    }

    fn computed_attr_aspects(
        &mut self,
        mut collection: crate::aspects::AspectCollection,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        let engine = self.prepare.engine;
        for (aspect, _) in &mut collection.used {
            if aspect.declaration.attr_aspects.is_some()
                && aspect.declaration.toolchains_aspects.is_some()
            {
                continue;
            }
            let memo = (key.clone(), aspect.descriptor.clone());
            let table = &self.frozen.aspects;
            let scheduled = || {
                table
                    .attr_aspects
                    .get(&memo)
                    .filter(|(position, _)| *position <= self.index)
                    .map(|(_, value)| value.clone())
            };
            let computed = engine
                .aspect_attr_aspects
                .get(&memo)
                .cloned()
                .or_else(scheduled)
                .or_else(|| {
                    Some((
                        crate::aspect_propagation::computed_attr_aspects(aspect, rule)
                            .ok()?
                            .map(Arc::from),
                        crate::aspect_propagation::computed_toolchains_aspects(aspect, rule)
                            .ok()?
                            .map(Arc::from),
                    ))
                });
            let Some((attr_aspects, toolchains_aspects)) = computed else {
                return Err(self.prepare.abort("aspect_attr_aspects"));
            };
            let mut definition = aspect.as_ref().clone();
            definition.computed_attr_aspects = attr_aspects;
            definition.computed_toolchains_aspects = toolchains_aspects;
            *aspect = Arc::new(definition);
        }
        Ok(collection)
    }

    fn attribute_configs(
        &self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<Arc<BTreeMap<String, TransitionConfigurations>>> {
        self.prepare
            .engine
            .aspect_attribute_configs
            .get(memo)
            .cloned()
            .or_else(|| {
                let (position, value) = self.frozen.aspects.attribute_configs.get(memo)?;
                (*position <= self.index).then(|| Arc::clone(value))
            })
    }

    fn aspect_toolchains(&self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>> {
        self.prepare
            .engine
            .aspect_toolchains
            .get(memo)
            .cloned()
            .or_else(|| {
                let (position, value) = self.frozen.aspects.toolchains.get(memo)?;
                (*position <= self.index).then(|| Arc::clone(value))
            })
    }

    fn walk_dependency(
        &mut self,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        walk: impl FnOnce(&mut Self, Vec<Arc<crate::aspects::AspectDefinition>>) -> Planned,
    ) -> Planned {
        if aspects.is_empty() {
            return walk(self, aspects);
        }
        let walked = (
            label.clone(),
            edge_config,
            aspects
                .iter()
                .map(|aspect| aspect.descriptor.clone())
                .collect::<Vec<_>>(),
        );
        let planned = match self.record().walked.get(&walked) {
            Some(planned) => planned.clone(),
            None => {
                let planned = walk(self, aspects)?;
                self.record().walked.insert(walked, planned.clone());
                planned
            }
        };
        let keys = planned.1.iter().map(|(key, _)| key.clone());
        match self.frames.last_mut() {
            Some(frame) => frame.extend(keys),
            None => {
                let keys: Vec<_> = keys.collect();
                self.record().placements.extend(keys);
            }
        }
        Ok(planned)
    }

    fn memo(&self, key: &AspectApplicationKey) -> Option<AspectProviders> {
        let table = &self.frozen.aspects;
        if let Some(providers) = table.memo.get(key) {
            return Some(providers.clone());
        }
        let applied = table.applications.get(key)?;
        match applied.kind {
            AppliedKind::Unusable => None,
            _ => Some(applied.providers.clone()),
        }
    }

    fn applying(&mut self, _key: &AspectApplicationKey) {}

    fn applied(
        &mut self,
        key: &AspectApplicationKey,
        providers: AspectProviders,
    ) -> AspectProviders {
        // A validation or non-applicable aspect's providers (no analysis):
        // one value for the walk too.
        let filled = self.frozen.aspects.applications.get_or_fill(key, || {
            Ok::<_, std::convert::Infallible>(Some(Applied {
                providers: providers.clone(),
                subs: Vec::new(),
                kind: AppliedKind::Trivial,
            }))
        });
        match filled {
            Ok(Some(applied)) => applied.providers.clone(),
            _ => providers,
        }
    }

    fn begin_application(&mut self) {
        self.frames.push(Vec::new());
    }

    fn end_application(&mut self) {
        self.frames.pop();
    }

    fn identity(
        &mut self,
        key: AspectIdentityKey,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target> {
        if let Some(identity) = self.frozen.aspects.memo_identities.get(&key) {
            return identity.clone();
        }
        let identity = match self
            .frozen
            .aspects
            .identities
            .get_or_fill(&key, || Ok::<_, std::convert::Infallible>(Some(fill())))
        {
            Ok(Some(identity)) => (*identity).clone(),
            _ => fill(),
        };
        self.record().identities.push((key, identity.clone()));
        identity
    }

    fn with_reads<R>(&self, f: impl FnOnce(&dyn PlanReads) -> R) -> R {
        let reads = MissReads {
            reads: prepare::PrepareCx::reads(&*self.prepare),
            missed: Default::default(),
        };
        let result = f(&reads);
        if let Some(key) = reads.missed.into_inner().unwrap_or_else(|e| e.into_inner()) {
            self.missed.borrow_mut().get_or_insert(key);
        }
        result
    }

    fn note_owner(
        &mut self,
        action_owner: &str,
        console: crate::shared_path::SharedPath,
        _base_key: &ConfiguredTargetKey,
        aspects: &dyn Fn() -> Vec<crate::aquery::Aspect>,
    ) {
        self.owner = Some((action_owner.to_owned(), console, aspects()));
    }

    fn analyze(
        &mut self,
        key: &AspectApplicationKey,
        application: crate::analysis::AspectApplication,
        commit: AspectCommit,
    ) -> Result<AspectProviders, String> {
        let missed = self.missed.borrow_mut().take();
        if let Some(missed) = missed {
            return Err(self.unready(missed));
        }
        let subs = self.frames.last().cloned().unwrap_or_default();
        let owner = self.owner.take();
        let filled = self.frozen.aspects.applications.get_or_fill(key, || {
            let outcome = super::aspects::analyze_one(key, &application, &commit.outputs());
            Ok::<_, std::convert::Infallible>(Some(applied_of(
                outcome,
                subs,
                Some(Box::new(commit)),
                owner,
            )))
        });
        match filled {
            Ok(Some(applied)) if !matches!(applied.kind, AppliedKind::Unusable) => {
                Ok(applied.providers.clone())
            }
            _ => Err(self.prepare.abort("aspect_unusable")),
        }
    }
}

fn applied_of(
    outcome: super::aspects::Outcome,
    subs: Vec<AspectApplicationKey>,
    commit: Option<Box<AspectCommit>>,
    owner: Option<(
        String,
        crate::shared_path::SharedPath,
        Vec<crate::aquery::Aspect>,
    )>,
) -> Applied {
    match outcome {
        super::aspects::Outcome::Analyzed { analyzed, printed } => {
            stat(0, 1);
            Applied {
                providers: analyzed.providers.clone(),
                subs,
                kind: AppliedKind::Analyzed {
                    commit,
                    owner,
                    analysis: std::sync::Mutex::new(Some(analyzed)),
                    printed,
                },
            }
        }
        super::aspects::Outcome::Unusable => {
            stat(1, 1);
            Applied {
                providers: BTreeMap::new(),
                subs,
                kind: AppliedKind::Unusable,
            }
        }
    }
}

/// A worker's prepare meets an aspect-applied edge (SharedPrepare::edge_view).
pub(in crate::engine) fn edge_view(
    prepare: &mut prepare::SharedPrepare<'_>,
    rule: &Rule,
    label: &Label,
    edge_config: ConfigId,
    aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
) -> Result<DependencyTarget, String> {
    let Some((frozen, index)) = prepare.frozen_view() else {
        return Err(prepare.abort("attribute_aspects"));
    };
    let mut cx = FrozenAspects {
        prepare,
        frozen,
        index,
        frames: Vec::new(),
        owner: None,
        missed: Default::default(),
    };
    let planned = aspect_plan::plan_dependency(&mut cx, label, edge_config, aspects);
    let missed = cx.missed.borrow_mut().take();
    if let Some(missed) = missed {
        cx.unready(missed);
    }
    let prepare = cx.prepare;
    if prepare
        .record
        .aspects
        .as_ref()
        .is_some_and(|record| record.unready.is_some())
    {
        return Err(prepare.abort("aspect_unready"));
    }
    // MergedConfiguredTarget.of's MergingException is reported at the
    // consumer's location (MergedConfiguredTarget.java:210-213).
    planned
        .map(|(view, _)| view)
        .map_err(|error| located(rule.location.as_deref(), error))
}

impl Engine {
    /// The coordinator's analysis of an application in a frozen walk with
    /// aspect jobs: the walk's one analysis of it (a worker's, else made
    /// here, isolated); `None`: analyze it here as before (not such a walk,
    /// or its analysis is not usable).
    pub(in crate::engine) fn walk_aspect_analysis(
        &self,
        key: &AspectApplicationKey,
        application: &crate::analysis::AspectApplication,
        outputs: &crate::analysis::ConfiguredOutputs<'_>,
    ) -> Option<(Box<crate::analysis::AnalysisResult>, Vec<String>)> {
        if !enabled() {
            return None;
        }
        let frozen = self.frozen_walk.as_ref()?;
        let mut here = false;
        let filled = frozen
            .aspects
            .applications
            .get_or_fill(key, || {
                here = true;
                let outcome = super::aspects::analyze_one(key, application, outputs);
                Ok::<_, std::convert::Infallible>(Some(applied_of(outcome, Vec::new(), None, None)))
            })
            .ok()??;
        stat(if here { 8 } else { 7 }, 1);
        let AppliedKind::Analyzed {
            analysis, printed, ..
        } = &filled.kind
        else {
            return None;
        };
        let analyzed = analysis.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        if !here && delta::verify_delta() {
            super::aspects::verify(key, application, outputs, &analyzed, printed);
        }
        Some((analyzed, printed.clone()))
    }

    /// At an aspect job's merge: commit the applications it met that are
    /// not in the aspect memo, in the sequential order (module docs).
    /// `false`: one could not be (the rule is prepared at its turn).
    pub(super) fn place_aspects(
        &mut self,
        state: &mut ActionPlanning,
        rule: &Rule,
        record: &AspectRecord,
    ) -> Result<bool, String> {
        let Some(frozen) = self.frozen_walk.clone() else {
            return Ok(false);
        };
        // The source lookups its prepare made (Engine::target): what they
        // record, their memoized paths.
        for (label, path, stable, side) in &record.sources {
            side.clone()
                .apply(&mut self.missing_sources, &mut self.source_directories);
            if *stable {
                self.source_targets.insert(label.clone(), path.clone());
            }
        }
        for (key, identity) in &record.identities {
            if state
                .aspect_target_identities
                .get(key)
                .is_some_and(|known| !Arc::ptr_eq(known, identity))
            {
                stat(6, 1);
                return Ok(false);
            }
        }
        for (key, identity) in &record.identities {
            state
                .aspect_target_identities
                .entry(key.clone())
                .or_insert_with(|| identity.clone());
        }
        for key in &record.placements {
            // An error is the rule's prepare's, at its location
            // (CoordPrepare::edge_view).
            if !self
                .place_application(state, &frozen, key)
                .map_err(|error| located(rule.location.as_deref(), error))?
            {
                return Ok(false);
            }
        }
        stat(4, 1);
        Ok(true)
    }

    fn place_application(
        &mut self,
        state: &mut ActionPlanning,
        frozen: &Frozen,
        key: &AspectApplicationKey,
    ) -> Result<bool, String> {
        if state.aspect_providers.contains_key(key) {
            return Ok(true);
        }
        let Some(applied) = frozen.aspects.applications.get(key) else {
            stat(9, 1);
            return Ok(false);
        };
        for sub in &applied.subs {
            if !self.place_application(state, frozen, sub)? {
                return Ok(false);
            }
        }
        match &applied.kind {
            AppliedKind::Unusable => {
                stat(10, 1);
                return Ok(false);
            }
            AppliedKind::Trivial => {}
            AppliedKind::Analyzed {
                commit,
                owner,
                analysis,
                printed,
            } => {
                let (Some(commit), Some((action_owner, console, aspects))) = (commit, owner) else {
                    return Ok(false);
                };
                let mut slot = analysis.lock().unwrap_or_else(|e| e.into_inner());
                let Some(analyzed) = slot.take() else {
                    stat(12, 1);
                    return Ok(false);
                };
                // Every input of its actions resolves (its own outputs, or
                // registered already): else its commit is the
                // coordinator's, in order.
                let own: HashSet<&File> = analyzed
                    .actions
                    .iter()
                    .flat_map(|action| &action.outputs)
                    .collect();
                if !analyzed.actions.iter().all(|action| {
                    action
                        .inputs
                        .iter()
                        .all(|file| own.contains(file) || state.artifact_index.get(file).is_some())
                }) {
                    *slot = Some(analyzed);
                    stat(11, 1);
                    return Ok(false);
                }
                drop(slot);
                self.console_owners
                    .insert(action_owner.clone(), console.clone());
                self.aquery_owner(action_owner, &commit.base_key, || aspects.clone());
                let mut planning = state.plan_state();
                aspect_plan::commit_application(
                    self,
                    &mut planning,
                    commit,
                    Ok(*analyzed),
                    printed.clone(),
                )?;
            }
        }
        stat(3, 1);
        crate::progress::aspect_applied();
        state
            .aspect_providers
            .insert(key.clone(), applied.providers.clone());
        Ok(true)
    }
}
