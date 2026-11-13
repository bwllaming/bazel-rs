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
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Whether requested `label`'s analysis failed resolving its own select()
/// keys (a config_setting failing — e.g. its Starlark flag key's analysis —
/// is the target's own ConfiguredValueCreationException in Bazel: no
/// `Analysis failed` line, genuine 9.3.1).
pub(crate) fn own_select_failure(error: &str, label: &Label) -> bool {
    error.contains(&format!(
        "errors encountered resolving select() keys for {}",
        crate::rule_declarations::canonical_form(label)
    ))
}

/// The keep-going state of one build ([`crate::engine::Engine::plan_build`]).
#[derive(Default)]
pub(crate) struct State {
    /// Phase-1 dependency edges: each configured key with the keys its
    /// analysis requested (memo hits included).
    edges: FxHashMap<ConfiguredTargetKey, Vec<ConfiguredTargetKey>>,
    /// The edges that are a key's attribute aspects' dependencies
    /// (`(owner, key)`): a requested target failing only through them has
    /// no `Analysis failed` line (genuine 9.3.1: its aspect's
    /// AspectCreationException, `Loading failed`, is the cause).
    /// (With how many of the owner's reaches of the key they are: the key
    /// may be a dependency of the owner's own too.)
    aspect_edges: FxHashMap<(ConfiguredTargetKey, ConfiguredTargetKey), usize>,
    /// Keys whose phase-2 planning failed, by their own error or a
    /// dependency's.
    failed: FxHashSet<ConfiguredTargetKey>,
    /// Keys whose own implementation (or planning) failed, with the error.
    own: FxHashMap<ConfiguredTargetKey, String>,
    /// Keys phase 2 planned successfully.
    planned: FxHashSet<ConfiguredTargetKey>,
    /// The root-cause errors, deduplicated, in report order.
    errors: Vec<String>,
    seen: HashSet<String>,
    /// The first configured key each requested label's analysis computed.
    pub(crate) roots: HashMap<Label, ConfiguredTargetKey>,
    /// The requested label being analyzed (phase 1), whose first computed
    /// key becomes its root.
    pub(crate) current: Option<Label>,
    /// The keys whose attribute aspects' dependencies phase 1 analyzes now,
    /// with the depth and identity of the active chain they left (the walk's
    /// `keep_going_aspect_owner`).
    pub(crate) aspect_owners: Vec<(ConfiguredTargetKey, usize, usize)>,
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

    /// Phase 1's edge `(owner, key)` is one of `owner`'s attribute aspects'
    /// dependencies.
    pub(crate) fn mark_aspect_edge(
        &mut self,
        owner: ConfiguredTargetKey,
        key: ConfiguredTargetKey,
    ) {
        *self.aspect_edges.entry((owner, key)).or_default() += 1;
    }

    /// A root-cause error (printed once, in this order).
    pub(crate) fn error(&mut self, message: String) {
        if self.seen.insert(message.clone()) {
            self.errors.push(message);
        }
    }

    /// Whether `key` failed, directly or through a key phase 2 never
    /// planned itself (a rule transition's pre-transition key delegating to
    /// its owner); `visiting` guards against revisiting.
    fn reaches_failure(
        &self,
        key: &ConfiguredTargetKey,
        visiting: &mut FxHashSet<ConfiguredTargetKey>,
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
        // Nothing failed (yet): nothing to reach.
        if self.failed.is_empty() {
            return false;
        }
        let mut visiting = FxHashSet::default();
        visiting.insert(key.clone());
        self.edges.get(key).is_some_and(|children| {
            children
                .iter()
                .any(|child| self.reaches_failure(child, &mut visiting))
        })
    }

    /// Whether phase 2 failed `key` itself (its own failure or skipped).
    pub(crate) fn is_failed(&self, key: &ConfiguredTargetKey) -> bool {
        self.failed.contains(key)
    }

    /// Phase 2: `key` is skipped because a dependency failed.
    pub(crate) fn fail_propagated(&mut self, key: &ConfiguredTargetKey) {
        self.failed.insert(key.clone());
    }

    /// Phase 2: `key`'s own planning failed with `message`.
    pub(crate) fn fail_own(&mut self, key: &ConfiguredTargetKey, message: String) {
        self.failed.insert(key.clone());
        self.own.insert(key.clone(), message.clone());
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
        if !self.reaches_failure(root, &mut FxHashSet::default()) {
            return None;
        }
        let own = self.own.contains_key(root)
            || delegation(root).is_some_and(|owner| self.own.contains_key(&owner));
        // Only through its attribute aspects' dependencies: the aspect's
        // failure is the cause (no `Analysis failed`).
        let through_dependency = |key: &ConfiguredTargetKey| {
            self.edges.get(key).is_some_and(|children| {
                children.iter().any(|child| {
                    let aspect = self
                        .aspect_edges
                        .get(&(key.clone(), child.clone()))
                        .copied()
                        .unwrap_or(0);
                    let regular = aspect == 0
                        || children.iter().filter(|other| *other == child).count() > aspect;
                    regular && self.reaches_failure(child, &mut FxHashSet::default())
                })
            })
        };
        let dependency = through_dependency(root)
            || delegation(root).is_some_and(|owner| through_dependency(&owner));
        Some(!own && dependency)
    }

    /// Whether phase 2 failed `key` (its own failure or a dependency's,
    /// through a key never planned itself): its root cause, the first
    /// failure in edge order (a config_setting's Starlark flag key under
    /// `--keep_going`: the config_setting fails with it, ConfigSetting's
    /// dependency on the flag's ConfiguredTargetValue).
    pub(crate) fn failure_of(&self, key: &ConfiguredTargetKey) -> Option<String> {
        if !self.reaches_failure(key, &mut FxHashSet::default()) {
            return None;
        }
        let mut key = key.clone();
        let mut visiting = FxHashSet::default();
        loop {
            if let Some(error) = self.own.get(&key) {
                return Some(error.clone());
            }
            if !visiting.insert(key.clone()) {
                return None;
            }
            let next = self.edges.get(&key).and_then(|children| {
                children
                    .iter()
                    .find(|child| self.reaches_failure(child, &mut FxHashSet::default()))
                    .cloned()
            });
            match next {
                Some(child) => key = child,
                // A dependency failed whose root cause is no error of its
                // own (a requested target's phase 1).
                None => return Some("a dependency failed to analyze".to_owned()),
            }
        }
    }

    /// The root-cause errors, taken.
    pub(crate) fn take_errors(&mut self) -> Vec<String> {
        self.seen.clear();
        std::mem::take(&mut self.errors)
    }
}
