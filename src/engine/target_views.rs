//! B61: `Target.actions` of a Starlark rule target whose views the analysis
//! did not keep (docs/ANALYSIS_TESTS.md, "Target.actions").
//!
//! Such a target holds a [`crate::target_actions::Lazy`] marker. An
//! analysis (an aspect application or a rule implementation) that reads it
//! before it is filled ends with the [`crate::target_actions::NEEDS_VIEWS`]
//! sentinel, wherever it ran: a worker's analysis with a sentinel is left
//! to the coordinator (an aspect job's or prefetch's application is
//! unusable, a compute's error reaches its commit). On the coordinator the
//! engine analyzes each target asked for again with its views kept
//! ([`Engine::fill_target_views`]: its prepare over the planning state, its
//! implementation, isolated: console dropped, nothing committed), fills its
//! marker and runs the reader again; the attempt that asked is dropped
//! whole, its prints included. So the answer never depends on the analysis
//! order: a target's views are those of its own analysis, kept or filled.

use super::*;

/// How deep a fill may itself fill (a target whose re-analysis asks for the
/// views of another), and how often one reader is retried.
const MAX_FILLS: usize = 16;

impl PlanReads for PlanState<'_> {
    fn dry(&self, key: &ConfiguredTargetKey) -> Option<Artifacts> {
        self.dry.get(key).cloned()
    }

    fn providers(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<BTreeMap<String, starlark::values::OwnedFrozenValue>> {
        self.custom_providers.get(key).cloned()
    }

    fn provider(
        &self,
        key: &ConfiguredTargetKey,
        id: &str,
    ) -> Option<starlark::values::OwnedFrozenValue> {
        self.custom_providers.get(key)?.get(id).cloned()
    }

    fn provider_keys(&self, key: &ConfiguredTargetKey) -> Option<Vec<String>> {
        self.provider_keys.get(key).cloned()
    }

    fn runfiles_info(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<Arc<crate::runfiles::TargetRunfiles>> {
        self.runfiles_info.get(key).cloned()
    }

    fn declared(&self, config: ConfigId, path: &Path) -> Option<Artifact> {
        self.declared.get(config, path).cloned()
    }

    fn validation(&self, key: &ConfiguredTargetKey) -> Option<crate::starlark_types::Depset> {
        self.validation.get(key).cloned()
    }

    fn repositories(&self, key: &ConfiguredTargetKey) -> Option<Arc<BTreeSet<String>>> {
        self.repositories.get(key).cloned()
    }
}

/// The prepare of a target analyzed again for its views: the coordinator's
/// over the planning state, whose target was prepared, validated and
/// committed before (its owners noted, its prerequisites checked, its
/// dependencies' views and aspect applications memoized).
struct ViewsPrepare<'s, 'p> {
    engine: &'s mut Engine,
    state: &'s mut PlanState<'p>,
    configured: &'s mut ConfiguredNodes,
}

impl prepare::Lookup for ViewsPrepare<'_, '_> {
    fn engine(&self) -> &Engine {
        self.engine
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.engine.target(label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        Ok(self.engine.configurations.intern_no_config())
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.engine.attribute_aspects(rule, identities)
    }
}

impl prepare::PrepareCx for ViewsPrepare<'_, '_> {
    fn reads(&self) -> &dyn PlanReads {
        &*self.state
    }

    fn note_owner(
        &mut self,
        _action_owner: &str,
        _owner: crate::shared_path::SharedPath,
        _key: &ConfiguredTargetKey,
    ) {
        // Noted by the target's own prepare.
    }

    fn prerequisite_errors(
        &mut self,
        _rule: &Rule,
        _key: &ConfiguredTargetKey,
        _exec_config: ConfigId,
        _targets: &BTreeMap<String, DependencyTarget>,
    ) -> Result<(), String> {
        // Checked by the target's own prepare, which passed: a target
        // whose prerequisite failures were suppressed answers no actions
        // (an AnalysisFailureInfo target keeps empty views, never a Lazy).
        Ok(())
    }

    fn edge_view(
        &mut self,
        rule: &Rule,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<DependencyTarget, String> {
        if aspects.is_empty() {
            let PlanState {
                dry,
                custom_providers,
                provider_keys,
                runfiles_info,
                artifact_index,
                declared,
                native_output_groups,
                validation,
                target_identities,
                views,
                ..
            } = &mut *self.state;
            return self.engine.plan_dependency_view(
                self.configured,
                dry,
                custom_providers,
                provider_keys,
                runfiles_info,
                artifact_index,
                declared,
                native_output_groups,
                validation,
                target_identities,
                views,
                label,
                edge_config,
            );
        }
        Ok(self
            .engine
            .plan_aspect_dependency(self.state, self.configured, label, edge_config, aspects)
            .map_err(|error| located(rule.location.as_deref(), error))?
            .0)
    }
}

impl Engine {
    /// Fill the views of every target in `wanted` (an analysis's
    /// [`crate::target_actions::take_wanted`]). `false`: one could not be
    /// filled (the reader's sentinel then stands for its own error).
    pub(super) fn fill_target_views(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        wanted: &[crate::target_actions::Wanted],
    ) -> bool {
        wanted
            .iter()
            .all(|wanted| self.fill_one_target_views(state, configured, wanted, 0))
    }

    /// Fill the marker `wanted` read, and the planning state's of its
    /// target (the same marker, unless the view it was read from holds a
    /// copy): both get the views of the target's analysis again.
    fn fill_one_target_views(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        wanted: &crate::target_actions::Wanted,
        depth: usize,
    ) -> bool {
        let Some(seen) = crate::target_actions::Lazy::from_value(wanted.marker.value()) else {
            return false;
        };
        if seen.views().is_some() {
            return true;
        }
        let key = &wanted.key;
        let planned = state
            .custom_providers
            .get(key)
            .and_then(|providers| providers.get(crate::target_actions::PROVIDER_ID))
            .cloned();
        let planned_lazy = planned
            .as_ref()
            .and_then(|marker| crate::target_actions::Lazy::from_value(marker.value()));
        if let Some(views) = planned_lazy.and_then(crate::target_actions::Lazy::views) {
            seen.fill(views.clone());
            return true;
        }
        if depth >= MAX_FILLS {
            return false;
        }
        let Some(rule) = self.configured_rules.get(key).cloned() else {
            return false;
        };
        // Nothing of the analysis again is observable but its views: its
        // console is dropped (the target's own analysis wrote it), its
        // observations are committed (the same reads), nothing is
        // registered or planned.
        let isolation = parallel::Isolation::enter();
        let views = self.target_views_again(state, configured, key, &rule, depth);
        let (effects, sentinel) = isolation.finish();
        effects.apply(self, false);
        match views {
            Some(views) if !sentinel => {
                if let Some(planned) = planned_lazy {
                    planned.fill(views.clone());
                }
                seen.fill(views);
                true
            }
            _ => false,
        }
    }

    /// The views of `key`'s analysis with them kept.
    fn target_views_again(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        depth: usize,
    ) -> Option<crate::target_actions::TargetActions> {
        let prepared = Engine::prepare_custom(
            &mut ViewsPrepare {
                engine: self,
                state,
                configured,
            },
            key,
            rule,
        )
        .ok()?;
        let PreparedKind::Custom(custom) = prepared.kind else {
            return None;
        };
        let mut snapshot = custom.snapshot;
        snapshot.retain_actions = true;
        let (mut snapshot, mut outcome) = Engine::compute_target(snapshot);
        for _ in 0..MAX_FILLS {
            let Err(error) = &outcome.analyzed else {
                break;
            };
            if error.contains(crate::platforms::NEEDS_TARGET_PLATFORM) {
                self.resolve_deferred_platform(&mut snapshot, prepared.config, configured)
                    .ok()?;
            } else if crate::target_actions::needs_views(error) {
                let wanted = std::mem::take(&mut outcome.wanted_views);
                if !wanted
                    .iter()
                    .all(|wanted| self.fill_one_target_views(state, configured, wanted, depth + 1))
                {
                    return None;
                }
            } else {
                return None;
            }
            (snapshot, outcome) = Engine::compute_target(snapshot);
        }
        let analyzed = outcome.analyzed.ok()?;
        let views = analyzed.providers.get(crate::target_actions::PROVIDER_ID)?;
        crate::target_actions::TargetActions::from_value(views.value()).cloned()
    }

    /// A rule implementation's compute that asked for views (its commit):
    /// filled, then computed again here, until it asks for none.
    pub(super) fn rule_with_target_views(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        mut snapshot: ComputeSnapshot,
        mut outcome: ComputeOutcome,
    ) -> (ComputeOutcome, ComputeSnapshot) {
        for _ in 0..MAX_FILLS {
            if !outcome
                .analyzed
                .as_ref()
                .is_err_and(|error| crate::target_actions::needs_views(error))
            {
                return (outcome, snapshot);
            }
            let wanted = std::mem::take(&mut outcome.wanted_views);
            if !self.fill_target_views(&mut state.plan_state(), configured, &wanted) {
                break;
            }
            (snapshot, outcome) = Engine::compute_target(snapshot);
        }
        outcome.analyzed = outcome.analyzed.map_err(crate::target_actions::unwanted);
        (outcome, snapshot)
    }

    /// A reader's analysis `attempt` with the views it asked for filled:
    /// `analyze` runs it again after each fill (its earlier attempt
    /// dropped, prints included) until it asks for none; a sentinel left
    /// (a fill failed) is the reader's own error.
    pub(super) fn with_target_views<T>(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        mut attempt: (Result<T, String>, Vec<String>),
        wanted: impl Fn() -> Vec<crate::target_actions::Wanted>,
        mut analyze: impl FnMut() -> (Result<T, String>, Vec<String>),
    ) -> (Result<T, String>, Vec<String>) {
        for _ in 0..MAX_FILLS {
            if !attempt
                .0
                .as_ref()
                .is_err_and(|error| crate::target_actions::needs_views(error))
            {
                return attempt;
            }
            if !self.fill_target_views(state, configured, &wanted()) {
                break;
            }
            attempt = analyze();
        }
        (
            attempt.0.map_err(crate::target_actions::unwanted),
            attempt.1,
        )
    }
}
