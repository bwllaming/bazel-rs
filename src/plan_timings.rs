//! Per-stage timers inside `Engine::plan_targets` (M5 of the parallel
//! analysis program, docs/PARALLEL_ANALYSIS.md "P3 instrumentation").
//!
//! `module_timings` (src/loading.rs) answers where loading wall time goes;
//! this module answers the same question for phase 2: for every planned
//! target, how much wall time its prepare / compute / commit stages took,
//! so a future P3 (parallel compute) can be sized — and checked against
//! the rule that commit stays sequential. Records are appended only when
//! `BAZEL_RS_TIMING=1` is set; disabled, every hook is one relaxed atomic
//! load and the report never runs, so user-visible output is unchanged.
//!
//! Design notes: timestamps are microseconds on a process-monotonic clock
//! (`Instant`), records are plain data (no allocation per record — the
//! rule-class name is interned by the caller into a `&'static str`-shaped
//! index via `class_id`, backed by a small mutexed map touched once per
//! distinct class), and the accumulator is a single mutexed `Vec` —
//! phase 2 is sequential today, so contention is nil, and the shape
//! already tolerates P3 workers appending concurrently.
//!
//! W0-A (docs/PERF_LAYER1.md) adds three work counters to the same
//! report, under the same switch: the batch sizes of the phase-1
//! `plan_pending` flushes (`PERF-STAGE flush-batch`), and dependency-view
//! requests vs. constructions vs. distinct view keys plus the number of
//! `Artifact::file` conversions (`PERF-STAGE views`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// One stage of one planned target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanStage {
    Prepare,
    Compute,
    Commit,
    /// Compute retries run inline inside a commit (the
    /// NEEDS_TARGET_PLATFORM / no-demand re-runs of `commit_custom_target`):
    /// real compute work, counted in coverage, kept distinct so the
    /// primary compute row stays the plan-loop stage.
    CommitRetry,
    /// One phase-1 `analyze` recursion step, attributed to the configured
    /// target being analyzed (its plan index when it joins the plan):
    /// dependency resolution, select() conditions and loading of packages
    /// phase 1 touches. Nested steps overlap their parent, so per-target
    /// sums can exceed wall; the report's coverage uses the outermost
    /// steps only (see `analyze_depth` in engine.rs) via `outermost`.
    Analyze,
    /// A `plan_pending` flush driven from phase 1 (a `config_setting`'s
    /// Starlark `flag_values` key): the flush's own wall time. The
    /// targets it plans record their own stage rows inside the span, so
    /// this row is never added into the coverage sum.
    PendingFlush,
    /// One `config_conditions` call (select() key analysis) driven from
    /// phase 1; inside an Analyze step, likewise report-only.
    ConfigConditions,
}

impl PlanStage {
    pub fn name(self) -> &'static str {
        match self {
            PlanStage::Prepare => "prepare",
            PlanStage::Compute => "compute",
            PlanStage::Commit => "commit",
            PlanStage::CommitRetry => "commit_retry",
            PlanStage::Analyze => "analyze",
            PlanStage::PendingFlush => "pending_flush",
            PlanStage::ConfigConditions => "config_conditions",
        }
    }
}

/// One timed stage occurrence. `index` is the target's position in the
/// build's plan (`ActionPlanning::done` at its commit); the synthetic
/// stages use `u64::MAX` and carry no index meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStageRecord {
    pub index: u64,
    pub stage: PlanStage,
    /// Interned rule-class id (see `class_id`); 0 = no class (synthetic).
    pub class: u32,
    pub elapsed_us: u64,
    /// Analyze only: this step is not nested inside another Analyze step
    /// (the report sums only outermost steps toward coverage).
    pub outermost: bool,
}

#[derive(Default)]
struct State {
    records: Vec<PlanStageRecord>,
    classes: Vec<String>,
    class_ids: HashMap<String, u32>,
    /// Wall span of the outermost `plan_targets` call (first entry to its
    /// return), microseconds; nested `plan_pending` flushes are inside it.
    span_us: Option<u64>,
    /// W0-A: targets planned by each phase-1 `plan_pending` flush, in
    /// flush order (`record_batch`).
    flush_batches: Vec<u64>,
    /// W0-A: targets planned by the phase-2 (outer) `plan_targets` call.
    phase2_batch: u64,
    /// W0-A: distinct dependency-view keys requested (hashes of the
    /// (label, edge configuration) pair; `note_dependency_view`).
    view_keys: HashSet<u64>,
}

/// W0-A counters (relaxed atomics, bumped only when enabled): dependency
/// views requested (one per edge), views actually built, and
/// `Artifact::file` conversions.
static VIEW_EDGES: AtomicU64 = AtomicU64::new(0);
static VIEW_CONSTRUCTIONS: AtomicU64 = AtomicU64::new(0);
static ARTIFACT_FILES: AtomicU64 = AtomicU64::new(0);

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
    f(guard.get_or_insert_with(State::default))
}
static ENABLED: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("BAZEL_RS_TIMING").is_some())
}

fn now_us() -> u64 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

/// A cheap stage timer: construct at stage entry, `finish` at exit.
/// When disabled this is two atomic loads and no clock read on entry.
pub struct StageTimer {
    stage: PlanStage,
    index: u64,
    class: u32,
    outermost: bool,
    start_us: u64,
}

impl StageTimer {
    pub fn start(stage: PlanStage, index: u64, class: u32) -> Self {
        Self::start_outermost(stage, index, class, false)
    }

    pub fn start_outermost(stage: PlanStage, index: u64, class: u32, outermost: bool) -> Self {
        let start_us = if enabled() { now_us() } else { 0 };
        Self {
            stage,
            index,
            class,
            outermost,
            start_us,
        }
    }

    /// Re-attribute to a plan index learned during the stage (an analyzed
    /// target joins the plan at the end of its analyze step).
    pub fn set_index(&mut self, index: u64) {
        self.index = index;
    }

    pub fn finish(self) {
        if !enabled() {
            return;
        }
        record(PlanStageRecord {
            index: self.index,
            stage: self.stage,
            class: self.class,
            elapsed_us: now_us().saturating_sub(self.start_us),
            outermost: self.outermost,
        });
    }
}

pub fn record(record: PlanStageRecord) {
    if !enabled() {
        return;
    }
    with_state(|state| state.records.push(record));
}

/// Intern a rule-class name to a small id (0 reserved for "no class").
/// Called on the coordinator only, once per planned target at most.
pub fn class_id(name: &str) -> u32 {
    if !enabled() {
        return 0;
    }
    let mut binding = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
    let state = binding.get_or_insert_with(State::default);
    if let Some(id) = state.class_ids.get(name) {
        return *id;
    }
    let id = state.classes.len() as u32 + 1;
    state.classes.push(name.to_owned());
    state.class_ids.insert(name.to_owned(), id);
    id
}

/// Mark the first `plan_targets` entry of the process (a phase-1
/// `plan_pending` flush precedes the phase-2 call, so the span starts
/// there, not at the outer call).
pub fn note_span_start() {
    if enabled() {
        let _ = START.get_or_init(Instant::now);
    }
}

/// Close the span at the end of the outermost (phase-2) `plan_targets`
/// call: elapsed since the first `plan_targets` entry. Flushes driven
/// from phase 1 are inside the span, so their targets' stage records
/// count toward coverage; the flush/config_conditions totals themselves
/// stay separate (they would double-count).
pub fn record_span() {
    if !enabled() {
        return;
    }
    let elapsed_us = now_us();
    with_state(|state| {
        if state.span_us.is_none() {
            state.span_us = Some(elapsed_us);
        }
    });
}

/// W0-A: the number of targets one `plan_targets` call planned. `outer`
/// is the phase-2 call of `plan_build`; every other call is a phase-1
/// `plan_pending` flush (a `config_setting` with Starlark `flag_values`
/// keys), whose batch sizes bound how much work a parallel compute (P3)
/// could overlap (docs/PERF_LAYER1.md).
pub fn record_batch(outer: bool, targets: u64) {
    if !enabled() {
        return;
    }
    with_state(|state| {
        if outer {
            state.phase2_batch += targets;
        } else {
            state.flush_batches.push(targets);
        }
    });
}

/// W0-A: one dependency-view request (`Engine::plan_dependency_view`,
/// one per dependency edge of a planned target or aspect application),
/// keyed by what a view memo would key it by (the requested label and the
/// edge configuration). `constructed` is false when a memo answered the
/// request without building the view (W1a); today every request builds.
/// The report compares edges, constructions and distinct keys.
pub fn note_dependency_view<K: Hash + ?Sized>(key: &K, constructed: bool) {
    if !enabled() {
        return;
    }
    VIEW_EDGES.fetch_add(1, Ordering::Relaxed);
    if constructed {
        VIEW_CONSTRUCTIONS.fetch_add(1, Ordering::Relaxed);
    }
    // A fixed-key SipHash: the distinct count needs only a stable hash
    // per process (a 64-bit collision merely undercounts by one).
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    with_state(|state| {
        state.view_keys.insert(hash);
    });
}

/// W0-A: one `Artifact::file` conversion (a File value rebuilt from an
/// artifact, R3: label parsing dominates it). One relaxed increment when
/// enabled; one atomic load otherwise.
pub fn note_artifact_file() {
    if enabled() {
        ARTIFACT_FILES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Nearest-rank percentile (`pct` in 0..=100) of an ascending slice; 0
/// for an empty one. The median is `percentile(sorted, 50)` (the lower
/// middle element for an even count).
pub fn percentile(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (pct.min(100) as usize * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// The `PERF-STAGE flush-batch` line for the given per-flush target counts
/// (flush order) and the phase-2 batch. Pure: unit-tested.
pub fn flush_batch_line(batches: &[u64], phase2: u64) -> String {
    let mut sorted = batches.to_vec();
    sorted.sort_unstable();
    format!(
        "PERF-STAGE flush-batch flushes={} planned={} empty={} min={} median={} p90={} max={} phase2={}",
        sorted.len(),
        sorted.iter().sum::<u64>(),
        sorted.iter().filter(|batch| **batch == 0).count(),
        sorted.first().copied().unwrap_or(0),
        percentile(&sorted, 50),
        percentile(&sorted, 90),
        sorted.last().copied().unwrap_or(0),
        phase2
    )
}

/// Longest dependency-chain sum of per-target compute times (µs) over
/// `edges` (target -> its dependencies, by plan index), plus the chain
/// itself (dependency-first). Pure: unit-tested without an engine.
pub fn critical_path(
    compute_us: &HashMap<u64, u64>,
    edges: &HashMap<u64, Vec<u64>>,
) -> (u64, Vec<u64>) {
    // best[i] = compute[i] + max(best[d] for d in deps(i)), memoized DFS.
    fn best(
        node: u64,
        compute_us: &HashMap<u64, u64>,
        edges: &HashMap<u64, Vec<u64>>,
        memo: &mut HashMap<u64, (u64, u64)>, // node -> (total, next)
    ) -> u64 {
        if let Some((total, _)) = memo.get(&node) {
            return *total;
        }
        let own = compute_us.get(&node).copied().unwrap_or(0);
        let mut total = own;
        let mut next = u64::MAX;
        if let Some(deps) = edges.get(&node) {
            for dep in deps {
                let candidate = own.saturating_add(best(*dep, compute_us, edges, memo));
                if candidate > total {
                    total = candidate;
                    next = *dep;
                }
            }
        }
        memo.insert(node, (total, next));
        total
    }
    let mut memo = HashMap::new();
    let mut top = (0u64, u64::MAX);
    for node in compute_us.keys() {
        let total = best(*node, compute_us, edges, &mut memo);
        if total > top.0 {
            top = (total, *node);
        }
    }
    let mut chain = Vec::new();
    let mut node = top.1;
    while node != u64::MAX {
        chain.push(node);
        node = memo.get(&node).map_or(u64::MAX, |(_, next)| *next);
    }
    chain.reverse();
    (top.0, chain)
}

/// Records sorted by plan index (stable within an index: prepare,
/// compute, commit by stage order). The report and tests consume this
/// order; append order under a future parallel compute need not match.
pub fn sorted_records(records: &[PlanStageRecord]) -> Vec<PlanStageRecord> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| (r.index, r.stage));
    sorted
}

/// The one-shot report (stderr, `PERF-STAGE`-prefixed like the other
/// PERF lines). `labels[i]` is the display label of plan index `i`, and
/// `edges` the dep graph by plan index, both supplied by the engine at
/// the end of `plan_targets`. Idempotent; silent unless enabled.
pub fn report(labels: &[String], edges: &HashMap<u64, Vec<u64>>) {
    static REPORTED: OnceLock<()> = OnceLock::new();
    if !enabled() || REPORTED.set(()).is_err() {
        return;
    }
    let (records, classes, span_us, flush_batches, phase2_batch, view_keys) = {
        let mut binding = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        let state = binding.get_or_insert_with(State::default);
        (
            std::mem::take(&mut state.records),
            std::mem::take(&mut state.classes),
            state.span_us.take(),
            std::mem::take(&mut state.flush_batches),
            std::mem::take(&mut state.phase2_batch),
            std::mem::take(&mut state.view_keys).len(),
        )
    };
    let class_name = |id: u32| -> &str {
        if id == 0 {
            "-"
        } else {
            classes.get((id - 1) as usize).map_or("?", String::as_str)
        }
    };
    let mut stage_totals: BTreeMap<PlanStage, (u64, u64)> = BTreeMap::new();
    let mut class_totals: BTreeMap<u32, [u64; 3]> = BTreeMap::new();
    let mut compute_us: HashMap<u64, u64> = HashMap::new();
    for record in &records {
        let entry = stage_totals.entry(record.stage).or_default();
        entry.0 += record.elapsed_us;
        entry.1 += 1;
        let slot = match record.stage {
            PlanStage::Prepare => Some(0),
            PlanStage::Compute | PlanStage::CommitRetry => Some(1),
            PlanStage::Commit => Some(2),
            _ => None,
        };
        if let Some(slot) = slot {
            class_totals.entry(record.class).or_default()[slot] += record.elapsed_us;
        }
        if matches!(record.stage, PlanStage::Compute | PlanStage::CommitRetry) {
            *compute_us.entry(record.index).or_default() += record.elapsed_us;
        }
    }
    // Coverage algebra (no double count): prepare/compute/commit cover
    // every planned target wherever it was planned; the outermost Analyze
    // steps cover phase 1, but the plan_pending flushes inside them
    // already contributed their targets' stage times — so only the
    // analyze time beyond the flushes is added.
    let flush_us = stage_totals
        .get(&PlanStage::PendingFlush)
        .map_or(0, |(us, _)| *us);
    let outermost_analyze_us: u64 = records
        .iter()
        .filter(|r| r.stage == PlanStage::Analyze && r.outermost)
        .map(|r| r.elapsed_us)
        .sum();
    let covered_us: u64 = [
        PlanStage::Prepare,
        PlanStage::Compute,
        PlanStage::Commit,
        PlanStage::CommitRetry,
    ]
    .iter()
    .map(|s| stage_totals.get(s).map_or(0, |(us, _)| *us))
    .sum::<u64>()
        + outermost_analyze_us.saturating_sub(flush_us);
    let span_us = span_us.unwrap_or(0);
    let coverage = if span_us > 0 {
        covered_us as f64 * 100.0 / span_us as f64
    } else {
        0.0
    };
    // Envelope: TOTAL stage sums + total plan span + accounting coverage.
    for stage in [
        PlanStage::Prepare,
        PlanStage::Compute,
        PlanStage::Commit,
        PlanStage::CommitRetry,
        PlanStage::Analyze,
        PlanStage::PendingFlush,
        PlanStage::ConfigConditions,
    ] {
        let (us, count) = stage_totals.get(&stage).copied().unwrap_or((0, 0));
        eprintln!(
            "PERF-STAGE total {} {:.3}s count={}",
            stage.name(),
            us as f64 / 1e6,
            count
        );
    }
    eprintln!(
        "PERF-STAGE total plan_span {:.3}s covered={:.1}%",
        span_us as f64 / 1e6,
        coverage
    );
    // W0-A: phase-1 flush batch sizes (targets per plan_pending flush)
    // and the dependency-view / Artifact::file work counters.
    eprintln!("{}", flush_batch_line(&flush_batches, phase2_batch));
    eprintln!(
        "PERF-STAGE views edges={} constructed={} distinct={} artifact_file={}",
        VIEW_EDGES.load(Ordering::Relaxed),
        VIEW_CONSTRUCTIONS.load(Ordering::Relaxed),
        view_keys,
        ARTIFACT_FILES.load(Ordering::Relaxed)
    );
    // Per-rule-class stage totals, ranked by total time (top 15).
    let mut ranked: Vec<(u32, [u64; 3])> = class_totals.into_iter().collect();
    ranked.sort_by_key(|(_, t)| std::cmp::Reverse(t[0] + t[1] + t[2]));
    for (id, [prepare, compute, commit]) in ranked.iter().take(15) {
        let total = prepare + compute + commit;
        let pct = if covered_us > 0 {
            total as f64 * 100.0 / covered_us as f64
        } else {
            0.0
        };
        eprintln!(
            "PERF-STAGE class {:>5.1}% prepare={:.3}s compute={:.3}s commit={:.3}s {}",
            pct,
            *prepare as f64 / 1e6,
            *compute as f64 / 1e6,
            *commit as f64 / 1e6,
            class_name(*id)
        );
    }
    // Critical path: longest dep chain's summed compute times.
    if !compute_us.is_empty() {
        let (total_us, chain) = critical_path(&compute_us, edges);
        eprintln!(
            "PERF-STAGE critical-path compute={:.3}s len={}",
            total_us as f64 / 1e6,
            chain.len()
        );
        for index in &chain {
            let label = labels.get(*index as usize).map_or("?", String::as_str);
            eprintln!(
                "PERF-STAGE critical-path-node index={} compute={:.3}s {}",
                index,
                compute_us.get(index).copied().unwrap_or(0) as f64 / 1e6,
                label
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(index: u64, stage: PlanStage, elapsed_us: u64) -> PlanStageRecord {
        PlanStageRecord {
            index,
            stage,
            class: 0,
            elapsed_us,
            outermost: false,
        }
    }

    #[test]
    fn records_sort_by_index_then_stage_order() {
        let records = vec![
            rec(2, PlanStage::Commit, 30),
            rec(0, PlanStage::Commit, 3),
            rec(2, PlanStage::Prepare, 10),
            rec(0, PlanStage::Prepare, 1),
            rec(2, PlanStage::Compute, 20),
            rec(0, PlanStage::Compute, 2),
            rec(1, PlanStage::Prepare, 5),
        ];
        let sorted = sorted_records(&records);
        let keys: Vec<(u64, PlanStage)> = sorted.iter().map(|r| (r.index, r.stage)).collect();
        assert_eq!(
            keys,
            vec![
                (0, PlanStage::Prepare),
                (0, PlanStage::Compute),
                (0, PlanStage::Commit),
                (1, PlanStage::Prepare),
                (2, PlanStage::Prepare),
                (2, PlanStage::Compute),
                (2, PlanStage::Commit),
            ]
        );
        // Occurrence bounds: sorting neither drops nor duplicates records.
        assert_eq!(sorted.len(), records.len());
        let mut elapsed: Vec<u64> = sorted.iter().map(|r| r.elapsed_us).collect();
        elapsed.sort_unstable();
        assert_eq!(elapsed, vec![1, 2, 3, 5, 10, 20, 30]);
    }

    #[test]
    fn stage_placement_prepare_compute_commit_per_index() {
        // The legal placement: within one index, prepare < compute <
        // commit; a native target legitimately has no compute record.
        let records = vec![
            rec(0, PlanStage::Prepare, 10),
            rec(0, PlanStage::Compute, 40),
            rec(0, PlanStage::Commit, 10),
            rec(1, PlanStage::Prepare, 10),
            rec(1, PlanStage::Commit, 10),
        ];
        let sorted = sorted_records(&records);
        for index in [0u64, 1] {
            let stages: Vec<PlanStage> = sorted
                .iter()
                .filter(|r| r.index == index)
                .map(|r| r.stage)
                .collect();
            let mut deduped = stages.clone();
            deduped.dedup();
            assert_eq!(stages, deduped, "stage repeated for index {index}");
            assert_eq!(stages.first(), Some(&PlanStage::Prepare));
            assert_eq!(stages.last(), Some(&PlanStage::Commit));
        }
    }

    #[test]
    fn percentile_is_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 0), 7);
        assert_eq!(percentile(&[7], 100), 7);
        let sorted: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&sorted, 50), 5);
        assert_eq!(percentile(&sorted, 90), 9);
        assert_eq!(percentile(&sorted, 91), 10);
        assert_eq!(percentile(&sorted, 100), 10);
        assert_eq!(percentile(&sorted, 0), 1);
        // Odd count: the true middle.
        assert_eq!(percentile(&[1, 2, 3], 50), 2);
    }

    #[test]
    fn flush_batch_line_summarizes_unsorted_batches() {
        let line = flush_batch_line(&[3, 0, 40, 8, 1], 1200);
        assert_eq!(
            line,
            "PERF-STAGE flush-batch flushes=5 planned=52 empty=1 min=0 median=3 p90=40 max=40 phase2=1200"
        );
        assert_eq!(
            flush_batch_line(&[], 7),
            "PERF-STAGE flush-batch flushes=0 planned=0 empty=0 min=0 median=0 p90=0 max=0 phase2=7"
        );
    }

    #[test]
    fn critical_path_sums_the_longest_chain() {
        // 0 -> 1 -> 3 (10+20+5) vs 0 -> 2 (10+100): chain via 2 wins.
        let compute: HashMap<u64, u64> = HashMap::from([(0, 10), (1, 20), (2, 100), (3, 5)]);
        let edges: HashMap<u64, Vec<u64>> = HashMap::from([(0, vec![1, 2]), (1, vec![3])]);
        let (total, chain) = critical_path(&compute, &edges);
        assert_eq!(total, 110);
        assert_eq!(chain, vec![2, 0]);
        // No edges: the single heaviest node alone.
        let (total, chain) = critical_path(&compute, &HashMap::new());
        assert_eq!(total, 100);
        assert_eq!(chain, vec![2]);
    }
}
