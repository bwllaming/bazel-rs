//! `--keep_going` analysis bookkeeping (docs/KEEP_GOING.md).
//!
//! Bazel's `--keep_going` (runtime/KeepGoingOption.java) lets the analysis
//! of every top-level target that does not depend on a failure complete:
//! a ConfiguredTargetValue in error fails exactly the targets above it
//! (SkyframeErrorProcessor.processAnalysisErrors), each failing top-level
//! target is reported once as `errors encountered while analyzing target
//! 'L', it will not be built.` (:676-684), the rest is built.
//!
//! The native engine analyzes in two sequential phases (crate::engine):
//! phase 1 resolves the configured target graph depth-first per requested
//! label, phase 2 runs the rule implementations in post-order. A phase-1
//! failure propagates (`?`) through every key on the active chain, so a
//! requested label whose analysis fails is skipped as a whole and nothing
//! of it reaches the plan. A phase-2 failure of key K fails K alone; the
//! dependency edges phase 1 recorded ([`State::record_edge`]) carry it to
//! every key above K, which is skipped before its implementation would
//! read K's missing providers. Every failure is recorded in the engine's
//! one sequential order, so what a keep-going run reports never depends on
//! the loading threads or phase-1 prefetch.

use crate::config_registry::ConfiguredTargetKey;
use crate::label::Label;
use std::collections::{BTreeMap, HashMap, HashSet};

/// The keep-going state of one build ([`crate::engine::Engine::plan_build`]).
#[derive(Default)]
pub(crate) struct State {
    /// Phase-1 dependency edges: each configured key with the keys its
    /// analysis requested (memo hits included).
    edges: HashMap<ConfiguredTargetKey, Vec<ConfiguredTargetKey>>,
    /// Keys whose phase-2 planning failed, by their own error or a
    /// dependency's.
    failed: HashSet<ConfiguredTargetKey>,
    /// Keys whose own implementation (or planning) failed.
    own: HashSet<ConfiguredTargetKey>,
    /// Keys phase 2 planned successfully.
    planned: HashSet<ConfiguredTargetKey>,
    /// The root-cause errors, deduplicated, in report order.
    errors: Vec<String>,
    seen: HashSet<String>,
    /// The first configured key each requested label's analysis computed.
    pub(crate) roots: HashMap<Label, ConfiguredTargetKey>,
    /// The requested label being analyzed (phase 1), whose first computed
    /// key becomes its root.
    pub(crate) current: Option<Label>,
    /// Requested labels whose analysis failed, with whether a dependency
    /// (not the target itself) failed.
    pub(crate) failed_targets: BTreeMap<Label, bool>,
}

impl State {
    /// Phase 1 computed `key` with `parent` on top of its active chain.
    pub(crate) fn record_edge(
        &mut self,
        parent: Option<&ConfiguredTargetKey>,
        key: &ConfiguredTargetKey,
    ) {
        match parent {
            Some(parent) => self
                .edges
                .entry(parent.clone())
                .or_default()
                .push(key.clone()),
            None => {
                if let Some(label) = self.current.take() {
                    self.roots.insert(label, key.clone());
                }
            }
        }
    }

    /// A root-cause error (printed once, in this order).
    pub(crate) fn error(&mut self, message: String) {
        if self.seen.insert(message.clone()) {
            self.errors.push(message);
        }
    }

    /// The number of root-cause errors so far.
    pub(crate) fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// The number of keys phase 2 failed so far.
    pub(crate) fn failed_count(&self) -> usize {
        self.failed.len()
    }

    /// The last root-cause error.
    pub(crate) fn last_error(&self) -> Option<&str> {
        self.errors.last().map(String::as_str)
    }

    /// The root-cause error `index`.
    pub(crate) fn error_at(&self, index: usize) -> Option<&str> {
        self.errors.get(index).map(String::as_str)
    }

    /// Whether `key` failed, directly or through a key phase 2 never
    /// planned itself (a rule transition's pre-transition key delegating to
    /// its owner); `visiting` guards against revisiting.
    fn reaches_failure(
        &self,
        key: &ConfiguredTargetKey,
        visiting: &mut HashSet<ConfiguredTargetKey>,
    ) -> bool {
        if self.failed.contains(key) {
            return true;
        }
        if self.planned.contains(key) || !visiting.insert(key.clone()) {
            return false;
        }
        self.edges.get(key).is_some_and(|children| {
            children
                .iter()
                .any(|child| self.reaches_failure(child, visiting))
        })
    }

    /// Phase 2: whether a dependency of plan key `key` failed (post-order:
    /// every planned dependency was decided before).
    pub(crate) fn dependency_failed(&self, key: &ConfiguredTargetKey) -> bool {
        let mut visiting = HashSet::new();
        visiting.insert(key.clone());
        self.edges.get(key).is_some_and(|children| {
            children
                .iter()
                .any(|child| self.reaches_failure(child, &mut visiting))
        })
    }

    /// Phase 2: `key` is skipped because a dependency failed.
    pub(crate) fn fail_propagated(&mut self, key: &ConfiguredTargetKey) {
        self.failed.insert(key.clone());
    }

    /// Phase 2: `key`'s own planning failed with `message`.
    pub(crate) fn fail_own(&mut self, key: &ConfiguredTargetKey, message: String) {
        self.failed.insert(key.clone());
        self.own.insert(key.clone());
        self.error(message);
    }

    /// Phase 2 planned `key`.
    pub(crate) fn planned(&mut self, key: &ConfiguredTargetKey) {
        self.planned.insert(key.clone());
    }

    /// Whether requested `label` (its analysis completed phase 1) failed in
    /// phase 2: `Some(dependency)` when it did, `dependency` false when the
    /// failing key is the target's own (its root or the owner its rule
    /// transition delegated to).
    pub(crate) fn phase2_failure(
        &self,
        label: &Label,
        delegation: impl Fn(&ConfiguredTargetKey) -> Option<ConfiguredTargetKey>,
    ) -> Option<bool> {
        let root = self.roots.get(label)?;
        if !self.reaches_failure(root, &mut HashSet::new()) {
            return None;
        }
        let own = self.own.contains(root)
            || delegation(root).is_some_and(|owner| self.own.contains(&owner));
        Some(!own)
    }

    /// The root-cause errors, taken.
    pub(crate) fn take_errors(&mut self) -> Vec<String> {
        self.seen.clear();
        std::mem::take(&mut self.errors)
    }
}
