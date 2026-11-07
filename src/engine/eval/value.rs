//! Node values (docs/EVALUATOR.md "Values"): what a node function
//! returns, wrapping today's types. A value is immutable once its node is
//! done (`Arc`-shared with every reader); errors are values too (`Result`),
//! so a failed node is done like any other and `--keep_going` revisits
//! replay its transcript.
//!
//! Owner: worker B (the node functions). Frozen in S0: the variants and
//! the `Ct1Value`/`Ct2Value` fields are the contract with A (storage) and
//! C (the linearizer reads the transcripts and takes the deltas).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::configuration::ConfigId;
use crate::package::{Package, Rule};
use crate::platforms::PlatformInfo;
use crate::toolchains::{RegisteredToolchains, UnloadedToolchainContext};

use super::super::delta::TargetDelta;
use super::super::{
    AspectToolchains, ConfiguredNode, ConfiguredTargetKey, ExecutionPlatforms, PlatformError,
    ResolutionError, Target, TransitionConfigurations, TransitionDetails,
};
use super::transcript::Transcript;

/// A node's value.
pub(crate) enum NodeValue {
    /// A loaded package (its load prints are a `PkgCommit` effect of the
    /// first transcript that reaches it).
    Pkg(Result<Arc<Package>, String>),
    /// A target lookup.
    Tgt(Result<Target, String>),
    /// A source file's analysis (visibility checked).
    Src(Result<(), String>),
    /// A resolved package group.
    Group(Result<Arc<crate::package_specification::GroupNode>, String>),
    Ct1(Arc<Ct1Value>),
    /// The ConfigMatchingProvider of a config_setting in a configuration.
    CsMatch(Result<crate::configured_attributes::ConfigMatching, String>),
    Platform(Result<Arc<PlatformInfo>, PlatformError>),
    RegToolchains(Result<Arc<RegisteredToolchains>, ResolutionError>),
    RegExecPlatforms(Result<ExecutionPlatforms, ResolutionError>),
    ModuleExecPlatforms(Result<Arc<Vec<(String, String)>>, String>),
    ExecCfg(Result<ConfigId, String>),
    /// An applied transition: the configurations and the print() lines
    /// every application replays (StarlarkTransitionCache). Failures are
    /// not memoized today: a failed application's node is re-run per
    /// demander (its value is not shared; docs/EVALUATOR.md).
    Transition(Result<TransitionValue, String>),
    TransitionDetails(Result<TransitionDetails, String>),
    FlagScope(Result<bool, String>),
    /// The predicate's result with the print() lines of its one
    /// evaluation (printed once, at the first linearized use).
    AspectPred(Result<(bool, Arc<[String]>), String>),
    AspectAttrAspects(Result<AspectAttrAspects, String>),
    AspectToolchains(Result<Arc<AspectToolchains>, String>),
    Status(Result<Arc<crate::workspace_status::ComputedWorkspaceStatus>, String>),
    Ct2(Arc<Ct2Value>),
    View(Result<crate::analysis::DependencyTarget, String>),
    Aspect(Arc<AspectValue>),
}

/// An applied Starlark transition.
#[derive(Clone, Debug)]
pub(crate) struct TransitionValue {
    pub(crate) configurations: TransitionConfigurations,
    pub(crate) printed: Arc<[String]>,
}

/// An aspect's function `attr_aspects` and `toolchains_aspects` lists.
pub(crate) type AspectAttrAspects = (Option<Arc<[String]>>, Option<Arc<[String]>>);

/// Phase 1 of a configured target: today's writes of `analyze_key_body`
/// to the engine, as one value (docs/EVALUATOR.md, the classification).
pub(crate) struct Ct1Value {
    /// The analysis's outcome: its configured node, or its error text.
    pub(crate) outcome: Result<Ct1Ok, String>,
    /// What the analysis did, in order (pass 1 replays it).
    pub(crate) transcript: Transcript,
}

/// A configured target's phase-1 results.
pub(crate) struct Ct1Ok {
    /// `configured[key]` (ConfiguredNode).
    pub(crate) node: ConfiguredNode,
    /// The select()-configured rule (`plan.push((key, rule))`;
    /// `configured_rules` and the cquery capture for custom rules).
    pub(crate) rule: Arc<Rule>,
    /// A rule transition delegated the key to `owner`
    /// (`rule_delegations`): `node` is the owner's.
    pub(crate) delegate: Option<ConfiguredTargetKey>,
    /// `toolchain_contexts[key]`.
    pub(crate) toolchain_context: Option<UnloadedToolchainContext>,
    /// `exec_group_contexts[key]` (present when the rule resolves
    /// toolchains).
    pub(crate) exec_groups: Option<crate::exec_groups::TargetExecGroups>,
    /// `exec_configurations[key]` (only with default-exec-group edges).
    pub(crate) exec_config: Option<ConfigId>,
    /// `group_exec_configurations[(key, group)]`.
    pub(crate) group_exec_configs: BTreeMap<String, ConfigId>,
    /// `edge_configs[key]` (Starlark-transition attributes).
    pub(crate) edge_configs: Option<BTreeMap<String, TransitionConfigurations>>,
    /// `deferred_duplicates[key]` (custom rules).
    pub(crate) deferred_duplicates: Option<Vec<String>>,
    /// `empty_configuration_rules` membership (a platform's tree or the
    /// no-config configuration).
    pub(crate) empty_configuration: bool,
    /// The aspect applications its aspect edges analyzed
    /// (`aspect_attribute_configs` entries), keyed by aspect identity.
    pub(crate) aspect_attribute_configs:
        BTreeMap<String, Arc<BTreeMap<String, TransitionConfigurations>>>,
    /// The select() conditions with their matching, in order (the cquery
    /// capture of an incompatible target).
    pub(crate) conditions: Vec<(String, crate::configured_attributes::ConfigMatching)>,
}

/// Phase 2 of a configured target: what its consumers read (today the
/// planning state's per-key maps) and the delta the linearizer merges.
pub(crate) struct Ct2Value {
    pub(crate) reads: Ct2Reads,
    /// The commit as data, taken once by the linearizer (pass 2).
    pub(crate) delta: Mutex<Option<TargetDelta>>,
    /// The commit's error (after its delta's operations).
    pub(crate) error: Option<String>,
}

/// What a consumer's phase 2 reads of a dependency's phase 2 (today
/// `ViewReads` over the planning state).
#[derive(Default)]
pub(crate) struct Ct2Reads {
    pub(crate) dry: Option<Arc<Vec<super::super::Artifact>>>,
    pub(crate) providers: Option<BTreeMap<String, starlark::values::OwnedFrozenValue>>,
    pub(crate) provider_keys: Option<Vec<String>>,
    pub(crate) runfiles_info: Option<Arc<crate::runfiles::TargetRunfiles>>,
    pub(crate) validation: Option<crate::starlark_types::Depset>,
    pub(crate) native_output_group: Option<starlark::values::OwnedFrozenValue>,
    /// The repositories of the packages it transitively depends on.
    pub(crate) repositories: Option<Arc<std::collections::BTreeSet<String>>>,
}

/// One aspect application's result.
pub(crate) struct AspectValue {
    pub(crate) providers: Result<BTreeMap<String, starlark::values::OwnedFrozenValue>, String>,
    pub(crate) delta: Mutex<Option<TargetDelta>>,
    pub(crate) transcript: Transcript,
}

impl NodeValue {
    /// The value's error text, if it is a failure (keep-going bookkeeping
    /// and the first-error frontier).
    pub(crate) fn error(&self) -> Option<String> {
        fn text<T, E: std::fmt::Debug>(result: &Result<T, E>) -> Option<String> {
            result.as_ref().err().map(|error| format!("{error:?}"))
        }
        match self {
            NodeValue::Pkg(r) => r.as_ref().err().cloned(),
            NodeValue::Tgt(r) => r.as_ref().err().cloned(),
            NodeValue::Src(r) => r.as_ref().err().cloned(),
            NodeValue::Group(r) => r.as_ref().err().cloned(),
            NodeValue::Ct1(value) => value.outcome.as_ref().err().cloned(),
            NodeValue::CsMatch(r) => r.as_ref().err().cloned(),
            NodeValue::Platform(r) => text(r),
            NodeValue::RegToolchains(r) => text(r),
            NodeValue::RegExecPlatforms(r) => text(r),
            NodeValue::ModuleExecPlatforms(r) => r.as_ref().err().cloned(),
            NodeValue::ExecCfg(r) => r.as_ref().err().cloned(),
            NodeValue::Transition(r) => r.as_ref().err().cloned(),
            NodeValue::TransitionDetails(r) => r.as_ref().err().cloned(),
            NodeValue::FlagScope(r) => r.as_ref().err().cloned(),
            NodeValue::AspectPred(r) => r.as_ref().err().cloned(),
            NodeValue::AspectAttrAspects(r) => r.as_ref().err().cloned(),
            NodeValue::AspectToolchains(r) => r.as_ref().err().cloned(),
            NodeValue::Status(r) => r.as_ref().err().cloned(),
            NodeValue::Ct2(value) => value.error.clone(),
            NodeValue::View(r) => r.as_ref().err().cloned(),
            NodeValue::Aspect(value) => value.providers.as_ref().err().cloned(),
        }
    }
}

/// Values are shared across the pool's threads (compile-time check).
#[allow(clippy::extra_unused_type_parameters)]
const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<NodeValue>();
const _: () = assert_send_sync::<Ct1Value>();
const _: () = assert_send_sync::<Ct2Value>();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_values() {
        let value = NodeValue::Src(Err("visibility".into()));
        assert_eq!(value.error().as_deref(), Some("visibility"));
        assert_eq!(NodeValue::FlagScope(Ok(true)).error(), None);
        let ct1 = NodeValue::Ct1(Arc::new(Ct1Value {
            outcome: Err("no such target".into()),
            transcript: Transcript::default(),
        }));
        assert_eq!(ct1.error().as_deref(), Some("no such target"));
    }
}
