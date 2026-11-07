//! The evaluator as a [`Host`]: a phase-1 body's view of one node
//! attempt. Reads go to [`EvalShared`] or to the values of needed nodes
//! ([`Cx::need`]); what the walk writes to the engine becomes the node's
//! value (`Ct1Ok`, memo values) or a transcript event ([`Event`]); the
//! S1 coordinator services (package loading, target lookups, aspect
//! definitions: Pkg/Tgt nodes in S2) go through [`Phase1Cx::engine`].
//!
//! Owner: worker B.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::configuration::{ConfigId, Configuration};
use crate::label::{Label, PackageId};
use crate::package::{Rule, RuleKind};
use crate::package_specification::GroupNode;
use crate::platforms::PlatformInfo;
use crate::repository::RepositorySet;
use crate::toolchains::{RegisteredToolchains, ToolchainContextKey, UnloadedToolchainContext};
use crate::visibility::Visibility;

use super::super::super::{
    AspectToolchains, ConfiguredNode, ConfiguredTargetKey, EdgeCfg, ExecutionPlatforms,
    PlatformError, Registration, ResolutionError, RunUnderEdge, Target, TransitionCacheKey,
    TransitionConfigurations, TransitionDetails, check_macro_namespace, cquery,
    uses_toolchain_resolution,
};
use super::super::cx::Missing;
use super::super::key::{CtKey, NodeKey, Observes};
use super::super::shared::EvalShared;
use super::super::transcript::{Event, Transcript};
use super::super::value::{Ct1Ok, Ct1Value, NodeValue};
use super::bodies::Phase1Cx;
use super::host::Host;
use super::phase1::Phase1;

/// The S1 first-visit state (the inline DFS order = the walk's; pass 1
/// decides it from S2 on).
#[derive(Default)]
pub(crate) struct FirstVisit {
    /// The first `CtKey` variant of each ConfiguredTargetKey.
    pub(crate) canonical: crate::fast_hash::FxHashMap<ConfiguredTargetKey, CtKey>,
    /// `toolchain_implementations`.
    pub(crate) claims: BTreeMap<ConfiguredTargetKey, Label>,
    pub(crate) toolchain_cycle_reports: BTreeMap<ConfigId, String>,
    pub(crate) execution_platform_cycle_reports: BTreeMap<ConfigId, String>,
    pub(crate) aspect_predicates: BTreeMap<(Label, String), bool>,
    #[allow(clippy::type_complexity)]
    pub(crate) aspect_attr_aspects:
        BTreeMap<(ConfiguredTargetKey, String), (Option<Arc<[String]>>, Option<Arc<[String]>>)>,
    pub(crate) aspect_toolchains: BTreeMap<(ConfigId, String), Arc<AspectToolchains>>,
    pub(crate) aspect_attribute_configs:
        BTreeMap<(ConfiguredTargetKey, String), Arc<BTreeMap<String, TransitionConfigurations>>>,
    /// The requested top-level rule being analyzed when test trimmed.
    pub(crate) requested_trimmed_root: Option<ConfiguredTargetKey>,
    /// StarlarkTransitionCache (S1: a first-visit memo; S2: `Transition`
    /// nodes keyed with their definition).
    pub(crate) transition_cache:
        crate::fast_hash::FxHashMap<TransitionCacheKey, (TransitionConfigurations, Vec<String>)>,
}

/// The error text a body returns when an attempt stops at a [`Missing`]
/// (the stashed reason wins over whatever the body made of the text).
const MISSING: &str = "\u{1}missing";

/// The accumulated `Ct1Ok` fields of a CT1 attempt.
#[derive(Default)]
struct Ct1Acc {
    node: Option<ConfiguredNode>,
    rule: Option<Arc<Rule>>,
    delegate: Option<ConfiguredTargetKey>,
    toolchain_context: Option<UnloadedToolchainContext>,
    exec_groups: Option<crate::exec_groups::TargetExecGroups>,
    exec_config: Option<ConfigId>,
    group_exec_configs: BTreeMap<String, ConfigId>,
    edge_configs: Option<BTreeMap<String, TransitionConfigurations>>,
    deferred_duplicates: Option<Vec<String>>,
    empty_configuration: bool,
    planned: bool,
}

/// One attempt of a phase-1 node body.
pub(crate) struct EvalHost<'c, 'e> {
    shared: &'c EvalShared,
    cx: &'c mut dyn Phase1Cx<'e>,
    /// The CT1 key being analyzed (CT1 and Src bodies).
    own: Option<CtKey>,
    /// Inside a platform's tree (the empty configuration).
    platform_tree: bool,
    acc: Ct1Acc,
    /// A copy of the CT1 transcript (the value carries it).
    events: Vec<Event>,
    missing: Option<Missing>,
    resolving_aspect_toolchains: bool,
}

impl<'c, 'e> EvalHost<'c, 'e> {
    pub(crate) fn new(
        shared: &'c EvalShared,
        cx: &'c mut dyn Phase1Cx<'e>,
        own: Option<CtKey>,
        platform_tree: bool,
    ) -> Self {
        Self {
            shared,
            cx,
            own,
            platform_tree,
            acc: Ct1Acc::default(),
            events: Vec::new(),
            missing: None,
            resolving_aspect_toolchains: false,
        }
    }

    fn emit(&mut self, event: Event) {
        if self.own.is_some() {
            self.events.push(event.clone());
        }
        self.cx.emit(event);
    }

    /// `need` with the attempt's missing reason stashed.
    fn need(&mut self, key: NodeKey) -> Result<Arc<NodeValue>, Missing> {
        self.cx.need_one(key)
    }

    fn stop(&mut self, missing: Missing) -> String {
        self.missing.get_or_insert(missing);
        MISSING.to_owned()
    }

    fn memo_use(&mut self, key: &NodeKey) {
        let id = self.cx.id_of(key);
        self.emit(Event::MemoUse(id));
    }

    fn own_key(&self) -> Option<&ConfiguredTargetKey> {
        self.own.as_ref().map(|own| &own.key)
    }

    /// The done CT1 value of `key`'s first variant (the walk's
    /// `configured` lookup: absent when not analyzed or failed).
    fn ct1_value(&self, key: &ConfiguredTargetKey) -> Option<Arc<super::super::value::Ct1Value>> {
        self.cx.coord().ct1(key)
    }

    fn ct1_ok<T>(&self, key: &ConfiguredTargetKey, read: impl FnOnce(&Ct1Ok) -> T) -> Option<T> {
        let value = self.ct1_value(key)?;
        value.outcome.as_ref().ok().map(read)
    }

    /// The CtKey of an edge to `rule` in `config` from this attempt: the
    /// first variant already analyzed, else this context's.
    fn edge_key(&mut self, rule: &Rule, key: ConfiguredTargetKey) -> CtKey {
        let no_config = self.configuration(key.config).native.no_config;
        let platform_tree = self.platform_tree;
        edge_ct_key(
            &mut self.cx.coord().first,
            no_config,
            rule,
            key,
            platform_tree,
        )
    }

    /// Need the CT1 node of `rule`'s key (a dependency edge): its outcome.
    fn need_ct1(
        &mut self,
        rule: &Arc<Rule>,
        key: ConfiguredTargetKey,
        reach: bool,
    ) -> Result<(), String> {
        let ct = self.edge_key(rule, key.clone());
        let node = NodeKey::Ct1(ct);
        let id = self.cx.id_of(&node);
        if reach {
            self.emit(Event::Reach(id));
        }
        let done = self.cx.coord().ct1(&key).is_some();
        if !done {
            // The walk's active chain checks (Engine::analyze_key_body).
            let active = self.cx.coord().active_ct1();
            if let Some(start) = active.iter().position(|active| *active == key) {
                return Err(self.cycle_error(rule, &active[..start], &active[start..]));
            }
            if active.len() >= 256 {
                return Err("native prototype dependency depth limit (256) exceeded".into());
            }
        }
        match self.need(node) {
            Ok(value) => match value.as_ref() {
                NodeValue::Ct1(value) => value.outcome.as_ref().map(|_| ()).map_err(Clone::clone),
                _ => Err("internal error: a CT1 key with another value".to_owned()),
            },
            Err(Missing::BackEdge(_)) => {
                // An ancestor outside the current detached segment: the
                // whole chain names the cycle.
                let active = self.cx.coord().active_ct1();
                let start = active.iter().position(|active| *active == key).unwrap_or(0);
                Err(self.cycle_error(rule, &active[..start], &active[start..]))
            }
            Err(missing) => Err(self.stop(missing)),
        }
    }

    /// Run the CT1 body for `key` (the evaluator's `analyze_key_body`
    /// after the memo): the loaded rule's gates, then
    /// [`Phase1::analyze_rule`].
    fn ct1(&mut self, key: ConfiguredTargetKey) -> Result<(), String> {
        let rule = match self.target(&key.label)? {
            Target::Rule(rule) | Target::Output(rule, _) => rule,
            Target::Source(_) => {
                return Err(format!(
                    "internal error: CT1 node of the source file {}",
                    key.label
                ));
            }
        };
        self.rule_gates(&rule)?;
        // The rule as loaded, for a delegation (whose plan entry is the
        // owner's).
        self.acc.rule = Some(Arc::clone(&rule));
        self.analyze_rule(rule, key, None)
    }

    fn finish_ct1(mut self, result: Result<(), String>) -> Result<NodeValue, Missing> {
        if let Some(missing) = self.missing.take() {
            return Err(missing);
        }
        let outcome = match result {
            Err(error) => {
                self.events.push(Event::Error(error.clone()));
                self.cx.emit(Event::Error(error.clone()));
                Err(error)
            }
            Ok(()) => {
                let acc = std::mem::take(&mut self.acc);
                match (acc.node, acc.rule) {
                    (Some(node), Some(rule)) => Ok(Ct1Ok {
                        node,
                        rule,
                        delegate: acc.delegate,
                        toolchain_context: acc.toolchain_context,
                        exec_groups: acc.exec_groups,
                        exec_config: acc.exec_config,
                        group_exec_configs: acc.group_exec_configs,
                        edge_configs: acc.edge_configs,
                        deferred_duplicates: acc.deferred_duplicates,
                        empty_configuration: acc.empty_configuration,
                        aspect_attribute_configs: BTreeMap::new(),
                        conditions: Vec::new(),
                    }),
                    _ => Err("internal error: a CT1 body published no configured node".to_owned()),
                }
            }
        };
        Ok(NodeValue::Ct1(Arc::new(Ct1Value {
            outcome,
            transcript: Transcript {
                events: std::mem::take(&mut self.events),
            },
        })))
    }

    fn finish<T>(self, result: T) -> Result<T, Missing> {
        match self.missing {
            Some(missing) => Err(missing),
            None => Ok(result),
        }
    }
}

impl Host for EvalHost<'_, '_> {
    fn repositories(&self) -> &Arc<RepositorySet> {
        &self.shared.repositories
    }

    fn configuration(&self, config: ConfigId) -> Arc<Configuration> {
        Arc::clone(self.shared.configurations.lock().unwrap().get(config))
    }

    fn top_configuration(&self) -> Arc<Configuration> {
        Arc::clone(
            self.shared
                .configurations
                .lock()
                .unwrap()
                .top_configuration(),
        )
    }

    fn configuration_mnemonic(&self, config: ConfigId) -> String {
        self.shared
            .configurations
            .lock()
            .unwrap()
            .mnemonic(config)
            .to_owned()
    }

    fn intern_exec(
        &mut self,
        from: ConfigId,
        platform: Label,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        self.shared
            .configurations
            .lock()
            .unwrap()
            .intern_exec(from, platform, target_scoped)
    }

    fn intern_derived(
        &mut self,
        configuration: Configuration,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        self.shared
            .configurations
            .lock()
            .unwrap()
            .intern_derived(configuration, target_scoped)
    }

    fn intern_no_config(&mut self) -> ConfigId {
        self.shared
            .configurations
            .lock()
            .unwrap()
            .intern_no_config()
    }

    fn cquery(&self) -> bool {
        self.shared.options.cquery
    }

    fn cquery_capture(&mut self) -> Option<&mut cquery::Capture> {
        None
    }

    fn run_under(&self) -> Option<&RunUnderEdge> {
        self.shared.options.run_under.as_ref()
    }

    fn requested_trimmed_root(&self) -> Option<ConfiguredTargetKey> {
        self.cx.coord().first.requested_trimmed_root.clone()
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        if self.cx.coord().tracking {
            self.emit(Event::Access(label.clone()));
        }
        let key = NodeKey::Tgt(label.clone());
        match self.need(key.clone()) {
            Ok(value) => match value.as_ref() {
                NodeValue::Tgt(result) => result.clone(),
                _ => Err("internal error: a Tgt key with another value".to_owned()),
            },
            Err(missing) => Err(self.stop(missing)),
        }
    }

    fn load_package(&mut self, package: &PackageId) -> Result<(), String> {
        self.cx.coord().engine.load_package(package)
    }

    fn source_visibility(&mut self, label: &Label) -> Visibility {
        self.cx.coord().engine.source_visibility(label)
    }

    fn resolve_group(&mut self, label: &Label) -> Result<Arc<GroupNode>, String> {
        match self.need(NodeKey::Group(label.clone())) {
            Ok(value) => match value.as_ref() {
                NodeValue::Group(result) => result.clone(),
                _ => Err("internal error: a Group key with another value".to_owned()),
            },
            Err(Missing::BackEdge(_)) => Err(format!("package group include cycle at {label}")),
            Err(missing) => Err(self.stop(missing)),
        }
    }

    fn package_args(&mut self, package: &PackageId) -> crate::package::PackageArgs {
        self.cx.coord().engine.packages[package].args.clone()
    }

    fn implicit_tests(&mut self, suite: &Rule) -> Vec<Label> {
        self.cx.coord().engine.implicit_tests(suite)
    }

    fn expand_registration_patterns(
        &mut self,
        patterns: &[(String, String)],
        kind: Registration,
    ) -> Result<Vec<Label>, ResolutionError> {
        self.cx
            .coord()
            .engine
            .expand_registration_patterns(patterns, kind)
    }

    fn warm_toolchain_patterns(&mut self, patterns: &[(String, String)]) {
        self.shared.repositories.warm_registrations(patterns);
    }

    fn module_execution_platforms(
        &mut self,
        resolve: bool,
    ) -> Result<Vec<(String, String)>, String> {
        self.cx.coord().engine.module_execution_platforms(resolve)
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.cx.coord().engine.attribute_aspects(rule, identities)
    }

    fn loaded_attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Vec<Arc<crate::aspects::AspectDefinition>> {
        self.cx
            .coord()
            .engine
            .loaded_attribute_aspects(rule, identities)
    }

    fn analyze(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        // A source file analyzed before (Engine::analyzed_sources): no
        // lookup.
        let source = NodeKey::Src(label.clone());
        if self.cx.coord().source_analyzed(label) {
            if self.cx.coord().tracking {
                self.emit(Event::Source(label.clone()));
            }
            return Ok(());
        }
        let rule = match self.target(label)? {
            Target::Source(_) => {
                if self.cx.coord().tracking {
                    self.emit(Event::Source(label.clone()));
                }
                return match self.need(source) {
                    Ok(value) => match value.as_ref() {
                        NodeValue::Src(result) => result.clone(),
                        _ => Err("internal error: a Src key with another value".to_owned()),
                    },
                    Err(missing) => Err(self.stop(missing)),
                };
            }
            Target::Rule(rule) => rule,
            Target::Output(rule, _) => {
                check_macro_namespace(&rule, label)?;
                rule
            }
        };
        let key = ConfiguredTargetKey::new(rule.label.clone(), config);
        self.need_ct1(&rule, key, true)
    }

    fn analyze_key(
        &mut self,
        label: &Label,
        config: ConfigId,
        apply_rule_transition: bool,
    ) -> Result<(), String> {
        let rule = match self.target(label)? {
            Target::Rule(rule) | Target::Output(rule, _) => rule,
            Target::Source(_) => return self.analyze(label, config),
        };
        let key = ConfiguredTargetKey {
            label: rule.label.clone(),
            config,
            apply_rule_transition,
        };
        self.need_ct1(&rule, key, true)
    }

    fn analyze_detached(&mut self, label: &Label, config: ConfigId) -> Result<(), String> {
        Host::analyze(self, label, config)
    }

    fn configured_node(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredNode> {
        if self.own_key() == Some(key) {
            return self.acc.node.clone();
        }
        self.ct1_ok(key, |ok| ok.node.clone())
    }

    fn configured_rule(&mut self, key: &ConfiguredTargetKey) -> Option<Arc<Rule>> {
        if self.own_key() == Some(key) {
            return self.acc.rule.clone().filter(|_| self.acc.planned);
        }
        // `configured_rules`: custom rules and incompatible targets, never
        // a delegating key.
        self.ct1_ok(key, |ok| {
            ((ok.rule.custom.is_some() || ok.node.incompatible.is_some()) && ok.delegate.is_none())
                .then(|| Arc::clone(&ok.rule))
        })
        .flatten()
    }

    fn rule_delegation(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredTargetKey> {
        if self.own_key() == Some(key) {
            return self.acc.delegate.clone();
        }
        self.ct1_ok(key, |ok| ok.delegate.clone()).flatten()
    }

    fn toolchain_context_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<UnloadedToolchainContext> {
        if self.own_key() == Some(key) {
            return self.acc.toolchain_context.clone();
        }
        self.ct1_ok(key, |ok| ok.toolchain_context.clone())
            .flatten()
    }

    fn exec_groups_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::exec_groups::TargetExecGroups> {
        if self.own_key() == Some(key) {
            return self.acc.exec_groups.clone();
        }
        self.ct1_ok(key, |ok| ok.exec_groups.clone()).flatten()
    }

    fn exec_config_of(&mut self, key: &ConfiguredTargetKey) -> Option<ConfigId> {
        if self.own_key() == Some(key) {
            return self.acc.exec_config;
        }
        self.ct1_ok(key, |ok| ok.exec_config).flatten()
    }

    fn group_exec_config_of(&mut self, key: &ConfiguredTargetKey, group: &str) -> Option<ConfigId> {
        if self.own_key() == Some(key) {
            return self.acc.group_exec_configs.get(group).copied();
        }
        self.ct1_ok(key, |ok| ok.group_exec_configs.get(group).copied())
            .flatten()
    }

    fn edge_configs_of(
        &mut self,
        key: &ConfiguredTargetKey,
        attribute: &str,
    ) -> Option<TransitionConfigurations> {
        if self.own_key() == Some(key) {
            return self
                .acc
                .edge_configs
                .as_ref()
                .and_then(|configs| configs.get(attribute).cloned());
        }
        self.ct1_ok(key, |ok| {
            ok.edge_configs
                .as_ref()
                .and_then(|configs| configs.get(attribute).cloned())
        })
        .flatten()
    }

    fn source_analyzed(&mut self, _label: &Label) -> bool {
        false
    }

    fn record_source_analyzed(&mut self, _label: &Label) {}

    fn configured_contains(&mut self, _key: &ConfiguredTargetKey) -> bool {
        false
    }

    fn record_keep_going_edge(&mut self, _key: &ConfiguredTargetKey) {}

    fn active_push(&mut self, _key: &ConfiguredTargetKey) {}

    fn active_pop(&mut self) {}

    fn active_position(&mut self, _key: &ConfiguredTargetKey) -> Option<usize> {
        None
    }

    fn active_len(&mut self) -> usize {
        0
    }

    fn active_keys(&mut self) -> Vec<ConfiguredTargetKey> {
        self.cx.coord().active_ct1()
    }

    fn ahead(&self) -> bool {
        false
    }

    fn ahead_abort(&mut self, what: &str) -> String {
        format!("internal error: ahead-of-turn {what} in the evaluator")
    }

    fn track_key(&mut self, _key: &ConfiguredTargetKey) -> Option<u32> {
        None
    }

    fn track_enter(&mut self, _tracked: Option<u32>) {}

    fn track_entry(&mut self, _tracked: Option<u32>, _plan_len: usize) {}

    fn track_position(&mut self, _tracked: Option<u32>, _plan_len: usize) {}

    fn track_source(&mut self, label: &Label) {
        if self.cx.coord().tracking {
            self.emit(Event::Source(label.clone()));
        }
    }

    fn prefetch_rule(&mut self, _rule: &Rule) {}

    fn prefetch_note_condition(
        &mut self,
        _text: &str,
        _matching: &crate::configured_attributes::ConfigMatching,
    ) {
    }

    fn prefetch_platforms(&mut self, _target: &Arc<PlatformInfo>, _host: &Arc<PlatformInfo>) {}

    fn prefetch_resolved(&mut self, _resolved: &UnloadedToolchainContext) {}

    fn prefetch_compatible_toolchain(
        &mut self,
        _declared: &crate::toolchains::DeclaredToolchainInfo,
    ) {
    }

    fn plan_len(&mut self) -> usize {
        0
    }

    fn plan_push(&mut self, key: ConfiguredTargetKey, rule: Arc<Rule>) {
        if let Some(own) = self.own.clone()
            && own.key == key
        {
            self.acc.rule = Some(rule);
            self.acc.planned = true;
            self.emit(Event::PlanPush(own));
        }
    }

    fn publish_node(&mut self, _key: &ConfiguredTargetKey, node: ConfiguredNode) {
        self.acc.node = Some(node);
    }

    fn record_delegation(&mut self, _key: ConfiguredTargetKey, owner: ConfiguredTargetKey) {
        self.acc.delegate = Some(owner);
    }

    fn record_configured_rule(&mut self, _key: &ConfiguredTargetKey, rule: &Arc<Rule>) {
        self.acc.rule = Some(Arc::clone(rule));
    }

    fn record_empty_configuration(&mut self, _key: &ConfiguredTargetKey) {
        self.acc.empty_configuration = true;
    }

    fn record_exec_config(&mut self, _key: &ConfiguredTargetKey, config: ConfigId) {
        self.acc.exec_config = Some(config);
    }

    fn record_group_exec_config(
        &mut self,
        _key: &ConfiguredTargetKey,
        group: &str,
        config: ConfigId,
    ) {
        self.acc.group_exec_configs.insert(group.to_owned(), config);
    }

    fn record_edge_configs(
        &mut self,
        _key: &ConfiguredTargetKey,
        configs: BTreeMap<String, TransitionConfigurations>,
    ) {
        self.acc.edge_configs = Some(configs);
    }

    fn record_deferred_duplicates(&mut self, _key: &ConfiguredTargetKey, errors: Vec<String>) {
        self.acc.deferred_duplicates = Some(errors);
    }

    fn record_toolchain_context(
        &mut self,
        _key: &ConfiguredTargetKey,
        context: UnloadedToolchainContext,
    ) {
        self.acc.toolchain_context = Some(context);
    }

    fn record_exec_groups(
        &mut self,
        _key: &ConfiguredTargetKey,
        groups: crate::exec_groups::TargetExecGroups,
    ) {
        self.acc.exec_groups = Some(groups);
    }

    fn warn_once(&mut self, warning: String) {
        self.emit(Event::Warn(warning));
    }

    fn warn(&mut self, warning: String) {
        self.emit(Event::Warn(warning));
    }

    fn print(&mut self, line: &str) {
        self.emit(Event::Print(line.to_owned()));
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
        if self.shared.options.cquery {
            self.emit(Event::CqueryEdge {
                from: from.clone(),
                to: label,
                config,
                attribute: attribute.to_owned(),
                tool,
                aspect,
            });
        }
    }

    fn capture_cquery_aspect_toolchain_context(
        &mut self,
        _config: ConfigId,
        _context: &ToolchainContextKey,
    ) -> Vec<u32> {
        Vec::new()
    }

    fn flush_pending(&mut self, _config_setting: &Rule) -> Result<(), String> {
        self.emit(Event::Flush);
        Ok(())
    }

    fn planned_feature_flag(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::feature_flags::FeatureFlagValue> {
        // S1: the walk's planned providers when it ran first (S2: the
        // CsMatch node needs the flag's CT2).
        self.cx.coord().engine.planned_feature_flag(key)
    }

    fn platform_tree(&self) -> bool {
        self.platform_tree
    }

    fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let key = NodeKey::Platform(ConfiguredTargetKey::new(label.clone(), config));
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::Platform(result) => result.clone(),
                _ => Err(PlatformError::Other(
                    "internal error: a Platform key with another value".to_owned(),
                )),
            },
            Err(Missing::BackEdge(_)) => Err(PlatformError::Other(format!(
                "platform {} is needed while it is being resolved; this is not implemented in the native profile",
                crate::rule_declarations::canonical_form(label)
            ))),
            Err(missing) => Err(PlatformError::Other(self.stop(missing))),
        }
    }

    fn registered_toolchains(
        &mut self,
        config: ConfigId,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        let key = NodeKey::RegToolchains(config);
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::RegToolchains(result) => result.clone(),
                _ => Err(ResolutionError::Propagated(
                    "internal error: a RegToolchains key with another value".to_owned(),
                )),
            },
            Err(Missing::BackEdge(_)) => Err(ResolutionError::Propagated(
                "internal error: registered toolchains needed while they are computed".to_owned(),
            )),
            Err(missing) => Err(ResolutionError::Propagated(self.stop(missing))),
        }
    }

    fn registered_toolchains_in_progress(&mut self, config: ConfigId) -> bool {
        self.cx.coord().in_progress(&NodeKey::RegToolchains(config))
    }

    fn set_computing_toolchain(&mut self, _label: &Label) {}

    fn record_toolchain_cycle_report(&mut self, config: ConfigId, report: String) {
        self.cx
            .coord()
            .first
            .toolchain_cycle_reports
            .insert(config, report);
    }

    fn take_toolchain_cycle_report(&mut self, config: ConfigId) -> Option<String> {
        self.cx
            .coord()
            .first
            .toolchain_cycle_reports
            .remove(&config)
    }

    fn registered_execution_platforms(
        &mut self,
        config: ConfigId,
        _requester: &Label,
    ) -> Result<ExecutionPlatforms, ResolutionError> {
        let key = NodeKey::RegExecPlatforms(config);
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::RegExecPlatforms(result) => result.clone(),
                _ => Err(ResolutionError::Propagated(
                    "internal error: a RegExecPlatforms key with another value".to_owned(),
                )),
            },
            Err(Missing::BackEdge(_)) => Err(ResolutionError::Propagated(
                "internal error: execution platforms needed while they are computed".to_owned(),
            )),
            Err(missing) => Err(ResolutionError::Propagated(self.stop(missing))),
        }
    }

    fn execution_platforms_in_progress(&mut self, config: ConfigId) -> bool {
        self.cx
            .coord()
            .in_progress(&NodeKey::RegExecPlatforms(config))
    }

    fn record_execution_platform_cycle_report(&mut self, config: ConfigId, report: String) {
        self.cx
            .coord()
            .first
            .execution_platform_cycle_reports
            .insert(config, report);
    }

    fn take_execution_platform_cycle_report(&mut self, config: ConfigId) -> Option<String> {
        self.cx
            .coord()
            .first
            .execution_platform_cycle_reports
            .remove(&config)
    }

    fn exec_transition(&mut self, config: ConfigId, platform: &Label) -> Result<ConfigId, String> {
        let key = NodeKey::ExecCfg(config, platform.clone());
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::ExecCfg(result) => result.clone(),
                _ => Err("internal error: an ExecCfg key with another value".to_owned()),
            },
            Err(missing) => Err(self.stop(missing)),
        }
    }

    fn flag_scope(&mut self, label: &Label) -> Result<bool, String> {
        let key = NodeKey::FlagScope(label.clone());
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::FlagScope(result) => result.clone(),
                _ => Err("internal error: a FlagScope key with another value".to_owned()),
            },
            Err(missing) => Err(self.stop(missing)),
        }
    }

    fn transition_details_for(
        &mut self,
        settings: Vec<Label>,
    ) -> Result<TransitionDetails, String> {
        let key = NodeKey::TransitionDetails(settings);
        self.memo_use(&key);
        match self.need(key) {
            Ok(value) => match value.as_ref() {
                NodeValue::TransitionDetails(result) => result.clone(),
                _ => Err("internal error: a TransitionDetails key with another value".to_owned()),
            },
            Err(missing) => Err(self.stop(missing)),
        }
    }

    fn transition_cache_get(
        &mut self,
        key: &TransitionCacheKey,
    ) -> Option<(TransitionConfigurations, Vec<String>)> {
        self.cx
            .coord()
            .first
            .transition_cache
            .get(key)
            .map(|(id, printed)| (Arc::clone(id), printed.clone()))
    }

    fn transition_cache_put(
        &mut self,
        key: TransitionCacheKey,
        value: (TransitionConfigurations, Vec<String>),
    ) {
        self.cx.coord().first.transition_cache.insert(key, value);
    }

    fn claim_toolchain_implementation(
        &mut self,
        key: ConfiguredTargetKey,
        exec_platform: &Label,
    ) -> Option<Label> {
        self.emit(Event::ToolchainImpl {
            implementation: key.clone(),
            exec_platform: exec_platform.clone(),
        });
        let claims = &mut self.cx.coord().first.claims;
        match claims.get(&key) {
            Some(existing) => Some(existing.clone()),
            None => {
                claims.insert(key, exec_platform.clone());
                None
            }
        }
    }

    fn forced_exec(&mut self, key: &ConfiguredTargetKey) -> Option<Label> {
        match &self.own {
            Some(own) if own.key == *key => own.forced_exec.clone(),
            _ => self.cx.coord().first.claims.get(key).cloned(),
        }
    }

    fn resolving_aspect_toolchains(&self) -> bool {
        self.resolving_aspect_toolchains
    }

    fn set_resolving_aspect_toolchains(&mut self, resolving: bool) {
        self.resolving_aspect_toolchains = resolving;
    }

    fn aspect_predicate(&mut self, memo: &(Label, String)) -> Option<bool> {
        self.cx.coord().first.aspect_predicates.get(memo).copied()
    }

    fn set_aspect_predicate(&mut self, memo: (Label, String), value: bool) {
        self.cx.coord().first.aspect_predicates.insert(memo, value);
    }

    fn aspect_attr_aspects(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<(Option<Arc<[String]>>, Option<Arc<[String]>>)> {
        self.cx.coord().first.aspect_attr_aspects.get(memo).cloned()
    }

    fn set_aspect_attr_aspects(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: (Option<Arc<[String]>>, Option<Arc<[String]>>),
    ) {
        self.cx
            .coord()
            .first
            .aspect_attr_aspects
            .insert(memo, value);
    }

    fn aspect_toolchains(&mut self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>> {
        self.cx.coord().first.aspect_toolchains.get(memo).cloned()
    }

    fn set_aspect_toolchains(&mut self, memo: (ConfigId, String), value: Arc<AspectToolchains>) {
        self.cx.coord().first.aspect_toolchains.insert(memo, value);
    }

    fn aspect_attribute_configs(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<Arc<BTreeMap<String, TransitionConfigurations>>> {
        self.cx
            .coord()
            .first
            .aspect_attribute_configs
            .get(memo)
            .cloned()
    }

    fn set_aspect_attribute_configs(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: Arc<BTreeMap<String, TransitionConfigurations>>,
    ) {
        self.cx
            .coord()
            .first
            .aspect_attribute_configs
            .insert(memo, value);
    }
}

/// The CtKey of an edge to `rule`'s `key` from a context inside a
/// platform's tree or not: the first variant already analyzed (the walk's
/// first-visit memo), else this context's.
pub(crate) fn edge_ct_key(
    first: &mut FirstVisit,
    no_config: bool,
    rule: &Rule,
    key: ConfiguredTargetKey,
    platform_tree: bool,
) -> CtKey {
    if let Some(found) = first.canonical.get(&key) {
        return found.clone();
    }
    let forced_exec = first.claims.get(&key).cloned();
    // platform_tree is kept for every rule of a tree: the context
    // propagates to the rule's dependencies (CtKey::normalized's per-rule
    // rule would let a filegroup's genrule dependency escape the empty
    // configuration).
    let ct = CtKey {
        key: key.clone(),
        platform_tree,
        forced_exec,
    }
    .normalized(Observes {
        no_config,
        toolchain_resolution: uses_toolchain_resolution(rule),
        exec_edges: true,
    });
    first.canonical.insert(key, ct.clone());
    ct
}

/// The phase-1 node bodies: the value of `key` (S1: the coordinator's
/// loading nodes run on the engine).
pub(crate) fn body(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    key: &NodeKey,
) -> Result<NodeValue, Missing> {
    match key {
        NodeKey::Tgt(label) => Ok(NodeValue::Tgt(cx.coord().engine.target(label))),
        NodeKey::Group(label) => Ok(NodeValue::Group(
            cx.coord().engine.resolve_group(label, &mut Vec::new()),
        )),
        NodeKey::Src(label) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let visibility = host.source_visibility(label);
            let result = host
                .visible(&visibility, &label.package_id(), None)
                .map(|_| ());
            let result = host.finish(result)?;
            Ok(NodeValue::Src(result))
        }
        NodeKey::Ct1(ct) => {
            let mut host = EvalHost::new(shared, cx, Some(ct.clone()), ct.platform_tree);
            let result = host.ct1(ct.key.clone());
            host.finish_ct1(result)
        }
        NodeKey::Platform(platform) => {
            let mut host = EvalHost::new(shared, cx, None, true);
            let result = host.compute_platform_info(&platform.label, platform.config);
            Ok(NodeValue::Platform(host.finish(result)?))
        }
        NodeKey::RegToolchains(config) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let result = host.compute_registered_toolchains(*config);
            Ok(NodeValue::RegToolchains(host.finish(result)?))
        }
        NodeKey::RegExecPlatforms(config) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let result = host.compute_execution_platforms(*config);
            Ok(NodeValue::RegExecPlatforms(host.finish(result)?))
        }
        NodeKey::ExecCfg(config, platform) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let result = host.compute_exec_transition(*config, platform);
            Ok(NodeValue::ExecCfg(host.finish(result)?))
        }
        NodeKey::FlagScope(label) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let result = host.compute_flag_scope(label);
            Ok(NodeValue::FlagScope(host.finish(result)?))
        }
        NodeKey::TransitionDetails(settings) => {
            let mut host = EvalHost::new(shared, cx, None, false);
            let result = host.compute_transition_details(settings);
            Ok(NodeValue::TransitionDetails(host.finish(result)?))
        }
        NodeKey::CsMatch(ct) => {
            // S1: the matching is the config_setting's CT1 result (the
            // Starlark flag_values split needing CT2 is S2).
            let value = cx.need_one(NodeKey::Ct1(ct.clone()))?;
            Ok(NodeValue::CsMatch(match value.as_ref() {
                NodeValue::Ct1(value) => match &value.outcome {
                    Ok(ok) => ok
                        .node
                        .matching
                        .clone()
                        .ok_or_else(|| format!("{} is not a config_setting", ct.key.label)),
                    Err(error) => Err(error.clone()),
                },
                _ => Err("internal error: a CT1 key with another value".to_owned()),
            }))
        }
        other => Ok(NodeValue::Status(Err(format!(
            "internal error: {} nodes are not phase-1 nodes (stage S1)",
            other.kind()
        )))),
    }
}

/// Whether an edge to `rule` observes an empty configuration (kept for
/// reference by S2's normalization: genrules, Starlark test rules and
/// exec-transition edges fail there).
#[allow(dead_code)]
pub(crate) fn exec_edges(rule: &Rule, edges: &[super::super::super::Edge]) -> bool {
    let license_kinds = rule
        .custom
        .as_ref()
        .is_some_and(|custom| custom.schema.contains_key("license_kinds"));
    matches!(rule.kind, RuleKind::Genrule)
        || rule
            .custom
            .as_ref()
            .is_some_and(|custom| custom.declaration.builtin.kind.test && !license_kinds)
        || (!license_kinds
            && edges
                .iter()
                .any(|edge| matches!(edge.cfg, EdgeCfg::Exec(_))))
}
