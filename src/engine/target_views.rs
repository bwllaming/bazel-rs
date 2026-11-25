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
        // The target was prepared and committed before: its dependency
        // views and its edges' aspect applications are memo hits (nothing
        // analyzed or committed again; debug builds check it).
        if aspects.is_empty() {
            #[cfg(debug_assertions)]
            {
                let key = ConfiguredTargetKey::new(label.clone(), edge_config);
                let memoized = match &self.engine.frozen_walk {
                    Some(frozen) => {
                        frozen.views.contains_key(&key) || self.state.views.contains_key(&key)
                    }
                    None => self.state.views.contains_key(&key),
                };
                debug_assert!(
                    memoized,
                    "internal error: a Target.actions fill's view of {label} is not memoized"
                );
            }
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
        let applications = self.state.aspect_providers.len();
        let planned = self
            .engine
            .plan_aspect_dependency(self.state, self.configured, label, edge_config, aspects)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        debug_assert!(
            self.state.aspect_providers.len() == applications
                && planned
                    .1
                    .iter()
                    .all(|(key, _)| self.state.aspect_providers.contains_key(key)),
            "internal error: a Target.actions fill planned an aspect application of {label} again"
        );
        let _ = applications;
        Ok(planned.0)
    }
}

/// How deep a fill may itself fill (a target whose analysis again reads
/// the views of another): the dependency graph's depth, bounded against a
/// bug looping.
const MAX_DEPTH: usize = 64;

/// The round of a reader's retries from which the unfilled markers of all
/// its direct dependency views are filled with what it asked for: a reader
/// of one target's actions pays one retry, a reader of many (`[d.actions
/// for d in ctx.attr.deps]`, which asks for one per attempt) two.
const FILL_DIRECT_FROM_ROUND: usize = 1;

/// `PERF-TARGET-VIEWS` (`BAZEL_RS_TIMING=1`): readers that asked, their
/// retries, the markers filled.
static ASKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn bump(counter: &std::sync::atomic::AtomicU64) {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `PERF-TARGET-VIEWS` line.
pub(super) fn report() {
    let get =
        |counter: &std::sync::atomic::AtomicU64| counter.load(std::sync::atomic::Ordering::Relaxed);
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-TARGET-VIEWS readers={} retries={} filled={}",
        get(&ASKS),
        get(&RETRIES),
        get(&FILLS)
    ));
}

/// The markers not filled yet of `views` (a reader's direct dependency
/// views).
pub(super) fn unfilled<'a>(
    views: impl Iterator<Item = &'a DependencyTarget>,
) -> Vec<crate::target_actions::Wanted> {
    let mut found: Vec<crate::target_actions::Wanted> = Vec::new();
    for view in views {
        let Some(marker) = view.providers.get(crate::target_actions::PROVIDER_ID) else {
            continue;
        };
        let Some(lazy) = crate::target_actions::Lazy::from_value(marker.value()) else {
            continue;
        };
        if lazy.views().is_none() && !found.iter().any(|known| known.key == lazy.key) {
            found.push(crate::target_actions::Wanted {
                key: lazy.key.clone(),
                marker: marker.clone(),
            });
        }
    }
    found
}

/// What one fill did.
enum Filled {
    /// The marker had its views already.
    Already,
    /// Filled now.
    Now,
    /// The target could not be analyzed again for them.
    Failed,
}

impl Engine {
    /// Fill the views of every target in `wanted`: `Ok(n)`, the markers
    /// filled now; `Err`: one could not be.
    fn fill_target_views(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        wanted: &[crate::target_actions::Wanted],
        depth: usize,
    ) -> Result<usize, ()> {
        let mut now = 0;
        for wanted in wanted {
            match self.fill_one_target_views(state, configured, wanted, depth) {
                Filled::Already => {}
                Filled::Now => now += 1,
                Filled::Failed => return Err(()),
            }
        }
        Ok(now)
    }

    /// One round of a reader's retries (`round` from 0): fill what it
    /// asked for (`error`'s sentinel, `wanted`) and, from
    /// [`FILL_DIRECT_FROM_ROUND`], every unfilled marker of its direct
    /// dependency views (`direct`). `Err`: the explicit error the reader
    /// fails with — a fill failed, or a retry asked again for nothing new
    /// (it would ask forever). Terminates: each round after the first fills
    /// at least one marker, a marker is only ever filled, and there are
    /// finitely many.
    pub(super) fn reader_fill_round(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        error: &str,
        mut wanted: Vec<crate::target_actions::Wanted>,
        direct: &dyn Fn() -> Vec<crate::target_actions::Wanted>,
        round: usize,
    ) -> Result<(), String> {
        if round == 0 {
            bump(&ASKS);
        }
        if round >= FILL_DIRECT_FROM_ROUND {
            for more in direct() {
                if !wanted.iter().any(|known| known.key == more.key) {
                    wanted.push(more);
                }
            }
        }
        // The first attempt may have run on a worker before another reader
        // filled what it asked for (nothing new to fill: it runs again);
        // every later one runs here, after the round, and asks only for
        // markers unfilled then.
        match self.fill_target_views(state, configured, &wanted, 0) {
            Ok(filled) if filled > 0 || round == 0 => {
                bump(&RETRIES);
                Ok(())
            }
            _ => Err(crate::target_actions::unwanted(error.to_owned())),
        }
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
    ) -> Filled {
        let Some(seen) = crate::target_actions::Lazy::from_value(wanted.marker.value()) else {
            return Filled::Failed;
        };
        if seen.views().is_some() {
            return Filled::Already;
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
            return Filled::Now;
        }
        if depth >= MAX_DEPTH {
            return Filled::Failed;
        }
        let Some(rule) = self.configured_rules.get(key).cloned() else {
            return Filled::Failed;
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
                bump(&FILLS);
                if let Some(planned) = planned_lazy {
                    planned.fill(views.clone());
                }
                seen.fill(views);
                Filled::Now
            }
            _ => Filled::Failed,
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
        let mut platform_resolved = false;
        while let Err(error) = &outcome.analyzed {
            if error.contains(crate::platforms::NEEDS_TARGET_PLATFORM) && !platform_resolved {
                platform_resolved = true;
                self.resolve_deferred_platform(&mut snapshot, prepared.config, configured)
                    .ok()?;
            } else if crate::target_actions::needs_views(error) {
                // As a reader's rounds, its direct dependencies at once (it
                // reads their actions: its own analysis did, from filled
                // markers or not at all).
                let mut wanted = std::mem::take(&mut outcome.wanted_views);
                for more in unfilled(snapshot.dependency_targets.values()) {
                    if !wanted.iter().any(|known| known.key == more.key) {
                        wanted.push(more);
                    }
                }
                match self.fill_target_views(state, configured, &wanted, depth + 1) {
                    Ok(filled) if filled > 0 => {}
                    _ => return None,
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

    /// A reader's analysis `attempt` (an aspect application) with the
    /// views it asked for filled: `analyze` runs it again after each round
    /// ([`Self::reader_fill_round`]; its earlier attempt dropped, prints
    /// included) until it asks for none.
    pub(super) fn with_target_views<T>(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        mut attempt: (Result<T, String>, Vec<String>),
        wanted: impl Fn() -> Vec<crate::target_actions::Wanted>,
        direct: &dyn Fn() -> Vec<crate::target_actions::Wanted>,
        mut analyze: impl FnMut() -> (Result<T, String>, Vec<String>),
    ) -> (Result<T, String>, Vec<String>) {
        let mut round = 0;
        loop {
            let error = match &attempt.0 {
                Err(error) if crate::target_actions::needs_views(error) => error.clone(),
                _ => return attempt,
            };
            if let Err(error) =
                self.reader_fill_round(state, configured, &error, wanted(), direct, round)
            {
                return (Err(error), attempt.1);
            }
            round += 1;
            attempt = analyze();
        }
    }
}
