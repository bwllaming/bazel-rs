//! T3 (docs/EVALUATOR.md §8, "T3: the plan-ordered merge buffer"): in a
//! frozen walk, phase 2 is computed in dependency order and merged in plan
//! order.
//!
//! The pipeline (P3/P4/S2/T2) has the coordinator classify every entry,
//! dispatch it, commit finished ones ahead of their turn where the order
//! cannot show and replay them at the turn: with every entry's prepare on
//! the workers (S3) the coordinator's bookkeeping is the walk's bottleneck.
//! Here the workers schedule themselves:
//!
//! - **Classification** on the workers over the frozen view (chunks of the
//!   window ahead of the cursor): a Starlark rule's requirements (the keys
//!   its prepare reads: dependency edges after the alias chain, output
//!   files' generating rules, resolved toolchains), a native rule's whose
//!   results a worker computes (`frozen::publishes_native`); every other
//!   entry is the coordinator's at its turn.
//! - **Dispatch on publication**: an entry whose requirements all have
//!   results (published by a worker, or committed) runs on a worker — a
//!   Starlark rule's prepare, compute and delta, a native rule's results —
//!   and publishes its own results, which readies its dependents.
//! - **The cursor** merges in plan order only: it takes the entry's
//!   finished job (waiting for it if it runs, running it itself if it is
//!   still queued), applies its held effects and commits it at its turn
//!   (`commit_turn`: the producers' exact check, stale provisional reads
//!   rebuilt, conflict texts); an entry without a job is prepared and
//!   computed there. The pass-2 ops apply at their positions; the first
//!   error stops the walk (what later jobs hold is dropped: nothing of it
//!   was applied).
//! - **Bounds**: nothing is classified beyond `cursor + window` (16,384,
//!   `BAZEL_RS_T3_WINDOW`), so at most that many results wait for their
//!   merge; the workers' queue takes the earliest plan index first (the
//!   cursor's entries).
//!
//! `BAZEL_RS_EVAL_NO_BUFFER=1` walks with the pipeline instead.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

/// Whether a frozen walk uses the merge buffer.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_EVAL_NO_BUFFER").is_none()
    });
    *ENABLED
}

/// How far past the cursor entries are classified (and so computed).
fn window() -> usize {
    static WINDOW: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling bound; the output is the same either way.
        std::env::var("BAZEL_RS_T3_WINDOW")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(16_384usize)
            .max(1)
    });
    *WINDOW
}

/// Entries per classification job.
const CHUNK: usize = 64;

// An entry's stage.
const IDLE: u8 = 0;
const CLASSIFYING: u8 = 1;
const QUEUED: u8 = 2;
const RUNNING: u8 = 3;
const DONE: u8 = 4;
const CLAIMED: u8 = 5;

// An entry's job.
const KIND_TURN: u8 = 0;
const KIND_STARLARK: u8 = 1;
const KIND_NATIVE: u8 = 2;

/// BAZEL_RS_TIMING (`PERF-PARALLEL buffer`).
static STATS: [AtomicU64; STAT_NAMES.len()] = [const { AtomicU64::new(0) }; STAT_NAMES.len()];
const STAT_NAMES: [&str; 12] = [
    "starlark",
    "native",
    "turn",
    "dispatched",
    "stolen",
    "waited",
    "wait_us",
    "at_turn",
    "aborted",
    "invalidated",
    "walks",
    "native_unpublished",
];
const S_STARLARK: usize = 0;
const S_NATIVE: usize = 1;
const S_TURN: usize = 2;
const S_DISPATCHED: usize = 3;
const S_STOLEN: usize = 4;
const S_WAITED: usize = 5;
const S_WAIT_US: usize = 6;
const S_AT_TURN: usize = 7;
const S_ABORTED: usize = 8;
const S_INVALIDATED: usize = 9;
const S_WALKS: usize = 10;
const S_NATIVE_UNPUBLISHED: usize = 11;

fn stat(index: usize, by: u64) {
    STATS[index].fetch_add(by, Ordering::Relaxed);
}

/// The cursor's progress of the walks (BAZEL_RS_TIMING): seconds since the
/// walk started at each tenth of its entries.
static PROGRESS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// BAZEL_RS_TIMING: the entries at their turn by why and rule class (count,
/// µs).
static AT_TURN: std::sync::Mutex<BTreeMap<String, (u64, u64)>> =
    std::sync::Mutex::new(BTreeMap::new());

fn note_at_turn(why: &str, rule: &Rule, elapsed: std::time::Duration) {
    if crate::plan_timings::enabled() {
        let mut classes = AT_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let entry = classes
            .entry(format!("{why}:{}", rule.rule_class_name()))
            .or_default();
        entry.0 += 1;
        entry.1 += elapsed.as_micros() as u64;
    }
}

/// The `PERF-PARALLEL buffer` lines.
pub(super) fn report() {
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    if get(&STATS[S_WALKS]) == 0 {
        return;
    }
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL buffer {}",
        STAT_NAMES
            .iter()
            .zip(&STATS)
            .map(|(name, value)| format!("{name}={}", get(value)))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let mut classes: Vec<(String, (u64, u64))> = AT_TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(class, value)| (class.clone(), *value))
        .collect();
    classes.sort_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL buffer at_turn {}",
        classes
            .iter()
            .take(16)
            .map(|(class, (count, us))| format!("{class}={count}:{:.3}s", *us as f64 / 1e6))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL buffer classified_turn {}",
        TURN_REASONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(why, count)| format!("{why}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for line in PROGRESS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        crate::snapshot::diag::line_fmt(format_args!("PERF-PARALLEL buffer cursor {line}"));
    }
}

/// `BAZEL_RS_T3_DEBUG=1`: the cursor's longest waits and entries at their
/// turn, with what readied them.
fn debug() -> bool {
    static DEBUG: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostics switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_T3_DEBUG").is_some()
    });
    *DEBUG
}

/// A finished job.
enum Finished {
    Starlark(Box<Result<frozen::FrozenOutcome, String>>),
    Native,
}

/// What the cursor found for its entry.
enum Claimed {
    /// No job: prepared (and computed) at its turn.
    Turn,
    Starlark(Box<Result<frozen::FrozenOutcome, String>>),
    Native,
}

/// Which entries wait for which keys' results.
#[derive(Default)]
struct Sched {
    waiting: HashMap<ConfiguredTargetKey, Vec<usize>>,
    missing: HashMap<usize, usize>,
}

/// The walk's shared state: what the workers' jobs and the cursor share.
struct Board {
    frozen: Arc<Frozen>,
    /// The plan entries from `start` (as the walk started).
    plan: Vec<(ConfiguredTargetKey, Arc<Rule>)>,
    start: usize,
    stage: Box<[AtomicU8]>,
    kind: Box<[AtomicU8]>,
    sched: std::sync::Mutex<Sched>,
    finished: std::sync::Mutex<HashMap<usize, Finished>>,
    finished_cv: std::sync::Condvar,
    /// What classifying an entry observed, applied at its turn.
    stash: std::sync::Mutex<HashMap<usize, Vec<Effects>>>,
    jobs: Arc<JobQueue>,
    /// The walk ended (its error, or done): queued jobs are skipped.
    stop: AtomicBool,
    cancelled: Arc<AtomicBool>,
    jitter: u64,
    verify_summary: bool,
    /// BAZEL_RS_T3_DEBUG: when each entry became ready and on which key.
    readied: std::sync::Mutex<HashMap<usize, (std::time::Instant, Option<ConfiguredTargetKey>)>>,
    /// BAZEL_RS_T3_DEBUG: (µs, what) of the cursor's slow steps.
    slow: std::sync::Mutex<Vec<(u64, String)>>,
}

impl Board {
    fn note_ready(&self, index: usize, by: Option<&ConfiguredTargetKey>) {
        if debug() {
            self.readied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(index, (std::time::Instant::now(), by.cloned()));
        }
    }

    fn note_slow(&self, index: usize, what: &str, elapsed: std::time::Duration) {
        if !debug() || elapsed < std::time::Duration::from_millis(2) {
            return;
        }
        // The chain of what readied it (each by the key whose results
        // came last), back to a classification or a commit.
        let mut ready = String::new();
        {
            let readied = self.readied.lock().unwrap_or_else(|e| e.into_inner());
            let mut at = index;
            for _ in 0..8 {
                let Some((when, by)) = readied.get(&at) else {
                    ready.push_str(" <- (not readied: at its turn)");
                    break;
                };
                ready.push_str(&format!(
                    " <- ready {:.0}ms ago by {}",
                    when.elapsed().as_secs_f64() * 1e3,
                    by.as_ref()
                        .map_or("classification".to_owned(), |key| key.label.to_string())
                ));
                let Some(next) = by
                    .as_ref()
                    .and_then(|key| self.plan.iter().position(|(other, _)| other == key))
                else {
                    break;
                };
                at = self.start + next;
            }
        }
        let label = self
            .slot(index)
            .map_or("?".to_owned(), |slot| self.plan[slot].0.label.to_string());
        self.slow.lock().unwrap_or_else(|e| e.into_inner()).push((
            elapsed.as_micros() as u64,
            format!("{what} #{index} {label}{ready}"),
        ));
    }

    fn slot(&self, index: usize) -> Option<usize> {
        index
            .checked_sub(self.start)
            .filter(|slot| *slot < self.plan.len())
    }

    fn end(&self) -> usize {
        self.start + self.plan.len()
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed)
    }

    fn submit(&self, index: usize, job: Job) {
        // A closed queue (the pool stops): the cursor runs it itself.
        let _ = self.jobs.push(index, job);
    }

    /// Classify entries `from..to` (a worker's job).
    fn classify_chunk(self: &Arc<Self>, from: usize, to: usize) {
        for index in from..to {
            if self.stopped() {
                return;
            }
            let Some(slot) = self.slot(index) else {
                continue;
            };
            if self.stage[slot]
                .compare_exchange(IDLE, CLASSIFYING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let (key, rule) = &self.plan[slot];
            let (kind, keys, stash) = classify(&self.frozen, index, key, rule);
            if let Some(effects) = stash {
                self.stash
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(index)
                    .or_default()
                    .push(effects);
            }
            stat(
                match kind {
                    KIND_STARLARK => S_STARLARK,
                    KIND_NATIVE => S_NATIVE,
                    _ => S_TURN,
                },
                1,
            );
            self.kind[slot].store(kind, Ordering::Release);
            if kind == KIND_TURN {
                self.stage[slot].store(IDLE, Ordering::Release);
                continue;
            }
            // Registered and released under the lock: a publication after
            // it finds the entry waiting (it readies it), one before it is
            // seen as results here.
            let ready = {
                let mut sched = self.sched.lock().unwrap_or_else(|e| e.into_inner());
                let mut missing = 0;
                for key in keys {
                    if self.frozen.results.get(&key).is_none() {
                        sched.waiting.entry(key).or_default().push(index);
                        missing += 1;
                    }
                }
                if missing > 0 {
                    sched.missing.insert(index, missing);
                }
                self.stage[slot].store(IDLE, Ordering::Release);
                missing == 0
            };
            if ready {
                self.note_ready(index, None);
                self.dispatch(index);
            }
        }
    }

    /// Queue entry `index`'s job (unless the cursor claimed it).
    fn dispatch(self: &Arc<Self>, index: usize) {
        let Some(slot) = self.slot(index) else {
            return;
        };
        if self.stage[slot]
            .compare_exchange(IDLE, QUEUED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        stat(S_DISPATCHED, 1);
        let board = Arc::clone(self);
        self.submit(
            index,
            Box::new(move || {
                if board.stopped()
                    || board.stage[slot]
                        .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return;
                }
                let finished = board.run(index);
                let mut done = board.finished.lock().unwrap_or_else(|e| e.into_inner());
                done.insert(index, finished);
                board.stage[slot].store(DONE, Ordering::Release);
                drop(done);
                board.finished_cv.notify_all();
            }),
        );
    }

    /// Run entry `index`'s job and publish its results.
    fn run(self: &Arc<Self>, index: usize) -> Finished {
        let slot = index - self.start;
        let (key, rule) = &self.plan[slot];
        let frozen = &self.frozen;
        match self.kind[slot].load(Ordering::Acquire) {
            KIND_NATIVE => {
                match frozen::run_native(frozen, index, key, rule) {
                    Some(published) => {
                        frozen::publish_native_results(frozen, index, key, *published);
                        self.mark(key);
                    }
                    None => stat(S_NATIVE_UNPUBLISHED, 1),
                }
                Finished::Native
            }
            _ => {
                let stopped = || self.stopped();
                let (result, _, _) = frozen::run_frozen(
                    frozen,
                    index,
                    key,
                    rule,
                    self.jitter,
                    self.verify_summary,
                    &stopped,
                );
                if let Ok(outcome) = &result
                    && let (Ok(Some(parts)), Some(Ok(worker))) =
                        (&outcome.prepared, &outcome.worker)
                    && frozen::publish_starlark_results(
                        frozen,
                        index,
                        parts,
                        worker,
                        outcome.platform_known,
                    )
                {
                    self.mark(key);
                }
                Finished::Starlark(Box::new(result))
            }
        }
    }

    /// `key`'s results exist (published or committed): dispatch the
    /// entries that waited only for them.
    fn mark(self: &Arc<Self>, key: &ConfiguredTargetKey) {
        let ready: Vec<usize> = {
            let mut sched = self.sched.lock().unwrap_or_else(|e| e.into_inner());
            let Some(waiters) = sched.waiting.remove(key) else {
                return;
            };
            let mut ready = Vec::new();
            for waiter in waiters {
                if let Some(missing) = sched.missing.get_mut(&waiter) {
                    *missing -= 1;
                    if *missing == 0 {
                        sched.missing.remove(&waiter);
                        ready.push(waiter);
                    }
                }
            }
            ready
        };
        for index in ready {
            self.note_ready(index, Some(key));
            self.dispatch(index);
        }
    }

    /// The cursor's entry `index`: its finished job (waited for, or run
    /// here when still queued), or none (at its turn).
    fn claim(self: &Arc<Self>, index: usize) -> Claimed {
        let Some(slot) = self.slot(index) else {
            return Claimed::Turn;
        };
        loop {
            match self.stage[slot].load(Ordering::Acquire) {
                IDLE => {
                    if self.stage[slot]
                        .compare_exchange(IDLE, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Claimed::Turn;
                    }
                }
                CLASSIFYING => std::thread::yield_now(),
                QUEUED => {
                    if self.stage[slot]
                        .compare_exchange(QUEUED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        stat(S_STOLEN, 1);
                        return match self.run(index) {
                            Finished::Native => Claimed::Native,
                            Finished::Starlark(outcome) => Claimed::Starlark(outcome),
                        };
                    }
                }
                RUNNING | DONE => {
                    let started = std::time::Instant::now();
                    let mut waited = false;
                    let mut done = self.finished.lock().unwrap_or_else(|e| e.into_inner());
                    let finished = loop {
                        if let Some(finished) = done.remove(&index) {
                            break finished;
                        }
                        waited = true;
                        done = self
                            .finished_cv
                            .wait(done)
                            .unwrap_or_else(|e| e.into_inner());
                    };
                    drop(done);
                    if waited {
                        stat(S_WAITED, 1);
                        stat(S_WAIT_US, started.elapsed().as_micros() as u64);
                        self.note_slow(index, "wait", started.elapsed());
                    }
                    self.stage[slot].store(CLAIMED, Ordering::Release);
                    return match finished {
                        Finished::Native => Claimed::Native,
                        Finished::Starlark(outcome) => Claimed::Starlark(outcome),
                    };
                }
                _ => return Claimed::Turn,
            }
        }
    }

    fn take_stash(&self, index: usize) -> Vec<Effects> {
        self.stash
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&index)
            .unwrap_or_default()
    }
}

/// Classify entry `index` on a worker: its job and the keys whose results
/// it reads, with what the lookups observed (`Some`: the entry is then at
/// its turn, where they are repeated).
fn classify(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    rule: &Rule,
) -> (u8, Vec<ConfiguredTargetKey>, Option<Effects>) {
    let (kind, keys, stash, why) = classify_why(frozen, index, key, rule);
    if kind == KIND_TURN && crate::plan_timings::enabled() {
        let mut reasons = TURN_REASONS.lock().unwrap_or_else(|e| e.into_inner());
        *reasons
            .entry(format!("{why}:{}", rule.rule_class_name()))
            .or_default() += 1;
    }
    (kind, keys, stash)
}

/// BAZEL_RS_TIMING: why entries are classified to their turn.
static TURN_REASONS: std::sync::Mutex<BTreeMap<String, u64>> =
    std::sync::Mutex::new(BTreeMap::new());

fn classify_why(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    rule: &Rule,
) -> (u8, Vec<ConfiguredTargetKey>, Option<Effects>, &'static str) {
    let turn = |why| (KIND_TURN, Vec::new(), None, why);
    if frozen
        .configured
        .get(key)
        .is_none_or(|node| node.incompatible.is_some())
    {
        return turn("incompatible");
    }
    let engine = &frozen.engine;
    let starlark = matches!(rule.kind, RuleKind::Custom);
    if starlark {
        // Attribute aspects apply at the prepare (outputs registered in
        // other packages): at its turn.
        let Some(custom) = rule.custom.as_ref() else {
            return turn("no_custom");
        };
        if custom
            .schema
            .values()
            .any(|spec| !spec.declaration.aspects.is_empty())
        {
            return turn("attribute_aspects");
        }
    } else if !frozen::publishes_native(engine, rule, key.config) {
        return turn("native");
    }
    let isolation = Isolation::enter();
    let mut cx = prepare::SharedPrepare::frozen(frozen, index);
    let keys = requirements(engine, &mut cx, key, rule, starlark);
    let aborted = cx.record.abort.is_some();
    let (effects, sentinel) = isolation.finish();
    let quiet =
        effects.console.is_empty() && effects.observations.is_empty() && effects.roots.is_empty();
    let stash = (!quiet).then_some(effects);
    let why = match (&keys, cx.record.abort) {
        (_, Some(why)) => why,
        (None, None) => "requirements",
        _ if sentinel => "sentinel",
        _ if !quiet => "observed",
        _ => "",
    };
    match keys {
        Some(keys) if !aborted && !sentinel && quiet => (
            if starlark { KIND_STARLARK } else { KIND_NATIVE },
            keys,
            None,
            "",
        ),
        _ => (KIND_TURN, Vec::new(), stash, why),
    }
}

/// The keys whose results `key`'s prepare (a Starlark rule's,
/// `Engine::ahead_requirements`) or results (a native rule's,
/// `Engine::native_requirements`) read, over the frozen view.
fn requirements(
    engine: &Engine,
    cx: &mut prepare::SharedPrepare<'_>,
    key: &ConfiguredTargetKey,
    rule: &Rule,
    starlark: bool,
) -> Option<Vec<ConfiguredTargetKey>> {
    let edges = engine.rule_edges(rule, key.config);
    let exec_config = engine
        .exec_configurations
        .get(key)
        .copied()
        .unwrap_or(key.config);
    let mut labels: Vec<(Label, ConfigId)> = Vec::with_capacity(edges.len());
    if starlark {
        let dependencies =
            Engine::custom_dependencies_in(cx, rule, key, &edges, exec_config).ok()?;
        if !dependencies.edge_aspects.is_empty() {
            return None;
        }
        labels.extend(
            dependencies
                .labels
                .into_iter()
                .map(|(label, config, _, _)| (label, config)),
        );
        let groups = engine.exec_group_contexts.get(key);
        for context in std::iter::once(engine.toolchain_contexts.get(key))
            .flatten()
            .chain(groups.iter().flat_map(|groups| {
                groups
                    .groups
                    .iter()
                    .filter_map(|group| group.context.as_ref())
            }))
        {
            for (_, implementation) in &context.resolved {
                labels.push((implementation.clone(), key.config));
            }
        }
    } else {
        for edge in &edges {
            let label = engine.resolve_rule_src(rule, &edge.label).ok()?;
            let config = match &edge.cfg {
                EdgeCfg::Target => key.config,
                EdgeCfg::Exec(None) => exec_config,
                EdgeCfg::Exec(Some(group)) => engine
                    .group_exec_configurations
                    .get(&(key.clone(), group.clone()))
                    .copied()
                    .unwrap_or(key.config),
                EdgeCfg::NoConfig => prepare::Lookup::no_config(cx).ok()?,
                EdgeCfg::Starlark(_) => return None,
            };
            labels.push((label, config));
        }
        if let Some(context) = engine.toolchain_contexts.get(key) {
            for implementation in context.resolved_labels() {
                labels.push((implementation, key.config));
            }
        }
    }
    let mut keys = Vec::with_capacity(labels.len());
    for (label, config) in labels {
        // Only rules and output files have results: a label of a loaded
        // package that is neither is a source file (or missing: its
        // prepare reports it), whether or not its lookup was memoized.
        let package = engine.packages.get(&label.package_id())?;
        if package.get(&label).is_some() {
            keys.push(ConfiguredTargetKey::new(label, config));
        } else if let Some((owner, _)) = engine.generated.get(&label) {
            keys.push(ConfiguredTargetKey::new(owner.clone(), config));
        }
    }
    Some(keys)
}

impl Engine {
    /// A frozen walk through the merge buffer (the module's doc).
    pub(super) fn plan_targets_buffered(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
    ) -> Result<(), String> {
        let frozen = self
            .frozen_walk
            .clone()
            .ok_or_else(|| "internal error: a buffered walk without its frozen view".to_owned())?;
        let (jobs, cancelled) = match self.analysis_pool.as_ref() {
            Some(pool) => (Arc::clone(&pool.jobs), Arc::clone(&pool.cancelled)),
            None => (
                Arc::new(JobQueue::default()),
                Arc::new(AtomicBool::new(false)),
            ),
        };
        let start = state.done;
        let plan: Vec<(ConfiguredTargetKey, Arc<Rule>)> = self
            .planning_plan
            .as_ref()
            .map(|plan| plan.get(start..).unwrap_or_default().to_vec())
            .unwrap_or_default();
        let len = plan.len();
        let board = Arc::new(Board {
            frozen,
            plan,
            start,
            stage: (0..len).map(|_| AtomicU8::new(IDLE)).collect(),
            kind: (0..len).map(|_| AtomicU8::new(KIND_TURN)).collect(),
            sched: Default::default(),
            finished: Default::default(),
            finished_cv: Default::default(),
            stash: Default::default(),
            jobs,
            stop: AtomicBool::new(false),
            cancelled,
            jitter: jitter_millis(),
            verify_summary: verify_ahead(),
            readied: Default::default(),
            slow: Default::default(),
        });
        stat(S_WALKS, 1);
        let started = std::time::Instant::now();
        let mut progress = Vec::new();
        let result = self.buffered_walk(state, configured, &board, started, &mut progress);
        board.stop.store(true, Ordering::Relaxed);
        if debug() {
            let mut slow =
                std::mem::take(&mut *board.slow.lock().unwrap_or_else(|e| e.into_inner()));
            let total: u64 = slow.iter().map(|(us, _)| us).sum();
            slow.sort_by_key(|(us, _)| std::cmp::Reverse(*us));
            eprintln!(
                "T3-DEBUG walk of {len}: slow steps {} ({:.3}s)",
                slow.len(),
                total as f64 / 1e6
            );
            for (us, what) in slow.iter().take(40) {
                eprintln!("T3-DEBUG {:.1}ms {what}", *us as f64 / 1e3);
            }
        }
        if result.is_ok() {
            // The jobs left in the queue (classifications passed by the
            // cursor) skip and release the frozen view, so its memos move
            // back into the planning state (Engine::thaw).
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while Arc::strong_count(&board) > 1 && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        if crate::plan_timings::enabled() {
            PROGRESS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!(
                    "entries={len} at(10%..100%)={} total={:.3}s",
                    progress.join("/"),
                    started.elapsed().as_secs_f64()
                ));
        }
        result
    }

    fn buffered_walk(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        board: &Arc<Board>,
        started: std::time::Instant,
        progress: &mut Vec<String>,
    ) -> Result<(), String> {
        let window = window();
        let mut classified = board.start;
        let tenth = board.plan.len().div_ceil(10).max(1);
        while let Some((key, rule)) = self
            .planning_plan
            .as_ref()
            .and_then(|plan| plan.get(state.done))
            .cloned()
        {
            {
                let _split = frozen::Split::enter(1);
                self.drain_lowerings(state, lowering::Drain::Bounded)?;
            }
            if self
                .eval_pass2
                .as_ref()
                .is_some_and(|pending| pending.due(state.done))
            {
                let _split = frozen::Split::enter(2);
                self.drain_lowerings(state, lowering::Drain::All)?;
                self.eval_pass2_turn(state.done);
            }
            let index = state.done;
            state.done += 1;
            crate::debug_panic::at("coordinator");
            frozen::profile(|profile| profile.turn(index));
            if crate::plan_timings::enabled()
                && index >= board.start
                && (index - board.start + 1).is_multiple_of(tenth)
            {
                progress.push(format!("{:.2}", started.elapsed().as_secs_f64()));
            }
            state.artifact_index.at_turn(index);
            state.producers.set_stamp(Some(index));
            state.producers.turn = state.producers.stamp;
            crate::progress::analysis_configured(state.done);
            {
                let _split = frozen::Split::enter(3);
                self.flush_delegations(state);
                for delegated in std::mem::take(&mut state.delegated_keys) {
                    board.frozen.results.publish(&delegated, state);
                    board.mark(&delegated);
                }
            }
            // Classify the window ahead of the cursor.
            let upto = (index + 1 + window).min(board.end());
            while classified < upto {
                let to = (classified + CHUNK).min(upto);
                let (from, chunk) = (classified, Arc::clone(board));
                board.submit(from, Box::new(move || chunk.classify_chunk(from, to)));
                classified = to;
            }
            for effects in board.take_stash(index) {
                effects.apply(self, false);
            }
            let published = board.frozen.results.get(&key).is_some();
            let claimed = {
                let _split = frozen::Split::enter(12);
                board.claim(index)
            };
            let result = match claimed {
                Claimed::Turn => {
                    stat(S_AT_TURN, 1);
                    let _split = frozen::Split::enter(15);
                    let turn_started = std::time::Instant::now();
                    let why = match board.slot(index) {
                        None => "unplanned",
                        Some(slot) if board.kind[slot].load(Ordering::Acquire) == KIND_TURN => {
                            "unclassified"
                        }
                        Some(_) => "unready",
                    };
                    let result = self.buffered_at_turn(state, configured, &key, &rule, index);
                    note_at_turn(why, &rule, turn_started.elapsed());
                    board.note_slow(index, why, turn_started.elapsed());
                    result
                }
                Claimed::Native => {
                    let _split = frozen::Split::enter(14);
                    self.plan_one_target(state, &key, &rule, configured, index as u64)
                }
                Claimed::Starlark(outcome) => {
                    let _split = frozen::Split::enter(13);
                    self.buffered_starlark(state, configured, &key, &rule, index, outcome)
                }
            };
            // Fail fast: the first error in plan order is the walk's; the
            // later entries' jobs held their effects, never applied.
            result?;
            {
                let _split = frozen::Split::enter(11);
                if published {
                    self.check_published(state, &key, &rule);
                }
                board.frozen.results.publish(&key, state);
                board.frozen.results.publish_declared(state);
                board.mark(&key);
            }
        }
        self.flush_delegations(state);
        Ok(())
    }

    /// An entry without a job at its turn: prepared and computed here, as
    /// the sequential walk does.
    fn buffered_at_turn(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
    ) -> Result<(), String> {
        let simple = configured
            .get(key)
            .is_some_and(|node| node.incompatible.is_some())
            || !matches!(rule.kind, RuleKind::Custom);
        if simple {
            return self.plan_one_target(state, key, rule, configured, index as u64);
        }
        let depth = self.track_depth();
        self.track_planning(key);
        let result = (|| {
            use crate::plan_timings::{PlanStage, StageTimer};
            let profiled = crate::profile::active().then(std::time::Instant::now);
            let class = crate::plan_timings::class_id(rule.rule_class_name());
            let (parts, compute) =
                self.prepare_at_turn(state, configured, key, rule, index, class)?;
            let Compute::Inline(snapshot) = compute else {
                return Err(format!(
                    "internal error: the Starlark rule {} prepared at its turn computes elsewhere",
                    key.label
                ));
            };
            let timer = StageTimer::start(PlanStage::Compute, index as u64, class);
            let (snapshot, outcome) = Engine::compute_target(*snapshot);
            timer.finish();
            self.commit_turn(
                state, configured, key, rule, index, class, parts, snapshot, outcome, profiled,
            )
        })();
        self.track_restore(depth);
        result
    }

    /// A Starlark entry whose prepare and compute ran on a worker, at its
    /// turn (`Engine::pipelined_turn_body` for a frozen job's outcome).
    fn buffered_starlark(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        outcome: Box<Result<frozen::FrozenOutcome, String>>,
    ) -> Result<(), String> {
        // A worker's panic is raised at its turn, as a compute's.
        let outcome = match *outcome {
            Ok(outcome) => outcome,
            Err(panic) => panic!("{panic}"),
        };
        let frozen::FrozenOutcome {
            prepared,
            abort,
            owner,
            effects,
            platform_known,
            worker,
            ..
        } = outcome;
        if let Some(why) = abort {
            stat(S_ABORTED, 1);
            frozen::FROZEN_STATS[1].fetch_add(1, Ordering::Relaxed);
            prepare_fallback(why);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            effects.apply(self, false);
            return self.buffered_at_turn(state, configured, key, rule, index);
        }
        frozen::FROZEN_STATS[0].fetch_add(1, Ordering::Relaxed);
        if let Some((action_owner, console, owner_key)) = owner {
            self.console_owners.insert(action_owner.clone(), console);
            self.aquery_owner(&action_owner, &owner_key, Vec::new);
        }
        let (parts, worker) = match (prepared, worker) {
            (Err(error), _) => {
                frozen::FROZEN_STATS[2].fetch_add(1, Ordering::Relaxed);
                effects.apply(self, true);
                return Err(error);
            }
            (Ok(Some(parts)), Some(worker)) => (parts, worker),
            _ => {
                effects.apply(self, false);
                return self.buffered_at_turn(state, configured, key, rule, index);
            }
        };
        if !platform_known && self.platform_resolved(key) {
            // A commit since resolved the target platform its ctx surface
            // saw deferred: prepared again here.
            stat(S_INVALIDATED, 1);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            effects.apply(self, false);
            drop(worker);
            return self.buffered_at_turn(state, configured, key, rule, index);
        }
        effects.apply(self, true);
        let worker = match worker {
            Ok(worker) => worker,
            // The panic the sequential engine raises at this compute.
            Err(panic) => panic!("{panic}"),
        };
        let profiled = crate::profile::active().then(std::time::Instant::now);
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        let WorkerOutcome {
            snapshot,
            outcome,
            effects,
            sentinel,
            ..
        } = worker;
        if verify_ahead() {
            self.verify_ahead(state, configured, key, rule, &parts, &snapshot);
        }
        let (snapshot, outcome) = if sentinel
            && !outcome
                .analyzed
                .as_ref()
                .is_err_and(|error| crate::repository_demand::is_needs_repository(error))
        {
            // A sentinel some caller swallowed: computed again here.
            bump(&stats().recomputed, 1);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            effects.apply(self, false);
            Engine::compute_target(snapshot)
        } else {
            note_effects("compute", &effects);
            effects.apply(self, true);
            (snapshot, outcome)
        };
        self.commit_turn(
            state, configured, key, rule, index, class, *parts, snapshot, outcome, profiled,
        )
    }
}
