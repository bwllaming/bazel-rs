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
use crate::engine::aspect_plan::{
    self, AspectCommit, AspectCx, AspectIdentityKey, Context, Planned,
};

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

/// `BAZEL_RS_T3_ASPECT_DELTAS=1`: an aspect job builds its rule's delta
/// too (merged after its applications' placement).
pub(super) fn worker_deltas() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var("BAZEL_RS_T3_ASPECT_DELTAS").is_ok_and(|value| value == "1")
    });
    *ON
}

/// `BAZEL_RS_T3_ASPECT_JOBS=verify`.
pub(super) fn verify_mode() -> bool {
    mode() == 2
}

/// BAZEL_RS_TIMING (`PERF-PARALLEL aspect_jobs`).
static STATS: [AtomicU64; 17] = [const { AtomicU64::new(0) }; 17];
const STAT_NAMES: [&str; 17] = [
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
    "place_range",
    "place_memo",
    "coordinator_rejects",
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

/// Aspect-configured `Target` values by key.
type Identities = Vec<(AspectIdentityKey, Arc<crate::starlark_types::Target>)>;

/// An application in its context (the walk's table key).
pub(in crate::engine) type Applying = (AspectApplicationKey, Context);

/// The plan positions `[from, to)` at whose turn what an analysis read is
/// the same (target platforms and phase 1's tables published by the
/// walk's pass-2 ops).
#[derive(Clone, Copy)]
struct Range(usize, usize);

impl Range {
    const ALL: Range = Range(0, usize::MAX);

    fn narrow(&mut self, from: usize, to: usize) {
        self.0 = self.0.max(from);
        self.1 = self.1.min(to);
    }

    fn contains(&self, index: usize) -> bool {
        self.0 <= index && index < self.1
    }
}

/// One application of a frozen walk in one context: analyzed once,
/// committed at its first dependent's merge.
pub(in crate::engine) struct Applied {
    providers: AspectProviders,
    /// The applications its dependencies met, in the recursion's order
    /// (committed before it).
    subs: Vec<Applying>,
    /// The aspect-configured `Target` values its dependencies' views hold.
    identities: Identities,
    /// Where its analysis holds (a placement elsewhere falls back).
    range: Range,
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
    applications: delta::SingleFlight<Applying, Applied>,
    /// By key and variant (the requested label, the merged providers'
    /// identities: `aspect_plan::identity_variant`).
    identities:
        delta::SingleFlight<(AspectIdentityKey, String), Arc<crate::starlark_types::Target>>,
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
    /// The plan index of each entry of the walk.
    positions: HashMap<ConfiguredTargetKey, usize>,
    /// The applications committed so far in this walk (by the merges, in
    /// plan order): what a later entry's job sees in the aspect memo, as at
    /// its turn.
    committed: std::sync::RwLock<HashMap<AspectApplicationKey, (AspectProviders, Context)>>,
}

type AttributeConfigs = Arc<BTreeMap<String, TransitionConfigurations>>;

type AspectAttrAspects = (Option<Arc<[String]>>, Option<Arc<[String]>>);

/// The value `scheduled` (published at a position) or `known` (before
/// the walk) as entry `index` sees it, with where that answer holds.
fn at_position<T: Clone>(
    known: Option<&T>,
    scheduled: Option<&(usize, T)>,
    index: usize,
) -> (Option<T>, usize, usize) {
    if let Some(value) = known {
        return (Some(value.clone()), 0, usize::MAX);
    }
    match scheduled {
        Some((position, value)) if *position <= index => {
            (Some(value.clone()), *position, usize::MAX)
        }
        Some((position, _)) => (None, 0, *position),
        None => (None, 0, usize::MAX),
    }
}

/// Whether two providers maps hold the very same values.
fn same_providers(a: &AspectProviders, b: &AspectProviders) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((ka, va), (kb, vb))| ka == kb && va.value().ptr_eq(vb.value()))
}

/// What a job's aspect planning met, for its merge.
#[derive(Default)]
pub(in crate::engine) struct AspectRecord {
    /// The applications of the rule's aspect-applied edges, in order.
    placements: Vec<Applying>,
    /// The shared `Target` values its views hold.
    identities: Identities,
    /// A dependency whose results were not published.
    pub(super) unready: Option<ConfiguredTargetKey>,
    /// The source files its lookups resolved (Engine::target's records,
    /// applied at its merge).
    pub(super) sources: Vec<(Label, PathBuf, bool, SourceSide)>,
    /// The dependencies walked (one result per job), with what each walk
    /// gave the frame that walked it.
    walked: HashMap<(Label, ConfigId, Vec<String>), (Planned, Walking)>,
}

/// What one application's planning met (its entry's subs, values, range).
#[derive(Default)]
struct Frame {
    subs: Vec<Applying>,
    identities: Identities,
    range: Option<Range>,
}

/// What walking one dependency gave the frame it was walked in (replayed
/// on the job's next walk of it).
#[derive(Clone, Default)]
struct Walking {
    /// The frame depth it was walked at.
    depth: usize,
    collected: Vec<Applying>,
    identities: Identities,
    range: Option<Range>,
}

#[derive(Default)]
struct Frames {
    frames: Vec<Frame>,
    walking: Vec<Walking>,
    missed: Option<ConfiguredTargetKey>,
    /// The last memo answer was a committed application's.
    committed_hit: bool,
}

impl Frames {
    /// A read whose answer holds at `[from, to)`: the innermost frame's
    /// (and the walks open at its depth).
    fn narrow(&mut self, from: usize, to: usize) {
        if (from, to) == (0, usize::MAX) {
            return;
        }
        let depth = self.frames.len();
        if let Some(frame) = self.frames.last_mut() {
            frame.range.get_or_insert(Range::ALL).narrow(from, to);
        }
        for walking in self.walking.iter_mut().rev() {
            if walking.depth != depth {
                break;
            }
            walking.range.get_or_insert(Range::ALL).narrow(from, to);
        }
    }
}

/// The aspect planning of a worker's prepare over the frozen view.
struct FrozenAspects<'p, 'a> {
    prepare: &'p mut prepare::SharedPrepare<'a>,
    frozen: &'a Frozen,
    /// The job's plan index.
    index: usize,
    state: std::cell::RefCell<Frames>,
    owner: Option<(
        String,
        crate::shared_path::SharedPath,
        Vec<crate::aquery::Aspect>,
    )>,
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

    fn narrow(&self, from: usize, to: usize) {
        self.state.borrow_mut().narrow(from, to);
    }

    /// The results `label`'s view reads, if they are not published yet.
    fn missing(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        let engine = self.prepare.engine;
        let Some(package) = engine.packages.get(&label.package_id()) else {
            return Ok(());
        };
        let key = if package.get(label).is_some() {
            ConfiguredTargetKey::new(label.clone(), config)
        } else if let Some((owner, _)) = engine.generated.get(label) {
            ConfiguredTargetKey::new(owner.clone(), config)
        } else {
            return Ok(());
        };
        if self.frozen.results.get(&key).is_some() {
            return Ok(());
        }
        // A dependency is earlier in the plan (a post-order).
        if self
            .frozen
            .aspects
            .positions
            .get(&key)
            .is_some_and(|position| *position >= self.index)
        {
            debug_assert!(
                false,
                "internal error: an aspect job reads a later entry {}",
                key.label
            );
            return Err(self.prepare.abort("aspect_later_dependency"));
        }
        Err(self.unready(key))
    }

    /// Note an application of the walk being collected.
    fn push_identity(&self, key: AspectIdentityKey, identity: Arc<crate::starlark_types::Target>) {
        let mut state = self.state.borrow_mut();
        if let Some(walking) = state.walking.last_mut() {
            walking.identities.push((key.clone(), identity.clone()));
        }
        if let Some(frame) = state.frames.last_mut() {
            frame.identities.push((key, identity));
        }
    }

    fn entry_of(&self) -> (Vec<Applying>, Identities, Range) {
        let state = self.state.borrow();
        match state.frames.last() {
            Some(frame) => (
                frame.subs.clone(),
                frame.identities.clone(),
                frame.range.unwrap_or(Range::ALL),
            ),
            None => (Vec::new(), Vec::new(), Range::ALL),
        }
    }
}

impl prepare::Lookup for FrozenAspects<'_, '_> {
    fn engine(&self) -> &Engine {
        self.prepare.engine
    }

    fn platform(&self, target: &ConfiguredTargetKey) -> Option<PlatformResult> {
        let (platform, from, to) = self.frozen.platform_range(target, self.index);
        self.narrow(from, to);
        platform
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
        if let Some(plan) = &engine.planning_plan {
            table.positions = plan
                .iter()
                .enumerate()
                .skip(state.done)
                .map(|(index, (key, _))| (key.clone(), index))
                .collect();
        }
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
    results: &'r PlannedResults,
    missed: std::sync::Mutex<Option<ConfiguredTargetKey>>,
}

impl MissReads<'_> {
    fn note(&self, key: &ConfiguredTargetKey) {
        if self.results.get(key).is_none() {
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
        self.missing(label, edge_config)?;
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
        let mut range = Range::ALL;
        let collection = crate::aspects::aspect_collection(aspects, rule, &mut |aspect| {
            let memo = (rule.label.clone(), aspect.descriptor.clone());
            let (value, from, to) = at_position(
                engine.aspect_predicates.get(&memo),
                table.predicates.get(&memo),
                index,
            );
            range.narrow(from, to);
            match value {
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
        self.narrow(range.0, range.1);
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
            let (known, from, to) = at_position(
                engine.aspect_attr_aspects.get(&memo),
                self.frozen.aspects.attr_aspects.get(&memo),
                self.index,
            );
            self.narrow(from, to);
            let computed = known.or_else(|| {
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
        let (value, from, to) = at_position(
            self.prepare.engine.aspect_attribute_configs.get(memo),
            self.frozen.aspects.attribute_configs.get(memo),
            self.index,
        );
        self.narrow(from, to);
        value
    }

    fn aspect_toolchains(&self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>> {
        let (value, from, to) = at_position(
            self.prepare.engine.aspect_toolchains.get(memo),
            self.frozen.aspects.toolchains.get(memo),
            self.index,
        );
        self.narrow(from, to);
        value
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
        let known = self.record().walked.get(&walked).cloned();
        let (planned, walking) = match known {
            Some(known) => known,
            None => {
                let depth = self.state.borrow().frames.len();
                self.state.borrow_mut().walking.push(Walking {
                    depth,
                    ..Walking::default()
                });
                let planned = walk(self, aspects);
                let walking = self.state.borrow_mut().walking.pop().unwrap_or_default();
                let planned = planned?;
                self.record()
                    .walked
                    .insert(walked, (Ok(planned.clone()), walking.clone()));
                (Ok(planned), walking)
            }
        };
        // What the walk gave its frame, again for this one.
        {
            let mut state = self.state.borrow_mut();
            if let Some(range) = walking.range {
                state.narrow(range.0, range.1);
            }
            if let Some(frame) = state.frames.last_mut() {
                frame.subs.extend(walking.collected.iter().cloned());
                frame.identities.extend(walking.identities.iter().cloned());
            }
            // An enclosing walk at the same depth replays it too.
            let depth = state.frames.len();
            if let Some(outer) = state.walking.last_mut()
                && outer.depth == depth
            {
                outer.identities.extend(walking.identities.iter().cloned());
            }
        }
        if self.state.borrow().frames.is_empty() {
            let record = self.record();
            record.placements.extend(walking.collected.iter().cloned());
            record.identities.extend(walking.identities.iter().cloned());
        }
        planned
    }

    fn memo(&self, key: &AspectApplicationKey, context: &Context) -> Option<AspectProviders> {
        self.state.borrow_mut().committed_hit = false;
        let table = &self.frozen.aspects;
        if let Some(providers) = table.memo.get(key) {
            return Some(providers.clone());
        }
        // Committed by an earlier entry's merge (jobs run for later
        // entries than the cursor's).
        if let Some(providers) = table
            .committed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
        {
            self.state.borrow_mut().committed_hit = true;
            return Some(providers.0.clone());
        }
        let applied = table.applications.get(&(key.clone(), context.clone()))?;
        match applied.kind {
            AppliedKind::Unusable => None,
            _ => Some(applied.providers.clone()),
        }
    }

    fn collected(&mut self, key: &AspectApplicationKey, context: &Context) {
        let committed = std::mem::take(&mut self.state.borrow_mut().committed_hit);
        if committed || self.frozen.aspects.memo.contains_key(key) {
            // Committed before the walk or by an earlier merge (a memo
            // hit wherever it is met later).
            return;
        }
        if let Some(walking) = self.state.borrow_mut().walking.last_mut() {
            walking.collected.push((key.clone(), context.clone()));
        }
    }

    fn applying(&mut self, _key: &AspectApplicationKey) {}

    fn applied(
        &mut self,
        key: &AspectApplicationKey,
        context: &Context,
        providers: AspectProviders,
    ) -> AspectProviders {
        self.state.borrow_mut().committed_hit = false;
        // A validation or non-applicable aspect's providers (no analysis):
        // one value for the walk too.
        let filled =
            self.frozen
                .aspects
                .applications
                .get_or_fill(&(key.clone(), context.clone()), || {
                    Ok::<_, std::convert::Infallible>(Some(Applied {
                        providers: providers.clone(),
                        subs: Vec::new(),
                        identities: Vec::new(),
                        range: Range::ALL,
                        kind: AppliedKind::Trivial,
                    }))
                });
        match filled {
            Ok(Some(applied)) => applied.providers.clone(),
            _ => providers,
        }
    }

    fn begin_application(&mut self) {
        self.state.borrow_mut().frames.push(Frame::default());
    }

    fn end_application(&mut self) {
        self.state.borrow_mut().frames.pop();
    }

    fn identity(
        &mut self,
        key: AspectIdentityKey,
        variant: &str,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target> {
        if let Some(identity) = self.frozen.aspects.memo_identities.get(&key) {
            return identity.clone();
        }
        let identity = match self
            .frozen
            .aspects
            .identities
            .get_or_fill(&(key.clone(), variant.to_owned()), || {
                Ok::<_, std::convert::Infallible>(Some(fill()))
            }) {
            Ok(Some(identity)) => (*identity).clone(),
            _ => fill(),
        };
        self.push_identity(key, identity.clone());
        identity
    }

    fn with_reads<R>(&self, f: impl FnOnce(&dyn PlanReads) -> R) -> R {
        let reads = MissReads {
            reads: prepare::PrepareCx::reads(&*self.prepare),
            results: &self.frozen.results,
            missed: Default::default(),
        };
        let result = f(&reads);
        if let Some(key) = reads.missed.into_inner().unwrap_or_else(|e| e.into_inner()) {
            // Reported by the application's analysis (the reads' answers
            // are not the walk's).
            self.state.borrow_mut().missed.get_or_insert(key);
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
        context: &Context,
        application: crate::analysis::AspectApplication,
        commit: AspectCommit,
    ) -> Result<AspectProviders, String> {
        let missed = self.state.borrow_mut().missed.take();
        if let Some(missed) = missed {
            return Err(self.unready(missed));
        }
        let (subs, identities, range) = self.entry_of();
        let owner = self.owner.take();
        let filled =
            self.frozen
                .aspects
                .applications
                .get_or_fill(&(key.clone(), context.clone()), || {
                    let outcome = super::aspects::analyze_one(key, &application, &commit.outputs());
                    Ok::<_, std::convert::Infallible>(Some(applied_of(
                        outcome,
                        subs,
                        identities,
                        range,
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
    subs: Vec<Applying>,
    identities: Identities,
    range: Range,
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
                identities,
                range,
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
                identities,
                range,
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
        state: Default::default(),
        owner: None,
    };
    let planned = aspect_plan::plan_dependency(&mut cx, label, edge_config, aspects);
    let missed = cx.state.borrow_mut().missed.take();
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
    /// The coordinator's aspect-configured `Target` value of `key` in its
    /// `variant` in a frozen walk with aspect jobs: the walk's (the one the
    /// jobs' analyses of the same view saw).
    pub(in crate::engine) fn walk_aspect_identity(
        &self,
        key: &AspectIdentityKey,
        variant: &str,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Option<Arc<crate::starlark_types::Target>> {
        if !enabled() {
            return None;
        }
        let frozen = self.frozen_walk.as_ref()?;
        frozen
            .aspects
            .identities
            .get_or_fill(&(key.clone(), variant.to_owned()), || {
                Ok::<_, std::convert::Infallible>(Some(fill()))
            })
            .ok()?
            .map(|identity| (*identity).clone())
    }

    /// The coordinator's analysis of an application in a frozen walk with
    /// aspect jobs: the walk's one analysis of it in its context (a
    /// worker's, else made here, isolated); `None`: analyze it here as
    /// before (not such a walk, its analysis is not usable, or does not
    /// hold at entry `index` with the planning state's `Target` values).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine) fn walk_aspect_analysis(
        &self,
        key: &AspectApplicationKey,
        context: &Context,
        application: &crate::analysis::AspectApplication,
        outputs: &crate::analysis::ConfiguredOutputs<'_>,
        index: usize,
        identities: &AspectTargetIdentities,
        memo: &BTreeMap<AspectApplicationKey, AspectProviders>,
    ) -> Option<(Box<crate::analysis::AnalysisResult>, Vec<String>)> {
        if !enabled() {
            return None;
        }
        let frozen = self.frozen_walk.as_ref()?;
        // (One no job analyzed is analyzed by the turn: its prefetch, or
        // here.)
        let filled = frozen
            .aspects
            .applications
            .get(&(key.clone(), context.clone()))?;
        let AppliedKind::Analyzed {
            analysis, printed, ..
        } = &filled.kind
        else {
            return None;
        };
        if !filled.range.contains(index)
                || filled.identities.iter().any(|(key, identity)| {
                    identities
                        .get(key)
                        .is_none_or(|known| !Arc::ptr_eq(known, identity))
                })
                // Its dependencies' applications as the planning state
                // committed them (the very providers its analysis saw).
                || filled.subs.iter().any(|sub| {
                    let committed = memo.get(&sub.0);
                    let seen = frozen.aspects.applications.get(sub);
                    match (committed, seen) {
                        (Some(committed), Some(seen)) => {
                            !same_providers(committed, &seen.providers)
                        }
                        _ => true,
                    }
                })
        {
            stat(16, 1);
            return None;
        }
        stat(7, 1);
        let analyzed = analysis.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        if delta::verify_delta() {
            super::aspects::verify(key, application, outputs, &analyzed, printed);
        }
        Some((analyzed, printed.clone()))
    }

    /// At an aspect job's merge (entry `index`): commit the applications
    /// it met that are not in the aspect memo, in the sequential order
    /// (module docs). `false`: one could not be (the rule is prepared at
    /// its turn).
    pub(super) fn place_aspects(
        &mut self,
        state: &mut ActionPlanning,
        rule: &Rule,
        index: usize,
        record: &AspectRecord,
    ) -> Result<bool, String> {
        let Some(frozen) = self.frozen_walk.clone() else {
            return Ok(false);
        };
        // The source lookups its prepare made (Engine::target): what they
        // record, their memoized paths. (Applied before the placement: a
        // commit error below stops the walk, unless --keep_going, whose
        // later lookups see them memoized either way.)
        for (label, path, stable, side) in &record.sources {
            side.clone()
                .apply(&mut self.missing_sources, &mut self.source_directories);
            if *stable {
                self.source_targets.insert(label.clone(), path.clone());
            }
        }
        // Its views' values: checked now, put in the planning state once
        // every application they merge is placed (the very providers).
        if Self::identities_differ(state, &record.identities) {
            return Ok(false);
        }
        for applying in &record.placements {
            // An error is the rule's prepare's, at its location
            // (CoordPrepare::edge_view).
            if !self
                .place_application(state, &frozen, index, applying)
                .map_err(|error| located(rule.location.as_deref(), error))?
            {
                return Ok(false);
            }
        }
        if !self.adopt_identities(state, &record.identities) {
            return Ok(false);
        }
        stat(4, 1);
        Ok(true)
    }

    fn identities_differ(state: &ActionPlanning, identities: &Identities) -> bool {
        let differ = identities.iter().any(|(key, identity)| {
            state
                .aspect_target_identities
                .get(key)
                .is_some_and(|known| !Arc::ptr_eq(known, identity))
        });
        if differ {
            stat(6, 1);
            if super::debug_t3() {
                for (key, identity) in identities {
                    if state
                        .aspect_target_identities
                        .get(key)
                        .is_some_and(|known| !Arc::ptr_eq(known, identity))
                    {
                        eprintln!("T3-DEBUG aspect identity differs: {key:?}");
                    }
                }
            }
        }
        differ
    }

    /// The `Target` values a placement's views hold: each the planning
    /// state's (the first in plan order), or put there now.
    fn adopt_identities(&self, state: &mut ActionPlanning, identities: &Identities) -> bool {
        if Self::identities_differ(state, identities) {
            return false;
        }
        for (key, identity) in identities {
            state
                .aspect_target_identities
                .entry(key.clone())
                .or_insert_with(|| identity.clone());
        }
        true
    }

    fn place_application(
        &mut self,
        state: &mut ActionPlanning,
        frozen: &Frozen,
        index: usize,
        applying: &Applying,
    ) -> Result<bool, String> {
        let key = &applying.0;
        let applied = frozen.aspects.applications.get(applying);
        if let Some(known) = state.aspect_providers.get(key) {
            // Committed already: the very providers the job's views hold
            // (another context's, or the coordinator's, are not).
            let committed = |known: &AspectProviders| {
                frozen
                    .aspects
                    .memo
                    .get(key)
                    .is_some_and(|frozen| same_providers(known, frozen))
                    || frozen
                        .aspects
                        .committed
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(key)
                        .is_some_and(|committed| same_providers(known, &committed.0))
            };
            let same = match &applied {
                Some(applied) => same_providers(known, &applied.providers),
                None => committed(known),
            };
            if !same {
                stat(15, 1);
                if super::debug_t3() {
                    eprintln!(
                        "T3-DEBUG aspect placement: {} {} on {} committed in another context than {:?} (entry {}): {:?}",
                        index,
                        key.aspect,
                        key.base.label,
                        applying.1,
                        applied.is_some(),
                        frozen
                            .aspects
                            .committed
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(key)
                            .map(|committed| committed.1.clone())
                    );
                }
            }
            return Ok(same);
        }
        let Some(applied) = applied else {
            stat(9, 1);
            return Ok(false);
        };
        if !applied.range.contains(index) {
            stat(14, 1);
            return Ok(false);
        }
        for sub in &applied.subs {
            if !self.place_application(state, frozen, index, sub)? {
                return Ok(false);
            }
        }
        if !self.adopt_identities(state, &applied.identities) {
            return Ok(false);
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
        frozen
            .aspects
            .committed
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), (applied.providers.clone(), applying.1.clone()));
        Ok(true)
    }
}
