//! The evaluator's shared read view (docs/EVALUATOR.md "Data"): what node
//! bodies read of the engine, `Sync`, never written but through interior
//! concurrent caches. Everything an analysis writes is a node value, a
//! transcript event, or linearizer-only state (the classification table
//! in docs/EVALUATOR.md).
//!
//! Owner: worker B (fills it from the Engine; the field list is the
//! frozen contract). Worker A only passes `&EvalShared` to bodies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::config_registry::Configurations;
use crate::label::Label;
use crate::repository::RepositorySet;

use super::super::delta::{InputGroups, InputMemos};
use super::super::{
    ClientEnvironment, ConcurrentArtifactIndex, OutputSelection, RunUnderEdge, TargetIdentityKey,
};

/// The immutable request options node bodies read (today plain Engine
/// fields set by the CLI before planning).
pub(crate) struct EvalOptions {
    /// `--loading_phase_threads` (the pool size when no `--jobs`-like
    /// analysis knob is given).
    pub(crate) loading_phase_threads: usize,
    pub(crate) keep_going: bool,
    /// cquery/aquery: the capture flags (`cquery_capture.is_some()`,
    /// `cquery_files`, `cquery_build`, `cquery_transitions`,
    /// `cquery_fragments`, `aquery_capture.is_some()`).
    pub(crate) cquery: bool,
    pub(crate) cquery_files: bool,
    pub(crate) cquery_build: bool,
    pub(crate) cquery_transitions: bool,
    pub(crate) cquery_fragments: bool,
    pub(crate) aquery: bool,
    pub(crate) compact_repo_mapping: bool,
    pub(crate) run_under: Option<RunUnderEdge>,
    pub(crate) output_selection: OutputSelection,
    /// `--aspects` identities and `--aspects_parameters`.
    pub(crate) requested_aspects: Vec<String>,
    pub(crate) aspect_parameters: BTreeMap<String, String>,
    pub(crate) tests_requested: bool,
    pub(crate) execute_build: bool,
    pub(crate) skip_incompatible_explicit_targets: bool,
    pub(crate) explicit_targets: Option<BTreeSet<Label>>,
    pub(crate) workspace_status: crate::workspace_status::WorkspaceStatusOptions,
    /// The run requests of `run`/`test` (`run_requests`, `run_trees`).
    pub(crate) run_requests: BTreeSet<Label>,
    pub(crate) run_trees: bool,
}

/// The shared read view of one evaluation.
///
/// In it (read-only for bodies):
/// - `Engine::repositories` (the repository set: module graph, `.bzl`
///   module cache — a shared single-flight cache already — repository
///   specs; repository *work* is coordinator-only, `Missing::Repo`),
/// - the request options ([`EvalOptions`]),
/// - `Engine::configurations` (interning is the one write: a concurrent
///   intern; content-addressed ids make it order-independent; the output
///   directory mnemonics are checked deterministic by EVAL_VERIFY),
/// - `Engine::aspects` (the AspectRegistry, filled by `.bzl` loads:
///   concurrent append, keyed by identity),
/// - `Engine::loaded_functions` (append-only registry, by `.bzl` label),
/// - the client environment (read once, OnceLock),
/// - the shared single-flight caches: tool runfiles groups
///   (`input_groups`), depset walk inputs (`input_memos`), the artifact
///   index (`ConcurrentArtifactIndex`; its stamps become a debug check),
///   the shared `Target` identities.
///
/// Not in it: everything classified "node value", "transcript" or
/// "linearizer-only" in docs/EVALUATOR.md.
pub(crate) struct EvalShared {
    pub(crate) repositories: Arc<RepositorySet>,
    pub(crate) options: EvalOptions,
    /// S0: today's registry behind a lock; S1 (worker B) replaces it with
    /// a sharded concurrent intern with the same API.
    pub(crate) configurations: Mutex<Configurations>,
    pub(crate) aspects: Mutex<crate::aspects::AspectRegistry>,
    pub(crate) loaded_functions: Arc<crate::analysis_runtime::LoadedFunctions>,
    pub(crate) client_environment: Arc<ClientEnvironment>,
    pub(crate) input_groups: Arc<InputGroups>,
    pub(crate) input_memos: Arc<InputMemos>,
    pub(crate) artifacts: Arc<ConcurrentArtifactIndex>,
    /// The one shared `Target` value per identity key (N55): a
    /// single-flight cache (today `ActionPlanning::target_identities`).
    pub(crate) target_identities:
        super::super::delta::SingleFlight<TargetIdentityKey, crate::starlark_types::Target>,
    /// The `Transition` nodes' inputs (S2, worker C; written once per key
    /// by the first requester).
    pub(crate) transitions: TransitionInputs,
}

/// The inputs of the `Transition` nodes, by cache key: the key names the
/// definition by id; the node reads it here.
pub(crate) type TransitionInputs = Mutex<
    crate::fast_hash::FxHashMap<
        super::super::TransitionCacheKey,
        Arc<super::nodes::host::TransitionInput>,
    >,
>;

/// Bodies run on pool threads with `&EvalShared` (compile-time check).
#[allow(clippy::extra_unused_type_parameters)]
const fn assert_sync<T: Sync>() {}
const _: () = assert_sync::<EvalShared>();

impl EvalShared {
    /// The shared view of `engine`'s request (S1: built per evaluation).
    /// The configurations move into the view (the one write bodies make,
    /// an intern) and back with [`Self::restore`]; the engine's coordinator
    /// services (package loading, target lookups, aspect definitions) never
    /// read them.
    pub(crate) fn from_engine(engine: &mut super::super::Engine) -> Self {
        let options = EvalOptions {
            loading_phase_threads: engine.loading_phase_threads,
            keep_going: engine.keep_going,
            cquery: engine.cquery_capture.is_some(),
            cquery_files: engine.cquery_files,
            cquery_build: engine.cquery_build,
            cquery_transitions: engine.cquery_transitions,
            cquery_fragments: engine.cquery_fragments,
            aquery: engine.aquery_capture.is_some(),
            compact_repo_mapping: engine.compact_repo_mapping,
            run_under: engine.run_under.clone(),
            output_selection: engine.output_selection.clone(),
            requested_aspects: engine.requested_aspects.clone(),
            aspect_parameters: engine.aspect_parameters.clone(),
            tests_requested: engine.tests_requested,
            execute_build: engine.execute_build,
            skip_incompatible_explicit_targets: engine.skip_incompatible_explicit_targets,
            explicit_targets: engine.explicit_targets.clone(),
            workspace_status: engine.workspace_status.clone(),
            run_requests: engine.run_requests.clone(),
            run_trees: engine.run_trees,
        };
        Self {
            repositories: Arc::clone(&engine.repositories),
            options,
            configurations: Mutex::new(std::mem::take(&mut engine.configurations)),
            // S1: aspect definitions are a coordinator service (the
            // engine's registry, filled by `.bzl` loads).
            aspects: Mutex::new(crate::aspects::AspectRegistry::default()),
            loaded_functions: Arc::clone(&engine.loaded_functions),
            client_environment: Arc::default(),
            input_groups: Arc::default(),
            input_memos: Arc::default(),
            artifacts: Arc::default(),
            target_identities: Default::default(),
            transitions: Default::default(),
        }
    }

    /// Give the configurations (with what the evaluation interned) back.
    pub(crate) fn restore(self, engine: &mut super::super::Engine) {
        engine.configurations = self
            .configurations
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
