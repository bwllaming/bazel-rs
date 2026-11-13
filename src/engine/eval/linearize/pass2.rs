//! Pass 2 (S3, docs/EVALUATOR.md "S3, pass 2"): the flushes' phase 2 off
//! phase 1's critical path.
//!
//! The walk runs phase 2 of the plan so far at every `config_setting`
//! with Starlark `flag_values` (`plan_pending`): on ~/code ~3,800 small
//! walks inside phase 1. Since the CsMatch split the evaluator does not
//! need them for phase 1 (the flags' FeatureFlagInfo is speculated), so a
//! pooled evaluation **defers** a flush: its phase 2 runs later, in one
//! walk over many flushes' plan segments, and what the tape applies after
//! the flush — console writes, `print()` lines, warnings,
//! `--toolchain_resolution_debug` events, load prints, package commits and
//! the memo values the flush published (platforms, registrations, …) — is
//! kept as [`Op`]s by the plan position before whose turn the walk applies
//! them ([`Pending`], [`Engine::eval_pass2_turn`]), and so are the tape's
//! incremental-tracker ops ([`TrackOp`], [`Engine::eval_pass2_track`]). So the console, the
//! warnings and the engine state each turn sees are the ordered walk's:
//! effects between flush k-1 and flush k come after the phase 2 of the
//! plan up to flush k-1 and before the next entry's.
//!
//! The configured targets (pass 1's canonical values, toolchain
//! implementations, source sides) are published at once: phase 2 reads
//! them by key, a dependency only after it was analyzed; the pipeline's
//! work ahead of its turn needs them.
//!
//! A flush that publishes a new platform is deferred too: the platform is
//! its op ([`Op::Publish`]) at the segment's start, applied before that
//! turn; work ahead of its turn sees it as the turn does (a frozen walk's
//! workers through `Frozen::platform_at`, the platforms the walk's ops
//! publish by position; the pipeline commits no entry ahead whose platform
//! an op publishes before its turn). `BAZEL_RS_EVAL_PLATFORM_WALKS=1`
//! walks such a flush at once, as before.
//!
//! A flush is not deferred (the walk runs now, after every deferred one:
//! the ordered path) when the order would show:
//!
//! - its segment holds a Starlark rule or a genrule whose configuration's
//!   target platform is not resolved (its phase 2 may resolve the
//!   deferred platform, whose tree the ordered walk plans at the flush);
//! - `--profile` (its spans in the walk's order);
//! - a body needs the planning state (a flush in the sequential pass, a
//!   flag's CT2 the pool did not speculate).
//!
//! A deferred segment's failure is the flush's ([`super::driver`]: the
//! walk's error wording), found at the walk that reaches it.

use std::sync::Arc;

use crate::label::PackageId;

use super::super::super::Engine;
use super::super::super::incremental::{TapeReplay, TrackOp};
use super::super::nodes::loading::PkgValue;
use super::driver::Published;

/// BAZEL_RS_EVAL_STATS: flushes deferred, walked now (by reason), the
/// catch-up walks.
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 8] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
pub(crate) const STAT_DEFERRED: usize = 0;
pub(crate) const STAT_NEW_PLATFORM: usize = 1;
pub(crate) const STAT_PLATFORM_UNRESOLVED: usize = 2;
pub(crate) const STAT_ORDERED_MODE: usize = 3;
pub(crate) const STAT_FORCED: usize = 4;
pub(crate) const STAT_WALKS: usize = 5;
pub(crate) const STAT_WALKED: usize = 6;
pub(crate) const STAT_FAILURES: usize = 7;

pub(crate) fn bump(index: usize, by: u64) {
    STATS[index].fetch_add(by, std::sync::atomic::Ordering::Relaxed);
}

/// An effect of the tape applied at its plan position.
pub(in crate::engine) enum Op {
    Print(String),
    Console(crate::ui::Deferred),
    Info(String),
    Warn(String),
    LoadPrints(Arc<[crate::loading::LoadPrintEvent]>),
    PkgCommit(PackageId, Option<Arc<PkgValue>>),
    Publish(Published),
    /// cquery: a capture write (`cquery_ops`), in the walk's order.
    CqueryEdge(Box<super::super::transcript::CqueryEdge>),
    Cquery(Box<super::super::cquery_ops::CqueryOp>),
    /// `--keep_going`: a requested target's phase 1 failed (the walk's
    /// `analyze_requested`): its root-cause error, and whether a dependency
    /// (not the target itself) failed.
    KeepGoingFailure {
        label: crate::label::Label,
        error: String,
        dependency: bool,
    },
}

/// The tape's tracker ops not applied yet, by the plan position before
/// whose turn they apply: runs of one position share a chunk, freed once
/// applied (~/code holds 6.8M at the end of phase 1; 8 bytes each).
#[derive(Default)]
pub(crate) struct TrackQueue {
    chunks: std::collections::VecDeque<(u32, Vec<TrackOp>)>,
    len: usize,
}

impl TrackQueue {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn push(&mut self, position: u32, op: TrackOp) {
        match self.chunks.back_mut() {
            Some((last, ops)) if *last == position => ops.push(op),
            _ => {
                if let Some((_, ops)) = self.chunks.back_mut() {
                    ops.shrink_to_fit();
                }
                self.chunks.push_back((position, vec![op]));
            }
        }
        self.len += 1;
    }

    /// The ops before position `end`, taken out.
    pub(crate) fn split_before(&mut self, end: usize) -> TrackQueue {
        let mut taken = TrackQueue::default();
        while self
            .chunks
            .front()
            .is_some_and(|(position, _)| (*position as usize) < end)
        {
            if let Some(chunk) = self.chunks.pop_front() {
                self.len -= chunk.1.len();
                taken.len += chunk.1.len();
                taken.chunks.push_back(chunk);
            }
        }
        taken
    }

    /// `earlier`'s ops before these.
    pub(crate) fn prepend(&mut self, earlier: TrackQueue) {
        self.len += earlier.len;
        for chunk in earlier.chunks.into_iter().rev() {
            self.chunks.push_front(chunk);
        }
    }

    /// Apply (`apply`) and free the ops due at or before position `index`.
    pub(crate) fn apply_due(&mut self, index: usize, mut apply: impl FnMut(TrackOp)) {
        while self
            .chunks
            .front()
            .is_some_and(|(position, _)| (*position as usize) <= index)
        {
            if let Some((_, ops)) = self.chunks.pop_front() {
                self.len -= ops.len();
                for op in ops {
                    apply(op);
                }
            }
        }
    }

    /// Whether every op is due at or before position `end`.
    pub(crate) fn all_at_or_before(&self, end: usize) -> bool {
        self.chunks
            .iter()
            .all(|(position, _)| (*position as usize) <= end)
    }

    pub(crate) fn clear(&mut self) {
        *self = TrackQueue::default();
    }
}

/// The ops of deferred flushes, by the plan position before whose turn
/// they apply (non-decreasing), while a walk runs them.
#[derive(Default)]
pub(in crate::engine) struct Pending {
    pub(crate) ops: std::collections::VecDeque<(usize, Op)>,
    /// The first position of the walk's last segment: a deferred target
    /// platform resolved before it would plan its tree elsewhere than the
    /// ordered walk (an internal error; the flush that may do it is not
    /// deferred).
    pub(crate) last_segment: usize,
    /// The entry at its turn.
    pub(crate) turn: usize,
    /// The tape's incremental-tracker ops due before the walk's end, by
    /// position ([`Engine::eval_pass2_track`]), and the replay applying
    /// them (moved here for the walk).
    pub(crate) track: TrackQueue,
    pub(crate) replay: Option<Box<TapeReplay>>,
}

impl Pending {
    /// Whether ops apply before the turn of `index`.
    pub(crate) fn due(&self, index: usize) -> bool {
        self.ops
            .front()
            .is_some_and(|(position, _)| *position <= index)
    }
}

impl Engine {
    /// Apply one op where the ordered walk applies it.
    pub(in crate::engine) fn apply_pass2_op(&mut self, op: Op) {
        match op {
            Op::Print(line) => crate::ui::note(&line),
            Op::Console(write) => crate::ui::replay(vec![write]),
            Op::Info(text) => crate::toolchains::print_debug_event(text),
            Op::Warn(warning) => self.warnings.push(warning),
            Op::LoadPrints(prints) => self.emit_load_prints(prints.to_vec()),
            Op::PkgCommit(package, loaded) => commit_package(self, &package, loaded.as_deref()),
            Op::Publish(published) => super::driver::publish_memo(self, &published),
            Op::CqueryEdge(edge) => self.capture_cquery_edge(
                &edge.from,
                edge.to,
                edge.config,
                &edge.attribute,
                edge.tool,
                edge.aspect,
            ),
            Op::Cquery(op) => self.replay_cquery_op(&op),
            Op::KeepGoingFailure {
                label,
                error,
                dependency,
            } => {
                self.keep_going_state.error(error);
                self.keep_going_state
                    .failed_targets
                    .insert(label, dependency);
            }
        }
    }

    /// The turn of plan entry `index` in a walk with deferred flushes:
    /// what the tape applied before it (`true`: something applied).
    pub(in crate::engine) fn eval_pass2_turn(&mut self, index: usize) -> bool {
        let Some(mut pending) = self.eval_pass2.take() else {
            return false;
        };
        pending.turn = index;
        let due = pending.due(index);
        while pending.due(index) {
            if let Some((_, op)) = pending.ops.pop_front() {
                self.apply_pass2_op(op);
            }
        }
        self.eval_pass2 = Some(pending);
        due
    }

    /// The tape's tracker ops before the turn of plan entry `index` in a
    /// walk with deferred flushes (no lowering drain: they change only the
    /// tracker).
    pub(in crate::engine) fn eval_pass2_track(&mut self, index: usize) {
        let Some(pending) = self.eval_pass2.as_mut() else {
            return;
        };
        let (replay, tracker) = (
            pending.replay.as_deref_mut(),
            self.incremental.as_deref_mut(),
        );
        match (replay, tracker) {
            (Some(replay), Some(tracker)) => {
                pending
                    .track
                    .apply_due(index, |op| replay.replay(tracker, op));
            }
            _ => pending.track.apply_due(index, |_| {}),
        }
    }

    /// A deferred target platform resolved by the entry at its turn
    /// before the walk's last segment: the ordered walk would plan its
    /// tree at that segment's flush (an internal error; such a flush is
    /// walked at once, so this does not happen).
    pub(in crate::engine) fn eval_pass2_resolves_platform(&self) -> Result<(), String> {
        match &self.eval_pass2 {
            Some(pending) if pending.turn < pending.last_segment => Err(
                "internal error: BAZEL_RS_ANALYSIS=eval: a deferred flush's phase 2 resolved a deferred target platform; rerun with BAZEL_RS_ANALYSIS=walk"
                    .to_owned(),
            ),
            _ => Ok(()),
        }
    }
}

/// `Engine::commit_loaded_package` of a package the evaluation loaded,
/// unless the engine loaded it itself meanwhile (a flush's phase 2).
pub(crate) fn commit_package(engine: &mut Engine, package: &PackageId, loaded: Option<&PkgValue>) {
    if engine.packages.contains_key(package) {
        return;
    }
    crate::progress::package_loaded();
    let Some(loaded) = loaded else {
        return;
    };
    let rules = &loaded.package;
    for rule in rules.values() {
        for out in &rule.outs {
            if let Ok(output) = rule.label.with_name(out) {
                engine
                    .generated
                    .insert(output, (rule.label.clone(), out.clone()));
            }
        }
    }
    if let Some(custom) = rules.values().find_map(|rule| rule.custom.as_ref()) {
        engine
            .loaded_functions
            .register(&custom.top_level_functions);
    }
    engine.packages.insert(package.clone(), Arc::clone(rules));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_queue_applies_by_position_and_keeps_the_rest() {
        assert_eq!(std::mem::size_of::<TrackOp>(), 8);
        let mut queue = TrackQueue::default();
        for (position, op) in [
            (0, TrackOp::Leave),
            (0, TrackOp::Print),
            (3, TrackOp::Access(1)),
            (5, TrackOp::Access(2)),
            (5, TrackOp::Leave),
        ] {
            queue.push(position, op);
        }
        assert_eq!(queue.len(), 5);
        let mut early = queue.split_before(4);
        assert_eq!((early.len(), queue.len()), (3, 2));
        let mut seen = Vec::new();
        early.apply_due(0, |op| seen.push(op));
        assert_eq!(seen, [TrackOp::Leave, TrackOp::Print]);
        queue.prepend(early);
        assert_eq!(queue.len(), 3);
        assert!(!queue.all_at_or_before(4));
        seen.clear();
        queue.apply_due(5, |op| seen.push(op));
        assert_eq!(
            seen,
            [TrackOp::Access(1), TrackOp::Access(2), TrackOp::Leave]
        );
        assert!(queue.is_empty());
    }
}
