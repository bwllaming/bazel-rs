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
//! them ([`Pending`], [`Engine::eval_pass2_turn`]). So the console, the
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
//! - the incremental tracker or the observation recording is on (they
//!   record phase 2 and the tape in their order);
//! - a body needs the planning state (a flush in the sequential pass, a
//!   flag's CT2 the pool did not speculate).
//!
//! A deferred segment's failure is the flush's ([`super::driver`]: the
//! walk's error wording), found at the walk that reaches it.

use std::sync::Arc;

use crate::label::PackageId;

use super::super::super::Engine;
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
