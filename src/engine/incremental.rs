//! Dependency tracking for incremental analysis (docs/INCREMENTAL_ANALYSIS.md,
//! tier A): while a full analysis that the analysis snapshot saves runs, the
//! engine records which configured targets reached which (every
//! `analyze_key` call, memo hits included, before the memo check), which
//! packages each configured target's own analysis looked up, and which
//! packages and targets were reached from computations whose result is
//! shared by many targets (toolchain resolution, platforms, transitions,
//! package groups, ...) or from outside any target (option parsing, the
//! top-level setup). The snapshot keeps the result
//! ([`crate::snapshot::incremental::Index`]); a later run whose recorded
//! inputs changed only inside some packages re-analyzes the requested
//! targets that reach those packages and splices the result into the saved
//! plan (crate::snapshot::incremental).
//!
//! The rules that keep this sound (a changed package that is not
//! attributable makes the next run fall back to the full analysis):
//!
//! * A target looked up (`Engine::target`) while a configured target's
//!   analysis or planning is the innermost frame counts as that target's
//!   access; while any shared computation runs ([`SharedScope`], a
//!   thread-local depth) the package is *global*; with no frame at all it
//!   is global too, except for the requested labels themselves (their own
//!   package: they are dirty whenever it is) and the target pattern phase
//!   ([`PatternScope`]: the package is *pattern-loaded*, its pattern summary
//!   must not change).
//! * A configured key reached inside a shared computation, or outside every
//!   frame (but the requested roots), is *shared*: if an edit dirties it, or
//!   anything it reaches, the run falls back.
//! * Edges are recorded before the memo check, so every consumer of a
//!   configured target is an edge, not only the first.

use super::*;
use crate::snapshot::incremental::{Fingerprint, Index, IndexKey, IndexPackage};
use std::cell::Cell;

thread_local! {
    static SHARED_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// The outermost shared computation (diagnostics).
    static SHARED_NAME: Cell<&'static str> = const { Cell::new("") };
    static PATTERN_DEPTH: Cell<u32> = const { Cell::new(0) };
    static SUITE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// The transcript names of the scopes that are not shared computations
/// (`Event::ScopeEnter`): a [`PatternScope`] and a [`SuiteScope`].
pub(super) const PATTERN_SCOPE: &str = "\u{0}pattern";
pub(super) const SUITE_SCOPE: &str = "\u{0}suite";

/// While alive, the packages the engine looks up are read by a test
/// suite's expansion in the target pattern phase.
pub(super) struct SuiteScope(());

impl SuiteScope {
    pub(super) fn enter() -> Self {
        super::eval::linearize::capture::scope_enter(SUITE_SCOPE);
        SUITE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for SuiteScope {
    fn drop(&mut self) {
        super::eval::linearize::capture::scope_exit();
        SUITE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// While alive, the engine's lookups belong to a computation whose result
/// many targets share (a memo not keyed by the configured target).
pub(super) struct SharedScope(());

impl SharedScope {
    pub(super) fn enter(name: &'static str) -> Self {
        super::eval::linearize::capture::scope_enter(name);
        SHARED_DEPTH.with(|depth| {
            if depth.get() == 0 {
                SHARED_NAME.with(|shared| shared.set(name));
            }
            depth.set(depth.get() + 1)
        });
        Self(())
    }
}

impl Drop for SharedScope {
    fn drop(&mut self) {
        super::eval::linearize::capture::scope_exit();
        SHARED_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// While alive, the engine's lookups belong to the target pattern phase.
pub(super) struct PatternScope(());

impl PatternScope {
    pub(super) fn enter() -> Self {
        super::eval::linearize::capture::scope_enter(PATTERN_SCOPE);
        PATTERN_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for PatternScope {
    fn drop(&mut self) {
        super::eval::linearize::capture::scope_exit();
        PATTERN_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn shared() -> bool {
    SHARED_DEPTH.with(Cell::get) > 0
}

fn shared_depth() -> u32 {
    SHARED_DEPTH.with(Cell::get)
}

fn pattern() -> bool {
    PATTERN_DEPTH.with(Cell::get) > 0
}

type KeyIdentity = (Label, Option<ConfigId>, bool);

/// The ids of the tracker's keys by their hash, the identities themselves
/// kept once (in `Tracker::keys`); the rare identity whose hash an earlier
/// one has is kept apart.
#[derive(Clone, Default)]
struct KeyIndex {
    by_hash: HashMap<u64, u32>,
    collided: HashMap<KeyIdentity, u32>,
}

impl KeyIndex {
    fn hash(identity: &KeyIdentity) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        identity.hash(&mut hasher);
        hasher.finish()
    }

    fn get(&self, keys: &[KeyIdentity], identity: &KeyIdentity) -> Option<u32> {
        if let Some(id) = self.by_hash.get(&Self::hash(identity))
            && keys.get(*id as usize) == Some(identity)
        {
            return Some(*id);
        }
        self.collided.get(identity).copied()
    }

    fn insert(&mut self, identity: &KeyIdentity, id: u32) {
        match self.by_hash.entry(Self::hash(identity)) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(id);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.collided.insert(identity.clone(), id);
            }
        }
    }
}

/// What one analysis records (see the module docs).
#[derive(Default)]
pub(super) struct Tracker {
    keys: Vec<KeyIdentity>,
    /// The id of each key (by its hash: the identities are kept once, in
    /// `keys`).
    key_ids: KeyIndex,
    /// The configured targets being analyzed or planned, innermost last,
    /// with the shared-computation depth their frame began at: a lookup
    /// in a frame is the frame's own unless a shared computation began
    /// inside it (a key analyzed by a shared computation, or a pending
    /// target planned while one runs, owns its lookups).
    frames: Vec<(u32, u32)>,
    edges: Vec<(u32, u32)>,
    edge_set: HashSet<(u32, u32)>,
    accesses: HashSet<(u32, u32)>,
    /// (key, package, target name): the main-repository targets each
    /// key's analysis looked up.
    label_accesses: HashSet<(u32, u32, String)>,
    packages: Vec<PackageRecord>,
    package_ids: HashMap<PackageId, u32>,
    shared_keys: BTreeSet<u32>,
    /// Keys whose analysis printed (`print()` in an implementation, an
    /// aspect or a transition while the key's frame was innermost).
    printed_keys: BTreeSet<u32>,
    /// (key, aspect application) requests.
    aspects: BTreeSet<(u32, String)>,
    roots: Vec<(u32, u32)>,
    requested: HashSet<Label>,
    /// (key, plan length when its analysis began, its plan position).
    planned: HashMap<u32, (u32, u32)>,
    /// Inside `analyze_requested`'s analysis of the requested label at
    /// this position (its first key is a root, not a shared one).
    root_analysis: Option<u32>,
    /// The root analysis reached its first key.
    root_reached: bool,
    /// The plan length when each requested label's analysis began (by
    /// position), and at the end of phase 1 (last).
    root_entries: Vec<u32>,
    /// (requested label position, `u32::MAX` for the workspace status;
    /// action id): the producers of the requested labels' hidden top-level
    /// artifacts (the plan's reachability roots besides the reported ones).
    demand: Vec<(u32, String)>,
    /// The target pattern phase's pattern being expanded (its position).
    pattern_position: Option<u32>,
    /// (pattern position, package) the wildcard patterns expanded.
    covered: BTreeSet<(u32, u32)>,
    /// The pattern phase was the plain `build` one (no tag filters, no
    /// tests): `Some(simple)` once it ran.
    patterns_simple: Option<bool>,
    /// The packages test suite expansions read.
    suite_packages: BTreeSet<u32>,
    /// Each planned key's phase-2 time (its implementation and actions),
    /// microseconds.
    costs: HashMap<u32, u32>,
    /// While the coordinator prepares a target ahead of its turn (P3,
    /// docs/PARALLEL_ANALYSIS.md): what the tracker would record, held
    /// with the thread-local context of each event and replayed at the
    /// target's turn ([`Engine::track_replay`]), so the recording is the
    /// sequential engine's.
    deferred: Option<Vec<Deferred>>,
    /// The lookup tables were dropped ([`Self::compact`]).
    compacted: bool,
}

impl Tracker {
    /// Drop the lookup tables of keys and edges once nothing is reached any
    /// more (the end of phase 2; memory): a key reached later gets a new
    /// id, an edge may repeat.
    pub(super) fn compact(&mut self) {
        self.edge_set = HashSet::new();
        self.compacted = true;
    }

    /// Whether nothing was recorded (a frozen walk's fork carries an empty
    /// tracker as the tracking mark: nothing may write it).
    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_empty()
            && self.frames.is_empty()
            && self.packages.is_empty()
            && self.accesses.is_empty()
            && self.costs.is_empty()
            && self.deferred.is_none()
    }

    /// A copy for a second engine (BAZEL_RS_EVAL_VERIFY's fork), not
    /// deferring.
    pub(super) fn fork(&self) -> Self {
        debug_assert!(self.deferred.is_none());
        Self {
            keys: self.keys.clone(),
            key_ids: self.key_ids.clone(),
            frames: self.frames.clone(),
            edges: self.edges.clone(),
            edge_set: self.edge_set.clone(),
            accesses: self.accesses.clone(),
            label_accesses: self.label_accesses.clone(),
            packages: self.packages.clone(),
            package_ids: self.package_ids.clone(),
            shared_keys: self.shared_keys.clone(),
            printed_keys: self.printed_keys.clone(),
            aspects: self.aspects.clone(),
            roots: self.roots.clone(),
            requested: self.requested.clone(),
            planned: self.planned.clone(),
            root_analysis: self.root_analysis,
            root_reached: self.root_reached,
            root_entries: self.root_entries.clone(),
            demand: self.demand.clone(),
            pattern_position: self.pattern_position,
            covered: self.covered.clone(),
            patterns_simple: self.patterns_simple,
            suite_packages: self.suite_packages.clone(),
            costs: self.costs.clone(),
            deferred: None,
            compacted: self.compacted,
        }
    }
}

/// One tracker event recorded while deferring ([`Tracker::deferred`]).
type Deferred = Box<dyn FnOnce(&mut Tracker) + Send + Sync>;

/// The thread-local scopes a tracker event reads, as it was recorded.
#[derive(Clone, Copy)]
pub(super) struct ScopeContext {
    shared_depth: u32,
    shared_name: &'static str,
    pattern_depth: u32,
    suite_depth: u32,
}

impl ScopeContext {
    pub(super) fn current() -> Self {
        Self {
            shared_depth: SHARED_DEPTH.with(Cell::get),
            shared_name: SHARED_NAME.with(Cell::get),
            pattern_depth: PATTERN_DEPTH.with(Cell::get),
            suite_depth: SUITE_DEPTH.with(Cell::get),
        }
    }

    /// Run `f` with this context's scopes, restoring the current ones.
    fn enter<R>(self, f: impl FnOnce() -> R) -> R {
        let saved = Self::current();
        self.install();
        let result = f();
        saved.install();
        result
    }

    fn install(self) {
        SHARED_DEPTH.with(|cell| cell.set(self.shared_depth));
        SHARED_NAME.with(|cell| cell.set(self.shared_name));
        PATTERN_DEPTH.with(|cell| cell.set(self.pattern_depth));
        SUITE_DEPTH.with(|cell| cell.set(self.suite_depth));
    }
}

/// The tracker events of one target prepared ahead of its turn
/// ([`Engine::track_defer_start`]).
#[derive(Default)]
pub(super) struct TrackLog(Vec<Deferred>);

impl TrackLog {
    /// The events of a prepare off the coordinator (S2, its target lookups
    /// recorded in order: Engine::track_access while deferring), each in
    /// the scopes `context` the coordinator had when it ran the prepare.
    pub(super) fn accesses(labels: Vec<Label>, context: ScopeContext) -> Self {
        Self(
            labels
                .into_iter()
                .map(|label| -> Deferred {
                    Box::new(move |tracker: &mut Tracker| context.enter(|| tracker.access(&label)))
                })
                .collect(),
        )
    }
}

#[derive(Clone, Default)]
struct PackageRecord {
    id: Option<PackageId>,
    global: bool,
    /// Why it is global (the first reason; diagnostics).
    why: Option<String>,
    pattern: bool,
    printed: bool,
}

impl Tracker {
    fn key(&mut self, identity: KeyIdentity) -> u32 {
        if let Some(id) = self.key_ids.get(&self.keys, &identity) {
            return id;
        }
        let id = self.keys.len() as u32;
        self.key_ids.insert(&identity, id);
        self.keys.push(identity);
        id
    }

    fn package(&mut self, package: &PackageId) -> u32 {
        if let Some(id) = self.package_ids.get(package) {
            return *id;
        }
        let id = self.packages.len() as u32;
        self.packages.push(PackageRecord {
            id: Some(package.clone()),
            ..PackageRecord::default()
        });
        self.package_ids.insert(package.clone(), id);
        id
    }

    fn make_global(&mut self, package: u32, why: impl FnOnce() -> String) {
        let record = &mut self.packages[package as usize];
        if !record.global {
            record.global = true;
            record.why = Some(why());
        }
    }

    fn edge(&mut self, parent: u32, child: u32) {
        if self.compacted && self.edges.contains(&(parent, child)) {
            return;
        }
        if self.edge_set.insert((parent, child)) {
            self.edges.push((parent, child));
        }
    }

    /// A configured key (or, `config` `None`, a source file) is reached:
    /// returns its id. Its frame is pushed by the caller when it is
    /// analyzed.
    fn reach(&mut self, identity: KeyIdentity) -> u32 {
        let package = identity.0.package_id();
        let id = self.key(identity);
        let inner = self.frames.last().copied();
        if let Some((parent, depth)) = inner
            && depth == shared_depth()
        {
            if parent != id {
                self.edge(parent, id);
            }
        } else if shared() {
            self.shared_keys.insert(id);
            let package = self.package(&package);
            let label = &self.keys[id as usize].0;
            let why = format!("{} reached by {}", label, SHARED_NAME.with(Cell::get));
            self.make_global(package, || why);
        } else if let Some(position) = self.root_analysis
            && !self.root_reached
        {
            self.root_reached = true;
            self.roots.push((position, id));
        } else {
            self.shared_keys.insert(id);
            let package = self.package(&package);
            let why = format!("{} reached outside every target", self.keys[id as usize].0);
            self.make_global(package, || why);
        }
        id
    }

    fn access(&mut self, label: &Label) {
        let package = label.package_id();
        let id = self.package(&package);
        let inner = self.frames.last().copied();
        if SUITE_DEPTH.with(Cell::get) > 0 {
            self.suite_packages.insert(id);
        }
        if pattern() {
            self.packages[id as usize].pattern = true;
        } else if let Some((frame, depth)) = inner
            && depth == shared_depth()
        {
            self.accesses.insert((frame, id));
            if label.repository.is_empty() {
                self.label_accesses.insert((frame, id, label.name.clone()));
            }
        } else if shared() {
            self.make_global(id, || {
                format!("{label} looked up by {}", SHARED_NAME.with(Cell::get))
            });
        } else if !self.requested.contains(label) {
            self.make_global(id, || format!("{label} looked up outside every target"));
        }
    }
}

/// Applies pass 1's tape to a tracker op by op, as the walk's tracker
/// calls at each position: reaches with their frames and plan entries,
/// target and source accesses in their shared scopes, prints, the
/// requested roots. A flush's phase 2 is not on the tape (its planning
/// frames are pass 2's).
///
/// S3 pass 2 (docs/EVALUATOR.md): with deferred flushes the tape's tracker
/// ops wait for the phase 2 before them ([`TrackOp`], applied by plan
/// position like the tape's other effects), so the tracker sees phase 2
/// and the tape in the walk's order.
pub(super) struct TapeReplay {
    plan_len: u32,
    /// (the entered key's id, the frame depth before its reach).
    entered: Vec<(u32, usize)>,
    /// The tracker scopes the tape entered, outermost first (its own
    /// context, installed around each op: the evaluation's thread-local
    /// scopes are the live ones).
    scopes: Vec<&'static str>,
    roots: usize,
    /// What the compact ops name ([`Self::compact`]).
    tables: TrackTables,
}

/// A tape op as the tracker replays it ([`TapeReplay::compact`]): its
/// labels and keys interned (millions are held while a deferred flush's
/// phase 2 is pending).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TrackOp {
    ScopeEnter(u32),
    ScopeExit,
    /// `TapeOp::Segment` of a requested label's analysis (its position).
    Root(u32),
    /// `TapeOp::Segment(SegmentKind::RootsDone)`.
    RootsDone,
    /// Any other segment.
    Segment,
    Reach {
        key: u32,
        enter: bool,
    },
    Leave,
    Plan {
        position: bool,
    },
    Access(u32),
    Source(u32),
    PkgCommit {
        package: u32,
        printed: bool,
    },
    Print,
}

/// The interned names of [`TrackOp`]s.
#[derive(Default)]
struct TrackTables {
    keys: Vec<KeyIdentity>,
    key_ids: HashMap<KeyIdentity, u32>,
    labels: Vec<Label>,
    label_ids: HashMap<Label, u32>,
    packages: Vec<PackageId>,
    package_ids: HashMap<PackageId, u32>,
    scopes: Vec<&'static str>,
}

fn intern<T: Clone + Eq + std::hash::Hash>(
    values: &mut Vec<T>,
    ids: &mut HashMap<T, u32>,
    value: &T,
) -> u32 {
    if let Some(id) = ids.get(value) {
        return *id;
    }
    let id = values.len() as u32;
    values.push(value.clone());
    ids.insert(value.clone(), id);
    id
}

impl TapeReplay {
    /// `plan_len`: the plan's length when the tape begins.
    pub(super) fn new(plan_len: usize) -> Self {
        Self {
            plan_len: plan_len as u32,
            entered: Vec::new(),
            scopes: Vec::new(),
            roots: 0,
            tables: TrackTables::default(),
        }
    }

    /// Drop the lookup side of the interned names (memory: the tape is
    /// through; a name interned later is added again, unshared).
    pub(super) fn forget_interned_ids(&mut self) {
        self.tables.key_ids = HashMap::new();
        self.tables.label_ids = HashMap::new();
        self.tables.package_ids = HashMap::new();
    }

    /// The interned names' counts: keys, labels, packages (diagnostics).
    pub(super) fn interned(&self) -> (usize, usize, usize) {
        (
            self.tables.keys.len(),
            self.tables.labels.len(),
            self.tables.packages.len(),
        )
    }

    /// `count` plan entries were made off the tape (a flush's phase 2
    /// analyzed a deferred platform's tree as the walk does).
    pub(super) fn planned_elsewhere(&mut self, count: usize) {
        self.plan_len += count as u32;
    }

    /// The scope context the tape is in.
    fn context(&self) -> ScopeContext {
        let shared: Vec<&'static str> = self
            .scopes
            .iter()
            .copied()
            .filter(|name| *name != PATTERN_SCOPE && *name != SUITE_SCOPE)
            .collect();
        ScopeContext {
            shared_depth: shared.len() as u32,
            shared_name: shared.first().copied().unwrap_or(""),
            pattern_depth: self
                .scopes
                .iter()
                .filter(|name| **name == PATTERN_SCOPE)
                .count() as u32,
            suite_depth: self
                .scopes
                .iter()
                .filter(|name| **name == SUITE_SCOPE)
                .count() as u32,
        }
    }

    /// The tracker's part of a tape op (`None`: the tracker ignores it);
    /// `reached`: a `Reach`'s configured key.
    pub(super) fn compact(
        &mut self,
        op: &super::eval::transcript::TapeOp,
        reached: Option<&ConfiguredTargetKey>,
    ) -> Option<TrackOp> {
        use super::eval::transcript::{Event, SegmentKind, TapeOp};
        let tables = &mut self.tables;
        Some(match op {
            TapeOp::Effect(Event::ScopeEnter(name)) => {
                let id = match tables.scopes.iter().position(|known| known == name) {
                    Some(id) => id,
                    None => {
                        tables.scopes.push(name);
                        tables.scopes.len() - 1
                    }
                };
                TrackOp::ScopeEnter(id as u32)
            }
            TapeOp::Effect(Event::ScopeExit) => TrackOp::ScopeExit,
            TapeOp::Segment(SegmentKind::Root { position, .. }) => TrackOp::Root(*position as u32),
            TapeOp::Segment(SegmentKind::RootsDone) => TrackOp::RootsDone,
            TapeOp::Segment(SegmentKind::Setup | SegmentKind::TopLevelAspects { .. }) => {
                TrackOp::Segment
            }
            TapeOp::Reach { enter, .. } => TrackOp::Reach {
                key: {
                    let key = reached?;
                    intern(
                        &mut tables.keys,
                        &mut tables.key_ids,
                        &(
                            key.label.clone(),
                            Some(key.config),
                            key.apply_rule_transition,
                        ),
                    )
                },
                enter: *enter,
            },
            TapeOp::Leave => TrackOp::Leave,
            TapeOp::Plan { position, .. } => TrackOp::Plan {
                position: *position,
            },
            TapeOp::Effect(Event::Access(label)) => {
                TrackOp::Access(intern(&mut tables.labels, &mut tables.label_ids, label))
            }
            TapeOp::Effect(Event::PkgCommit(package, printed)) => TrackOp::PkgCommit {
                package: intern(&mut tables.packages, &mut tables.package_ids, package),
                printed: *printed,
            },
            TapeOp::Effect(Event::Source(label)) => TrackOp::Source(intern(
                &mut tables.keys,
                &mut tables.key_ids,
                &(label.clone(), None, true),
            )),
            TapeOp::Effect(Event::Print(_)) => TrackOp::Print,
            TapeOp::Fail(_) | TapeOp::Flush | TapeOp::Effect(_) => return None,
        })
    }

    /// Apply a compact op ([`Self::compact`]) where the walk's tracker
    /// records it.
    pub(super) fn replay(&mut self, tracker: &mut Tracker, op: TrackOp) {
        match op {
            TrackOp::ScopeEnter(name) => self.scopes.push(self.tables.scopes[name as usize]),
            TrackOp::ScopeExit => {
                self.scopes.pop();
            }
            op => self.context().enter(|| self.replay_in_context(tracker, op)),
        }
    }

    fn replay_in_context(&mut self, tracker: &mut Tracker, op: TrackOp) {
        match op {
            TrackOp::Root(_) | TrackOp::RootsDone | TrackOp::Segment => {
                tracker.root_analysis = None;
                tracker.root_reached = false;
                match op {
                    TrackOp::Root(position) => {
                        let position = position as usize;
                        if position == 0 {
                            tracker.root_entries.clear();
                        }
                        if tracker.root_entries.len() == position {
                            tracker.root_entries.push(self.plan_len);
                        }
                        tracker.root_analysis = Some(position as u32);
                        self.roots = position + 1;
                    }
                    TrackOp::RootsDone => {
                        if self.roots == 0 {
                            tracker.root_entries.clear();
                        }
                        if tracker.root_entries.len() == self.roots {
                            tracker.root_entries.push(self.plan_len);
                        }
                    }
                    _ => {}
                }
            }
            TrackOp::Reach { key, enter } => {
                let depth = tracker.frames.len();
                let id = tracker.reach(self.tables.keys[key as usize].clone());
                if enter {
                    tracker.frames.push((id, shared_depth()));
                    tracker
                        .planned
                        .entry(id)
                        .or_insert((self.plan_len, u32::MAX));
                    self.entered.push((id, depth));
                }
            }
            TrackOp::Leave => {
                if let Some((_, depth)) = self.entered.pop() {
                    tracker.frames.truncate(depth);
                }
            }
            TrackOp::Plan { position } => {
                if position
                    && let Some((id, _)) = self.entered.last()
                    && let Some(planned) = tracker.planned.get_mut(id)
                    && planned.1 == u32::MAX
                {
                    planned.1 = self.plan_len;
                }
                self.plan_len += 1;
            }
            TrackOp::Access(label) => tracker.access(&self.tables.labels[label as usize]),
            // The commit's record (`track_package_printed`; its BUILD
            // prints are pass 2's to note).
            TrackOp::PkgCommit { package, printed } => {
                let id = tracker.package(&self.tables.packages[package as usize]);
                let record = &mut tracker.packages[id as usize];
                record.printed |= printed;
                record.pattern |= pattern();
            }
            TrackOp::Source(key) => {
                tracker.reach(self.tables.keys[key as usize].clone());
            }
            TrackOp::Print => {
                if let Some((frame, _)) = tracker.frames.last().copied() {
                    tracker.printed_keys.insert(frame);
                }
            }
            TrackOp::ScopeEnter(_) | TrackOp::ScopeExit => {}
        }
    }

    pub(super) fn finish(self, tracker: &mut Tracker) {
        tracker.root_analysis = None;
        tracker.root_reached = false;
    }
}

impl Engine {
    /// Record dependencies for incremental analysis (docs/INCREMENTAL_ANALYSIS.md)
    /// from now on: the CLI turns it on for the builds whose analysis the
    /// snapshot saves.
    pub fn set_incremental_tracking(&mut self, on: bool) {
        self.incremental = on.then(Box::default);
    }

    /// Whether this engine records dependencies.
    pub(super) fn tracking(&self) -> bool {
        self.incremental.is_some()
    }

    /// Apply `event` to the tracker, or hold it with its scope context
    /// while deferring ([`Self::track_defer_start`]).
    fn track_with(&mut self, event: impl FnOnce(&mut Tracker) + Send + Sync + 'static) {
        let Some(tracker) = self.incremental.as_mut() else {
            return;
        };
        match tracker.deferred.as_mut() {
            Some(deferred) => {
                let context = ScopeContext::current();
                deferred.push(Box::new(move |tracker: &mut Tracker| {
                    context.enter(|| event(tracker))
                }));
            }
            None => event(tracker),
        }
    }

    fn track_deferring(&self) -> bool {
        self.incremental
            .as_ref()
            .is_some_and(|tracker| tracker.deferred.is_some())
    }

    /// Hold every tracker event from now on (P3: the coordinator prepares
    /// a target ahead of its turn) until [`Self::track_defer_finish`].
    pub(super) fn track_defer_start(&mut self) {
        if let Some(tracker) = self.incremental.as_mut() {
            debug_assert!(tracker.deferred.is_none(), "tracker deferrals do not nest");
            tracker.deferred = Some(Vec::new());
        }
    }

    /// The events held since [`Self::track_defer_start`].
    pub(super) fn track_defer_finish(&mut self) -> TrackLog {
        TrackLog(
            self.incremental
                .as_mut()
                .and_then(|tracker| tracker.deferred.take())
                .unwrap_or_default(),
        )
    }

    /// Apply held events where the sequential engine records them (the
    /// target's turn, its planning frame innermost), each in the scopes it
    /// was recorded in.
    pub(super) fn track_replay(&mut self, log: TrackLog) {
        if let Some(tracker) = self.incremental.as_mut() {
            debug_assert!(tracker.deferred.is_none());
            for event in log.0 {
                event(tracker);
            }
        }
    }

    /// `analyze_key` reached `key` (before its memo check).
    pub(super) fn track_key(&mut self, key: &ConfiguredTargetKey) -> Option<u32> {
        if self.track_deferring() {
            // Held: no frame is entered for it while deferring (an
            // analysis ahead of its turn aborts, Engine::ahead_abort).
            let identity = (
                key.label.clone(),
                Some(key.config),
                key.apply_rule_transition,
            );
            self.track_with(move |tracker| {
                tracker.reach(identity);
            });
            return None;
        }
        let tracker = self.incremental.as_mut()?;
        Some(tracker.reach((
            key.label.clone(),
            Some(key.config),
            key.apply_rule_transition,
        )))
    }

    /// `analyze_key` reached the source file `label`.
    pub(super) fn track_source(&mut self, label: &Label) {
        if self.tracking() {
            let label = label.clone();
            self.track_with(move |tracker| {
                tracker.reach((label, None, true));
            });
        }
    }

    /// The analysis or planning of a configured target begins (its frame).
    pub(super) fn track_enter(&mut self, id: Option<u32>) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id) {
            tracker.frames.push((id, shared_depth()));
        }
    }

    /// The analysis of `id` begins with `plan_len` targets planned.
    pub(super) fn track_entry(&mut self, id: Option<u32>, plan_len: usize) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id) {
            tracker
                .planned
                .entry(id)
                .or_insert((plan_len as u32, u32::MAX));
        }
    }

    /// `id` is planned at `position` (phase 1's post-order).
    pub(super) fn track_position(&mut self, id: Option<u32>, position: usize) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id)
            && let Some(planned) = tracker.planned.get_mut(&id)
            && planned.1 == u32::MAX
        {
            planned.1 = position as u32;
        }
    }

    /// The frame depth (restored by [`Self::track_restore`]).
    pub(super) fn track_depth(&self) -> usize {
        self.incremental
            .as_ref()
            .map_or(0, |tracker| tracker.frames.len())
    }

    pub(super) fn track_restore(&mut self, depth: usize) {
        // While deferring no frame is pushed (track_key holds the reach
        // and returns no id): nothing to restore.
        if let Some(tracker) = self.incremental.as_mut()
            && tracker.deferred.is_none()
        {
            tracker.frames.truncate(depth);
        }
    }

    /// The innermost frame's phase 2 took `elapsed`.
    pub(super) fn track_cost(&mut self, elapsed: std::time::Duration) {
        self.track_with(move |tracker| {
            if let Some((frame, _)) = tracker.frames.last().copied() {
                let micros = elapsed.as_micros().min(u32::MAX as u128) as u32;
                let cost = tracker.costs.entry(frame).or_default();
                *cost = cost.saturating_add(micros);
            }
        });
    }

    /// Phase 2 plans `key`: its frame (no edge: phase 1 recorded them).
    pub(super) fn track_planning(&mut self, key: &ConfiguredTargetKey) {
        if self.tracking() {
            let identity = (
                key.label.clone(),
                Some(key.config),
                key.apply_rule_transition,
            );
            self.track_with(move |tracker| {
                let id = tracker.key(identity);
                tracker.frames.push((id, shared_depth()));
            });
        }
    }

    /// `Engine::target` looks up `label`.
    pub(super) fn track_access(&mut self, label: &Label) {
        if let Some(capture) = self.access_capture.last_mut()
            && self.incremental.is_some()
        {
            capture.push(label.clone());
        }
        if let Some(tracker) = self.incremental.as_mut() {
            if tracker.deferred.is_none() {
                tracker.access(label);
            } else {
                let label = label.clone();
                self.track_with(move |tracker| tracker.access(&label));
            }
        }
    }

    /// Print a line an analysis printed (`print()`), noting the configured
    /// target whose frame is innermost.
    pub(super) fn note_analysis_print(&mut self, line: &str) {
        self.track_analysis_print();
        crate::ui::note(line);
    }

    /// [`Self::note_analysis_print`]'s tracking of a line written already
    /// (streamed when its job finished: BAZEL_RS_ANALYSIS_ORDER=genuine).
    pub(super) fn track_analysis_print(&mut self) {
        self.track_with(|tracker| {
            if let Some((frame, _)) = tracker.frames.last().copied() {
                tracker.printed_keys.insert(frame);
            }
        });
    }

    /// The innermost key's planning uses the aspect application `key`.
    pub(super) fn track_aspect_application(&mut self, key: &crate::aspects::AspectApplicationKey) {
        if self.tracking() {
            let base = &key.base;
            let identity = format!(
                "{}|{}|{}|{}|{}",
                base.label,
                base.config,
                base.apply_rule_transition,
                key.aspect,
                key.bases.join(",")
            );
            self.track_with(move |tracker| {
                let frame = tracker.frames.last().map_or(u32::MAX, |(frame, _)| *frame);
                tracker.aspects.insert((frame, identity));
            });
        }
    }

    /// A package was committed; `printed`: its BUILD file printed.
    pub(super) fn track_package_printed(&mut self, package: &PackageId, printed: bool) {
        // BAZEL_RS_ANALYSIS=eval: the commit belongs to the running node.
        super::eval::linearize::capture::package(package, printed);
        if self.tracking() {
            let package = package.clone();
            self.track_with(move |tracker| {
                let id = tracker.package(&package);
                let record = &mut tracker.packages[id as usize];
                record.printed |= printed;
                record.pattern |= pattern();
            });
        }
    }

    /// The requested labels of this build (accesses to them outside any
    /// frame are theirs).
    pub(super) fn track_requested(&mut self, labels: &[Label]) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.requested = labels.iter().cloned().collect();
        }
    }

    /// `analyze_requested` analyzes the requested label at `position`
    /// (`None`: done).
    pub(super) fn track_root_analysis(&mut self, position: Option<usize>) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.root_analysis = position.map(|position| position as u32);
            tracker.root_reached = false;
        }
    }

    /// The analysis of requested label `position` begins with `plan_len`
    /// targets planned (`position` = the labels' count: phase 1 ended).
    pub(super) fn track_root_entry(&mut self, position: usize, plan_len: usize) {
        if let Some(tracker) = self.incremental.as_mut() {
            if position == 0 {
                tracker.root_entries.clear();
            }
            if tracker.root_entries.len() == position {
                tracker.root_entries.push(plan_len as u32);
            }
        }
    }

    /// The hidden top-level artifacts' producers of the request (the
    /// requested label at each position; `None`: the workspace status).
    pub(super) fn track_demand(&mut self, demand: Vec<(Option<usize>, String)>) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.demand = demand
                .into_iter()
                .map(|(position, id)| (position.map_or(u32::MAX, |position| position as u32), id))
                .collect();
            tracker.demand.sort();
            tracker.demand.dedup();
        }
    }

    /// The target pattern phase expands the pattern at `position` (`None`:
    /// done).
    pub(super) fn track_pattern(&mut self, position: Option<usize>) {
        if let Some(tracker) = self.incremental.as_mut() {
            // Only the target pattern phase's (crate::engine's `select*`
            // serve other commands too).
            tracker.pattern_position = position
                .filter(|_| pattern())
                .map(|position| position as u32);
        }
    }

    /// The pattern being expanded covers `packages`.
    pub(super) fn track_pattern_packages(&mut self, packages: &[PackageId]) {
        if let Some(tracker) = self.incremental.as_mut()
            && let Some(position) = tracker.pattern_position
        {
            for package in packages {
                let id = tracker.package(package);
                tracker.covered.insert((position, id));
            }
        }
    }

    /// A test suite's expansion read `label`'s package.
    pub(super) fn track_suite_package(&mut self, label: &Label) {
        if self.tracking() {
            let package = label.package_id();
            self.track_with(move |tracker| {
                let id = tracker.package(&package);
                tracker.suite_packages.insert(id);
            });
        }
    }

    /// The target pattern phase ended; `simple`: a plain `build` one.
    pub(super) fn track_patterns_simple(&mut self, simple: bool) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.patterns_simple = Some(simple && tracker.patterns_simple != Some(false));
        }
    }

    /// The dependency index of the last planned build (`labels`: its
    /// requested labels), with the fingerprints of its main-repository
    /// packages; `None` without tracking.
    pub(super) fn incremental_index(&mut self, labels: &[Label]) -> Option<Index> {
        let tracker = self.incremental.as_mut()?;
        // Every loaded package has a record (fingerprints of the main
        // repository's).
        let loaded: Vec<PackageId> = self.packages.keys().cloned().collect();
        for package in &loaded {
            tracker.package(package);
        }
        let main: Vec<(u32, &Package)> = tracker
            .packages
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let id = record.id.as_ref()?;
                if !id.repository.is_empty() {
                    return None;
                }
                self.packages
                    .get(id)
                    .map(|package| (index as u32, &**package))
            })
            .collect();
        let fingerprints = fingerprints(&main);
        let mut packages: Vec<IndexPackage> = tracker
            .packages
            .iter()
            .map(|record| {
                let id = record.id.clone().unwrap_or_else(|| PackageId {
                    repository: String::new(),
                    package: String::new(),
                });
                IndexPackage {
                    repository: id.repository,
                    package: id.package,
                    global: record.global,
                    why: record.why.clone(),
                    pattern: record.pattern,
                    printed: record.printed,
                    loaded: false,
                    fingerprint: None,
                    loads: Vec::new(),
                }
            })
            .collect();
        for (index, fingerprint) in fingerprints {
            packages[index as usize].fingerprint = Some(fingerprint);
        }
        for package in &loaded {
            if let Some(id) = tracker.package_ids.get(package) {
                packages[*id as usize].loaded = true;
            }
        }
        let mut accesses: Vec<(u32, u32)> = tracker.accesses.iter().copied().collect();
        accesses.sort_unstable();
        let mut label_accesses: Vec<(u32, u32, String)> =
            tracker.label_accesses.iter().cloned().collect();
        label_accesses.sort_unstable();
        let main_root = self
            .repositories
            .root("")
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let package_loads: Vec<(u32, Vec<Label>)> = tracker
            .packages
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let package = self.packages.get(record.id.as_ref()?)?;
                Some((index as u32, package.direct_loads.clone()))
            })
            .collect();
        let keys: Vec<(u32, Label)> = tracker
            .keys
            .iter()
            .enumerate()
            .filter(|(_, (_, config, _))| config.is_some())
            .map(|(index, (label, _, _))| (index as u32, label.clone()))
            .collect();
        let graph = module_graph(&self.packages);
        let rules = rule_modules(&self.packages, &keys);
        let repository_inputs: Vec<_> = crate::observed_fs::repository_inputs()
            .into_iter()
            .map(|(path, tree, owner)| {
                (crate::snapshot::incremental::PathBufWire(path), tree, owner)
            })
            .collect();
        let mut index = Index {
            main_root,
            repository_inputs: repository_inputs.clone(),
            repository_roots: {
                let owners: BTreeSet<&str> = repository_inputs
                    .iter()
                    .filter_map(|(_, _, owner)| owner.as_deref())
                    .collect();
                self.repositories
                    .materialized_roots()
                    .into_iter()
                    .filter(|(name, _)| owners.contains(name.as_str()))
                    .map(|(name, root)| (name, crate::snapshot::incremental::PathBufWire(root)))
                    .collect()
            },
            top: self.configurations.top().bytes(),
            labels: labels.to_vec(),
            keys: tracker
                .keys
                .iter()
                .map(|(label, config, apply)| IndexKey {
                    label: label.clone(),
                    config: config.map(|config| config.bytes()),
                    apply: *apply,
                })
                .collect(),
            edges: tracker.edges.clone(),
            accesses,
            label_accesses,
            packages,
            roots: tracker.roots.clone(),
            shared: tracker.shared_keys.iter().copied().collect(),
            printed: tracker.printed_keys.iter().copied().collect(),
            aspects: tracker.aspects.iter().cloned().collect(),
            modules: Vec::new(),
            rule_modules: Vec::new(),
            planned: {
                let mut planned: Vec<(u32, u32, u32)> = tracker
                    .planned
                    .iter()
                    .filter(|(_, (_, position))| *position != u32::MAX)
                    .map(|(key, (entry, position))| (*key, *entry, *position))
                    .collect();
                planned.sort_unstable();
                planned
            },
            root_entries: if tracker.root_entries.len() == labels.len() + 1 {
                tracker.root_entries.clone()
            } else {
                Vec::new()
            },
            demand: tracker.demand.clone(),
            patterns: (tracker.patterns_simple == Some(true))
                .then(|| tracker.covered.iter().copied().collect()),
            suite_packages: tracker.suite_packages.iter().copied().collect(),
            costs: {
                let mut costs: Vec<(u32, u32)> = tracker
                    .costs
                    .iter()
                    .map(|(key, cost)| (*key, *cost))
                    .collect();
                costs.sort_unstable();
                costs
            },
        };
        crate::snapshot::incremental::set_modules(&mut index, &graph, &package_loads, &rules);
        Some(index)
    }

    /// The `.bzl` load graph of every loaded package, and the modules
    /// definition loads entered (crate::snapshot::incremental::ModuleGraph).
    pub fn module_graph(&self) -> crate::snapshot::incremental::ModuleGraph {
        module_graph(&self.packages)
    }

    /// The modules defining the rules of `keys` (see [`rule_modules`]).
    pub fn rule_modules(&self, keys: &[(u32, Label)]) -> Vec<(u32, Vec<Label>)> {
        rule_modules(&self.packages, keys)
    }

    /// The direct loads of every loaded package.
    pub fn package_loads(&self) -> Vec<(PackageId, Vec<Label>)> {
        self.packages
            .iter()
            .map(|(id, package)| (id.clone(), package.direct_loads.clone()))
            .collect()
    }

    /// Load `package` (if it is not yet) and return its fingerprint, and
    /// whether its BUILD file printed when it was loaded by this engine.
    pub fn package_fingerprint(
        &mut self,
        package: &PackageId,
    ) -> Result<(Fingerprint, bool), String> {
        {
            let _pattern = PatternScope::enter();
            self.load_package(package)?;
        }
        let printed = self.incremental.as_ref().is_some_and(|tracker| {
            tracker
                .package_ids
                .get(package)
                .is_some_and(|id| tracker.packages[*id as usize].printed)
        });
        Ok((fingerprint(&self.packages[package]), printed))
    }

    /// What the plain `build` target pattern phase (crate::engine's
    /// `select_filtered` with FILTER_MANUAL — none under `affected
    /// --include_manual` — no tag filters, tests or test suites) selects of the loaded main-repository package `package` now:
    /// `covered` names the patterns (positions) whose wildcards expanded it
    /// (the saved analysis's: the directory listings did not change). The
    /// error says why it cannot be computed this way (a test suite, an
    /// ambiguous `:all`, a label that does not exist).
    pub fn package_selection(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        covered: &BTreeSet<u32>,
        package: &PackageId,
        expand_test_suites: bool,
    ) -> Result<BTreeSet<Label>, String> {
        use crate::target_pattern::Kind;
        // The lookups are the target pattern phase's.
        let _pattern = PatternScope::enter();
        let mut selected: BTreeSet<Label> = BTreeSet::new();
        for (position, text) in patterns.iter().enumerate() {
            let pattern = crate::target_pattern::parse(text, relative_directory)?;
            let filter = !pattern.negative && !crate::affected::include_manual();
            let covers = covered.contains(&(position as u32));
            let targets: BTreeSet<Label> = match &pattern.kind {
                Kind::TargetsBelowDirectory { rules_only } => {
                    if covers {
                        self.package_targets(package, *rules_only, filter)?
                            .into_iter()
                            .collect()
                    } else {
                        BTreeSet::new()
                    }
                }
                Kind::TargetsInPackage {
                    suffix,
                    absolute,
                    rules_only,
                } => {
                    if !covers {
                        BTreeSet::new()
                    } else if *absolute
                        && self
                            .existing_target_kind(package, suffix)
                            .ok()
                            .flatten()
                            .is_some()
                    {
                        return Err(format!("the pattern '{text}' names an existing target"));
                    } else {
                        self.package_targets(package, *rules_only, filter)?
                            .into_iter()
                            .collect()
                    }
                }
                Kind::SingleTarget { .. } | Kind::PathAsTarget { .. } => {
                    let label = match &pattern.kind {
                        Kind::SingleTarget { target } => {
                            self.repositories
                                .parse(&pattern.label_text(target), "", "")?
                        }
                        Kind::PathAsTarget { path } => self
                            .path_as_target(path)
                            .map_err(PatternError::into_message)?,
                        _ => unreachable!("a single target"),
                    };
                    if label.package_id() == *package {
                        self.target(&label)?;
                        BTreeSet::from([label])
                    } else {
                        BTreeSet::new()
                    }
                }
            };
            if pattern.negative {
                selected.retain(|label| !targets.contains(label));
            } else {
                selected.extend(targets);
            }
        }
        if expand_test_suites
            && let Some(suite) = selected.iter().find(|label| self.loaded_test_suite(label))
        {
            return Err(format!("{suite} is a test suite"));
        }
        Ok(selected)
    }

    /// The root of repository `name`, materialized now (its warm check, or
    /// its execution: incremental analysis re-runs a repository whose
    /// recorded inputs changed).
    pub fn repository_root_now(&self, name: &str) -> Result<PathBuf, String> {
        self.repositories.root(name).map(Path::to_path_buf)
    }

    /// [`Self::package_fingerprint`] of every package of `packages` (in
    /// order), their loads on the loading-phase-threads pool (commits in
    /// order, like the target pattern phase's).
    pub fn package_fingerprints(
        &mut self,
        packages: &[PackageId],
    ) -> Result<Vec<(Fingerprint, bool)>, (PackageId, String)> {
        {
            let _pattern = PatternScope::enter();
            // As the target pattern phase's loads: the repositories the
            // loaded files enter are fetched (reused) ahead on the
            // speculation pool, so the workers' loads find them.
            let speculating = self.start_pattern_speculation();
            let loaded = self.load_packages_parallel(packages, |_, _| Ok(()));
            if speculating {
                self.repositories.stop_speculation();
            }
            if let Err(error) = loaded {
                // The first one that does not load (named by its own load).
                for package in packages {
                    if let Err(error) = self.load_package(package) {
                        return Err((package.clone(), error));
                    }
                }
                return Err((
                    packages.first().cloned().unwrap_or(PackageId {
                        repository: String::new(),
                        package: String::new(),
                    }),
                    error,
                ));
            }
        }
        packages
            .iter()
            .map(|package| {
                self.package_fingerprint(package)
                    .map_err(|error| (package.clone(), error))
            })
            .collect()
    }
}

/// The fingerprints of `packages`, on a few threads (a large workspace
/// has thousands of main-repository packages).
fn fingerprints(packages: &[(u32, &Package)]) -> Vec<(u32, Fingerprint)> {
    if packages.len() < 64 {
        return packages
            .iter()
            .map(|(index, package)| (*index, fingerprint(package)))
            .collect();
    }
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 8);
    let chunk = packages.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = packages
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|(index, package)| (*index, fingerprint(package)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// Rule kinds whose targets feed computations many targets share
/// (visibility, toolchain and platform resolution, configurations): a
/// package holding one is never re-analyzed incrementally.
fn shared_kind(rule: &Rule) -> bool {
    if matches!(rule.kind, RuleKind::PackageGroup(_)) {
        return true;
    }
    if rule
        .custom
        .as_ref()
        .is_some_and(|custom| custom.build_setting.is_some())
    {
        return true;
    }
    matches!(
        rule.rule_class_name(),
        "toolchain"
            | "toolchain_type"
            | "platform"
            | "constraint_setting"
            | "constraint_value"
            | "label_flag"
            | "label_setting"
            | "config_feature_flag"
            | "environment"
            | "environment_group"
    )
}

/// The fingerprint of a loaded package: a digest of everything its load
/// produced that analysis can read (rules with their attributes, exports,
/// package arguments, loads), and the summaries the target pattern phase
/// depends on.
pub(crate) fn fingerprint(package: &Package) -> Fingerprint {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = write!(
        text,
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        package.exports,
        package.default_visibility,
        package.macro_names,
        package.args,
        package.build_file,
        package.direct_loads,
    );
    let package_digest = *blake3::hash(text.as_bytes()).as_bytes();
    let mut rules_summary = String::new();
    let mut shared_kinds = false;
    let mut targets: Vec<(String, [u8; 16])> = Vec::new();
    for (label, rule) in package.rules.iter() {
        let start = text.len();
        let custom = rule.custom.as_ref().map(|custom| {
            (
                &custom.attrs,
                &custom.build_setting,
                &custom.declaration,
                custom.transitions.keys().collect::<Vec<_>>(),
            )
        });
        let _ = write!(
            text,
            "\n{label}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            rule.kind,
            rule.srcs,
            rule.outs,
            rule.cmd,
            rule.visibility,
            custom,
            rule.native,
            rule.unanalyzed,
            rule.package_metadata,
            rule.location,
            rule.explicit_visibility,
            rule.macro_owner,
            rule.call_stack,
            rule.generator,
        );
        // The rule's own digest, for its name and each of its outputs.
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&blake3::hash(&text.as_bytes()[start..]).as_bytes()[..16]);
        targets.push((rule.label.name.clone(), digest));
        for out in &rule.outs {
            targets.push((out.clone(), digest));
        }
        shared_kinds |= shared_kind(rule);
        let test_attributes = rule.native.as_ref().map(|native| {
            [
                "size",
                "timeout",
                "flaky",
                "shard_count",
                "local",
                "tests",
                "deprecation",
                "testonly",
            ]
            .iter()
            .filter_map(|name| native.attrs.get(*name).map(|value| (*name, value)))
            .collect::<Vec<_>>()
        });
        // The built-in attributes but the generator's location (it moves
        // with every line inserted above the macro call; the digest has it).
        let custom_test_attributes = rule.custom.as_ref().map(|custom| {
            let builtin = &custom.declaration.builtin;
            (
                (
                    &builtin.kind,
                    &builtin.target,
                    &builtin.visibility,
                    builtin.testonly,
                    &builtin.deprecation,
                    builtin
                        .generator
                        .as_ref()
                        .map(|generator| (&generator.name, &generator.function)),
                ),
                custom.declaration.rule_class.as_str(),
            )
        });
        let _ = write!(
            rules_summary,
            "\n{label}|{}|{:?}|{}|{:?}|{:?}|{:?}",
            rule.rule_class_name(),
            rule.tags(),
            rule.is_manual(),
            rule.outs,
            test_attributes,
            custom_test_attributes,
        );
    }
    targets.sort();
    Fingerprint {
        digest: *blake3::hash(text.as_bytes()).as_bytes(),
        summary: *blake3::hash(rules_summary.as_bytes()).as_bytes(),
        shared_kinds,
        package: package_digest,
        targets,
    }
}

/// The `.bzl` load graph of every loaded package, and the modules
/// definition loads entered (crate::snapshot::incremental::ModuleGraph).
fn module_graph(
    packages: &BTreeMap<PackageId, Arc<Package>>,
) -> crate::snapshot::incremental::ModuleGraph {
    let mut graph = crate::snapshot::incremental::ModuleGraph::default();
    let mut seen: HashSet<*const crate::loading::LoadGraph> = HashSet::new();
    for package in packages.values() {
        if !seen.insert(Arc::as_ptr(&package.load_graph)) {
            continue;
        }
        for (module, loads) in package.load_graph.iter() {
            graph
                .loads
                .entry(module.clone())
                .or_insert_with(|| loads.clone());
        }
    }
    graph.definitions = crate::loading::definition_modules().into_iter().collect();
    graph.code = crate::snapshot::incremental::module_code();
    graph
}

/// The modules defining the rules of `keys` (loaded packages only): the
/// `.bzl` that called `rule()` and the one being initialized then.
fn rule_modules(
    packages: &BTreeMap<PackageId, Arc<Package>>,
    keys: &[(u32, Label)],
) -> Vec<(u32, Vec<Label>)> {
    keys.iter()
        .filter_map(|(key, label)| {
            let rule = packages.get(&label.package_id())?.rules.get(label)?;
            let custom = rule.custom.as_ref()?;
            let modules = [
                &custom.declaration.definition_label,
                &custom.declaration.definition_environment_label,
            ]
            .into_iter()
            .map(|text| {
                // Unparsable: no module edit is attributable (the index
                // says so, crate::snapshot::incremental::UNATTRIBUTED).
                Label::parse_canonical(text)
                    .unwrap_or_else(|_| crate::snapshot::incremental::unattributed_module())
            })
            .collect();
            Some((*key, modules))
        })
        .collect()
}
