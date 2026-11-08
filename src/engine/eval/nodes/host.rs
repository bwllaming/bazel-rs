//! What a phase-1 body ([`super::phase1::Phase1`]) may read and do besides
//! computing: the primitives the sequential walk (`super::walk::Walk`, the
//! engine and its phase-1 maps: today's behaviour) and the evaluator
//! (`super::eval_host::EvalHost`: node needs, node values, transcript
//! events) implement differently. docs/EVALUATOR.md §7 classifies every
//! write behind them.
//!
//! Owner: worker B.

use std::collections::BTreeSet;
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
    AspectToolchains, ConfiguredNode, ConfiguredTargetKey, ExecutionPlatforms, PlatformError,
    Registration, ResolutionError, RunUnderEdge, Target, TransitionCacheKey,
    TransitionConfigurations, TransitionDetails, cquery,
};

/// What one Starlark transition application reads (the `Transition`
/// node's input, registered under its cache key by the first requester:
/// every requester of one key gives an equal input).
pub(crate) struct TransitionInput {
    pub(crate) def: crate::starlark_transitions::TransitionDef,
    pub(crate) kind: crate::starlark_transitions::Kind,
    pub(crate) target: Label,
    pub(crate) from: ConfigId,
    pub(crate) attributes: crate::starlark_transitions::AttributeValues,
}

/// The primitives of a phase-1 body.
pub(crate) trait Host {
    // ---- shared reads (EvalShared) -------------------------------------

    fn repositories(&self) -> &Arc<RepositorySet>;
    /// `configurations.get(config)`.
    fn configuration(&self, config: ConfigId) -> Arc<Configuration>;
    fn top_configuration(&self) -> Arc<Configuration>;
    /// The output path mnemonic of an interned configuration.
    fn configuration_mnemonic(&self, config: ConfigId) -> String;
    fn intern_exec(
        &mut self,
        from: ConfigId,
        platform: Label,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String>;
    fn intern_derived(
        &mut self,
        configuration: Configuration,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String>;
    fn intern_no_config(&mut self) -> ConfigId;
    /// A cquery/aquery capture is on.
    fn cquery(&self) -> bool;
    /// The walk's capture (the evaluator does not support cquery in S1).
    fn cquery_capture(&mut self) -> Option<&mut cquery::Capture>;
    fn run_under(&self) -> Option<&RunUnderEdge>;
    /// The requested top-level rule being analyzed when it is test trimmed.
    fn requested_trimmed_root(&self) -> Option<ConfiguredTargetKey>;

    // ---- loading (coordinator services in S1; Pkg/Tgt/Group nodes) -----

    fn target(&mut self, label: &Label) -> Result<Target, String>;
    fn load_package(&mut self, package: &PackageId) -> Result<(), String>;
    fn source_visibility(&mut self, label: &Label) -> Visibility;
    fn resolve_group(&mut self, label: &Label) -> Result<Arc<GroupNode>, String>;
    /// The package's `package()` arguments (deprecation, testonly).
    fn package_args(&mut self, package: &PackageId) -> crate::package::PackageArgs;
    fn implicit_tests(&mut self, suite: &Rule) -> Vec<Label>;
    fn expand_registration_patterns(
        &mut self,
        patterns: &[(String, String)],
        kind: Registration,
    ) -> Result<Vec<Label>, ResolutionError>;
    fn warm_toolchain_patterns(&mut self, patterns: &[(String, String)]);
    fn module_execution_platforms(
        &mut self,
        resolve: bool,
    ) -> Result<Vec<(String, String)>, String>;
    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String>;
    fn loaded_attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Vec<Arc<crate::aspects::AspectDefinition>>;

    // ---- dependency edges ------------------------------------------------

    /// Analyze `label` in `config` as a dependency edge (the walk's
    /// `analyze`; the evaluator's `Src`/`Ct1` need and `Reach`).
    fn analyze(&mut self, label: &Label, config: ConfigId) -> Result<(), String>;
    /// [`Self::analyze`] with an explicit `shouldApplyRuleTransition` (a
    /// rule transition's delegate).
    fn analyze_key(
        &mut self,
        label: &Label,
        config: ConfigId,
        apply_rule_transition: bool,
    ) -> Result<(), String>;
    /// [`Self::analyze`] from a fresh active chain (registration and
    /// platform analyses).
    fn analyze_detached(&mut self, label: &Label, config: ConfigId) -> Result<(), String>;
    /// The configured node of an analyzed rule target (`configured[key]`).
    fn configured_node(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredNode>;
    /// The select()-configured custom rule of an analyzed key.
    fn configured_rule(&mut self, key: &ConfiguredTargetKey) -> Option<Arc<Rule>>;
    fn rule_delegation(&mut self, key: &ConfiguredTargetKey) -> Option<ConfiguredTargetKey>;
    fn toolchain_context_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<UnloadedToolchainContext>;
    fn exec_groups_of(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::exec_groups::TargetExecGroups>;
    fn exec_config_of(&mut self, key: &ConfiguredTargetKey) -> Option<ConfigId>;
    fn group_exec_config_of(&mut self, key: &ConfiguredTargetKey, group: &str) -> Option<ConfigId>;
    fn edge_configs_of(
        &mut self,
        key: &ConfiguredTargetKey,
        attribute: &str,
    ) -> Option<TransitionConfigurations>;

    // ---- the walk's per-key bookkeeping (evaluator: no-ops/events) -----

    fn source_analyzed(&mut self, label: &Label) -> bool;
    fn record_source_analyzed(&mut self, label: &Label);
    fn configured_contains(&mut self, key: &ConfiguredTargetKey) -> bool;
    fn record_keep_going_edge(&mut self, key: &ConfiguredTargetKey);
    fn active_push(&mut self, key: &ConfiguredTargetKey);
    fn active_pop(&mut self);
    fn active_position(&mut self, key: &ConfiguredTargetKey) -> Option<usize>;
    fn active_len(&mut self) -> usize;
    /// The active chain since the last detached analysis (cycle reports).
    fn active_keys(&mut self) -> Vec<ConfiguredTargetKey>;
    fn ahead(&self) -> bool;
    fn ahead_abort(&mut self, what: &str) -> String;
    fn track_key(&mut self, key: &ConfiguredTargetKey) -> Option<u32>;
    fn track_enter(&mut self, tracked: Option<u32>);
    fn track_entry(&mut self, tracked: Option<u32>, plan_len: usize);
    fn track_position(&mut self, tracked: Option<u32>, plan_len: usize);
    fn track_source(&mut self, label: &Label);
    fn prefetch_rule(&mut self, rule: &Rule);
    /// The dependencies a rule's edge loop analyzes next, in order (the
    /// evaluator's pool schedules them; S2).
    fn hint_edges(&mut self, deps: &[(Label, ConfigId)]) {
        let _ = deps;
    }
    fn prefetch_note_condition(
        &mut self,
        text: &str,
        matching: &crate::configured_attributes::ConfigMatching,
    );
    fn prefetch_platforms(&mut self, target: &Arc<PlatformInfo>, host: &Arc<PlatformInfo>);
    fn prefetch_resolved(&mut self, resolved: &UnloadedToolchainContext);
    fn prefetch_compatible_toolchain(
        &mut self,
        declared: &crate::toolchains::DeclaredToolchainInfo,
    );

    // ---- the analyzed key's results (the evaluator's Ct1Ok) -------------

    fn plan_len(&mut self) -> usize;
    fn plan_push(&mut self, key: ConfiguredTargetKey, rule: Arc<Rule>);
    fn publish_node(&mut self, key: &ConfiguredTargetKey, node: ConfiguredNode);
    fn record_delegation(&mut self, key: ConfiguredTargetKey, owner: ConfiguredTargetKey);
    fn record_configured_rule(&mut self, key: &ConfiguredTargetKey, rule: &Arc<Rule>);
    fn record_empty_configuration(&mut self, key: &ConfiguredTargetKey);
    fn record_exec_config(&mut self, key: &ConfiguredTargetKey, config: ConfigId);
    fn record_group_exec_config(
        &mut self,
        key: &ConfiguredTargetKey,
        group: &str,
        config: ConfigId,
    );
    fn record_edge_configs(
        &mut self,
        key: &ConfiguredTargetKey,
        configs: std::collections::BTreeMap<String, TransitionConfigurations>,
    );
    fn record_deferred_duplicates(&mut self, key: &ConfiguredTargetKey, errors: Vec<String>);
    fn record_toolchain_context(
        &mut self,
        key: &ConfiguredTargetKey,
        context: UnloadedToolchainContext,
    );
    fn record_exec_groups(
        &mut self,
        key: &ConfiguredTargetKey,
        groups: crate::exec_groups::TargetExecGroups,
    );

    // ---- order-observable effects (evaluator: transcript events) --------

    /// A warning, reported once (EmittedEventState.addWarning).
    fn warn_once(&mut self, warning: String);
    /// A warning, always reported.
    fn warn(&mut self, warning: String);
    /// An analysis print() line.
    fn print(&mut self, line: &str);
    fn capture_cquery_edge(
        &mut self,
        from: &ConfiguredTargetKey,
        label: Label,
        config: ConfigId,
        attribute: &str,
        tool: bool,
        aspect: bool,
    );
    fn capture_cquery_aspect_toolchain_context(
        &mut self,
        config: ConfigId,
        context: &ToolchainContextKey,
    ) -> Vec<u32>;
    /// A config_setting with Starlark flag_values keys needs the CT2 of
    /// what is planned so far (`plan_pending`; the evaluator's `Flush`).
    fn flush_pending(&mut self, config_setting: &Rule) -> Result<(), String>;
    /// The FeatureFlagInfo a planned Starlark target returned.
    fn planned_feature_flag(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::feature_flags::FeatureFlagValue>;

    // ---- memo nodes ------------------------------------------------------

    /// Inside a platform's tree (the empty configuration).
    fn platform_tree(&self) -> bool;
    /// PlatformValue (`Platform` node).
    fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<Arc<PlatformInfo>, PlatformError>;
    /// RegisteredToolchainsValue (`RegToolchains` node).
    fn registered_toolchains(
        &mut self,
        config: ConfigId,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError>;
    /// Whether `config`'s registered toolchains are being computed (a
    /// registered target needs them: a cycle).
    fn registered_toolchains_in_progress(&mut self, config: ConfigId) -> bool;
    fn set_computing_toolchain(&mut self, label: &Label);
    fn record_toolchain_cycle_report(&mut self, config: ConfigId, report: String);
    fn take_toolchain_cycle_report(&mut self, config: ConfigId) -> Option<String>;
    /// RegisteredExecutionPlatformsValue (`RegExecPlatforms` node).
    fn registered_execution_platforms(
        &mut self,
        config: ConfigId,
        requester: &Label,
    ) -> Result<ExecutionPlatforms, ResolutionError>;
    fn execution_platforms_in_progress(&mut self, config: ConfigId) -> bool;
    fn record_execution_platform_cycle_report(&mut self, config: ConfigId, report: String);
    fn take_execution_platform_cycle_report(&mut self, config: ConfigId) -> Option<String>;
    /// The exec configuration of (parent, execution platform) (`ExecCfg`
    /// node); the error is unlocated.
    fn exec_transition(&mut self, config: ConfigId, platform: &Label) -> Result<ConfigId, String>;
    /// Whether a Starlark flag is target-scoped (`FlagScope` node).
    fn flag_scope(&mut self, label: &Label) -> Result<bool, String>;
    /// StarlarkBuildSettingsDetailsValue (`TransitionDetails` node).
    fn transition_details_for(&mut self, settings: Vec<Label>)
    -> Result<TransitionDetails, String>;
    /// The application keyed `key` as a `Transition` node (the evaluator;
    /// `input` registers it): its configurations when it succeeded — its
    /// effects (the details' and flags' memos, the print() lines) are the
    /// node's, replayed at its first linearized use, the lines again at
    /// every later use (`Err`: the attempt stops). `None`: apply here (the walk; a failure, which the
    /// walk does not memoize either).
    #[allow(clippy::type_complexity)]
    fn transition_node(
        &mut self,
        key: &TransitionCacheKey,
        input: &dyn Fn() -> TransitionInput,
    ) -> Option<Result<TransitionConfigurations, crate::starlark_transitions::Failure>> {
        let _ = (key, input);
        None
    }
    /// The evaluator's registration cycle report (`toolchains`: the
    /// toolchains', else the execution platforms'), its chain built
    /// bottom-up from here (linearize::chains); `None`: the walk words it
    /// from its active chain.
    fn open_cycle_report(&mut self, toolchains: bool, tail: &str) -> Option<String> {
        let _ = (toolchains, tail);
        None
    }
    /// The computation of a first-visit memo (an aspect memo whose getter
    /// returned `None`) failed: nothing is memoized (the evaluator closes
    /// the memo's scope).
    fn memo_failed(&mut self) {}
    /// StarlarkTransitionCache.
    fn transition_cache_get(
        &mut self,
        key: &TransitionCacheKey,
    ) -> Option<(TransitionConfigurations, Vec<String>)>;
    fn transition_cache_put(
        &mut self,
        key: TransitionCacheKey,
        value: (TransitionConfigurations, Vec<String>),
    );
    /// The first claim of a toolchain implementation's execution platform
    /// (`toolchain_implementations`): the existing claim, if any (the
    /// evaluator's `ToolchainImpl`).
    fn claim_toolchain_implementation(
        &mut self,
        key: ConfiguredTargetKey,
        exec_platform: &Label,
        location: Option<&str>,
    ) -> Option<Label>;
    /// The execution platform a toolchain implementation's own resolution
    /// is forced to (`CtKey.forced_exec`).
    fn forced_exec(&mut self, key: &ConfiguredTargetKey) -> Option<Label>;
    fn resolving_aspect_toolchains(&self) -> bool;
    fn set_resolving_aspect_toolchains(&mut self, resolving: bool);
    fn aspect_predicate(&mut self, memo: &(Label, String)) -> Option<bool>;
    fn set_aspect_predicate(&mut self, memo: (Label, String), value: bool);
    #[allow(clippy::type_complexity)]
    fn aspect_attr_aspects(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<(Option<Arc<[String]>>, Option<Arc<[String]>>)>;
    #[allow(clippy::type_complexity)]
    fn set_aspect_attr_aspects(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: (Option<Arc<[String]>>, Option<Arc<[String]>>),
    );
    fn aspect_toolchains(&mut self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>>;
    fn set_aspect_toolchains(&mut self, memo: (ConfigId, String), value: Arc<AspectToolchains>);
    fn aspect_attribute_configs(
        &mut self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<Arc<std::collections::BTreeMap<String, TransitionConfigurations>>>;
    fn set_aspect_attribute_configs(
        &mut self,
        memo: (ConfiguredTargetKey, String),
        value: Arc<std::collections::BTreeMap<String, TransitionConfigurations>>,
    );
}
