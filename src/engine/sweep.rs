//! Genuine parity, task A (docs/EVALUATOR.md §8, "The sweep"): a worker's
//! delta is merged as built, its producer ids rewritten, instead of being
//! built again at its merge.
//!
//! A delta built on a worker names the producers it found in the artifact
//! index — a published result's own action ids (provisional), the merged
//! ones (canonical) — and its own outputs' Artifacts name its own actions.
//! At its merge, in plan order, two things may differ from what the
//! sequential walk's commit would have built, and only in producer ids:
//!
//! - **A contested reader**: a File it read provisionally is canonical now
//!   with another producer — a shared output whose winner is an earlier
//!   entry (`read_aliases`: each such read's id maps to the canonical id;
//!   every other difference — a File read as missing that is registered
//!   now, another field — is not a rewrite, and the delta is built again
//!   as before).
//! - **A loser**: an output path of its own meets a producer registered
//!   earlier in plan order (`localize`): the merge replays the walk's
//!   output registration over the build's producers (`check_output`: New,
//!   Shared, the partial-share rule; a conflict is built again for its
//!   exact text and partial state), and each shared action becomes the
//!   walk's — its `Output` ops `Shared`, its declarations `replace` only
//!   when predeclared, no node, execution or listing — and its id maps to
//!   the winner's.
//!
//! The delta is then rewritten through the map ([`rewrite`]): every
//! Artifact naming a mapped id names the canonical one (its output index
//! kept: a shared action shares every output, in order), and so does every
//! generated input of its nodes and shared input groups (each group once,
//! by pointer) — lowerings in flight are received and handed on. Nothing
//! else of a node depends on a producer id (inputs at one exec path are one
//! input by producer class, `input_occurrences`). Dependents are not
//! affected: providers and views hold Files, which name no producer.
//!
//! `BAZEL_RS_EVAL_SWEEP=0` builds such deltas again at their merge (the T3
//! cursor's rebuilds); `BAZEL_RS_PARITY_VERIFY=1` builds them again too and
//! compares, op for op, and checks at each walk's end that every generated
//! input names a planned node.

use super::*;

/// Whether merges rewrite contested and losing deltas instead of building
/// them again (`BAZEL_RS_EVAL_SWEEP=0`: built again). Only with the T3
/// merge buffer (`BAZEL_RS_EVAL_NO_BUFFER=1` turns it off too): the T2
/// pipeline's builds ahead of their turn read the index without recording
/// what they read, so a loser publishing its own ids would reach them.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a merge strategy switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_EVAL_SWEEP").is_none_or(|value| value != "0")
            && parallel::buffered()
    });
    // While the analysis snapshot records only with
    // `BAZEL_RS_EVAL_SWEEP_RECORDING=1`: it keeps the walk's plan there too
    // (one rewritten group per tree, Rewrite::rewritten_group) but gains no
    // time with the recording's 4 workers and costs memory (~/code miss
    // peak 9.92 -> 10.0 GB); such deltas are built again at the merge.
    static RECORDING: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a merge strategy switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_EVAL_SWEEP_RECORDING").is_some_and(|value| value == "1")
    });
    *ENABLED && (*RECORDING || !crate::observed_fs::recording())
}

/// `BAZEL_RS_PARITY_VERIFY=1`: every rewritten delta is built again as the
/// T3 cursor builds it and compared; every walk's planned nodes are checked
/// for producer ids naming no node.
pub(super) fn verify() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostics check; it changes no result (it panics on a difference).
        std::env::var_os("BAZEL_RS_PARITY_VERIFY").is_some_and(|value| value == "1")
    });
    *VERIFY
}

/// BAZEL_RS_TIMING (`PERF-PARALLEL sweep`): deltas rewritten as readers,
/// localized as losers, built again (fallbacks), verified; ids rewritten.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 10] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 10];
pub(super) const S_READER: usize = 0;
pub(super) const S_LOSER: usize = 1;
pub(super) const S_FALLBACK: usize = 2;
pub(super) const S_VERIFIED: usize = 3;
pub(super) const S_ARTIFACTS: usize = 4;
/// µs: the reads checked, the outputs replayed, the rewrites (of them
/// waiting for lowerings).
const S_READS_US: usize = 5;
const S_LOCALIZE_US: usize = 6;
const S_REWRITE_US: usize = 7;
const S_LOWERING_US: usize = 8;
/// Workers' tool runfiles groups rewritten for the build (to the memo's).
const S_GROUPS: usize = 9;

fn stat_since(index: usize, started: std::time::Instant) {
    stat(index, started.elapsed().as_micros() as u64);
}

pub(super) fn stat(index: usize, by: u64) {
    STATS[index].fetch_add(by, std::sync::atomic::Ordering::Relaxed);
}

/// The `PERF-PARALLEL sweep` line.
pub(super) fn report() {
    let get = |index: usize| STATS[index].load(std::sync::atomic::Ordering::Relaxed);
    if (0..STATS.len()).all(|index| get(index) == 0) {
        return;
    }
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL sweep readers={} losers={} fallbacks={} verified={} artifacts_rewritten={} groups_rewritten={} reads={:.3}s localize={:.3}s rewrite={:.3}s lowering_wait={:.3}s",
        get(S_READER),
        get(S_LOSER),
        get(S_FALLBACK),
        get(S_VERIFIED),
        get(S_ARTIFACTS),
        get(S_GROUPS),
        get(S_READS_US) as f64 / 1e6,
        get(S_LOCALIZE_US) as f64 / 1e6,
        get(S_REWRITE_US) as f64 / 1e6,
        get(S_LOWERING_US) as f64 / 1e6,
    ));
}

/// The provisional reads found canonical at a merge
/// ([`ActionPlanning::canonical_reads`]), by the identity of the artifact
/// read (kept alive here, so the identity is never reused).
#[derive(Default)]
pub(super) struct CanonicalReads(crate::fast_hash::FxHashMap<usize, Artifact>);

impl CanonicalReads {
    fn key(artifact: &Artifact) -> usize {
        Arc::as_ptr(&artifact.0) as usize
    }

    /// Whether `artifact`, read as `file`'s, was found canonical (its
    /// identity, and the File it was found for: a reused identity is not).
    fn contains(&self, file: &File, artifact: &Artifact) -> bool {
        self.0
            .get(&Self::key(artifact))
            .is_some_and(|known| known.file == *file && Arc::ptr_eq(&known.0, &artifact.0))
    }

    fn insert(&mut self, artifact: &Artifact) {
        self.0.insert(Self::key(artifact), artifact.clone());
    }
}

/// Producer action ids a delta names → the ids its merge names.
pub(super) type Aliases = crate::fast_hash::FxHashMap<String, String>;

/// Add `from → to` to `aliases`; `false` if `from` maps elsewhere already.
fn alias(aliases: &mut Aliases, from: &str, to: &str) -> bool {
    match aliases.get(from) {
        Some(known) => known == to,
        None => {
            aliases.insert(from.to_owned(), to.to_owned());
            true
        }
    }
}

/// The reads of a worker's delta (of plan entry `reader`) at its merge
/// ([`delta::check_reads`] with every earlier entry merged): `Some` with
/// the producer ids to rewrite when every read is the index's entry now or
/// differs from it in its producer id only; `None` otherwise (built again).
pub(super) fn read_aliases(
    index: &ConcurrentArtifactIndex,
    canonical: &mut CanonicalReads,
    reads: &[delta::ProvisionalRead],
    reader: u32,
) -> Option<Aliases> {
    let mut aliases = Aliases::default();
    for read in reads {
        match &read.artifact {
            // Found canonical at an earlier merge: canonical entries never
            // change and readers merge in plan order.
            Some(artifact) if canonical.contains(&read.file, artifact) => {}
            Some(artifact) => {
                let (now, provisional, stamp) = index.current(&read.file)?;
                if stamp >= reader {
                    return None;
                }
                if same_artifact(&now, artifact) {
                    if !provisional {
                        canonical.insert(artifact);
                    }
                    continue;
                }
                // A still provisional entry is never replaced by another
                // provisional one: a difference there is not a rewrite.
                if provisional {
                    return None;
                }
                let (Some((read_id, read_index)), Some((now_id, now_index))) =
                    (artifact.producer.as_ref(), now.producer.as_ref())
                else {
                    return None;
                };
                if read_index != now_index
                    || !same_artifact(&artifact.with_producer_id(now_id), &now)
                    || !alias(&mut aliases, read_id, now_id)
                {
                    return None;
                }
            }
            None => {
                // Read as missing: still not visible before the reader.
                if index
                    .current(&read.file)
                    .is_some_and(|(_, _, stamp)| stamp < reader)
                {
                    return None;
                }
            }
        }
    }
    Some(aliases)
}

/// What a losing delta's merge decides of its outputs ([`localize`]).
pub(super) struct Localized {
    /// Each `Output` op's registration (by op position): `Some(stamp)`
    /// shared with the action registered at that plan index.
    shared_at: Vec<Option<u32>>,
    /// The delta's shared actions (their nodes are not planned).
    shared: crate::fast_hash::FxHashSet<String>,
}

/// The walk's output registration of `delta` (a worker's, built without
/// the build's producers) over `producers`: `None` when it is not a plain
/// replay — an error, an output the build failed on, a sharing within the
/// delta, a conflict or a partial share (built again: its exact error and
/// partial state) —, else the shared actions with their winners added to
/// `aliases`.
pub(super) fn localize(
    producers: &Producers,
    delta: &delta::TargetDelta,
    aliases: &mut Aliases,
) -> Option<Localized> {
    if delta.error.is_some() || delta.failed_output.is_some() {
        return None;
    }
    let mut shared_at = Vec::new();
    // Per action id: (whether its first output was shared, with whom).
    let mut decided: crate::fast_hash::FxHashMap<&str, Option<String>> = Default::default();
    for op in &delta.ops {
        match op {
            delta::DeltaOp::Shared(..) => return None,
            delta::DeltaOp::Output(path, producer) => {
                let registration = check_output(producers, path.path(), producer).ok()?;
                let (sharing, stamp) = match registration {
                    OutputRegistration::New => (None, None),
                    OutputRegistration::Shared(id, registered) => (Some(id), Some(registered)),
                };
                let Some((id, _)) = producer.action.as_ref() else {
                    // A native producer is never shared.
                    if sharing.is_some() {
                        return None;
                    }
                    shared_at.push(None);
                    continue;
                };
                match decided.get(id.as_str()) {
                    // The partial-share rule: every output as the first.
                    Some(first) if *first != sharing => return None,
                    Some(_) => {}
                    None => {
                        decided.insert(id.as_str(), sharing.clone());
                    }
                }
                shared_at.push(stamp);
            }
            _ => {}
        }
    }
    let mut shared = crate::fast_hash::FxHashSet::default();
    for (id, winner) in decided {
        if let Some(winner) = winner {
            if !alias(aliases, id, &winner) {
                return None;
            }
            shared.insert(id.to_owned());
        }
    }
    Some(Localized { shared_at, shared })
}

/// A lowering's outcome (lowering::Pending's item).
type Outcome = std::thread::Result<Result<lowering::Lowered, String>>;

/// A lowering received: handed on as one in flight.
fn pending(outcome: Outcome) -> lowering::Pending {
    let (sender, receiver) = std::sync::mpsc::channel();
    let _ = sender.send(outcome);
    receiver
}

/// The rewrite of one delta's producer ids, shared with the placement of
/// its lowerings in flight ([`Rewrite::place`]).
pub(super) struct Rewrite {
    aliases: Aliases,
    /// The delta's shared actions: their lowered nodes are not placed.
    dropped: crate::fast_hash::FxHashSet<String>,
    caches: std::sync::Mutex<Caches>,
    /// The build's tool runfiles groups: a group naming a mapped id is
    /// rewritten once for the build, to the memo's group of its tree
    /// ([`Self::rewritten_group`]).
    groups: Arc<delta::InputGroups>,
}

/// Each artifact and shared input group rewritten once per delta, by
/// identity (the original kept: the identity is never reused while it is a
/// key).
#[derive(Default)]
struct Caches {
    artifacts: crate::fast_hash::FxHashMap<usize, (Artifact, Artifact)>,
    groups: crate::fast_hash::FxHashMap<
        usize,
        (Arc<scheduler::InputGroup>, Arc<scheduler::InputGroup>),
    >,
}

impl Rewrite {
    fn touched(&self, artifact: &Artifact) -> bool {
        artifact
            .producer
            .as_ref()
            .is_some_and(|(id, _)| self.aliases.contains_key(id))
    }

    fn caches(&self) -> std::sync::MutexGuard<'_, Caches> {
        self.caches.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn artifact(&self, artifact: &mut Artifact) {
        let Some((id, _)) = artifact.producer.as_ref() else {
            return;
        };
        let Some(to) = self.aliases.get(id) else {
            return;
        };
        let key = Arc::as_ptr(&artifact.0) as usize;
        let rewritten = self
            .caches()
            .artifacts
            .entry(key)
            .or_insert_with(|| {
                stat(S_ARTIFACTS, 1);
                (artifact.clone(), artifact.with_producer_id(to))
            })
            .1
            .clone();
        *artifact = rewritten;
    }

    fn artifacts(&self, artifacts: &mut Artifacts) {
        if artifacts.iter().any(|artifact| self.touched(artifact)) {
            let mut list = (**artifacts).clone();
            for artifact in &mut list {
                self.artifact(artifact);
            }
            *artifacts = Arc::new(list);
        }
    }

    fn dependencies(&self, dependencies: &mut [scheduler::Dependency]) {
        for dependency in dependencies {
            if let Some(to) = self.aliases.get(&dependency.action_id) {
                dependency.action_id = to.clone();
            }
        }
    }

    fn names_alias(&self, group: &scheduler::InputGroup) -> bool {
        group
            .dependencies
            .iter()
            .any(|dependency| self.aliases.contains_key(&dependency.action_id))
    }

    fn group(&self, group: &mut Arc<scheduler::InputGroup>) {
        let key = Arc::as_ptr(group) as usize;
        if let Some((_, rewritten)) = self.caches().groups.get(&key) {
            *group = Arc::clone(rewritten);
            return;
        }
        let rewritten = if self.names_alias(group) {
            self.rewritten_group(group)
        } else {
            Arc::clone(group)
        };
        self.caches()
            .groups
            .insert(key, (Arc::clone(group), Arc::clone(&rewritten)));
        *group = rewritten;
    }

    /// `group` (naming a mapped id) as the walk shares it: the walk fills
    /// its tree's group once, over the canonical producers, and every
    /// action using the tree holds that one group. A worker's fill from
    /// provisional reads is rewritten once for the build (every delta
    /// holding it gets the same group: the ids it names map alike in every
    /// delta, canonical entries never change), and the memo keeps the
    /// rewritten group from then on — or the rewrite is the memo's group
    /// when it is the same (filled again over the canonical producers since)
    /// —, so later fills and earlier refills hold one group as in the walk.
    fn rewritten_group(&self, group: &Arc<scheduler::InputGroup>) -> Arc<scheduler::InputGroup> {
        let key = Arc::as_ptr(group) as usize;
        if let Some((_, known)) = self
            .groups
            .rewritten
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && !self.names_alias(known)
        {
            return Arc::clone(known);
        }
        let mut copy = (**group).clone();
        self.dependencies(&mut copy.dependencies);
        let rewritten = match self.groups.provisional_fill(group) {
            Some((identity, expanded)) => {
                let settled = self.groups.settle(&identity, |now| match now {
                    Some(now) if crate::snapshot::codec::same_group(&now.group, &copy) => {
                        Arc::clone(now)
                    }
                    _ => Arc::new(ExpandedTree {
                        tree: Arc::clone(&expanded.tree),
                        group: Arc::new(copy),
                        reads: expanded
                            .reads
                            .iter()
                            .map(|read| {
                                let mut read = read.clone();
                                if let Some(artifact) = read.artifact.as_mut() {
                                    self.artifact(artifact);
                                }
                                read
                            })
                            .collect(),
                    }),
                });
                stat(S_GROUPS, 1);
                Arc::clone(&settled.group)
            }
            None => Arc::new(copy),
        };
        self.groups
            .rewritten
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (Arc::clone(group), Arc::clone(&rewritten)));
        rewritten
    }

    fn node(&self, planned: &mut PlannedAction, sources: &mut Option<(String, SourceInputs)>) {
        self.dependencies(&mut planned.dependencies);
        for group in &mut planned.groups {
            self.group(group);
        }
        if let Some((_, sources)) = sources {
            for group in &mut sources.groups {
                self.group(group);
            }
        }
    }

    fn runfiles_nodes(&self, nodes: &mut RunfilesNodes) {
        for artifact in [nodes.tree.as_mut(), nodes.manifest.as_mut()]
            .into_iter()
            .flatten()
        {
            self.artifact(artifact);
        }
        self.artifact(&mut nodes.repo_mapping);
        self.artifact(&mut nodes.executable);
        for (_, artifact) in &mut nodes.entries {
            if let Some(artifact) = artifact {
                self.artifact(artifact);
            }
        }
    }

    /// Whether the lowering of a shared action (not placed).
    pub(super) fn drops(&self, lowered: &lowering::Lowered) -> bool {
        let id = match lowered {
            lowering::Lowered::Node { planned, .. } => &planned.action.id,
            lowering::Lowered::Listing(listing) => &listing.action.id,
        };
        self.dropped.contains(id)
    }

    /// A lowered node of the delta placed (its listed path's nodes
    /// rewritten where they are built, [`Self::placed`]).
    pub(super) fn lowered(&self, lowered: &mut lowering::Lowered) {
        if let lowering::Lowered::Node { planned, sources } = lowered {
            self.node(planned, sources);
        }
    }

    /// A node the placement of a listed action built.
    pub(super) fn placed(
        &self,
        planned: &mut PlannedAction,
        sources: &mut Option<(String, SourceInputs)>,
    ) {
        self.node(planned, sources);
        debug_assert!(
            !self.dropped.contains(&planned.action.id)
                && !planned
                    .dependencies
                    .iter()
                    .chain(planned.groups.iter().flat_map(|group| &group.dependencies))
                    .any(|dependency| self.aliases.contains_key(&dependency.action_id)),
            "internal error: the sweep placed {} unrewritten",
            planned.action.id
        );
    }
}

/// `delta` as its merge in the walk would have built it: `localized`'s
/// shared actions dropped (their outputs `Shared`, their declarations
/// replacing only when predeclared) and every producer id of `aliases`
/// rewritten — its lowerings in flight where they are placed.
pub(super) fn rewrite(
    delta: delta::TargetDelta,
    localized: Option<&Localized>,
    aliases: Aliases,
    groups: &Arc<delta::InputGroups>,
) -> delta::TargetDelta {
    let delta::TargetDelta {
        ops,
        error,
        unchecked,
        uses,
        failed_output,
        reads: _,
    } = delta;
    let rewrite = Arc::new(Rewrite {
        aliases,
        dropped: localized
            .map(|localized| localized.shared.clone())
            .unwrap_or_default(),
        caches: Default::default(),
        groups: Arc::clone(groups),
    });
    let dropped = |id: &str| rewrite.dropped.contains(id);
    let own_shared = |artifact: &Artifact| {
        artifact
            .producer
            .as_ref()
            .is_some_and(|(id, _)| dropped(id))
    };
    let mut out = Vec::with_capacity(ops.len());
    let mut unplaced = Vec::new();
    let mut outputs = 0usize;
    for op in ops {
        match op {
            delta::DeltaOp::Output(path, producer) => {
                let shared = localized.and_then(|localized| localized.shared_at[outputs]);
                outputs += 1;
                match shared {
                    Some(registered) => out.push(delta::DeltaOp::Shared(
                        path.path().to_path_buf(),
                        registered,
                    )),
                    None => out.push(delta::DeltaOp::Output(path, producer)),
                }
            }
            delta::DeltaOp::Artifact(file, mut artifact, hash) => {
                rewrite.artifact(&mut artifact);
                out.push(delta::DeltaOp::Artifact(file, artifact, hash));
            }
            delta::DeltaOp::Declared {
                key,
                mut artifact,
                replace,
                predeclared,
            } => {
                let replace = if own_shared(&artifact) {
                    predeclared
                } else {
                    replace
                };
                rewrite.artifact(&mut artifact);
                out.push(delta::DeltaOp::Declared {
                    key,
                    artifact,
                    replace,
                    predeclared,
                });
            }
            delta::DeltaOp::Dry(mut dry) => {
                rewrite.artifacts(&mut dry);
                out.push(delta::DeltaOp::Dry(dry));
            }
            delta::DeltaOp::RunfilesNodes(mut nodes) => {
                rewrite.runfiles_nodes(&mut nodes);
                out.push(delta::DeltaOp::RunfilesNodes(nodes));
            }
            delta::DeltaOp::Node(mut planned, mut sources) => {
                if dropped(&planned.action.id) {
                    // The listing marker the listed path pushed before it.
                    if matches!(out.last(), Some(delta::DeltaOp::ListedTreeAction)) {
                        out.pop();
                    }
                    continue;
                }
                rewrite.node(&mut planned, &mut sources);
                out.push(delta::DeltaOp::Node(planned, sources));
            }
            // A shared action's lowering is never placed (nor waited
            // for: the walk never lowers it, so it cannot fail the build).
            delta::DeltaOp::Lowering(pending, Some(id)) if dropped(&id) => {
                // (Its lowered nodes freed off the cursor.)
                unplaced.push(pending);
            }
            delta::DeltaOp::Lowering(pending, _) | delta::DeltaOp::Rewritten(pending, _) => {
                out.push(delta::DeltaOp::Rewritten(pending, Arc::clone(&rewrite)));
            }
            delta::DeltaOp::ActionExecution(id, _) | delta::DeltaOp::BuildInfoAction(id)
                if dropped(&id) => {}
            other => out.push(other),
        }
    }
    if !unplaced.is_empty() {
        super::drop_elsewhere(unplaced);
    }
    let rewritten = delta::TargetDelta {
        ops: out,
        error,
        unchecked,
        uses,
        failed_output,
        reads: Vec::new(),
    };
    if cfg!(debug_assertions) || verify() {
        check_rewritten(&rewritten, &rewrite);
    }
    rewritten
}

/// Debug builds, BAZEL_RS_PARITY_VERIFY: a rewritten delta names no mapped
/// (or dropped) producer id, plans no shared action and registers none of
/// its outputs as new.
fn check_rewritten(delta: &delta::TargetDelta, rewrite: &Rewrite) {
    let fail = |what: String| -> ! { panic!("internal error: the sweep's rewrite left {what}") };
    let artifact = |what: &str, artifact: &Artifact| {
        if rewrite.touched(artifact) {
            fail(format!(
                "{what} {} naming {:?}",
                artifact.exec.display(),
                artifact.producer
            ));
        }
    };
    let node = |planned: &PlannedAction| {
        if rewrite.dropped.contains(&planned.action.id) {
            fail(format!("the shared action {}'s node", planned.action.id));
        }
        for dependency in planned
            .dependencies
            .iter()
            .chain(planned.groups.iter().flat_map(|group| &group.dependencies))
        {
            if rewrite.aliases.contains_key(&dependency.action_id) {
                fail(format!(
                    "input {} of {} naming {}",
                    dependency.destination.display(),
                    planned.action.id,
                    dependency.action_id
                ));
            }
        }
    };
    for op in &delta.ops {
        match op {
            delta::DeltaOp::Output(path, producer) => {
                if producer
                    .action
                    .as_ref()
                    .is_some_and(|(id, _)| rewrite.dropped.contains(id))
                {
                    fail(format!(
                        "the shared output {} registered new",
                        path.path().display()
                    ));
                }
            }
            delta::DeltaOp::Artifact(_, registered, _) => artifact("the artifact", registered),
            delta::DeltaOp::Declared {
                artifact: declared, ..
            } => artifact("the declaration", declared),
            delta::DeltaOp::Dry(dry) => {
                for file in dry.iter() {
                    artifact("the file to build", file);
                }
            }
            delta::DeltaOp::RunfilesNodes(nodes) => {
                artifact("the executable", &nodes.executable);
                for (_, entry) in &nodes.entries {
                    if let Some(entry) = entry {
                        artifact("the runfile", entry);
                    }
                }
            }
            delta::DeltaOp::Node(planned, _) => node(planned),
            delta::DeltaOp::ActionExecution(id, _) | delta::DeltaOp::BuildInfoAction(id)
                if rewrite.dropped.contains(id) =>
            {
                fail(format!("the shared action {id}'s execution"))
            }
            delta::DeltaOp::Lowering(_, Some(id)) if rewrite.dropped.contains(id) => {
                fail(format!("the shared action {id}'s lowering"))
            }
            _ => {}
        }
    }
}

/// The merge's adoption of a worker's delta (entry `reader`; the module's
/// doc): `Ok` merged as returned (with whether it was rewritten), `Err`
/// built again (the delta given back when it was not consumed). `meets`:
/// an output path meets a producer.
#[allow(clippy::result_large_err)]
pub(super) fn adopt(
    state: &mut ActionPlanning,
    delta: delta::TargetDelta,
    reader: u32,
    meets: bool,
) -> Result<(delta::TargetDelta, bool), Option<delta::TargetDelta>> {
    let started = std::time::Instant::now();
    let Some(mut aliases) = (if delta.reads.is_empty() {
        Some(Aliases::default())
    } else {
        read_aliases(
            &state.artifact_index.shared,
            &mut state.canonical_reads,
            &delta.reads,
            reader,
        )
    }) else {
        return Err(Some(delta));
    };
    stat_since(S_READS_US, started);
    let started = std::time::Instant::now();
    let localized = if meets {
        match localize(&state.producers, &delta, &mut aliases) {
            Some(localized) => Some(localized),
            None => return Err(Some(delta)),
        }
    } else {
        None
    };
    if aliases.is_empty() && localized.is_none() {
        let mut delta = delta;
        delta.reads.clear();
        return Ok((delta, false));
    }
    stat_since(S_LOCALIZE_US, started);
    // No chains (a loser's id never names a winner) and no shared action
    // kept: else built again (an internal inconsistency, asserted).
    let chained = aliases.values().any(|to| aliases.contains_key(to));
    debug_assert!(
        !chained,
        "internal error: a producer id alias chain: {aliases:?}"
    );
    if chained {
        return Err(Some(delta));
    }
    stat(if meets { S_LOSER } else { S_READER }, 1);
    let started = std::time::Instant::now();
    let rewritten = rewrite(delta, localized.as_ref(), aliases, &state.input_groups);
    stat_since(S_REWRITE_US, started);
    Ok((rewritten, true))
}

/// A comparable rendering of `delta`'s ops (BAZEL_RS_PARITY_VERIFY): its
/// lowerings received (and handed on) and shown as placed (a rewritten
/// delta's dropped or rewritten), shared input groups by content.
pub(super) fn rendered(delta: &mut delta::TargetDelta) -> Vec<String> {
    let mut lines = Vec::with_capacity(delta.ops.len());
    for op in &mut delta.ops {
        let (receiver, rewrite) = match op {
            delta::DeltaOp::Lowering(receiver, _) => (receiver, None),
            delta::DeltaOp::Rewritten(receiver, rewrite) => (receiver, Some(Arc::clone(rewrite))),
            delta::DeltaOp::Node(planned, sources) => {
                lines.push(node_line(planned, sources.as_ref()));
                continue;
            }
            other => {
                lines.push(delta::rendered(other));
                continue;
            }
        };
        let started = std::time::Instant::now();
        let outcome = std::mem::replace(receiver, std::sync::mpsc::channel().1)
            .recv()
            .unwrap_or_else(|_| Err(Box::new("a lowering worker stopped".to_owned())));
        stat_since(S_LOWERING_US, started);
        if let Some(line) = lowered_line(&outcome, rewrite.as_deref()) {
            lines.push(line);
        }
        *receiver = pending(outcome);
    }
    lines
}

/// A lowering's product as its placement places it.
fn lowered_line(outcome: &Outcome, rewrite: Option<&Rewrite>) -> Option<String> {
    Some(match outcome {
        Ok(Ok(lowered)) if rewrite.is_some_and(|rewrite| rewrite.drops(lowered)) => return None,
        Ok(Ok(lowering::Lowered::Node { planned, sources })) => match rewrite {
            Some(rewrite) => {
                let (mut planned, mut sources) = (planned.clone(), sources.clone());
                rewrite.placed(&mut planned, &mut sources);
                node_line(&planned, sources.as_ref())
            }
            None => node_line(planned, sources.as_ref()),
        },
        Ok(Ok(lowering::Lowered::Listing(listing))) => {
            let mut own = listing.own.clone();
            if let Some(rewrite) = rewrite {
                for artifact in &mut own {
                    rewrite.artifact(artifact);
                }
            }
            format!("listing {} {own:?}", listing.action.id)
        }
        Ok(Err(error)) => format!("lowering error {error}"),
        Err(_) => "lowering panic".to_owned(),
    })
}

fn node_line(planned: &PlannedAction, sources: Option<&(String, SourceInputs)>) -> String {
    format!(
        "node {planned:?} {:?}",
        sources.map(|(id, sources)| (id, &sources.own, &sources.groups))
    )
}

/// BAZEL_RS_PARITY_VERIFY: `adopted` (the merge's rewrite of a worker's
/// delta of `key`) against `rebuilt` (built again at the merge, as the T3
/// cursor does): every op in order and the error.
pub(super) fn compare(
    key: &ConfiguredTargetKey,
    adopted: &mut delta::TargetDelta,
    rebuilt: &mut delta::TargetDelta,
) {
    let (a, b) = (rendered(adopted), rendered(rebuilt));
    let fail = |what: &str, adopted: &dyn std::fmt::Debug, rebuilt: &dyn std::fmt::Debug| -> ! {
        panic!(
            "internal error: BAZEL_RS_PARITY_VERIFY: the rewritten delta of {} differs from the one built again in {what}:\nrewritten: {adopted:#?}\nrebuilt:   {rebuilt:#?}",
            key.label
        )
    };
    if a.len() != b.len() {
        fail("its operations", &a, &b);
    }
    for (position, (x, y)) in a.iter().zip(&b).enumerate() {
        if x != y {
            fail(&format!("operation {position}"), x, y);
        }
    }
    if adopted.error != rebuilt.error {
        fail("its error", &adopted.error, &rebuilt.error);
    }
    stat(S_VERIFIED, 1);
}

/// BAZEL_RS_PARITY_VERIFY at a walk's end: every generated input of every
/// planned node (its own, its shared groups'), every files-to-build and
/// runfiles artifact names a planned node's action (the workspace status
/// excepted).
pub(super) fn check_ids(state: &ActionPlanning, action_executions: &ActionExecutions) {
    let nodes = state.nodes.planned();
    // The planned nodes', and the runfiles supports' (planned at the end).
    let mut ids: crate::fast_hash::FxHashSet<&str> =
        nodes.iter().map(|node| node.action.id.as_str()).collect();
    for nodes in state.runfiles_nodes.values() {
        for artifact in [nodes.tree.as_ref(), nodes.manifest.as_ref()]
            .into_iter()
            .flatten()
            .chain([&nodes.repo_mapping])
        {
            if let Some((id, _)) = &artifact.producer {
                ids.insert(id);
            }
        }
    }
    let known = |id: &str| ids.contains(id) || id.starts_with(crate::request::STATUS_ID_PREFIX);
    let fail = |what: String| -> ! {
        panic!("internal error: BAZEL_RS_PARITY_VERIFY: {what} names no planned action")
    };
    for node in nodes {
        for dependency in node
            .dependencies
            .iter()
            .chain(node.groups.iter().flat_map(|group| &group.dependencies))
        {
            if !known(&dependency.action_id) {
                fail(format!(
                    "input {} of {} ({})",
                    dependency.destination.display(),
                    node.action.id,
                    dependency.action_id
                ));
            }
        }
    }
    let artifact = |what: &str, artifact: &Artifact| {
        if let Some((id, _)) = &artifact.producer
            && !known(id)
        {
            fail(format!("{what} {} ({id})", artifact.exec.display()));
        }
    };
    for (key, dry) in state.dry.iter() {
        for file in dry.iter() {
            artifact(&format!("a file to build of {}", key.label), file);
        }
    }
    // The index's canonical entries (output groups, validation, coverage
    // and the requested files resolve through it at the end), the
    // declarations (top-level output-file targets), the per-action tables.
    state.artifact_index.shared.for_each_shard(|shard| {
        for entry in shard.iter() {
            // (Task D: published by an early job of an entry after the
            // walk, merged by a later walk.)
            if entry.provisional && entry.stamp as usize >= state.done {
                continue;
            }
            artifact(
                &format!("the index entry of {}", entry.file.path),
                &entry.artifact,
            );
        }
    });
    for (key, declared) in state.declared.entries.iter() {
        artifact(
            &format!("the declaration of {}", key.path.display()),
            declared,
        );
    }
    for id in action_executions.keys() {
        if !ids.contains(id.as_str()) {
            fail(format!("the execution table's {id}"));
        }
    }
    for id in state.source_inputs.keys() {
        if !ids.contains(id.as_str()) {
            fail(format!("the source inputs' {id}"));
        }
    }
    for (key, nodes) in state.runfiles_nodes.iter() {
        let what = format!("a runfiles artifact of {}", key.label);
        artifact(&what, &nodes.executable);
        for (_, entry) in &nodes.entries {
            if let Some(entry) = entry {
                artifact(&what, entry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(path: &str, owner: &str, id: &str, index: usize) -> Artifact {
        Artifact::new(
            PathBuf::from(path),
            PathBuf::from(path),
            Some((id.to_owned(), index)),
            owner.to_owned(),
            false,
            false,
        )
    }

    /// A read of a provisional entry the merge since made canonical with
    /// another producer maps to it; any other difference does not.
    #[test]
    fn reads_map_to_the_canonical_producer_only() {
        let index = ConcurrentArtifactIndex::default();
        let loser = generated("bazel-out/k8/bin/p/out", "//p:b", "//p:b#0", 0);
        let winner = loser.with_producer_id("//p:a#0");
        let file = loser.file();
        // Published by entry 5, read by entry 9, merged canonical (the winner's id).
        index.publish_provisional(file_hash(&file), &file, &loser, 5);
        index.register(file_hash(&file), &file, &winner, u32::MAX, 5, false);
        let read = delta::ProvisionalRead {
            file: file.clone(),
            artifact: Some(loser.clone()),
            stamp: 5,
        };
        let fresh = || CanonicalReads::default();
        let aliases =
            read_aliases(&index, &mut fresh(), std::slice::from_ref(&read), 9).expect("a rewrite");
        assert_eq!(aliases.get("//p:b#0").map(String::as_str), Some("//p:a#0"));
        // The reader before the publisher's stamp: not its view.
        assert!(read_aliases(&index, &mut fresh(), std::slice::from_ref(&read), 5).is_none());
        // Another output index is not a rewrite.
        let other = delta::ProvisionalRead {
            artifact: Some(generated("bazel-out/k8/bin/p/out", "//p:b", "//p:b#0", 1)),
            ..read.clone()
        };
        assert!(read_aliases(&index, &mut fresh(), &[other], 9).is_none());
        // A File read as missing that is registered now: built again.
        let missing = delta::ProvisionalRead {
            file: file.clone(),
            artifact: None,
            stamp: u32::MAX,
        };
        assert!(read_aliases(&index, &mut fresh(), &[missing], 9).is_none());
        // Two reads mapping one id two ways: built again.
        let second = generated("bazel-out/k8/bin/p/out2", "//p:b", "//p:b#0", 1);
        let second_file = second.file();
        index.publish_provisional(file_hash(&second_file), &second_file, &second, 5);
        index.register(
            file_hash(&second_file),
            &second_file,
            second.with_producer_id("//p:c#0"),
            u32::MAX,
            5,
            false,
        );
        let both = [
            read.clone(),
            delta::ProvisionalRead {
                file: second_file,
                artifact: Some(second),
                stamp: 5,
            },
        ];
        assert!(read_aliases(&index, &mut fresh(), &both, 9).is_none());
    }

    /// A loser's outputs replay the walk's registration: its shared action
    /// maps to the winner, its outputs become `Shared`, its node and
    /// execution go, its declarations replace only when predeclared, and
    /// its other ops name the winner.
    #[test]
    fn a_loser_is_localized_as_the_walk_registers_it() {
        let path = "bazel-out/k8/bin/p/out";
        let mut producers = Producers::new();
        producers.set_stamp(Some(3));
        producers.insert(
            PathBuf::from(path),
            super::super::tests::writing_producer("//p:a", "//p:a#0", &[path], "same"),
        );
        producers.set_stamp(Some(8));
        let loser = super::super::tests::writing_producer("//p:b", "//p:b#0", &[path], "same");
        let action = Action {
            id: "//p:b#0".to_owned(),
            ..Action::default()
        };
        let own = generated(path, "//p:b", "//p:b#0", 0);
        let file = own.file();
        let planned = PlannedAction {
            action: action.clone(),
            dependencies: Vec::new(),
            deferred: None,
            groups: Vec::new(),
        };
        let delta = delta::TargetDelta {
            ops: vec![
                delta::DeltaOp::Output(HashedPath::new(PathBuf::from(path)), Arc::clone(&loser)),
                delta::DeltaOp::Artifact(file.clone(), own.clone(), file_hash(&file)),
                delta::DeltaOp::Declared {
                    key: DeclaredKey::new(
                        crate::configuration::Configuration::default().id,
                        PathBuf::from(path),
                    ),
                    artifact: own.clone(),
                    replace: true,
                    predeclared: false,
                },
                delta::DeltaOp::Node(Box::new(planned), None),
                delta::DeltaOp::Dry(Arc::new(vec![own.clone()])),
            ],
            error: None,
            unchecked: true,
            uses: Vec::new(),
            failed_output: None,
            reads: Vec::new(),
        };
        let mut aliases = Aliases::default();
        let localized = localize(&producers, &delta, &mut aliases).expect("a replay");
        assert_eq!(aliases.get("//p:b#0").map(String::as_str), Some("//p:a#0"));
        let rewritten = rewrite(delta, Some(&localized), aliases, &Arc::default());
        let kinds: Vec<&str> = rewritten.ops.iter().map(delta::DeltaOp::kind).collect();
        assert_eq!(kinds, ["shared", "artifact", "declared", "dry"]);
        for op in &rewritten.ops {
            match op {
                delta::DeltaOp::Shared(_, registered) => assert_eq!(registered, &3),
                delta::DeltaOp::Artifact(_, artifact, _) => {
                    assert!(same_artifact(artifact, &own.with_producer_id("//p:a#0")))
                }
                delta::DeltaOp::Declared {
                    artifact, replace, ..
                } => {
                    assert!(!replace);
                    assert!(same_artifact(artifact, &own.with_producer_id("//p:a#0")));
                }
                delta::DeltaOp::Dry(dry) => {
                    assert!(same_artifact(&dry[0], &own.with_producer_id("//p:a#0")))
                }
                _ => unreachable!(),
            }
        }
        // A conflicting producer (a native one) is not a replay: built again.
        let mut conflicting = Producers::new();
        conflicting.insert(PathBuf::from(path), OutputProducer::native("//p:g"));
        conflicting.set_stamp(Some(8));
        let delta = delta::TargetDelta {
            ops: vec![delta::DeltaOp::Output(
                HashedPath::new(PathBuf::from(path)),
                loser,
            )],
            error: None,
            unchecked: true,
            uses: Vec::new(),
            failed_output: None,
            reads: Vec::new(),
        };
        assert!(localize(&conflicting, &delta, &mut Aliases::default()).is_none());
    }
}
