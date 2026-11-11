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
            // ~/code: 16,384 held the jobs back behind the cursor (the
            // deferred walk 8.4 -> 7.8 s without it, the same peak RSS).
            .unwrap_or(262_144usize)
            .max(1)
    });
    *WINDOW
}

/// `BAZEL_RS_T3_CLASSIFY_FIRST=1`: the classification chunks ahead of
/// every job (as before task B).
fn classify_first() -> bool {
    static FIRST: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var("BAZEL_RS_T3_CLASSIFY_FIRST").is_ok_and(|value| value == "1")
    });
    *FIRST
}

/// `BAZEL_RS_T3_CLASSIFIERS=<n>`: the threads that classify a walk's
/// entries beside its workers (`0`: the classification chunks are the
/// workers' jobs, queued in plan order among the others).
fn classifiers() -> usize {
    static THREADS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var("BAZEL_RS_T3_CLASSIFIERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2)
    });
    if classify_first() { 0 } else { *THREADS }
}

/// `BAZEL_RS_T3_CLASSIFY_JITTER=<µs>` (a test hook): each classification
/// first sleeps a reproducible time up to that bound.
fn classify_jitter() -> u64 {
    static JITTER: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a hidden test hook delaying classifications; no result depends on it.
        std::env::var("BAZEL_RS_T3_CLASSIFY_JITTER")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    });
    *JITTER
}

/// [`frozen::run_native`], a slow one named on stderr (BAZEL_RS_T3_DEBUG).
fn run_native_timed(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    rule: &Arc<Rule>,
) -> Option<Box<frozen::NativePublished>> {
    let started = std::time::Instant::now();
    let published = frozen::run_native(frozen, index, key, rule);
    if debug() && started.elapsed() > std::time::Duration::from_millis(20) {
        eprintln!(
            "T3-DEBUG slow native #{index} {} {:.1}ms srcs={} sources={}",
            key.label,
            started.elapsed().as_secs_f64() * 1e3,
            rule.srcs.len(),
            published
                .as_ref()
                .map_or(0, |published| published.sources.len())
        );
    }
    published
}

thread_local! {
    /// The board of the buffered walk running on this (the coordinator's)
    /// thread.
    static CURRENT_BOARD: std::cell::RefCell<Option<Arc<Board>>> = const { std::cell::RefCell::new(None) };
}

/// A Starlark rule with attribute aspects (`attr.*(aspects = [...])`).
fn has_attribute_aspects(rule: &Rule) -> bool {
    rule.custom.as_ref().is_some_and(|custom| {
        custom
            .schema
            .values()
            .any(|spec| !spec.declaration.aspects.is_empty())
    })
}

/// `BAZEL_RS_T3_CRITICAL=1`: measure the job DAG's critical path.
fn critical() -> bool {
    static CRITICAL: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
        std::env::var_os("BAZEL_RS_T3_CRITICAL").is_some_and(|value| value == "1")
    });
    *CRITICAL
}

impl Board {
    /// The job DAG's critical path (each entry's work, requirement edges):
    /// the walk's length if nothing waited for plan order; printed with
    /// the total work and its longest chain.
    fn critical_path(&self) {
        let len = self.plan.len();
        let work: Vec<u64> = self
            .work
            .iter()
            .map(|w| w.load(Ordering::Relaxed))
            .collect();
        let mut edges = self
            .edges_kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // By dependent: its requirements (earlier in the plan).
        edges.sort_unstable_by_key(|&(requirement, dependent)| (dependent, requirement));
        let mut finish = vec![0u64; len];
        let mut via = vec![u32::MAX; len];
        let mut next = 0;
        for slot in 0..len {
            let mut start = 0;
            while next < edges.len() && edges[next].1 as usize == slot {
                let requirement = edges[next].0 as usize;
                if requirement < slot && finish[requirement] > start {
                    start = finish[requirement];
                    via[slot] = requirement as u32;
                }
                next += 1;
            }
            finish[slot] = start + work[slot];
        }
        let Some((mut at, longest)) = finish.iter().copied().enumerate().max_by_key(|(_, f)| *f)
        else {
            return;
        };
        let total: u64 = work.iter().sum();
        let mut chain = Vec::new();
        loop {
            chain.push(format!(
                "{}:{:.1}ms",
                self.plan[at].0.label,
                work[at] as f64 / 1e3
            ));
            match via[at] {
                u32::MAX => break,
                previous => at = previous as usize,
            }
        }
        let heavy = {
            let mut heavy: Vec<(u64, usize)> = work.iter().copied().zip(0..).collect();
            heavy.sort_unstable_by_key(|(w, _)| std::cmp::Reverse(*w));
            heavy
                .iter()
                .take(10)
                .map(|(w, slot)| format!("{}:{:.1}ms", self.plan[*slot].0.label, *w as f64 / 1e3))
                .collect::<Vec<_>>()
        };
        eprintln!(
            "PERF-PARALLEL critical entries={len} edges={} path={:.3}s links={} total_work={:.3}s heaviest={}",
            edges.len(),
            longest as f64 / 1e6,
            chain.len(),
            total as f64 / 1e6,
            heavy.join(",")
        );
        eprintln!("PERF-PARALLEL critical chain {}", chain.join(" <- "));
    }
}

/// `BAZEL_RS_T3_CHAIN_WEIGHT=<entries>`: how much earlier in the queue a
/// job runs per Starlark implementation in its chain of dependents.
/// `BAZEL_RS_T3_ASPECT_WEIGHT=<links>`: what a rule with attribute aspects
/// weighs in its dependencies' chains (`Board::chains`; a Starlark rule 1).
fn aspect_weight() -> u32 {
    static WEIGHT: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling bound; the output is the same either way.
        std::env::var("BAZEL_RS_T3_ASPECT_WEIGHT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1)
    });
    *WEIGHT
}

fn chain_weight() -> usize {
    static WEIGHT: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling bound; the output is the same either way.
        std::env::var("BAZEL_RS_T3_CHAIN_WEIGHT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(256)
    });
    *WEIGHT
}

/// `BAZEL_RS_PARITY_REVERSE=1` (a test hook): the ready job latest in plan
/// order runs first (the merges' results must not depend on it).
fn reverse() -> bool {
    static REVERSE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a hidden test hook ordering the jobs; no result depends on it.
        std::env::var_os("BAZEL_RS_PARITY_REVERSE").is_some_and(|value| value == "1")
    });
    *REVERSE
}

/// How long the cursor waits for one entry before it names it on stderr.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(120);

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
/// Not classified yet (the cursor classifies it itself when it gets there
/// first).
const KIND_UNCLASSIFIED: u8 = 3;

/// BAZEL_RS_TIMING (`PERF-PARALLEL buffer`).
static STATS: [AtomicU64; STAT_NAMES.len()] = [const { AtomicU64::new(0) }; STAT_NAMES.len()];
const STAT_NAMES: [&str; 15] = [
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
    "fast_native",
    "later_requirement",
    "held_view",
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
const S_FAST_NATIVE: usize = 12;
const S_LATER_REQUIREMENT: usize = 13;
const S_HELD_VIEW: usize = 14;

/// Whether a plain native rule whose results a worker published merges
/// them ([`Engine::merge_published_native`]) instead of committing again
/// (`BAZEL_RS_VERIFY_DELTA=1`, `BAZEL_RS_VERIFY_AHEAD=1`, `--profile` and
/// `BAZEL_RS_T3_NATIVE_COMMIT=1` commit, checking the published results).
fn fast_natives() -> bool {
    static FAST: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_T3_NATIVE_COMMIT").is_none()
    });
    *FAST && !delta::verify_delta() && !verify_ahead() && !crate::profile::active()
}

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
    super::super::report_shared_copies();
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
pub(super) fn debug() -> bool {
    static DEBUG: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostics switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_T3_DEBUG").is_some()
    });
    *DEBUG
}

/// A finished job.
enum Finished {
    /// `published`: its results were published (its dependents may have
    /// run on them).
    Starlark {
        outcome: Box<Result<frozen::FrozenOutcome, String>>,
        published: bool,
    },
    /// What its merge applies, when its results were published.
    Native(Option<frozen::NativeMerge>),
    /// The job panicked (its text): raised at the entry's turn.
    Panicked(String),
}

/// What the cursor found for its entry.
enum Claimed {
    /// No job: prepared (and computed) at its turn.
    Turn,
    Done(Finished),
}

/// How often an aspect job is queued again before it is left to its turn.
const MAX_REQUEUES: u8 = 16;

impl Finished {
    /// An aspect job's dependency whose results were not published.
    fn unready(&self) -> Option<ConfiguredTargetKey> {
        match self {
            Finished::Starlark { outcome, .. } => outcome
                .as_ref()
                .as_ref()
                .ok()?
                .aspects
                .as_ref()?
                .unready
                .clone(),
            _ => None,
        }
    }

    fn published(&self) -> bool {
        match self {
            Finished::Starlark { published, .. } => *published,
            Finished::Native(registered) => registered.is_some(),
            Finished::Panicked(_) => false,
        }
    }
}

/// The text of the panic an analysis worker just caught (its hook
/// recorded it, parallel::install_worker_panic_hook).
fn worker_panic() -> String {
    WORKER_PANIC
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| "an analysis worker panicked".to_owned())
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
    /// BAZEL_RS_T3_DEBUG: each job's dispatch, start and end.
    job_times: std::sync::Mutex<HashMap<usize, [Option<std::time::Instant>; 3]>>,
    /// BAZEL_RS_T3_DEBUG: (µs, what) of the cursor's slow steps.
    slow: std::sync::Mutex<Vec<(u64, String)>>,
    /// The plan index of each entry's key (a requirement must be earlier).
    index_of: HashMap<ConfiguredTargetKey, usize>,
    /// Classifications that panicked (their text), raised at the turn.
    panics: std::sync::Mutex<HashMap<usize, String>>,
    /// The requirement edges found so far (requirement's slot, entry's
    /// slot) and the classification jobs still to finish: the last one
    /// computes each entry's downstream chain ([`Board::chains`]).
    edges: std::sync::Mutex<Vec<(u32, u32)>>,
    chunks_left: std::sync::atomic::AtomicUsize,
    /// Each entry's longest chain of Starlark implementations through its
    /// dependents (itself included), once every entry is classified: its
    /// job runs ahead of entries this many links earlier in plan order.
    chain: std::sync::OnceLock<Box<[u32]>>,
    /// The cursor's plan index, and the ready entries beyond its window
    /// (dispatched as it advances).
    cursor: std::sync::atomic::AtomicUsize,
    window: usize,
    /// `BAZEL_RS_T3_CRITICAL=1`: each entry's work (µs: its job, and its
    /// merge or turn on the cursor, waits excluded) and the requirement
    /// edges, for the job DAG's critical path ([`Board::critical_path`]).
    work: Box<[AtomicU64]>,
    edges_kept: std::sync::Mutex<Vec<(u32, u32)>>,
    parked: std::sync::Mutex<std::collections::BinaryHeap<std::cmp::Reverse<usize>>>,
    /// BAZEL_RS_TIMING: when the walk started and its last job ended (µs
    /// after it): how long the merges ran on after the last job.
    started: std::time::Instant,
    last_job: AtomicU64,
    /// When the last classification chunk ended (µs since the walk
    /// started; BAZEL_RS_TIMING).
    classified_at: AtomicU64,
    /// Task B: how often each aspect job was queued again (a dependency
    /// its applications read was not published).
    requeues: Box<[AtomicU8]>,
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

    fn note_job(&self, index: usize, step: usize) {
        if debug() {
            self.job_times
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(index)
                .or_default()[step] = Some(std::time::Instant::now());
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
            for _ in 0..16 {
                let Some((when, by)) = readied.get(&at) else {
                    ready.push_str(" <- (not readied: at its turn)");
                    break;
                };
                let job = self
                    .job_times
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&at)
                    .copied()
                    .unwrap_or_default();
                let ago = |at: Option<std::time::Instant>| {
                    at.map_or("-".to_owned(), |at| {
                        format!("{:.0}", at.elapsed().as_secs_f64() * 1e3)
                    })
                };
                ready.push_str(&format!(
                    " <- [job queued/started/ended {}/{}/{}ms ago] ready {:.0}ms ago by {}",
                    ago(job[0]),
                    ago(job[1]),
                    ago(job[2]),
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

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed)
    }

    fn submit(&self, index: usize, job: Job) {
        // A closed queue (the pool stops): the cursor runs it itself.
        let _ = self.jobs.push(index, job);
    }

    /// `BAZEL_RS_T3_CLASSIFIERS` threads of their own classify the walk's
    /// chunks in plan order, beside the workers (`false`: none could be
    /// started, the chunks are the workers' jobs).
    fn spawn_classifiers(self: &Arc<Self>, threads: usize) -> bool {
        let next = Arc::new(std::sync::atomic::AtomicUsize::new(self.start));
        let mut spawned = 0;
        for thread in 0..threads {
            let (board, next) = (Arc::clone(self), Arc::clone(&next));
            let started = std::thread::Builder::new()
                .name(format!("t3-classify-{thread}"))
                .stack_size(64 << 20)
                .spawn(move || {
                    let end = board.start + board.plan.len();
                    loop {
                        let from = next.fetch_add(CHUNK, Ordering::Relaxed);
                        if from >= end || board.stopped() {
                            break;
                        }
                        board.classify_chunk(from, (from + CHUNK).min(end));
                    }
                });
            if started.is_ok() {
                spawned += 1;
            }
        }
        spawned > 0
    }

    /// Classify entries `from..to` (a worker's job, or a classifier's).
    fn classify_chunk(self: &Arc<Self>, from: usize, to: usize) {
        for index in from..to {
            if self.stopped() {
                return;
            }
            self.classify_entry(index);
        }
        if self.chunks_left.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.classified_at
                .store(self.started.elapsed().as_micros() as u64, Ordering::Relaxed);
            self.chains();
        }
    }

    /// Classify entry `index` (a worker's chunk, or the cursor meeting it
    /// unclassified) and dispatch it if ready.
    fn classify_entry(self: &Arc<Self>, index: usize) {
        {
            let Some(slot) = self.slot(index) else {
                return;
            };
            if self.kind[slot].load(Ordering::Acquire) != KIND_UNCLASSIFIED
                || self.stage[slot]
                    .compare_exchange(IDLE, CLASSIFYING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                return;
            }
            // Classified meanwhile (its kind is stored before it is
            // released): once only.
            if self.kind[slot].load(Ordering::Acquire) != KIND_UNCLASSIFIED {
                self.stage[slot].store(IDLE, Ordering::Release);
                return;
            }
            let (key, rule) = &self.plan[slot];
            // A panic is raised at the entry's turn, by the cursor (the
            // entry left to it: never a classification left half done).
            let classified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::debug_panic::at("t3-classify");
                let jitter = classify_jitter();
                if jitter > 0 {
                    // The test hook: the cursor meets entries still being
                    // classified.
                    let roll = loading_jitter_roll(index as u64, 0x5435) % (jitter + 1);
                    std::thread::sleep(std::time::Duration::from_micros(roll));
                }
                let (kind, keys, stash) = classify(&self.frozen, index, key, rule);
                // A requirement later in plan order than the entry cannot
                // be (the plan is a post-order): left to its turn.
                if keys
                    .iter()
                    .any(|key| self.index_of.get(key).is_some_and(|at| *at >= index))
                {
                    stat(S_LATER_REQUIREMENT, 1);
                    debug_assert!(
                        false,
                        "internal error: {} requires a later entry",
                        key.label
                    );
                    return (KIND_TURN, Vec::new(), stash);
                }
                (kind, keys, stash)
            }));
            let (kind, keys, stash) = match classified {
                Ok(classified) => classified,
                Err(_) => {
                    self.panics
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(index, worker_panic());
                    (KIND_TURN, Vec::new(), None)
                }
            };
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
                return;
            }
            {
                let mut edges = self.edges.lock().unwrap_or_else(|e| e.into_inner());
                for key in &keys {
                    if let Some(at) = self.index_of.get(key) {
                        edges.push(((at - self.start) as u32, slot as u32));
                    }
                }
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

    /// Queue entry `index`'s job (unless the cursor claimed it), or park
    /// it until the cursor's window reaches it (the results waiting for
    /// their merge are bounded).
    fn dispatch(self: &Arc<Self>, index: usize) {
        let Some(slot) = self.slot(index) else {
            return;
        };
        if index >= self.cursor.load(Ordering::Acquire) + self.window {
            self.parked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(std::cmp::Reverse(index));
            // The cursor may have moved past the check: release again.
            self.release();
            return;
        }
        if self.stage[slot]
            .compare_exchange(IDLE, QUEUED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        stat(S_DISPATCHED, 1);
        self.note_job(index, 0);
        let board = Arc::clone(self);
        self.submit(
            self.priority(index),
            Box::new(move || {
                if board.stopped()
                    || board.stage[slot]
                        .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return;
                }
                // A panic (the job's, its publication's) is raised at the
                // entry's turn: the cursor always finds the entry done.
                board.note_job(index, 1);
                // The key a rerun found published already (twice: the job
                // is left to its turn).
                let mut published_again: Option<ConfiguredTargetKey> = None;
                let finished = loop {
                    let finished =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| board.run(index)))
                            .unwrap_or_else(|_| Finished::Panicked(worker_panic()));
                    // Task B: an aspect job that read a dependency not
                    // published yet runs again once it is.
                    if let Some(unready) = finished.unready()
                        && published_again.as_ref() != Some(&unready)
                        && board.requeues[slot].fetch_add(1, Ordering::Relaxed) < MAX_REQUEUES
                    {
                        super::aspect_jobs::stat(super::aspect_jobs::S_REQUEUED, 1);
                        if board.wait_for(index, slot, unready.clone()) {
                            return;
                        }
                        published_again = Some(unready);
                        continue;
                    }
                    break finished;
                };
                board.note_job(index, 2);
                let mut done = board.finished.lock().unwrap_or_else(|e| e.into_inner());
                done.insert(index, finished);
                board.stage[slot].store(DONE, Ordering::Release);
                drop(done);
                board.finished_cv.notify_all();
            }),
        );
    }

    /// Task B: entry `index` (running) waits for `key`'s results and runs
    /// again once they are published (`false`: they are now, it runs
    /// again at once).
    fn wait_for(self: &Arc<Self>, index: usize, slot: usize, key: ConfiguredTargetKey) -> bool {
        {
            let mut sched = self.sched.lock().unwrap_or_else(|e| e.into_inner());
            if self.frozen.results.get(&key).is_some() {
                return false;
            }
            sched.waiting.entry(key).or_default().push(index);
            sched.missing.insert(index, 1);
            // Released under the lock: a publication after it readies it.
            self.stage[slot].store(IDLE, Ordering::Release);
        }
        // A cursor waiting for it finds it idle (at its turn).
        let _done = self.finished.lock().unwrap_or_else(|e| e.into_inner());
        self.finished_cv.notify_all();
        true
    }

    /// Run entry `index`'s job and publish its results.
    fn run(self: &Arc<Self>, index: usize) -> Finished {
        let started = critical().then(std::time::Instant::now);
        let finished = self.run_job(index);
        self.last_job
            .fetch_max(self.started.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(started) = started {
            self.work[index - self.start]
                .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        finished
    }

    fn run_job(self: &Arc<Self>, index: usize) -> Finished {
        let slot = index - self.start;
        let (key, rule) = &self.plan[slot];
        let frozen = &self.frozen;
        match self.kind[slot].load(Ordering::Acquire) {
            KIND_NATIVE => match run_native_timed(frozen, index, key, rule) {
                Some(published) => {
                    crate::debug_panic::at("t3-publish");
                    let merge = frozen::publish_native_results(frozen, index, key, *published);
                    self.mark(key);
                    Finished::Native(Some(merge))
                }
                None => {
                    stat(S_NATIVE_UNPUBLISHED, 1);
                    Finished::Native(None)
                }
            },
            _ => {
                let stopped = || self.stopped();
                let (result, _, _) = frozen::run_frozen(
                    frozen,
                    index,
                    key,
                    rule,
                    self.jitter,
                    self.verify_summary,
                    true,
                    true,
                    &stopped,
                );
                let mut published = false;
                if let Ok(outcome) = &result
                    && let (Ok(Some(parts)), Some(Ok(worker))) =
                        (&outcome.prepared, &outcome.worker)
                {
                    crate::debug_panic::at("t3-publish");
                    if frozen::publish_starlark_results(
                        frozen,
                        index,
                        parts,
                        worker,
                        outcome.platform_known,
                    ) {
                        published = true;
                        self.mark(key);
                    }
                }
                // A delta sharing an output with an earlier entry is rebuilt
                // at its merge: once published, only what the rebuild needs
                // is kept (the analysis result, the runfiles support, the
                // outputs check), not the delta too (memory).
                // The sweep merges the delta itself (super::super::sweep):
                // kept whole, the analysis too (a delta it cannot rewrite
                // is built again from it).
                let mut result = result;
                if !super::super::sweep::enabled()
                    && let Ok(outcome) = &mut result
                    && let Some(Ok(worker)) = &mut outcome.worker
                    && let Some(delta) = worker.outcome.delta.as_mut()
                    && let Some(kept) = delta.kept.take()
                {
                    let (analyzed, runfiles, outputs_checked) = *kept;
                    let built = std::mem::replace(
                        &mut delta.built,
                        delta::Built::Fallback("delta_meets_producer"),
                    );
                    super::super::drop_elsewhere(built);
                    worker.outcome.analyzed = Ok(analyzed);
                    worker.outcome.runfiles = runfiles;
                    worker.outcome.outputs_checked = outputs_checked;
                }
                Finished::Starlark {
                    outcome: Box::new(result),
                    published,
                }
            }
        }
    }

    /// The queue's key of entry `index`'s job (the lowest first): its plan
    /// index, earlier by `BAZEL_RS_T3_CHAIN_WEIGHT` (256) entries per
    /// Starlark implementation in its longest chain of dependents (a long
    /// chain starts early, its links then run one after the other).
    fn priority(&self, index: usize) -> usize {
        if reverse() {
            // The test hook: the latest ready entry first.
            return usize::MAX - index;
        }
        let chain = self
            .chain
            .get()
            .and_then(|chain| chain.get(index - self.start).copied())
            .unwrap_or(0) as usize;
        index.saturating_sub(chain.saturating_mul(chain_weight()))
    }

    /// Dispatch the parked entries the cursor's window reaches now.
    fn release(self: &Arc<Self>) {
        let limit = self.cursor.load(Ordering::Acquire) + self.window;
        let released: Vec<usize> = {
            let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
            let mut released = Vec::new();
            while let Some(std::cmp::Reverse(index)) = parked.peek().copied() {
                if index >= limit {
                    break;
                }
                parked.pop();
                released.push(index);
            }
            released
        };
        for index in released {
            self.dispatch(index);
        }
    }

    /// Every entry classified: each one's longest chain of Starlark
    /// implementations through its dependents (a reverse sweep: the plan
    /// is a post-order, a dependent comes after its requirements).
    fn chains(&self) {
        let len = self.plan.len();
        let mut edges = std::mem::take(&mut *self.edges.lock().unwrap_or_else(|e| e.into_inner()));
        edges.sort_unstable();
        if critical() {
            *self.edges_kept.lock().unwrap_or_else(|e| e.into_inner()) = edges.clone();
        }
        let mut chain = vec![0u32; len];
        let mut next = edges.len();
        for slot in (0..len).rev() {
            let rule = &self.plan[slot].1;
            // A rule with attribute aspects analyzes their applications
            // over its dependencies' closure in its job: a heavy link
            // (BAZEL_RS_T3_ASPECT_WEIGHT links).
            let own = if super::aspect_jobs::has_attribute_aspects(rule) {
                aspect_weight()
            } else {
                u32::from(matches!(rule.kind, RuleKind::Custom))
            };
            let mut longest = 0;
            while next > 0 && edges[next - 1].0 as usize == slot {
                next -= 1;
                longest = longest.max(chain[edges[next].1 as usize]);
            }
            // Edges of later requirements were consumed above (sorted by
            // requirement): a slot's edges are contiguous.
            chain[slot] = own + longest;
        }
        let _ = self.chain.set(chain.into_boxed_slice());
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
        let started = std::time::Instant::now();
        let mut warned = false;
        // A watchdog (debug builds, BAZEL_RS_T3_DEBUG): an entry the cursor
        // waits for this long names itself (a job that never finishes
        // would hang the walk).
        let mut watch = |what: &str| {
            if !warned && (cfg!(debug_assertions) || debug()) && started.elapsed() > WATCHDOG {
                warned = true;
                eprintln!(
                    "bazel-rs: internal warning: the merge buffer has waited {}s for plan entry {index} ({}) {what}",
                    started.elapsed().as_secs(),
                    self.plan[slot].0.label
                );
            }
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
                CLASSIFYING => {
                    watch("being classified");
                    std::thread::yield_now();
                }
                QUEUED => {
                    if self.stage[slot]
                        .compare_exchange(QUEUED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        stat(S_STOLEN, 1);
                        let _split = frozen::Split::enter(17);
                        return Claimed::Done(self.run(index));
                    }
                }
                RUNNING | DONE => {
                    let _split = frozen::Split::enter(18);
                    let started = std::time::Instant::now();
                    let mut waited = false;
                    let mut done = self.finished.lock().unwrap_or_else(|e| e.into_inner());
                    let finished = loop {
                        if let Some(finished) = done.remove(&index) {
                            break Some(finished);
                        }
                        // An aspect job queued again (Board::wait_for).
                        if self.stage[slot].load(Ordering::Acquire) == IDLE {
                            break None;
                        }
                        waited = true;
                        done = self
                            .finished_cv
                            .wait_timeout(done, std::time::Duration::from_secs(10))
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                        watch("running on a worker");
                    };
                    drop(done);
                    let Some(finished) = finished else {
                        continue;
                    };
                    if waited {
                        stat(S_WAITED, 1);
                        stat(S_WAIT_US, started.elapsed().as_micros() as u64);
                        self.note_slow(index, "wait", started.elapsed());
                    }
                    self.stage[slot].store(CLAIMED, Ordering::Release);
                    return Claimed::Done(finished);
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
        if !super::aspect_jobs::enabled()
            && custom
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
    cx.aspect_jobs = super::aspect_jobs::enabled();
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
        // Task B: an aspect job's requirements are its edges'; what its
        // applications read beyond them requeues it (Board::wait_for).
        if !dependencies.edge_aspects.is_empty() && !cx.aspect_jobs {
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
        // The per-target tables the merges fill, sized once for the walk
        // (growing them rehashed every entry on the cursor).
        state.dry.reserve(len);
        state.repositories.reserve(len);
        state.custom_providers.reserve(len);
        state.provider_keys.reserve(len);
        state.runfiles_info.reserve(len);
        self.console_owners.reserve(len);
        let index_of: HashMap<ConfiguredTargetKey, usize> = plan
            .iter()
            .enumerate()
            .map(|(slot, (key, _))| (key.clone(), start + slot))
            .collect();
        let board = Arc::new(Board {
            frozen,
            plan,
            start,
            stage: (0..len).map(|_| AtomicU8::new(IDLE)).collect(),
            kind: (0..len).map(|_| AtomicU8::new(KIND_UNCLASSIFIED)).collect(),
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
            job_times: Default::default(),
            slow: Default::default(),
            index_of,
            panics: Default::default(),
            edges: Default::default(),
            chunks_left: std::sync::atomic::AtomicUsize::new(len.div_ceil(CHUNK)),
            chain: std::sync::OnceLock::new(),
            cursor: std::sync::atomic::AtomicUsize::new(start),
            window: window(),
            work: (0..len).map(|_| AtomicU64::new(0)).collect(),
            edges_kept: Default::default(),
            parked: Default::default(),
            started: std::time::Instant::now(),
            last_job: AtomicU64::new(0),
            classified_at: AtomicU64::new(0),
            requeues: (0..len).map(|_| AtomicU8::new(0)).collect(),
        });
        // Every entry is classified at once, ahead of every job (cheap:
        // lookups; what is ready beyond the window waits, parked): the
        // chains of dependents decide the jobs' order.
        let classifying = board.spawn_classifiers(classifiers());
        for from in (start..start + len).step_by(CHUNK) {
            if classifying {
                break;
            }
            let (to, chunk) = ((from + CHUNK).min(start + len), Arc::clone(&board));
            // In plan order among the jobs (a job readied early in the
            // plan does not wait for the classification of the whole
            // walk; BAZEL_RS_T3_CLASSIFY_FIRST=1: every chunk first).
            let priority = if classify_first() { 0 } else { from };
            board.submit(priority, Box::new(move || chunk.classify_chunk(from, to)));
        }
        stat(S_WALKS, 1);
        let sched = thread_schedstat();
        let started = std::time::Instant::now();
        let mut progress = Vec::new();
        let result = self.buffered_walk(state, configured, &board, started, &mut progress);
        board.stop.store(true, Ordering::Relaxed);
        if critical() {
            board.critical_path();
        }
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
            if Arc::strong_count(&board) > 1 {
                // A job still holds the frozen view: its memos are copied
                // back (Engine::thaw), its tables stay shared meanwhile.
                stat(S_HELD_VIEW, 1);
            }
        }
        if crate::plan_timings::enabled() {
            PROGRESS.lock().unwrap_or_else(|e| e.into_inner()).push({
                let now = thread_schedstat();
                format!(
                    "entries={len} at(10%..100%)={} total={:.3}s last_job={:.3}s classified={:.3}s cpu={:.3}s runqueue={:.3}s",
                    progress.join("/"),
                    started.elapsed().as_secs_f64(),
                    board.last_job.load(Ordering::Relaxed) as f64 / 1e6,
                    board.classified_at.load(Ordering::Relaxed) as f64 / 1e6,
                    (now.0 - sched.0) as f64 / 1e9,
                    (now.1 - sched.1) as f64 / 1e9,
                )
            });
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
        // The aspect prefetch of the entries at their turn queues on the
        // walk's workers (Engine::buffered_at_turn).
        struct Current;
        impl Drop for Current {
            fn drop(&mut self) {
                CURRENT_BOARD.with(|cell| cell.borrow_mut().take());
            }
        }
        CURRENT_BOARD.with(|cell| *cell.borrow_mut() = Some(Arc::clone(board)));
        let _current = Current;
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
            self.eval_pass2_track(state.done);
            let index = state.done;
            state.done += 1;
            crate::debug_panic::at("coordinator");
            frozen::profile(|profile| profile.turn(index));
            if crate::plan_timings::enabled()
                && index >= board.start
                && (index - board.start + 1).is_multiple_of(tenth)
            {
                progress.push(format!("{:.2}", started.elapsed().as_secs_f64()));
                crate::perf_mark("pass2.cursor");
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
            // The window follows the cursor.
            board.cursor.store(index, Ordering::Release);
            board.release();
            // An entry not classified yet: classified here (its job, if
            // ready, is then the cursor's own).
            board.classify_entry(index);
            let mut fast = false;
            let body_started = critical().then(std::time::Instant::now);
            let claimed = {
                let _split = frozen::Split::enter(12);
                board.claim(index)
            };
            let claim_done = body_started.map(|started| started.elapsed());
            // After the claim: a classification still running pushed what
            // it observed before releasing the entry.
            for effects in board.take_stash(index) {
                effects.apply(self, false);
            }
            if let Some(panic) = board
                .panics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&index)
            {
                // Its classification panicked on a worker: raised here.
                panic!("{panic}");
            }
            // Whether its dependents may have run on a worker's results
            // (its own job published them, on a worker or stolen here).
            let published = matches!(&claimed, Claimed::Done(finished) if finished.published());
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
                Claimed::Done(Finished::Panicked(panic)) => {
                    // A worker's panic, raised at its turn (P3's rule).
                    panic!("{panic}")
                }
                Claimed::Done(Finished::Native(registered)) => {
                    let _split = frozen::Split::enter(14);
                    match registered {
                        // (With the tracker its planning frame records
                        // the commit's accesses: committed here.)
                        Some(merge)
                            if fast_natives() && matches!(rule.kind, RuleKind::PackageGroup(_)) =>
                        {
                            stat(S_FAST_NATIVE, 1);
                            fast = true;
                            let _split = frozen::Split::enter(16);
                            self.merge_published_group(state, &board.frozen, &key, &rule, merge)
                        }
                        // (A filegroup's output group: its worker selected
                        // the files from its `srcs`' published views.)
                        Some(merge)
                            if fast_natives()
                                && !self.tracking()
                                && (native_results::plain(&rule)
                                    || native_results::selected_output_group(&rule).is_some()) =>
                        {
                            stat(S_FAST_NATIVE, 1);
                            fast = true;
                            let _split = frozen::Split::enter(16);
                            self.merge_published_native(state, &board.frozen, &key, &rule, merge)
                        }
                        registered => {
                            let started = std::time::Instant::now();
                            let result =
                                self.plan_one_target(state, &key, &rule, configured, index as u64);
                            note_at_turn(
                                if registered.is_some() {
                                    "native_commit"
                                } else {
                                    "native_unpublished"
                                },
                                &rule,
                                started.elapsed(),
                            );
                            result
                        }
                    }
                }
                Claimed::Done(Finished::Starlark { outcome, .. }) => {
                    let _split = frozen::Split::enter(13);
                    self.buffered_starlark(state, configured, &key, &rule, index, outcome)
                }
            };
            // Fail fast: the first error in plan order is the walk's; the
            // later entries' jobs held their effects, never applied.
            result?;
            {
                let _split = frozen::Split::enter(19);
                // The committed results replace the published ones unless
                // they are the very same (a worker's delta merged as built,
                // a native rule merged from its published results): a
                // delta rebuilt here names the canonical producers.
                let same = published
                    && board
                        .frozen
                        .results
                        .get(&key)
                        .is_some_and(|results| results.same_dry(state.dry.get(&key)));
                // Its dependents read what a worker published: checked
                // against the commit (the same Files and values) — a merge
                // of the very published files to build (the published delta
                // or results themselves) in debug builds and under
                // BAZEL_RS_VERIFY_DELTA only.
                if published && !fast && (!same || cfg!(debug_assertions) || delta::verify_delta())
                {
                    self.check_published(state, &key, &rule);
                }
                if !same {
                    board.frozen.results.publish(&key, state);
                }
                if !published {
                    board.mark(&key);
                }
                board.frozen.results.publish_declared(state, self);
            }
            if let (Some(started), Some(claim), Some(slot)) =
                (body_started, claim_done, board.slot(index))
            {
                let cursor = started.elapsed().saturating_sub(claim);
                board.work[slot].fetch_add(cursor.as_micros() as u64, Ordering::Relaxed);
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
            // Its attribute aspects' applications analyzed on the workers
            // first (super::aspects), replayed by its prepare.
            if has_attribute_aspects(rule)
                && let Some(board) = CURRENT_BOARD.with(|cell| cell.borrow().clone())
            {
                let submit = |job: Job| board.submit(0, job);
                self.prefetch_aspects(state, configured, key, rule, &submit);
            }
            let prepared = self.prepare_at_turn(state, configured, key, rule, index, class);
            super::aspects::end_prefetch();
            let (parts, compute) = prepared?;
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
            aspects,
            accesses,
            elapsed,
            sources,
            ..
        } = outcome;
        if let Some(why) = abort {
            stat(S_ABORTED, 1);
            if debug() {
                eprintln!("T3-DEBUG aborted #{index} {}: {why:?}", key.label);
            }
            frozen::FROZEN_STATS[1].fetch_add(1, Ordering::Relaxed);
            prepare_fallback(why);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            effects.apply(self, false);
            return self.buffered_at_turn(state, configured, key, rule, index);
        }
        // Task B: an aspect job whose prepare failed or observed something
        // is prepared at its turn (its applications' commits and prints,
        // in order, are the coordinator's).
        if aspects.is_some()
            && (prepared.is_err()
                || worker.is_none()
                || !effects.console.is_empty()
                || !effects.observations.is_empty()
                || !effects.roots.is_empty()
                || super::aspect_jobs::verify_mode())
        {
            super::aspect_jobs::stat(super::aspect_jobs::S_FALLBACK_PREPARE, 1);
            if debug() {
                eprintln!(
                    "T3-DEBUG aspect job {} at its turn: prepared={:?} worker={} console={} observations={}",
                    key.label,
                    prepared.as_ref().err(),
                    worker.is_some(),
                    effects.console.len(),
                    !effects.observations.is_empty()
                );
            }
            effects.apply(self, false);
            return self.buffered_at_turn(state, configured, key, rule, index);
        }
        frozen::FROZEN_STATS[0].fetch_add(1, Ordering::Relaxed);
        // The source lookups its prepare made (Engine::target): what they
        // record, their memoized paths (the same facts whoever looks them
        // up; a fallback below looks them up again, to the same effect).
        for (label, path, stable, side) in sources {
            side.apply(&mut self.missing_sources, &mut self.source_directories);
            if stable {
                self.source_targets.insert(label, path);
            }
        }
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
        // Its applications not committed yet, where its prepare meets them.
        if let Some(record) = &aspects
            && !self.place_aspects(state, rule, index, record)?
        {
            super::aspect_jobs::stat(super::aspect_jobs::S_FALLBACK, 1);
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
        // The incremental tracker: its planning frame, the prepare's
        // accesses in it (in this turn's scopes), then the commit's events,
        // as the sequential walk records them (plan_one_target).
        let depth = self.track_depth();
        self.track_planning(key);
        let started = self.tracking().then(std::time::Instant::now);
        if self.tracking() {
            self.track_replay(incremental::TrackLog::accesses(
                accesses,
                incremental::ScopeContext::current(),
            ));
        }
        let result = self.buffered_commit(state, configured, key, rule, index, *parts, worker);
        if let Some(started) = started {
            self.track_cost(started.elapsed() + elapsed);
        }
        self.track_restore(depth);
        result
    }

    /// [`Self::buffered_starlark`]'s commit of a worker's compute.
    #[allow(clippy::too_many_arguments)]
    fn buffered_commit(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        parts: PreparedParts,
        worker: WorkerOutcome,
    ) -> Result<(), String> {
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
            state, configured, key, rule, index, class, parts, snapshot, outcome, profiled,
        )
    }
}

impl Engine {
    /// The commit of a plain native rule (`Engine::commit_native_target`'s
    /// `native_results::plain` branch, with `prepare_target`'s console
    /// owner) from the results a worker published (the same function of
    /// the same dependencies' results, `native_results`): its artifacts
    /// resolved through the index now (every earlier entry merged: the
    /// canonical producers), the rest as published.
    fn merge_published_native(
        &mut self,
        state: &mut ActionPlanning,
        frozen: &Frozen,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        merge: frozen::NativeMerge,
    ) -> Result<(), String> {
        let frozen::NativeMerge {
            registered,
            sources,
            owner,
        } = merge;
        // The source lookups its commit would have made (Engine::target):
        // what they record, their memoized paths.
        for (label, path, stable, side) in sources {
            side.apply(&mut self.missing_sources, &mut self.source_directories);
            if stable {
                self.source_targets.insert(label, path);
            }
        }
        let results = frozen
            .results
            .get(key)
            .expect("a published native rule's results");
        self.native_owner(key, owner);
        let (dry, mut providers, mut runfiles, validation, repositories) = results.parts();
        if forwards(rule)
            && let Some(actual) = rule.srcs.first()
        {
            // An alias-like target forwards its actual's providers and
            // runfiles as committed (the commit's; its worker published
            // the actual's published ones).
            let actual = ConfiguredTargetKey::new(self.resolve_rule_src(rule, actual)?, key.config);
            providers = state.custom_providers.get(&actual).cloned();
            runfiles = state.runfiles_info.get(&actual).cloned();
        }
        if let Some(repositories) = repositories {
            state.repositories.insert(key.clone(), repositories);
        }
        if let Some(validation) = validation {
            state.validation.insert(key.clone(), validation);
        }
        let canonical = |index: &ArtifactIndex, artifact: &Artifact| {
            if artifact.producer.is_none() {
                return artifact.clone();
            }
            index
                .get(&artifact.file)
                .unwrap_or_else(|| artifact.clone())
        };
        for artifact in &registered {
            let artifact = canonical(&state.artifact_index, artifact);
            register_artifact(&state.artifact_index, &artifact.file, &artifact);
        }
        if let Some(providers) = providers {
            state.custom_providers.insert(key.clone(), providers);
        }
        if let Some(runfiles) = runfiles {
            state.runfiles_info.insert(key.clone(), runfiles);
        }
        // The published files to build, unless a producer changed (one
        // value for both: memory).
        let dry = match dry {
            None => Arc::new(Vec::new()),
            Some(dry) => {
                let resolved: Vec<Artifact> = dry
                    .iter()
                    .map(|artifact| canonical(&state.artifact_index, artifact))
                    .collect();
                if resolved
                    .iter()
                    .zip(dry.iter())
                    .all(|(now, published)| same_artifact(now, published))
                {
                    dry
                } else {
                    Arc::new(resolved)
                }
            }
        };
        state.dry.insert(key.clone(), dry);
        Ok(())
    }

    /// Task B: the commit of a package group whose results a worker
    /// published (`Engine::prepare_target` and its `PackageGroup` commit):
    /// its owner, its transitive repositories, its node resolved again here
    /// (the memo and the budget as the commit leaves them; an error is the
    /// commit's), the published provider and empty files to build.
    fn merge_published_group(
        &mut self,
        state: &mut ActionPlanning,
        frozen: &Frozen,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        merge: frozen::NativeMerge,
    ) -> Result<(), String> {
        debug_assert!(merge.registered.is_empty() && merge.sources.is_empty());
        let results = frozen
            .results
            .get(key)
            .expect("a published package group's results");
        self.native_owner(key, merge.owner);
        let (dry, providers, _, _, repositories) = results.parts();
        if let Some(repositories) = repositories {
            state.repositories.insert(key.clone(), repositories);
        }
        self.resolve_group(&rule.label, &mut Vec::new())?;
        if let Some(providers) = providers {
            state.custom_providers.insert(key.clone(), providers);
        }
        state.dry.insert(key.clone(), dry.unwrap_or_default());
        Ok(())
    }

    /// A native rule's console and aquery owners (`Engine::prepare_target`).
    /// (Computed by its worker: `owner`.)
    fn native_owner(
        &mut self,
        key: &ConfiguredTargetKey,
        (action_owner, console): (String, crate::shared_path::SharedPath),
    ) {
        self.console_owners.insert(action_owner.clone(), console);
        self.aquery_owner(&action_owner, key, Vec::new);
    }
}

/// This thread's time on the CPU and waiting for one (runnable), in
/// nanoseconds (/proc/thread-self/schedstat; BAZEL_RS_TIMING).
fn thread_schedstat() -> (u64, u64) {
    // snapshot-unrecorded: a BAZEL_RS_TIMING diagnostic of the cursor's scheduling; no result depends on it.
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()
        .and_then(|text| {
            let mut fields = text
                .split_whitespace()
                .map(|field| field.parse::<u64>().ok());
            Some((fields.next()??, fields.next()??))
        })
        .unwrap_or_default()
}
