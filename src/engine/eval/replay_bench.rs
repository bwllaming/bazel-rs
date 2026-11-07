//! S0 linearizer throughput bench (`BAZEL_RS_EVAL_REPLAY_BENCH=1`,
//! docs/EVALUATOR.md "Measured: linearizer merge throughput"): every CT2
//! delta the coordinator merges ([`Engine::merge_delta`]) is copied, the
//! real merge is timed, and at the end of phase 2 the copies are replayed
//! through the linearizer's merge ([`delta::merge_ops`]) into a fresh
//! planning state — the stub linearizer of pass 2 — once untimed (the
//! throughput) and once with every operation timed (the per-merge cost
//! breakdown). The final planning state's volume (producers, nodes, index
//! entries, …) extrapolates the cost to a build whose natives merge as
//! deltas too (the design's 133k CT2 deltas).
//!
//! Diagnostic only: off by default, nothing is copied or replayed unless
//! the variable is `1`; the report goes to the diagnostics stream.
//!
//! Owner: worker C (the linearizer).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::super::delta::{self, DeltaOp, TargetDelta};
use super::super::{ActionPlanning, ConfiguredTargetKey};

/// Whether `BAZEL_RS_EVAL_REPLAY_BENCH=1` asks for the bench.
pub(in crate::engine) fn enabled() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
        std::env::var_os("BAZEL_RS_EVAL_REPLAY_BENCH").is_some_and(|value| value == "1")
    });
    *ON
}

/// A copied delta (its lowerings in flight are counted, not copied).
struct Recorded {
    key: ConfiguredTargetKey,
    delta: TargetDelta,
}

#[derive(Default)]
struct Bench {
    recorded: Vec<Recorded>,
    /// Lowerings in flight the copies left out.
    skipped_lowerings: usize,
    /// The real merges: count and total time.
    merges: usize,
    merge_time: Duration,
}

thread_local! {
    // Merges run on the coordinator only: a thread-local needs no `Send`
    // of the frozen provider values.
    static BENCH: RefCell<Bench> = RefCell::new(Bench::default());
}

/// Copy `delta` (the commit of `key`) before its real merge.
pub(in crate::engine) fn record(key: &ConfiguredTargetKey, delta: &TargetDelta) {
    let mut skipped = 0;
    let ops = delta
        .ops
        .iter()
        .filter_map(|op| {
            let copy = op.replay_copy();
            skipped += usize::from(copy.is_none());
            copy
        })
        .collect();
    let copy = TargetDelta {
        ops,
        error: delta.error.clone(),
        unchecked: delta.unchecked,
        uses: delta.uses.clone(),
        failed_output: delta.failed_output.clone(),
    };
    BENCH.with(|bench| {
        let mut bench = bench.borrow_mut();
        bench.skipped_lowerings += skipped;
        bench.recorded.push(Recorded {
            key: key.clone(),
            delta: copy,
        });
    });
}

/// One real merge took `elapsed`.
pub(in crate::engine) fn note_merge(elapsed: Duration) {
    BENCH.with(|bench| {
        let mut bench = bench.borrow_mut();
        bench.merges += 1;
        bench.merge_time += elapsed;
    });
}

/// The final planning state's volume: what a linearizer that merges every
/// configured target as a delta would apply.
pub(in crate::engine) struct Volume {
    pub(in crate::engine) plan: usize,
    pub(in crate::engine) producers: usize,
    pub(in crate::engine) nodes: usize,
    pub(in crate::engine) artifacts: usize,
    pub(in crate::engine) declared: usize,
    pub(in crate::engine) dry: usize,
    pub(in crate::engine) providers: usize,
    pub(in crate::engine) runfiles_info: usize,
    pub(in crate::engine) runfiles_nodes: usize,
}

/// Per operation kind: count and time.
type Breakdown = BTreeMap<&'static str, (usize, Duration)>;

/// Replay the copies into a fresh planning state; `timed`: every operation
/// timed into the breakdown. Returns the replay's wall time (the copies
/// are consumed; the replayed state is dropped outside the timing).
fn replay(
    recorded: Vec<Recorded>,
    timed: bool,
    presize: bool,
    breakdown: &mut Breakdown,
) -> Duration {
    let mut state = ActionPlanning::default();
    if presize {
        // The maps sized for the whole replay up front (no rehashing):
        // what a linearizer that knows the CT2 count could do.
        let (mut outputs, mut declared) = (0, 0);
        for Recorded { delta, .. } in &recorded {
            for op in &delta.ops {
                match op {
                    DeltaOp::Output(..) => outputs += 1,
                    DeltaOp::Declared { .. } => declared += 1,
                    _ => {}
                }
            }
        }
        state.producers.paths.reserve(outputs);
        state.producers.ancestors.reserve(outputs);
        state.declared.reserve(declared);
        state.custom_providers.reserve(recorded.len());
        state.provider_keys.reserve(recorded.len());
        state.runfiles_info.reserve(recorded.len());
        state.dry.reserve(recorded.len());
    }
    let mut action_executions = BTreeMap::new();
    let mut build_info_actions = BTreeSet::new();
    let started = Instant::now();
    for Recorded { key, delta } in recorded {
        if timed {
            for op in delta.ops {
                let kind = op.kind();
                let at = Instant::now();
                delta::merge_op(
                    &mut state,
                    &mut action_executions,
                    &mut build_info_actions,
                    (0, 0),
                    &key,
                    op,
                );
                let entry = breakdown.entry(kind).or_default();
                entry.0 += 1;
                entry.1 += at.elapsed();
            }
        } else {
            let sink = delta::MergeSink {
                action_executions: &mut action_executions,
                build_info_actions: &mut build_info_actions,
                warnings: 0,
                errors: 0,
            };
            let _ = delta::merge_ops(&mut state, sink, &key, delta);
        }
    }
    let elapsed = started.elapsed();
    drop(state);
    elapsed
}

/// Copies of the recorded deltas (the bench replays each pass from its own
/// copies, so both start from the same input).
fn copies(recorded: &[Recorded]) -> Vec<Recorded> {
    recorded
        .iter()
        .map(|Recorded { key, delta }| Recorded {
            key: key.clone(),
            delta: TargetDelta {
                ops: delta.ops.iter().filter_map(DeltaOp::replay_copy).collect(),
                error: delta.error.clone(),
                unchecked: delta.unchecked,
                uses: delta.uses.clone(),
                failed_output: delta.failed_output.clone(),
            },
        })
        .collect()
}

/// Replay and report (`PERF-EVAL-REPLAY` lines); the end of phase 2.
pub(in crate::engine) fn report(volume: Volume) {
    let bench = BENCH.with(|bench| std::mem::take(&mut *bench.borrow_mut()));
    let deltas = bench.recorded.len();
    let ops: usize = bench.recorded.iter().map(|r| r.delta.ops.len()).sum();
    let line = |text: String| crate::snapshot::diag::line(&text);
    line(format!(
        "PERF-EVAL-REPLAY real merges={} total={:.3}s per_merge={:.2}us rate={:.0}/s",
        bench.merges,
        bench.merge_time.as_secs_f64(),
        micros(bench.merge_time, bench.merges),
        rate(bench.merges, bench.merge_time)
    ));
    let mut ignored = Breakdown::new();
    let mut best = Duration::MAX;
    for _ in 0..3 {
        best = best.min(replay(copies(&bench.recorded), false, false, &mut ignored));
    }
    let mut presized = Duration::MAX;
    for _ in 0..3 {
        presized = presized.min(replay(copies(&bench.recorded), false, true, &mut ignored));
    }
    line(format!(
        "PERF-EVAL-REPLAY stub deltas={deltas} ops={ops} skipped_lowerings={} best_of_3={:.3}s per_merge={:.2}us rate={:.0}/s",
        bench.skipped_lowerings,
        best.as_secs_f64(),
        micros(best, deltas),
        rate(deltas, best)
    ));
    line(format!(
        "PERF-EVAL-REPLAY stub presized best_of_3={:.3}s per_merge={:.2}us rate={:.0}/s",
        presized.as_secs_f64(),
        micros(presized, deltas),
        rate(deltas, presized)
    ));
    let mut breakdown = Breakdown::new();
    let timed = replay(bench.recorded, true, false, &mut breakdown);
    line(format!(
        "PERF-EVAL-REPLAY timed total={:.3}s (per-op timing overhead included)",
        timed.as_secs_f64()
    ));
    for (kind, (count, time)) in &breakdown {
        line(format!(
            "PERF-EVAL-REPLAY op {kind:<18} count={count:>9} total={:.3}s per_op={:.3}us per_merge={:.3}us",
            time.as_secs_f64(),
            micros(*time, *count),
            micros(*time, deltas)
        ));
    }
    // The extrapolation: every configured target merging as a delta,
    // each operation kind at its measured per-op cost, scaled to the final
    // state's volume (natives included).
    let per_op = |kind: &str| {
        breakdown
            .get(kind)
            .map_or(0.0, |(count, time)| micros(*time, *count))
    };
    let estimate_us = per_op("output") * volume.producers as f64
        + per_op("node") * volume.nodes as f64
        + per_op("artifact") * volume.artifacts as f64
        + per_op("declared") * volume.declared as f64
        + per_op("dry") * volume.dry as f64
        + per_op("providers") * volume.providers as f64
        + per_op("runfiles_info") * volume.runfiles_info as f64
        + per_op("runfiles_nodes") * volume.runfiles_nodes as f64
        + per_op("action_execution") * volume.nodes as f64;
    line(format!(
        "PERF-EVAL-REPLAY volume plan={} producers={} nodes={} artifacts={} declared={} dry={} providers={} runfiles_info={} runfiles_nodes={}",
        volume.plan,
        volume.producers,
        volume.nodes,
        volume.artifacts,
        volume.declared,
        volume.dry,
        volume.providers,
        volume.runfiles_info,
        volume.runfiles_nodes
    ));
    let seconds = estimate_us / 1e6;
    line(format!(
        "PERF-EVAL-REPLAY extrapolated all-delta merge={seconds:.3}s for {} merges ({:.0}/s)",
        volume.plan,
        if seconds > 0.0 {
            volume.plan as f64 / seconds
        } else {
            0.0
        }
    ));
}

fn micros(time: Duration, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        time.as_secs_f64() * 1e6 / count as f64
    }
}

fn rate(count: usize, time: Duration) -> f64 {
    let seconds = time.as_secs_f64();
    if seconds > 0.0 {
        count as f64 / seconds
    } else {
        0.0
    }
}
