//! The sequential walk as a [`Host`]: the engine with the phase-1 maps of
//! the plan being built (`active`, `configured`, `plan`). Every primitive
//! is today's code: `BAZEL_RS_ANALYSIS=walk` behaves exactly as before
//! the phase-1 bodies moved to [`super::phase1`].
//!
//! Owner: worker B.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::configuration::{ConfigId, Configuration};
use crate::label::{Label, PackageId};
use crate::package::Rule;
use crate::package_specification::GroupNode;
use crate::platforms::PlatformInfo;
use crate::repository::RepositorySet;
use crate::toolchains::{RegisteredToolchains, ToolchainContextKey, UnloadedToolchainContext};
use crate::visibility::Visibility;

use super::super::super::{
    AspectToolchains, ConfiguredNode, ConfiguredNodes, ConfiguredTargetKey, Engine,
    ExecutionPlatforms, Plan, PlatformError, Registration, ResolutionError, RunUnderEdge, Target,
    TransitionCacheKey, TransitionConfigurations, TransitionDetails, cquery, incremental,
};
use super::host::Host;
use super::phase1::Phase1;

/// The walk's host: the engine and the plan's phase-1 maps.
pub(crate) struct Walk<'a> {
    pub(crate) engine: &'a mut Engine,
    pub(crate) active: &'a mut Vec<ConfiguredTargetKey>,
    pub(crate) configured: &'a mut ConfiguredNodes,
    pub(crate) plan: &'a mut Plan,
}

impl<'a> Walk<'a> {
    pub(crate) fn new(
        engine: &'a mut Engine,
        active: &'a mut Vec<ConfiguredTargetKey>,
        configured: &'a mut ConfiguredNodes,
        plan: &'a mut Plan,
    ) -> Self {
        Self {
            engine,
            active,
            configured,
            plan,
        }
    }

    /// The walk's `analyze` (M5 timing around [`Self::analyze_key`]).
    fn analyze_timed(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        // M5: phase-1 steps inside the plan span (see plan_timings).
        let timing = crate::plan_timings::enabled();
        let outermost = timing && self.engine.analyze_depth == 0;
        if timing {
            self.engine.analyze_depth += 1;
        }
        let before = self.plan.len();
        let mut timer = crate::plan_timings::StageTimer::start_outermost(
            crate::plan_timings::PlanStage::Analyze,
            u64::MAX,
            0,
            outermost,
        );
        let result = Host::analyze_key(self, label, config, true);
        if timing {
            self.engine.analyze_depth -= 1;
            if self.plan.len() > before {
                timer.set_index(before as u64);
            }
        }
        timer.finish();
        result
    }
}

impl Host for Walk<'_> {
    fn repositories(&self) -> &Arc<RepositorySet> {
        &self.engine.repositories
    }

    fn configuration(&self, config: ConfigId) -> Arc<Configuration> {
        Arc::clone(self.engine.configurations.get(config))
    }

    fn top_configuration(&self) -> Arc<Configuration> {
        Arc::clone(self.engine.configurations.top_configuration())
    }

    fn configuration_mnemonic(&self, config: ConfigId) -> String {
        self.engine.configurations.mnemonic(config).to_owned()
    }

    fn intern_exec(
        &mut self,
        from: ConfigId,
        platform: Label,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        self.engine
            .configurations
            .intern_exec(from, platform, target_scoped)
    }

    fn intern_derived(
        &mut self,
        configuration: Configuration,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        self.engine
            .configurations
            .intern_derived(configuration, target_scoped)
    }

    fn intern_no_config(&mut self) -> ConfigId {
        self.engine.configurations.intern_no_config()
    }

    fn cquery(&self) -> bool {
        self.engine.cquery_capture.is_some()
    }

    fn cquery_capture(&mut self) -> Option<&mut cquery::Capture> {
        self.engine.cquery_capture.as_mut()
    }

    fn run_under(&self) -> Option<&RunUnderEdge> {
        self.engine.run_under.as_ref()
    }

    fn requested_trimmed_root(&self) -> Option<ConfiguredTargetKey> {
        self.engine.requested_trimmed_root.clone()
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.engine.target(label)
    }

    fn load_package(&mut self, package: &PackageId) -> Result<(), String> {
        self.engine.load_package(package)
    }

    fn source_visibility(&mut self, label: &Label) -> Visibility {
        self.engine.source_visibility(label)
    }

    fn resolve_group(&mut self, label: &Label) -> Result<Arc<GroupNode>, String> {
        self.engine.resolve_group(label, &mut Vec::new())
    }

    fn package_args(&mut self, package: &PackageId) -> crate::package::PackageArgs {
        self.engine.packages[package].args.clone()
    }

    fn implicit_tests(&mut self, suite: &Rule) -> Vec<Label> {
        self.engine.implicit_tests(suite)
    }

    fn expand_registration_patterns(
        &mut self,
        patterns: &[(String, String)],
        kind: Registration,
    ) -> Result<Vec<Label>, ResolutionError> {
        self.engine.expand_registration_patterns(patterns, kind)
    }

    fn warm_toolchain_patterns(&mut self, patterns: &[(String, String)]) {
        self.engine.repositories.warm_registrations(patterns);
        self.engine.prefetch_toolchain_patterns(patterns);
    }

    fn module_execution_platforms(
        &mut self,
        resolve: bool,
    ) -> Result<Vec<(String, String)>, String> {
        self.engine.module_execution_platforms(resolve)
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.engine.attribute_aspects(rule, identities)
    }

    fn loaded_attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Vec<Arc<crate::aspects::AspectDefinition>> {
        self.engine.loaded_attribute_aspects(rule, identities)
    }

    fn analyze(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        self.analyze_timed(label, config)
    }

    fn analyze_key(
        &mut self,
        label: &Label,
        config: ConfigId,
        apply_rule_transition: bool,
    ) -> Result<(), String> {
        // Incremental analysis: the frame `analyze_key_body` pushes for the
        // key it analyzes ends with it.
        let depth = self.engine.track_depth();
        let result = self.analyze_key_body(label, config, apply_rule_transition);
        self.engine.track_restore(depth);
        result
    }

    fn analyze_detached(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        let saved = std::mem::take(self.active);
        let result = self.analyze_timed(label, config);
        *self.active = saved;
        result
    }

    fn configured_node(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredNode> {
        self.configured.get(key).map(|node| (**node).clone())
    }

    fn configured_rule(&mut self, key: &ConfiguredTargetKey) -> Option<Arc<Rule>> {
        self.engine.configured_rules.get(key).cloned()
    }

    fn rule_delegation(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredTargetKey> {
        self.engine.rule_delegations.get(key).cloned()
    }

    fn toolchain_context_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<UnloadedToolchainContext> {
        self.engine.toolchain_contexts.get(key).cloned()
    }

    fn exec_groups_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::exec_groups::TargetExecGroups> {
        self.engine.exec_group_contexts.get(key).cloned()
    }

    fn exec_config_of(&mut self, key: &ConfiguredTargetKey) -> Option<ConfigId> {
        self.engine.exec_configurations.get(key).copied()
    }

    fn group_exec_config_of(&mut self, key: &ConfiguredTargetKey, group: &str) -> Option<ConfigId> {
        self.engine
            .group_exec_configurations
            .get(&(key.clone(), group.to_owned()))
            .copied()
    }

    fn edge_configs_of(
        &mut self,
        key: &ConfiguredTargetKey,
        attribute: &str,
    ) -> Option<TransitionConfigurations> {
        self.engine
            .edge_configs
            .get(key)
            .and_then(|attributes| attributes.get(attribute))
            .cloned()
    }

    fn source_analyzed(&mut self, label: &Label) -> bool {
        self.engine.analyzed_sources.contains(label)
    }

    fn record_source_analyzed(&mut self, label: &Label) {
        self.engine.analyzed_sources.insert(label.clone());
    }

    fn configured_contains(&mut self, key: &ConfiguredTargetKey) -> bool {
        self.configured.contains_key(key)
    }

    fn record_keep_going_edge(&mut self, key: &ConfiguredTargetKey) {
        if self.engine.keep_going {
            self.engine
                .keep_going_state
                .record_edge(self.active.last(), key);
        }
    }

    fn active_push(&mut self, key: &ConfiguredTargetKey) {
        self.active.push(key.clone());
    }

    fn active_pop(&mut self) {
        self.active.pop();
    }

    fn active_position(&mut self, key: &ConfiguredTargetKey) -> Option<usize> {
        self.active.iter().position(|active| active == key)
    }

    fn active_len(&mut self) -> usize {
        self.active.len()
    }

    fn active_keys(&mut self) -> Vec<ConfiguredTargetKey> {
        self.active.clone()
    }

    fn ahead(&self) -> bool {
        self.engine.ahead
    }

    fn ahead_abort(&mut self, what: &str) -> String {
        self.engine.ahead_abort(what)
    }

    fn track_key(&mut self, key: &ConfiguredTargetKey) -> Option<u32> {
        self.engine.track_key(key)
    }

    fn track_enter(&mut self, tracked: Option<u32>) {
        self.engine.track_enter(tracked);
    }

    fn track_entry(&mut self, tracked: Option<u32>, plan_len: usize) {
        self.engine.track_entry(tracked, plan_len);
    }

    fn track_position(&mut self, tracked: Option<u32>, plan_len: usize) {
        self.engine.track_position(tracked, plan_len);
    }

    fn track_source(&mut self, label: &Label) {
        self.engine.track_source(label);
    }

    fn prefetch_rule(&mut self, rule: &Rule) {
        self.engine.prefetch_rule(rule);
    }

    fn prefetch_note_condition(
        &mut self,
        text: &str,
        matching: &crate::configured_attributes::ConfigMatching,
    ) {
        // Phase-1 prefetch: which select() branches the build certainly
        // takes (crate::prefetch::certain_labels).
        if let Some(prefetch) = &self.engine.prefetch
            && let Ok(key) = Label::parse_canonical(text)
        {
            prefetch.note_condition(
                &key,
                match &matching.result {
                    crate::configured_attributes::MatchResult::Match => Some(true),
                    crate::configured_attributes::MatchResult::NoMatch(_) => Some(false),
                    crate::configured_attributes::MatchResult::InError(_) => None,
                },
            );
        }
    }

    fn prefetch_platforms(&mut self, target: &Arc<PlatformInfo>, host: &Arc<PlatformInfo>) {
        // Phase-1 prefetch: from here on, the implementation of every
        // toolchain compatible with these platforms is queued as soon as
        // the toolchain is analyzed (`prefetch_compatible_toolchain`).
        if self.engine.prefetch.is_some() && self.engine.prefetch_platforms.is_none() {
            self.engine.prefetch_platforms = Some((target.clone(), host.clone()));
        }
    }

    fn prefetch_resolved(&mut self, resolved: &UnloadedToolchainContext) {
        // Phase-1 prefetch: the selected implementations are analyzed after
        // the rule's attribute dependencies.
        if self.engine.prefetch.is_some() {
            for label in resolved.resolved_labels() {
                if self.engine.prefetch_followed.insert(label.clone())
                    && let Some(prefetch) = &self.engine.prefetch
                {
                    prefetch.submit_targets([(label.package_id(), Some(label.name))], 1, true);
                }
            }
        }
    }

    fn prefetch_compatible_toolchain(
        &mut self,
        declared: &crate::toolchains::DeclaredToolchainInfo,
    ) {
        self.engine.prefetch_compatible_toolchain(declared);
    }

    fn plan_len(&mut self) -> usize {
        self.plan.len()
    }

    fn plan_push(&mut self, key: ConfiguredTargetKey, rule: Arc<Rule>) {
        self.plan.push((key, rule));
    }

    fn publish_node(&mut self, key: &ConfiguredTargetKey, node: ConfiguredNode) {
        self.configured.insert(key.clone(), Arc::new(node));
    }

    fn record_delegation(&mut self, key: ConfiguredTargetKey, owner: ConfiguredTargetKey) {
        self.engine.rule_delegations.insert(key, owner);
    }

    fn record_configured_rule(&mut self, key: &ConfiguredTargetKey, rule: &Arc<Rule>) {
        self.engine
            .configured_rules
            .insert(key.clone(), Arc::clone(rule));
    }

    fn record_empty_configuration(&mut self, key: &ConfiguredTargetKey) {
        self.engine.empty_configuration_rules.insert(key.clone());
    }

    fn record_exec_config(&mut self, key: &ConfiguredTargetKey, config: ConfigId) {
        self.engine.exec_configurations.insert(key.clone(), config);
    }

    fn record_group_exec_config(
        &mut self,
        key: &ConfiguredTargetKey,
        group: &str,
        config: ConfigId,
    ) {
        self.engine
            .group_exec_configurations
            .insert((key.clone(), group.to_owned()), config);
    }

    fn record_edge_configs(
        &mut self,
        key: &ConfiguredTargetKey,
        configs: BTreeMap<String, TransitionConfigurations>,
    ) {
        self.engine.edge_configs.insert(key.clone(), configs);
    }

    fn record_deferred_duplicates(&mut self, key: &ConfiguredTargetKey, errors: Vec<String>) {
        self.engine.deferred_duplicates.insert(key.clone(), errors);
    }

    fn record_toolchain_context(
        &mut self,
        key: &ConfiguredTargetKey,
        context: UnloadedToolchainContext,
    ) {
        self.engine.toolchain_contexts.insert(key.clone(), context);
    }

    fn record_exec_groups(
        &mut self,
        key: &ConfiguredTargetKey,
        groups: crate::exec_groups::TargetExecGroups,
    ) {
        self.engine.exec_group_contexts.insert(key.clone(), groups);
    }

    fn warn_once(&mut self, warning: String) {
        if !self.engine.warnings.contains(&warning) {
            self.engine.warnings.push(warning);
        }
    }

    fn warn(&mut self, warning: String) {
        self.engine.warnings.push(warning);
    }

    fn print(&mut self, line: &str) {
        self.engine.note_analysis_print(line);
    }

    fn capture_cquery_edge(
        &mut self,
        from: &ConfiguredTargetKey,
        label: Label,
        config: ConfigId,
        attribute: &str,
        tool: bool,
        aspect: bool,
    ) {
        self.engine
            .capture_cquery_edge(from, label, config, attribute, tool, aspect);
    }

    fn capture_cquery_aspect_toolchain_context(
        &mut self,
        config: ConfigId,
        context: &ToolchainContextKey,
    ) -> Vec<u32> {
        self.engine
            .capture_cquery_aspect_toolchain_context(config, context)
    }

    fn flush_pending(&mut self, config_setting: &Rule) -> Result<(), String> {
        self.engine
            .plan_pending(config_setting, self.plan, self.configured)
    }

    fn planned_feature_flag(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::feature_flags::FeatureFlagValue> {
        self.engine.planned_feature_flag(key)
    }

    fn platform_tree(&self) -> bool {
        !self.engine.resolving_platforms.is_empty()
    }

    fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let _shared = incremental::SharedScope::enter("platform_info");
        let key = ConfiguredTargetKey::new(label.clone(), config);
        if let Some(cached) = self.engine.platforms.get(&key) {
            return cached.clone();
        }
        if self.engine.resolving_platforms.contains(&key) {
            return Err(PlatformError::Other(format!(
                "platform {} is needed while it is being resolved; this is not implemented in the native profile",
                crate::rule_declarations::canonical_form(label)
            )));
        }
        self.engine.resolving_platforms.push(key.clone());
        let result = self.compute_platform_info(label, config);
        self.engine.resolving_platforms.pop();
        self.engine.platforms.insert(key, result.clone());
        result
    }

    fn registered_toolchains(
        &mut self,
        config: ConfigId,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        let _shared = incremental::SharedScope::enter("registered_toolchains");
        if let Some(cached) = self.engine.registered_toolchains.get(&config) {
            return cached.clone();
        }
        if let Some(key) = self
            .engine
            .computing_toolchains
            .iter()
            .find(|computing| computing.config == config)
        {
            // Reached only through resolve_toolchains, which reports the
            // cycle itself (Phase1::toolchain_cycle).
            return Err(ResolutionError::Propagated(format!(
                "internal error: registered toolchains needed while analyzing {}",
                crate::rule_declarations::canonical_form(&key.label)
            )));
        }
        // Set to each registered target while it is analyzed.
        self.engine
            .computing_toolchains
            .push(ConfiguredTargetKey::new(
                Label {
                    repository: String::new(),
                    package: String::new(),
                    name: String::new(),
                },
                config,
            ));
        let result = self.compute_registered_toolchains(config);
        self.engine.computing_toolchains.pop();
        self.engine
            .registered_toolchains
            .insert(config, result.clone());
        if result.is_ok()
            && let Some(request) = self.engine.root_checkpoint.as_mut()
        {
            request.first.get_or_insert(config);
        }
        result
    }

    fn registered_toolchains_in_progress(&mut self, config: ConfigId) -> bool {
        self.engine
            .computing_toolchains
            .iter()
            .any(|computing| computing.config == config)
            && !self.engine.registered_toolchains.contains_key(&config)
    }

    fn set_computing_toolchain(&mut self, label: &Label) {
        if let Some(computing) = self.engine.computing_toolchains.last_mut() {
            computing.label = label.clone();
        }
    }

    fn record_toolchain_cycle_report(&mut self, config: ConfigId, report: String) {
        self.engine.toolchain_cycle_reports.insert(config, report);
    }

    fn take_toolchain_cycle_report(&mut self, config: ConfigId) -> Option<String> {
        self.engine.toolchain_cycle_reports.remove(&config)
    }

    fn registered_execution_platforms(
        &mut self,
        config: ConfigId,
        requester: &Label,
    ) -> Result<ExecutionPlatforms, ResolutionError> {
        if let Some(cached) = self.engine.registered_execution_platforms.get(&config) {
            return cached.clone();
        }
        self.engine
            .computing_execution_platforms
            .push(ConfiguredTargetKey::new(requester.clone(), config));
        let result = self.compute_execution_platforms(config);
        self.engine.computing_execution_platforms.pop();
        self.engine
            .registered_execution_platforms
            .insert(config, result.clone());
        result
    }

    fn execution_platforms_in_progress(&mut self, config: ConfigId) -> bool {
        !self
            .engine
            .registered_execution_platforms
            .contains_key(&config)
            && self
                .engine
                .computing_execution_platforms
                .iter()
                .any(|computing| computing.config == config)
    }

    fn record_execution_platform_cycle_report(&mut self, config: ConfigId, report: String) {
        self.engine
            .execution_platform_cycle_reports
            .insert(config, report);
    }

    fn take_execution_platform_cycle_report(&mut self, config: ConfigId) -> Option<String> {
        self.engine.execution_platform_cycle_reports.remove(&config)
    }

    fn exec_transition(&mut self, config: ConfigId, platform: &Label) -> Result<ConfigId, String> {
        let key = (config, platform.clone());
        if let Some(&exec) = self.engine.exec_transitions.get(&key) {
            // Debug builds recompute a memoized exec transition on every
            // hit: its key must cover every input of the computation. The
            // recomputation has no side effects: the flag scopes and the
            // exec configuration were recorded by the memoized call.
            #[cfg(debug_assertions)]
            {
                let recomputed = self.compute_exec_transition(config, platform);
                assert_eq!(
                    recomputed,
                    Ok(exec),
                    "internal error: stale exec configuration memo for {config} and {platform}"
                );
            }
            return Ok(exec);
        }
        let exec = self.compute_exec_transition(config, platform)?;
        self.engine.exec_transitions.insert(key, exec);
        Ok(exec)
    }

    fn flag_scope(&mut self, label: &Label) -> Result<bool, String> {
        if let Some(scoped) = self.engine.flag_scopes.get(label) {
            return Ok(*scoped);
        }
        let scoped = self.compute_flag_scope(label)?;
        self.engine.flag_scopes.insert(label.clone(), scoped);
        Ok(scoped)
    }

    fn transition_details_for(
        &mut self,
        settings: Vec<Label>,
    ) -> Result<TransitionDetails, String> {
        if let Some(details) = self.engine.transition_details.get(&settings) {
            return details.clone();
        }
        let details = self.compute_transition_details(&settings);
        self.engine
            .transition_details
            .insert(settings, details.clone());
        details
    }

    fn transition_cache_get(
        &mut self,
        key: &TransitionCacheKey,
    ) -> Option<(TransitionConfigurations, Vec<String>)> {
        self.engine
            .transition_cache
            .get(key)
            .map(|(id, printed)| (Arc::clone(id), printed.clone()))
    }

    fn transition_cache_put(
        &mut self,
        key: TransitionCacheKey,
        value: (TransitionConfigurations, Vec<String>),
    ) {
        self.engine.transition_cache.insert(key, value);
    }

    fn claim_toolchain_implementation(
        &mut self,
        key: ConfiguredTargetKey,
        exec_platform: &Label,
        _location: Option<&str>,
    ) -> Option<Label> {
        match self.engine.toolchain_implementations.get(&key) {
            Some(existing) => Some(existing.clone()),
            None => {
                self.engine
                    .toolchain_implementations
                    .insert(key, exec_platform.clone());
                None
            }
        }
    }

    fn forced_exec(&mut self, key: &ConfiguredTargetKey) -> Option<Label> {
        self.engine.toolchain_implementations.get(key).cloned()
    }

    fn resolving_aspect_toolchains(&self) -> bool {
        self.engine.resolving_aspect_toolchains
    }

    fn set_resolving_aspect_toolchains(&mut self, resolving: bool) {
        self.engine.resolving_aspect_toolchains = resolving;
    }

    fn aspect_predicate(&mut self, memo: &(Label, String)) -> Option<bool> {
        self.engine.aspect_predicates.get(memo).copied()
    }

    fn set_aspect_predicate(&mut self, memo: (Label, String), value: bool) {
        self.engine.aspect_predicates.insert(memo, value);
    }

    fn aspect_attr_aspects(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<(Option<Arc<[String]>>, Option<Arc<[String]>>)> {
        self.engine.aspect_attr_aspects.get(memo).cloned()
    }

    fn set_aspect_attr_aspects(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: (Option<Arc<[String]>>, Option<Arc<[String]>>),
    ) {
        self.engine.aspect_attr_aspects.insert(memo, value);
    }

    fn aspect_toolchains(&mut self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>> {
        self.engine.aspect_toolchains.get(memo).cloned()
    }

    fn set_aspect_toolchains(&mut self, memo: (ConfigId, String), value: Arc<AspectToolchains>) {
        self.engine.aspect_toolchains.insert(memo, value);
    }

    fn aspect_attribute_configs(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<Arc<BTreeMap<String, TransitionConfigurations>>> {
        self.engine.aspect_attribute_configs.get(memo).cloned()
    }

    fn set_aspect_attribute_configs(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: Arc<BTreeMap<String, TransitionConfigurations>>,
    ) {
        self.engine.aspect_attribute_configs.insert(memo, value);
    }
}

/// The engine's entry points into the phase-1 bodies (today's signatures).
impl Engine {
    /// Analyze `label` in `config` as part of the plan being built.
    pub(in crate::engine) fn analyze(
        &mut self,
        label: &Label,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        Host::analyze(
            &mut Walk::new(self, active, configured, plan),
            label,
            config,
        )
    }

    /// [`Phase1::target_platform`] in the walk.
    pub(in crate::engine) fn target_platform(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, String> {
        Walk::new(self, &mut Vec::new(), configured, plan).target_platform(config)
    }

    /// [`Host::platform_info`] in the walk.
    pub(in crate::engine) fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        Host::platform_info(
            &mut Walk::new(self, &mut Vec::new(), configured, plan),
            label,
            config,
        )
    }

    /// [`Phase1::analyze_aspect_applications`] in the walk.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine) fn analyze_aspect_applications(
        &mut self,
        label: &Label,
        config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
        seen: &mut BTreeSet<(ConfiguredTargetKey, Vec<String>)>,
    ) -> Result<Vec<u32>, String> {
        Walk::new(self, active, configured, plan)
            .analyze_aspect_applications(label, config, aspects, seen)
    }

    /// [`Phase1::aspect_collection_for`] (its memo only).
    pub(in crate::engine) fn aspect_collection_for(
        &mut self,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        Walk::new(
            self,
            &mut Vec::new(),
            &mut ConfiguredNodes::default(),
            &mut Vec::new(),
        )
        .aspect_collection_for(aspects, rule)
    }

    /// [`Phase1::with_computed_attr_aspects`] (its memo only).
    pub(in crate::engine) fn with_computed_attr_aspects(
        &mut self,
        collection: crate::aspects::AspectCollection,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        Walk::new(
            self,
            &mut Vec::new(),
            &mut ConfiguredNodes::default(),
            &mut Vec::new(),
        )
        .with_computed_attr_aspects(collection, key, rule)
    }
}
