//! Helper threads of one pattern unit's `.bzl` load (docs/COLD_ANALYSIS.md,
//! "Pattern loading: `.bzl` helpers").
//!
//! A pattern-loading worker's unit walks its BUILD file's `.bzl` graph on
//! one thread. A wide fan-out — `@npm//:defs.bzl` loads 1,936
//! `__links//:defs.bzl` files, each in its own repository — then costs one
//! module after another (~0.5 ms each: the repository's root and ignored
//! subdirectories, the read, the parse, the evaluation). When the unit's
//! traversal enters a module with many loads the shared cache does not
//! hold yet, those loads are handed to helper threads that load them,
//! last first, into the repository set's shared [`super::BzlCache`]; the
//! unit's own traversal goes on in its order from the first and finds the
//! helpers' modules there.
//!
//! A helper is exactly another pattern worker loading one `.bzl` file of a
//! committed unit: the same no-demand, reuse-only, committed-unit guards,
//! a BUILD-phase loader over the shared cache (whoever evaluates a label
//! first, the entry is the same — the cache's invariant), its own print
//! sink, roots and observation frame, all discarded: a module's prints,
//! obtained root and observations are stored with its cache entry and
//! replayed where the unit's traversal reaches it (as for a module another
//! unit evaluated). A helper's failure is the failure any worker would
//! publish (sentinels, cycles and depth errors are never published), and
//! the unit's traversal still visits every module itself, in order, with
//! its cycle, depth and visibility checks. So the unit's result, its print
//! events and its observations are those it computes without helpers.
//!
//! `BAZEL_RS_BZL_HELPERS` sets the helpers per unit (default
//! [`DEFAULT_WIDTH`]; `0` turns them off).

use super::{Label, LoadPrints, Loader, RepositorySet};
use starlark::environment::Globals;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// A helper's load stopped because its unit's traversal is done (an
/// inline-rerun sentinel: never published, like a wait-cycle abort).
pub(super) const STOPPED: &str = "\u{1}parallel-bzl-cycle: the unit's .bzl helpers stopped";

/// Helpers per unit by default.
const DEFAULT_WIDTH: usize = 8;

/// Fewer loads the shared cache misses than this are not handed out.
pub(super) const MIN_FANOUT: usize = 32;

/// Helper threads' stack (a pattern worker's: Starlark evaluation recurses).
const STACK: usize = 16 << 20;

/// The helpers per unit (`BAZEL_RS_BZL_HELPERS`).
pub(super) fn width() -> usize {
    static WIDTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WIDTH.get_or_init(|| {
        // snapshot-unrecorded: a scheduling switch; the loaded modules are the same.
        std::env::var("BAZEL_RS_BZL_HELPERS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(DEFAULT_WIDTH)
    })
}

thread_local! {
    static ALLOWED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Marks this thread as a pattern-loading worker evaluating a committed
/// unit (crate::engine's `load_packages_parallel`): its `.bzl` loads may
/// use helpers.
pub(crate) struct AllowGuard;

pub(crate) fn allow() -> AllowGuard {
    ALLOWED.with(|depth| depth.set(depth.get() + 1));
    AllowGuard
}

impl Drop for AllowGuard {
    fn drop(&mut self) {
        ALLOWED.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Whether a shared load on this thread may use helpers: a pattern worker
/// in its no-demand, reuse-only, committed-unit mode, not inside an
/// extension group's speculative preparation.
pub(super) fn allowed_here() -> bool {
    ALLOWED.with(std::cell::Cell::get) > 0
        && crate::repository_demand::no_demand_active()
        && crate::repository_demand::reuse_only_active()
        && crate::repository_demand::committed_unit_active()
        && !crate::extension_speculation::active()
        && width() > 0
}

#[derive(Default)]
struct State {
    /// Labels handed out, in the unit's order.
    labels: Vec<Label>,
    /// Whether each label was taken (by a helper or the unit itself).
    taken: Vec<bool>,
    /// The position of each label handed out.
    positions: HashMap<Label, usize>,
    /// Labels below this one may still be untaken (helpers take from the
    /// end: the next one to look at is `back - 1`).
    back: usize,
    stop: bool,
}

/// The work queue of one unit's helpers.
#[derive(Default)]
pub(super) struct Queue {
    state: Mutex<State>,
    work: Condvar,
    spawned: AtomicBool,
    /// Set by [`Queue::stop`]: helpers stop before their next module.
    stopped: AtomicBool,
    taken_by_helpers: std::sync::atomic::AtomicU64,
    /// Helper loads that ended in an inline-rerun sentinel.
    sentinels: std::sync::atomic::AtomicU64,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hand out `labels` (in the unit's order); whether the helpers must
    /// be started.
    pub(super) fn submit(&self, labels: impl IntoIterator<Item = Label>) -> bool {
        let mut state = self.lock();
        if state.stop {
            return false;
        }
        let before = state.labels.len();
        for label in labels {
            if state.positions.contains_key(&label) {
                continue;
            }
            let position = state.labels.len();
            state.positions.insert(label.clone(), position);
            state.labels.push(label);
            state.taken.push(false);
        }
        if state.labels.len() == before {
            return false;
        }
        // Helpers move on to the new labels' end; untaken older ones are
        // left to the unit's traversal.
        state.back = state.labels.len();
        drop(state);
        self.work.notify_all();
        !self.spawned.swap(true, Ordering::AcqRel)
    }

    /// The unit's traversal enters `label` itself: no helper starts it now.
    pub(super) fn take_own(&self, label: &Label) {
        let mut state = self.lock();
        if let Some(&position) = state.positions.get(label) {
            state.taken[position] = true;
        }
    }

    /// A helper's next label, last first; `None` once stopped.
    fn next(&self) -> Option<Label> {
        let mut state = self.lock();
        loop {
            if state.stop {
                return None;
            }
            while state.back > 0 {
                state.back -= 1;
                let position = state.back;
                if !state.taken[position] {
                    state.taken[position] = true;
                    self.taken_by_helpers.fetch_add(1, Ordering::Relaxed);
                    return Some(state.labels[position].clone());
                }
            }
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The unit's traversal is done: helpers finish the label they are on
    /// and exit.
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.lock().stop = true;
        self.work.notify_all();
    }

    pub(super) fn taken_by_helpers(&self) -> u64 {
        self.taken_by_helpers.load(Ordering::Relaxed)
    }

    pub(super) fn sentinels(&self) -> u64 {
        self.sentinels.load(Ordering::Relaxed)
    }
}

/// What a unit's loader holds to hand loads out ([`Loader::helpers`]).
#[derive(Clone, Copy)]
pub(super) struct Helpers<'a> {
    pub(super) queue: &'a Queue,
    /// Starts the helper threads (once, at the first hand-out).
    pub(super) spawn: &'a dyn Fn(),
}

/// Start `width` helpers of `queue` on `scope`.
pub(super) fn spawn<'scope, 'env: 'scope>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    queue: &'env Queue,
    repositories: &'env Arc<RepositorySet>,
    globals: &'env Globals,
    width: usize,
) {
    for _ in 0..width {
        let spawned = std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, move || helper(queue, repositories, globals));
        if spawned.is_err() {
            // Fewer helpers: the unit's traversal loads the rest.
            break;
        }
    }
}

/// One helper: load handed-out labels into the shared cache until stopped.
fn helper(queue: &Queue, repositories: &Arc<RepositorySet>, globals: &Globals) {
    let _guard = crate::repository_demand::no_demand();
    let _reuse = crate::repository_demand::reuse_only();
    let _committed = crate::repository_demand::committed_unit();
    // What it observes and obtains outside the modules' own captures is
    // its own (discarded): the unit replays each module's stored set.
    let frame = crate::observed_fs::hook(crate::observed_fs::Hook::Unit)
        .then(crate::observed_fs::unit_scope);
    let roots = crate::repository::UnitRoots::record();
    let prints = LoadPrints::default();
    let mut loader = Loader::shared_build_phase(Arc::clone(repositories), globals, &prints);
    loader.stop = Some(&queue.stopped);
    while let Some(label) = queue.next() {
        let cache = repositories.build_bzl_cache();
        if matches!(cache.get(&label), Ok(Some(_))) || !cache.help(&label) {
            continue;
        }
        // Its outcome is the shared cache's: a failure there is the one
        // any worker publishes, a sentinel or a cycle is left to the unit.
        if let Err(error) = loader.load(label.clone())
            && crate::repository_demand::is_inline_rerun_sentinel(&error)
        {
            queue.sentinels.fetch_add(1, Ordering::Relaxed);
        }
        match cache.get(&label) {
            Ok(Some(Err(failure))) => debug_assert!(
                !crate::repository_demand::is_inline_rerun_sentinel(&failure.error),
                "a helper published a sentinel for {label}: {}",
                failure.error
            ),
            Ok(Some(Ok(_))) => {}
            // Abandoned: another unit's helper may take it again.
            _ => cache.unhelp(&label),
        }
        prints.take();
    }
    drop(roots.finish());
    if let Some(frame) = frame {
        drop(frame.finish());
    }
}
