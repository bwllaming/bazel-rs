//! A deliberately small native analysis/execution pipeline, not Skyframe.
use crate::action::{self, Action};
use crate::analysis::{self, DependencyTarget};
use crate::config_registry::Configurations;
pub use crate::config_registry::ConfiguredTargetKey;
use crate::configuration::{ConfigId, Configuration, LabelParser, SettingValue};
use crate::configured_attributes::{self, ConfigMatching};
use crate::ignored::IgnoredSubdirectories;
use crate::label::{Label, PackageId};
pub use crate::output_group_info::OutputSelection;
use crate::package::{self, Package, Rule, RuleKind};
use crate::package_specification::{self, GroupNode};
use crate::platforms::{ConstraintSettingInfo, ConstraintValueInfo, PlatformInfo};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;
use crate::request::PlannedRequest;
use crate::scheduler::{self, Dependency, PlannedAction};
use crate::shared_path::SharedPath;
use crate::starlark_options::{BuildSettingInfo, LoadedTarget, SettingType};
use crate::starlark_types::{File, FileData};
use crate::toolchains::{
    DeclaredToolchainInfo, RegisteredToolchains, ToolchainContextKey, ToolchainTypeInfo,
    ToolchainTypeRequirement, UnloadedToolchainContext,
};
use crate::visibility::Visibility;
use crate::workspace;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

mod alloc_report;
pub mod cquery;
mod keep_going;
pub mod query;
mod query_attrs;
mod split;

/// A planned artifact. Its data is shared by its copies (MEM4): the
/// planner holds one artifact in many places — a target's files to build
/// (`dry`), the declared map, the artifact index, the dependency views, the
/// runfiles mappings of every executable and the top-level request — and a
/// copy is one reference count.
#[derive(Clone)]
struct Artifact(Arc<ArtifactData>);

impl std::ops::Deref for Artifact {
    type Target = ArtifactData;

    fn deref(&self) -> &ArtifactData {
        &self.0
    }
}

// The derived format of the artifact's fields (`Artifact { real: .. }`).
impl std::fmt::Debug for Artifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let data = &*self.0;
        f.debug_struct("Artifact")
            .field("real", &data.real)
            .field("exec", &data.exec)
            .field("producer", &data.producer)
            .field("owner", &data.owner)
            .field("directory", &data.directory)
            .field("constant_metadata", &data.constant_metadata)
            .field("file", &data.file)
            .finish()
    }
}

/// The fields of an [`Artifact`].
#[derive(Clone)]
struct ArtifactData {
    real: SharedPath,
    exec: SharedPath,
    producer: Option<(String, usize)>,
    owner: String,
    /// A tree artifact (declared directory).
    directory: bool,
    /// A constant-metadata artifact (Artifact.isConstantMetadata): its
    /// digest is excluded from consumer action keys — the volatile
    /// workspace status file and the volatile build-info files
    /// (N63, docs/WORKSPACE_STATUS.md).
    constant_metadata: bool,
    /// The artifact's File, computed once by [`Artifact::new`] (W1a,
    /// PLAN-L1 §2.1) from `exec`, the producer's presence, `owner` and
    /// `directory` — clones share it; a workspace status artifact's is the
    /// status File itself ([`Artifact::workspace_status`]). It is fixed at
    /// construction: a staged tool runfile (commit_custom_target) rewrites
    /// only `exec` to its runfiles-tree destination and keeps the staged
    /// artifact's File; plan_action compares such copies on their fields.
    file: File,
}

impl Artifact {
    /// An already-validated analysis File without an execution producer. Reuse
    /// its identity instead of computing and discarding a second File value.
    fn semantic(file: &File) -> Self {
        let exec = SharedPath::from(file.path.as_str());
        Self(Arc::new(ArtifactData {
            real: exec.clone(),
            exec,
            producer: None,
            owner: file.owner.clone(),
            directory: file.is_directory,
            constant_metadata: false,
            file: file.clone(),
        }))
    }

    fn new(
        real: PathBuf,
        exec: PathBuf,
        producer: Option<(String, usize)>,
        owner: String,
        directory: bool,
        constant_metadata: bool,
    ) -> Self {
        let file = Self::compute_file(&exec, producer.is_some(), &owner, directory);
        Self(Arc::new(ArtifactData {
            real: real.into(),
            exec: exec.into(),
            producer,
            owner,
            directory,
            constant_metadata,
            file,
        }))
    }

    /// A workspace status artifact (BazelWorkspaceStatusModule.java:280-281):
    /// `bazel-out/<name>` in the build-data root, owned by no target, planned
    /// (`producer`: the status node of an executing build) or bare (the
    /// --nobuild artifact no action produces). Its File is the status File
    /// ctx.info_file / ctx.version_file hold
    /// (crate::analysis_runtime::workspace_status_file: short_path `<name>`,
    /// a derived File, root `bazel-out`) in both cases, so the File a
    /// consumer reaches through a dependency's files (DefaultInfo,
    /// ctx.files, Target.files — the view hands out Artifact Files) equals
    /// its own ctx.info_file, as in Bazel. [`Artifact::compute_file`] cannot
    /// give it: the path lies below no configuration's output root (it
    /// would be the short path too) and a bare artifact has no producer (it
    /// would be a source).
    fn workspace_status(
        name: &str,
        producer: Option<(String, usize)>,
        constant_metadata: bool,
    ) -> Self {
        // W0-A work counter: one File per Artifact, as Artifact::new.
        crate::plan_timings::note_artifact_file();
        let file = crate::analysis_runtime::workspace_status_file(name);
        let exec = SharedPath::from(file.path.as_str());
        Self(Arc::new(ArtifactData {
            real: exec.clone(),
            exec,
            producer,
            owner: file.owner.clone(),
            directory: file.is_directory,
            constant_metadata,
            file,
        }))
    }

    /// The File of an artifact at `exec` (the algorithm `file()` ran on
    /// every call before W1a).
    fn compute_file(exec_path: &Path, generated: bool, owner: &str, directory: bool) -> File {
        // W0-A work counter: one File computed (a source owner's label
        // parsing dominates it; since W1a once per Artifact, `file()` only
        // clones the cache).
        crate::plan_timings::note_artifact_file();
        let exec = exec_path.to_string_lossy();
        // External short paths are metadata, never filesystem lookup paths.
        let short_path = if generated {
            // A generated file is relative to its configuration's output
            // root, and its short path follows the one rule of every
            // generated File, whoever owns it (the declared File of a
            // rule's output is the same File; review F2 of d2fd11c).
            let relative =
                crate::label::split_generated_root(&exec).map_or(&*exec, |(_, relative)| relative);
            crate::label::generated_short_path(relative)
        } else if Label::parse_canonical(owner).is_ok_and(|label| !label.repository.is_empty()) {
            exec_path
                .strip_prefix("external")
                .map(|path| PathBuf::from("..").join(path))
                .unwrap_or_else(|_| exec_path.to_path_buf())
                .to_string_lossy()
                .into_owned()
        } else {
            exec.clone().into_owned()
        };
        File::new(FileData {
            is_directory: directory,
            is_symlink: false,
            path: exec.into_owned(),
            short_path,
            owner: owner.to_owned(),
            is_source: !generated,
        })
    }

    fn file(&self) -> File {
        self.file.clone()
    }

    /// This artifact's data, to change (copied first when shared).
    fn make_mut(&mut self) -> &mut ArtifactData {
        Arc::make_mut(&mut self.0)
    }

    /// The artifact as an unresolved symlink artifact
    /// (ctx.actions.declare_symlink) when `symlink`: its File is the
    /// declared symlink File.
    fn with_symlink(mut self, symlink: bool) -> Self {
        if symlink {
            let file = File::new(FileData {
                is_symlink: true,
                ..self.file.clone().into_data()
            });
            self.make_mut().file = file;
        }
        self
    }
}

/// A configured target's artifacts (its files to build), shared: the
/// planned `dry` lists, their delegation copies and the dependency views
/// hold one list (W1a).
type Artifacts = Arc<Vec<Artifact>>;
type Locations = BTreeMap<String, Vec<PathBuf>>;
/// Declared generated artifacts by configuration and execution path.
type Declared = BTreeMap<(ConfigId, PathBuf), Artifact>;
type ResolvedInputs = (Vec<Artifact>, Locations);

/// The identity key of a configured target's one shared `Target` value
/// (N55): the label and the configuration Bazel keys the configured target
/// by — `None` is the null configuration of a source file or package group
/// (one InputFileConfiguredTarget/PackageGroupConfiguredTarget per label
/// serves every consumer, InputFileConfiguredTarget.java:36-41,
/// PackageGroup.java:166-168), `Some` the post-delegation
/// configuration of a rule, alias or output-file edge.
type TargetIdentityKey = (Label, Option<ConfigId>);

/// The one shared `Target` value of each aspect-configured target (N58):
/// keyed by the base configured target's label, its configuration and the
/// applied aspects' identities.
type AspectTargetIdentities =
    BTreeMap<(Label, Option<ConfigId>, Vec<String>), std::sync::Arc<crate::starlark_types::Target>>;

#[derive(Clone)]
enum Target {
    Rule(Arc<Rule>),
    Output(Arc<Rule>, String),
    Source(PathBuf),
}

/// Phase-2 state of a build (action planning, plan order): dry
/// artifacts, the Starlark providers of analyzed targets and the planned
/// actions. `done` counts the planned entries of the build's plan.
#[derive(Default)]
struct ActionPlanning {
    dry: BTreeMap<ConfiguredTargetKey, Artifacts>,
    custom_providers:
        BTreeMap<ConfiguredTargetKey, BTreeMap<String, starlark::values::OwnedFrozenValue>>,
    /// Returned declared-provider names per analyzed rule (Target debugPrint).
    provider_keys: BTreeMap<ConfiguredTargetKey, Vec<String>>,
    /// RunfilesProvider/FilesToRunProvider of analyzed rule targets
    /// (docs/RUNFILES.md); file targets and other native rules have none.
    runfiles_info: BTreeMap<ConfiguredTargetKey, Arc<crate::runfiles::TargetRunfiles>>,
    /// The build's one File -> Artifact resolver (W1b, PLAN-L1 §2.1): the
    /// role of Bazel's Artifact identity (actions/Artifact.java:677-702),
    /// so an action input, a DefaultInfo file or a runfile resolves to its
    /// producer however the File reached the consumer — a provider field, a
    /// closure, a held Target — as in Bazel, which never enumerates
    /// provider files. Every File is registered once, before any consumer
    /// can see it ([`register_artifact`]): a source when its view is first
    /// built or a native rule takes it as an input, a generated file at its
    /// producer's commit. The workspace status files (owned by no target)
    /// are registered by [`Engine::ensure_workspace_status`] on first need:
    /// an action input or DefaultInfo file of a committing target, or any
    /// other lookup that reaches one ([`Engine::indexed_artifact`]: a
    /// runfiles tree, an output group, an aspect's outputs) — so a status
    /// File never fails to resolve, whichever targets the build has. One
    /// producer per output path (register_output) and one label per source
    /// path make it a function of the File.
    artifact_index: BTreeMap<File, Artifact>,
    declared: Declared,
    /// Output path -> its producer (register_output): the owning
    /// action-lookup key and, for a Starlark action, the action's id and
    /// owner-independent key (crate::shared_actions).
    producers: Producers,
    nodes: Vec<PlannedAction>,
    /// Every source input of every action, before execroot planting drops
    /// main-repository inputs under ignored top-level entries.
    source_inputs: BTreeMap<String, SourceInputs>,
    /// The shared input group of each tool runfiles tree an action of this
    /// build used, by the tree's identity (PA-1,
    /// crate::analysis_runtime::ToolRunfilesTree::identity), with the tree
    /// it was expanded from: expanded once, held by every action that uses
    /// the tool. Holding the tree keeps it the one the analysis memo hands
    /// out for the rest of planning (computed once per build) and lets a
    /// hit check, in debug builds, that the identity's content is the one
    /// expanded. In-process only: identities are never persisted.
    input_groups: HashMap<u64, ExpandedTree>,
    /// Actions whose tool runfiles trees are listed in the action instead
    /// (the groups could not stay shared, crate::scheduler::groups_apart):
    /// reported by PERF-GROUPS.
    listed_tree_actions: usize,
    /// Requested keys delegating to the key a rule transition analyzed them
    /// under (Engine::rule_delegations), waiting for that key to be planned
    /// (flush_delegations).
    delegations: Vec<(ConfiguredTargetKey, ConfiguredTargetKey)>,
    /// The framework OutputGroupInfo of native rule targets (custom rules
    /// carry theirs in custom_providers), built once per configured target
    /// (the end of an alias chain) so every dependent sees one instance.
    native_output_groups: BTreeMap<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
    /// The `_validation` set RuleConfiguredTargetBuilder propagated to each
    /// native rule target from its attribute prerequisites: one node
    /// composed of theirs (crate::output_group_info::compose_validation,
    /// W1c), present only when non-empty
    /// (analysis::transitive_validation is the Starlark-rule path; custom
    /// rules' groups live in their providers).
    validation: BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
    done: usize,
    /// The client environment inherited action variables resolve against,
    /// read once per build on first need (crate::action_env).
    client_environment: std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    /// The client environment this build's workspace status computed
    /// against (N63 follow-up 2): captured by ensure_workspace_status so
    /// a test can inject a per-build environment deterministically.
    status_client_environment: std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    /// The repositories of the packages each configured target transitively
    /// depends on, itself included (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest,
    /// N23f): per configured target key, since select() can choose
    /// different dependencies in different configurations.
    repositories: BTreeMap<ConfiguredTargetKey, Arc<BTreeSet<String>>>,
    /// The runfiles support nodes of each configured target with runfiles
    /// support (N23f, Engine::plan_runfiles_support).
    runfiles_nodes: BTreeMap<ConfiguredTargetKey, RunfilesNodes>,
    /// The providers each aspect application returned (N58), keyed per
    /// aspect like Bazel's AspectKey (crate::aspects).
    aspect_providers: BTreeMap<
        crate::aspects::AspectApplicationKey,
        BTreeMap<String, starlark::values::OwnedFrozenValue>,
    >,
    /// The one shared `Target` value of each aspect-configured target (N58,
    /// N55): the base configured target with a set of aspects merged in
    /// (MergedConfiguredTarget), one per (label, configuration, aspects).
    aspect_target_identities: BTreeMap<
        (Label, Option<ConfigId>, Vec<String>),
        std::sync::Arc<crate::starlark_types::Target>,
    >,
    /// The workspace status of this build (N63): the computed
    /// BazelWorkspaceStatusAction outputs and the planned system node's
    /// artifacts. Computed exactly once per executing invocation
    /// (std::sync::OnceLock in Engine::workspace_status_once) and planned
    /// once per build that executes — Bazel adds both status artifacts to
    /// artifactsToBuild unconditionally (BuildView.createResult
    /// :647-650,675), so the status action runs on every build/test/run
    /// that reaches execution, whether or not any target consumes it.
    workspace_status: Option<WorkspaceStatusPlan>,
    /// The one shared `Target` value of each dependency edge's configured
    /// target (N55): allocated once and referenced by every consumer's
    /// `ctx.attr`, every provider that stores it and every depset snapshot
    /// — Bazel's one ConfiguredTarget instance per label and configuration
    /// (skyframe/ConfiguredTargetKey.java:60; AbstractConfiguredTarget
    /// identity equality), keyed as Bazel keys it: a rule or alias edge by
    /// its label and POST-DELEGATION configuration (two edges whose
    /// pre-transition configurations converge through the same Starlark
    /// rule transition delegate to one key,
    /// analysis/producers/TargetAndConfigurationProducer.java:244-251),
    /// a source file or package group by its label and NO configuration
    /// (`isConfigurable()` false, InputFile.java:88-90,
    /// PackageGroup.java:166-168: one InputFileConfiguredTarget or
    /// PackageGroupConfiguredTarget per label serves every consumer), an
    /// output file by the generating rule's configuration. An alias edge
    /// and an exec-configured rule edge are their own keys, hence their
    /// own instances.
    target_identities: BTreeMap<TargetIdentityKey, std::sync::Arc<crate::starlark_types::Target>>,
    /// The memoized dependency view of each dependency edge key (W1a,
    /// PLAN-L1 §2.1; Engine::plan_dependency_view).
    views: DependencyViews,
}

/// The dependency views of a build, keyed like the edge that requested
/// them: the requested label in the edge's configuration.
type DependencyViews = HashMap<ConfiguredTargetKey, DependencyTarget>;

/// BAZEL_RS_VERIFY_VIEWS: the memoized view of `key` equals the view this
/// edge rebuilt (what the per-edge code computed before W1a): its files,
/// kind, providers (ids and values), rule class, executable and the shared
/// Target identity (pointer). A provider value is one instance per
/// configured target (the committed providers, the native OutputGroupInfo
/// memo), so the rebuilt view holds the very same value (pointer); the
/// framework providers a view build synthesizes afresh — an output file's
/// `_validation` OutputGroupInfo, a coverage configuration's
/// InstrumentedFilesInfo — compare by repr. Rebuilding re-registers the
/// view's artifacts, which checks that their index entries are unchanged
/// ([`register_artifact`] under BAZEL_RS_VERIFY_ARTIFACTS or in debug
/// builds).
fn verify_view(
    key: &ConfiguredTargetKey,
    memoized: &DependencyTarget,
    rebuilt: Result<DependencyTarget, String>,
) {
    let stale = |what: &str, memoized: &dyn std::fmt::Debug, rebuilt: &dyn std::fmt::Debug| {
        stale_view(key, what, memoized, rebuilt)
    };
    let rebuilt = match rebuilt {
        Ok(rebuilt) => rebuilt,
        Err(error) => stale_view(key, "the outcome", &"Ok", &error),
    };
    let (memo, now) = (&**memoized, &*rebuilt);
    if memo.label != now.label {
        stale("label", &memo.label, &now.label);
    }
    if memo.files != now.files {
        stale("files", &memo.files, &now.files);
    }
    let kinds = |view: &analysis::DependencyView| {
        (
            view.is_source,
            view.is_rule,
            view.is_package_group,
            view.is_filegroup,
        )
    };
    if kinds(memo) != kinds(now) {
        stale("target kind", &kinds(memo), &kinds(now));
    }
    let provider_ids =
        |view: &analysis::DependencyView| view.providers.keys().cloned().collect::<Vec<_>>();
    if provider_ids(memo) != provider_ids(now) {
        stale("providers", &provider_ids(memo), &provider_ids(now));
    }
    for (id, value) in &memo.providers {
        let (value, other) = (value.value(), now.providers[id].value());
        let synthesized = matches!(
            id.as_str(),
            crate::output_group_info::PROVIDER_ID | crate::instrumented_files::PROVIDER_ID
        );
        if !value.ptr_eq(other) && (!synthesized || value.to_repr() != other.to_repr()) {
            stale(
                &format!("provider {id}"),
                &value.to_repr(),
                &other.to_repr(),
            );
        }
    }
    if memo.provider_keys != now.provider_keys {
        stale("provider keys", &memo.provider_keys, &now.provider_keys);
    }
    if memo.rule_class != now.rule_class {
        stale("rule class", &memo.rule_class, &now.rule_class);
    }
    if !Arc::ptr_eq(&memo.identity, &now.identity) {
        stale(
            "Target identity",
            &Arc::as_ptr(&memo.identity),
            &Arc::as_ptr(&now.identity),
        );
    }
    let runfiles = |view: &analysis::DependencyView| {
        (
            view.runfiles.files(),
            view.runfiles
                .files_to_run
                .as_ref()
                .map(|files_to_run| files_to_run.executable().cloned()),
        )
    };
    if runfiles(memo) != runfiles(now) {
        stale("runfiles", &runfiles(memo), &runfiles(now));
    }
}

/// The BAZEL_RS_VERIFY_VIEWS failure: a memoized dependency view differs
/// from the one its edge rebuilt.
fn stale_view(
    key: &ConfiguredTargetKey,
    what: &str,
    memoized: &dyn std::fmt::Debug,
    rebuilt: &dyn std::fmt::Debug,
) -> ! {
    panic!(
        "BAZEL_RS_VERIFY_VIEWS: the memoized dependency view of {} in {} is stale: {what} differs\n  memoized: {memoized:?}\n  rebuilt:  {rebuilt:?}",
        key.label, key.config
    )
}

/// Whether two artifacts are the same in every field.
fn same_artifact(a: &Artifact, b: &Artifact) -> bool {
    a.real == b.real
        && a.exec == b.exec
        && a.producer == b.producer
        && a.owner == b.owner
        && a.directory == b.directory
        && a.constant_metadata == b.constant_metadata
        && a.file == b.file
}

/// Whether BAZEL_RS_VERIFY_VIEWS=1 asks to rebuild every memoized
/// dependency view on each later edge and compare (W1a's debug verifier).
fn verify_views() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("BAZEL_RS_VERIFY_VIEWS").is_some_and(|value| value == "1")
    });
    *VERIFY
}

/// Whether BAZEL_RS_VERIFY_ARTIFACTS=1 asks to check the artifact-index
/// invariant in release builds too (debug builds always check it): a File
/// re-registered with a different Artifact is an internal error
/// ([`register_artifact`]). (W1b's migration verifier also checked, after
/// each commit, that every File the pre-W1b provider-file walk reached from
/// the committed target was registered; it soaked on the whole test suite
/// and on ~/code, and W1d deleted it with the walk. It exempted the
/// status files, now registered for every lookup that can reach one
/// (before it, or by it: [`Engine::indexed_artifact`]).)
fn verify_artifacts() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("BAZEL_RS_VERIFY_ARTIFACTS").is_some_and(|value| value == "1")
    });
    *VERIFY
}

/// Register `artifact` as the Artifact of `file` in the build's one
/// resolver (W1b registration invariant, PLAN-L1 §2.1). A File is
/// registered once: a second registration keeps the first entry, which the
/// invariant makes the same Artifact — re-registering it with a different
/// one is an internal error, checked in debug builds and under
/// BAZEL_RS_VERIFY_ARTIFACTS.
fn register_artifact(index: &mut BTreeMap<File, Artifact>, file: &File, artifact: &Artifact) {
    match index.get(file) {
        Some(registered) => {
            if (cfg!(debug_assertions) || verify_artifacts())
                && !same_artifact(registered, artifact)
            {
                panic!(
                    "internal error: File {} re-registered with a different artifact: registered {registered:?}, now {artifact:?}",
                    file.path
                );
            }
        }
        None => {
            index.insert(file.clone(), artifact.clone());
        }
    }
}

/// The File/Artifact identity of a committed output (review F2 of
/// d2fd11c): the File a rule declared — what its own Starlark code holds,
/// returns in providers and registers as an action output — is the File its
/// Artifact computes, which dependency views hand to consumers, so one
/// artifact never reaches a consumer as two Files. Both follow one
/// short-path rule (crate::label::generated_short_path). The workspace
/// status files are held to it too: ctx.info_file / ctx.version_file are
/// their artifacts' Files, planned or bare ([`Artifact::workspace_status`],
/// checked where [`Engine::ensure_workspace_status`] registers them). A
/// divergence is an internal error, checked in debug builds and under
/// BAZEL_RS_VERIFY_ARTIFACTS like [`register_artifact`].
fn verify_declared_file(file: &File, artifact: &Artifact) {
    if (cfg!(debug_assertions) || verify_artifacts()) && artifact.file != *file {
        panic!(
            "internal error: declared output {} differs from its artifact's File: declared {file:?}, artifact {:?}",
            file.path, artifact.file
        );
    }
}

/// Configured target keys by index (MEM4): the planned request's hidden
/// top-level artifacts name their target by index, one key per target.
#[derive(Default)]
struct KeyTable {
    keys: Vec<ConfiguredTargetKey>,
    index: HashMap<ConfiguredTargetKey, usize>,
}

impl KeyTable {
    /// The index of `key`, added on first use.
    fn index(&mut self, key: ConfiguredTargetKey) -> usize {
        if let Some(index) = self.index.get(&key) {
            return *index;
        }
        let index = self.keys.len();
        self.keys.push(key.clone());
        self.index.insert(key, index);
        index
    }
}

/// The mutable planning fields an aspect application needs (N58): the same
/// maps [`Engine::plan_dependency_view`] takes individually, grouped so the
/// aspect planner can recurse (an aspect application plans its own
/// dependencies through the shared machinery).
struct PlanState<'a> {
    pub dry: &'a mut BTreeMap<ConfiguredTargetKey, Artifacts>,
    pub custom_providers:
        &'a mut BTreeMap<ConfiguredTargetKey, BTreeMap<String, starlark::values::OwnedFrozenValue>>,
    pub provider_keys: &'a mut BTreeMap<ConfiguredTargetKey, Vec<String>>,
    pub runfiles_info: &'a mut BTreeMap<ConfiguredTargetKey, Arc<crate::runfiles::TargetRunfiles>>,
    pub artifact_index: &'a mut BTreeMap<File, Artifact>,
    pub declared: &'a mut Declared,
    pub native_output_groups:
        &'a mut BTreeMap<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
    pub validation: &'a mut BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
    pub target_identities:
        &'a mut BTreeMap<TargetIdentityKey, std::sync::Arc<crate::starlark_types::Target>>,
    pub aspect_providers: &'a mut BTreeMap<
        crate::aspects::AspectApplicationKey,
        BTreeMap<String, starlark::values::OwnedFrozenValue>,
    >,
    pub aspect_target_identities: &'a mut AspectTargetIdentities,
    pub nodes: &'a mut Vec<PlannedAction>,
    pub producers: &'a mut Producers,
    pub source_inputs: &'a mut BTreeMap<String, SourceInputs>,
    pub client_environment: &'a mut std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    pub status_client_environment:
        &'a mut std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    pub workspace_status: &'a mut Option<WorkspaceStatusPlan>,
    pub views: &'a mut DependencyViews,
    pub input_groups: &'a mut HashMap<u64, ExpandedTree>,
    pub listed_tree_actions: &'a mut usize,
}

impl ActionPlanning {
    /// Reborrow every planning field (the caller's bindings stay usable).
    fn plan_state(&mut self) -> PlanState<'_> {
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            native_output_groups,
            validation,
            aspect_providers,
            target_identities,
            aspect_target_identities,
            workspace_status,
            views,
            input_groups,
            listed_tree_actions,
            ..
        } = self;
        PlanState {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            workspace_status,
            views,
            input_groups,
            listed_tree_actions,
        }
    }
}

/// The per-build workspace status plan (N63): the computed status and the
/// system write node's artifacts (the one BazelWorkspaceStatusAction's
/// stable and volatile outputs — Bazel has ONE action with both outputs,
/// BazelWorkspaceStatusModule.java:87-98).
struct WorkspaceStatusPlan {
    computed: crate::workspace_status::ComputedWorkspaceStatus,
    stable: Artifact,
    volatile: Artifact,
}
/// What `bazel run` executes after the build (Engine::build_for_run).
#[derive(Clone, Debug)]
struct RunPlan {
    /// FilesToRunProvider.getExecutable, `None` when not executable.
    executable: Option<Artifact>,
    /// The runfiles tree's MANIFEST (RunfilesSupport), if any.
    tree: Option<Artifact>,
    /// The runfiles mapping of the RunfilesSupport (RunfilesNodes::entries).
    entries: Option<Vec<(Arc<str>, Option<Artifact>)>>,
    settings: crate::runfiles::RunSettings,
    /// The configuration of the target (its output directory mnemonic).
    config: ConfigId,
    /// The producers of the target's top-level artifacts
    /// (crate::request::PlannedRun::completion).
    completion: Vec<String>,
}

/// RunCommand.isExecutable (runtime/commands/RunCommand.java:1186-1211):
/// whether a target may be run before it is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runnable {
    /// A file, an executable rule or an alias (AliasProvider.mayBeAlias).
    Yes,
    /// A test rule (TargetUtils.isTestRule).
    Test,
    No,
}

/// The executable of a `run` build (crate::engine::Engine::build_for_run).
#[derive(Clone, Debug)]
pub struct RunTarget {
    /// The published executable (FilesToRunProvider.getExecutable).
    pub executable: PathBuf,
    /// The native runfiles root holding `bazel-out/...`: the published
    /// executable sibling and its `.runfiles` tree (the execroot of
    /// RunfilesSupport.getRunfilesDirectory), when the target has runfiles
    /// support.
    pub runfiles: Option<RunRunfiles>,
    pub settings: crate::runfiles::RunSettings,
    /// The executable's exec path (`bazel-out/<cfg>/bin/<pkg>/<name>`).
    pub executable_exec: PathBuf,
    /// The executable's runfiles path relative to the workspace runfiles
    /// directory (Artifact.getRunfilesPath: `pkg/name`, `../repo/pkg/name`).
    pub executable_short_path: String,
    /// The runfiles mapping with published paths (`None`: an empty file),
    /// when the target has runfiles support.
    pub entries: Option<Vec<(String, Option<PathBuf>)>>,
    /// The runfiles paths of `entries` that are unresolved symlink
    /// artifacts (their published path is the symlink itself, staged with
    /// its link text like SymlinkTreeHelper.createRunfilesSymlinks).
    pub symlinks: BTreeSet<String>,
    /// The output directory mnemonic of the target's configuration.
    pub mnemonic: String,
    /// The configuration's action environment (N42), TestPolicy step 2.
    pub action_env: crate::action_env::ActionEnvironment,
    /// The configuration's `--test_env` environment (N42), TestPolicy step 3.
    pub test_env: crate::action_env::ActionEnvironment,
}

/// What one request plans ([`Engine::plan_request`]).
pub struct RequestSpec<'a> {
    /// The command and what only it reads after planning (the tests of
    /// `test`, the test properties of a `run` target).
    pub command: crate::request::RequestCommand,
    /// The targets to build.
    pub build: &'a [Label],
    /// The run/test targets whose executables and runfiles are planned too
    /// (crate::request::PlannedRun).
    pub runs: &'a [Label],
    /// Whether the run targets need their runfiles tree (`run`,
    /// RunCommand.ensureRunfilesBuilt), or only the runfiles artifacts
    /// (`test` stages them itself).
    pub trees: bool,
}

impl<'a> RequestSpec<'a> {
    /// `bazel build LABELS`.
    pub fn build(labels: &'a [Label]) -> Self {
        Self {
            command: crate::request::RequestCommand::Build,
            build: labels,
            runs: &[],
            trees: true,
        }
    }
}

/// The runfiles of a `run` target.
#[derive(Clone, Debug)]
pub struct RunRunfiles {
    /// `<root>/bazel-out/<cfg>/bin/<pkg>/<name>.runfiles`.
    pub directory: PathBuf,
    /// `<root>/bazel-out/<cfg>/bin/<pkg>/<name>`, a hardlink of the executable blob.
    pub executable: PathBuf,
    /// `<root>`, the analog of Bazel's execution root for BUILD_EXECROOT.
    pub root: PathBuf,
}

/// The native actions of one RunfilesSupport (analysis/RunfilesSupport.java:280-347).
#[derive(Clone, Debug)]
struct RunfilesNodes {
    /// SymlinkTreeAction's `<exe>.runfiles/MANIFEST`: built with the target
    /// under --build_runfile_links, by `run` otherwise (absent without
    /// runfiles manifests).
    tree: Option<Artifact>,
    /// SourceManifestAction's `<exe>.runfiles_manifest`
    /// (--build_runfile_manifests).
    manifest: Option<Artifact>,
    /// RepoMappingManifestAction's `<exe>.repo_mapping`.
    repo_mapping: Artifact,
    /// The executable.
    executable: Artifact,
    /// The runfiles mapping (Runfiles.getRunfilesInputs, `_repo_mapping`
    /// included): each runfiles path with its artifact, `None` for an empty
    /// file. A test action stages these by content (N23g).
    entries: Vec<(Arc<str>, Option<Artifact>)>,
    /// Prefix conflicts reported by the manifest action, with the rule's
    /// location and the conflict policy (ERROR when true).
    conflicts: Vec<String>,
    conflict_error: bool,
    location: Option<String>,
}

/// An analyzed rule target in its configuration.
#[derive(Clone, Debug, Default)]
struct ConfiguredNode {
    /// ConfigMatchingProvider of an analyzed `config_setting` (forwarded by
    /// aliases).
    matching: Option<ConfigMatching>,
    /// The native BuildSettingProvider (forwarded by `alias`, overridden by
    /// label_flag/label_setting; rules/LateBoundAlias.java:58-79).
    build_setting: Option<BuildSettingInfo>,
    /// The direct `actual` of `alias`, `label_flag` and `label_setting`
    /// (AliasConfiguredTarget, rules/AliasConfiguredTarget.java:99-123).
    actual: Option<Label>,
    /// ConstraintSettingInfo of a `constraint_setting` (forwarded by aliases).
    constraint_setting: Option<ConstraintSettingInfo>,
    /// ConstraintValueInfo of a `constraint_value` (forwarded by aliases).
    constraint_value: Option<ConstraintValueInfo>,
    /// PlatformInfo of a `platform` (forwarded by aliases).
    platform: Option<Arc<PlatformInfo>>,
    /// ToolchainTypeInfo of a `toolchain_type` (forwarded by aliases).
    toolchain_type: Option<ToolchainTypeInfo>,
    /// DeclaredToolchainInfo of a `toolchain` (forwarded by aliases).
    declared_toolchain: Option<Arc<DeclaredToolchainInfo>>,
    /// ConfigFeatureFlagProvider of a `config_feature_flag` (forwarded by
    /// aliases; Starlark FeatureFlagInfo values are phase-2 providers).
    feature_flag: Option<crate::feature_flags::FeatureFlagValue>,
}

/// A failure of toolchain resolution: a ToolchainException, reported as
/// `While resolving toolchains for target L (CONFIG): MESSAGE` at the
/// rule's location (skyframe/toolchains/ToolchainException.java:67-91), or
/// an error that already carries its own report (the analysis error of a
/// prerequisite, ConfiguredValueCreationException, which
/// asConfiguredValueCreationException propagates unchanged).
/// A configuration's registered execution platforms, in order
/// (RegisteredExecutionPlatformsValue).
type ExecutionPlatforms = Arc<Vec<Arc<PlatformInfo>>>;

/// What a registration pattern registers (Engine::expand_registration_patterns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Registration {
    Toolchain,
    ExecutionPlatform,
}

impl Registration {
    fn noun(self) -> &'static str {
        match self {
            Self::Toolchain => "toolchain",
            Self::ExecutionPlatform => "execution platform",
        }
    }
}

/// Attribute.getName of a Starlark rule's attribute: a private `_x` is
/// `$x` natively (StarlarkRuleClassFunctions.java:1855, Attribute.getStarlarkName,
/// packages/Attribute.java:2307-2312), as dependency errors name it
/// (DependencyProducer.getMessageWithEdgeTransitionInfo, :258-266).
fn native_attribute_name(name: &str) -> std::borrow::Cow<'_, str> {
    match name.strip_prefix('_') {
        Some(rest) => format!("${rest}").into(),
        None => name.into(),
    }
}

/// Engine::platform_resolution_error's split as a ResolutionError: a
/// loading error names the label as a non-platform; other errors keep their
/// reported lines and wrap the last one.
fn platform_resolution_failure(label: &Label, error: PlatformError) -> ResolutionError {
    let text = match error {
        PlatformError::Missing { .. } => crate::platforms::not_a_platform(label),
        PlatformError::Other(text) => text,
    };
    match text.rsplit_once('\n') {
        Some((events, last)) => ResolutionError::Reported(events.to_owned(), last.to_owned()),
        None => ResolutionError::Toolchain(text),
    }
}

#[derive(Clone, Debug)]
enum ResolutionError {
    Toolchain(String),
    /// An error that aborts the analysis of the requested target
    /// (`Analysis of target 'L' failed; build aborted: MESSAGE`).
    Aborted(String),
    /// A ToolchainException preceded by reported events (a loading event).
    Reported(String, String),
    Propagated(String),
}

/// Why a platform label could not be resolved (PlatformFunction).
#[derive(Clone, Debug)]
enum PlatformError {
    /// A package or target loading error (NoSuchThingException, propagated
    /// unchanged by InvalidPlatformException, PlatformLookupUtil.java:186-190).
    Missing {
        class: &'static str,
        message: String,
    },
    /// Any other failure, its full text.
    Other(String),
}

impl PlatformError {
    /// A `no such target`/`no such package` loading error, or another error.
    fn loading(error: String) -> Self {
        let class = if error.starts_with("no such target '") {
            "NoSuchTargetException"
        } else if error.starts_with("no such package '") {
            if error.contains("BUILD file not found") {
                "BuildFileNotFoundException"
            } else {
                "NoSuchPackageException"
            }
        } else {
            return Self::Other(error);
        };
        Self::Missing {
            class,
            message: error,
        }
    }

    /// The text of a failing target platform: Bazel 9.3 prints the loading
    /// exception itself (`com.google.devtools.build.lib.packages.CLASS:
    /// MESSAGE`, paired capture A4/platforms_e_nope, U/.../platforms_missing).
    fn target_text(&self) -> String {
        match self {
            Self::Missing { class, message } => {
                format!("com.google.devtools.build.lib.packages.{class}: {message}")
            }
            Self::Other(text) => text.clone(),
        }
    }
}

/// The platform providers of a `constraint_setting`, `constraint_value` or
/// `platform` target.
type PlatformProviders = (
    Option<ConstraintSettingInfo>,
    Option<ConstraintValueInfo>,
    Option<Arc<PlatformInfo>>,
);

/// A native provider an attribute requires (`mandatoryProviders`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeProvider {
    ConstraintSetting,
    ConstraintValue,
    Platform,
    ToolchainType,
}

impl NativeProvider {
    /// BuiltinProvider names (RequiredProviders.getDescription quotes them,
    /// packages/RequiredProviders.java:209-213).
    fn name(self) -> &'static str {
        match self {
            Self::ConstraintSetting => "ConstraintSettingInfo",
            Self::ConstraintValue => "ConstraintValueInfo",
            Self::Platform => "PlatformInfo",
            Self::ToolchainType => "ToolchainTypeInfo",
        }
    }

    fn provided_by(self, node: &ConfiguredNode) -> bool {
        match self {
            Self::ConstraintSetting => node.constraint_setting.is_some(),
            Self::ConstraintValue => node.constraint_value.is_some(),
            Self::Platform => node.platform.is_some(),
            Self::ToolchainType => node.toolchain_type.is_some(),
        }
    }
}

/// A label attribute of a native rule as RuleContext.Builder validates its
/// prerequisites (analysis/RuleContext.java:1845-1880, 2002-2093, 2165-2172):
/// every attribute here allows no files.
struct PrerequisiteAttribute {
    name: &'static str,
    labels: Vec<String>,
    /// `allowedRuleClasses` (empty: any rule).
    allowed_rule_classes: &'static [&'static str],
    mandatory_provider: Option<NativeProvider>,
}

/// Rule.useToolchainResolution (packages/Rule.java:846-854) of the native
/// rule classes and Starlark rules: disabled for `constraint_setting`,
/// `constraint_value`, `platform`, `toolchain`, `toolchain_type`
/// (ToolchainResolutionMode.DISABLED, rules/platform/*Rule.java,
/// rules/ToolchainType.java:62), label settings and Starlark build settings
/// (rules/LabelBuildSettings.java:99, packages/RuleClass.java:916-918),
/// conditional for `alias` (select() keys or target_compatible_with,
/// rules/Alias.java:78-98) and `config_setting` (non-empty constraint_values,
/// rules/config/ConfigRuleClasses.java:196-201), enabled otherwise.
fn uses_toolchain_resolution(rule: &Rule) -> bool {
    if let Some(custom) = &rule.custom {
        return custom.build_setting.is_none();
    }
    match (&rule.kind, &rule.native) {
        (RuleKind::PackageGroup(_), _) => false,
        (RuleKind::Native("alias"), Some(native)) => {
            !configured_attributes::rule_config_dependencies(rule).is_empty()
                || !native.labels("target_compatible_with").is_empty()
                || native.configurable.contains_key("target_compatible_with")
        }
        (RuleKind::Native("config_setting"), Some(native)) => {
            !native.labels("constraint_values").is_empty()
        }
        (
            RuleKind::Native(
                "label_flag"
                | "label_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain"
                | "toolchain_type"
                | "config_feature_flag",
            ),
            _,
        ) => false,
        _ => true,
    }
}

/// The label attributes of `constraint_setting` (ConstraintSettingRule.java:84-89),
/// `constraint_value` (ConstraintValueRule.java:56-63) and `platform`
/// (PlatformRule.java:75-89, 149-152), in rule-class attribute order.
fn platform_prerequisite_attributes(rule: &Rule) -> Vec<PrerequisiteAttribute> {
    let Some(native) = &rule.native else {
        return Vec::new();
    };
    let label = |name: &str| match native.attrs.get(name) {
        Some(crate::custom_rule::AttrValue::Label(Some(text))) => vec![text.clone()],
        _ => Vec::new(),
    };
    match rule.kind {
        RuleKind::Native("constraint_setting") => vec![PrerequisiteAttribute {
            name: "refines_constraint_value",
            labels: label("refines_constraint_value"),
            allowed_rule_classes: &["constraint_value"],
            mandatory_provider: Some(NativeProvider::ConstraintValue),
        }],
        RuleKind::Native("constraint_value") => vec![PrerequisiteAttribute {
            name: "constraint_setting",
            labels: label("constraint_setting"),
            allowed_rule_classes: &["constraint_setting"],
            mandatory_provider: Some(NativeProvider::ConstraintSetting),
        }],
        RuleKind::Native("platform") => vec![
            PrerequisiteAttribute {
                name: "constraint_values",
                labels: native.labels("constraint_values"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            PrerequisiteAttribute {
                name: "parents",
                labels: native.labels("parents"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::Platform),
            },
            PrerequisiteAttribute {
                name: "required_settings",
                labels: native.labels("required_settings"),
                allowed_rule_classes: &["config_setting"],
                mandatory_provider: None,
            },
        ],
        // rules/platform/ToolchainRule.java:57-111 (the NODEP `toolchain`
        // attribute is not a prerequisite).
        RuleKind::Native("toolchain") => vec![
            PrerequisiteAttribute {
                name: "toolchain_type",
                labels: label("toolchain_type"),
                allowed_rule_classes: &["toolchain_type"],
                mandatory_provider: Some(NativeProvider::ToolchainType),
            },
            PrerequisiteAttribute {
                name: "exec_compatible_with",
                labels: native.labels("exec_compatible_with"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            PrerequisiteAttribute {
                name: "target_compatible_with",
                labels: native.labels("target_compatible_with"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            // `target_settings`: only config_setting rules, no files, no
            // mandatory provider (ToolchainRule.java:97-103).
            PrerequisiteAttribute {
                name: "target_settings",
                labels: native.labels("target_settings"),
                allowed_rule_classes: &["config_setting"],
                mandatory_provider: None,
            },
        ],
        _ => Vec::new(),
    }
}

/// RuleContext.Builder.validateRuleDependency (analysis/RuleContext.java:2002-2093):
/// a rule prerequisite is accepted when its class is allowed, else when it
/// has the mandatory provider; otherwise both unfulfilled requirements are
/// joined (StringUtil.joinEnglishList with "and").
fn rule_dependency_error(
    attribute: &PrerequisiteAttribute,
    chain: &[Label],
    actual: &Label,
    actual_rule: &Rule,
    node: Option<&ConfiguredNode>,
) -> Option<String> {
    let mut unfulfilled = Vec::new();
    if !attribute.allowed_rule_classes.is_empty() {
        if attribute
            .allowed_rule_classes
            .contains(&actual_rule.rule_class_name())
        {
            return None;
        }
        unfulfilled.push(format!(
            "{} is misplaced here (expected {})",
            describe_with_aliases(
                chain,
                actual,
                &format!("{} rule", actual_rule.rule_class_name())
            ),
            join_english_list(attribute.allowed_rule_classes, "or")
        ));
    }
    if let Some(provider) = attribute.mandatory_provider {
        if node.is_some_and(|node| provider.provided_by(node)) {
            return None;
        }
        unfulfilled.push(format!(
            "'{}' does not have mandatory providers: '{}'",
            crate::rule_declarations::canonical_form(actual),
            provider.name()
        ));
    }
    (!unfulfilled.is_empty()).then(|| join_english_list(&unfulfilled, "and"))
}

/// StringUtil.joinEnglishList (util/StringUtil.java:54-73) with the Oxford
/// comma.
fn join_english_list<T: AsRef<str>>(choices: &[T], last: &str) -> String {
    let mut text = String::new();
    for (index, choice) in choices.iter().enumerate() {
        if index > 0 {
            let is_last = index + 1 == choices.len();
            if !is_last || index >= 2 {
                text.push(',');
            }
            if is_last {
                text.push(' ');
                text.push_str(last);
            }
            text.push(' ');
        }
        text.push_str(choice.as_ref());
    }
    if text.is_empty() {
        "nothing".to_owned()
    } else {
        text
    }
}

/// PlatformLookupUtil.hasPlatformInfo (skyframe/toolchains/PlatformLookupUtil.java:160-172):
/// the target's rule does not use toolchain resolution and its class
/// advertises PlatformInfo (`platform`, PlatformRule.java:53) or can have any
/// provider (`alias`, rules/Alias.java:76; label settings,
/// rules/LabelBuildSettings.java:98).
fn has_platform_info(rule: &Rule) -> bool {
    !uses_toolchain_resolution(rule)
        && matches!(
            rule.kind,
            RuleKind::Native("platform" | "alias" | "label_flag" | "label_setting")
        )
}

/// The mandatory-provider error of the first `target_compatible_with`
/// label without a ConstraintValueInfo or ConfigMatchingProvider, if any
/// (Engine::check_target_compatibility).
fn unprovided_error(label: Option<&Label>, error: impl Fn(&Label) -> String) -> Result<(), String> {
    label.map_or(Ok(()), |label| Err(error(label)))
}

/// Rules whose configured target is an AliasConfiguredTarget: `alias`
/// (rules/Alias.java:43-56) and the LateBoundAlias build settings
/// label_flag/label_setting (rules/LateBoundAlias.java:47-79).
fn forwards(rule: &Rule) -> bool {
    matches!(
        rule.kind,
        RuleKind::Native("alias" | "label_flag" | "label_setting")
    )
}

/// AliasProvider.describeTargetWithAliases (analysis/AliasProvider.java:123-148):
/// `KIND 'L'`, or `alias 'A' referring to KIND 'L'[ through 'B' -> 'C']`.
fn describe_with_aliases(chain: &[Label], target: &Label, kind: &str) -> String {
    let target = crate::rule_declarations::canonical_form(target);
    let Some((first, rest)) = chain.split_first() else {
        return format!("{kind} '{target}'");
    };
    let mut text = format!(
        "alias '{}' referring to {kind} '{target}'",
        crate::rule_declarations::canonical_form(first)
    );
    if !rest.is_empty() {
        text.push_str(&format!(
            " through '{}'",
            rest.iter()
                .map(crate::rule_declarations::canonical_form)
                .collect::<Vec<_>>()
                .join("' -> '")
        ));
    }
    text
}

type ConfiguredNodes = BTreeMap<ConfiguredTargetKey, ConfiguredNode>;

/// The transition of a dependency edge.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EdgeCfg {
    /// No transition: the parent's configuration.
    Target,
    /// ExecutionTransitionFactory for the default (`None`) or a named exec
    /// group: Starlark `cfg = "exec"`/`"host"`/`config.exec(...)`, genrule
    /// `tools` (rules/genrule/GenRuleBaseRule.java:89-92).
    Exec(Option<String>),
    /// NoConfigTransition: the `package_metadata` edges
    /// (analysis/BaseRuleClasses.java:291-297), analyzed in the no-config
    /// configuration (crate::config_registry::no_config).
    NoConfig,
    /// The Starlark `transition()` of the named attribute
    /// (StarlarkAttributeTransitionProvider; crate::starlark_transitions).
    Starlark(String),
}

/// A dependency edge of a configured rule: the label text, its attribute
/// and transition.
struct Edge {
    label: String,
    attribute: String,
    cfg: EdgeCfg,
}

/// The dependency edges of a configured (select-resolved) rule, in the
/// order of `rule.srcs` (Starlark attributes in name order,
/// crate::package::custom_dependency_labels), then genrule `tools`, then
/// the effective `package_metadata` (nonconfigurable, NoConfigTransition).
/// The implicit test-runner attributes of every Starlark test rule that
/// are analyzed without `--collect_code_coverage` and `--run_under`
/// (StarlarkRuleClassFunctions.getTestBaseRule, analysis/starlark/StarlarkRuleClassFunctions.java:290-352):
/// all use the exec transition of the `test` exec group. `:coverage_report_generator`
/// is null without coverage (CoverageConfiguration.reportGenerator,
/// analysis/test/CoverageConfiguration.java:98-103), the run-under
/// attributes without `--run_under` (BaseRuleClasses.java:155-197).
pub(crate) const TEST_RUNNER_ATTRIBUTES: [(&str, &str); 7] = [
    ("$test_wrapper", "@@bazel_tools//tools/test:test_wrapper"),
    ("$xml_writer", "@@bazel_tools//tools/test:xml_writer"),
    ("$test_runtime", "@@bazel_tools//tools/test:runtime"),
    ("$test_setup_script", "@@bazel_tools//tools/test:test_setup"),
    (
        "$xml_generator_script",
        "@@bazel_tools//tools/test:test_xml_generator",
    ),
    (
        "$collect_coverage_script",
        "@@bazel_tools//tools/test:collect_coverage",
    ),
    (
        ":coverage_support",
        "@@bazel_tools//tools/test:coverage_support",
    ),
];

/// The loading-time test properties of a test rule (TestTargetProperties,
/// analysis/test/TestTargetProperties.java:81-135): size, timeout, the
/// taggable `flaky`/`local`, `tags` and the rule class; `None` for any other
/// rule. Never fails: `shard_count` is the plain value, unchecked (-1 when
/// unspecified or a select()); [`test_rule_info_of`] checks it.
fn test_rule_properties(rule: &Rule) -> Option<crate::test_command::TestRuleInfo> {
    use crate::custom_rule::AttrValue;
    use crate::test_command::{TestRuleInfo, TestSize, TestTimeout};
    let custom = rule.custom.as_ref()?;
    if !custom.declaration.builtin.kind.test {
        return None;
    }
    let target = &custom.declaration.builtin.target;
    let string = |name: &str| match target.attrs.get(name) {
        Some(AttrValue::String(value)) => Some(value.clone()),
        _ => None,
    };
    let boolean = |name: &str| matches!(target.attrs.get(name), Some(AttrValue::Bool(true)));
    // Loading validated size and timeout (RuleClass.checkForValidSizeAndTimeoutValues).
    let size = string("size")
        .and_then(|size| TestSize::parse(&size))
        .unwrap_or(TestSize::Medium);
    let timeout = string("timeout")
        .and_then(|timeout| TestTimeout::parse(&timeout))
        .unwrap_or(size.default_timeout());
    let shard_count = match target.attrs.get("shard_count") {
        Some(AttrValue::Int(count)) => *count,
        _ => -1,
    };
    Some(TestRuleInfo {
        rule_class: target.rule_class.clone(),
        size,
        timeout,
        flaky: boolean("flaky"),
        shard_count,
        local: boolean("local"),
        tags: target.tags.clone(),
    })
}

/// A test rule's Rule.getRuleTags (packages/Rule.java:752-765), what its
/// TestTagsProvider carries (analysis/RuleConfiguredTargetBuilder.java:189-192);
/// `None` for any other rule. Tags only: no shard_count is read.
fn rule_tags_of(rule: &Rule) -> Option<BTreeSet<String>> {
    test_rule_properties(rule).map(|info| info.rule_tags())
}

/// How [`test_rule_info_of`] treats a select() in `shard_count`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShardSelect {
    /// The native test runner needs the shard count of a test it runs
    /// (`test`, `run` of a test): an explicit error.
    Reject,
    /// The target pattern phase's TestFilter reads no shard count
    /// (pkgcache/TestFilter.java:59-80): -1 stands in for it.
    Ignore,
}

/// The test properties of a test rule ([`test_rule_properties`]) with
/// RuleConfiguredTargetBuilder.initializeTestProvider's shard_count checks
/// (analysis/RuleConfiguredTargetBuilder.java:470-484) and its location;
/// `None` for any other rule.
fn test_rule_info_of(
    label: &Label,
    rule: &Rule,
    shard_select: ShardSelect,
) -> Result<Option<(crate::test_command::TestRuleInfo, Option<String>)>, String> {
    use crate::custom_rule::AttrValue;
    let Some(info) = test_rule_properties(rule) else {
        return Ok(None);
    };
    let Some(custom) = &rule.custom else {
        return Ok(None);
    };
    let target = &custom.declaration.builtin.target;
    if shard_select == ShardSelect::Reject && target.configurable.contains_key("shard_count") {
        return Err(located(
            rule.location.as_deref(),
            format!(
                "{label}: a select() in shard_count is not supported by the native test runner"
            ),
        ));
    }
    if let Some(AttrValue::Int(count)) = target.attrs.get("shard_count") {
        let attribute_error = |message: &str| {
            located(
                rule.location.as_deref(),
                format!(
                    "in shard_count attribute of {} rule {}: {message}",
                    target.rule_class,
                    crate::rule_declarations::canonical_form(label)
                ),
            )
        };
        if *count < 0 {
            return Err(attribute_error("Must not be negative."));
        }
        if *count > 50 {
            return Err(attribute_error(
                "Having more than 50 shards is indicative of poor test organization. Please reduce the number of shards.",
            ));
        }
    }
    Ok(Some((info, rule.location.clone())))
}

/// The tags a test_suite filters a test rule with (TestTargetUtils
/// .filterTests, packages/TestTargetUtils.java:95-117, and
/// TestSuiteImplicitTestsAccumulator.processRule :71-90): its `tags` plus
/// its `size`; `None` for a rule that is no test rule.
fn test_suite_filter_tags(rule: &Rule) -> Option<BTreeSet<String>> {
    use crate::custom_rule::AttrValue;
    let custom = rule.custom.as_ref()?;
    if !custom.declaration.builtin.kind.test {
        return None;
    }
    let target = &custom.declaration.builtin.target;
    // Loading validated the size (RuleClass.checkForValidSizeAndTimeoutValues).
    let size = match target.attrs.get("size") {
        Some(AttrValue::String(size)) => crate::test_command::TestSize::parse(size),
        _ => None,
    }
    .unwrap_or(crate::test_command::TestSize::Medium);
    let mut tags: BTreeSet<String> = target.tags.iter().cloned().collect();
    tags.insert(size.name().to_owned());
    Some(tags)
}

/// The label texts of an aspect attribute's value (its default; parameters
/// are never labels): the dependencies of the aspect's own attributes,
/// resolved from the aspect's `.bzl` package.
/// The [`Engine::aspect_attribute_configurations`] memo name of the
/// attributes of `owner` in an application of `main`: the aspect's identity
/// for its own attributes, `owner@main` for a base aspect's (resolved over
/// the main aspect's execution platforms).
fn aspect_attribute_memo_name(
    owner: &crate::aspects::AspectDefinition,
    main: &crate::aspects::AspectDefinition,
) -> String {
    if owner.identity == main.identity {
        owner.identity.clone()
    } else {
        format!("{}@{}", owner.identity, main.identity)
    }
}

fn aspect_attribute_labels(spec: &crate::custom_rule::AttrSpec) -> Vec<String> {
    match &spec.default {
        crate::custom_rule::AttrValue::Label(Some(label)) => vec![label.clone()],
        crate::custom_rule::AttrValue::Labels(labels) => labels.clone(),
        _ => Vec::new(),
    }
}

fn dependency_edges(rule: &Rule) -> Vec<Edge> {
    let mut edges = attribute_edges(rule);
    edges.extend(rule.package_metadata.iter().map(|label| Edge {
        label: label.clone(),
        attribute: "package_metadata".to_owned(),
        cfg: EdgeCfg::NoConfig,
    }));
    edges
}

fn attribute_edges(rule: &Rule) -> Vec<Edge> {
    let edge = |label: &String, attribute: &str, cfg: EdgeCfg| Edge {
        label: label.clone(),
        attribute: attribute.to_owned(),
        cfg,
    };
    if let Some(custom) = &rule.custom {
        let mut edges = Vec::new();
        for (name, value) in &custom.attrs {
            let cfg = match custom.schema.get(name).map(|spec| &spec.declaration.cfg) {
                Some(crate::rule_declarations::Cfg::Exec(group)) => EdgeCfg::Exec(
                    group
                        .clone()
                        // The default exec group by name
                        // (DeclaredExecGroup.DEFAULT_EXEC_GROUP_NAME).
                        .filter(|group| group != "default-exec-group"),
                ),
                Some(crate::rule_declarations::Cfg::Starlark {
                    analysis_test: false,
                    ..
                }) => EdgeCfg::Starlark(name.clone()),
                _ => EdgeCfg::Target,
            };
            // As crate::package::custom_dependency_labels, per attribute.
            match value {
                crate::custom_rule::AttrValue::Label(Some(label)) => {
                    edges.push(edge(label, name, cfg));
                }
                crate::custom_rule::AttrValue::Labels(labels) => {
                    edges.extend(labels.iter().map(|label| edge(label, name, cfg.clone())));
                }
                crate::custom_rule::AttrValue::LabelKeyedStringDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .map(|(label, _)| edge(label, name, cfg.clone())),
                    );
                }
                // DictType.visitLabels (packages/Type.java:504-514): label values.
                crate::custom_rule::AttrValue::StringKeyedLabelDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .map(|(_, label)| edge(label, name, cfg.clone())),
                    );
                }
                crate::custom_rule::AttrValue::LabelListDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .flat_map(|(_, labels)| labels.iter())
                            .map(|label| edge(label, name, cfg.clone())),
                    );
                }
                _ => {}
            }
        }
        // The built-in `toolchains` attribute (StarlarkRuleClassFunctions.java:165-168):
        // TemplateVariableInfo prerequisites in the target configuration.
        edges.extend(
            custom
                .declaration
                .builtin
                .target
                .labels("toolchains")
                .iter()
                .map(|label| edge(label, "toolchains", EdgeCfg::Target)),
        );
        return edges;
    }
    let mut edges = rule
        .srcs
        .iter()
        .map(|label| edge(label, "srcs", EdgeCfg::Target))
        .collect::<Vec<_>>();
    if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
        edges.extend(
            native
                .labels("tools")
                .iter()
                .map(|label| edge(label, "tools", EdgeCfg::Exec(None))),
        );
        // GenRuleBaseRule.java:322-333: TemplateVariableInfo and/or
        // toolchain_type prerequisites, in the target configuration.
        edges.extend(
            native
                .labels("toolchains")
                .iter()
                .map(|label| edge(label, "toolchains", EdgeCfg::Target)),
        );
    }
    edges
}

/// The planned actions of a build and the requested targets' artifacts.
/// The ToolchainCollection of one aspect in one configuration (the
/// aspect's own toolchain contexts, AspectFunction.
/// getUnloadedToolchainContextsInputs, AspectFunction.java:811-838): its
/// default exec group's context and its declared and automatic exec groups.
#[derive(Clone, Debug, Default)]
struct AspectToolchains {
    /// The default exec group's resolved context; `None` when resolution
    /// needed no platform (the lazy default host platform) or the
    /// configuration resolves no toolchains (a platform's tree).
    default: Option<UnloadedToolchainContext>,
    groups: crate::exec_groups::TargetExecGroups,
    /// Query-only IDs of ToolchainResolution's direct configured deps. Shared
    /// by applications that reuse this context, never provider/graph copies.
    cquery_dependencies: Vec<u32>,
}

/// The provider map one aspect application returned (AspectValue's
/// ConfiguredAspect.getProviders).
type AspectProviders = BTreeMap<String, starlark::values::OwnedFrozenValue>;

/// The applications of one dependency's aspect collection, in application
/// order: each application's key and providers.
type AspectApplications = Vec<(crate::aspects::AspectApplicationKey, AspectProviders)>;

type BuildPlan = (
    Vec<PlannedAction>,
    Vec<(ConfiguredTargetKey, Artifact)>,
    Vec<(String, Label, Vec<Artifact>)>,
    // Labels whose own artifacts print a target result (the --show_result
    // budget; BuildResultPrinter.outputTargetsAndAspects).
    usize,
);

/// The analyzed rules of a build in analysis (postorder) order, each with
/// its configured target key (the configuration it was analyzed in).
type Plan = Vec<(ConfiguredTargetKey, Arc<Rule>)>;

/// The owned, `Send` inputs of one custom rule's pure analysis (the
/// compute boundary, review §1.7): the rule with its rule context
/// prepared by the coordinator (toolchain/exec-group contexts, ctx
/// surface, C++ fragment view, target platform), the resolved
/// dependency targets, output naming, the fetched-only repository set
/// and the loaded-function registry. Nothing in it borrows the
/// engine; `Engine::compute_target` touches nothing else.
struct ComputeSnapshot {
    rule: Rule,
    dependency_targets: BTreeMap<String, DependencyTarget>,
    output_root: String,
    action_owner: String,
    action_env: Arc<crate::action_env::ActionEnvironment>,
    runfiles: crate::runfiles_tree::RunfilesOptions,
    loaded_functions: Arc<crate::analysis_runtime::LoadedFunctions>,
    repositories: Arc<RepositorySet>,
}

/// The compute of one custom rule: the analysis result (or error) and
/// the implementation's buffered `print()` lines, in order. The lines
/// are emitted by the commit (review §1.8); a discarded attempt's
/// lines are dropped with it.
struct ComputeOutcome {
    analyzed: Result<analysis::AnalysisResult, String>,
    printed: Vec<String>,
}

/// One plan entry after prepare (the P1 split of plan_target): the
/// coordinator-computed context of the target, with the kind deciding
/// what compute and commit do.
struct PreparedTarget {
    key: ConfiguredTargetKey,
    config: ConfigId,
    output_root: String,
    action_env: Arc<crate::action_env::ActionEnvironment>,
    action_owner: String,
    exec_config: ConfigId,
    main_ignored: Arc<IgnoredSubdirectories>,
    /// The transitive repository closure of the target's dependency
    /// edges (Starlark rules refine it from their resolved
    /// dependencies; the commit inserts it).
    transitive: BTreeSet<String>,
    kind: PreparedKind,
}

enum PreparedKind {
    /// A package group: committed directly, no compute.
    PackageGroup,
    /// A native rule (genrule, filegroup, platforms, aliases): its
    /// analysis stays coordinator-side (P3 dispatches custom rules
    /// only; native runs inline).
    Native,
    /// A custom rule (boxed: the snapshot dominates the enum's size):
    /// `snapshot` feeds the compute, `action_execution` the commit's
    /// execution-platform table. The commit resolves every File through
    /// the build's `artifact_index` (W1b). The plan's own rule (a
    /// parameter of commit) serves the commit.
    Custom(Box<CustomPrepared>),
}

struct CustomPrepared {
    snapshot: ComputeSnapshot,
    action_execution: Vec<crate::exec_groups::ActionExecution>,
}

/// The worker stack size for parallel package loading (review §1.6):
/// Rust's 2 MiB default overflows where the 8 MiB main thread works
/// (Starlark evaluation, depset flattening, JSON); 16
/// MiB is comfortable.
const LOADING_WORKER_STACK: usize = 16 << 20;

/// The `BAZEL_RS_LOADING_JITTER` roll of unit `index` on worker `worker`
/// (P2's pattern-loading workers and crate::prefetch's alike).
pub(crate) fn loading_jitter_roll(index: u64, worker: u64) -> u64 {
    splitmix64(index << 32 ^ worker)
}

/// The `BAZEL_RS_LOADING_JITTER` hidden test hook's bound in milliseconds
/// (0: off).
pub(crate) fn loading_jitter_millis() -> u64 {
    std::env::var("BAZEL_RS_LOADING_JITTER")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// `splitmix64`: reproducible pseudo-random worker jitter (no rand
/// dependency).
fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The compute-boundary types are `Send`: P1 has no threads, but the
/// types must already admit P3's workers (review §1.7). Asserted by
/// `compute_boundary_types_are_send` (tests) — the proof function is
/// dead code outside them by design.
#[allow(dead_code)]
fn compute_boundary_is_send() {
    fn send<T: Send>() {}
    send::<ComputeSnapshot>();
    send::<ComputeOutcome>();
}

/// StarlarkBuildSettingsDetailsValue: the details of each setting a
/// transition references, keyed by the label as referenced.
type TransitionDetails = BTreeMap<Label, crate::starlark_transitions::SettingDetail>;

/// A loaded build-setting candidate (verifyBuildSettingsAndGetAliases).
enum SettingTarget {
    Alias(Label),
    Setting(crate::starlark_transitions::SettingDetail),
}

/// The key of the Starlark transition cache: the transition, the source
/// configuration and a digest of the target and attribute values the
/// transition reads (StarlarkTransitionCache.Key,
/// analysis/config/StarlarkTransitionCache.java:189-225: the transition
/// instance encapsulates the target's attributes).
type TransitionCacheKey = (u64, ConfigId, [u8; 32]);

type TransitionConfigurations = Arc<[(String, ConfigId)]>;

enum EdgeConfigurations {
    One(ConfigId),
    Transition(TransitionConfigurations),
}

impl EdgeConfigurations {
    fn ids(&self) -> impl Iterator<Item = ConfigId> + '_ {
        let (one, branches): (Option<ConfigId>, &[(String, ConfigId)]) = match self {
            Self::One(id) => (Some(*id), &[]),
            Self::Transition(branches) => (None, branches),
        };
        one.into_iter().chain(branches.iter().map(|(_, id)| *id))
    }
}

#[derive(Debug, Default)]
pub struct BuildSummary {
    pub outputs: Vec<(Label, PathBuf)>,
    /// Labels whose own (non-aspect) artifacts print a target result — the
    /// `--show_result` budget half for targets (N58).
    pub non_empty_targets: usize,
    /// `Aspect NAME of LABEL up-to-date:` reports with their artifacts
    /// (BuildResultPrinter.outputAspects, :312-332), in aspect order.
    pub aspects: Vec<(String, Label, Vec<PathBuf>)>,
    pub executed: usize,
    pub cache_hits: usize,
    /// Actions whose outputs came from the remote cache (crate::remote_cache).
    pub remote_hits: usize,
}

/// A target pattern evaluation failure (`Engine::expand_pattern`).
#[derive(Clone, Debug)]
pub(crate) enum PatternError {
    /// A TargetParsingException raised while evaluating a parsed pattern
    /// (`no targets found beneath`, a path that names no target); syntax
    /// errors are crate::target_pattern::parse's.
    Parsing(String),
    /// Resolving the pattern's repository, or loading a package or target it
    /// names, failed.
    Loading(String),
}

impl PatternError {
    pub(crate) fn into_message(self) -> String {
        match self {
            PatternError::Parsing(message) | PatternError::Loading(message) => message,
        }
    }
}

/// The TargetPatternPhaseKey options of [`Engine::target_pattern_phase`].
pub struct TargetPatternPhaseOptions<'a> {
    /// The `test` command (TargetPatternPhaseKey.getDetermineTests).
    pub determine_tests: bool,
    /// `--build_tests_only`.
    pub build_tests_only: bool,
    /// `--[no]expand_test_suites` (LoadingOptions, default true).
    pub expand_test_suites: bool,
    /// The command-line TestFilter (`--test_tag_filters`,
    /// `--test_size_filters`, ...; crate::test_command::TestOptions::accepts).
    pub test_filter: &'a dyn Fn(&crate::test_command::TestRuleInfo) -> bool,
}

/// The TargetPatternPhaseValue of `build`/`test`.
#[derive(Debug, Default)]
pub struct TargetPatternPhase {
    /// The targets to analyze and build (test suites expanded).
    pub build: Vec<Label>,
    /// `test`: the tests to run.
    pub tests: Option<Vec<Label>>,
    /// determineTests found tests but the test filters dropped every one
    /// (`All specified test targets were excluded by filters`).
    pub all_excluded: bool,
    /// The test-suite expansion error events, in report order.
    pub errors: Vec<String>,
    /// An expansion of the tests to run failed (TargetPatternPhaseValue
    /// .hasError: `command succeeded, but there were errors parsing the
    /// target pattern`, BuildView.java:764-768).
    pub pattern_error: bool,
    /// An expansion of the targets to build failed (hasPostExpansionError:
    /// `command succeeded, but there were loading phase errors`, :770-775).
    pub post_expansion_error: bool,
    /// `--keep_going`: the loading errors of the wildcard packages skipped,
    /// in loading order (reported before `Target pattern parsing failed.`).
    pub loading_errors: Vec<String>,
    /// `--keep_going`: `Skipping 'PATTERN': MESSAGE` for each pattern that
    /// failed to parse or to evaluate (getTargetsToBuild, :300-335).
    pub skipped: Vec<String>,
}

/// The failures of a keep-going pattern expansion ([`Engine::select_keep_going`]).
#[derive(Default)]
struct PatternFailures {
    loading: Vec<String>,
    skipped: Vec<String>,
}

/// The TestExpansionValues of one target pattern phase: each suite's tests
/// and error flag, and the error events reported (once each).
#[derive(Default)]
struct TestExpansions {
    values: HashMap<Label, (BTreeSet<Label>, bool)>,
    events: Vec<String>,
}

pub struct Engine {
    repositories: Arc<RepositorySet>,
    packages: BTreeMap<PackageId, Package>,
    /// `--loading_phase_threads` (LoadingPhaseThreadsOption.java: "auto",
    /// which expands to the number of available processors; §3 P2 of the
    /// parallel-analysis review): the worker pool size for parallel
    /// package loading. 1 keeps loading fully sequential.
    loading_phase_threads: usize,
    /// M5: nesting depth of `analyze_key` (plan_timings::PlanStage::Analyze
    /// outermost attribution); untouched unless BAZEL_RS_TIMING=1.
    analyze_depth: usize,
    /// Phase-1 package prefetch (crate::prefetch): background workers
    /// loading the packages analysis is about to demand, alive during a
    /// build's phase-1 analysis when `loading_phase_threads` > 1. Units are
    /// committed by `load_package` at the sequential demand point.
    prefetch: Option<crate::prefetch::Prefetcher>,
    /// The rules whose labels phase-1 prefetch already queued (lookups
    /// only).
    prefetch_followed: HashSet<Label>,
    /// The target and host platforms of the first toolchain resolution
    /// while phase-1 prefetch runs (`prefetch_compatible_toolchain`).
    prefetch_platforms: Option<(Arc<PlatformInfo>, Arc<PlatformInfo>)>,
    /// W7: the process exits when the command ends (the CLI sets it):
    /// state a finished command never reads again is left to the OS
    /// instead of being torn down (`set_skip_teardown`).
    skip_teardown: bool,
    /// Opt-in configured-query capture. Ordinary builds retain no query graph.
    cquery_capture: Option<cquery::Capture>,
    /// `.bzl` labels whose buffered top-level print lines the coordinator
    /// already emitted (one emission per evaluated module, review §1.4);
    /// lookups only, never iterated.
    emitted_bzl_prints: HashSet<Label>,
    // N44 hash maps/sets (declared_inputs, source_targets, analyzed_sources)
    // answer lookups only and are never iterated: output order must not
    // depend on the per-process hash seed (tests/loading.rs
    // repeated_processes_produce_byte_identical_outputs).
    /// Per loaded package, every label its non-macro rules name as an
    /// input (Package.java:893-920's assumed input files), built on the
    /// first source-file lookup in the package. Loaded packages never
    /// change, so the index never goes stale (lookups only; no order).
    declared_inputs: HashMap<PackageId, HashSet<Label>>,
    /// Source-file targets already resolved: label -> path. A pure memo of
    /// [`Self::target`]; its filesystem side effects are recorded once
    /// (lookups only).
    source_targets: HashMap<Label, PathBuf>,
    /// Source-file targets analyzed: their analysis (the target and its
    /// visibility's package groups) does not depend on the configuration or
    /// the consumer, so it runs once per label (a ConfiguredTargetKey of an
    /// input file has no configuration), not once per dependency edge
    /// (lookups only).
    analyzed_sources: HashSet<Label>,
    /// Top-level functions of every loaded package (Args `map_each`,
    /// crate::analysis_runtime::LoadedFunctions).
    loaded_functions: Arc<crate::analysis_runtime::LoadedFunctions>,
    generated: BTreeMap<Label, (Label, String)>,
    group_nodes: BTreeMap<Label, Arc<GroupNode>>,
    group_budget: HostBudget,
    /// The configurations of the build: requested targets are analyzed in
    /// the top-level one; dependency edges keep their parent's configuration
    /// except across the exec transition and Starlark rule/attribute
    /// transitions (docs/CONFIGURATION.md).
    configurations: Configurations,
    /// Declared source files that do not exist (path -> label).
    missing_sources: BTreeMap<PathBuf, Label>,
    /// Warnings in emission order, without Bazel's `WARNING: ` prefix (added
    /// by the CLI): filtered target patterns and analysis warnings.
    warnings: Vec<String>,
    /// PlatformValue per platform label and configuration
    /// (PlatformFunction, analysis/platform/PlatformFunction.java:57-126).
    platforms: BTreeMap<ConfiguredTargetKey, Result<Arc<PlatformInfo>, PlatformError>>,
    /// Platforms being resolved (reentrancy guard).
    resolving_platforms: Vec<ConfiguredTargetKey>,
    /// The modules' `register_execution_platforms` patterns with the
    /// repository each is parsed in (crate::repository::RepositorySet::
    /// execution_platform_registrations), once per build.
    module_execution_platforms: Option<Result<Vec<(String, String)>, String>>,
    /// Rules analyzed while resolving a platform (no toolchain context).
    empty_configuration_rules: BTreeSet<ConfiguredTargetKey>,
    /// RegisteredToolchainsValue per configuration, computed when a rule
    /// first requests a toolchain type
    /// (skyframe/toolchains/RegisteredToolchainsFunction.java:64-147).
    registered_toolchains: BTreeMap<ConfigId, Result<Arc<RegisteredToolchains>, ResolutionError>>,
    /// Reentrancy guards of [`Self::registered_toolchains`]: per
    /// configuration whose registered toolchains are being computed, the
    /// registered toolchain target being analyzed (a stack: an exec edge of
    /// a registered target may compute another configuration's).
    computing_toolchains: Vec<ConfiguredTargetKey>,
    /// The cycle report of a registered toolchain that needs toolchain
    /// resolution itself (Self::toolchain_cycle), per configuration.
    toolchain_cycle_reports: BTreeMap<ConfigId, String>,
    /// RegisteredExecutionPlatformsValue per configuration with
    /// `--extra_execution_platforms` (the platforms in registration order,
    /// before the host platform; RegisteredExecutionPlatformsFunction.java:72-157),
    /// computed when a rule of the configuration first resolves toolchains.
    registered_execution_platforms: BTreeMap<ConfigId, Result<ExecutionPlatforms, ResolutionError>>,
    /// Reentrancy guards of [`Self::registered_execution_platforms`]: per
    /// configuration whose execution platforms are being computed, the
    /// registered target being analyzed.
    computing_execution_platforms: Vec<ConfiguredTargetKey>,
    /// The cycle report of a registered execution platform that needs
    /// toolchain resolution itself (Self::execution_platform_cycle), per
    /// configuration.
    execution_platform_cycle_reports: BTreeMap<ConfigId, String>,
    /// The exec configuration of each analyzed target's default exec group
    /// (its parent configuration when the exec transition is a no-op), for
    /// targets with exec-transition dependency edges.
    exec_configurations: BTreeMap<ConfiguredTargetKey, ConfigId>,
    /// Whether a Starlark flag's rule explicitly sets `scope = "target"`
    /// (reset by the exec transition), per flag (computed on first use).
    flag_scopes: BTreeMap<Label, bool>,
    /// The interned exec configuration of each (parent configuration,
    /// execution platform) (Self::exec_configuration): the exec transition
    /// is a function of the parent's interned configuration (never replaced
    /// once interned), the platform and the parent's target-scoped flags
    /// (Self::target_scoped_flags: the Starlark flag labels of the
    /// top-level and the parent configuration, and their `flag_scopes`
    /// entries, fixed once recorded), so it is computed once per pair.
    /// Only successes are memoized; cleared with `configurations` and
    /// `flag_scopes` (Self::set_configuration).
    exec_transitions: HashMap<(ConfigId, Label), ConfigId>,
    /// Applied Starlark transitions: the resulting configuration
    /// (StarlarkTransitionCache; failures are not cached).
    transition_cache: BTreeMap<TransitionCacheKey, (TransitionConfigurations, Vec<String>)>,
    /// StarlarkBuildSettingsDetailsValue per referenced setting label.
    transition_details: BTreeMap<Vec<Label>, Result<TransitionDetails, String>>,
    /// The configuration of each Starlark-transition attribute of each
    /// analyzed target (read by the build loop).
    edge_configs: BTreeMap<ConfiguredTargetKey, BTreeMap<String, TransitionConfigurations>>,
    /// A target requested with a rule transition that changed its
    /// configuration delegates to the key it was analyzed under
    /// (RuleTransitionApplier, analysis/producers/RuleTransitionApplier.java:69-110,
    /// 340-378): requested key -> analyzed key.
    rule_delegations: BTreeMap<ConfiguredTargetKey, ConfiguredTargetKey>,
    /// The requested top-level rule being analyzed when its rule is test
    /// trimmed (not alias-like): its top-level key precedes it in cycle paths
    /// (Self::cycle_error).
    requested_trimmed_root: Option<ConfiguredTargetKey>,
    /// The resolved toolchain context of each toolchain-resolving rule of
    /// the build (UnloadedToolchainContext), read by the build loop.
    toolchain_contexts: BTreeMap<ConfiguredTargetKey, UnloadedToolchainContext>,
    /// The other exec groups of each toolchain-resolving rule's
    /// ToolchainCollection (docs/EXEC_GROUPS.md; the default `test` group of
    /// a Starlark test rule, docs/TEST_FLAGS.md).
    exec_group_contexts: BTreeMap<ConfiguredTargetKey, crate::exec_groups::TargetExecGroups>,
    /// The exec configuration of each named exec group with
    /// `config.exec(exec_group = NAME)` dependency edges.
    group_exec_configurations: BTreeMap<(ConfiguredTargetKey, String), ConfigId>,
    /// The exec group, execution platform and exec properties of each
    /// planned Starlark action (docs/EXEC_GROUPS.md).
    action_executions: BTreeMap<String, crate::exec_groups::ActionExecution>,
    /// Phase-2 state of the current build (Self::plan_pending).
    action_planning: Option<ActionPlanning>,
    /// Targets analyzed as toolchain implementations, with their parent's
    /// execution platform (the PlatformInfo label), which their own
    /// resolution forces (ToolchainContextUtil.java:136-141; docs/TOOLCHAINS.md).
    toolchain_implementations: BTreeMap<ConfiguredTargetKey, Label>,
    /// Duplicate-label attribute errors of Starlark rules, reported with
    /// their other prerequisite errors (Self::starlark_prerequisite_errors).
    deferred_duplicates: BTreeMap<ConfiguredTargetKey, Vec<String>>,
    /// Source directories used as file targets (path -> label).
    /// Several labels (a directory and links to it, N28d) may name one.
    source_directories: BTreeMap<PathBuf, BTreeSet<Label>>,
    /// The runfiles options of builds (N23f, crate::runfiles_tree).
    runfiles_options: crate::runfiles_tree::RunfilesOptions,
    /// The requested top-level output groups of builds
    /// (`--output_groups`/`--run_validations`, N56,
    /// crate::output_group_info::determine_output_groups).
    output_selection: OutputSelection,
    /// Every aspect definition the invocation can reach (N58,
    /// crate::aspects): exported by loaded `.bzl` modules, or the native
    /// ValidateTarget. Keyed by identity (`//pkg:def.bzl%name`).
    aspects: crate::aspects::AspectRegistry,
    /// The aspects requested by `--aspects`, as identities, with
    /// `--experimental_use_validation_aspect`'s ValidateTarget appended
    /// (BuildRequest.getAspects, BuildRequest.java:421-427).
    requested_aspects: Vec<String>,
    /// `--aspects_parameters` values (AspectParameters).
    aspect_parameters: BTreeMap<String, String>,
    /// The select()-configured custom rules produced by phase 1, shared with
    /// the plan. Both aspect dependency analysis and implementation planning
    /// must walk these same resolved attributes, regardless of pending flushes.
    configured_rules: BTreeMap<ConfiguredTargetKey, Arc<Rule>>,
    /// The configuration of each label attribute of each aspect
    /// application (base configured target key, aspect identity), computed
    /// when phase 1 analyzes the application's own attribute dependencies
    /// (Self::aspect_attribute_configurations) and read by its planning.
    aspect_attribute_configs:
        BTreeMap<(ConfiguredTargetKey, String), Arc<BTreeMap<String, TransitionConfigurations>>>,
    /// The aspect's own toolchain contexts per (configuration, aspect
    /// identity) (Self::aspect_toolchain_contexts), resolved by phase 1
    /// and read by the application's planning.
    aspect_toolchains: BTreeMap<(ConfigId, String), Arc<AspectToolchains>>,
    /// Set while an aspect's toolchain context is resolved: its
    /// ToolchainException is reported as is (AspectFunction's
    /// AspectCreationException), not as a target's `While resolving
    /// toolchains for target` error (Self::toolchain_error).
    resolving_aspect_toolchains: bool,
    /// Whether the building command runs tests (BuildRequest.shouldRunTests
    /// keeps the `default` group for failure reporting).
    tests_requested: bool,
    /// The last planned build's reported output groups (label, group,
    /// range of its `requested` artifacts), for the build event plan.
    bep_groups: Vec<(Label, String, usize, usize)>,
    /// The target of a `run` build and what it runs (Self::build_for_run).
    run_requests: BTreeSet<Label>,
    /// Whether the run requests need their runfiles tree (`run`), or only
    /// the runfiles artifacts (`test` stages them itself).
    run_trees: bool,
    run_plans: BTreeMap<Label, RunPlan>,
    /// The ids of the actions whose planning evaluated a build-info
    /// translation callback in the current build (snapshot blockers when
    /// the plan keeps them, Self::plan_request).
    build_info_actions: BTreeSet<String>,
    /// Errors reported during a build that do not fail it, without the
    /// `ERROR: ` prefix (runfiles prefix conflicts under ConflictPolicy.ERROR).
    errors: Vec<String>,
    /// `--workspace_status_command`/`--embed_label` of later builds
    /// (WorkspaceStatusAction.Options; N63).
    workspace_status: crate::workspace_status::WorkspaceStatusOptions,
    /// The one computed workspace status of this invocation (N63
    /// follow-up): computed exactly once, behind a OnceLock so parallel
    /// planning (N62) can never run the status script twice.
    workspace_status_once: std::sync::OnceLock<crate::workspace_status::ComputedWorkspaceStatus>,
    /// Whether this invocation executes (--nobuild stops before the
    /// execution phase, BuildRequestOptions.performExecutionPhase
    /// :154-160; the workspace status action is an execution-phase
    /// artifact, so it is not planned under --nobuild).
    execute_build: bool,
    /// `--keep_going`/`-k` (KeepGoingOption, docs/KEEP_GOING.md): loading
    /// and analysis failures skip the targets they fail instead of failing
    /// the build.
    keep_going: bool,
    /// The keep-going bookkeeping of the current build ([`keep_going::State`]).
    keep_going_state: keep_going::State,
    /// The keep-going outcome of the last planned build, taken by
    /// [`Self::plan_request`].
    keep_going_plan: Option<crate::request::KeepGoingPlan>,
    /// While a keep-going target pattern phase expands a wildcard: the
    /// loading errors of its packages, each skipped (`Some`), instead of
    /// failing the pattern (`None`).
    pattern_load_errors: Option<Vec<String>>,
}

impl Engine {
    pub fn new(root: PathBuf) -> Self {
        Self::with_repositories(Arc::new(RepositorySet::new(root)))
    }

    pub fn with_repositories(repositories: Arc<RepositorySet>) -> Self {
        Self {
            repositories,
            packages: BTreeMap::new(),
            emitted_bzl_prints: HashSet::new(),
            loading_phase_threads: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
            analyze_depth: 0,
            prefetch: None,
            prefetch_followed: HashSet::new(),
            prefetch_platforms: None,
            skip_teardown: false,
            cquery_capture: None,
            declared_inputs: HashMap::new(),
            source_targets: HashMap::new(),
            analyzed_sources: HashSet::new(),
            loaded_functions: Arc::default(),
            generated: BTreeMap::new(),
            group_nodes: BTreeMap::new(),
            group_budget: HostBudget::default(),
            configurations: Configurations::default(),
            missing_sources: BTreeMap::new(),
            warnings: Vec::new(),
            platforms: BTreeMap::new(),
            resolving_platforms: Vec::new(),
            module_execution_platforms: None,
            empty_configuration_rules: BTreeSet::new(),
            registered_toolchains: BTreeMap::new(),
            computing_toolchains: Vec::new(),
            toolchain_cycle_reports: BTreeMap::new(),
            registered_execution_platforms: BTreeMap::new(),
            computing_execution_platforms: Vec::new(),
            execution_platform_cycle_reports: BTreeMap::new(),
            exec_configurations: BTreeMap::new(),
            flag_scopes: BTreeMap::new(),
            exec_transitions: HashMap::new(),
            transition_cache: BTreeMap::new(),
            transition_details: BTreeMap::new(),
            edge_configs: BTreeMap::new(),
            rule_delegations: BTreeMap::new(),
            requested_trimmed_root: None,
            toolchain_contexts: BTreeMap::new(),
            exec_group_contexts: BTreeMap::new(),
            group_exec_configurations: BTreeMap::new(),
            action_executions: BTreeMap::new(),
            action_planning: None,
            toolchain_implementations: BTreeMap::new(),
            deferred_duplicates: BTreeMap::new(),
            source_directories: BTreeMap::new(),
            runfiles_options: crate::runfiles_tree::RunfilesOptions::default(),
            output_selection: OutputSelection::default(),
            aspects: crate::aspects::AspectRegistry::default(),
            requested_aspects: Vec::new(),
            aspect_parameters: BTreeMap::new(),
            configured_rules: BTreeMap::new(),
            aspect_attribute_configs: BTreeMap::new(),
            aspect_toolchains: BTreeMap::new(),
            resolving_aspect_toolchains: false,
            tests_requested: false,
            bep_groups: Vec::new(),
            run_requests: BTreeSet::new(),
            run_trees: true,
            run_plans: BTreeMap::new(),
            build_info_actions: BTreeSet::new(),
            errors: Vec::new(),
            workspace_status: crate::workspace_status::WorkspaceStatusOptions::default(),
            workspace_status_once: std::sync::OnceLock::new(),
            execute_build: true,
            keep_going: false,
            keep_going_state: keep_going::State::default(),
            keep_going_plan: None,
            pattern_load_errors: None,
        }
    }

    /// A label-typed command-line option, converted with the main
    /// repository mapping (CoreOptionConverters.convertOptionsLabel,
    /// options/CoreOptionConverters.java:351-357).
    pub fn parse_option_label(&self, text: &str) -> Result<Label, String> {
        self.repositories.parse(text, "", "")
    }

    /// The runfiles options of later builds (`--build_runfile_links`, ...).
    pub fn set_runfiles_options(&mut self, options: crate::runfiles_tree::RunfilesOptions) {
        self.runfiles_options = options;
    }

    /// The workspace status options of later builds
    /// (`--workspace_status_command`, `--embed_label`;
    /// WorkspaceStatusAction.Options, N63).
    pub fn set_workspace_status(
        &mut self,
        options: crate::workspace_status::WorkspaceStatusOptions,
    ) {
        self.workspace_status = options;
    }

    /// Whether later builds execute their actions (`--nobuild` stops
    /// before the execution phase: BuildRequestOptions
    /// .performExecutionPhase :154-160, ExecutionTool.executeBuild
    /// :364-366). Under --nobuild the workspace status action is not
    /// planned: it is an execution-phase artifact (BuildView adds it to
    /// artifactsToBuild only on the execution path).
    pub fn set_execute_build(&mut self, execute: bool) {
        self.execute_build = execute;
    }

    /// `--keep_going` (docs/KEEP_GOING.md): later target pattern phases skip
    /// the patterns and packages that fail to load, later builds the
    /// targets whose analysis fails.
    pub fn set_keep_going(&mut self, keep_going: bool) {
        self.keep_going = keep_going;
    }

    /// Whether later builds execute (see `set_execute_build`).
    pub fn execute_build(&self) -> bool {
        self.execute_build
    }

    /// W7: the process exits when this command ends (the CLI, never a
    /// library user that keeps building): a `--nobuild` build leaves its
    /// analysis and planning maps to the OS instead of tearing them down.
    pub fn set_skip_teardown(&mut self, skip: bool) {
        self.skip_teardown = skip;
    }

    /// Whether this engine is left to the OS when its command ends (W7,
    /// [`Self::set_skip_teardown`]): a `--nobuild` build of the CLI, whose
    /// planning maps were forgotten at the end of planning. The CLI then
    /// never drops the engine either; an executing build's engine is freed
    /// while the build executes (L2-W4).
    pub fn skips_teardown(&self) -> bool {
        self.skip_teardown && !self.execute_build
    }

    /// The one workspace status computation of this invocation (N63
    /// follow-up): OnceLock-guarded so it runs exactly once even when
    /// parallel planning (N62) reaches it from several threads.
    fn workspace_status_computed(
        &self,
        client_env: &BTreeMap<String, std::ffi::OsString>,
    ) -> &crate::workspace_status::ComputedWorkspaceStatus {
        self.workspace_status_once.get_or_init(|| {
            match crate::workspace_status::compute(
                &self.workspace_status,
                self.repositories.main_root(),
                client_env,
                crate::workspace_status::current_time_millis(client_env),
            ) {
                Ok(status) => crate::workspace_status::ComputedWorkspaceStatus::Ok(status),
                Err(error) => crate::workspace_status::ComputedWorkspaceStatus::Failed(error),
            }
        })
    }

    /// The requested top-level output groups of later builds
    /// (`--output_groups` tokens with their `+`/`-` prefixes and
    /// `--run_validations`), BuildRequest.getTopLevelArtifactContext's
    /// output-group half.
    pub fn set_output_selection(&mut self, selection: OutputSelection) {
        self.output_selection = selection;
    }

    /// Whether the building command also runs the tests it builds
    /// (BuildRequest.shouldRunTests: `bazel test`).
    pub fn set_tests_requested(&mut self, tests: bool) {
        self.tests_requested = tests;
    }

    /// The effective top-level output groups of a build
    /// (OutputGroupInfo.determineOutputGroups over `--output_groups`,
    /// `--run_validations`, `--experimental_use_validation_aspect` and the
    /// command).
    fn selected_output_groups(&self) -> BTreeSet<String> {
        crate::output_group_info::determine_output_groups(
            &self.output_selection.groups,
            self.output_selection.run_validations,
            self.output_selection.use_validation_aspect,
            self.tests_requested,
        )
    }

    /// The top-level aspects of builds (BuildRequest.getAspects,
    /// BuildRequest.java:421-427): the `--aspects` list, plus the native
    /// ValidateTarget when validation actions run through the aspect
    /// (`--experimental_use_validation_aspect`, ValidationMode.ASPECT) and
    /// it was not already requested.
    pub fn set_requested_aspects(
        &mut self,
        aspects: Vec<String>,
        parameters: BTreeMap<String, String>,
    ) {
        self.requested_aspects = aspects;
        self.aspect_parameters = parameters;
    }

    /// `--experimental_use_validation_aspect` (ValidationMode.ASPECT).
    pub fn set_use_validation_aspect(&mut self, enabled: bool) {
        self.output_selection.use_validation_aspect = enabled;
    }

    /// The effective top-level aspect identities, ValidateTarget appended
    /// when the validation aspect mode is on.
    fn effective_aspects(&self) -> Vec<String> {
        let mut aspects = self.requested_aspects.clone();
        if self.output_selection.run_validations
            && self.output_selection.use_validation_aspect
            && !aspects
                .iter()
                .any(|aspect| aspect == crate::aspects::VALIDATION_ASPECT_IDENTITY)
        {
            aspects.push(crate::aspects::VALIDATION_ASPECT_IDENTITY.to_owned());
        }
        aspects
    }

    /// The aspects of an `attr.*(aspects = [...])` list of `rule`, loaded,
    /// each with the parameters `rule` gives it
    /// (crate::aspects::AspectDefinition::for_attribute_of).
    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<std::sync::Arc<crate::aspects::AspectDefinition>>, String> {
        let mut aspects = Vec::with_capacity(identities.len());
        for identity in identities {
            let definition = self
                .aspect_definition(identity, true)?
                .ok_or_else(|| format!("Aspect '{identity}' is unknown"))?;
            aspects.push(match definition.for_attribute_of(rule) {
                Some(parameterized) => std::sync::Arc::new(parameterized),
                None => definition,
            });
        }
        Ok(aspects)
    }

    /// [`Self::attribute_aspects`] over the definitions loaded already (an
    /// unloaded one is skipped, as the registry lookup always did).
    fn loaded_attribute_aspects(
        &self,
        rule: &Rule,
        identities: &[String],
    ) -> Vec<std::sync::Arc<crate::aspects::AspectDefinition>> {
        identities
            .iter()
            .filter_map(|identity| self.aspects.get(identity))
            .map(|definition| match definition.for_attribute_of(rule) {
                Some(parameterized) => std::sync::Arc::new(parameterized),
                None => definition,
            })
            .collect()
    }

    /// The top-level aspects as LoadAspectsFunction.getTopLevelAspects
    /// builds them (skyframe/LoadAspectsFunction.java:82-126): an
    /// AspectsList over the `--aspects` classes in order, each Starlark
    /// aspect preceded by its required aspects (AspectsList.Builder,
    /// required first, each aspect once; a repeated explicit aspect fails),
    /// a native class put once.
    fn top_level_aspects_list(
        &mut self,
    ) -> Result<Vec<std::sync::Arc<crate::aspects::AspectDefinition>>, String> {
        let mut list = crate::rule_declarations::AspectsListBuilder::default();
        for identity in self.effective_aspects() {
            if identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
                // Builder.addAspect(NativeAspectClass): a plain put.
                if !list.contains(&identity) {
                    list.add(&identity, None)
                        .map_err(|error| error.to_string())?;
                }
                continue;
            }
            self.add_top_level_aspect(&mut list, &identity, None, 0)?;
        }
        let mut definitions = Vec::new();
        for identity in list.build() {
            let definition = self
                .aspect_definition(&identity, true)?
                .ok_or_else(|| format!("Aspect '{identity}' is unknown"))?;
            if identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
                // Native aspects ignore top-level parameters
                // (NativeAspectDetails.getTopLevelAspect).
                definitions.push(definition);
                continue;
            }
            // AspectsList.buildAspects: each Starlark aspect with its
            // top-level parameters (StarlarkDefinedAspect.
            // extractTopLevelParameters, :345-390).
            let mut top_level = (*definition).clone();
            top_level.parameters = self.top_level_aspect_parameters(&definition)?;
            top_level.top_level = true;
            top_level.refresh_descriptor();
            definitions.push(std::sync::Arc::new(top_level));
        }
        Ok(definitions)
    }

    /// The `--aspects_parameters` values of `aspect`'s parameter attributes
    /// (public, of a scalar or string-list type), typed by their attribute
    /// (StarlarkDefinedAspect.extractTopLevelParameters: boolean, int and
    /// string parsing, :345-416).
    fn top_level_aspect_parameters(
        &self,
        aspect: &crate::aspects::AspectDefinition,
    ) -> Result<BTreeMap<String, crate::custom_rule::AttrValue>, String> {
        use crate::custom_rule::{AttrKind, AttrValue};
        let mut parameters = BTreeMap::new();
        for (parameter, raw) in &self.aspect_parameters {
            let Some((_, spec)) = aspect.declaration.attrs.iter().find(|(attr, spec)| {
                attr == parameter && crate::aspects::is_parameter_attribute(attr, spec)
            }) else {
                continue;
            };
            let value = match spec.kind {
                AttrKind::Bool => match raw.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "t" | "y" => AttrValue::Bool(true),
                    "false" | "0" | "no" | "f" | "n" => AttrValue::Bool(false),
                    _ => {
                        return Err(format!(
                            "{}: expected value of type 'bool' for attribute '{parameter}' but got '{raw}'",
                            aspect.identity
                        ));
                    }
                },
                AttrKind::Int => AttrValue::Int(raw.parse().map_err(|_| {
                    format!(
                        "{}: expected value of type 'int' for attribute '{parameter}' but got '{raw}'",
                        aspect.identity
                    )
                })?),
                AttrKind::StringList => {
                    AttrValue::Strings(raw.split(',').map(str::to_owned).collect::<Vec<_>>())
                }
                _ => AttrValue::String(raw.clone()),
            };
            parameters.insert(parameter.clone(), value);
        }
        Ok(parameters)
    }

    /// AspectsList.Builder.addAspect(StarlarkAspect, requiredByAspect)
    /// over a loaded definition: its `requires` first (recursively), then
    /// the aspect itself. A `--aspects` class loads public-only like a
    /// load(); a required aspect is the value the requiring aspect holds.
    fn add_top_level_aspect(
        &mut self,
        list: &mut crate::rule_declarations::AspectsListBuilder,
        identity: &str,
        required_by: Option<&str>,
        depth: usize,
    ) -> Result<(), String> {
        let definition = self
            .aspect_definition(identity, required_by.is_some())?
            .ok_or_else(|| format!("Aspect '{identity}' is unknown"))?;
        if depth < 256 {
            for required in &definition.declaration.requires {
                self.add_top_level_aspect(list, required, Some(identity), depth + 1)?;
            }
        }
        list.add(identity, required_by)
            .map_err(|error| error.to_string())
    }

    /// The aspect definition of `identity`, loading its `.bzl` export on
    /// first use (LoadAspectsFunction/AspectFunction.loadAspectFromBzl).
    /// `None` resolves nothing (an unknown identity is the caller's error).
    /// `allow_private` reads a private top-level binding: legal only for the
    /// attr(aspects=...) identities a rule class captured from its own
    /// `.bzl` (Bazel keeps the aspect value; it never re-resolves by load());
    /// top-level `--aspects` stay public-only like a load().
    fn aspect_definition(
        &mut self,
        identity: &str,
        allow_private: bool,
    ) -> Result<Option<std::sync::Arc<crate::aspects::AspectDefinition>>, String> {
        if let Some(definition) = self.aspects.get(identity) {
            return Ok(Some(definition));
        }
        // The native ValidateTarget (rules/core/ValidateTarget.java): a
        // non-recursive aspect that promotes `_validation` outputs to
        // `_validation_top_level`. Its application is computed natively;
        // the implementation slot stays an unused marker.
        if identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
            let declaration = crate::rule_declarations::AspectDeclaration {
                attr_aspects: Some(Vec::new()),
                ..Default::default()
            };
            self.aspects.insert(crate::aspects::AspectDefinition {
                identity: identity.to_owned(),
                definition_label: crate::label::Label::parse_canonical("//:validate.bzl")
                    .expect("static label parses"),
                exported_name: identity.to_owned(),
                declaration,
                value: starlark::environment::Module::with_temp_heap(|module| {
                    module.set("__impl", starlark::values::Value::new_none());
                    module
                        .freeze()
                        .expect("None freezes")
                        .get("__impl")
                        .expect("__impl was set")
                }),
                parameters: BTreeMap::new(),
                top_level: false,
                descriptor: identity.to_owned(),
            });
            return Ok(self.aspects.get(identity));
        }
        // StarlarkAspectClass: `{extension label}%{exported name}`, split at
        // the LAST '%' of the identity.
        let Some((extension, name)) = identity.rsplit_once('%') else {
            return Ok(None);
        };
        let label = self
            .repositories
            .parse(extension, "", "")
            .map_err(|error| format!("Invalid aspect '{identity}': {error}"))?;
        // A BUILD-phase load of the shared `.bzl` cache (its invariant, W6
        // review F1): a phase-1 prefetch worker may have evaluated the
        // module already, for a package the build may never commit. The
        // load's print events replay here like a load unit's at its commit
        // — the lines of a module it evaluated, or the stored lines of a
        // cache hit not emitted yet — also when the load fails, so the
        // lines print once, at this sequential point, whoever evaluated the
        // module first.
        let prints = crate::loading::LoadPrints::default();
        let loaded = crate::loading::load_shared_definition_any_in(
            self.repositories.clone(),
            &label,
            name,
            &self.repositories.build_phase_globals().1,
            !allow_private,
            &prints,
        );
        self.emit_load_prints(prints.take());
        let loaded = loaded?;
        let Some(aspect) = crate::rule_declarations::Aspect::from_value(loaded.value.value())
        else {
            return Err(format!("{name} from {label} is not an aspect"));
        };
        let definition = crate::aspects::AspectDefinition {
            identity: identity.to_owned(),
            definition_label: label,
            exported_name: name.to_owned(),
            declaration: aspect.declaration.clone(),
            value: loaded.value,
            parameters: BTreeMap::new(),
            top_level: false,
            descriptor: identity.to_owned(),
        };
        self.aspects.insert(definition);
        Ok(self.aspects.get(identity))
    }

    /// Analyze builds in `configuration` (default: the default configuration).
    /// `--loading_phase_threads` (LoadingPhaseThreadsOption.java: "auto"
    /// expands to the available processors; P2 of the parallel-analysis
    /// review). Must be positive.
    pub fn set_loading_phase_threads(&mut self, threads: usize) {
        self.loading_phase_threads = threads.max(1);
    }

    pub fn set_configuration(&mut self, configuration: Configuration) {
        self.configurations = Configurations::new(configuration);
        self.platforms.clear();
        self.registered_toolchains.clear();
        self.registered_execution_platforms.clear();
        self.flag_scopes.clear();
        self.exec_transitions.clear();
        self.transition_cache.clear();
        self.transition_details.clear();
    }

    /// Drain the warnings reported so far (also after a failed build, as
    /// Bazel prints warnings before the error that stops it).
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Errors reported by a build that did not fail it (printed with
    /// Bazel's `ERROR: ` prefix by the CLI).
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    fn ignored(&self, repository: &str) -> Result<Arc<IgnoredSubdirectories>, String> {
        self.repositories.ignored_subdirectories(repository)
    }

    fn load_package(&mut self, package: &PackageId) -> Result<(), String> {
        // A label whose apparent repository the main repository does not see
        // (crate::label::unknown_repository_name) names no loadable package:
        // RepositoryMapping's unresolved name (cmdline/RepositoryName.java),
        // reported like PackageLookupFunction's repository error.
        if let Some(message) = crate::label::unresolvable_package_message(package) {
            return Err(message);
        }
        if !self.packages.contains_key(package) {
            // Phase-1 prefetch (crate::prefetch): a unit a background
            // worker loaded is committed here, at the sequential demand
            // point, exactly like an inline load; a sentinel unit re-runs
            // inline in `commit_load_unit` (P2).
            let prefetched = self
                .prefetch
                .as_ref()
                .and_then(|prefetch| prefetch.take(package));
            let inline = prefetched.as_ref().is_none_or(|unit| {
                unit.result
                    .as_ref()
                    .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error))
            });
            let unit = match prefetched {
                Some(unit) => unit,
                None => self.load_package_attempt(package),
            };
            let result = self.commit_load_unit(package, unit);
            // An inline load may have prepared an extension group or
            // materialized a root a worker's unit was blocked on.
            if inline && let Some(prefetch) = &self.prefetch {
                let repositories = &self.repositories;
                prefetch.retry(|blocked| repositories.sentinel_ready(blocked));
            }
            return result;
        }
        Ok(())
    }

    /// Commit one finished load attempt at its commit slot (P2: the
    /// commit of a worker's result, in pattern order): the unit's
    /// buffered print lines are emitted — each not-yet-emitted `.bzl`
    /// module's in the package's load-DFS order, then the BUILD file's —
    /// and the package commits (validate_output_directory -> generated
    /// inserts -> packages.insert). On the no-demand sentinel (a worker,
    /// review §1.2) the `.bzl` lines are emitted now — those modules are
    /// cached and never evaluate again — the BUILD lines are dropped
    /// with the aborted evaluation, and the unit re-runs inline, where
    /// fetching is allowed, reproducing the sequential output: the fetch
    /// output lands exactly where the sequential load demanded the
    /// repository.
    fn commit_load_unit(
        &mut self,
        package: &PackageId,
        unit: package::LoadUnit,
    ) -> Result<(), String> {
        let package::LoadUnit {
            result,
            prints: unit_prints,
            roots,
            observations,
        } = unit;
        // The roots this load obtained on a worker count as materialized
        // from its commit on, as at the sequential engine's load (W6
        // review F2), and what it observed reaches the analysis snapshot's
        // recording here (W6R); an aborted unit's are the inline re-run's
        // to note and observe.
        if !result
            .as_ref()
            .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error))
        {
            self.repositories.note_committed_roots(&roots);
            crate::observed_fs::commit(observations);
        }
        let (rules, prints) = match result {
            Ok(rules) => (rules, unit_prints),
            Err(error) => {
                if crate::repository_demand::is_inline_rerun_sentinel(&error) {
                    // An inline-rerun sentinel: the no-demand guard (a
                    // worker, review §1.2) or a parallel `.bzl` wait-cycle
                    // abort (N62). The module events (evaluated lines and
                    // cache-hit markers) are emitted now — the modules are
                    // cached and never evaluate again — while the BUILD
                    // lines are dropped with the aborted evaluation; the
                    // inline re-run reproduces the sequential output, fetch
                    // output and genuine cycle messages included.
                    let module_events = unit_prints
                        .into_iter()
                        .filter(|event| !matches!(event, crate::loading::LoadPrintEvent::Build(_)))
                        .collect();
                    self.emit_load_prints(module_events);
                    let mut retried = self.load_package_attempt(package);
                    self.repositories.note_committed_roots(&retried.roots);
                    crate::observed_fs::commit(std::mem::take(&mut retried.observations));
                    return match retried.result {
                        Ok(rules) => {
                            self.emit_load_prints(retried.prints);
                            self.commit_loaded_package(package, rules)
                        }
                        Err(error) => {
                            self.emit_load_prints(retried.prints);
                            // A wait-cycle abort on the inline re-run is
                            // not spurious: nothing is left to wait for,
                            // so the demand is a genuine cycle (an
                            // extension loading the very module being
                            // evaluated). Report it as the load cycle it
                            // is — the raw sentinel never reaches the
                            // user (N62 follow-up).
                            Err(if crate::repository_demand::is_parallel_bzl_cycle(&error) {
                                format!(
                                    "load cycle: //{} (a module demanded while already being evaluated)",
                                    package.package
                                )
                            } else {
                                error
                            })
                        }
                    };
                }
                self.emit_load_prints(unit_prints);
                return Err(error);
            }
        };
        self.emit_load_prints(prints);
        self.commit_loaded_package(package, rules)
    }

    /// Load the packages of one `//dir/...` pattern expansion (P2,
    /// review §3): with more than one package and thread, the loads run
    /// on a scoped pool of `--loading_phase_threads` workers — 16 MiB
    /// stacks (§1.6), each in no-demand mode with its own Starlark
    /// Module/heap/Evaluator, buffered prints and fresh thread-locals —
    /// and the coordinator commits in pattern (sequential) order, the
    /// first error in that order winning and later results discarded.
    /// A guard-aborted unit re-runs inline at its commit slot. With one
    /// thread (or one package) this is exactly the sequential P1 path.
    fn load_packages_parallel(
        &mut self,
        ids: &[PackageId],
        mut after_commit: impl FnMut(&mut Self, &PackageId) -> Result<(), String>,
    ) -> Result<(), String> {
        let pending: Vec<PackageId> = ids
            .iter()
            .filter(|id| !self.packages.contains_key(*id))
            .cloned()
            .collect();
        if pending.len() <= 1 || self.loading_phase_threads <= 1 {
            for id in ids {
                if let Err(error) = self.load_package(id) {
                    self.skip_package_error(error)?;
                    continue;
                }
                after_commit(self, id)?;
            }
            return Ok(());
        }
        // Resolve the module graph on the coordinator before any worker
        // starts (review follow-up): resolution is coordinator-only work,
        // and a lazily-resolving worker could fetch outside the guard.
        // Already resolved, this is a cheap OnceLock read. So is the
        // repository store's path, whose first computation the analysis
        // snapshot records (W6R: never in a worker's unit).
        self.repositories.resolve_graph()?;
        let _ = self.repositories.source_cache_path();
        let workers = self.loading_phase_threads.min(pending.len());
        let slots: Vec<std::sync::Mutex<Option<package::LoadUnit>>> = pending
            .iter()
            .map(|_| std::sync::Mutex::new(None))
            .collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let repositories = self.repositories.clone();
        // Shared by reference: every worker closure captures these Copy
        // references (the scope keeps them alive).
        let pending = &pending;
        let slots = &slots;
        let next = &next;
        let repositories = &repositories;
        // Hidden test hook (review §5): reproducible per-package jitter
        // shuffles worker completion order without changing results.
        let jitter_millis = loading_jitter_millis();
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for worker in 0..workers {
                let jitter_seed = worker as u64;
                let spawn = std::thread::Builder::new()
                    .stack_size(LOADING_WORKER_STACK)
                    .spawn_scoped(scope, move || {
                        let _ = jitter_seed;
                        loop {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(id) = pending.get(index) else {
                                break;
                            };
                            if jitter_millis > 0 {
                                let bound = jitter_millis + 1;
                                let roll = loading_jitter_roll(index as u64, worker as u64);
                                std::thread::sleep(std::time::Duration::from_millis(roll % bound));
                            }
                            // Workers never fetch (review §1.2): asking for
                            // an unfetched repository returns the sentinel;
                            // the coordinator re-runs the unit inline at
                            // its commit slot. Step C: reuse-only demand is
                            // enabled — a warm publication whose recorded
                            // inputs validate may be served on the worker
                            // (pure reads, published into the demand slot);
                            // a miss still aborts with the sentinel.
                            let guard = crate::repository_demand::no_demand();
                            let reuse = crate::repository_demand::reuse_only();
                            let unit = package::evaluate_load_unit(repositories.clone(), id);
                            drop(reuse);
                            drop(guard);
                            *slots[index].lock().unwrap() = Some(unit);
                        }
                    });
                match spawn {
                    Ok(handle) => handles.push(handle),
                    Err(error) => {
                        return Err(format!("cannot spawn package-loading worker: {error}"));
                    }
                }
            }
            for handle in handles {
                if handle.join().is_err() {
                    return Err("parallel package loading worker panicked".to_owned());
                }
            }
            Ok(())
        })?;
        // Commit in pattern order: the same sequence of commits — and of
        // printed output — as the sequential engine, whatever order the
        // workers finished in. Each package's follow-up (its targets)
        // runs right after its commit, interleaved exactly like the
        // sequential path, so warnings and first errors cannot move.
        // Packages already loaded before this call commit nothing but
        // still run their follow-up, like the sequential path.
        let mut dispatched = 0;
        for id in ids {
            if !self.packages.contains_key(id) {
                let unit = slots[dispatched]
                    .lock()
                    .unwrap()
                    .take()
                    .expect("every dispatched package finished");
                dispatched += 1;
                if let Err(error) = self.commit_load_unit(id, unit) {
                    self.skip_package_error(error)?;
                    continue;
                }
            }
            after_commit(self, id)?;
        }
        Ok(())
    }

    /// A package of a wildcard failed to load: under a keep-going target
    /// pattern phase its error is recorded and the package skipped (its
    /// targets are not part of the pattern, docs/KEEP_GOING.md), otherwise
    /// the error fails the pattern.
    fn skip_package_error(&mut self, error: String) -> Result<(), String> {
        match &mut self.pattern_load_errors {
            Some(errors) if !crate::repository_demand::is_inline_rerun_sentinel(&error) => {
                errors.push(error);
                Ok(())
            }
            _ => Err(error),
        }
    }

    /// One package load attempt (P1 loading commit order): the BUILD file
    /// and its `.bzl` graph evaluate with print output buffered into the
    /// returned unit; nothing is emitted here. The caller emits the
    /// lines at the unit's commit slot. `BAZEL_RS_LOADING_NO_DEMAND`
    /// (hidden test hook, review §5) runs the attempt under the
    /// no-demand guard, exercising the inline fallback a P2 worker
    /// exercises; a warm load demands nothing and is unaffected.
    fn load_package_attempt(&mut self, package: &PackageId) -> package::LoadUnit {
        let guard = std::env::var_os("BAZEL_RS_LOADING_NO_DEMAND")
            .is_some()
            .then(crate::repository_demand::no_demand);
        let unit = package::evaluate_load_unit(self.repositories.clone(), package);
        drop(guard);
        unit
    }

    /// Commit a loaded package (P2's commit order):
    /// validate_output_directory -> generated inserts -> packages.insert.
    fn commit_loaded_package(&mut self, package: &PackageId, rules: Package) -> Result<(), String> {
        crate::progress::package_loaded();
        let ignored = self.ignored(&package.repository)?;
        for rule in rules.values() {
            for out in &rule.outs {
                self.validate_output_directory(package, out, &ignored)?;
                let output = rule.label.with_name(out)?;
                self.generated
                    .insert(output, (rule.label.clone(), out.clone()));
            }
        }
        // Every rule of a package shares its load graph's functions.
        if let Some(custom) = rules.values().find_map(|rule| rule.custom.as_ref()) {
            self.loaded_functions.register(&custom.top_level_functions);
        }
        // Phase-1 prefetch: a committed package is never loaded again; the
        // targets wanted of it are followed.
        if let Some(prefetch) = &self.prefetch {
            prefetch.committed(package, &rules);
        }
        self.packages.insert(package.to_owned(), rules);
        Ok(())
    }

    /// Emit one load unit's buffered print events (the coordinator's
    /// replay, review §1.4 and the P2 follow-up): BUILD lines once; a
    /// module's lines — its own block, or the ones stored with its cache
    /// entry at a hit marker — only if that module's prints were not
    /// emitted yet (the emitted-`.bzl` set). The event order is the
    /// loader's DFS, so success and failure commits replay exactly the
    /// sequential loader's output. A module's lines are contiguous in
    /// the stream (one push per evaluation): the whole block prints or
    /// the whole block is skipped.
    fn emit_load_prints(&mut self, events: Vec<crate::loading::LoadPrintEvent>) {
        use crate::loading::LoadPrintEvent;
        let mut block: Option<(Label, bool)> = None;
        for event in events {
            match event {
                LoadPrintEvent::Build(line) => {
                    block = None;
                    crate::ui::note(&line);
                }
                LoadPrintEvent::Module { label, line } => {
                    let emit = match &block {
                        Some((current, emit)) if current == &label => *emit,
                        _ => self.emitted_bzl_prints.insert(label.clone()),
                    };
                    if emit {
                        crate::ui::note(&line);
                    }
                    block = Some((label, emit));
                }
                LoadPrintEvent::Cached(label) => {
                    block = None;
                    if self.emitted_bzl_prints.insert(label.clone()) {
                        for line in self.repositories.build_bzl_cache().prints(&label) {
                            crate::ui::note(&line);
                        }
                    }
                }
            }
        }
    }

    fn validate_output_directory(
        &self,
        package: &PackageId,
        out: &str,
        ignored: &IgnoredSubdirectories,
    ) -> Result<(), String> {
        let mut directory = self
            .repositories
            .root(&package.repository)?
            .join(&package.package);
        let mut relative = package.package.clone();
        let components = out.split('/').collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            directory.push(component);
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(component);
            match crate::observed_fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "output {out} has an unsupported symlink source-tree parent {}",
                        directory.display()
                    ));
                }
                // Generated outputs inhabit another tree. A regular source file
                // or missing path cannot contain a source subpackage boundary.
                Ok(metadata) if !metadata.is_dir() => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect output package boundary {}: {error}",
                        directory.display()
                    ));
                }
            }
            // An ignored directory is a deleted package, never a subpackage
            // (ContainingPackageLookupFunction).
            // So is an invalid package name (PackageLookupFunction.java:91-96).
            let subpackage = ignored.matching_entry(&relative).is_none()
                && crate::label::is_valid_package_name(&relative)
                && (crate::observed_fs::is_file(directory.join("BUILD.bazel"))
                    || crate::observed_fs::is_file(directory.join("BUILD")));
            // No nested repositories in Bazel 9.3
            // (LocalRepositoryLookupFunction.java:27-35).
            if subpackage {
                return Err(format!(
                    "output {out} crosses a package boundary at {}",
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    /// The labels a package's rules name as inputs (the assumed input files
    /// of Package.java:893-920): resolved `srcs` of every non-macro,
    /// non-package_group rule, labels in any other native attribute or
    /// select() (Package.java:897-920), built-in attributes and select()
    /// keys/branches of Starlark rules, and the computed `package_metadata`
    /// default (Rule.getLabels visits computed defaults too; paired capture
    /// N28 B4/missing_default: `rule '//p:nope' does not exist`).
    fn package_input_labels(&self, package: &Package) -> HashSet<Label> {
        let mut labels = HashSet::new();
        for rule in package.values().filter(|rule| {
            rule.macro_owner.is_none() && !matches!(rule.kind, RuleKind::PackageGroup(_))
        }) {
            labels.extend(
                rule.srcs
                    .iter()
                    .filter_map(|src| self.resolve_rule_src(rule, src).ok()),
            );
            if let Some(native) = &rule.native {
                labels.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                labels.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                labels.extend(custom.declaration.configurable_labels.iter().cloned());
            }
            labels.extend(
                rule.package_metadata
                    .iter()
                    .filter_map(|text| Label::parse_canonical(text).ok()),
            );
        }
        labels
    }

    /// Whether `label` is an input file target of its loaded package: an
    /// exported file, or a label one of its rules names as an input (the
    /// index is built once per package).
    fn declared_source(&mut self, label: &Label, package_id: &PackageId) -> bool {
        let package = &self.packages[package_id];
        if package.exports.contains_key(label) {
            return true;
        }
        if package.macro_names.contains(&label.name) {
            return false;
        }
        if !self.declared_inputs.contains_key(package_id) {
            let labels = self.package_input_labels(package);
            self.declared_inputs.insert(package_id.clone(), labels);
        }
        self.declared_inputs[package_id].contains(label)
    }

    /// Mutable access to a loaded package for unit tests, dropping the
    /// derived per-package indexes and memos.
    #[cfg(test)]
    fn package_mut(&mut self, package: &PackageId) -> &mut Package {
        self.declared_inputs.clear();
        self.source_targets.clear();
        self.analyzed_sources.clear();
        self.packages.get_mut(package).unwrap()
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        // Loaded packages are immutable: a label resolved as a source file
        // stays one (its rule, output and filesystem facts are fixed).
        if let Some(path) = self.source_targets.get(label) {
            return Ok(Target::Source(path.clone()));
        }
        let package_id = label.package_id();
        self.load_package(&package_id)?;
        if let Some(rule) = self.packages[&package_id].get(label) {
            return Ok(Target::Rule(rule.clone()));
        }
        if let Some((owner, output)) = self.generated.get(label) {
            return Ok(Target::Output(
                self.packages[&owner.package_id()][owner].clone(),
                output.clone(),
            ));
        }
        if !self.declared_source(label, &package_id) {
            return Err(self.no_such_target(label)?);
        }
        let (path, stable) = self.source_target(label)?;
        if stable {
            self.source_targets.insert(label.clone(), path.clone());
        }
        Ok(Target::Source(path))
    }

    /// The path of a declared source-file target, checking its package
    /// boundaries and recording missing files and source directories, and
    /// whether it may be memoized: a symlinked source resolves under the
    /// link policy of the repositories materialized so far, so it is
    /// resolved again on every lookup, as before.
    fn source_target(&mut self, label: &Label) -> Result<(PathBuf, bool), String> {
        let repository_root = self.repositories.root(&label.repository)?.to_path_buf();
        let package_dir = repository_root.join(&label.package);
        // A source name with a lone (non-UTF-8) byte escape: Bazel 9.3 reads
        // source files through java.io, which cannot name that byte, so the
        // file is an input in error at execution even when it exists (paired
        // probe, docs/LABEL_SYNTAX.md). Native: a missing input file.
        if crate::bazel_bytes::has_lone_bytes(&label.name) {
            let full = package_dir.join(&label.name);
            self.missing_sources.insert(full.clone(), label.clone());
            return Ok((full, true));
        }
        let ignored = self.ignored(&label.repository)?;
        let mut path = package_dir.clone();
        let mut relative = label.package.clone();
        let mut linked = false;
        let parts: Vec<_> = Path::new(&label.name).components().collect();
        if parts
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!("unsafe source label {label}"));
        }
        for (i, component) in parts.iter().enumerate() {
            path.push(component.as_os_str());
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(&component.as_os_str().to_string_lossy());
            let metadata = match crate::observed_fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                // A declared (assumed) input file is a target whether or not
                // the file exists (Package.java:893-920); a missing file only
                // fails when an action or the build request needs it
                // (skyframe/ArtifactFunction.java:79-90).
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let full = package_dir.join(&label.name);
                    self.missing_sources.insert(full.clone(), label.clone());
                    return Ok((full, true));
                }
                Err(error) => return Err(format!("no target or source {label}: {error}")),
            };
            // Bazel follows source symlinks (docs/ARCHIVE_SYMLINKS.md); package
            // boundaries below stay checked on the logical label path.
            linked |= metadata.file_type().is_symlink();
            // An invalid package name is no package (PackageLookupFunction
            // .java:91-96), so no boundary either.
            if i + 1 < parts.len()
                && ignored.matching_entry(&relative).is_none()
                && crate::label::is_valid_package_name(&relative)
                && (crate::observed_fs::is_file(path.join("BUILD.bazel"))
                    || crate::observed_fs::is_file(path.join("BUILD")))
            {
                return Err(format!(
                    "{label} crosses a package boundary at {}",
                    path.display()
                ));
            }
        }
        if linked {
            let relative = Path::new(&label.package).join(&label.name);
            // Links may continue into a repository already materialized in
            // this invocation, or (repository-rule-generated repositories
            // only) a host path; reads only (source_links::LinkPolicy).
            let policy = self.repositories.link_policy(&label.repository);
            match crate::source_links::resolve_with_policy(&repository_root, &relative, &policy)
                .map_err(|error| format!("source {label}: {error}"))?
            {
                crate::source_links::Resolution::Resolved(resolved) => path = resolved,
                // A dangling source symlink is a missing input file, like a
                // declared source that does not exist.
                crate::source_links::Resolution::Missing(missing) => {
                    self.missing_sources.insert(missing.clone(), label.clone());
                    return Ok((missing, false));
                }
            }
        }
        // A source directory is an input file target too, matched by name
        // like any file (Package.java:893-920, RuleContext.java:1865); only
        // its use as an action input or requested output is not modelled.
        if crate::observed_fs::is_dir(&path) {
            self.source_directories
                .entry(path.clone())
                .or_default()
                .insert(label.clone());
            return Ok((path, !linked));
        }
        if !crate::observed_fs::is_file(&path) {
            return Err(format!("source is not a regular file: {label}"));
        }
        Ok((path, !linked))
    }

    /// NoSuchTargetException for an undeclared target of a loaded package:
    /// Package.getTarget (packages/Package.java:335-362) with
    /// getAlternateTargetSuggestion (:364-396) and TargetSuggester
    /// (packages/TargetSuggester.java), NoSuchTargetException.java:51.
    fn no_such_target(&self, label: &Label) -> Result<String, String> {
        let root = self.repositories.root(&label.repository)?;
        let Ok(build) = workspace::package_build_file(root, &label.package) else {
            // Without a BUILD file path, the succinct form (Package.java:348-350).
            return Ok(format!(
                "no such target '{}': target '{}' not declared in package '{}'",
                crate::rule_declarations::canonical_form(label),
                label.name,
                label.package
            ));
        };
        let directory = root.join(&label.package);
        let candidate = directory.join(&label.name);
        let build_name = build
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let relative_build = if label.package.is_empty() {
            build_name
        } else {
            format!("{}/{build_name}", label.package)
        };
        let suggestion = if label.name == "*" {
            String::new()
        } else if crate::observed_fs::is_dir(&candidate) {
            format!(
                "; however, a source directory of this name exists.  (Perhaps add 'exports_files([\"{}\"])' to {relative_build}, or define a filegroup?)",
                label.name
            )
        } else if crate::observed_fs::exists(&candidate) {
            format!(
                "; however, a source file of this name exists.  (Perhaps add 'exports_files([\"{}\"])' to {relative_build}?)",
                label.name
            )
        } else {
            suggest_targets(&label.name, &self.package_target_names(&label.package_id()))
        };
        Ok(format!(
            "no such target '{}': target '{}' not declared in package '{}' defined by {}{suggestion}",
            crate::rule_declarations::canonical_form(label),
            label.name,
            label.package,
            build.display()
        ))
    }

    /// Names of a loaded package's targets (rules, outputs, exported and
    /// assumed input files), in sorted order (Packageoid.targets is an
    /// ImmutableSortedMap, packages/Packageoid.java:84).
    fn package_target_names(&self, package: &PackageId) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let Some(rules) = self.packages.get(package) else {
            return names;
        };
        names.extend(rules.exports.keys().map(|label| label.name.clone()));
        for rule in rules.values() {
            names.insert(rule.label.name.clone());
            names.extend(rule.outs.iter().cloned());
            if rule.macro_owner.is_some() || matches!(rule.kind, RuleKind::PackageGroup(_)) {
                continue;
            }
            let mut labels = rule
                .srcs
                .iter()
                .filter_map(|src| self.resolve_rule_src(rule, src).ok())
                .collect::<Vec<_>>();
            if let Some(native) = &rule.native {
                labels.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                labels.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                labels.extend(custom.declaration.configurable_labels.iter().cloned());
            }
            labels.extend(
                rule.package_metadata
                    .iter()
                    .filter_map(|text| Label::parse_canonical(text).ok()),
            );
            names.extend(
                labels
                    .into_iter()
                    .filter(|label| label.package_id() == *package)
                    .map(|label| label.name),
            );
        }
        names
    }

    fn resolve_rule_src(&self, rule: &Rule, text: &str) -> Result<Label, String> {
        if matches!(rule.kind, RuleKind::Custom)
            || (rule.native.is_some() && (text.starts_with("//") || text.starts_with("@@")))
        {
            // Custom and native attributes were canonicalized during package evaluation.
            // In particular, //p:t here denotes main, even in an external rule.
            Label::parse_canonical(text)
        } else {
            // Native rules keep raw apparent-repository labels; Bazel's
            // attribute conversion never fails on a non-visible repository
            // (Label.parseWithPackageContext), only its package lookup does
            // (crate::label::unresolvable_package_message).
            self.repositories
                .parse_deferred(text, &rule.label.repository, &rule.label.package)
        }
    }

    /// Target patterns (crate::target_pattern), not the query language:
    /// `query` evaluates one pattern with FilteringPolicies.NO_FILTER.
    /// `relative_directory` is the working directory below the workspace root
    /// (TargetPattern.Parser's relativeDirectory).
    pub fn select(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<Vec<Label>, String> {
        self.select_filtered(patterns, relative_directory, false)
    }

    /// Build target patterns: wildcards skip rules tagged `manual`
    /// (FilteringPolicies.FILTER_MANUAL, pkgcache/FilteringPolicies.java:60-65;
    /// skyframe/TargetPatternPhaseFunction.java:293-296); literal labels stay.
    pub fn select_for_build(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<Vec<Label>, String> {
        self.select_filtered(patterns, relative_directory, true)
    }

    /// TargetPatternPhaseFunction (skyframe/TargetPatternPhaseFunction.java:
    /// 300-370): every pattern is parsed, then expanded in order; a positive
    /// pattern's targets (with the command's filtering policy) are added, a
    /// negative pattern's (unfiltered, TargetPatternValue.java:63-66) removed
    /// from what was accumulated so far (`mergeAll`).
    fn select_filtered(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        filter_manual: bool,
    ) -> Result<Vec<Label>, String> {
        let parsed = patterns
            .iter()
            .map(|pattern| crate::target_pattern::parse(pattern, relative_directory))
            .collect::<Result<Vec<_>, _>>()?;
        let mut selected = BTreeSet::new();
        for pattern in &parsed {
            let filter = filter_manual && !pattern.negative;
            let targets = self
                .expand_pattern(pattern, filter, filter_manual)
                .map_err(PatternError::into_message)?;
            if pattern.negative {
                selected.retain(|label| !targets.contains(label));
            } else {
                selected.extend(targets);
            }
        }
        Ok(selected.into_iter().collect())
    }

    /// One pattern's targets (TargetPattern.eval with
    /// RecursivePackageProviderBackedTargetPatternResolver).
    /// `build` resolves with RecursivePackageProviderBackedTargetPatternResolver,
    /// whose package errors for `:all` carry the `while parsing` prefix; the
    /// query environment reports them unwrapped (paired capture
    /// tests/fixtures/label_syntax, E/relative_in_non_package_dir).
    pub(crate) fn expand_pattern(
        &mut self,
        pattern: &crate::target_pattern::Pattern,
        filter_manual: bool,
        build: bool,
    ) -> Result<BTreeSet<Label>, PatternError> {
        use crate::target_pattern::Kind;
        use PatternError::{Loading, Parsing};
        let original = &pattern.original;
        let mut found = BTreeSet::new();
        match &pattern.kind {
            Kind::TargetsBelowDirectory { rules_only } => {
                let context = self
                    .repositories
                    .parse(&pattern.label_text("__pattern__"), "", "")
                    .map_err(Loading)?;
                // TargetPattern.TargetsBelowDirectory.eval: a directory inside
                // an ignored directory yields a warning and no targets; the
                // rest is searched with filterForDirectory'd entries.
                let ignored = self.ignored(&context.repository).map_err(Loading)?;
                if let Some(entry) = ignored.matching_entry(&context.package) {
                    self.warnings.push(format!(
                        "Pattern '{original}' was filtered out by ignored directory '{entry}'"
                    ));
                    return Ok(found);
                }
                let ignored = ignored.filter_for_directory(&context.package);
                let root = self
                    .repositories
                    .root(&context.repository)
                    .map_err(Loading)?
                    .to_path_buf();
                let skip = self.repositories.link_policy(&context.repository).blocked;
                let packages = if crate::observed_fs::is_dir(root.join(&context.package)) {
                    let discovered = workspace::discover_packages_skipping(
                        &root,
                        &context.package,
                        &ignored,
                        &skip,
                    )
                    .map_err(Loading)?;
                    crate::perf_mark("select.discovered");
                    discovered
                } else {
                    Vec::new()
                };
                // RecursivePackageProviderBackedTargetPatternResolver.java:326-329.
                if packages.is_empty() {
                    return Err(Parsing(format!(
                        "no targets found beneath '{}'",
                        context.package
                    )));
                }
                let ids: Vec<PackageId> = packages
                    .into_iter()
                    .map(|package| PackageId {
                        repository: context.repository.clone(),
                        package,
                    })
                    .collect();
                // P2: the pattern's package loads run on the
                // loading-phase-threads pool; commits — each followed by
                // that package's targets, interleaved exactly like the
                // sequential path — stay in pattern order.
                let rules_only = *rules_only;
                self.load_packages_parallel(&ids, |engine, id| {
                    found.extend(engine.package_targets(id, rules_only, filter_manual)?);
                    Ok(())
                })
                .map_err(Loading)?;
                crate::perf_mark("select.loaded");
            }
            Kind::TargetsInPackage {
                suffix,
                absolute,
                rules_only,
            } => {
                let id = self
                    .repositories
                    .parse(&pattern.label_text("__pattern__"), "", "")
                    .map_err(Loading)?
                    .package_id();
                // TargetsInPackage.getWildcardConflict (TargetPattern.java:
                // 524-560): an absolute `:all` naming an existing target.
                if *absolute
                    && let Some(kind) = self.existing_target_kind(&id, suffix).map_err(Loading)?
                {
                    self.warnings.push(format!(
                        "The target pattern '{original}' is ambiguous: ':{suffix}' is both a wildcard, and the name of an existing {kind}; using the latter interpretation"
                    ));
                    found.insert(Label {
                        repository: id.repository.clone(),
                        package: id.package.clone(),
                        name: suffix.clone(),
                    });
                    return Ok(found);
                }
                // TargetPatternResolverUtil.getParsingErrorMessage
                // (pkgcache/TargetPatternResolverUtil.java:33-39).
                self.load_package(&id).map_err(|error| {
                    Loading(if build {
                        format!("while parsing '{original}': {error}")
                    } else {
                        error
                    })
                })?;
                found.extend(
                    self.package_targets(&id, *rules_only, filter_manual)
                        .map_err(Loading)?,
                );
            }
            Kind::SingleTarget { target } => {
                let label = self
                    .repositories
                    .parse(&pattern.label_text(target), "", "")
                    .map_err(Loading)?;
                self.target(&label).map_err(Loading)?;
                found.insert(label);
            }
            Kind::PathAsTarget { path } => {
                let label = self.path_as_target(path)?;
                self.target(&label).map_err(Loading)?;
                found.insert(label);
            }
        }
        Ok(found)
    }

    /// InterpretPathAsTarget.eval (cmdline/TargetPattern.java:348-383): the
    /// path itself as a package (its default target), else the longest
    /// package prefix owning the rest as a file target.
    fn path_as_target(&mut self, path: &str) -> Result<Label, PatternError> {
        use PatternError::{Loading, Parsing};
        let root = self.repositories.root("").map_err(Loading)?.to_path_buf();
        let ignored = self.ignored("").map_err(Loading)?;
        let is_package = |package: &str| -> Result<bool, PatternError> {
            Ok(ignored.matching_entry(package).is_none()
                && workspace::is_package(&root, package).map_err(Loading)?)
        };
        let pieces: Vec<&str> = path.split('/').collect();
        if is_package(path)? {
            return Label::parse_canonical(&format!("//{path}:{}", pieces[pieces.len() - 1]))
                .map_err(Parsing);
        }
        for split in (0..pieces.len()).rev() {
            let package = pieces[..split].join("/");
            if is_package(&package)? {
                let target = pieces[split..].join("/");
                return Label {
                    repository: String::new(),
                    package,
                    name: String::new(),
                }
                .with_name(&target)
                .map_err(Parsing);
            }
        }
        Err(Parsing(format!(
            "couldn't determine target from filename '{path}'"
        )))
    }

    /// `Target.getTargetKind` of an existing target `name` of package `id`,
    /// or None when the package or target does not exist
    /// (RecursivePackageProviderBackedTargetPatternResolver.getTargetOrNull,
    /// :117-128).
    fn existing_target_kind(
        &mut self,
        id: &PackageId,
        name: &str,
    ) -> Result<Option<String>, String> {
        if self.load_package(id).is_err() {
            return Ok(None);
        }
        let label = Label {
            repository: id.repository.clone(),
            package: id.package.clone(),
            name: name.to_owned(),
        };
        if !self.package_target_names(id).contains(name) {
            return Ok(None);
        }
        Ok(match self.target(&label) {
            Ok(Target::Rule(rule)) if matches!(rule.kind, RuleKind::PackageGroup(_)) => {
                Some("package group".to_owned())
            }
            Ok(Target::Rule(rule)) => Some(format!("{} rule", rule.rule_class_name())),
            Ok(Target::Output(..)) => Some("generated file".to_owned()),
            Ok(Target::Source(_)) => Some("source file".to_owned()),
            Err(_) => None,
        })
    }

    /// TargetPatternResolverUtil.resolvePackageTargets
    /// (pkgcache/TargetPatternResolverUtil.java:40-51) of a loaded package:
    /// its rules (`rules_only`, FilteringPolicies.RULES_ONLY: package groups
    /// are no rules) or every target, without `manual` rules under
    /// FILTER_MANUAL.
    fn package_targets(
        &self,
        id: &PackageId,
        rules_only: bool,
        filter_manual: bool,
    ) -> Result<Vec<Label>, String> {
        let package = &self.packages[id];
        let rules = package
            .values()
            .filter(|rule| !(rules_only && matches!(rule.kind, RuleKind::PackageGroup(_))))
            .filter(|rule| !(filter_manual && rule.is_manual()))
            .map(|rule| rule.label.clone());
        if rules_only {
            return Ok(rules.collect());
        }
        let manual: BTreeSet<&str> = package
            .values()
            .filter(|rule| filter_manual && rule.is_manual())
            .map(|rule| rule.label.name.as_str())
            .collect();
        let root = self.repositories.root(&id.repository)?;
        // The BUILD file is the package's first input file target
        // (Package.Builder, packages/Package.java).
        let build = workspace::package_build_file(root, &id.package)?;
        let build_name = build
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("BUILD")
            .to_owned();
        let mut names = self.package_target_names(id);
        names.insert(build_name);
        Ok(names
            .into_iter()
            .filter(|name| !manual.contains(name.as_str()))
            .map(|name| Label {
                repository: id.repository.clone(),
                package: id.package.clone(),
                name,
            })
            .collect())
    }

    fn resolve_group(
        &mut self,
        label: &Label,
        active: &mut Vec<Label>,
    ) -> Result<Arc<GroupNode>, String> {
        let package = label.package_id();
        self.load_package(&package)?;
        let Some(rule) = self.packages[&package].get(label) else {
            // Preserve existing missing/source/output diagnostics, but never
            // analyze a callback or fabricate a source from an include edge.
            self.target(label)?;
            return Err(format!("{label} does not refer to a package group"));
        };
        let RuleKind::PackageGroup(definition) = &rule.kind else {
            return Err(format!("{label} does not refer to a package group"));
        };
        // Cache hits cannot bypass actual target kind or symbolic ownership.
        check_macro_namespace(rule, label)?;
        if let Some(node) = self.group_nodes.get(label) {
            return Ok(Arc::clone(node));
        }
        if active.contains(label) {
            return Err(format!("package group include cycle at {label}"));
        }
        if active.len() >= 256 {
            return Err("package group DAG exceeds depth limit 256 path nodes".into());
        }
        self.group_budget.node()?;
        definition.own_specs.charge(&mut self.group_budget)?;
        for part in [&label.repository, &label.package, &label.name] {
            self.group_budget.string(part)?;
        }
        // Repeated edges consume budget too, BEFORE allocating the edge vector.
        for edge in &definition.includes {
            self.group_budget.node()?;
            for part in [&edge.repository, &edge.package, &edge.name] {
                self.group_budget.string(part)?;
            }
        }
        // Keep the declaration alive across recursive package loading without
        // copying its specifications or include labels. Cache hits above do not
        // even clone this Arc. The sole local payload copy is precharged above.
        let definition = Arc::clone(definition);
        active.push(label.clone());
        let result = (|| {
            let mut includes = Vec::with_capacity(definition.includes.len());
            for edge in &definition.includes {
                includes.push(self.resolve_group(edge, active)?);
            }
            // Cached child heights are checked by the constructor, independently
            // of this traversal's active depth.
            let node = Arc::new(GroupNode::new(
                Arc::new(definition.own_specs.clone()),
                includes,
            )?);
            self.group_nodes.insert(label.clone(), node.clone());
            Ok(node)
        })();
        active.pop();
        result
    }

    fn source_visibility(&self, label: &Label) -> Visibility {
        let package = &self.packages[&label.package_id()];
        package
            .exports
            .get(label)
            .unwrap_or(&package.default_visibility)
            .clone()
    }

    /// Resolve every effective group reference, even when a pure scope or the
    /// same-authority fast path would already grant access. Roots pass None:
    /// their outgoing visibility dependencies are validated, not access-checked.
    fn visible(
        &mut self,
        visibility: &Visibility,
        declaring: &PackageId,
        requesting: Option<&PackageId>,
    ) -> Result<bool, String> {
        let mut group_grant = false;
        for label in visibility.group_labels() {
            let node = self.resolve_group(label, &mut Vec::new())?;
            group_grant |= requesting.is_some_and(|package| node.contains(package));
        }
        Ok(
            requesting
                .is_none_or(|package| visibility.allows_in(declaring, package) || group_grant),
        )
    }

    /// Analyze one target in configuration `config`, in Bazel's per-target
    /// order (ConfiguredTargetFunction / PrerequisiteProducer): load-time gates
    /// and pre-dependency gaps, configuration conditions in the same
    /// configuration (ConfigConditionsProducer), attribute validation
    /// (ConfiguredAttributeMapper.validateAttributes), gaps of the resolved
    /// attribute values, then dependencies from resolved values only.
    fn analyze(
        &mut self,
        label: &Label,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        // M5: phase-1 steps inside the plan span (see plan_timings).
        let timing = crate::plan_timings::enabled();
        let outermost = timing && self.analyze_depth == 0;
        if timing {
            self.analyze_depth += 1;
        }
        let before = plan.len();
        let mut timer = crate::plan_timings::StageTimer::start_outermost(
            crate::plan_timings::PlanStage::Analyze,
            u64::MAX,
            0,
            outermost,
        );
        let result = self.analyze_key(label, config, true, active, configured, plan);
        if timing {
            self.analyze_depth -= 1;
            if plan.len() > before {
                timer.set_index(before as u64);
            }
        }
        timer.finish();
        result
    }

    /// [`Self::analyze`] for a ConfiguredTargetKey with an explicit
    /// `shouldApplyRuleTransition` (false only for the delegate of a
    /// non-idempotent rule transition).
    fn analyze_key(
        &mut self,
        label: &Label,
        config: ConfigId,
        apply_rule_transition: bool,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        if self.analyzed_sources.contains(label) {
            return Ok(());
        }
        let rule = match self.target(label)? {
            Target::Source(_) => {
                self.visible(&self.source_visibility(label), &label.package_id(), None)?;
                self.analyzed_sources.insert(label.clone());
                return Ok(());
            }
            Target::Rule(rule) => rule,
            Target::Output(rule, _) => {
                check_macro_namespace(&rule, label)?;
                rule
            }
        };
        // Check the requested target and its generator before memoization, but
        // never scan unrelated outputs or impose this analysis gate on query.
        check_macro_namespace(&rule, &rule.label)?;
        self.visible(&rule.visibility, &rule_authority(&rule), None)?;
        if matches!(rule.kind, RuleKind::PackageGroup(_)) {
            self.resolve_group(&rule.label, &mut Vec::new())?;
        }
        // A rule transition decides the configuration everything else is
        // evaluated in (TargetAndConfigurationProducer); `config.none()` is
        // not modelled.
        if let Some(custom) = &rule.custom
            && !custom.declaration.transition_gaps.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.transition_gaps));
        }
        let key = ConfiguredTargetKey {
            label: rule.label.clone(),
            config,
            apply_rule_transition,
        };
        if self.keep_going {
            self.keep_going_state.record_edge(active.last(), &key);
        }
        if configured.contains_key(&key) {
            return Ok(());
        }
        self.prefetch_rule(&rule);
        if let Some(start) = active.iter().position(|active| *active == key) {
            return Err(self.cycle_error(&rule, &active[..start], &active[start..]));
        }
        if active.len() >= 256 {
            return Err("native prototype dependency depth limit (256) exceeded".into());
        }
        // A Starlark rule transition (RuleTransitionApplier): a target whose
        // configuration it changes is the target analyzed under the
        // transitioned key.
        if apply_rule_transition
            && rule
                .custom
                .as_ref()
                .is_some_and(|custom| custom.transitions.contains_key(""))
        {
            active.push(key.clone());
            let delegate = self.rule_transition(&rule, config, active, configured, plan)?;
            if let Some(owner) = delegate {
                self.analyze_key(
                    &rule.label,
                    owner.config,
                    owner.apply_rule_transition,
                    active,
                    configured,
                    plan,
                )?;
                active.pop();
                let node = configured.get(&owner).cloned().unwrap_or_default();
                configured.insert(key.clone(), node);
                self.rule_delegations.insert(key, owner);
                return Ok(());
            }
            active.pop();
        }
        let configuration = self.configurations.get(config).clone();
        let no_config = configuration.native.no_config;
        active.push(key.clone());
        if self.cquery_capture.is_some()
            && let RuleKind::PackageGroup(group) = &rule.kind
        {
            for include in &group.includes {
                self.analyze(include, config, active, configured, plan)?;
                self.capture_cquery_edge(&key, include.clone(), config, "includes", false, false);
            }
        }
        // constraint_setting and constraint_value are analyzed under
        // NoConfigTransition (rules/platform/ConstraintSettingRule.java:42,
        // ConstraintValueRule.java:41): select() in their configurable
        // attributes would be evaluated in the no-op configuration, which is
        // not modelled.
        if let RuleKind::Native(kind @ ("constraint_setting" | "constraint_value")) = rule.kind
            && !configured_attributes::rule_config_dependencies(&rule).is_empty()
        {
            return Err(format!(
                "{}: select() in {kind} attributes is not implemented in the native profile (Bazel analyzes {kind} targets in the no-op configuration, NoConfigTransition)",
                rule.label
            ));
        }
        // select() in the no-config configuration would match config_settings
        // against CoreOptions defaults only, with every other fragment
        // missing: not modelled.
        if no_config && !configured_attributes::rule_config_dependencies(&rule).is_empty() {
            return Err(format!(
                "{}: select() in the no-config configuration (a package_metadata dependency, NoConfigTransition) is not implemented in the native profile",
                rule.label
            ));
        }
        // DependencyContextProducerWithCompatibilityCheck
        // (analysis/producers/DependencyContextProducerWithCompatibilityCheck.java:80-110, 132-145):
        // a rule that uses toolchain resolution resolves its target platform
        // before its select() conditions. A non-default target platform is
        // resolved so; the default host platform only where a constraint
        // needs it (constraint_value keys in config_conditions,
        // config_setting constraint_values matching below; docs/PLATFORMS.md).
        // Inside a platform's tree (the empty configuration, no
        // PlatformConfiguration) no rule resolves toolchains
        // (ToolchainContextUtil.java:79-82: platformConfig == null).
        // The no-config configuration has no PlatformConfiguration either
        // (CommonOptions.EMPTY_OPTIONS): no toolchain resolution, so no
        // execution platform and exec edges keep it
        // (ExecutionTransitionFactory.java:192-197).
        let in_empty_configuration = !self.resolving_platforms.is_empty() || no_config;
        if in_empty_configuration {
            self.empty_configuration_rules.insert(key.clone());
        }
        let uses_toolchains = uses_toolchain_resolution(&rule) && !in_empty_configuration;
        if self.cquery_capture.is_some() && uses_toolchains {
            let platform = configuration.native.target_platform().clone();
            self.platform_info(&platform, config, configured, plan)
                .map_err(|e| e.target_text())?;
            self.capture_cquery_edge(&key, platform, config, "$target_platform", true, false);
        }
        // The exec groups (ToolchainContextUtil.getUnloadedToolchainContextsInputs,
        // RuleTransitionApplier): before the target platform.
        let (use_auto_exec_groups, processed_groups) =
            self.exec_group_inputs(&rule, config, !in_empty_configuration)?;
        let mut target_platform = None;
        if uses_toolchains && !self.default_target_platform(config) {
            target_platform = Some(self.target_platform(config, configured, plan)?);
        }
        // PrerequisiteProducer order (analysis/producers/PrerequisiteProducer.java):
        // select() conditions, then toolchain resolution, then dependency
        // resolution (attribute validation, dependencies), then the factory.
        let ordered = self.config_conditions(&rule, config, active, configured, plan)?;
        if self.cquery_capture.is_some() {
            for (condition, _) in &ordered {
                let dependency = self.resolve_rule_src(&rule, condition)?;
                self.capture_cquery_edge(
                    &key,
                    dependency,
                    config,
                    "$config_dependencies",
                    false,
                    false,
                );
            }
        }
        // Platform compatibility after the target platform and the select()
        // conditions, before toolchain resolution and every other dependency
        // (DependencyContextProducerWithCompatibilityCheck.java:159-189).
        self.check_target_compatibility(
            &rule,
            config,
            &ordered,
            uses_toolchains,
            active,
            configured,
            plan,
        )?;
        if let Some(custom) = &rule.custom
            && !custom.declaration.toolchain_gaps.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.toolchain_gaps));
        }
        // ToolchainResolutionFunction (skyframe/toolchains/ToolchainResolutionFunction.java:62-131)
        // for the default exec group; under automatic exec groups its key has
        // no toolchain types (ToolchainContextUtil.createDefaultToolchainContextKey,
        // :120-144).
        let toolchain_context = if uses_toolchains {
            let mut key = self.toolchain_context_key(&rule, config)?;
            if use_auto_exec_groups {
                key.types.clear();
            }
            self.resolve_toolchains(&rule, config, key, active, configured, plan)?
        } else {
            None
        };
        // Every other exec group of the ToolchainCollection, after the
        // default context (UnloadedToolchainContextsProducer.java:88-126;
        // only the first error propagates, :181-186): the declared groups
        // (the default `test` group of a Starlark test rule,
        // RuleClass.java:153-167), then the automatic groups. Divergence D3
        // (docs/TEST_FLAGS.md): without the built-in profile there is no
        // `@bazel_tools`, so the default test group is not resolved.
        let mut exec_groups = crate::exec_groups::TargetExecGroups {
            use_auto_exec_groups,
            groups: Vec::new(),
        };
        if uses_toolchains {
            for group in processed_groups {
                let types = group
                    .key
                    .types
                    .iter()
                    .map(|requirement| requirement.label.clone())
                    .collect();
                let context = if group.builtin_only {
                    None
                } else {
                    self.resolve_toolchains(&rule, config, group.key, active, configured, plan)?
                };
                exec_groups.groups.push(crate::exec_groups::ResolvedGroup {
                    name: group.name,
                    types,
                    context,
                    builtin_only: group.builtin_only,
                });
            }
        }
        native_analysis_gate(&rule)?;
        let conditions = ordered.iter().cloned().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        // DependencyResolutionHelpers.visitRule reports a validation failure
        // at the rule's location (analysis/DependencyResolutionHelpers.java:309-313).
        let configured_rule = configured_attributes::configure(&rule, &resolver)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        let duplicate_errors =
            configured_attributes::concatenated_duplicate_errors(&rule, &configured_rule);
        let mut rule = configured_rule;
        if let Some(custom) = &rule.custom
            && !custom.declaration.unanalyzed.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.unanalyzed));
        }
        resolved_value_gate(&rule)?;
        coverage_gate(&rule, &configuration)?;
        // An alias-like rule has one dependency, its (resolved) actual: the
        // `actual` attribute, or the late-bound `:alias` of label_flag /
        // label_setting (rules/LabelBuildSettings.java:62-83).
        let build_setting = self.build_setting_info(&rule)?;
        if forwards(&rule) {
            rule.srcs = vec![self.alias_actual(&rule, &configuration, build_setting.as_ref())?];
        }
        // Late-bound attributes skip the visibility check
        // (CommonPrerequisiteValidator.java:159-163); rules that can have any
        // provider skip the testonly check (:730-738).
        let late_bound_rule = matches!(rule.kind, RuleKind::Native("label_flag" | "label_setting"));
        let analysis_dependent = rule
            .custom
            .as_ref()
            .map(|custom| {
                crate::late_bound::analysis_dependent_labels(&custom.schema, &custom.attrs)
            })
            .unwrap_or_default();
        let skip_testonly = forwards(&rule);
        let requesting_package = rule_authority(&rule);
        // The configuration of each dependency edge
        // (DependencyResolutionHelpers.getExecutionPlatformLabel,
        // analysis/DependencyResolutionHelpers.java:223-253): the exec
        // transition needs the parent's execution platform, which toolchain
        // resolution chose before any dependency (ExecutionTransitionFactory).
        let edges = self.rule_edges(&rule);
        // The native genrule embeds this real built-in source in its action
        // instead of requesting a provider view. Configured querying retains
        // its source-file dependency when the built-in repository is enabled.
        if self.cquery_capture.is_some()
            && matches!(rule.kind, RuleKind::Genrule)
            && self.repositories.root("bazel_tools").is_ok()
        {
            let setup = Label::parse_canonical("@@bazel_tools//tools/genrule:genrule-setup.sh")?;
            self.analyze(&setup, config, active, configured, plan)?;
            self.capture_cquery_edge(&key, setup, config, "$genrule_setup", true, false);
        }
        // ExecutionTransitionFactory.create (analysis/config/ExecutionTransitionFactory.java:76-114):
        // in the no-config (and a platform's empty) configuration
        // CommonOptions.EMPTY_OPTIONS has no Starlark exec transition
        // (`starlarkExecConfig = null`, analysis/config/CommonOptions.java:34-41),
        // so every exec-transition dependency edge fails to create its
        // transition before any dependency is analyzed - unless the rule has
        // an attribute named `license_kinds` (AttributeMap.has, :109-111, the
        // rules_license license() hack), whose exec edges become
        // NoConfigTransition. Genrules always have the exec edge
        // `$genrule_setup` (bazel/rules/genrule/BazelGenRuleRule.java:44-46),
        // Starlark test rules their `$test_*` tool edges
        // (StarlarkRuleClassFunctions.java:290-352).
        if in_empty_configuration {
            let custom_exec = rule.custom.as_ref().is_some_and(|custom| {
                custom.declaration.builtin.kind.test && !custom.schema.contains_key("license_kinds")
            });
            let has_license_kinds = rule
                .custom
                .as_ref()
                .is_some_and(|custom| custom.schema.contains_key("license_kinds"));
            let exec_edge = edges
                .iter()
                .any(|edge| matches!(edge.cfg, EdgeCfg::Exec(_)));
            if matches!(rule.kind, RuleKind::Genrule)
                || custom_exec
                || (exec_edge && !has_license_kinds)
            {
                return Err(located(
                    rule.location.as_deref(),
                    "expected a Starlark exec transition definition, but was null".to_owned(),
                ));
            }
        }
        // The configuration of each Starlark-transition attribute, computed
        // at its first edge (an error names that edge, DependencyProducer
        // getMessageWithEdgeTransitionInfo, analysis/producers/DependencyProducer.java:258-266).
        let mut attribute_configs: BTreeMap<String, TransitionConfigurations> = BTreeMap::new();
        let exec_config =
            if uses_toolchains && edges.iter().any(|edge| edge.cfg == EdgeCfg::Exec(None)) {
                let exec_config = self.exec_configuration(
                    &rule,
                    config,
                    toolchain_context.as_ref(),
                    configured,
                    plan,
                )?;
                self.exec_configurations.insert(key.clone(), exec_config);
                exec_config
            } else {
                // No execution platform: the exec transition is a no-op
                // (ExecutionTransitionFactory.java:194-197).
                config
            };
        // The exec configurations of named exec groups
        // (config.exec(exec_group = NAME)): the group's execution platform.
        let mut group_configs: BTreeMap<String, ConfigId> = BTreeMap::new();
        for edge in edges {
            let dep_configs = match &edge.cfg {
                EdgeCfg::Target => EdgeConfigurations::One(config),
                EdgeCfg::Exec(None) => EdgeConfigurations::One(exec_config),
                EdgeCfg::NoConfig => {
                    EdgeConfigurations::One(self.configurations.intern_no_config())
                }
                // DependencyResolutionHelpers.getExecutionPlatformLabel
                // (analysis/DependencyResolutionHelpers.java:223-253): a
                // group of the ToolchainCollection gives its execution
                // platform; an unknown group is a dependency error when the
                // rule has toolchain contexts (DependencyProducer.ExecGroupErrorEmitter);
                // without them there is no execution platform and the
                // transition is a no-op.
                EdgeCfg::Exec(Some(group)) if uses_toolchains => {
                    EdgeConfigurations::One(if let Some(group_config) = group_configs.get(group) {
                        *group_config
                    } else {
                        let Some(resolved) =
                            exec_groups.groups.iter().find(|item| item.name == *group)
                        else {
                            return Err(located(
                                rule.location.as_deref(),
                                format!(
                                    "Attr '{}' declares a transition for non-existent exec group '{group}'",
                                    native_attribute_name(&edge.attribute)
                                ),
                            ));
                        };
                        let context = resolved.context.clone();
                        let group_config = self.exec_configuration(
                            &rule,
                            config,
                            context.as_ref(),
                            configured,
                            plan,
                        )?;
                        self.group_exec_configurations
                            .insert((key.clone(), group.clone()), group_config);
                        group_configs.insert(group.clone(), group_config);
                        group_config
                    })
                }
                EdgeCfg::Exec(Some(_)) => EdgeConfigurations::One(config),
                // Applied per dependency edge (DependencyProducer), so
                // print() output repeats per edge; the result is per
                // attribute (its values do not depend on the edge).
                EdgeCfg::Starlark(attribute) => {
                    let dependency = self.resolve_rule_src(&rule, &edge.label)?;
                    let id = self.attribute_transition(&rule, config, attribute, &dependency)?;
                    attribute_configs.insert(attribute.clone(), Arc::clone(&id));
                    EdgeConfigurations::Transition(id)
                }
            };
            for dep_config in dep_configs.ids() {
                let src = &edge.label;
                let late_bound = late_bound_rule || analysis_dependent.contains(src);
                let dependency = self.resolve_rule_src(&rule, src)?;
                if self.cquery_capture.is_some() {
                    self.capture_cquery_edge(
                        &key,
                        dependency.clone(),
                        dep_config,
                        if forwards(&rule) {
                            if late_bound_rule { ":alias" } else { "actual" }
                        } else {
                            &edge.attribute
                        },
                        matches!(edge.cfg, EdgeCfg::Exec(_)),
                        false,
                    );
                }
                // The allowlist Bazel adds to rules with a Starlark transition
                // lives in @bazel_tools, which the explicit-module profile
                // (--experimental-builtin-modules=false) does not provide.
                if edge.attribute == crate::custom_rule::FUNCTION_TRANSITION_ALLOWLIST_ATTRIBUTE
                    && dependency.repository == "bazel_tools"
                    && self.repositories.root("bazel_tools").is_err()
                {
                    return Err(located(
                        rule.location.as_deref(),
                        format!(
                            "the Starlark transition allowlist {} of {} needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide; drop --experimental-builtin-modules=false, or declare `_allowlist_function_transition = attr.label(default = \"//tools/allowlists/function_transition_allowlist\")` with a package_group of that name",
                            crate::rule_declarations::canonical_form(&dependency),
                            crate::rule_declarations::canonical_form(&rule.label)
                        ),
                    ));
                }
                let target = self.target(&dependency).map_err(|error| {
                    let error = self.missing_edge_error(&rule, &dependency, error);
                    if analysis_dependent.contains(src) {
                        // Name the gap: the edge is a resolved late-bound default
                        // (crate::late_bound), e.g. apple.xcode_config_label's
                        // @bazel_tools//tools/cpp:host_xcodes.
                        crate::late_bound::edge_error(&rule.label, &dependency, error)
                    } else {
                        error
                    }
                })?;
                let is_source = matches!(target, Target::Source(_));
                let forwarding = matches!(&target, Target::Rule(dep) if forwards(dep));
                // CommonPrerequisiteValidator.validateDirectPrerequisiteVisibility
                // (analysis/CommonPrerequisiteValidator.java:181-221): an implicit
                // (private, `_x`) attribute of a Starlark rule - subrule-lifted
                // attributes included - is checked against the package of the
                // `.bzl` that defines the rule class first, then against the
                // consumer; a failure names that `.bzl` (:384-386). That `.bzl`
                // is the rule class's definition environment label
                // (getOwnerDefinitionBzl, :133-143): the module being initialized
                // when rule() ran, not a helper `.bzl` that called rule().
                let implicit_owner = rule
                    .custom
                    .as_ref()
                    .filter(|_| edge.attribute.starts_with('_') && !late_bound)
                    .and_then(|custom| {
                        Label::parse_canonical(&custom.declaration.definition_environment_label)
                            .ok()
                    });
                let (dep_visibility, dep_declaring) = match &target {
                    Target::Rule(dep) | Target::Output(dep, _) => {
                        (dep.visibility.clone(), rule_authority(dep))
                    }
                    Target::Source(_) => {
                        (self.source_visibility(&dependency), dependency.package_id())
                    }
                };
                let owner_visible = match &implicit_owner {
                    Some(owner) => {
                        self.visible(&dep_visibility, &dep_declaring, Some(&owner.package_id()))?
                    }
                    None => false,
                };
                let visible_from = implicit_owner.as_ref();
                let visible = owner_visible
                    || match target {
                        Target::Rule(dep) | Target::Output(dep, _) => {
                            check_macro_namespace(&dep, &dependency)?;
                            check_macro_namespace(&dep, &dep.label)?;
                            if matches!(dep.kind, RuleKind::PackageGroup(_))
                                && !matches!(rule.kind, RuleKind::Custom)
                            {
                                return Err(format!(
                                    "{}: package group '{dependency}' is misplaced in {}",
                                    rule.label, edge.attribute
                                ));
                            }
                            late_bound
                                || self.visible(
                                    &dep.visibility,
                                    &rule_authority(&dep),
                                    Some(&requesting_package),
                                )?
                        }
                        Target::Source(_) => {
                            late_bound
                                || self.visible(
                                    &self.source_visibility(&dependency),
                                    &dependency.package_id(),
                                    Some(&requesting_package),
                                )?
                        }
                    };
                if !forwarding {
                    if !visible {
                        return Err(visibility_error(
                            &rule,
                            &describe_with_aliases(&[], &dependency, "target"),
                            is_source,
                            visible_from,
                        ));
                    }
                    self.validate_prerequisite(
                        &rule,
                        &dependency,
                        &|| describe_with_aliases(&[], &dependency, "target"),
                        skip_testonly,
                    )?;
                    self.analyze(&dependency, dep_config, active, configured, plan)?;
                    continue;
                }
                // The configured alias (its actual's label and kind) is needed to
                // describe and validate the edge (AliasProvider.describeTargetWithAliases).
                self.analyze(&dependency, dep_config, active, configured, plan)?;
                let (chain, actual) = self.alias_chain(configured, dep_config, &dependency);
                // Formatted only for a reported error.
                let description = || describe_with_aliases(&chain, &actual, "target");
                if !visible {
                    let is_source = matches!(self.target(&actual)?, Target::Source(_));
                    return Err(visibility_error(
                        &rule,
                        &description(),
                        is_source,
                        visible_from,
                    ));
                }
                self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
            }
        }
        // The resolved toolchain implementations are dependencies after the
        // attribute dependencies (DependencyResolutionHelpers.java:358-373),
        // analyzed in the parent's configuration (DependencyProducer.java:160-175,
        // DependencyResolver.java:835-866), each once.
        // Every exec group's toolchains are dependencies too, in
        // ToolchainCollection order (default first; :358-373).
        for context in std::iter::once(&toolchain_context)
            .chain(exec_groups.groups.iter().map(|group| &group.context))
            .flatten()
        {
            for implementation in context.resolved_labels() {
                self.toolchain_implementation(
                    &implementation,
                    config,
                    &context.exec_platform.label,
                )
                .map_err(|error| located(rule.location.as_deref(), error))?;
                self.target(&implementation)
                    .map_err(|error| self.missing_edge_error(&rule, &implementation, error))?;
                self.analyze(&implementation, config, active, configured, plan)?;
                self.capture_cquery_edge(&key, implementation, config, "$toolchains", true, false);
            }
        }
        // Starlark rules: prerequisite errors need the analyzed prerequisites'
        // files and providers, so they are reported before the implementation
        // runs (Self::starlark_prerequisite_errors).
        // package_metadata (no-config edges): native rules here; a Starlark
        // rule's are part of its prerequisite pass, as a built-in attribute
        // before its own attributes.
        if rule.custom.is_none() {
            let errors = self.package_metadata_errors(&rule, configured)?;
            if !errors.is_empty() {
                return Err(errors
                    .into_iter()
                    .map(|error| located(rule.location.as_deref(), error))
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
        }
        // TestSuite.checkTestsAndSuites (rules/test/TestSuite.java:44-49,
        // 104-118) after RuleContext's prerequisite validation.
        let errors = self.test_suite_errors(&rule, config, configured)?;
        if !errors.is_empty() {
            return Err(crate::rule_errors::located_events(
                rule.location.as_deref(),
                errors,
            ));
        }
        if !attribute_configs.is_empty() {
            let splits = self.split_attribute_dependencies(&rule, &attribute_configs)?;
            if let Some(custom) = &mut rule.custom {
                custom.split_dependencies = Arc::new(splits);
            }
            self.edge_configs.insert(key.clone(), attribute_configs);
        }
        // config_setting flag_values/constraint_values and the platform rules'
        // label attributes: prerequisites analyzed, then validated
        // (RuleContext.Builder, analysis/RuleContext.java:1700-1745, 1845-1880, 2165-2172).
        let (flag_prerequisites, constraint_prerequisites) =
            self.config_setting_prerequisites(&rule, config, active, configured, plan)?;
        let platform_prerequisites =
            self.platform_rule_prerequisites(&rule, config, active, configured, plan)?;
        // RuleContext.Builder.validateExtraPrerequisites first checks resolved
        // label lists for duplicates (analysis/RuleContext.java:2110-2113,
        // 1553-1568), attribute errors at the rule's location.
        if rule.custom.is_some() {
            self.deferred_duplicates
                .insert(key.clone(), duplicate_errors.clone());
        } else if !duplicate_errors.is_empty() {
            return Err(duplicate_errors
                .iter()
                .map(|error| located(rule.location.as_deref(), error.clone()))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        // RuleContext.Builder.validateExtraPrerequisites validates the config
        // conditions like direct prerequisites, after dependency analysis
        // (analysis/RuleContext.java:2104-2152). Under the DEFAULT_PUBLIC
        // policy the validated prerequisite is the END of an alias chain
        // (`condition.fromConfiguredTargetNoCheck(getActual())`, :2148-2150;
        // AliasConfiguredTarget.getActual, rules/AliasConfiguredTarget.java:229-233):
        // the alias's own visibility is skipped and the message names the
        // config_setting without an alias description.
        for (condition, _) in &ordered {
            let dependency = self.resolve_rule_src(&rule, condition)?;
            let (_, actual) = self.alias_chain(configured, config, &dependency);
            if let Target::Rule(dep) = self.target(&actual)? {
                // Formatted only for a reported error.
                let description = || describe_with_aliases(&[], &actual, "target");
                if !self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )? {
                    return Err(visibility_error(&rule, &description(), false, None));
                }
                self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
            }
        }
        // ConfigSetting.create (rules/config/ConfigSetting.java:111-166).
        let mut matching = match (&rule.kind, &rule.native) {
            (RuleKind::Native("config_setting"), Some(native)) => {
                let repositories = self.repositories.clone();
                let context = rule.label.clone();
                let mut warnings = Vec::new();
                let mut run = |configuration: &Configuration, platform: Option<&PlatformInfo>| {
                    crate::config_setting::analyze(
                        &rule.label,
                        native,
                        configuration,
                        &flag_prerequisites,
                        &constraint_prerequisites,
                        platform,
                        &|text| repositories.parse(text, &context.repository, &context.package),
                        &mut warnings,
                    )
                };
                let mut result = run(&configuration, target_platform.as_deref());
                if result.as_ref().err().map(String::as_str)
                    == Some(crate::config_setting::NEEDS_TARGET_PLATFORM)
                {
                    let platform = self.target_platform(config, configured, plan)?;
                    result = run(&configuration, Some(&platform));
                }
                // Rule warnings precede the rule's errors; an identical
                // warning (another configuration, the retry above) is
                // reported once (EmittedEventState.addWarning).
                for warning in warnings {
                    let warning = located(rule.location.as_deref(), warning);
                    if !self.warnings.contains(&warning) {
                        self.warnings.push(warning);
                    }
                }
                Some(result.map_err(|error| located_lines(rule.location.as_deref(), &error))?)
            }
            _ => None,
        };
        // ConstraintSetting/ConstraintValue/Platform.create (rules/platform/*.java).
        let (mut constraint_setting, mut constraint_value, mut platform) =
            self.platform_rule_providers(&rule, config, &platform_prerequisites, configured)?;
        // ToolchainType.create / Toolchain.create (rules/ToolchainType.java:40-52,
        // rules/platform/Toolchain.java:42-110).
        let (mut toolchain_type, mut declared_toolchain) =
            self.toolchain_rule_providers(&rule, config, &platform_prerequisites, configured)?;
        if let Some(declared) = &declared_toolchain {
            self.prefetch_compatible_toolchain(declared);
        }
        // ConfigFeatureFlag.create (rules/config/ConfigFeatureFlag.java:109-169).
        let mut feature_flag = self.config_feature_flag_provider(&rule)?;
        // AliasConfiguredTarget forwards the actual's providers; label_flag and
        // label_setting override BuildSettingProvider (LateBoundAlias.java:58-79).
        let mut actual = None;
        let mut build_setting = build_setting;
        if forwards(&rule) {
            let target = self.resolve_rule_src(&rule, &rule.srcs[0])?;
            let forwarded = configured
                .get(&ConfiguredTargetKey::new(target.clone(), config))
                .cloned()
                .unwrap_or_default();
            matching = forwarded.matching;
            if build_setting.is_none() {
                build_setting = forwarded.build_setting;
            }
            constraint_setting = forwarded.constraint_setting;
            constraint_value = forwarded.constraint_value;
            platform = forwarded.platform;
            toolchain_type = forwarded.toolchain_type;
            declared_toolchain = forwarded.declared_toolchain;
            feature_flag = forwarded.feature_flag;
            actual = Some(target);
        }
        // ctx.build_setting_value of a Starlark build setting.
        if let (Some(info), Some(custom)) = (&build_setting, rule.custom.as_mut()) {
            custom.setting_value = Some(info.value(&configuration));
        }
        active.pop();
        if let Some(context) = toolchain_context {
            self.toolchain_contexts.insert(key.clone(), context);
        }
        if uses_toolchains {
            self.exec_group_contexts.insert(key.clone(), exec_groups);
        }
        configured.insert(
            key.clone(),
            ConfiguredNode {
                matching,
                build_setting,
                actual,
                constraint_setting,
                constraint_value,
                platform,
                toolchain_type,
                declared_toolchain,
                feature_flag,
            },
        );
        let rule = Arc::new(rule);
        if rule.custom.is_some() {
            self.configured_rules.insert(key.clone(), Arc::clone(&rule));
        }
        self.analyze_aspect_edges(&rule, &key, active, configured, plan)?;
        if let Some(capture) = &mut self.cquery_capture {
            capture.rules.insert(key.clone(), Arc::clone(&rule));
        }
        plan.push((key, rule));
        Ok(())
    }

    /// The aspects attached to a rule's attribute edges need their own
    /// dependency analysis (N58): every aspect application reachable from
    /// the rule's attributes — the aspects' own attribute dependencies and
    /// the applications propagated along each dependency's `attr_aspects`
    /// edges — is analyzed here, before the rule's planning
    /// (AspectFunction's AspectValue dependencies,
    /// skyframe/AspectFunction.java:682-703).
    fn analyze_aspect_edges(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        if !rule.custom.as_ref().is_some_and(|custom| {
            custom
                .schema
                .values()
                .any(|spec| !spec.declaration.aspects.is_empty())
        }) {
            return Ok(());
        }
        let mut seen: BTreeSet<(ConfiguredTargetKey, Vec<String>)> = BTreeSet::new();
        for edge in self.rule_edges(rule) {
            let Some(spec) = rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute))
            else {
                continue;
            };
            if spec.declaration.aspects.is_empty() {
                continue;
            }
            let aspects = self.attribute_aspects(rule, &spec.declaration.aspects)?;
            let edge_configs = self.analyzed_edge_configurations(key, &edge.cfg)?;
            let dependency = self.resolve_rule_src(rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                let applications = self.analyze_aspect_applications(
                    &dependency,
                    edge_config,
                    aspects.clone(),
                    active,
                    configured,
                    plan,
                    &mut seen,
                )?;
                if let Some(capture) = &mut self.cquery_capture {
                    capture.request(key, applications);
                }
            }
        }
        Ok(())
    }

    /// Analyze every aspect application of `aspects` on `label`'s configured
    /// target (and, recursively, the applications they propagate along the
    /// target's `attr_aspects` edges): the aspect collection of the edge,
    /// each applied aspect's own attribute dependencies, then the
    /// propagated applications (DependencyResolver over the aspect path).
    #[allow(clippy::too_many_arguments)]
    fn analyze_aspect_applications(
        &mut self,
        label: &Label,
        config: ConfigId,
        aspects: Vec<std::sync::Arc<crate::aspects::AspectDefinition>>,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
        seen: &mut BTreeSet<(ConfiguredTargetKey, Vec<String>)>,
    ) -> Result<Vec<u32>, String> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        // The application's configuration is the base configured target's,
        // after a rule transition's delegation (AspectKeyCreator over the
        // ConfiguredTargetKey; as Self::plan_aspect_collection).
        let config = self.delegated_config(&key, config);
        // Aspects never apply in the no-config configuration
        // (AspectFunction.java:296-305: NonApplicableAspect).
        if self.configurations.get(config).native.no_config {
            return Ok(Vec::new());
        }
        let key = self.rule_delegations.get(&key).cloned().unwrap_or(key);
        // Aspects do not apply to source files or package groups
        // (AspectFunction.java:327-334); an output file applies its
        // generating rule when the aspect asks for it
        // (apply_to_generating_rules, AspectDefinition.java).
        let mut base_label = key.label.clone();
        let mut target = self.target(&base_label)?;
        if let Target::Output(generating, _) = &target {
            let applies = aspects.iter().any(|aspect| {
                aspect.declaration.apply_to_generating_rules
                    || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
            });
            if !applies {
                return Ok(Vec::new());
            }
            base_label = generating.label.clone();
            target = self.target(&base_label)?;
        }
        let Target::Rule(base_rule) = target else {
            return Ok(Vec::new());
        };
        if matches!(base_rule.kind, RuleKind::PackageGroup(_)) {
            return Ok(Vec::new());
        }
        // Alias edges resolve to the actual target's aspect application
        // (createAspectsKeys, ToplevelStarlarkAspectFunction.java:140-157).
        while let Some(actual) = configured
            .get(&ConfiguredTargetKey::new(base_label.clone(), config))
            .and_then(|node| node.actual.clone())
        {
            base_label = actual;
        }
        let base_key = ConfiguredTargetKey::new(base_label.clone(), config);
        let base_rule = match self.target(&base_label)? {
            // Loading declarations still contain select() placeholders. Using
            // them here omits selected deps, whereas phase 2 later walks the
            // configured rule and demands aspect applications never analyzed.
            // Publish/read the shared resolved declaration in phase 1, not as
            // a side effect of whichever targets plan_pending committed first.
            Target::Rule(rule) => self
                .configured_rules
                .get(&base_key)
                .cloned()
                .unwrap_or(rule),
            _ => return Ok(Vec::new()),
        };
        let collection = crate::aspects::aspect_collection(aspects, &base_rule)?;
        if collection.is_empty() {
            return Ok(Vec::new());
        }
        let applications = self
            .cquery_capture
            .as_mut()
            .map(|capture| capture.applications(&base_key, &collection, &base_rule))
            .unwrap_or_default();
        let memo = (base_key.clone(), collection.descriptors());
        if !seen.insert(memo) {
            return Ok(applications);
        }
        let incoming: Vec<std::sync::Arc<crate::aspects::AspectDefinition>> = collection
            .used
            .iter()
            .map(|(aspect, _)| aspect.clone())
            .collect();
        for (index, (aspect, _)) in collection.used.iter().enumerate() {
            // A NonApplicableAspect (AspectFunction.java:332-339) resolves
            // no dependencies of its own.
            if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
                || !aspect.applies_to_rule(&base_rule)
            {
                continue;
            }
            // The aspect's own attribute dependencies, each in its
            // attribute's configuration (Self::aspect_attribute_configurations),
            // validated from the aspect's `.bzl` package
            // (CommonPrerequisiteValidator, Self::analyze_aspect_attribute_edges).
            // The aspect's own toolchain contexts first: they decide its
            // execution platforms (DependencyContextProducer runs before
            // the dependencies); a ToolchainException is the application's
            // error as is (AspectFunction.java:510-515).
            let toolchains = self
                .aspect_toolchain_contexts(&base_rule, config, aspect, active, configured, plan)?;
            if let Some(capture) = &mut self.cquery_capture {
                // Record per application, including memoized toolchain contexts.
                // The resolution memo is not an edge owner.
                capture.aspect_targets(applications[index], &toolchains.cquery_dependencies);
                for context in std::iter::once(toolchains.default.as_ref())
                    .chain(
                        toolchains
                            .groups
                            .groups
                            .iter()
                            .map(|group| group.context.as_ref()),
                    )
                    .flatten()
                {
                    for label in context.resolved_labels() {
                        capture.aspect_target(applications[index], label, config);
                    }
                }
            }
            let attribute_configs = self.aspect_attribute_configurations(
                &base_rule,
                &base_key,
                aspect,
                aspect,
                &toolchains,
                configured,
                plan,
            )?;
            let path = collection.path_aspects(&aspect.descriptor);
            self.analyze_aspect_attribute_edges(
                &base_rule,
                &base_label,
                aspect,
                aspect,
                None,
                &attribute_configs,
                &path,
                applications.get(index).copied(),
                active,
                configured,
                plan,
                seen,
            )?;
            // The attributes of the base aspects of the path that
            // `ctx.rule.attr` holds are dependencies of this application
            // too (DependencyResolutionHelpers.getAttributes/
            // addAspectAttributes), each in its attribute's configuration
            // over THIS aspect's execution platforms.
            for (owner, names) in crate::aspects::base_aspect_attributes(&path, &base_rule) {
                let owner_configs = self.aspect_attribute_configurations(
                    &base_rule,
                    &base_key,
                    &owner,
                    aspect,
                    &toolchains,
                    configured,
                    plan,
                )?;
                self.analyze_aspect_attribute_edges(
                    &base_rule,
                    &base_label,
                    &owner,
                    aspect,
                    Some(&names),
                    &owner_configs,
                    &path,
                    None,
                    active,
                    configured,
                    plan,
                    seen,
                )?;
            }
        }
        // The applications propagated along the base target's edges: each
        // aspect of the collection that propagates along the attribute name
        // applies to the edge's dependency (computePropagatingAspects), in
        // the edge's configuration exactly as the application's planning
        // reaches it (Self::plan_aspect_application).
        for edge in self.rule_edges(&base_rule) {
            let attribute_aspects = base_rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute))
                .map(|spec| spec.declaration.aspects.clone())
                .unwrap_or_default();
            let attribute_aspects = self.loaded_attribute_aspects(&base_rule, &attribute_aspects);
            let propagating =
                crate::aspects::propagating_aspects(&attribute_aspects, &incoming, &edge.attribute);
            if propagating.is_empty() {
                continue;
            }
            if edge.cfg == EdgeCfg::NoConfig {
                continue;
            }
            let edge_configs = self.analyzed_edge_configurations(&base_key, &edge.cfg)?;
            let dependency = self.resolve_rule_src(&base_rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                let children = self.analyze_aspect_applications(
                    &dependency,
                    edge_config,
                    propagating.clone(),
                    active,
                    configured,
                    plan,
                    seen,
                )?;
                if let Some(capture) = &mut self.cquery_capture {
                    capture.propagated(
                        &applications,
                        &collection,
                        &children,
                        &edge.attribute,
                        &attribute_aspects,
                    );
                }
            }
        }
        Ok(applications)
    }

    /// Phase 1 of the label attributes of `owner` (the applied aspect
    /// `main` itself, or a base aspect of its path whose attributes
    /// `ctx.rule.attr` holds, restricted to `names`) in one application of
    /// `main` to `base_rule`: each dependency analyzed in its attribute's
    /// configuration, with the aspects of the path that propagate over the
    /// attribute (collectPropagatingAspects: never the owner itself), and
    /// visibility-checked from the owner's `.bzl` package
    /// (CommonPrerequisiteValidator). `capture` is the cquery application
    /// of `main` recording the owner's edges (its own attributes only).
    #[allow(clippy::too_many_arguments)]
    fn analyze_aspect_attribute_edges(
        &mut self,
        base_rule: &Rule,
        base_label: &Label,
        owner: &crate::aspects::AspectDefinition,
        main: &crate::aspects::AspectDefinition,
        names: Option<&[String]>,
        attribute_configs: &BTreeMap<String, TransitionConfigurations>,
        path: &[std::sync::Arc<crate::aspects::AspectDefinition>],
        capture: Option<u32>,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
        seen: &mut BTreeSet<(ConfiguredTargetKey, Vec<String>)>,
    ) -> Result<(), String> {
        let authority = crate::label::PackageId {
            repository: owner.definition_label.repository.clone(),
            package: owner.definition_label.package.clone(),
        };
        let failure = |error: String| {
            format!(
                "in {} aspect on {} rule {}: {error}",
                main.identity,
                base_rule.rule_class_name(),
                crate::rule_declarations::canonical_form(base_label)
            )
        };
        for (name, spec) in &owner.declaration.attrs {
            if names.is_some_and(|names| !names.contains(name)) {
                continue;
            }
            let Some(branches) = attribute_configs.get(name) else {
                continue;
            };
            let propagating = crate::aspects::propagating_over_aspect_attribute(path, owner, name);
            for text in aspect_attribute_labels(spec) {
                // Aspect attribute labels resolve relative to the
                // aspect's `.bzl` package (LabelConverter.
                // forBzlEvaluatingThread at aspect() declaration).
                let dependency = self.repositories.parse(
                    &text,
                    &owner.definition_label.repository,
                    &owner.definition_label.package,
                )?;
                for (_, attribute_config) in branches.iter() {
                    let attribute_config = *attribute_config;
                    self.analyze(&dependency, attribute_config, active, configured, plan)
                        .map_err(failure)?;
                    if let (Some(index), Some(cquery)) = (capture, &mut self.cquery_capture) {
                        cquery.aspect_target(index, dependency.clone(), attribute_config);
                    }
                    if !propagating.is_empty() {
                        self.analyze_aspect_applications(
                            &dependency,
                            attribute_config,
                            propagating.clone(),
                            active,
                            configured,
                            plan,
                            seen,
                        )?;
                    }
                    let dep_rule = match self.target(&dependency)? {
                        Target::Rule(dep_rule) => dep_rule,
                        Target::Output(generating, _) => generating,
                        _ => continue,
                    };
                    if let Err(error) = self.visible(&dep_rule.visibility, &authority, None) {
                        return Err(failure(error));
                    }
                }
            }
        }
        Ok(())
    }

    /// The aspect's OWN ToolchainCollection in configuration `config`
    /// (AspectFunction.getUnloadedToolchainContextsInputs,
    /// AspectFunction.java:811-838, with DependencyContextProducer and
    /// UnloadedToolchainContextsProducer): the default exec group's context
    /// over the aspect's `exec_compatible_with` and — without automatic exec
    /// groups — its `toolchains` (ToolchainContextUtil.
    /// createDefaultToolchainContextKey; no forced execution platform), then
    /// DeclaredExecGroup.process's groups: the aspect's `exec_groups`, and
    /// under automatic exec groups (an `_use_auto_exec_groups` attribute,
    /// else `--incompatible_auto_exec_groups`; shouldUseAutoExecGroups,
    /// :840-849) one group per toolchain type. Never the base rule's
    /// contexts. The resolved toolchains are dependencies of the
    /// application, analyzed in the base configuration
    /// (DependencyKind.forExecGroup; DependencyProducer.java:160-175).
    /// Memoized per (configuration, aspect): resolution depends on nothing
    /// else.
    fn aspect_toolchain_contexts(
        &mut self,
        base_rule: &Rule,
        config: ConfigId,
        aspect: &crate::aspects::AspectDefinition,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<AspectToolchains>, String> {
        let memo = (config, aspect.identity.clone());
        if let Some(found) = self.aspect_toolchains.get(&memo) {
            return Ok(found.clone());
        }
        let declaration = &aspect.declaration;
        let use_auto = match declaration
            .attrs
            .iter()
            .find(|(name, _)| name == "_use_auto_exec_groups")
            .map(|(_, spec)| &spec.default)
        {
            None => {
                self.configurations
                    .get(config)
                    .native
                    .incompatible_auto_exec_groups
            }
            Some(crate::custom_rule::AttrValue::Bool(value)) => *value,
            Some(_) => {
                return Err(format!(
                    "aspect {}: a non-boolean '_use_auto_exec_groups' attribute: Bazel 9.3 reads it as a boolean (AspectFunction.shouldUseAutoExecGroups) and fails; not supported by the native profile",
                    aspect.identity
                ));
            }
        };
        let types = requirements(&declaration.toolchains)?;
        let mut exec_constraints: Vec<Label> = Vec::new();
        for text in &declaration.exec_compatible_with {
            let label = Label::parse_canonical(text)?;
            if !exec_constraints.contains(&label) {
                exec_constraints.push(label);
            }
        }
        let mut toolchains = AspectToolchains {
            default: None,
            cquery_dependencies: Vec::new(),
            groups: crate::exec_groups::TargetExecGroups {
                use_auto_exec_groups: use_auto,
                groups: Vec::new(),
            },
        };
        // A platform's tree and the no-config configuration resolve no
        // toolchains (as a rule there, Self::analyze_key).
        let in_empty_configuration = !self.resolving_platforms.is_empty()
            || self.configurations.get(config).native.no_config;
        if !in_empty_configuration {
            let mut processed = Vec::new();
            for group in &declaration.exec_groups {
                let mut constraints = Vec::new();
                for text in &group.exec_compatible_with {
                    constraints.push(Label::parse_canonical(text)?);
                }
                processed.push((
                    group.name.clone(),
                    ToolchainContextKey {
                        types: requirements(&group.toolchains)?,
                        exec_constraints: constraints,
                        forced_exec: None,
                    },
                ));
            }
            if use_auto {
                for requirement in &types {
                    processed.push((
                        requirement.label.to_string(),
                        ToolchainContextKey {
                            types: vec![requirement.clone()],
                            exec_constraints: exec_constraints.clone(),
                            forced_exec: None,
                        },
                    ));
                }
            }
            let default_key = ToolchainContextKey {
                types: if use_auto { Vec::new() } else { types.clone() },
                exec_constraints: exec_constraints.clone(),
                forced_exec: None,
            };
            self.resolving_aspect_toolchains = true;
            let resolved = (|| -> Result<(), String> {
                let cquery_key = self.cquery_capture.as_ref().map(|_| default_key.clone());
                toolchains.default = self.resolve_toolchains(
                    base_rule,
                    config,
                    default_key,
                    active,
                    configured,
                    plan,
                )?;
                if let Some(key) = cquery_key {
                    toolchains
                        .cquery_dependencies
                        .extend(self.capture_cquery_aspect_toolchain_context(config, &key));
                }
                for (name, key) in processed {
                    let group_types = key
                        .types
                        .iter()
                        .map(|requirement| requirement.label.clone())
                        .collect();
                    let cquery_key = self.cquery_capture.as_ref().map(|_| key.clone());
                    let context =
                        self.resolve_toolchains(base_rule, config, key, active, configured, plan)?;
                    if let Some(key) = cquery_key {
                        toolchains
                            .cquery_dependencies
                            .extend(self.capture_cquery_aspect_toolchain_context(config, &key));
                    }
                    toolchains
                        .groups
                        .groups
                        .push(crate::exec_groups::ResolvedGroup {
                            name,
                            types: group_types,
                            context,
                            builtin_only: false,
                        });
                }
                Ok(())
            })();
            self.resolving_aspect_toolchains = false;
            resolved?;
            // The resolved toolchain implementations, in ToolchainCollection
            // order (default first), each once.
            let implementations = std::iter::once(toolchains.default.as_ref())
                .chain(
                    toolchains
                        .groups
                        .groups
                        .iter()
                        .map(|group| group.context.as_ref()),
                )
                .flatten()
                .flat_map(|context| {
                    context
                        .resolved_labels()
                        .into_iter()
                        .map(|label| (label, context.exec_platform.label.clone()))
                })
                .collect::<Vec<_>>();
            for (implementation, exec_platform) in implementations {
                self.toolchain_implementation(&implementation, config, &exec_platform)
                    .map_err(|error| located(base_rule.location.as_deref(), error))?;
                self.target(&implementation)
                    .map_err(|error| self.missing_edge_error(base_rule, &implementation, error))?;
                self.analyze(&implementation, config, active, configured, plan)?;
            }
        }
        let toolchains = Arc::new(toolchains);
        self.aspect_toolchains.insert(memo, toolchains.clone());
        Ok(toolchains)
    }

    /// The configuration of each label attribute of `aspect` applied to the
    /// configured target `base_key` (AspectFunction's dependency
    /// resolution: DependencyProducer over the main aspect's attributes,
    /// analysis/producers/DependencyProducer.java:184-219, with
    /// DependencyResolutionHelpers.getExecutionPlatformLabel,
    /// analysis/DependencyResolutionHelpers.java:195-253): the base
    /// target's configuration for `cfg = "target"`; for `cfg = "exec"` or
    /// `config.exec()` (ExecutionTransitionFactory) the exec transition for
    /// the execution platform of the ASPECT's own default exec group (its
    /// toolchain context, AspectFunction.getUnloadedToolchainContextsInputs,
    /// AspectFunction.java:811-838 — the host platform natively, the only
    /// execution platform; docs/TOOLCHAINS.md), never the base rule's; the
    /// attribute's Starlark transition applied to the base configuration
    /// over the base target's attributes (AttributeTransitionData.attributes,
    /// the base rule's attribute map). A named exec group the aspect does
    /// not declare is DependencyProducer.ExecGroupErrorEmitter's error at
    /// the base target. Memoized per application
    /// (Self::aspect_attribute_configs), read by its planning.
    #[allow(clippy::too_many_arguments)]
    fn aspect_attribute_configurations(
        &mut self,
        base_rule: &Rule,
        base_key: &ConfiguredTargetKey,
        aspect: &crate::aspects::AspectDefinition,
        main: &crate::aspects::AspectDefinition,
        toolchains: &AspectToolchains,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<BTreeMap<String, TransitionConfigurations>>, String> {
        use crate::rule_declarations::Cfg;
        let memo = (base_key.clone(), aspect_attribute_memo_name(aspect, main));
        if let Some(configs) = self.aspect_attribute_configs.get(&memo) {
            return Ok(configs.clone());
        }
        let config = base_key.config;
        let mut exec_config = None;
        let mut group_configs: BTreeMap<String, ConfigId> = BTreeMap::new();
        let mut configs = BTreeMap::new();
        for (name, spec) in &aspect.declaration.attrs {
            if aspect_attribute_labels(spec).is_empty() {
                continue;
            }
            // Attribute.getName: the native name (`$x` for `_x`).
            let attribute =
                crate::custom_rule::native_attribute_name(name, spec.declaration.late_bound);
            let edge_config = match &spec.declaration.cfg {
                Cfg::Target => config,
                Cfg::Exec(group)
                    if group.as_deref().is_none_or(|group| {
                        group == crate::exec_groups::DEFAULT_EXEC_GROUP_NAME
                    }) =>
                {
                    match exec_config {
                        Some(exec) => exec,
                        None => {
                            let exec = self.exec_configuration(
                                base_rule,
                                config,
                                toolchains.default.as_ref(),
                                configured,
                                plan,
                            )?;
                            exec_config = Some(exec);
                            exec
                        }
                    }
                }
                // An exec group of the aspect's ToolchainCollection gives its
                // execution platform.
                Cfg::Exec(Some(group)) => match group_configs.get(group) {
                    Some(&exec) => exec,
                    None => {
                        let Some(resolved) = toolchains
                            .groups
                            .groups
                            .iter()
                            .find(|resolved| resolved.name == *group)
                        else {
                            return Err(located(
                                base_rule.location.as_deref(),
                                format!(
                                    "Attr '{attribute}' declares a transition for non-existent exec group '{group}'"
                                ),
                            ));
                        };
                        let context = resolved.context.clone();
                        let exec = self.exec_configuration(
                            base_rule,
                            config,
                            context.as_ref(),
                            configured,
                            plan,
                        )?;
                        group_configs.insert(group.clone(), exec);
                        exec
                    }
                },
                Cfg::Starlark {
                    analysis_test: false,
                    ..
                } => {
                    let transition = aspect.attribute_transition(name).ok_or_else(|| {
                        format!(
                            "aspect {}: attribute '{name}' declares a Starlark transition that the native profile did not retain; {}",
                            aspect.identity,
                            crate::rule_declarations::NOT_ANALYZED
                        )
                    })?;
                    let def = crate::starlark_transitions::TransitionDef::of(&transition)?;
                    let attributes =
                        configured_attributes::transition_attributes(base_rule, None, &[]);
                    let dependency = aspect_attribute_labels(spec)
                        .first()
                        .and_then(|text| {
                            self.repositories
                                .parse(
                                    text,
                                    &aspect.definition_label.repository,
                                    &aspect.definition_label.package,
                                )
                                .ok()
                        })
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_default();
                    let branches = self
                        .apply_transition(
                            &def,
                            crate::starlark_transitions::Kind::Attribute,
                            &base_rule.label,
                            config,
                            &attributes,
                        )
                        .map_err(|failure| {
                            let mut lines = failure.events;
                            lines.push(located(
                                base_rule.location.as_deref(),
                                format!(
                                    "on dependency edge {} ({}) -|{attribute}|-> {dependency}: {}",
                                    crate::rule_declarations::canonical_form(&base_rule.label),
                                    &config.to_string()[..7],
                                    failure.message
                                ),
                            ));
                            lines.join("\n")
                        })?;
                    configs.insert(name.clone(), branches);
                    continue;
                }
                other => {
                    return Err(format!(
                        "aspect {}: attribute '{name}' cfg={} (configuration transition): {}",
                        aspect.identity,
                        other.describe(),
                        crate::rule_declarations::NOT_ANALYZED
                    ));
                }
            };
            configs.insert(name.clone(), Arc::from([(String::new(), edge_config)]));
        }
        let configs = Arc::new(configs);
        self.aspect_attribute_configs.insert(memo, configs.clone());
        Ok(configs)
    }

    /// ResolvedToolchainContext.load (analysis/ResolvedToolchainContext.java:48-110)
    /// with ConfiguredTargetFunction.java:348-366 for one exec group: the
    /// ToolchainInfo of each resolved implementation (through an alias),
    /// else TargetNotToolchainException. `None` is a context that needed no
    /// platform: no toolchains.
    fn load_toolchain_context(
        &self,
        rule: &Rule,
        config: ConfigId,
        context: Option<&UnloadedToolchainContext>,
        target_description: &str,
        custom_providers: &BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
    ) -> Result<crate::toolchain_info::ToolchainContextData, String> {
        let mut toolchains = BTreeMap::new();
        let mut requested = Vec::new();
        if let Some(context) = context {
            for (info, implementation) in &context.resolved {
                let Some(provider) = custom_providers
                    .get(&ConfiguredTargetKey::new(implementation.clone(), config))
                    .and_then(|providers| providers.get(crate::toolchain_info::PROVIDER_ID))
                else {
                    return Err(self.toolchain_error(
                        rule,
                        config,
                        "",
                        &crate::toolchains::not_a_toolchain(&info.type_label, implementation),
                    ));
                };
                toolchains.insert(info.type_label.clone(), provider.clone());
            }
            requested = context
                .requested
                .iter()
                .map(|(label, info)| (label.clone(), info.type_label.clone()))
                .collect();
        }
        Ok(crate::toolchain_info::ToolchainContextData {
            target_description: target_description.to_owned(),
            requested,
            toolchains,
            repositories: self.repositories.clone(),
        })
    }

    /// ExecGroupCollection.Builder.build (analysis/ExecGroupCollection.java:52-64),
    /// run by RuleContext.Builder.build (analysis/RuleContext.java:1491-1531)
    /// before the implementation: per exec group of the ToolchainCollection
    /// (`None` for a rule without toolchain resolution), its execution
    /// platform (the host platform natively) and combined exec properties;
    /// an exec_properties key naming an unknown group is an
    /// InvalidExecGroupException at the target's location
    /// (ConfiguredTargetFunction.java:390-395, 468-469).
    fn action_execution_table(
        &self,
        rule: &Rule,
        config: ConfigId,
        groups: Option<&crate::exec_groups::TargetExecGroups>,
    ) -> Result<Vec<crate::exec_groups::ActionExecution>, String> {
        let raw = match rule.custom.as_ref().and_then(|custom| {
            custom
                .declaration
                .builtin
                .target
                .attrs
                .get("exec_properties")
        }) {
            None => Vec::new(),
            Some(crate::custom_rule::AttrValue::StringDict(entries)) => entries.clone(),
            Some(_) => {
                return Err(located(
                    rule.location.as_deref(),
                    "a configurable exec_properties value is not supported by the native profile"
                        .to_owned(),
                ));
            }
        };
        let key = ConfiguredTargetKey::new(rule.label.clone(), config);
        self.execution_table(
            config,
            &raw,
            self.toolchain_contexts.get(&key),
            groups,
            &self.display_form(&rule.label),
        )
        .map_err(|error| located(rule.location.as_deref(), error))
    }

    /// The per-exec-group execution platform and combined exec properties
    /// of a ToolchainCollection (`groups`, `None` without toolchain
    /// resolution; `default` its default group's context) over the raw
    /// `exec_properties` (ExecGroupCollection.Builder.build); `target`
    /// names the target in an InvalidExecGroupException.
    fn execution_table(
        &self,
        config: ConfigId,
        raw: &[(String, String)],
        default: Option<&UnloadedToolchainContext>,
        groups: Option<&crate::exec_groups::TargetExecGroups>,
        target: &str,
    ) -> Result<Vec<crate::exec_groups::ActionExecution>, String> {
        let host = self.configurations.get(config).native.host_platform.clone();
        // The execution platform of a context; a context that needed no
        // platform runs on the host platform, whose properties are known
        // once it was resolved (the default host platform has none).
        let platform_of =
            |context: Option<&UnloadedToolchainContext>| -> (Label, Vec<(String, String)>) {
                match context {
                    Some(context) => (
                        context.exec_platform.label.clone(),
                        context.exec_platform.exec_properties.clone(),
                    ),
                    None => {
                        let properties = match self
                            .platforms
                            .get(&ConfiguredTargetKey::new(host.clone(), config))
                        {
                            Some(Ok(info)) => info.exec_properties.clone(),
                            _ => Vec::new(),
                        };
                        (host.clone(), properties)
                    }
                }
            };
        let mut platforms = Vec::new();
        if let Some(groups) = groups {
            platforms.push((
                crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
                platform_of(default),
            ));
            for group in &groups.groups {
                platforms.push((group.name.clone(), platform_of(group.context.as_ref())));
            }
        }
        let collection = platforms
            .iter()
            .map(
                |(name, (_, properties))| crate::exec_groups::GroupPlatform {
                    name,
                    platform_properties: properties,
                },
            )
            .collect::<Vec<_>>();
        let combined = crate::exec_groups::combined_exec_properties(
            groups.map(|_| collection.as_slice()),
            raw,
            target,
        )
        .map_err(|error| error.to_string())?;
        Ok(combined
            .into_iter()
            .map(|(name, properties)| {
                let platform = platforms
                    .iter()
                    .find(|(group, _)| *group == name)
                    .map_or_else(|| host.clone(), |(_, (label, _))| label.clone());
                crate::exec_groups::ActionExecution {
                    exec_group: name,
                    platform,
                    properties,
                }
            })
            .collect())
    }

    /// The configuration of `rule`'s exec-transition edges (its default exec
    /// group): the exec transition (crate::config_registry::exec_transition)
    /// for the execution platform chosen by its toolchain resolution — the
    /// host platform, the only execution platform (docs/TOOLCHAINS.md),
    /// resolved here when toolchain resolution needed no platform (the lazy
    /// default host platform, docs/PLATFORMS.md).
    fn exec_configuration(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        context: Option<&UnloadedToolchainContext>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ConfigId, String> {
        let platform = match context {
            Some(context) => context.exec_platform.label.clone(),
            None => {
                let host = self.configurations.get(config).native.host_platform.clone();
                self.platform_info(&host, config, configured, plan)
                    .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?
                    .label
                    .clone()
            }
        };
        // Everything the transition below depends on besides `config` and
        // `platform` is fixed until Self::set_configuration (Self::exec_transitions).
        let key = (config, platform);
        if let Some(&exec) = self.exec_transitions.get(&key) {
            #[cfg(debug_assertions)]
            self.check_exec_transition_memo(&key, exec);
            return Ok(exec);
        }
        let target_scoped = self.target_scoped_flags(config)?;
        // An invalid exec output directory name fails the dependency's
        // configuration (InvalidMnemonicException), reported at the rule.
        let exec = self
            .configurations
            .intern_exec(config, key.1.clone(), &target_scoped)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        self.exec_transitions.insert(key, exec);
        Ok(exec)
    }

    /// Debug builds recompute a memoized exec transition
    /// (Self::exec_transitions) on every hit: its key must cover every input
    /// of the computation. The recomputation has no side effects: the flag
    /// scopes and the exec configuration were recorded by the memoized call.
    #[cfg(debug_assertions)]
    fn check_exec_transition_memo(&mut self, key: &(ConfigId, Label), memoized: ConfigId) {
        let (config, platform) = key;
        let recomputed = self.target_scoped_flags(*config).and_then(|target_scoped| {
            self.configurations
                .intern_exec(*config, platform.clone(), &target_scoped)
        });
        assert_eq!(
            recomputed,
            Ok(memoized),
            "internal error: stale exec configuration memo for {config} and {platform}"
        );
    }

    /// The Starlark flags of the top-level configuration and of `config`
    /// whose rule explicitly sets `scope = "target"` (Scope.ScopeType.TARGET,
    /// analysis/config/Scope.java:31-46): the flags the exec transition of
    /// `config` resets, also covering the top-level configuration's own so
    /// that the memoized exec baseline (crate::config_registry) is the same
    /// whichever configuration computes it first.
    fn target_scoped_flags(&mut self, config: ConfigId) -> Result<BTreeSet<Label>, String> {
        let mut labels = self
            .configurations
            .top_configuration()
            .starlark
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        labels.extend(self.configurations.get(config).starlark.keys().cloned());
        let mut flags = BTreeSet::new();
        for label in labels {
            let scoped = match self.flag_scopes.get(&label) {
                Some(scoped) => *scoped,
                None => {
                    let scoped = match self.target(&label)? {
                        Target::Rule(rule) => self
                            .build_setting_info(&rule)?
                            .is_some_and(|info| info.scope.is_target()),
                        _ => false,
                    };
                    self.flag_scopes.insert(label.clone(), scoped);
                    scoped
                }
            };
            if scoped {
                flags.insert(label);
            }
        }
        Ok(flags)
    }

    /// The retained Starlark transition of `rule` under `key` (`""` for
    /// the rule's `cfg`, else the attribute name).
    fn transition_def(
        rule: &Rule,
        key: &str,
    ) -> Result<Option<crate::starlark_transitions::TransitionDef>, String> {
        rule.custom
            .as_ref()
            .and_then(|custom| custom.transitions.get(key))
            .map(crate::starlark_transitions::TransitionDef::of)
            .transpose()
    }

    /// StarlarkBuildSettingsDetailsFunction
    /// (skyframe/StarlarkBuildSettingsDetailsFunction.java:60-330) for the
    /// Starlark settings a transition reads or writes (inputs then outputs,
    /// StarlarkTransitionCache.getAllStarlarkBuildSettings): each must be a
    /// build setting, possibly through unconfigured aliases. Errors are the
    /// TransitionException texts (a loading exception as `CLASS: MESSAGE`).
    fn transition_details(
        &mut self,
        def: &crate::starlark_transitions::TransitionDef,
    ) -> Result<TransitionDetails, String> {
        let settings = def.starlark_settings();
        if let Some(details) = self.transition_details.get(&settings) {
            return details.clone();
        }
        let details = self.compute_transition_details(&settings);
        self.transition_details.insert(settings, details.clone());
        details
    }

    /// getBuildSettingPackages (:139-195): rounds over the not yet verified
    /// labels — every label (also an alias's actual) may be seen once, so a
    /// transition naming both an alias and its actual, or an alias cycle,
    /// is `Dependency cycle involving 'L' detected in aliased build
    /// settings`; their packages are loaded, then each is verified
    /// (verifyBuildSettingsAndGetAliases, :210-262) and the next round holds
    /// the aliases' actuals. Then each setting's details are those of the
    /// end of its alias chain (getActual, :272-290).
    fn compute_transition_details(
        &mut self,
        settings: &[Label],
    ) -> Result<TransitionDetails, String> {
        let mut seen = BTreeSet::new();
        let mut targets: BTreeMap<Label, SettingTarget> = BTreeMap::new();
        let mut unverified = settings.to_vec();
        while !unverified.is_empty() {
            for label in &unverified {
                if !seen.insert(label.clone()) {
                    return Err(format!(
                        "Dependency cycle involving '{}' detected in aliased build settings",
                        crate::rule_declarations::canonical_form(label)
                    ));
                }
            }
            let mut packages: Vec<PackageId> = Vec::new();
            for label in &unverified {
                let package = label.package_id();
                if !packages.contains(&package) {
                    packages.push(package);
                }
            }
            for package in &packages {
                self.load_package(package)
                    .map_err(|error| PlatformError::loading(error).target_text())?;
            }
            let mut next = Vec::new();
            for label in &unverified {
                let target = self.setting_target(label)?;
                if let SettingTarget::Alias(actual) = &target
                    && !next.contains(actual)
                {
                    next.push(actual.clone());
                }
                targets.insert(label.clone(), target);
            }
            unverified = next;
        }
        let mut details = BTreeMap::new();
        for label in settings {
            let mut current = label;
            let detail = loop {
                match targets.get(current) {
                    Some(SettingTarget::Alias(actual)) => current = actual,
                    Some(SettingTarget::Setting(detail)) => break detail.clone(),
                    None => {
                        return Err(format!(
                            "internal error: build setting {current} was not loaded"
                        ));
                    }
                }
            };
            details.insert(label.clone(), detail);
        }
        Ok(details)
    }

    /// One label of verifyBuildSettingsAndGetAliases (:210-262).
    fn setting_target(&mut self, label: &Label) -> Result<SettingTarget, String> {
        let shown = crate::rule_declarations::canonical_form(label);
        let not_a_setting =
            || format!("attempting to transition on '{shown}' which is not a build setting");
        let rule = match self
            .target(label)
            .map_err(|error| PlatformError::loading(error).target_text())?
        {
            Target::Source(_) => return Err(not_a_setting()),
            // Target.getAssociatedRule of an output file is its generating
            // rule, which is not a build setting.
            Target::Output(..) => return Err(not_a_setting()),
            Target::Rule(rule) => rule,
        };
        if let (RuleKind::Native("alias"), Some(native)) = (&rule.kind, &rule.native) {
            if native.configurable.contains_key("actual") {
                return Err(format!(
                    "attempting to transition on aliased build setting '{shown}', the actual value of which uses select(). Aliased build settings with configured actual values is not supported."
                ));
            }
            let Some(crate::custom_rule::AttrValue::Label(Some(actual))) =
                native.attrs.get("actual")
            else {
                return Err(not_a_setting());
            };
            return Ok(SettingTarget::Alias(self.resolve_rule_src(&rule, actual)?));
        }
        let Some(info) = self.build_setting_info(&rule)? else {
            return Err(not_a_setting());
        };
        Ok(SettingTarget::Setting(
            crate::starlark_transitions::SettingDetail {
                actual: info.label.clone(),
                ty: info.ty,
                default: info.default,
            },
        ))
    }

    /// Apply a Starlark transition attached to `target` to `from`
    /// (TransitionApplier with StarlarkTransitionCache): the resulting
    /// configuration, interned with its output path mnemonic.
    fn apply_transition(
        &mut self,
        def: &crate::starlark_transitions::TransitionDef,
        kind: crate::starlark_transitions::Kind,
        target: &Label,
        from: ConfigId,
        attributes: &crate::starlark_transitions::AttributeValues,
    ) -> Result<TransitionConfigurations, crate::starlark_transitions::Failure> {
        use sha2::Digest;
        let failure = |message: String| crate::starlark_transitions::Failure {
            events: Vec::new(),
            message,
        };
        let digest: [u8; 32] = sha2::Sha256::digest(
            serde_json::to_vec(&(
                target.to_string(),
                kind == crate::starlark_transitions::Kind::Rule,
                attributes,
            ))
            .map_err(|error| failure(error.to_string()))?,
        )
        .into();
        let cache_key = (def.id, from, digest);
        // StarlarkTransitionCache.computeIfAbsent (:128-185): a hit replays
        // the application's non-error events (print() output), so every
        // application prints, also the IdempotencyChecker's and a
        // delegate's re-application (RuleTransitionApplier).
        if let Some((id, printed)) = self.transition_cache.get(&cache_key) {
            for line in printed {
                crate::ui::note(line);
            }
            return Ok(Arc::clone(id));
        }
        let configuration = self.configurations.get(from).clone();
        let mapped = def.with_flag_aliases(&configuration.native.flag_aliases);
        let def = &mapped;
        let details = self.transition_details(def).map_err(failure)?;
        let applied = crate::starlark_transitions::apply(
            def,
            &crate::starlark_transitions::Application {
                kind,
                from: &configuration,
                details: &details,
                target,
                attributes,
                repositories: &self.repositories,
            },
        )?;
        for line in &applied.printed {
            crate::ui::note(line);
        }
        let target_scoped = self.target_scoped_flags(from).map_err(failure)?;
        let id: TransitionConfigurations = applied
            .configurations
            .into_iter()
            .map(|(key, config)| {
                self.configurations
                    .intern_derived(config, &target_scoped)
                    .map(|id| (key, id))
                    .map_err(failure)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into();
        self.transition_cache
            .insert(cache_key, (Arc::clone(&id), applied.printed));
        Ok(id)
    }

    /// RuleTransitionApplier (analysis/producers/RuleTransitionApplier.java:130-378):
    /// the target platform and select() conditions of `rule` in its
    /// requested configuration, the rule's Starlark transition over its
    /// attributes (StarlarkRuleTransitionProvider.createTransition,
    /// analysis/starlark/StarlarkRuleTransitionProvider.java:94-200), then
    /// the idempotency check: `None` for an identity transition (the target
    /// is analyzed under its requested key), else the key it is analyzed
    /// under — with `shouldApplyRuleTransition` only when applying the
    /// transition again gives the same configuration (IDEMPOTENT), without
    /// it otherwise (NON_IDEMPOTENT). Errors are reported at the rule.
    fn rule_transition(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Option<ConfiguredTargetKey>, String> {
        let Some(def) = Self::transition_def(rule, "")? else {
            return Ok(None);
        };
        let uses_toolchains =
            uses_toolchain_resolution(rule) && self.resolving_platforms.is_empty();
        if uses_toolchains && !self.default_target_platform(config) {
            self.target_platform(config, configured, plan)?;
        }
        let ordered = self.config_conditions(rule, config, active, configured, plan)?;
        let conditions = ordered.into_iter().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        let attributes = configured_attributes::transition_attributes(
            rule,
            Some(&resolver),
            &def.given_outputs(),
        );
        let report = |failure: crate::starlark_transitions::Failure| {
            let mut lines = failure.events;
            lines.push(located(rule.location.as_deref(), failure.message));
            lines.join("\n")
        };
        let kind = crate::starlark_transitions::Kind::Rule;
        let post = self
            .apply_transition(&def, kind, &rule.label, config, &attributes)
            .map_err(report)?[0]
            .1;
        if post == config {
            return Ok(None);
        }
        let again = self
            .apply_transition(&def, kind, &rule.label, post, &attributes)
            .map_err(report)?[0]
            .1;
        Ok(Some(if again == post {
            ConfiguredTargetKey::new(rule.label.clone(), post)
        } else {
            ConfiguredTargetKey::without_rule_transition(rule.label.clone(), post)
        }))
    }

    /// The configuration of the dependencies of `rule`'s attribute
    /// `attribute`, whose `cfg` is a Starlark transition
    /// (StarlarkAttributeTransitionProvider, analysis/starlark/StarlarkAttributeTransitionProvider.java:105-135):
    /// the transition over the rule's configured attribute values. A
    /// failure is reported at the rule as `on dependency edge L (CONFIG)
    /// -|ATTR|-> DEP: MESSAGE` (DependencyProducer.java:230-266; CONFIG is
    /// the first 7 hex digits of the native ConfigId).
    fn attribute_transition(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attribute: &str,
        dependency: &Label,
    ) -> Result<TransitionConfigurations, String> {
        let def = Self::transition_def(rule, attribute)?.ok_or_else(|| {
            format!(
                "{}: attribute '{attribute}' declares a Starlark transition that the native profile did not retain (a lifted subrule attribute); {}",
                rule.label,
                crate::rule_declarations::NOT_ANALYZED
            )
        })?;
        let attributes = configured_attributes::transition_attributes(rule, None, &[]);
        self.apply_transition(
            &def,
            crate::starlark_transitions::Kind::Attribute,
            &rule.label,
            config,
            &attributes,
        )
        .map_err(|failure| {
            let mut lines = failure.events;
            lines.push(located(
                rule.location.as_deref(),
                format!(
                    "on dependency edge {} ({}) -|{}|-> {}: {}",
                    crate::rule_declarations::canonical_form(&rule.label),
                    &config.to_string()[..7],
                    native_attribute_name(attribute),
                    crate::rule_declarations::canonical_form(dependency),
                    failure.message
                ),
            ));
            lines.join("\n")
        })
    }

    /// RuleConfiguredTargetBuilder.handleAllowlistChecker for the Starlark
    /// function transition allowlist (analysis/RuleConfiguredTargetBuilder.java:104-112,
    /// 299-320; StarlarkRuleClassFunctions.java:2052-2057): after the
    /// implementation, a rule with a Starlark transition must be
    /// instantiated in (INSTANCE) or defined in (DEFINITION) a package of
    /// its `_allowlist_function_transition` package group
    /// (Allowlist.isAvailableFor, analysis/Allowlist.java:62-98), else the
    /// rule error `Non-allowlisted use of Starlark transition`.
    fn check_transition_allowlist(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        configured: &ConfiguredNodes,
    ) -> Result<(), String> {
        let Some(custom) = &rule.custom else {
            return Ok(());
        };
        if custom.transitions.is_empty() {
            return Ok(());
        }
        // Allowlist.getAttributeNameFromAllowlistName: `$whitelist_` then
        // `$allowlist_` (analysis/Allowlist.java:140-144).
        let Some((name, text)) = [
            "_whitelist_function_transition",
            "_allowlist_function_transition",
        ]
        .into_iter()
        .find_map(|name| match custom.attrs.get(name) {
            Some(crate::custom_rule::AttrValue::Label(Some(text))) => Some((name, text)),
            _ => None,
        }) else {
            return Err(format!(
                "{}: the function transition allowlist attribute has no value; {}",
                rule.label,
                crate::rule_declarations::NOT_ANALYZED
            ));
        };
        let label = self.resolve_rule_src(rule, text)?;
        let edge_cfg = match custom.schema.get(name).map(|spec| &spec.declaration.cfg) {
            Some(crate::rule_declarations::Cfg::Exec(group)) => EdgeCfg::Exec(
                group
                    .clone()
                    .filter(|g| g != crate::exec_groups::DEFAULT_EXEC_GROUP_NAME),
            ),
            Some(crate::rule_declarations::Cfg::Starlark { .. }) => {
                EdgeCfg::Starlark(name.to_owned())
            }
            _ => EdgeCfg::Target,
        };
        let edge_configs = self.analyzed_edge_configurations(key, &edge_cfg)?;
        for edge_config in edge_configs.ids() {
            let actual = self
                .forwarded_actual(
                    configured,
                    &ConfiguredTargetKey::new(label.clone(), edge_config),
                )
                .unwrap_or_else(|| label.clone());
            let group = match self.target(&actual)? {
                Target::Rule(group) if matches!(group.kind, RuleKind::PackageGroup(_)) => {
                    self.resolve_group(&actual, &mut Vec::new())?
                }
                _ => {
                    return Err(format!(
                        "{}: the function transition allowlist {} is not a package_group (a PackageSpecificationProvider that is not a package_group is not modelled natively)",
                        rule.label,
                        crate::rule_declarations::canonical_form(&actual)
                    ));
                }
            };
            let definition =
                Label::parse_canonical(&custom.declaration.definition_environment_label)?;
            if group.contains(&rule.label.package_id()) || group.contains(&definition.package_id())
            {
                continue;
            }
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "in {} rule {}: Non-allowlisted use of Starlark transition",
                    rule.rule_class_name(),
                    crate::rule_declarations::canonical_form(&rule.label)
                ),
            ));
        }
        Ok(())
    }

    /// A missing dependency edge (missing_edge), preceded by the target
    /// loading event when the target belongs to another package.
    fn missing_edge_error(&self, rule: &Rule, dependency: &Label, error: String) -> String {
        let event = self.foreign_target_event(&rule.label.package_id(), dependency, &error);
        let edge = missing_edge(rule, dependency, error);
        match event {
            Some(event) => format!("{event}\n{edge}"),
            None => edge,
        }
    }

    /// Bazel reports a missing target of another package first as that
    /// package's loading event, located at its BUILD file
    /// (`/ws/p/BUILD.bazel: no such target '//p:x': ...`, an event of the
    /// target lookup, as observed in the paired captures N16b
    /// A10/missing_target_edge and N16c B2/lx_use_cv_missing), before the
    /// consumer's edge error.
    fn foreign_target_event(
        &self,
        consumer: &PackageId,
        dependency: &Label,
        error: &str,
    ) -> Option<String> {
        if dependency.package_id() == *consumer {
            return None;
        }
        self.target_loading_event(dependency, error)
    }

    /// The loading event of a missing target, at its package's BUILD file.
    fn target_loading_event(&self, dependency: &Label, error: &str) -> Option<String> {
        if !error.starts_with("no such target '") {
            return None;
        }
        let root = self.repositories.root(&dependency.repository).ok()?;
        let build = workspace::package_build_file(root, &dependency.package).ok()?;
        Some(format!("{}: {error}", build.display()))
    }

    /// AbstractLabelCycleReporter.maybeReportCycle (skyframe/AbstractLabelCycleReporter.java:81-170):
    /// `in KIND L: cycle in dependency graph:` at the location of the cycle's
    /// first element, the path to the cycle indented by four spaces, then the
    /// cycle drawn with `.-> `, `|   `, `` `-- `` (`[self-edge]` for one
    /// element). Configured target keys print as `LABEL (CONFIG)`; the native
    /// configuration is the full native ConfigId, not Bazel's BuildOptions
    /// checksum, and the single native configuration has no separate
    /// top-level key (docs/CONFIGURATION.md).
    fn cycle_error(
        &self,
        head: &Rule,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
    ) -> String {
        let key = ConfiguredTargetKey::to_string;
        let mut message = String::from("cycle in dependency graph:");
        // The path starts at the requested target's top-level key: for a
        // test-trimmed rule, its key in the untrimmed configuration, then its
        // own (AbstractLabelCycleReporter prints every key of the path,
        // skyframe/AbstractLabelCycleReporter.java:105-114).
        if let Some(root) = &self.requested_trimmed_root
            && path.first().or(cycle.first()) == Some(root)
        {
            message.push_str(&format!(
                "\n    {} ({})",
                crate::rule_declarations::canonical_form(&root.label),
                self.configurations.get(root.config).untrimmed_id()
            ));
        }
        for element in path {
            message.push_str("\n    ");
            message.push_str(&key(element));
        }
        message.push_str("\n.-> ");
        message.push_str(&key(&cycle[0]));
        if cycle.len() == 1 {
            message.push_str(" [self-edge]\n`--");
        } else {
            for element in &cycle[1..] {
                message.push_str("\n|   ");
                message.push_str(&key(element));
            }
            message.push_str("\n`-- ");
            message.push_str(&key(&cycle[0]));
        }
        located(
            head.location.as_deref(),
            crate::rule_errors::RuleEvents {
                rule_class: head.rule_class_name(),
                label: &crate::rule_declarations::canonical_form(&head.label),
            }
            .rule_message(&message),
        )
    }

    /// The alias chain from `label` (AliasProvider.getAliasChain) and its
    /// end (AliasConfiguredTarget.getLabel, rules/AliasConfiguredTarget.java:178-185).
    fn alias_chain(
        &self,
        configured: &ConfiguredNodes,
        config: ConfigId,
        label: &Label,
    ) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        while let Some(actual) = configured
            .get(&ConfiguredTargetKey::new(current.clone(), config))
            .and_then(|node| node.actual.clone())
        {
            if chain.contains(&current) {
                break;
            }
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// The configured actual of an alias-like rule, as label text: `actual`
    /// after select() resolution, or the label_flag/label_setting value
    /// (LabelBuildSettings.ACTUAL, rules/LabelBuildSettings.java:62-83).
    fn alias_actual(
        &self,
        rule: &Rule,
        configuration: &Configuration,
        build_setting: Option<&BuildSettingInfo>,
    ) -> Result<String, String> {
        if let Some(info) = build_setting {
            return match info.value(configuration) {
                SettingValue::Label(label) => Ok(label.to_string()),
                other => Err(format!(
                    "internal error: label build setting {} has value {other}",
                    rule.label
                )),
            };
        }
        match rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("actual"))
        {
            Some(crate::custom_rule::AttrValue::Label(Some(actual))) => Ok(actual.clone()),
            _ => Err(format!(
                "internal error: alias {} has no resolved actual",
                rule.label
            )),
        }
    }

    /// The BuildSettingProvider of a build-setting rule: Starlark rules with
    /// `build_setting` (RuleConfiguredTargetBuilder.java:250-259) and
    /// label_flag/label_setting (NODEP_LABEL, LabelBuildSettings.java:97).
    fn build_setting_info(&self, rule: &Rule) -> Result<Option<BuildSettingInfo>, String> {
        if let Some(custom) = &rule.custom
            && let Some(setting) = &custom.build_setting
        {
            return Ok(Some(BuildSettingInfo {
                label: rule.label.clone(),
                ty: SettingType::of(setting.kind),
                default: crate::analysis::setting_default(custom)
                    .map_err(|error| format!("{}: {error}", rule.label))?,
                is_flag: setting.flag,
                repeatable: setting.repeatable,
                scope: custom.declaration.explicit_scope.clone(),
            }));
        }
        let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind else {
            return Ok(None);
        };
        let Some(crate::custom_rule::AttrValue::Label(Some(default))) = rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("build_setting_default"))
        else {
            return Err(format!(
                "internal error: {kind} {} has no build_setting_default",
                rule.label
            ));
        };
        Ok(Some(BuildSettingInfo {
            label: rule.label.clone(),
            ty: SettingType::Label,
            default: SettingValue::Label(self.resolve_rule_src(rule, default)?),
            is_flag: kind == "label_flag",
            repeatable: false,
            // The native `scope` attribute (rules/LabelBuildSettings.java:93-96)
            // is stored only when explicitly specified.
            scope: match rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("scope"))
            {
                Some(crate::custom_rule::AttrValue::String(scope)) => {
                    crate::starlark_options::ScopeAttribute::Value(scope.clone())
                }
                _ => crate::starlark_options::ScopeAttribute::Unspecified,
            },
        }))
    }

    /// RuleContext.Builder.validateDirectPrerequisiteType for an attribute
    /// that allows no files (analysis/RuleContext.java:1845-1880): the end of
    /// the alias chain must be a rule. A missing input file is `rule 'L' does
    /// not exist` (`target` when the name has a `.`); a file is `KIND 'L' is
    /// misplaced here (expected no files)` with the alias description
    /// (RuleContext.java:1819-1830, AliasProvider.java:123-148).
    fn no_files_error(
        &mut self,
        chain: &[Label],
        actual: &Label,
    ) -> Result<Option<String>, String> {
        let misplaced = |kind: &str| {
            Some(format!(
                "{} is misplaced here (expected no files)",
                describe_with_aliases(chain, actual, kind)
            ))
        };
        Ok(match self.target(actual)? {
            Target::Source(path) if crate::observed_fs::symlink_metadata(&path).is_err() => {
                Some(format!(
                    "{} '{}' does not exist",
                    if actual.name.contains('.') {
                        "target"
                    } else {
                        "rule"
                    },
                    crate::rule_declarations::canonical_form(actual)
                ))
            }
            Target::Source(_) => misplaced("source file"),
            Target::Output(..) => misplaced("generated file"),
            Target::Rule(_) => None,
        })
    }

    /// The genrule `toolchains` prerequisites (GenRuleBaseRule.java:322-333):
    /// no files (validateDirectPrerequisiteType), and rules must provide
    /// TemplateVariableInfo or ToolchainTypeInfo
    /// (RuleContext.checkRuleDependencyMandatoryProviders, analysis/RuleContext.java:2070-2095),
    /// attribute errors at the rule's location.
    fn genrule_toolchains_errors(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        configured: &ConfiguredNodes,
        custom_providers: &BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
    ) -> Result<(), String> {
        let Some(native) = &rule.native else {
            return Ok(());
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let mut errors = Vec::new();
        for text in native.labels("toolchains") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            let (chain, actual) = self.alias_chain(configured, config, &dependency);
            let error = match self.target(&actual)? {
                Target::Rule(_) => {
                    let template = custom_providers
                        .get(&ConfiguredTargetKey::new(dependency.clone(), config))
                        .is_some_and(|providers| {
                            providers.contains_key(crate::template_variable_info::PROVIDER_ID)
                        });
                    let toolchain_type = configured
                        .get(&ConfiguredTargetKey::new(actual.clone(), config))
                        .is_some_and(|node| node.toolchain_type.is_some());
                    (!template && !toolchain_type).then(|| {
                        format!(
                            "'{}' does not have mandatory providers: 'TemplateVariableInfo' or 'ToolchainTypeInfo'",
                            crate::rule_declarations::canonical_form(&actual)
                        )
                    })
                }
                _ => self.no_files_error(&chain, &actual)?,
            };
            if let Some(error) = error {
                errors.push(
                    crate::rule_errors::RuleEvents {
                        rule_class: "genrule",
                        label: &shown,
                    }
                    .attribute_message("toolchains", &error),
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(crate::rule_errors::located_events(
                rule.location.as_deref(),
                errors,
            ))
        }
    }

    /// A test_suite's `tests` (TestSuiteRule.java:96-104, `allowedFileTypes()`
    /// = no files): a file prerequisite is RuleContext's attribute error;
    /// then TestSuite.checkTestsAndSuites (rules/test/TestSuite.java:104-118):
    /// a prerequisite (its alias chain's end) that is neither a test rule
    /// (TestTagsProvider) nor a test_suite (TransitiveTestsProvider) is `in
    /// tests attribute of test_suite rule L: expecting a test or a test_suite
    /// rule but 'M' is not one`, M being `dep.getLabel()`: for an alias the
    /// end of its chain (AliasConfiguredTarget.getLabel returns its actual's
    /// label, rules/AliasConfiguredTarget.java:171-179).
    fn test_suite_errors(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        configured: &ConfiguredNodes,
    ) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) else {
            return Ok(errors);
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let mut members = Vec::new();
        for text in native.labels("tests") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            let (chain, actual) = self.alias_chain(configured, config, &dependency);
            match self.no_files_error(&chain, &actual)? {
                Some(error) => {
                    errors.push(format!(
                        "in tests attribute of test_suite rule {shown}: {error}"
                    ));
                }
                None => members.push(actual),
            }
        }
        if !errors.is_empty() {
            return Ok(errors);
        }
        for actual in members {
            let accepted = match self.target(&actual)? {
                Target::Rule(member) => {
                    test_suite_filter_tags(&member).is_some()
                        || matches!(member.kind, RuleKind::Native("test_suite"))
                }
                _ => false,
            };
            if !accepted {
                errors.push(format!(
                    "in tests attribute of test_suite rule {shown}: expecting a test or a test_suite rule but '{actual}' is not one"
                ));
            }
        }
        Ok(errors)
    }

    /// `package_metadata` allows no files (`allowedFileTypes(FileTypeSet.NO_FILE)`,
    /// analysis/BaseRuleClasses.java:291-297): every file prerequisite is an
    /// attribute error `in package_metadata attribute of KIND rule L: ...`
    /// (unlocated; the caller reports them at the rule's location). The
    /// edges are analyzed in the no-config configuration.
    fn package_metadata_errors(
        &mut self,
        rule: &Rule,
        configured: &ConfiguredNodes,
    ) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        if rule.package_metadata.is_empty() {
            return Ok(errors);
        }
        let no_config = self.configurations.intern_no_config();
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        for text in &rule.package_metadata {
            let dependency = self.resolve_rule_src(rule, text)?;
            let (chain, actual) = self.alias_chain(configured, no_config, &dependency);
            if let Some(error) = self.no_files_error(&chain, &actual)? {
                errors.push(format!(
                    "in package_metadata attribute of {} rule {shown}: {error}",
                    rule.rule_class_name()
                ));
            }
        }
        Ok(errors)
    }

    /// RuleContext.Builder.validateDirectPrerequisite for a file at the end
    /// of the alias chain of an attribute with an extension list
    /// (analysis/RuleContext.java:2165-2172): validateDirectPrerequisiteType
    /// (:1845-1886) tests the label name, so a mismatch is `target 'L' does
    /// not exist` for a missing input file (the set is never NO_FILE) or `KIND
    /// 'L' is misplaced here (expected TYPES)`; then
    /// validateDirectPrerequisiteFileTypes (:1945-1994) tests the file's
    /// own name (FileConfiguredTarget.java:94-96). Rules are checked against
    /// their files to build at analysis (crate::analysis).
    fn file_type_errors(
        &mut self,
        chain: &[Label],
        actual: &Label,
        spec: &crate::custom_rule::AttrSpec,
        name: &str,
        rule_class: &str,
    ) -> Result<Vec<String>, String> {
        let allowed = spec.allowed_file_types();
        let shown = crate::rule_declarations::canonical_form(actual);
        let kind = match self.target(actual)? {
            Target::Rule(_) => return Ok(Vec::new()),
            Target::Source(path) => {
                if !allowed.matches(&actual.name)
                    && crate::observed_fs::symlink_metadata(&path).is_err()
                {
                    None
                } else {
                    Some("source file")
                }
            }
            Target::Output(..) => Some("generated file"),
        };
        let mut errors = Vec::new();
        if !allowed.matches(&actual.name) {
            errors.push(match kind {
                Some(kind) => format!(
                    "{} is misplaced here (expected {allowed})",
                    describe_with_aliases(chain, actual, kind)
                ),
                None => format!("target '{shown}' does not exist"),
            });
        }
        let file_name = actual.name.rsplit('/').next().unwrap_or(&actual.name);
        errors.extend(spec.produced_file_type_error(
            name,
            rule_class,
            &shown,
            std::iter::once(file_name),
        ));
        Ok(errors)
    }

    /// RuleContext.Builder.build for a Starlark rule, after its prerequisites
    /// are analyzed and before the implementation runs
    /// (analysis/RuleContext.java:1491-1503): createTargetMap validates every
    /// prerequisite of every attribute, in attribute declaration order then
    /// prerequisite order (validateDirectPrerequisite, :1708-1740,
    /// 2165-2172), then validateExtraPrerequisites reports empty attributes
    /// and duplicate labels (:2102-2113, 1532-1568). Every error is an
    /// attribute error `in A attribute of KIND rule L: ...` at the rule's
    /// location, all reported; identical events are reported once (a
    /// configured target's events are a NestedSet, whose builder keeps one
    /// of equal elements: skyframe/SkyFunctionEnvironment.java:318-321,
    /// collect/nestedset/NestedSetBuilder.java:43, 86).
    ///
    /// Without `allow_files` a Starlark label attribute allows no files and
    /// checks labels strictly (StarlarkAttrModule.java:431-445,
    /// Attribute.java:2349); extension lists check file prerequisites by
    /// name (file_type_errors); other prerequisites are checked against their
    /// providers and files to build (analysis::prerequisite_errors).
    fn starlark_prerequisite_errors(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        exec_config: ConfigId,
        configured: &ConfiguredNodes,
        targets: &BTreeMap<String, DependencyTarget>,
    ) -> Result<(), String> {
        let Some(custom) = &rule.custom else {
            return Ok(());
        };
        let config = key.config;
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let consumer = analysis::Consumer {
            rule_class: rule.rule_class_name(),
            label: &shown,
        };
        let events = crate::rule_errors::RuleEvents {
            rule_class: rule.rule_class_name(),
            label: &shown,
        };
        let prefix = |name: &str, spec: &crate::custom_rule::AttrSpec| {
            events.attribute_message(&crate::custom_rule::attribute_message_name(name, spec), "")
        };
        // The built-in `package_metadata` attribute precedes the rule's own
        // attributes in the rule class (BaseRuleClasses.java:291-297); its
        // no-config prerequisites are file-checked first (N28c).
        let mut errors = self.package_metadata_errors(rule, configured)?;
        // The built-in `toolchains` attribute follows (StarlarkRuleClassFunctions.baseRule,
        // :159-194): TemplateVariableInfo rules, no files.
        let toolchains_spec = crate::custom_rule::toolchains_attribute_spec();
        for text in custom.declaration.builtin.target.labels("toolchains") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            let (chain, actual) = self.alias_chain(configured, config, &dependency);
            let found = match self.target(&actual)? {
                Target::Rule(_) => {
                    let map_key = analysis::dependency_key(&dependency.to_string(), false);
                    let target = targets.get(&map_key).ok_or_else(|| {
                        format!(
                            "internal error: prerequisite {map_key} of {shown} (attribute toolchains) has no analyzed dependency entry"
                        )
                    })?;
                    // N50's allow_rules texts describe the target with its
                    // alias chain (AliasProvider.describeTargetWithAliases).
                    let described = target.rule_class.as_ref().map(|class| {
                        describe_with_aliases(&chain, &actual, &format!("{class} rule"))
                    });
                    analysis::prerequisite_errors(
                        "toolchains",
                        &toolchains_spec,
                        target,
                        consumer,
                        described.as_deref(),
                    )
                }
                _ => self.no_files_error(&chain, &actual)?.into_iter().collect(),
            };
            for error in found {
                errors.push(format!("{}{error}", prefix("toolchains", &toolchains_spec)));
            }
        }
        for name in &custom.declaration.attr_order {
            let (Some(spec), Some(value)) = (custom.schema.get(name), custom.attrs.get(name))
            else {
                continue;
            };
            let allowed = spec.allowed_file_types();
            let labels = match value {
                crate::custom_rule::AttrValue::Label(Some(label)) => vec![label.clone()],
                crate::custom_rule::AttrValue::Labels(labels) => labels.clone(),
                // DictType.visitLabels of LabelKeyedDictType: the keys
                // (packages/Type.java:504-514, BuildType.java:436-439).
                crate::custom_rule::AttrValue::LabelKeyedStringDict(entries) => {
                    entries.iter().map(|(label, _)| label.clone()).collect()
                }
                crate::custom_rule::AttrValue::StringKeyedLabelDict(entries) => {
                    entries.iter().map(|(_, label)| label.clone()).collect()
                }
                crate::custom_rule::AttrValue::LabelListDict(entries) => entries
                    .iter()
                    .flat_map(|(_, labels)| labels.iter().cloned())
                    .collect(),
                _ => continue,
            };
            // No prerequisites, nothing to validate (and no edge, so a
            // transitioned attribute has no configuration).
            if labels.is_empty() {
                continue;
            }
            // The attribute's edge configuration, as the build loop maps the
            // dependency edges (EdgeCfg): the exec configuration, the
            // attribute's Starlark-transition configuration (N18c,
            // Self::edge_configs) or the target's; the dependency map key
            // follows the same edge (analysis::attribute_dependency_key).
            let edge_config = match &spec.declaration.cfg {
                crate::rule_declarations::Cfg::Exec(group)
                    if group
                        .as_deref()
                        .is_none_or(|group| group == crate::exec_groups::DEFAULT_EXEC_GROUP_NAME) =>
                {
                    exec_config
                }
                // A named exec group's configuration (N35,
                // Engine::group_exec_configurations); none without toolchain
                // contexts (a no-op transition).
                crate::rule_declarations::Cfg::Exec(Some(group)) => self
                    .group_exec_configurations
                    .get(&(key.clone(), group.clone()))
                    .copied()
                    .unwrap_or(config),
                crate::rule_declarations::Cfg::Starlark {
                    analysis_test: false,
                    ..
                } => self
                    .edge_configs
                    .get(key)
                    .and_then(|configs| configs.get(name))
                    .map(|branches| branches[0].1)
                    .ok_or_else(|| {
                        format!(
                            "internal error: no transitioned configuration for {shown} attribute {name}"
                        )
                    })?,
                _ => config,
            };
            let prerequisites = if let Some(split) = custom.split_dependencies.get(name) {
                split
                    .prerequisites
                    .iter()
                    .map(|p| (p.label.clone(), p.config, p.map_key.clone()))
                    .collect::<Vec<_>>()
            } else {
                labels
                    .iter()
                    .map(|text| {
                        let label = self.resolve_rule_src(rule, text)?;
                        let map_key =
                            analysis::attribute_dependency_key(&label.to_string(), name, spec);
                        Ok((label, edge_config, map_key))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            };
            for (dependency, edge_config, map_key) in prerequisites {
                let (chain, actual) = self.alias_chain(configured, edge_config, &dependency);
                let found = match self.target(&actual)? {
                    Target::Rule(_) => {
                        // Every prerequisite was analyzed before this point;
                        // a missing entry must never skip validation.
                        let target = targets.get(&map_key).ok_or_else(|| {
                            format!(
                                "internal error: prerequisite {map_key} of {shown} (attribute {name}) has no analyzed dependency entry"
                            )
                        })?;
                        let described = target.rule_class.as_ref().map(|class| {
                            describe_with_aliases(&chain, &actual, &format!("{class} rule"))
                        });
                        analysis::prerequisite_errors(
                            name,
                            spec,
                            target,
                            consumer,
                            described.as_deref(),
                        )
                    }
                    _ => match allowed {
                        crate::custom_rule::AllowedFileTypes::None => {
                            self.no_files_error(&chain, &actual)?.into_iter().collect()
                        }
                        // A file is its single file to build, of any type.
                        crate::custom_rule::AllowedFileTypes::Any => Vec::new(),
                        crate::custom_rule::AllowedFileTypes::Extensions(_) => self
                            .file_type_errors(
                                &chain,
                                &actual,
                                spec,
                                name,
                                rule.rule_class_name(),
                            )?,
                    },
                };
                for error in found {
                    errors.push(format!("{}{error}", prefix(name, spec)));
                }
            }
        }
        errors.extend(analysis::empty_attribute_errors(rule, custom));
        // Recorded by the target's first analysis pass (Self::analyze).
        errors.extend(self.deferred_duplicates.remove(key).ok_or_else(|| {
            format!("internal error: {shown} has no recorded duplicate-label check")
        })?);
        let mut seen = BTreeSet::new();
        errors.retain(|error| seen.insert(error.clone()));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }

    /// The `flag_values` and `constraint_values` prerequisites of a
    /// `config_setting`: every key and label is analyzed first, then each
    /// edge is validated like RuleContext.Builder does
    /// (validateDirectPrerequisiteType, analysis/RuleContext.java:1845-1880,
    /// with no allowed file types, ConfigRuleClasses.java:292-294, 314-316;
    /// then visibility, testonly and deprecation), in attribute order. All
    /// errors are reported.
    fn config_setting_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<
        (
            Vec<crate::config_setting::FlagPrerequisite>,
            Vec<crate::config_setting::ConstraintPrerequisite>,
        ),
        String,
    > {
        let (RuleKind::Native("config_setting"), Some(native)) = (&rule.kind, &rule.native) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut entries = match native.attrs.get("flag_values") {
            Some(crate::custom_rule::AttrValue::LabelKeyedStringDict(entries)) => entries.clone(),
            _ => Vec::new(),
        };
        let explicit_flag_labels = entries
            .iter()
            .map(|(key, _)| self.resolve_rule_src(rule, key))
            .collect::<Result<BTreeSet<_>, _>>()?;
        // ConfigSetting.getSettings: values whose keys are flag aliases are
        // Starlark expectations, including aliases of removed native flags.
        let aliases = &self.configurations.get(config).native.flag_aliases;
        let mut aliased = Vec::new();
        if let Some(crate::custom_rule::AttrValue::StringDict(values)) = native.attrs.get("values")
        {
            aliased.extend(values.iter().filter_map(|(name, value)| {
                aliases
                    .get(name)
                    .map(|label| (name.clone(), label.clone(), value.clone()))
            }));
        }
        if let Some(crate::custom_rule::AttrValue::StringDict(defines)) =
            native.attrs.get("define_values")
            && let Some(label) = aliases.get("define")
        {
            aliased.extend(defines.iter().map(|(name, value)| {
                (
                    "define".to_owned(),
                    label.clone(),
                    format!("{name}={value}"),
                )
            }));
        }
        let alias_labels = aliased
            .iter()
            .map(|(_, label, _)| label.clone())
            .collect::<BTreeSet<_>>();
        for (name, label, value) in aliased {
            let mut found = false;
            for (key, existing) in &entries {
                if self.resolve_rule_src(rule, key)? == label {
                    if existing != &value {
                        return Err(format!(
                            "Conflicting flag value expectations: {}: --{name} is a flag alias for --{label}, but values and flag_values match different values ({value:?} and {existing:?})",
                            rule.label
                        ));
                    }
                    found = true;
                }
            }
            if !found {
                entries.push((
                    format!("@@{}//{}:{}", label.repository, label.package, label.name),
                    value,
                ));
            }
        }
        let mut keys = Vec::new();
        for (key, value) in entries {
            let dependency = self.resolve_rule_src(rule, &key)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config, active, configured, plan)?;
            keys.push(("flag_values", dependency, Some(value)));
        }
        for text in native.labels("constraint_values") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config, active, configured, plan)?;
            keys.push(("constraint_values", dependency, None));
        }
        // UserDefinedFlagMatch reads the FeatureFlagInfo a key returns
        // (ConfigSetting.java:651-687): a Starlark key's implementation runs
        // first (Self::plan_pending).
        let starlark_keys = keys.iter().any(|(attribute, dependency, _)| {
            *attribute == "flag_values"
                && matches!(
                    self.target(&self.alias_chain(configured, config, dependency).1),
                    Ok(Target::Rule(dep)) if matches!(dep.kind, RuleKind::Custom)
                )
        });
        if starlark_keys {
            self.plan_pending(rule, plan, configured)?;
        }
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        let mut flags = Vec::new();
        let mut constraints = Vec::new();
        for (attribute, dependency, value) in keys {
            if self.cquery_capture.is_some() {
                if attribute != "flag_values" || explicit_flag_labels.contains(&dependency) {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        attribute,
                        false,
                        false,
                    );
                }
                if attribute == "flag_values" && alias_labels.contains(&dependency) {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        ":flag_alias_settings",
                        true,
                        false,
                    );
                }
            }
            let (chain, actual) = self.alias_chain(configured, config, &dependency);
            let is_source = matches!(self.target(&actual)?, Target::Source(_));
            if let Some(error) = self.no_files_error(&chain, &actual)? {
                errors.push(format!(
                    "in {attribute} attribute of config_setting rule {shown}: {error}"
                ));
            }
            let visible = match self.target(&dependency)? {
                Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )?,
                Target::Source(_) => self.visible(
                    &self.source_visibility(&dependency),
                    &dependency.package_id(),
                    Some(&requesting_package),
                )?,
            };
            // Formatted only for a reported error.
            let description = || describe_with_aliases(&chain, &actual, "target");
            if !visible {
                errors.push(visibility_message(rule, &description(), is_source, None));
            }
            if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                errors.push(error);
            }
            let node = configured.get(&ConfiguredTargetKey::new(dependency.clone(), config));
            match value {
                Some(value) => flags.push(crate::config_setting::FlagPrerequisite {
                    feature_flag: node.and_then(|node| node.feature_flag.clone()).or_else(|| {
                        self.planned_feature_flag(&ConfiguredTargetKey::new(
                            dependency.clone(),
                            config,
                        ))
                    }),
                    specified: dependency,
                    actual,
                    value,
                    setting: node.and_then(|node| node.build_setting.clone()),
                }),
                None => constraints.push(crate::config_setting::ConstraintPrerequisite {
                    actual,
                    value: node.and_then(|node| node.constraint_value.clone()),
                }),
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok((flags, constraints))
    }

    /// The label attributes of `constraint_setting`
    /// (ConstraintSettingRule.java:84-89), `constraint_value`
    /// (ConstraintValueRule.java:56-63) and `platform` (PlatformRule.java:75-89,
    /// 149-152), in rule-class attribute order, analyzed and validated (the
    /// resolved labels per attribute).
    fn platform_rule_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<Vec<Label>>, String> {
        let attributes = platform_prerequisite_attributes(rule);
        if attributes.is_empty() {
            return Ok(Vec::new());
        }
        self.native_prerequisites(rule, config, &attributes, active, configured, plan)
    }

    /// RuleContext.Builder.createTargetMap for native label attributes
    /// (analysis/RuleContext.java:1708-1745, 2165-2172): every prerequisite is
    /// analyzed, then validated in attribute order — validateRuleDependency
    /// for rules (allowed rule classes, else mandatory providers, :2002-2093),
    /// the no-files check for files (:1845-1880), then visibility, testonly
    /// and deprecation. All errors are reported at the rule's location.
    fn native_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attributes: &[PrerequisiteAttribute],
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<Vec<Label>>, String> {
        let mut resolved = Vec::new();
        for attribute in attributes {
            let mut labels = Vec::new();
            for text in &attribute.labels {
                let dependency = self.resolve_rule_src(rule, text)?;
                self.target(&dependency)
                    .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
                self.analyze(&dependency, config, active, configured, plan)?;
                if self.cquery_capture.is_some() {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        attribute.name,
                        false,
                        false,
                    );
                }
                labels.push(dependency);
            }
            resolved.push(labels);
        }
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        for (attribute, labels) in attributes.iter().zip(&resolved) {
            let prefix = crate::rule_errors::RuleEvents {
                rule_class: &class,
                label: &shown,
            }
            .attribute_message(attribute.name, "");
            for dependency in labels {
                let (chain, actual) = self.alias_chain(configured, config, dependency);
                let actual_target = self.target(&actual)?;
                let is_source = matches!(actual_target, Target::Source(_));
                let type_error = match &actual_target {
                    Target::Rule(actual_rule) => rule_dependency_error(
                        attribute,
                        &chain,
                        &actual,
                        actual_rule,
                        configured.get(&ConfiguredTargetKey::new(actual.clone(), config)),
                    ),
                    _ => self.no_files_error(&chain, &actual)?,
                };
                if let Some(error) = type_error {
                    errors.push(format!("{prefix}{error}"));
                }
                let visible = match self.target(dependency)? {
                    Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                        &dep.visibility,
                        &rule_authority(&dep),
                        Some(&requesting_package),
                    )?,
                    Target::Source(_) => self.visible(
                        &self.source_visibility(dependency),
                        &dependency.package_id(),
                        Some(&requesting_package),
                    )?,
                };
                // Formatted only for a reported error.
                let description = || describe_with_aliases(&chain, &actual, "target");
                if !visible {
                    errors.push(visibility_message(rule, &description(), is_source, None));
                }
                if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                    errors.push(error);
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok(resolved)
    }

    /// The providers of `constraint_setting` (rules/platform/ConstraintSetting.java:227-304),
    /// `constraint_value` (ConstraintValue.java:338-353) and `platform`
    /// (Platform.java:44-120) from their validated prerequisites.
    fn platform_rule_providers(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
        configured: &ConfiguredNodes,
    ) -> Result<PlatformProviders, String> {
        let Some(native) = rule.native.clone() else {
            return Ok((None, None, None));
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let attribute_error = |attribute: &str, message: &str| {
            located(
                rule.location.as_deref(),
                crate::rule_errors::RuleEvents {
                    rule_class: &class,
                    label: &shown,
                }
                .attribute_message(attribute, message),
            )
        };
        let node = |label: &Label| configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        let internal = |what: &str| {
            format!(
                "internal error: validated prerequisite of {} lacks {what}",
                rule.label
            )
        };
        match rule.kind {
            RuleKind::Native("constraint_setting") => {
                // validateDefaultConstraintValue (ConstraintSetting.java:270-304).
                let mut default_value = None;
                if let Some(crate::custom_rule::AttrValue::Label(Some(text))) =
                    native.attrs.get("default_constraint_value")
                {
                    let default = self.resolve_rule_src(rule, text)?;
                    if default.package_id() != rule.label.package_id() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            "The default constraint value must be defined in the same package as the constraint setting itself.",
                        ));
                    }
                    if self.target(&default).is_err() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            &format!(
                                "The default constraint value '{}' does not exist",
                                crate::rule_declarations::canonical_form(&default)
                            ),
                        ));
                    }
                    default_value = Some(default);
                }
                let refined = match prerequisites.first().and_then(|labels| labels.first()) {
                    Some(label) => Some(Box::new(
                        node(label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    )),
                    None => None,
                };
                Ok((
                    Some(ConstraintSettingInfo {
                        label: rule.label.clone(),
                        default_value,
                        refined,
                    }),
                    None,
                    None,
                ))
            }
            RuleKind::Native("constraint_value") => {
                let setting = prerequisites
                    .first()
                    .and_then(|labels| labels.first())
                    .and_then(node)
                    .and_then(|node| node.constraint_setting.clone())
                    .ok_or_else(|| internal("ConstraintSettingInfo"))?;
                Ok((
                    None,
                    Some(ConstraintValueInfo {
                        setting: Arc::new(setting),
                        label: rule.label.clone(),
                    }),
                    None,
                ))
            }
            RuleKind::Native("platform") => {
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let mut constraint_values = Vec::new();
                for label in provided(0) {
                    constraint_values.push(
                        node(&label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    );
                }
                let mut parents = Vec::new();
                for label in provided(1) {
                    parents.push(
                        node(&label)
                            .and_then(|node| node.platform.clone())
                            .ok_or_else(|| internal("PlatformInfo"))?,
                    );
                }
                let required_settings = provided(2)
                    .iter()
                    .map(|label| self.alias_chain(configured, config, label).1)
                    .collect();
                let mut allowed_toolchain_types = Vec::new();
                for text in native.labels("allowed_toolchain_types") {
                    allowed_toolchain_types.push(self.resolve_rule_src(rule, &text)?);
                }
                let exec_properties = match native.attrs.get("exec_properties") {
                    Some(crate::custom_rule::AttrValue::StringDict(entries)) => entries.clone(),
                    _ => Vec::new(),
                };
                let attributes = crate::platforms::PlatformAttributes {
                    parents,
                    constraint_values,
                    remote_execution_properties: native
                        .string("remote_execution_properties")
                        .unwrap_or_default()
                        .to_owned(),
                    exec_properties,
                    flags: native.strings("flags"),
                    required_settings,
                    check_toolchain_types: matches!(
                        native.attrs.get("check_toolchain_types"),
                        Some(crate::custom_rule::AttrValue::Bool(true))
                    ),
                    allowed_toolchain_types,
                    missing_toolchain_error: native
                        .string("missing_toolchain_error")
                        .unwrap_or(crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR)
                        .to_owned(),
                };
                let mapping = self.repositories.mapping("").cloned().unwrap_or_default();
                let platform =
                    crate::platforms::create_platform(&rule.label, attributes, &|label| {
                        crate::platforms::shorthand_display_form(label, &mapping)
                    })
                    .map_err(|errors| {
                        errors
                            .iter()
                            .map(|error| attribute_error(error.attribute, &error.message))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })?;
                Ok((None, None, Some(Arc::new(platform))))
            }
            _ => Ok((None, None, None)),
        }
    }

    /// The target platform of configuration `config`
    /// (PlatformConfiguration.getTargetPlatform, analysis/PlatformConfiguration.java:84-86),
    /// resolved like PlatformFunction. Loading errors carry Bazel's
    /// exception-class prefix (PlatformError::target_text).
    fn target_platform(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, String> {
        let label = self
            .configurations
            .get(config)
            .native
            .target_platform()
            .clone();
        self.platform_info(&label, config, configured, plan)
            .map_err(|error| error.target_text())
    }

    /// Whether the target platform of `config` is the default host
    /// platform, which the native profile resolves only where a constraint
    /// needs it.
    fn default_target_platform(&self, config: ConfigId) -> bool {
        *self.configurations.get(config).native.target_platform()
            == crate::configuration::default_host_platform()
    }

    /// The modules' execution platform registrations (patterns with the
    /// repository each is parsed in), computed once per build: the other
    /// modules' need the module graph, resolved when `resolve` (a rule that
    /// resolves toolchains) or already resolved
    /// (crate::repository::RepositorySet::execution_platform_registrations);
    /// an incomplete list is not kept.
    fn module_execution_platforms(
        &mut self,
        resolve: bool,
    ) -> Result<Vec<(String, String)>, String> {
        if let Some(found) = &self.module_execution_platforms {
            return found.clone();
        }
        let flatten = |registrations: Vec<(String, Vec<String>)>| -> Vec<(String, String)> {
            registrations
                .into_iter()
                .flat_map(|(repository, patterns)| {
                    patterns
                        .into_iter()
                        .map(move |pattern| (repository.clone(), pattern))
                })
                .collect()
        };
        match self.repositories.execution_platform_registrations(resolve) {
            Ok((registrations, complete)) => {
                let found = flatten(registrations);
                if complete {
                    self.module_execution_platforms = Some(Ok(found.clone()));
                }
                Ok(found)
            }
            Err(error) => {
                self.module_execution_platforms = Some(Err(error.clone()));
                Err(error)
            }
        }
    }

    /// ToolchainException.asConfiguredValueCreationException
    /// (skyframe/toolchains/ToolchainException.java:67-91) as reported by
    /// ConfiguredTargetFunction (:394-404): `While resolving toolchains for
    /// target L (CONFIG): MESSAGE` at the rule's location, after `events`
    /// (CONFIG is the native identifier, docs/PLATFORMS.md).
    fn toolchain_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        events: &str,
        message: &str,
    ) -> String {
        // An aspect's own toolchain resolution: AspectFunction reports the
        // ToolchainException's message itself (AspectCreationException with
        // a LabelCause, AspectFunction.java:510-515), no target prefix.
        if self.resolving_aspect_toolchains {
            return if events.is_empty() {
                message.to_owned()
            } else {
                format!("{events}\n{message}")
            };
        }
        let id = config.to_string();
        let error = located(
            rule.location.as_deref(),
            format!(
                "While resolving toolchains for target {} ({}): {message}",
                crate::rule_declarations::canonical_form(&rule.label),
                &id[..7]
            ),
        );
        if events.is_empty() {
            error
        } else {
            format!("{events}\n{error}")
        }
    }

    /// An InvalidPlatformException of PlatformKeys (PlatformKeys.java:150-166):
    /// a loading error names the label as a non-platform (paired capture
    /// B2/host_host_platform_fg_platforms_p_mac); other errors keep their
    /// reported lines and wrap the last one.
    fn platform_resolution_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        label: &Label,
        error: PlatformError,
    ) -> String {
        let text = match error {
            PlatformError::Missing { .. } => crate::platforms::not_a_platform(label),
            PlatformError::Other(text) => text,
        };
        match text.rsplit_once('\n') {
            Some((events, last)) => self.toolchain_error(rule, config, events, last),
            None => self.toolchain_error(rule, config, "", &text),
        }
    }

    fn resolution_error(&self, rule: &Rule, config: ConfigId, error: ResolutionError) -> String {
        match error {
            ResolutionError::Toolchain(message) => self.toolchain_error(rule, config, "", &message),
            ResolutionError::Reported(events, message) => {
                self.toolchain_error(rule, config, &events, &message)
            }
            ResolutionError::Propagated(message) => message,
            ResolutionError::Aborted(message) => format!("{ANALYSIS_ABORTED}{message}"),
        }
    }

    /// Record `implementation` as a toolchain implementation of a parent
    /// whose execution platform is `exec_platform`. Bazel keys the
    /// implementation's configured target by that platform
    /// (ConfiguredTargetKey.executionPlatformLabel, DependencyResolver.java:835-866);
    /// the native key has no execution platform, so one implementation
    /// needed for two execution platforms in one configuration fails
    /// explicitly.
    fn toolchain_implementation(
        &mut self,
        implementation: &Label,
        config: ConfigId,
        exec_platform: &Label,
    ) -> Result<(), String> {
        let key = ConfiguredTargetKey::new(implementation.clone(), config);
        match self.toolchain_implementations.get(&key) {
            Some(existing) if existing != exec_platform => Err(format!(
                "toolchain implementation {} is needed for the execution platforms {} and {} in one configuration: configured targets keyed by their execution platform (ConfiguredTargetKey.executionPlatformLabel) are not implemented in the native profile",
                crate::rule_declarations::canonical_form(implementation),
                crate::rule_declarations::canonical_form(existing),
                crate::rule_declarations::canonical_form(exec_platform),
            )),
            Some(_) => Ok(()),
            None => {
                self.toolchain_implementations
                    .insert(key, exec_platform.clone());
                Ok(())
            }
        }
    }

    /// ToolchainContextUtil.getUnloadedToolchainContextsInputs
    /// (skyframe/toolchains/ToolchainContextUtil.java:60-165): the rule
    /// class's toolchain types and execution constraints; a toolchain
    /// implementation forces its parent's execution platform.
    fn toolchain_context_key(
        &self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<ToolchainContextKey, String> {
        let mut key = ToolchainContextKey {
            forced_exec: self
                .toolchain_implementations
                .get(&ConfiguredTargetKey::new(rule.label.clone(), config))
                .cloned(),
            ..Default::default()
        };
        if let Some(custom) = &rule.custom {
            key.types = requirements(&custom.declaration.toolchain_types)?;
            key.exec_constraints = self.default_exec_constraints(rule)?;
        }
        // updateToolchainTypesFromAttribute (ToolchainContextUtil.java:61-64,
        // 94-117): a genrule's (nonconfigurable) `toolchains` are optional
        // toolchain types, silently ignored when not a toolchain_type
        // (Make-variable targets).
        if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
            for text in native.labels("toolchains") {
                let requirement = ToolchainTypeRequirement {
                    label: self.resolve_rule_src(rule, &text)?,
                    mandatory: false,
                    ignore_if_invalid: true,
                };
                if !key.types.contains(&requirement) {
                    key.types.push(requirement);
                }
            }
        }
        Ok(key)
    }

    /// Label.getDisplayForm with the main repository's mapping
    /// (cmdline/Label.java:511-513, RepositoryName.getDisplayForm).
    fn display_form(&self, label: &Label) -> String {
        if label.repository.is_empty() {
            return label.to_string();
        }
        let mapping = self.repositories.mapping("").cloned().unwrap_or_default();
        let repository = mapping
            .iter()
            .find(|(_, canonical)| **canonical == label.repository)
            .map_or_else(
                || format!("@@{}", label.repository),
                |(apparent, _)| format!("@{apparent}"),
            );
        format!("{repository}//{}:{}", label.package, label.name)
    }

    /// ToolchainContextUtil.getExecutionPlatformConstraints (:150-167): the
    /// rule class's constraints, then the target's `exec_compatible_with`
    /// (an ImmutableSet).
    fn default_exec_constraints(&self, rule: &Rule) -> Result<Vec<Label>, String> {
        let Some(custom) = &rule.custom else {
            return Ok(Vec::new());
        };
        let mut labels = Vec::new();
        for text in &custom.declaration.exec_compatible_with {
            labels.push(Label::parse_canonical(text)?);
        }
        for text in custom
            .declaration
            .builtin
            .target
            .labels("exec_compatible_with")
        {
            labels.push(self.resolve_rule_src(rule, &text)?);
        }
        let mut unique = Vec::new();
        for label in labels {
            if !unique.contains(&label) {
                unique.push(label);
            }
        }
        Ok(unique)
    }

    /// RuleClass.AutoExecGroupsMode.DYNAMIC.isEnabled (packages/RuleClass.java:179-203)
    /// for a Starlark rule: its `_use_auto_exec_groups` attribute, else
    /// `--incompatible_auto_exec_groups`.
    fn use_auto_exec_groups(&self, rule: &Rule, config: ConfigId) -> Result<bool, String> {
        let Some(custom) = &rule.custom else {
            return Ok(false);
        };
        match custom.attrs.get("_use_auto_exec_groups") {
            None => Ok(self
                .configurations
                .get(config)
                .native
                .incompatible_auto_exec_groups),
            Some(crate::custom_rule::AttrValue::Bool(value)) => Ok(*value),
            Some(_) => Err(located(
                rule.location.as_deref(),
                "a non-boolean '_use_auto_exec_groups' attribute: Bazel 9.3 reads it as a boolean (RuleClass.AutoExecGroupsMode.isEnabled) and fails; not supported by the native profile".to_owned(),
            )),
        }
    }

    /// ToolchainContextUtil.getUnloadedToolchainContextsInputs
    /// (skyframe/toolchains/ToolchainContextUtil.java:42-93) for the exec
    /// groups: RuleClass.AutoExecGroupsMode.DYNAMIC (packages/RuleClass.java:179-203:
    /// a `_use_auto_exec_groups` attribute, else
    /// `--incompatible_auto_exec_groups`), the per-group
    /// `exec_group_compatible_with` constraints (:169-245, an
    /// InvalidExecGroupException at the target's location,
    /// skyframe/DependencyResolver.java:627-636) and DeclaredExecGroup.process
    /// (packages/DeclaredExecGroup.java:94-150): the declared groups (the
    /// default `test` group of a test rule that declares none,
    /// StarlarkRuleClassFunctions.java:1066-1082), then one automatic group
    /// per rule toolchain type, named by its Label.toString().
    fn exec_group_inputs(
        &self,
        rule: &Rule,
        config: ConfigId,
        with_platform_configuration: bool,
    ) -> Result<(bool, Vec<crate::exec_groups::ProcessedGroup>), String> {
        use crate::exec_groups::{ProcessedGroup, TEST_EXEC_GROUP_NAME, invalid_exec_group};
        let Some(custom) = &rule.custom else {
            return Ok((false, Vec::new()));
        };
        let use_auto = self.use_auto_exec_groups(rule, config)?;
        let default_types = requirements(&custom.declaration.toolchain_types)?;
        let default_exec = self.default_exec_constraints(rule)?;
        // The rule class's declared groups: rule(exec_groups), then the
        // default test group.
        let mut declared: Vec<(String, Vec<ToolchainTypeRequirement>, Vec<Label>, bool)> =
            Vec::new();
        for group in &custom.declaration.exec_groups {
            let mut constraints = Vec::new();
            for text in &group.exec_compatible_with {
                constraints.push(Label::parse_canonical(text)?);
            }
            declared.push((
                group.name.clone(),
                requirements(&group.toolchains)?,
                constraints,
                false,
            ));
        }
        if custom.declaration.builtin.kind.test
            && !declared
                .iter()
                .any(|(name, ..)| name == TEST_EXEC_GROUP_NAME)
        {
            declared.push((
                TEST_EXEC_GROUP_NAME.to_owned(),
                crate::toolchains::default_test_exec_group_key().types,
                Vec::new(),
                !self.repositories.has_builtin_modules(),
            ));
        }
        // getPerExecGroupExecutionPlatformConstraints (:169-245).
        let mut extra: Vec<(String, Vec<Label>)> = Vec::new();
        let mut extra_auto: Vec<(Label, Vec<Label>)> = Vec::new();
        let entries = match custom
            .declaration
            .builtin
            .target
            .attrs
            .get("exec_group_compatible_with")
        {
            Some(crate::custom_rule::AttrValue::StringListDict(entries))
                if with_platform_configuration =>
            {
                entries.clone()
            }
            _ => Vec::new(),
        };
        let target = crate::rule_declarations::canonical_form(&rule.label);
        let invalid = |key: &str, valid: &[&str]| {
            located(
                rule.location.as_deref(),
                invalid_exec_group("execution constraints", &target, &[key], valid),
            )
        };
        for (key, texts) in entries {
            let mut labels = Vec::new();
            for text in &texts {
                labels.push(self.resolve_rule_src(rule, text)?);
            }
            if crate::exec_groups::is_valid_group_name(&key) {
                if !declared.iter().any(|(name, ..)| *name == key) {
                    let names = declared
                        .iter()
                        .map(|(name, ..)| name.as_str())
                        .collect::<Vec<_>>();
                    return Err(invalid(&key, &names));
                }
                match extra.iter_mut().find(|(name, _)| *name == key) {
                    Some((_, existing)) => existing.extend(labels),
                    None => extra.push((key, labels)),
                }
            } else if use_auto {
                let Ok(label) =
                    self.repositories
                        .parse(&key, &rule.label.repository, &rule.label.package)
                else {
                    return Err(invalid(&key, &[]));
                };
                if !default_types
                    .iter()
                    .any(|requirement| requirement.label == label)
                {
                    // Display forms of the types, for a main-repository rule
                    // only (:218-226).
                    let suggestions = if rule.label.repository.is_empty() {
                        default_types
                            .iter()
                            .map(|requirement| self.display_form(&requirement.label))
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    let names = suggestions.iter().map(String::as_str).collect::<Vec<_>>();
                    return Err(invalid(&key, &names));
                }
                match extra_auto.iter_mut().find(|(seen, _)| *seen == label) {
                    Some((_, existing)) => existing.extend(labels),
                    None => extra_auto.push((label, labels)),
                }
            } else {
                return Err(invalid(&key, &[]));
            }
        }
        let union = |base: &[Label], more: Option<&Vec<Label>>| {
            let mut labels = base.to_vec();
            for label in more.into_iter().flatten() {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
            labels
        };
        let mut processed = Vec::new();
        for (name, types, constraints, builtin_only) in declared {
            let more = extra
                .iter()
                .find(|(group, _)| *group == name)
                .map(|(_, labels)| labels);
            processed.push(ProcessedGroup {
                key: ToolchainContextKey {
                    types,
                    exec_constraints: union(&constraints, more),
                    forced_exec: None,
                },
                name,
                builtin_only,
            });
        }
        if use_auto {
            for requirement in default_types {
                let more = extra_auto
                    .iter()
                    .find(|(label, _)| *label == requirement.label)
                    .map(|(_, labels)| labels);
                processed.push(ProcessedGroup {
                    name: requirement.label.to_string(),
                    key: ToolchainContextKey {
                        exec_constraints: union(&default_exec, more),
                        types: vec![requirement],
                        forced_exec: None,
                    },
                    builtin_only: false,
                });
            }
        }
        Ok((use_auto, processed))
    }

    /// ToolchainResolutionFunction.compute (skyframe/toolchains/ToolchainResolutionFunction.java:62-131)
    /// for a toolchain-resolving rule: PlatformKeys (the target platform,
    /// then the host platform as the only execution platform, filtered by
    /// the rule's execution constraints; PlatformKeys.java:78-224), the
    /// toolchain types (ToolchainTypeLookupUtil.java:44-170), the registered
    /// toolchains and the per-type resolution. The default host platform is
    /// resolved only when the rule has toolchain types or execution
    /// constraints (docs/TOOLCHAINS.md); a non-default `--host_platform` is
    /// always validated.
    fn resolve_toolchains(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        key: ToolchainContextKey,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Option<UnloadedToolchainContext>, String> {
        let configuration = self.configurations.get(config).clone();
        let host = configuration.native.host_platform.clone();
        let target_label = configuration.native.target_platform().clone();
        let default_host = host == crate::configuration::default_host_platform();
        // `--extra_execution_platforms` and the modules'
        // `register_execution_platforms`: the registered execution platforms
        // come before the host platform (PlatformKeys.findExecutionPlatformKeys,
        // :106-133), so a rule without toolchain types may run on one of
        // them; resolved first, like Bazel.
        let extra = !configuration.native.extra_execution_platforms.is_empty()
            || !self
                .module_execution_platforms(key.needs_platforms())?
                .is_empty();
        if !key.needs_platforms() && !extra {
            if default_host {
                return Ok(None);
            }
            let host_info = self
                .platform_info(&host, config, configured, plan)
                .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
            // PlatformKeys.isPlatformSuitable (:265-275): without toolchain
            // types a platform that checks toolchain types is unsuitable,
            // unless forced.
            if host_info.check_toolchain_types && key.forced_exec.is_none() {
                let target = self
                    .platform_info(&target_label, config, configured, plan)
                    .map_err(|error| {
                        self.platform_resolution_error(rule, config, &target_label, error)
                    })?;
                return Err(self.toolchain_error(
                    rule,
                    config,
                    "",
                    &crate::toolchains::no_matching_platform(&[], &target.label, &[host_info]),
                ));
            }
            return Ok(None);
        }
        let registered_platforms = if extra {
            self.execution_platforms(rule, config, &key, active, configured, plan)?
        } else {
            Arc::new(Vec::new())
        };
        let target = self
            .platform_info(&target_label, config, configured, plan)
            .map_err(|error| self.platform_resolution_error(rule, config, &target_label, error))?;
        let host_info = self
            .platform_info(&host, config, configured, plan)
            .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
        // PlatformKeys.build (:66-104): the registered execution platforms,
        // then the host platform unless it is one of them (a LinkedHashSet
        // of the alias-resolved keys).
        let mut candidates = registered_platforms.as_ref().clone();
        if !candidates
            .iter()
            .any(|platform| platform.label == host_info.label)
        {
            candidates.push(host_info.clone());
        }
        // Phase-1 prefetch: from here on, the implementation of every
        // toolchain compatible with these platforms is queued as soon as
        // the toolchain is analyzed (`prefetch_compatible_toolchain`).
        if self.prefetch.is_some() && self.prefetch_platforms.is_none() {
            self.prefetch_platforms = Some((target.clone(), host_info.clone()));
        }
        // PlatformKeys.filterExecutionPlatforms (:168-205).
        let mut constraints = Vec::new();
        for label in &key.exec_constraints {
            let value = self
                .exec_constraint(rule, label, config, active, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            constraints.push(value);
        }
        let required = constraints.iter().collect::<Vec<_>>();
        let exec = candidates
            .into_iter()
            .filter(|platform| platform.constraints.contains_all(&required))
            .collect::<Vec<_>>();
        let mut lookups = Vec::new();
        for requirement in &key.types {
            let info = self
                .toolchain_type_info(rule, requirement, config, active, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            lookups.push((requirement.clone(), info));
        }
        let types = crate::toolchains::resolved_types(&lookups);
        let requested = crate::toolchains::requested_types(&lookups);
        // SingleToolchainResolutionFunction reads RegisteredToolchainsValue
        // only for a type to resolve (:72-84).
        let registered = if types.is_empty() {
            Arc::new(RegisteredToolchains::default())
        } else {
            if self
                .computing_toolchains
                .iter()
                .any(|computing| computing.config == config)
                && !self.registered_toolchains.contains_key(&config)
            {
                return Err(self.toolchain_cycle(
                    config,
                    active,
                    &key,
                    &types[0].requirement.label,
                ));
            }
            self.registered_toolchains(config, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?
        };
        // PlatformKeys.find (:230-245): the forced platform is the target
        // platform or one of the execution platforms.
        let forced = key.forced_exec.as_ref().and_then(|forced| {
            if *forced == target.label {
                Some(target.clone())
            } else {
                exec.iter().find(|exec| exec.label == *forced).cloned()
            }
        });
        let resolved = crate::toolchains::resolve(
            &types,
            requested,
            crate::toolchains::Platforms {
                target: &target,
                exec: &exec,
                forced,
            },
            &registered,
        )
        .map_err(|message| self.toolchain_error(rule, config, "", &message))?;
        // A context that needed no platform and runs on the host platform
        // stays the lazy host context (`None`, as without
        // `--extra_execution_platforms`).
        if !key.needs_platforms() && resolved.exec_platform.label == host_info.label {
            return Ok(None);
        }
        // Phase-1 prefetch: the selected implementations are analyzed after
        // the rule's attribute dependencies.
        if self.prefetch.is_some() {
            for label in resolved.resolved_labels() {
                if self.prefetch_followed.insert(label.clone())
                    && let Some(prefetch) = &self.prefetch
                {
                    prefetch.submit_targets([(label.package_id(), Some(label.name))], 1, true);
                }
            }
        }
        Ok(Some(resolved))
    }

    /// Phase-1 prefetch: a toolchain compatible with the build's target
    /// platform and its host execution platform (the resolution of a rule
    /// without execution constraints of its own; target settings are not
    /// checked) has its implementation queued once the toolchain is
    /// analyzed during registration — long before the first rule that
    /// resolves it analyzes the implementation.
    fn prefetch_compatible_toolchain(&mut self, declared: &DeclaredToolchainInfo) {
        let Some((target, host)) = &self.prefetch_platforms else {
            return;
        };
        if !crate::toolchains::compatible(declared, target, host)
            || !self.prefetch_followed.insert(declared.resolved.clone())
        {
            return;
        }
        if let Some(prefetch) = &self.prefetch {
            prefetch.submit_targets(
                [(
                    declared.resolved.package_id(),
                    Some(declared.resolved.name.clone()),
                )],
                1,
                false,
            );
        }
    }

    /// RegisteredToolchainsCycleReporter.maybeReportCycle
    /// (skyframe/toolchains/RegisteredToolchainsCycleReporter.java:61-139)
    /// with AbstractLabelCycleReporter.printCycle (:134-170): a registered
    /// toolchain target whose analysis (`active`, from the registered target
    /// to the rule) needs the registered toolchains. The cycle is
    /// RegisteredToolchains, the configured targets, the rule's toolchain
    /// context key (`toolchain types ...`) and its first type's single
    /// resolution (`toolchain type T`); reported without location.
    fn toolchain_cycle(
        &mut self,
        config: ConfigId,
        active: &[ConfiguredTargetKey],
        key: &ToolchainContextKey,
        first_type: &Label,
    ) -> String {
        let shown = crate::rule_declarations::canonical_form;
        let head = active
            .first()
            .map(|key| shown(&key.label))
            .unwrap_or_default();
        let mut report = format!(
            "Misconfigured toolchains: {head} is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains"
        );
        for element in active {
            report.push_str("\n|   ");
            report.push_str(&shown(&element.label));
        }
        report.push_str(&format!(
            "\n|   toolchain types {}\n|   toolchain type {}\n`-- RegisteredToolchains",
            key.types
                .iter()
                .map(|requirement| shown(&requirement.label))
                .collect::<Vec<_>>()
                .join(", "),
            shown(first_type)
        ));
        self.toolchain_cycle_reports.insert(config, report.clone());
        report
    }

    /// A target loaded for toolchain resolution: a loading error is the
    /// NoSuchThingException itself (`com.google...NoSuchTargetException:
    /// ...`, ToolchainTypeLookupUtil.java:139-144,
    /// ConstraintValueLookupUtil.java:101-105), preceded by the loading event
    /// of another package's BUILD file.
    fn resolution_target(&mut self, rule: &Rule, label: &Label) -> Result<(), ResolutionError> {
        // An unresolvable apparent repository (crate::toolchains::deferred_label_text):
        // the BuildFileNotFoundException of its package, also reported as an
        // event without location (paired capture B4/resolution_errors/unknown_repo).
        if let Some(message) = crate::label::unresolvable_package_message(&label.package_id()) {
            return Err(ResolutionError::Reported(
                message.clone(),
                format!(
                    "com.google.devtools.build.lib.packages.BuildFileNotFoundException: {message}"
                ),
            ));
        }
        self.target(label).map(|_| ()).map_err(|error| {
            // The loading event is reported for the rule's own package too
            // (paired capture A5/resolution_errors/missing_type).
            let _ = rule;
            let event = self.target_loading_event(label, &error);
            let message = PlatformError::loading(error).target_text();
            match event {
                Some(event) => ResolutionError::Reported(event, message),
                None => ResolutionError::Toolchain(message),
            }
        })
    }

    /// ConstraintValueLookupUtil.findConstraintValueInfo
    /// (skyframe/toolchains/ConstraintValueLookupUtil.java:62-92).
    fn exec_constraint(
        &mut self,
        rule: &Rule,
        label: &Label,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ConstraintValueInfo, ResolutionError> {
        self.resolution_target(rule, label)?;
        self.analyze(label, config, active, configured, plan)
            .map_err(ResolutionError::Propagated)?;
        let node = configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        node.and_then(|node| node.constraint_value.clone())
            .ok_or_else(|| {
                ResolutionError::Toolchain(crate::toolchains::not_a_constraint_value(
                    &self.alias_chain(configured, config, label).1,
                ))
            })
    }

    /// ToolchainTypeLookupUtil.findToolchainTypeInfo
    /// (skyframe/toolchains/ToolchainTypeLookupUtil.java:97-148): the type
    /// target is analyzed in the rule's configuration; `None` for an invalid
    /// type that is ignored.
    fn toolchain_type_info(
        &mut self,
        rule: &Rule,
        requirement: &ToolchainTypeRequirement,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Option<ToolchainTypeInfo>, ResolutionError> {
        let label = &requirement.label;
        self.resolution_target(rule, label)?;
        self.analyze(label, config, active, configured, plan)
            .map_err(ResolutionError::Propagated)?;
        let node = configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        if let Some(info) = node.and_then(|node| node.toolchain_type.clone()) {
            return Ok(Some(info));
        }
        if requirement.ignore_if_invalid {
            return Ok(None);
        }
        let is_toolchain = node.is_some_and(|node| node.declared_toolchain.is_some());
        Err(ResolutionError::Toolchain(
            crate::toolchains::invalid_toolchain_type(
                &self.alias_chain(configured, config, label).1,
                is_toolchain,
            ),
        ))
    }

    /// RegisteredExecutionPlatformsFunction.compute
    /// (skyframe/toolchains/RegisteredExecutionPlatformsFunction.java:72-157)
    /// for a configuration with `--extra_execution_platforms`, once per
    /// configuration: the platforms in registration order (alias-resolved
    /// PlatformInfo). Module registrations (`register_execution_platforms`)
    /// come after the configuration's (Self::module_execution_platforms).
    /// A registered target that needs toolchain resolution itself is
    /// RegisteredExecutionPlatformsCycleReporter's cycle.
    fn execution_platforms(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        key: &ToolchainContextKey,
        active: &[ConfiguredTargetKey],
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ExecutionPlatforms, String> {
        if let Some(cached) = self.registered_execution_platforms.get(&config) {
            return cached
                .clone()
                .map_err(|error| self.resolution_error(rule, config, error));
        }
        if self
            .computing_execution_platforms
            .iter()
            .any(|computing| computing.config == config)
        {
            return Err(self.execution_platform_cycle(config, active, key));
        }
        self.computing_execution_platforms
            .push(ConfiguredTargetKey::new(rule.label.clone(), config));
        let result = self.compute_execution_platforms(config, configured, plan);
        self.computing_execution_platforms.pop();
        self.registered_execution_platforms
            .insert(config, result.clone());
        result.map_err(|error| self.resolution_error(rule, config, error))
    }

    fn compute_execution_platforms(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ExecutionPlatforms, ResolutionError> {
        // The configuration's platforms, parsed with the main repository
        // mapping (:86-99), then the modules' registrations, each in its
        // module's repository (:107-111, 165-189).
        let mut patterns = self
            .configurations
            .get(config)
            .native
            .extra_execution_platforms
            .iter()
            .map(|pattern| (String::new(), pattern.clone()))
            .collect::<Vec<_>>();
        patterns.extend(
            self.module_execution_platforms(true)
                .map_err(ResolutionError::Propagated)?,
        );
        let labels =
            self.expand_registration_patterns(&patterns, Registration::ExecutionPlatform)?;
        // configureRegisteredExecutionPlatforms (:193-243): each target in
        // the configuration; its key is the alias-resolved label.
        let mut platforms: Vec<(Label, Arc<PlatformInfo>)> = Vec::new();
        let mut cycle = None;
        for label in labels {
            let info = match self.target(&label) {
                Ok(Target::Rule(rule)) if rule.label == label && has_platform_info(&rule) => self
                    .platform_info(&label, config, configured, plan)
                    .map_err(|error| platform_resolution_failure(&label, error)),
                // Any other target is configured like a dependency: one
                // that resolves toolchains needs these platforms (a cycle);
                // otherwise it lacks PlatformInfo (InvalidPlatformException,
                // after its own errors).
                _ => match self.analyze(&label, config, &mut Vec::new(), configured, plan) {
                    Ok(()) => configured
                        .get(&ConfiguredTargetKey::new(label.clone(), config))
                        .and_then(|node| node.platform.clone())
                        .ok_or_else(|| {
                            ResolutionError::Toolchain(crate::platforms::not_a_platform(&label))
                        }),
                    Err(error) => match self.execution_platform_cycle_reports.remove(&config) {
                        Some(report) if error.contains(&report) => {
                            cycle = Some(report);
                            continue;
                        }
                        _ => Err(ResolutionError::Reported(
                            error,
                            crate::platforms::not_a_platform(&label),
                        )),
                    },
                },
            };
            let info = info?;
            if let Some((first, _)) = platforms.iter().find(|(_, seen)| seen.label == info.label) {
                return Err(ResolutionError::Propagated(format!(
                    "the registered execution platforms {} and {} are the same platform {}: Bazel 9.3 crashes on it (IllegalArgumentException: Multiple entries with same key, ImmutableMap.Builder.buildOrThrow in RegisteredExecutionPlatformsFunction.configureRegisteredExecutionPlatforms, skyframe/toolchains/RegisteredExecutionPlatformsFunction.java:243), which is not implemented in the native profile",
                    crate::rule_declarations::canonical_form(first),
                    crate::rule_declarations::canonical_form(&label),
                    crate::rule_declarations::canonical_form(&info.label),
                )));
            }
            platforms.push((label, info));
        }
        if let Some(report) = cycle {
            return Err(ResolutionError::Propagated(report));
        }
        Ok(Arc::new(
            platforms.into_iter().map(|(_, info)| info).collect(),
        ))
    }

    /// RegisteredExecutionPlatformsCycleReporter.maybeReportCycle
    /// (skyframe/toolchains/RegisteredExecutionPlatformsCycleReporter.java:55-104)
    /// with AbstractLabelCycleReporter.printCycle: a registered execution
    /// platform target whose analysis (`active`, from the registered target
    /// to the rule) needs the configuration's execution platforms. The
    /// cycle is RegisteredExecutionPlatforms, the configured targets and the
    /// rule's toolchain context key (`toolchain types ...`); reported
    /// without location.
    fn execution_platform_cycle(
        &mut self,
        config: ConfigId,
        active: &[ConfiguredTargetKey],
        key: &ToolchainContextKey,
    ) -> String {
        let shown = crate::rule_declarations::canonical_form;
        let head = active
            .first()
            .map(|key| shown(&key.label))
            .unwrap_or_default();
        let mut report = format!(
            "Misconfigured execution platforms: {head} is declared as a platform but has inappropriate dependencies. Execution platforms should not have dependencies that themselves require toolchain resolution.\n.-> RegisteredExecutionPlatforms"
        );
        for element in active {
            report.push_str("\n|   ");
            report.push_str(&shown(&element.label));
        }
        report.push_str(&format!(
            "\n|   toolchain types {}\n`-- RegisteredExecutionPlatforms",
            key.types
                .iter()
                .map(|requirement| shown(&requirement.label))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        self.execution_platform_cycle_reports
            .insert(config, report.clone());
        report
    }

    /// RegisteredToolchainsFunction.compute (skyframe/toolchains/RegisteredToolchainsFunction.java:64-147),
    /// once per configuration (D1: every registered toolchain is analyzed).
    fn registered_toolchains(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        if let Some(cached) = self.registered_toolchains.get(&config) {
            return cached.clone();
        }
        if let Some(key) = self
            .computing_toolchains
            .iter()
            .find(|computing| computing.config == config)
        {
            // Reached only through resolve_toolchains, which reports the
            // cycle itself (Self::toolchain_cycle).
            return Err(ResolutionError::Propagated(format!(
                "internal error: registered toolchains needed while analyzing {}",
                crate::rule_declarations::canonical_form(&key.label)
            )));
        }
        // Set to each registered target while it is analyzed.
        self.computing_toolchains.push(ConfiguredTargetKey::new(
            Label {
                repository: String::new(),
                package: String::new(),
                name: String::new(),
            },
            config,
        ));
        let result = self.compute_registered_toolchains(config, configured, plan);
        self.computing_toolchains.pop();
        self.registered_toolchains.insert(config, result.clone());
        result
    }

    fn compute_registered_toolchains(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        // --extra_toolchains, reversed so the last one wins, parsed with the
        // main repository mapping (:82-93); then the modules of the
        // dependency graph (:96-104, 149-172).
        let mut patterns = self
            .configurations
            .get(config)
            .native
            .extra_toolchains
            .iter()
            .rev()
            .map(|pattern| (String::new(), pattern.clone()))
            .collect::<Vec<_>>();
        for (repository, labels) in self
            .repositories
            .toolchain_registrations()
            .map_err(ResolutionError::Propagated)?
        {
            patterns.extend(labels.into_iter().map(|label| (repository.clone(), label)));
        }
        self.prefetch_toolchain_patterns(&patterns);
        let labels = self.expand_toolchain_patterns(&patterns)?;
        // configureRegisteredToolchains (:174-215).
        let mut declared = Vec::new();
        // Of several registered targets in a toolchain cycle, Bazel reports
        // the last one (paired capture B4/resolution_errors/extra_cyc_all_o:
        // `//cyc:all` names //cyc:type_is_user, not //cyc:exec_user).
        let mut cycle = None;
        let mut analyzed = Vec::new();
        for label in labels {
            if let Some(computing) = self.computing_toolchains.last_mut() {
                computing.label = label.clone();
            }
            match self.analyze(&label, config, &mut Vec::new(), configured, plan) {
                Ok(()) => analyzed.push(label),
                Err(error) => match self.toolchain_cycle_reports.remove(&config) {
                    Some(report) if error.contains(&report) => cycle = Some(report),
                    _ => return Err(ResolutionError::Propagated(error)),
                },
            }
        }
        if let Some(report) = cycle {
            return Err(ResolutionError::Propagated(report));
        }
        for label in analyzed {
            let info = configured
                .get(&ConfiguredTargetKey::new(label.clone(), config))
                .and_then(|node| node.declared_toolchain.clone())
                .ok_or_else(|| {
                    ResolutionError::Toolchain(crate::toolchains::invalid_registered_toolchain(
                        &crate::rule_declarations::canonical_form(&label),
                        "target does not provide the DeclaredToolchainInfo provider",
                    ))
                })?;
            declared.push(info);
        }
        // ConfigMatchingUtil.validate per toolchain (:120-141).
        let mut toolchains = Vec::new();
        for info in declared {
            match crate::toolchains::target_settings_match(&info.target_label, &info.settings) {
                Ok(true) => toolchains.push(info),
                Ok(false) => {}
                Err(message) => {
                    return Err(ResolutionError::Toolchain(
                        crate::toolchains::invalid_registered_toolchain(
                            &crate::rule_declarations::canonical_form(&info.target_label),
                            &message,
                        ),
                    ));
                }
            }
        }
        Ok(Arc::new(RegisteredToolchains { toolchains }))
    }

    /// Phase-1 prefetch (crate::prefetch) of the registered toolchain
    /// packages: each pattern's package, in registration order, at the
    /// front of the queue — they are what `expand_toolchain_patterns`
    /// loads next. `/...` patterns are left to the sequential expansion
    /// (their package discovery needs the repository root), and nothing
    /// after a pattern the expansion rejects is queued. The labels resolve
    /// under the no-demand guard: a hint never prepares a repository.
    fn prefetch_toolchain_patterns(&self, patterns: &[(String, String)]) {
        let Some(prefetch) = &self.prefetch else {
            return;
        };
        // A hint observes nothing for the analysis snapshot (W6R): the
        // expansion's own lookups are recorded where it makes them.
        let targets = crate::observed_fs::discard(|| {
            let _guard = crate::repository_demand::no_demand();
            let mut targets = Vec::new();
            for (repository, pattern) in patterns {
                if pattern.starts_with('-') {
                    break;
                }
                let all = match pattern.rsplit_once(':') {
                    Some((base, "all")) => {
                        if base == "..." || base.ends_with("/...") {
                            continue;
                        }
                        true
                    }
                    Some((_, "*" | "all-targets")) => break,
                    _ => false,
                };
                if let Ok(label) = self.repositories.parse(pattern, repository, "") {
                    let name = (!all).then(|| label.name.clone());
                    targets.push((label.package_id(), name));
                }
            }
            targets
        });
        prefetch.submit_targets(targets, 0, false);
    }

    /// TargetPatternUtil.expandTargetPatterns (skyframe/TargetPatternUtil.java:44-87)
    /// with FilteringPolicies.ruleTypeExplicit("toolchain")
    /// (pkgcache/FilteringPolicies.java:45-47): patterns in order, the first
    /// copy of a label kept; wildcards keep only `toolchain` rules (manual
    /// tags are not filtered), explicit labels are kept whatever they are.
    /// `:all` lists a package's targets by name; `/...` visits packages in
    /// the RecursivePkgFunction order (sorted directory entries, each
    /// directory's subpackages before itself: a STABLE_ORDER nested set
    /// iterates postorder, collect/nestedset/NestedSet.java:190-230,
    /// skyframe/RecursivePkgFunction.java:88-116).
    fn expand_toolchain_patterns(
        &mut self,
        patterns: &[(String, String)],
    ) -> Result<Vec<Label>, ResolutionError> {
        self.expand_registration_patterns(patterns, Registration::Toolchain)
    }

    /// TargetPatternUtil.expandTargetPatterns for registered toolchains
    /// (above) or execution platforms (RegisteredExecutionPlatformsFunction.HAS_PLATFORM_INFO,
    /// :66-68: wildcards keep the rules PlatformLookupUtil.hasPlatformInfo
    /// accepts, :160-173 — `platform`, and the alias-like rules that do not
    /// resolve toolchains; explicit labels are kept whatever they are).
    fn expand_registration_patterns(
        &mut self,
        patterns: &[(String, String)],
        kind: Registration,
    ) -> Result<Vec<Label>, ResolutionError> {
        let mut labels: Vec<Label> = Vec::new();
        // The labels already in `labels`: first-copy dedupe in O(1) per
        // label (a linear `labels.contains` was quadratic in the number of
        // registered toolchains).
        let mut seen: HashSet<Label> = HashSet::new();
        for (repository, pattern) in patterns {
            let invalid = |message: String| {
                ResolutionError::Toolchain(match kind {
                    Registration::Toolchain => {
                        crate::toolchains::invalid_registered_toolchain(pattern, &message)
                    }
                    // InvalidExecutionPlatformLabelException (:262-275).
                    Registration::ExecutionPlatform => {
                        format!("invalid registered execution platform '{pattern}': {message}")
                    }
                })
            };
            if pattern.starts_with('-') {
                return Err(ResolutionError::Propagated(format!(
                    "negative {} registration pattern {pattern:?} is not implemented in the native profile",
                    kind.noun()
                )));
            }
            // An unknown apparent repository fails the whole analysis, not
            // the toolchain resolution (paired capture
            // B1/extra_extra_toolchains_nope_x): `Analysis of target 'L'
            // failed; build aborted: No repository visible as '@R' from main
            // repository` (RepositoryFetchFunction.java:149-153,
            // RepositoryName.getContextRepoDisplayString, :251-258).
            let parse = |this: &Self, text: &str| {
                this.repositories
                    .parse(text, repository, "")
                    .map_err(
                        |error| match unknown_apparent_repository(text, repository, &error) {
                            Some(message) => ResolutionError::Aborted(message),
                            None => invalid(error),
                        },
                    )
            };
            let (base, wildcard) = match pattern.rsplit_once(':') {
                Some((base, "all")) => (base, true),
                // HAS_PLATFORM_INFO reads the associated rule null-safely:
                // file targets are dropped like any non-platform target.
                Some((base, "*" | "all-targets")) if kind == Registration::ExecutionPlatform => {
                    (base, true)
                }
                // FilteringPolicies.RuleTypeFilter.shouldRetain
                // (pkgcache/FilteringPolicies.java:120) dereferences the
                // associated rule of every target, so Bazel 9.3 crashes with
                // a NullPointerException on the package's file targets (paired
                // capture B1/extra_extra_toolchains_names, exit 37).
                Some((_, kind @ ("*" | "all-targets"))) => {
                    return Err(ResolutionError::Propagated(format!(
                        "toolchain registration pattern '{pattern}' (the :{kind} wildcard) is not implemented in the native profile: Bazel 9.3 crashes expanding it (NullPointerException in FilteringPolicies.RuleTypeFilter.shouldRetain, pkgcache/FilteringPolicies.java:120)"
                    )));
                }
                _ => (pattern.as_str(), false),
            };
            let found = if base == "..." || base.ends_with("/...") {
                // TargetPattern.TargetsBelowDirectory: `//...` and `@r//...`
                // keep their `//`, other directories drop the trailing `/`
                // (as select_filtered).
                let prefix = base.strip_suffix("...").unwrap_or_default();
                let directory = if prefix.is_empty() || prefix.ends_with("//") {
                    prefix
                } else {
                    prefix.strip_suffix('/').unwrap_or(prefix)
                };
                let context = parse(self, &format!("{directory}:__pattern__"))?;
                let ignored = self.ignored(&context.repository).map_err(invalid)?;
                // A directory inside an ignored directory warns and yields no
                // targets (TargetPattern.TargetsBelowDirectory.eval; the
                // warning of select_filtered, docs/IGNORED_DIRECTORIES.md).
                if let Some(entry) = ignored.matching_entry(&context.package) {
                    self.warnings.push(format!(
                        "Pattern '{pattern}' was filtered out by ignored directory '{entry}'"
                    ));
                    continue;
                }
                let ignored = ignored.filter_for_directory(&context.package);
                let root = self
                    .repositories
                    .root(&context.repository)
                    .map_err(invalid)?
                    .to_path_buf();
                let mut packages = if crate::observed_fs::is_dir(root.join(&context.package)) {
                    workspace::discover_packages_skipping(
                        &root,
                        &context.package,
                        &ignored,
                        &self.repositories.link_policy(&context.repository).blocked,
                    )
                    .map_err(invalid)?
                } else {
                    Vec::new()
                };
                // RecursivePackageProviderBackedTargetPatternResolver.java:326-329.
                if packages.is_empty() {
                    return Err(invalid(format!(
                        "no targets found beneath '{}'",
                        context.package
                    )));
                }
                packages.sort_by(|a, b| subpackages_first(a, b));
                let mut found = Vec::new();
                for package in packages {
                    let id = PackageId {
                        repository: context.repository.clone(),
                        package,
                    };
                    self.load_package(&id).map_err(invalid)?;
                    found.extend(self.package_registration_rules(&id, kind));
                }
                found
            } else if wildcard {
                let id = parse(self, &format!("{base}:all"))?.package_id();
                // TargetPatternResolverUtil.getParsingErrorMessage
                // (pkgcache/TargetPatternResolverUtil.java:33-39).
                self.load_package(&id)
                    .map_err(|error| invalid(format!("while parsing '{pattern}': {error}")))?;
                self.package_registration_rules(&id, kind)
            } else {
                let label = parse(self, pattern)?;
                self.target(&label).map_err(invalid)?;
                vec![label]
            };
            for label in found {
                if !seen.contains(&label) {
                    seen.insert(label.clone());
                    labels.push(label);
                }
            }
        }
        Ok(labels)
    }

    /// The rules of a loaded package a registration wildcard keeps, by
    /// name: `toolchain` rules, or those with PlatformInfo.
    fn package_registration_rules(&self, id: &PackageId, kind: Registration) -> Vec<Label> {
        self.packages[id]
            .values()
            .filter(|rule| match kind {
                Registration::Toolchain => matches!(rule.kind, RuleKind::Native("toolchain")),
                Registration::ExecutionPlatform => has_platform_info(rule),
            })
            .map(|rule| rule.label.clone())
            .collect()
    }

    /// ConfigFeatureFlag.create (rules/config/ConfigFeatureFlag.java:109-169)
    /// after RuleContext's checks: `allowed_values` must be non empty
    /// (RuleContext.checkAttributesNonEmpty, analysis/RuleContext.java:1531-1549),
    /// then the `$allowlist_config_feature_flag` package group
    /// (`@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag`,
    /// ConfigFeatureFlag.java:56-64, ConfigRuleClasses.java:459-466) must contain
    /// the rule's package (Allowlist.isAvailableFor, analysis/Allowlist.java:88-99).
    fn config_feature_flag_provider(
        &mut self,
        rule: &Rule,
    ) -> Result<Option<crate::feature_flags::FeatureFlagValue>, String> {
        let (RuleKind::Native("config_feature_flag"), Some(native)) = (&rule.kind, &rule.native)
        else {
            return Ok(None);
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let allowed = native.strings("allowed_values");
        if allowed.is_empty() {
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "in allowed_values attribute of config_feature_flag rule {shown}: attribute must be non empty"
                ),
            ));
        }
        let allowlist = Label::parse_canonical(
            "@@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag",
        )?;
        if !self.repositories.has_builtin_modules() {
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "config_feature_flag rule {shown}: its allowlist {} needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide; drop --experimental-builtin-modules=false",
                    crate::rule_declarations::canonical_form(&allowlist)
                ),
            ));
        }
        let group = self.resolve_group(&allowlist, &mut Vec::new())?;
        if !group.contains(&rule.label.package_id()) {
            // AllowlistChecker (ConfigRuleClasses.java:459-466); the genuine
            // embedded group is `packages = ["public"]`.
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "in config_feature_flag rule {shown}: the config_feature_flag rule is not available in this package"
                ),
            ));
        }
        crate::feature_flags::analyze(
            &shown,
            &allowed,
            native
                .explicit("default_value")
                .then(|| native.string("default_value").unwrap_or_default()),
        )
        .map(Some)
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }
    /// ToolchainType.create (rules/ToolchainType.java:40-52) and
    /// Toolchain.create (rules/platform/Toolchain.java:42-110) from the
    /// validated prerequisites (crate::toolchains).
    fn toolchain_rule_providers(
        &self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
        configured: &ConfiguredNodes,
    ) -> Result<
        (
            Option<ToolchainTypeInfo>,
            Option<Arc<DeclaredToolchainInfo>>,
        ),
        String,
    > {
        let Some(native) = &rule.native else {
            return Ok((None, None));
        };
        match rule.kind {
            RuleKind::Native("toolchain_type") => Ok((
                Some(ToolchainTypeInfo::new(
                    rule.label.clone(),
                    native.string("no_match_error").unwrap_or_default(),
                )),
                None,
            )),
            RuleKind::Native("toolchain") => {
                let node = |label: &Label| {
                    configured.get(&ConfiguredTargetKey::new(label.clone(), config))
                };
                let internal = |what: &str| {
                    format!(
                        "internal error: validated prerequisite of {} lacks {what}",
                        rule.label
                    )
                };
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let toolchain_type = provided(0)
                    .first()
                    .and_then(node)
                    .and_then(|node| node.toolchain_type.clone())
                    .ok_or_else(|| internal("ToolchainTypeInfo"))?;
                let constraints = |labels: Vec<Label>| {
                    labels
                        .iter()
                        .map(|label| {
                            node(label)
                                .and_then(|node| node.constraint_value.clone())
                                .ok_or_else(|| internal("ConstraintValueInfo"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                };
                let exec_compatible_with = constraints(provided(1))?;
                let target_compatible_with = constraints(provided(2))?;
                let target_settings = provided(3)
                    .iter()
                    .map(|label| {
                        node(label)
                            .and_then(|node| node.matching.clone())
                            .ok_or_else(|| internal("ConfigMatchingProvider"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let Some(crate::custom_rule::AttrValue::Label(Some(toolchain))) =
                    native.attrs.get("toolchain")
                else {
                    return Err(internal("a toolchain attribute"));
                };
                let toolchain = self.resolve_rule_src(rule, toolchain)?;
                let shown = crate::rule_declarations::canonical_form(&rule.label);
                let declared = crate::toolchains::declare_toolchain(
                    &rule.label,
                    crate::toolchains::ToolchainAttributes {
                        toolchain_type,
                        exec_compatible_with,
                        target_compatible_with,
                        use_target_platform_constraints: matches!(
                            native.attrs.get("use_target_platform_constraints"),
                            Some(crate::custom_rule::AttrValue::Bool(true))
                        ),
                        target_settings,
                        toolchain,
                    },
                )
                .map_err(|errors| {
                    errors
                        .iter()
                        .map(|error| {
                            located(
                                rule.location.as_deref(),
                                format!(
                                    "in {} attribute of toolchain rule {shown}: {}",
                                    error.attribute, error.message
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })?;
                Ok((None, Some(Arc::new(declared))))
            }
            _ => Ok((None, None)),
        }
    }

    /// PlatformFunction.compute (analysis/platform/PlatformFunction.java:57-126),
    /// memoized per configuration.
    fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        if let Some(cached) = self.platforms.get(&key) {
            return cached.clone();
        }
        if self.resolving_platforms.contains(&key) {
            return Err(PlatformError::Other(format!(
                "platform {} is needed while it is being resolved; this is not implemented in the native profile",
                crate::rule_declarations::canonical_form(label)
            )));
        }
        self.resolving_platforms.push(key.clone());
        let result = self.compute_platform_info(label, config, configured, plan);
        self.resolving_platforms.pop();
        self.platforms.insert(key, result.clone());
        result
    }

    fn compute_platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let shown = crate::rule_declarations::canonical_form(label);
        // An apparent repository the main repository does not see stays an
        // unresolvable label (`@@[unknown repo 'R' requested from @@]`,
        // crate::configuration) until the package is looked up.
        if let Some(message) = crate::label::unresolvable_package_message(&label.package_id()) {
            return Err(PlatformError::Missing {
                class: "BuildFileNotFoundException",
                message,
            });
        }
        // The default --host_platform is @bazel_tools//tools:host_platform
        // (PlatformOptions.java:46-56), an alias of @platforms//host whose
        // constraints the platforms module's host_platform extension computes
        // from the host (docs/PLATFORMS.md). Without the built-in bazel_tools
        // repository there is nothing to resolve.
        if label.repository == "bazel_tools"
            && let Err(error) = self.repositories.root("bazel_tools")
        {
            return Err(PlatformError::Other(format!(
                "the target platform {shown} (the default host platform) needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide ({error}); drop --experimental-builtin-modules=false, or select a platform with --platforms"
            )));
        }
        // PlatformLookupUtil.hasPlatformInfo before analysis (:160-172);
        // package and target errors propagate (InvalidPlatformException(NoSuchThingException)).
        let rule = match self.target(label) {
            Err(error) => return Err(PlatformError::loading(error)),
            Ok(Target::Rule(rule) | Target::Output(rule, _)) => rule,
            Ok(Target::Source(_)) => {
                return Err(PlatformError::Other(crate::platforms::not_a_platform(
                    label,
                )));
            }
        };
        if rule.label != *label || !has_platform_info(&rule) {
            return Err(PlatformError::Other(crate::platforms::not_a_platform(
                label,
            )));
        }
        self.check_platform_tree(label)?;
        self.analyze(label, config, &mut Vec::new(), configured, plan)
            .map_err(|error| {
                PlatformError::Other(format!(
                    "{error}\n{}",
                    crate::platforms::not_a_platform(label)
                ))
            })?;
        let platform = configured
            .get(&ConfiguredTargetKey::new(label.clone(), config))
            .and_then(|node| node.platform.clone())
            .ok_or_else(|| {
                PlatformError::Other(crate::platforms::not_a_platform(
                    &self.alias_chain(configured, config, label).1,
                ))
            })?;
        // PlatformFunction parses PlatformInfo.flags into the configuration
        // (:102-125, ParsedFlagsValue): not modelled.
        if !platform.flags.is_empty() {
            return Err(PlatformError::Other(format!(
                "platform {shown} sets flags [{}]: platform-based flags (platform(flags = ...), analysis/platform/PlatformFunction.java:102-125) are not implemented in the native profile",
                platform.flags.join(", ")
            )));
        }
        Ok(platform)
    }

    /// The alias chain from `label` through plain (select()-free) aliases,
    /// stopping at a label setting (no actual in the empty configuration):
    /// the aliases passed and the end.
    fn static_alias_chain(&mut self, label: &Label) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        loop {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            if !matches!(rule.kind, RuleKind::Native("alias")) || chain.contains(&current) {
                break;
            }
            let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("actual"))
            else {
                break;
            };
            let Ok(actual) = self.resolve_rule_src(&rule, actual) else {
                break;
            };
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// Bazel analyzes a platform and its whole dependency tree in
    /// `CommonOptions.EMPTY_OPTIONS` (PlatformFunction.configuredTargetDep,
    /// analysis/platform/PlatformFunction.java:46-52;
    /// analysis/config/CommonOptions.java:32-42: CoreOptions defaults only,
    /// no platform options and no Starlark build settings). The native
    /// profile analyzes in the build configuration, which gives the same
    /// result only when nothing in the tree depends on the configuration:
    ///
    /// * a `label_flag`/`label_setting` on the platform label's alias chain
    ///   resolves to no actual there, so it provides no PlatformInfo (Bazel
    ///   names it: `Target //:plat_flag was referenced as a platform, but
    ///   does not provide PlatformInfo`, paired capture B2 platforms*_plat_flag,
    ///   platforms*_fg_flag); one reached through a platform rule's attribute
    ///   fails that rule's prerequisite validation (Bazel's text);
    /// * select() keys anywhere in the tree, label settings reached otherwise
    ///   and `required_settings` naming config_settings (config_setting
    ///   matching, only used by execution-platform resolution, N17) fail
    ///   explicitly.
    fn check_platform_tree(&mut self, platform: &Label) -> Result<(), PlatformError> {
        let shown = crate::rule_declarations::canonical_form(platform);
        let mut current = platform.clone();
        let mut chain = BTreeSet::new();
        while chain.insert(current.clone()) {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            match rule.kind {
                RuleKind::Native("label_flag" | "label_setting") => {
                    return Err(PlatformError::Other(crate::platforms::not_a_platform(
                        &current,
                    )));
                }
                RuleKind::Native("alias") => {
                    let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                        .native
                        .as_ref()
                        .and_then(|native| native.attrs.get("actual"))
                    else {
                        break;
                    };
                    match self.resolve_rule_src(&rule, actual) {
                        Ok(actual) => current = actual,
                        Err(_) => break,
                    }
                }
                _ => break,
            }
        }
        let gap = |what: String| {
            PlatformError::Other(format!(
                "{what} in the dependency tree of platform {shown} is not implemented in the native profile: Bazel analyzes platforms and their dependencies in the empty configuration (analysis/platform/PlatformFunction.java:46-52, analysis/config/CommonOptions.java:32-42), which the native profile does not model"
            ))
        };
        let mut stack = vec![platform.clone()];
        let mut visited = BTreeSet::new();
        while let Some(label) = stack.pop() {
            if visited.len() >= 100_000 || !visited.insert(label.clone()) {
                continue;
            }
            let Ok(Target::Rule(rule)) = self.target(&label) else {
                continue;
            };
            let rule_shown = crate::rule_declarations::canonical_form(&rule.label);
            if !configured_attributes::rule_config_dependencies(&rule).is_empty() {
                return Err(gap(format!("{rule_shown}: select()")));
            }
            if let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind {
                return Err(gap(format!("{rule_shown}: {kind}")));
            }
            // A label setting reached through a platform rule's attribute
            // has no providers in the empty configuration: Bazel's
            // prerequisite error (paired capture A8/gap_flag_parent,
            // B8/target_cv_flag_platform), then InvalidPlatformException.
            let attributes = platform_prerequisite_attributes(&rule);
            let mut errors = Vec::new();
            let mut required_are_settings = true;
            for attribute in &attributes {
                for text in &attribute.labels {
                    let Ok(dependency) = self.resolve_rule_src(&rule, text) else {
                        continue;
                    };
                    let (chain, end) = self.static_alias_chain(&dependency);
                    let Ok(Target::Rule(end_rule)) = self.target(&end) else {
                        required_are_settings &= attribute.name != "required_settings";
                        continue;
                    };
                    if attribute.name == "required_settings" {
                        required_are_settings &=
                            matches!(end_rule.kind, RuleKind::Native("config_setting"));
                    }
                    if matches!(
                        end_rule.kind,
                        RuleKind::Native("label_flag" | "label_setting")
                    ) && let Some(error) =
                        rule_dependency_error(attribute, &chain, &end, &end_rule, None)
                    {
                        errors.push(located(
                            rule.location.as_deref(),
                            crate::rule_errors::RuleEvents {
                                rule_class: rule.rule_class_name(),
                                label: &rule_shown,
                            }
                            .attribute_message(attribute.name, &error),
                        ));
                    }
                }
            }
            if !errors.is_empty() {
                errors.push(crate::platforms::not_a_platform(platform));
                return Err(PlatformError::Other(errors.join("\n")));
            }
            // Non-config_setting required settings fail prerequisite
            // validation, which the analysis below reports like Bazel.
            if let (RuleKind::Native("platform"), Some(native)) = (&rule.kind, &rule.native)
                && !native.labels("required_settings").is_empty()
                && required_are_settings
            {
                return Err(gap(format!(
                    "{rule_shown}: required_settings (config_setting matching for execution platform resolution, N17)"
                )));
            }
            if let Some(native) = &rule.native {
                stack.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                stack.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                for text in crate::package::custom_dependency_labels(&custom.attrs) {
                    if let Ok(dependency) = self.resolve_rule_src(&rule, &text) {
                        stack.push(dependency);
                    }
                }
            }
        }
        Ok(())
    }

    /// Starlark build settings of the command line
    /// (runtime/StarlarkOptionsParser.java, crate::starlark_options): each
    /// option name is loaded as a target pattern
    /// (BlazeOptionHandler.SkyframeExecutorTargetLoader,
    /// runtime/BlazeOptionHandler.java:371-410). `command_args` are the
    /// command's arguments (for the `Did you mean` hint); `labels` converts
    /// label values with the main repository mapping.
    pub fn starlark_options(
        &mut self,
        args: &[String],
        command_args: &[String],
        labels: &LabelParser<'_>,
        current_package: &str,
    ) -> Result<BTreeMap<Label, SettingValue>, String> {
        if args.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut loader = |name: &str| self.load_option_target(name, current_package);
        crate::starlark_options::parse(args, command_args, &mut loader, labels)
    }

    fn load_option_target(
        &mut self,
        name: &str,
        current_package: &str,
    ) -> Result<LoadedTarget, String> {
        let labels =
            if name.ends_with("/...")
                || name.ends_with(":all")
                || name.ends_with(":*")
                || name.ends_with(":all-targets")
            {
                self.select(&[name.to_owned()], current_package)?
            } else {
                // Label syntax first (LabelParser.Parts.parse, cmdline/LabelParser.java),
                // then repository visibility with Bazel's wording
                // (cmdline/RepositoryMapping.java, RepositoryName.getNameWithAt).
                crate::bazel_label::Parts::parse(name)?;
                let label = self.repositories.parse(name, "", current_package).map_err(
                    |error| match name
                        .strip_prefix('@')
                        .filter(|rest| !rest.starts_with('@'))
                        .and_then(|rest| rest.split_once("//"))
                    {
                        Some((repository, _))
                            if error.starts_with("unknown apparent repository") =>
                        {
                            format!("No repository visible as '@{repository}' from main repository")
                        }
                        _ => error,
                    },
                )?;
                self.target(&label)?;
                vec![label]
            };
        let [label] = labels.as_slice() else {
            return Err("user-defined flags must reference exactly one target".to_owned());
        };
        // Options are loaded through the target pattern phase, which reports
        // deprecated targets (BlazeOptionHandler.java:393-409).
        self.report_pattern_deprecation(label);
        Ok(match self.target(label)? {
            Target::Source(_) => LoadedTarget::NotRule(label.clone()),
            // Target.getAssociatedRule of an output file is its generating rule.
            Target::Output(..) => LoadedTarget::OtherRule(label.clone()),
            Target::Rule(rule) => {
                if let Some(info) = self.build_setting_info(&rule)? {
                    LoadedTarget::BuildSetting(info)
                } else if matches!(rule.kind, RuleKind::Native("alias")) {
                    let native = rule.native.as_ref().expect("native alias");
                    let actual = if native.configurable.contains_key("actual") {
                        None
                    } else {
                        match native.attrs.get("actual") {
                            Some(crate::custom_rule::AttrValue::Label(Some(actual))) => {
                                Some(self.resolve_rule_src(&rule, actual)?)
                            }
                            _ => None,
                        }
                    };
                    LoadedTarget::Alias {
                        label: label.clone(),
                        actual,
                    }
                } else {
                    LoadedTarget::OtherRule(label.clone())
                }
            }
        })
    }

    /// IncompatibleTargetChecker.IncompatibleTargetProducer
    /// (analysis/constraints/IncompatibleTargetChecker.java:126-227). The
    /// configured `target_compatible_with` labels (the built-in attribute
    /// of Starlark rules and of native rules that are not exempt from
    /// constraint checking) are analyzed in the rule's own configuration;
    /// each must provide a ConstraintValueInfo or a ConfigMatchingProvider
    /// (the attribute's mandatory providers, BaseRuleClasses.java:331-337),
    /// and they are the rule's `ctx.attr.target_compatible_with`. For a rule
    /// that uses toolchain resolution (`checked`), with a known target
    /// platform: each constraint value must be the platform's value of its
    /// setting (ConstraintCollection.hasConstraintValue) and each
    /// config_setting must match; duplicate constraint settings
    /// (ConstraintCollection.validateConstraints) and config_setting errors
    /// are validation errors at the rule's location. A compatible target is
    /// analyzed as usual; an incompatible one fails explicitly, naming the
    /// target platform, the unmet constraints and the non-matching
    /// config_settings (each sorted and deduplicated like
    /// IncompatiblePlatformProvider) in Bazel's words
    /// (TopLevelConstraintSemantics.formatIncompatibilityReasons): Bazel
    /// makes it an IncompatiblePlatformProvider target that wildcard patterns
    /// skip and an explicit request rejects, which the native profile does
    /// not model (docs/PLATFORMS.md). An incompatible target never reaches
    /// the mandatory-provider check (its dependencies are not analyzed).
    #[allow(clippy::too_many_arguments)]
    fn check_target_compatibility(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        conditions: &[(String, ConfigMatching)],
        checked: bool,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        // The toolchain rule's own target_compatible_with is a toolchain
        // resolution input (platform_prerequisite_attributes).
        if matches!(rule.kind, RuleKind::Native("toolchain")) {
            return Ok(());
        }
        // Most targets do not set the attribute: no condition map for them.
        if !configured_attributes::has_native_value(rule, "target_compatible_with") {
            return Ok(());
        }
        let conditions = conditions.iter().cloned().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        let labels = match configured_attributes::configured_native_value(
            rule,
            &resolver,
            "target_compatible_with",
        )
        .map_err(|error| located(rule.location.as_deref(), error))?
        {
            Some(crate::custom_rule::AttrValue::Labels(labels)) if !labels.is_empty() => labels,
            _ => return Ok(()),
        };
        let mut constraints = Vec::new();
        let mut settings = Vec::new();
        let mut unprovided = None;
        for text in &labels {
            let label = self.resolve_rule_src(rule, text)?;
            self.analyze(&label, config, active, configured, plan)?;
            if self.cquery_capture.is_some() {
                self.capture_cquery_edge(
                    &ConfiguredTargetKey::new(rule.label.clone(), config),
                    label.clone(),
                    config,
                    "target_compatible_with",
                    false,
                    false,
                );
            }
            let node = configured
                .get(&ConfiguredTargetKey::new(label.clone(), config))
                .cloned()
                .unwrap_or_default();
            if node.constraint_value.is_none() && node.matching.is_none() {
                unprovided.get_or_insert(label.clone());
            }
            if let Some(value) = node.constraint_value {
                constraints.push(value);
            }
            if let Some(matching) = node.matching {
                // ConfiguredTarget.getOriginalLabel: the label as written.
                settings.push((label, matching));
            }
        }
        let provider_error = |label: &Label| {
            located(
                rule.location.as_deref(),
                format!(
                    "in target_compatible_with attribute of {} rule {}: '{}' does not have mandatory providers: ConfigMatchingProvider or 'ConstraintValueInfo'",
                    rule.rule_class_name(),
                    crate::rule_declarations::canonical_form(&rule.label),
                    crate::rule_declarations::canonical_form(label)
                ),
            )
        };
        if !checked {
            return unprovided_error(unprovided.as_ref(), provider_error);
        }
        crate::platforms::validate_constraints(&constraints)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        // ConfigMatchingProvider.accumulateMatchResults (:155-168).
        let mut errors: Vec<(&Label, &[String])> = Vec::new();
        let mut not_matching = Vec::new();
        for (label, matching) in &settings {
            match &matching.result {
                configured_attributes::MatchResult::InError(messages) => {
                    errors.push((label, messages))
                }
                configured_attributes::MatchResult::NoMatch(_) => not_matching.push(label),
                configured_attributes::MatchResult::Match => {}
            }
        }
        if !errors.is_empty() {
            return Err(located(
                rule.location.as_deref(),
                errors
                    .iter()
                    .map(|(label, messages)| {
                        format!("For config_setting {label}: {}", messages.join(", "))
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        // The target platform is needed only to compare constraint values
        // (a config_setting's result is already known).
        let platform = if constraints.is_empty() {
            None
        } else {
            Some(self.target_platform(config, configured, plan)?)
        };
        let mut unmet = platform.as_ref().map_or_else(Vec::new, |platform| {
            constraints
                .iter()
                .filter(|value| platform.constraints.get(&value.setting).as_ref() != Some(*value))
                .collect::<Vec<_>>()
        });
        if unmet.is_empty() && not_matching.is_empty() {
            return unprovided_error(unprovided.as_ref(), provider_error);
        }
        // IncompatiblePlatformProvider.incompatibleDueToConstraintsAndConfigSettings
        // (analysis/IncompatiblePlatformProvider.java:113-125): the unmet
        // constraints sorted by label and the non-matching config_settings
        // sorted (Label.compareTo: repository, package, name — the native
        // Label order), each without duplicates (a constraint value listed
        // directly and through an alias is one ConstraintValueInfo; a label
        // has one setting, so equal values are adjacent once sorted).
        unmet.sort_by(|left, right| left.label.cmp(&right.label));
        unmet.dedup();
        not_matching.sort();
        not_matching.dedup();
        // TopLevelConstraintSemantics.formatIncompatibilityReasons (:316-351).
        let mut reasons = Vec::new();
        if let Some(platform) = platform.as_ref().filter(|_| !unmet.is_empty()) {
            let mut reason = format!(
                " target platform ({}) didn't satisfy constraint",
                platform.label
            );
            match unmet.as_slice() {
                [only] => reason.push_str(&format!(" {}", only.label)),
                many => reason.push_str(&format!(
                    "s [{}]",
                    many.iter()
                        .map(|value| value.label.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
            reasons.push(reason);
        }
        if !not_matching.is_empty() {
            reasons.push(match not_matching.as_slice() {
                [only] => format!(" target configuration didn't match config_setting {only}"),
                many => format!(
                    " target configuration didn't match config_settings [{}]",
                    many.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Err(located(
            rule.location.as_deref(),
            format!(
                "{} is incompatible with its target platform:{}; skipping incompatible targets (IncompatiblePlatformProvider) is not implemented in the native profile (docs/PLATFORMS.md)",
                crate::rule_declarations::canonical_form(&rule.label),
                reasons.join(" and")
            ),
        ))
    }

    /// ConfigConditionsProducer (analysis/producers/ConfigConditionsProducer.java:91-172):
    /// every `$config_dependencies` label is analyzed in this configuration;
    /// each must then provide a ConfigMatchingProvider
    /// (ConfigConditions.fromConfiguredTarget, analysis/config/ConfigConditions.java:64-83).
    fn config_conditions(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<(String, ConfigMatching)>, String> {
        let timer = crate::plan_timings::StageTimer::start(
            crate::plan_timings::PlanStage::ConfigConditions,
            u64::MAX,
            0,
        );
        let result = self.config_conditions_timed(rule, config, active, configured, plan);
        timer.finish();
        result
    }

    fn config_conditions_timed(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<(String, ConfigMatching)>, String> {
        let mut keys = Vec::new();
        for text in configured_attributes::rule_config_dependencies(rule) {
            let label = self.resolve_rule_src(rule, &text)?;
            // A key that fails to load or analyze is reported by itself, then
            // the consumer adds `errors encountered resolving select() keys
            // for L` at its location (ConfigConditionsProducer.emitErrorIfMostImportant,
            // analysis/producers/ConfigConditionsProducer.java:121-129, 200-211).
            // A missing key of another package is first reported at that
            // package's BUILD file (the target loading event, paired capture
            // B2/lx_missing_key).
            let loaded = self.target(&label).map(|_| ()).map_err(|error| {
                self.foreign_target_event(&rule.label.package_id(), &label, &error)
                    .unwrap_or(error)
            });
            loaded
                .and_then(|()| self.analyze(&label, config, active, configured, plan))
                .map_err(|error| {
                    format!(
                        "{error}\n{}",
                        located(
                            rule.location.as_deref(),
                            format!(
                                "errors encountered resolving select() keys for {}",
                                crate::rule_declarations::canonical_form(&rule.label)
                            )
                        )
                    )
                })?;
            keys.push((text, label));
        }
        let mut conditions = Vec::new();
        for (text, label) in keys {
            let key = ConfiguredTargetKey::new(label.clone(), config);
            // The invalid-condition message names the prerequisite's target,
            // the end of an alias chain (ConfigConditionsProducer.java:156-165,
            // ConfiguredTargetAndData.getTargetLabel).
            let label = self.alias_chain(configured, config, &label).1;
            // ConfigConditions.fromConfiguredTarget (analysis/config/ConfigConditions.java:64-83):
            // a ConfigMatchingProvider, or a constraint_value matched against
            // the target platform when the consuming rule uses toolchain
            // resolution (otherwise the key is not a valid condition).
            let node = configured.get(&key);
            let mut matching = node.and_then(|node| node.matching.clone());
            if matching.is_none()
                && let Some(value) = node.and_then(|node| node.constraint_value.clone())
                && uses_toolchain_resolution(rule)
            {
                let platform = self.target_platform(config, configured, plan)?;
                matching = Some(value.config_matching(&platform));
            }
            let Some(matching) = matching else {
                let target = crate::rule_declarations::canonical_form(&rule.label);
                return Err(located(
                    rule.location.as_deref(),
                    format!(
                        "{} is not a valid select() condition for {target}.\nTo inspect the select(), run: bazel query --output=build {target}.\nFor more help, see https://bazel.build/reference/be/functions#select.\n\n",
                        crate::rule_declarations::canonical_form(&label)
                    ),
                ));
            };
            // Phase-1 prefetch: which select() branches the build certainly
            // takes (crate::prefetch::certain_labels).
            if let Some(prefetch) = &self.prefetch
                && let Ok(key) = Label::parse_canonical(&text)
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
            conditions.push((text, matching));
        }
        Ok(conditions)
    }

    /// CommonPrerequisiteValidator.validateDirectPrerequisiteForTestOnly and
    /// validateDirectPrerequisiteForDeprecation (CommonPrerequisiteValidator.java:683-776)
    /// with BazelPrerequisiteValidator (bazel/rules/BazelPrerequisiteValidator.java:24-33:
    /// no experimental packages, same logical package = same package). Messages
    /// use RuleContext's `in CLASS rule LABEL: ` prefix
    /// (analysis/EventHandlingErrorReporter.java:80-82).
    fn validate_prerequisite(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &dyn Fn() -> String,
        skip_testonly: bool,
    ) -> Result<(), String> {
        // A RuleContext.ruleError at the consuming rule's location.
        self.prerequisite_error(rule, dependency, description, skip_testonly)?
            .map_or(Ok(()), |error| {
                Err(located(rule.location.as_deref(), error))
            })
    }

    /// The configuration facts of the N23c `ctx` members of `rule` analyzed
    /// in `config` (crate::ctx_surface).
    fn ctx_surface(
        &self,
        rule: &Rule,
        config: ConfigId,
        output_root: &str,
        target_platform: crate::platforms::TargetPlatform,
        template_variables: Vec<Vec<(String, String)>>,
    ) -> crate::ctx_surface::CtxSurface {
        use crate::ctx_surface::FeatureSet;
        let configuration = self.configurations.get(config);
        let native = &configuration.native;
        // RuleContext.computeFeatures (analysis/RuleContext.java:268-276): the
        // package's features, then the rule's `features`, under the
        // configuration's (`--features`/`--host_features`, rejected natively).
        let package = self
            .packages
            .get(&rule.label.package_id())
            .map(|package| package.args.features.clone())
            .unwrap_or_default();
        let own = rule
            .custom
            .as_ref()
            .and_then(|custom| custom.declaration.builtin.target.attrs.get("features"))
            .map(|value| match value {
                crate::custom_rule::AttrValue::Strings(features) => {
                    FeatureSet::parse(features.iter().map(String::as_str))
                }
                _ => FeatureSet::default(),
            })
            .unwrap_or_default();
        let features = FeatureSet::merge_with_global(
            &FeatureSet::merge(&package, &own),
            &FeatureSet::default(),
        );
        // Package.Metadata buildFilename relative to the source root: the
        // package path and the BUILD file name of the rule's location.
        let build_file = rule
            .location
            .as_deref()
            .and_then(|location| {
                let path = location.rsplitn(3, ':').nth(2)?;
                Path::new(path).file_name()?.to_str().map(str::to_owned)
            })
            .unwrap_or_else(|| "BUILD".to_owned());
        let build_file_path = if rule.label.package.is_empty() {
            build_file
        } else {
            format!("{}/{build_file}", rule.label.package)
        };
        crate::ctx_surface::CtxSurface {
            bin_dir: output_root.to_owned(),
            compilation_mode: native.compilation_mode,
            defines: native.define.clone(),
            is_exec: native.is_exec,
            no_config: native.no_config,
            stamp: native.stamp,
            coverage_enabled: native.collect_code_coverage,
            platforms: if native.platforms.is_empty() {
                vec![native.host_platform.clone()]
            } else {
                native.platforms.clone()
            },
            host_platform: native.host_platform.clone(),
            short_id: configuration.id.to_string()[..7].to_owned(),
            cpp: crate::custom_rule::CppConfigurationView::of(native),
            target_platform,
            template_variables,
            features,
            build_file_path,
            apple_sdk_versions: native.apple_sdk_versions.clone(),
            // Replaced by the analyzed target's own Context::action_env
            // (crate::analysis), the same configuration's environment.
            action_env: Arc::new(native.action_environment()),
            option_values: native.values.clone(),
        }
    }

    /// The testonly error of [`Self::validate_prerequisite`] (`None` when
    /// valid); deprecation warnings are recorded. `dependency` is the end of
    /// an alias chain and `description` its describeTargetWithAliases text.
    fn prerequisite_error(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &dyn Fn() -> String,
        skip_testonly: bool,
    ) -> Result<Option<String>, String> {
        let (generating, output) = match self.target(dependency)? {
            Target::Rule(dep) => (dep, false),
            Target::Output(dep, _) => (dep, true),
            Target::Source(_) => return Ok(None),
        };
        // The texts are formatted only for a reported error or warning.
        let shown = || crate::rule_declarations::canonical_form(&rule.label);
        let prefix = || format!("in {} rule {}: ", rule.rule_class_name(), shown());
        let args = &self.packages[&rule.label.package_id()].args;
        let dependency_args = &self.packages[&generating.label.package_id()].args;
        // Output files of testonly rules are only checked with
        // --incompatible_check_testonly_for_output_files (default false,
        // analysis/config/CoreOptions.java:690-701).
        let testonly_error = (!skip_testonly
            && !output
            && generating.is_test_only(dependency_args)
            && !rule.is_test_only(args))
        .then(|| {
            format!(
                "{}non-test target '{}' depends on testonly {} and doesn't have testonly attribute set",
                prefix(),
                shown(),
                description()
            )
        });
        // validate() runs the testonly check, then the deprecation check, for
        // each edge (CommonPrerequisiteValidator.java:48-60): an edge's
        // deprecation warning is still reported with its testonly error.
        if rule.deprecation_warning(args).is_none()
            && rule.label.package_id() != generating.label.package_id()
            && let Some(warning) = generating.deprecation_warning(dependency_args)
        {
            let shown_dependency = crate::rule_declarations::canonical_form(dependency);
            let message = if output {
                format!(
                    "{}target '{}' depends on the output file {shown_dependency} of a deprecated rule {}': {warning}",
                    prefix(),
                    shown(),
                    crate::rule_declarations::canonical_form(&generating.label)
                )
            } else {
                format!(
                    "{}target '{}' depends on deprecated target '{shown_dependency}': {warning}",
                    prefix(),
                    shown()
                )
            };
            // Skyframe reports an identical warning event once
            // (EmittedEventState.addWarning, skyframe/EmittedEventState.java:32-52):
            // an edge to the same deprecated target in two configurations,
            // or through two attributes, warns once.
            let warning = located(rule.location.as_deref(), message);
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        Ok(testonly_error)
    }

    /// The artifacts of `label` configured in `config`.
    fn artifacts(
        &mut self,
        label: &Label,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Artifacts>,
        declared: &Declared,
    ) -> Result<Vec<Artifact>, String> {
        self.shared_artifacts(label, config, built, declared)
            .map(Arc::unwrap_or_clone)
    }

    /// [`Self::artifacts`], sharing a planned target's list.
    fn shared_artifacts(
        &mut self,
        label: &Label,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Artifacts>,
        declared: &Declared,
    ) -> Result<Artifacts, String> {
        match self.target(label)? {
            Target::Source(real) => Ok(Arc::new(vec![Artifact::new(
                real,
                label.source_exec_path(),
                None,
                label.to_string(),
                false,
                false,
            )])),
            Target::Rule(rule) => built
                .get(&ConfiguredTargetKey::new(rule.label.clone(), config))
                .cloned()
                .ok_or_else(|| format!("internal analysis order error: {label}")),
            Target::Output(rule, out) => {
                // The generating rule's configuration, after its rule
                // transition (RuleTransitionApplier uses the output's
                // associated rule).
                let config = self
                    .rule_delegations
                    .get(&ConfiguredTargetKey::new(rule.label.clone(), config))
                    .map_or(config, |owner| owner.config);
                let exec = rule
                    .label
                    .with_name(&out)?
                    .generated_exec_path(&self.configurations.output_root(config));
                declared
                    .get(&(config, exec))
                    .cloned()
                    .map(|artifact| Arc::new(vec![artifact]))
                    .ok_or_else(|| format!("missing analyzed output {label}"))
            }
        }
    }

    /// Native provider collection for configured analysis, before any global
    /// action reconciliation. Filegroup's NestedSet<Artifact> distinguishes
    /// owner/kind identities even when their execution paths coincide. Repeated
    /// aliases/forwarded Files deduplicate, in first-occurrence order.
    ///
    /// This is intentionally not `inputs`: cquery skips global action conflicts
    /// and has no producers to normalize by. No location/path vectors, action
    /// inputs, fingerprints or execution trees are needed here.
    fn semantic_inputs(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Artifacts>,
        declared: &Declared,
    ) -> Result<Vec<Artifact>, String> {
        let mut inputs = Vec::new();
        let mut seen: HashSet<File> = HashSet::new();
        for src in &rule.srcs {
            let label = self.resolve_rule_src(rule, src)?;
            for artifact in self.artifacts(&label, config, built, declared)? {
                if seen.insert(artifact.file.clone()) {
                    inputs.push(artifact);
                }
            }
        }
        Ok(inputs)
    }

    fn inputs(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Artifacts>,
        declared: &Declared,
    ) -> Result<ResolvedInputs, String> {
        let mut inputs = Vec::new();
        // Lookups only: `inputs` keeps the declaration order.
        let mut seen: HashMap<SharedPath, Artifact> = HashMap::new();
        let mut locations = BTreeMap::new();
        for src in &rule.srcs {
            let label = self.resolve_rule_src(rule, src)?;
            let artifacts = self.artifacts(&label, config, built, declared)?;
            let paths = artifacts
                .iter()
                .map(|a| a.exec.to_path_buf())
                .collect::<Vec<_>>();
            locations.insert(label.to_string(), paths.clone());
            locations.insert(src.clone(), paths);
            for artifact in artifacts {
                if let Some(previous) = seen.get(artifact.exec.as_path()) {
                    // One artifact without its owner: a generated one is
                    // identified by its producer (two targets' Artifacts of
                    // a shared action's output are one input).
                    if previous.real != artifact.real
                        || previous.producer != artifact.producer
                        || (artifact.producer.is_none() && previous.owner != artifact.owner)
                    {
                        return Err(format!(
                            "different artifacts map to the same execution path: {}",
                            artifact.exec.display()
                        ));
                    }
                } else {
                    seen.insert(artifact.exec.clone(), artifact.clone());
                    inputs.push(artifact);
                }
            }
        }
        Ok((inputs, locations))
    }

    /// TargetPatternPhaseFunction.maybeReportDeprecation
    /// (skyframe/TargetPatternPhaseFunction.java:255-275): a loaded rule whose
    /// `deprecation` is explicitly specified (package defaults excluded, ""
    /// included) warns at its location.
    fn report_pattern_deprecation(&mut self, label: &Label) {
        if let Ok(Target::Rule(rule)) = self.target(label) {
            let explicit = rule
                .native
                .as_ref()
                .and_then(|native| native.string("deprecation"))
                .or_else(|| {
                    rule.custom
                        .as_ref()
                        .and_then(|custom| custom.declaration.builtin.target.string("deprecation"))
                });
            if let Some(message) = explicit {
                self.warnings.push(located(
                    rule.location.as_deref(),
                    format!(
                        "target '{}' is deprecated: {message}",
                        crate::rule_declarations::canonical_form(&rule.label)
                    ),
                ));
            }
        }
    }

    pub fn build(&mut self, labels: &[Label], cache: &Path) -> Result<BuildSummary, String> {
        self.build_with_jobs(labels, cache, 1)
    }

    pub fn build_with_jobs(
        &mut self,
        labels: &[Label],
        cache: &Path,
        jobs: usize,
    ) -> Result<BuildSummary, String> {
        self.build_with_options(labels, cache, jobs, None)
    }

    /// `build_with_jobs` plus the native-only `--native-action-dump=<file>`
    /// diagnostic: after planning, every action of the build is appended to
    /// the file as one JSON object (id, mnemonic, argv, environment, inputs,
    /// outputs, execution requirements) before execution (a native
    /// introspection facility; Bazel has no equivalent option — aquery is
    /// the genuine surface, docs/CC_ACTIONS.md).
    ///
    /// The library path of `build`, the CLI's pipeline (L2-W4): plan the
    /// request ([`Self::plan_request`]), hand it over
    /// (crate::snapshot::codec::hand_over) and execute it
    /// (crate::request::execute). The analysis warnings and errors stay in
    /// the engine ([`Self::take_warnings`], [`Self::take_errors`]).
    pub fn build_with_options(
        &mut self,
        labels: &[Label],
        cache: &Path,
        jobs: usize,
        action_dump: Option<&Path>,
    ) -> Result<BuildSummary, String> {
        if jobs == 0 {
            return Err("jobs must be greater than zero".into());
        }
        let request = self.plan_request(RequestSpec::build(labels))?;
        let mut request = crate::snapshot::codec::hand_over(request)?;
        let diagnostics = request.take_diagnostics();
        self.warnings.extend(diagnostics.warnings);
        self.errors.extend(diagnostics.errors);
        let status = self.take_workspace_status();
        crate::request::execute(
            request,
            crate::request::StatusSource::Planned(status),
            &crate::request::ExecuteOptions {
                cache,
                jobs,
                action_dump,
            },
        )
        .map(|executed| executed.summary)
    }

    /// Plan one build, test or run request (L2-W4, PLAN-L2 §1d): analysis
    /// and action planning ([`Self::plan_build`]), then everything execution
    /// and reporting read, as plain data (crate::request::PlannedRequest):
    /// the reachable actions in plan order with the workspace status
    /// placeholders, the reported artifacts, the run plans of `spec.runs`
    /// with their configurations' summaries, and the analysis warnings and
    /// errors (moved out of the engine). The snapshot blockers are decided
    /// here, on the plan: a build-info callback or a `map_each` of an action
    /// the plan pruned does not block (the record mode's eligibility,
    /// crate::observed_fs::settle_plan_taints). Marks `plan.end` (which
    /// closes the recording window) when planning succeeded.
    pub fn plan_request(&mut self, spec: RequestSpec<'_>) -> Result<PlannedRequest, String> {
        self.run_requests = spec.runs.iter().cloned().collect();
        self.run_trees = spec.trees;
        self.run_plans.clear();
        let planned = self.plan_build(spec.build);
        self.run_requests.clear();
        self.run_trees = true;
        let (nodes, requested, aspect_reports, non_empty_targets) = planned?;
        let plans = std::mem::take(&mut self.run_plans);
        // --keep_going: a `test` request runs only the tests that analyzed
        // (a test whose analysis failed is not one of its tests, genuine
        // capture tests/fixtures/keep_going).
        let keep_going = self.keep_going_plan.take();
        let mut command = spec.command;
        if let (Some(plan), crate::request::RequestCommand::Test { build, tests }) =
            (&keep_going, &mut command)
            && !plan.failed.is_empty()
        {
            let failed: HashSet<&Label> = plan.failed.iter().map(|(label, _)| label).collect();
            build.retain(|label| !failed.contains(label));
            tests.retain(|test| !failed.contains(&test.label));
        }
        let test_count = match &command {
            crate::request::RequestCommand::Test { tests, .. } => Some(tests.len()),
            _ => None,
        };
        let produced = |artifact: &Artifact| crate::request::Produced {
            real: artifact.real.to_path_buf(),
            producer: artifact.producer.clone(),
        };
        // What the run targets read from their configurations, once per
        // configuration (every run plan's is the top-level one today).
        let mut configurations: Vec<crate::request::ConfigurationSummary> = Vec::new();
        let mut configuration_index: BTreeMap<ConfigId, usize> = BTreeMap::new();
        let mut run_plans = Vec::new();
        for (label, plan) in plans {
            // A plan without an executable runs nothing (no run target).
            let Some(executable) = &plan.executable else {
                continue;
            };
            let configuration = match configuration_index.get(&plan.config) {
                Some(index) => *index,
                None => {
                    let native = &self.configurations.get(plan.config).native;
                    configurations.push(crate::request::ConfigurationSummary {
                        mnemonic: self.configurations.mnemonic(plan.config).to_owned(),
                        action_env: native.action_environment(),
                        test_env: native.test_environment(),
                    });
                    configuration_index.insert(plan.config, configurations.len() - 1);
                    configurations.len() - 1
                }
            };
            run_plans.push((
                label,
                crate::request::PlannedRun {
                    executable: produced(executable),
                    executable_exec: executable.exec.to_path_buf(),
                    executable_short_path: executable.file.short_path.clone(),
                    tree: plan.tree.as_ref().map(|tree| {
                        (
                            produced(tree),
                            crate::request::PlannedPath(tree.exec.to_path_buf()),
                        )
                    }),
                    entries: plan.entries.as_ref().map(|entries| {
                        entries
                            .iter()
                            .map(|(path, artifact)| {
                                (path.to_string(), artifact.as_ref().map(produced))
                            })
                            .collect()
                    }),
                    settings: plan.settings,
                    configuration,
                    completion: plan.completion,
                },
            ));
        }
        // The build-info actions the plan kept (it prunes unreachable ones).
        let planned_ids: HashSet<&str> = nodes.iter().map(|node| node.action.id.as_str()).collect();
        let build_info = std::mem::take(&mut self.build_info_actions)
            .into_iter()
            .filter(|id| planned_ids.contains(id.as_str()))
            .collect();
        drop(planned_ids);
        let bep = self.bep_plan(&requested);
        let request = PlannedRequest {
            command,
            keep_going,
            execute: self.execute_build,
            requested: requested
                .iter()
                .map(|(key, artifact)| (key.label.clone(), produced(artifact)))
                .collect(),
            aspect_reports: aspect_reports
                .iter()
                .map(|(aspect, label, artifacts)| {
                    (
                        aspect.clone(),
                        label.clone(),
                        artifacts.iter().map(produced).collect(),
                    )
                })
                .collect(),
            nodes,
            non_empty_targets,
            run_plans,
            configurations,
            warnings: std::mem::take(&mut self.warnings),
            errors: std::mem::take(&mut self.errors),
            build_info,
            bep,
        };
        // The link policy's canonical forms, once (W6R).
        self.repositories.record_link_anchors();
        // Portable descriptions of the deferred `map_each` functions, while
        // the analysis snapshot records (crate::snapshot::mapper): checked
        // ones do not block the save.
        if self.repositories.records_for_replay() {
            let settled = crate::snapshot::mapper::settle(&request, &self.repositories);
            crate::snapshot::mapper::report_settled(&settled);
        }
        crate::observed_fs::settle_plan_taints(
            request
                .blockers()
                .into_iter()
                .map(|blocker| (blocker.taint, blocker.detail)),
        );
        crate::progress::analysis_report(spec.build, test_count);
        crate::perf_mark("plan.end");
        Ok(request)
    }

    /// The build event plan (crate::bep::plan, docs/BEP.md) of the last
    /// planned build: each requested target's TargetConfigured data, its
    /// reported output groups (indices into `requested`) and the
    /// configurations named. The first configuration is the top-level one
    /// as requested (with the test options; BuildConfigurationEvent of the
    /// analysis), a non-test target's the trimmed one
    /// (TestTrimmingTransition).
    fn bep_plan(
        &mut self,
        requested: &[(ConfiguredTargetKey, Artifact)],
    ) -> crate::bep::plan::BepPlan {
        use crate::bep::plan::{BepConfiguration, BepPlan, BepTarget};
        let groups = std::mem::take(&mut self.bep_groups);
        let top = self.configurations.top();
        let configuration = |engine: &Self, id: String| {
            let native = &engine.configurations.get(top).native;
            let mnemonic = engine.configurations.mnemonic(top).to_owned();
            let cpu = native
                .cpu
                .clone()
                .unwrap_or_else(|| crate::configuration::host_cpu().to_owned());
            BepConfiguration {
                id,
                platform_name: cpu.clone(),
                make_variables: vec![
                    ("TARGET_CPU".to_owned(), std::env::consts::ARCH.to_owned()),
                    (
                        "COMPILATION_MODE".to_owned(),
                        native.compilation_mode.to_owned(),
                    ),
                    (
                        "BINDIR".to_owned(),
                        crate::config_registry::output_root(&mnemonic),
                    ),
                    (
                        "GENDIR".to_owned(),
                        crate::config_registry::output_root(&mnemonic),
                    ),
                ],
                cpu,
                mnemonic,
                is_tool: native.is_exec,
            }
        };
        let untrimmed = self
            .configurations
            .top_configuration()
            .untrimmed_id()
            .to_string();
        let mut configurations = vec![configuration(self, untrimmed.clone())];
        let mut targets: Vec<BepTarget> = Vec::new();
        let mut index: HashMap<Label, usize> = HashMap::new();
        for (label, group, start, end) in groups {
            let position = match index.get(&label) {
                Some(position) => *position,
                None => {
                    let (kind, tags, completion_tags, test) = match self.target(&label) {
                        Ok(Target::Rule(rule)) => {
                            let kind = format!("{} rule", rule.rule_class_name());
                            let tags = rule.tags().to_vec();
                            match test_rule_properties(&rule) {
                                Some(info) => (
                                    kind,
                                    tags,
                                    info.rule_tags().into_iter().collect(),
                                    Some((
                                        info.size.name().to_ascii_uppercase(),
                                        info.timeout.seconds(),
                                    )),
                                ),
                                None => (kind, tags.clone(), tags, None),
                            }
                        }
                        Ok(Target::Output(..)) => {
                            ("generated file".to_owned(), Vec::new(), Vec::new(), None)
                        }
                        _ => ("source file".to_owned(), Vec::new(), Vec::new(), None),
                    };
                    // A test target keeps the test options; any other is
                    // trimmed (the modelled top-level configuration).
                    let configuration_index = if test.is_some() {
                        0
                    } else {
                        if configurations.len() == 1 {
                            configurations.push(configuration(self, top.to_string()));
                        }
                        1
                    };
                    targets.push(BepTarget {
                        label: crate::test_runner::label_print(&label),
                        kind,
                        tags,
                        completion_tags,
                        test,
                        configuration: configuration_index,
                        groups: Vec::new(),
                    });
                    index.insert(label.clone(), targets.len() - 1);
                    targets.len() - 1
                }
            };
            let files: Vec<usize> = (start..end.min(requested.len())).collect();
            if !files.is_empty() || group == crate::output_group_info::DEFAULT {
                targets[position].groups.push((group, files));
            }
        }
        BepPlan {
            configurations,
            targets,
        }
    }

    /// Record the canonical forms the link policy took unrecorded (W6R,
    /// crate::repository::RepositorySet::record_link_anchors): at the end
    /// of planning, inside the analysis snapshot's recording window — for
    /// build, test and run [`Self::plan_request`] does; `query` (whose
    /// planning is its loading) before its window closes.
    pub fn record_link_anchors(&self) {
        self.repositories.record_link_anchors();
    }

    /// The workspace status the last planned request computed, taken: its
    /// finalize expands the status placeholders with it
    /// (crate::request::StatusSource::Planned). `None` when planning
    /// computed none (no placeholder then).
    pub fn take_workspace_status(
        &mut self,
    ) -> Option<crate::workspace_status::ComputedWorkspaceStatus> {
        std::mem::take(&mut self.workspace_status_once).into_inner()
    }

    /// The REAPI platform inputs of `rule`'s spawns in `config`
    /// (PlatformUtils.getPlatformProto, analysis/platform/PlatformUtils.java;
    /// crate::remote_execution::SpawnPlatform): the spawn's combined exec
    /// properties — its exec group's (N35, crate::exec_groups::ActionExecution)
    /// or, for a rule without an execution table (genrule), its execution
    /// platform's default-group properties under the target's
    /// `exec_properties` (`target_properties`: an aspect's spawn has none of
    /// its base target's, ConfiguredTargetFactory.createAspect) — and the
    /// execution platform's own `exec_properties` and
    /// `remote_execution_properties`. `Err` names why they are unknown: an
    /// execution platform analysis did not resolve (N41), whose spawns then
    /// never use the remote cache or remote execution.
    fn remote_platform(
        &self,
        rule: &Rule,
        config: ConfigId,
        execution: Option<&crate::exec_groups::ActionExecution>,
        target_properties: bool,
    ) -> Result<crate::remote_execution::SpawnPlatform, String> {
        let host = &self.configurations.get(config).native.host_platform;
        let label = execution.map_or(host, |execution| &execution.platform);
        let (exec_properties, remote_execution_properties) = if *label
            == crate::configuration::default_host_platform()
        {
            (Vec::new(), String::new())
        } else {
            match self
                .platforms
                .get(&ConfiguredTargetKey::new(label.clone(), config))
            {
                Some(Ok(info)) => (
                    info.exec_properties.clone(),
                    info.remote_execution_properties.clone(),
                ),
                _ => {
                    return Err(format!(
                        "actions whose execution platform {label} has or may have exec_properties/remote_execution_properties (not resolved by the native analysis)"
                    ));
                }
            }
        };
        let combined = match execution {
            Some(execution) => execution.properties.clone(),
            None => {
                let attrs = match (&rule.native, &rule.custom) {
                    (Some(native), _) => Some(&native.attrs),
                    (None, Some(custom)) => Some(&custom.declaration.builtin.target.attrs),
                    _ => None,
                };
                let raw = match attrs.and_then(|attrs| attrs.get("exec_properties")) {
                    Some(crate::custom_rule::AttrValue::StringDict(entries))
                        if target_properties =>
                    {
                        entries.clone()
                    }
                    Some(crate::custom_rule::AttrValue::StringDict(_)) | None => Vec::new(),
                    Some(_) => {
                        return Err(format!(
                            "actions of {} (a configurable exec_properties value)",
                            rule.label
                        ));
                    }
                };
                let group = crate::exec_groups::GroupPlatform {
                    name: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME,
                    platform_properties: &exec_properties,
                };
                crate::exec_groups::combined_exec_properties(
                    Some(std::slice::from_ref(&group)),
                    &raw,
                    &self.display_form(&rule.label),
                )
                .map_err(|error| format!("actions of {}: {error}", rule.label))?
                .into_iter()
                .find(|(name, _)| name == crate::exec_groups::DEFAULT_EXEC_GROUP_NAME)
                .map(|(_, properties)| properties)
                .unwrap_or_default()
            }
        };
        Ok(crate::remote_execution::SpawnPlatform {
            combined,
            label: self.display_form(label),
            exec_properties,
            remote_execution_properties,
        })
    }

    /// [`Self::remote_platform`] applied to a spawn: the platform, or the
    /// reason that keeps it off the remote cache and remote execution.
    fn set_remote_platform(
        &self,
        action: &mut Action,
        rule: &Rule,
        config: ConfigId,
        execution: Option<&crate::exec_groups::ActionExecution>,
        target_properties: bool,
    ) {
        match self.remote_platform(rule, config, execution, target_properties) {
            Ok(platform) => {
                action.remote_platform = platform;
                action.remote_unsupported = None;
            }
            Err(reason) => action.remote_unsupported = Some(reason),
        }
    }

    /// The transitive repositories of the dependency `label` in
    /// `config` (a rule transition's delegated key included); a dependency
    /// that was not planned (a file, a no-config edge) contributes its own
    /// repository.
    fn dependency_repositories(
        &self,
        repositories: &BTreeMap<ConfiguredTargetKey, Arc<BTreeSet<String>>>,
        label: Label,
        config: Option<ConfigId>,
    ) -> BTreeSet<String> {
        let own = BTreeSet::from([label.repository.clone()]);
        let Some(config) = config else {
            return own;
        };
        let key = ConfiguredTargetKey::new(label, config);
        let key = self.rule_delegations.get(&key).cloned().unwrap_or(key);
        repositories
            .get(&key)
            .map(|known| (**known).clone())
            .unwrap_or(own)
    }

    /// The configuration `key`'s configured target was analyzed under: a
    /// rule transition's delegated (post-transition) configuration
    /// (TargetAndConfigurationProducer.delegateTo, :244-251, returns the
    /// TRANSITIONED key's ConfiguredTargetValue), else `config` itself.
    /// Edges whose pre-transition configurations converge on one
    /// transitioned configuration must see one configured target.
    fn delegated_config(&self, key: &ConfiguredTargetKey, config: ConfigId) -> ConfigId {
        self.rule_delegations
            .get(key)
            .map_or(config, |owner| owner.config)
    }

    /// The configured key whose OutputGroupInfo serves requested target
    /// `label` in `config`: an output file's generating rule
    /// (OutputFileConfiguredTarget delegates its `_validation` group,
    /// OutputFileConfiguredTarget.java:109-118, in the generating rule's
    /// transitioned configuration, Self::artifacts), else the owner of a
    /// rule transition's delegation, else the target itself.
    fn output_group_key(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<ConfiguredTargetKey, String> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        if let Target::Output(rule, _) = self.target(label)?.to_owned() {
            let owner = ConfiguredTargetKey::new(rule.label.clone(), config);
            return Ok(self.rule_delegations.get(&owner).cloned().unwrap_or(owner));
        }
        Ok(self.rule_delegations.get(&key).cloned().unwrap_or(key))
    }

    /// The `_validation` set of the configured target `key` as planned: a
    /// Starlark rule's merged OutputGroupInfo group (an alias's forwarded
    /// copy; the requested key of a rule transition resolves to its
    /// owner's), or a native rule's propagated memo — the set itself, never
    /// its files (W1c), as the stable-order set a consumer shares
    /// (crate::output_group_info::shared_validation); `None` when empty.
    /// The OutputGroupInfo every other target collected from this one
    /// (RuleConfiguredTargetBuilder.collectTransitiveValidationOutputGroups).
    fn planned_validation(
        &self,
        custom_providers: &BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
        validation: &BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::starlark_types::Depset> {
        let key = self.rule_delegations.get(key).unwrap_or(key);
        if let Some(set) = validation.get(key) {
            return Some(set.clone());
        }
        custom_providers
            .get(key)
            .and_then(|providers| providers.get(crate::output_group_info::PROVIDER_ID))
            .and_then(|info| {
                crate::output_group_info::validation(info.value())
                    .map(crate::output_group_info::shared_validation)
            })
    }

    /// The Files of configured target `key`'s output group `group`
    /// (TopLevelArtifactHelper.getAllArtifactsToBuild's
    /// outputGroupInfo.getOutputGroup, :217-220): a Starlark rule's returned
    /// groups; a native rule has none beyond its hidden and validation
    /// groups; the default group is the target's files to build, read by
    /// the caller.
    fn planned_output_group(
        &self,
        custom_providers: &BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
        validation: &BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
        key: &ConfiguredTargetKey,
        group: &str,
    ) -> Vec<crate::starlark_types::File> {
        if group == crate::output_group_info::VALIDATION {
            return self
                .planned_validation(custom_providers, validation, key)
                .map(|set| crate::output_group_info::depset_files(&set))
                .unwrap_or_default();
        }
        let key = self.rule_delegations.get(key).unwrap_or(key);
        custom_providers
            .get(key)
            .and_then(|providers| providers.get(crate::output_group_info::PROVIDER_ID))
            .and_then(|info| crate::output_group_info::groups(info.value()))
            .and_then(|groups| {
                groups
                    .iter()
                    .find(|(name, _)| name == group)
                    .map(|(_, depset)| {
                        depset
                            .flatten()
                            .into_iter()
                            .filter_map(|item| match item {
                                crate::starlark_types::Item::File(file) => Some(file.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    })
            })
            .unwrap_or_default()
    }

    /// The dependency edges of a configured rule: its attribute edges and,
    /// for a Starlark test rule in the built-in profile (where
    /// `@bazel_tools` and the default `test` exec group exist, D3 of
    /// docs/TEST_FLAGS.md), the implicit test-runner attributes
    /// (TEST_RUNNER_ATTRIBUTES), so they are analyzed and their packages'
    /// repositories reach the target's repository mapping manifest
    /// (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest; N23g).
    fn rule_edges(&self, rule: &Rule) -> Vec<Edge> {
        let mut edges = dependency_edges(rule);
        // FilegroupRule (rules/filegroup/FilegroupRule.java:36-80): `data`
        // (after `srcs`), target-configuration prerequisites that only
        // contribute runfiles (Filegroup.java:80-104).
        if let (RuleKind::Filegroup, Some(native)) = (&rule.kind, &rule.native) {
            edges.extend(native.labels("data").into_iter().map(|label| Edge {
                label,
                attribute: "data".to_owned(),
                cfg: EdgeCfg::Target,
            }));
        }
        // TestSuiteRule (rules/test/TestSuiteRule.java:96-110): `tests`,
        // then `$implicit_tests`, both in the target configuration.
        if let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) {
            let edge = |label: String, attribute: &str| Edge {
                label,
                attribute: attribute.to_owned(),
                cfg: EdgeCfg::Target,
            };
            edges.extend(
                native
                    .labels("tests")
                    .into_iter()
                    .map(|label| edge(label, "tests")),
            );
            edges.extend(
                self.implicit_tests(rule)
                    .iter()
                    .map(|label| edge(label.to_string(), "$implicit_tests")),
            );
        }
        let test = rule
            .custom
            .as_ref()
            .is_some_and(|custom| custom.declaration.builtin.kind.test);
        if test && self.repositories.root("bazel_tools").is_ok() {
            edges.extend(
                TEST_RUNNER_ATTRIBUTES
                    .iter()
                    .map(|(attribute, label)| Edge {
                        label: (*label).to_owned(),
                        attribute: (*attribute).to_owned(),
                        cfg: EdgeCfg::Exec(Some(
                            crate::exec_groups::TEST_EXEC_GROUP_NAME.to_owned(),
                        )),
                    }),
            );
        }
        edges
    }

    /// The test properties of a test rule target (TestTargetProperties;
    /// RuleConfiguredTargetBuilder.initializeTestProvider's shard_count
    /// checks, analysis/RuleConfiguredTargetBuilder.java:470-484) and its
    /// location; `None` for any other target.
    pub fn test_rule_info(
        &mut self,
        label: &Label,
    ) -> Result<Option<(crate::test_command::TestRuleInfo, Option<String>)>, String> {
        let Target::Rule(rule) = self.target(label)? else {
            return Ok(None);
        };
        test_rule_info_of(label, &rule, ShardSelect::Reject)
    }

    /// [`Self::test_rule_info`] for the target pattern phase's filters
    /// (FilteringPolicies.FILTER_TESTS, TestFilter): a select() in
    /// `shard_count` is no error there; a `build` never runs the test.
    fn test_filter_info(
        &mut self,
        label: &Label,
    ) -> Result<Option<crate::test_command::TestRuleInfo>, String> {
        let Target::Rule(rule) = self.target(label)? else {
            return Ok(None);
        };
        Ok(test_rule_info_of(label, &rule, ShardSelect::Ignore)?.map(|(info, _)| info))
    }

    /// TargetPatternPhaseFunction (skyframe/TargetPatternPhaseFunction.java
    /// :75-250) for `build` and `test`: the targets of every pattern
    /// (FILTER_MANUAL, negative patterns subtracted, `select_filtered`);
    /// with `determine_tests` (`test`) or `--build_tests_only` the tests
    /// (determineTests, :408-500: each positive pattern's tests and test
    /// suites, FilteringPolicies.FILTER_TESTS, a negative pattern's targets
    /// unfiltered, each expanded by TestsForTargetPatternFunction, then the
    /// command-line TestFilter `accepts`); `--build_tests_only` builds just
    /// those, `test` adds them to the targets; with `--expand_test_suites`
    /// (default) every test_suite among the targets is replaced by its
    /// tests. Test suites expand strictly (TestExpansionValue.key(target,
    /// true), TestsForTargetPatternFunction.java:52), once per suite (the
    /// Skyframe value): a member that is neither a test nor a test suite is
    /// an error event, and the expansion still yields the suite's tests.
    pub fn target_pattern_phase(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        options: &TargetPatternPhaseOptions<'_>,
    ) -> Result<TargetPatternPhase, String> {
        let mut failures = PatternFailures::default();
        let selected = if self.keep_going {
            self.select_keep_going(patterns, relative_directory, &mut failures)
        } else {
            self.select_filtered(patterns, relative_directory, true)?
        };
        let mut expansions = TestExpansions::default();
        let mut tests = None;
        let mut all_excluded = false;
        let mut pattern_error = false;
        if options.determine_tests || options.build_tests_only {
            let (found, filtered, error) =
                self.determine_tests(patterns, relative_directory, options, &mut expansions)?;
            all_excluded = found.is_empty() && !filtered.is_empty();
            pattern_error = error;
            tests = Some(found);
        }
        let mut targets: BTreeSet<Label> = selected.into_iter().collect();
        let mut tests_to_run = None;
        if let Some(found) = tests {
            if options.build_tests_only {
                targets = found.iter().cloned().collect();
            } else {
                targets.extend(found.iter().cloned());
            }
            if options.determine_tests {
                tests_to_run = Some(found);
            }
        }
        let mut build = BTreeSet::new();
        let mut post_expansion_error = false;
        for label in targets {
            // Every selected label's package is loaded: a lookup, no
            // filesystem facts of source-file targets.
            if options.expand_test_suites && self.loaded_test_suite(&label) {
                let (expanded, error) =
                    self.tests_in_suite(&label, &mut expansions, &mut Vec::new())?;
                post_expansion_error |= error;
                build.extend(expanded);
            } else {
                build.insert(label);
            }
        }
        let PatternFailures { loading, skipped } = failures;
        Ok(TargetPatternPhase {
            build: build.into_iter().collect(),
            tests: tests_to_run.map(|tests| tests.into_iter().collect()),
            all_excluded,
            errors: expansions.events,
            pattern_error: pattern_error || !loading.is_empty() || !skipped.is_empty(),
            post_expansion_error,
            loading_errors: loading,
            skipped,
        })
    }

    /// [`Self::select_filtered`] of a keep-going build (FILTER_MANUAL;
    /// getTargetsToBuild and mergeAll, TargetPatternPhaseFunction.java
    /// :280-370 under --keep_going): a pattern that does not parse, or whose
    /// evaluation fails, is skipped (`Skipping 'PATTERN': MESSAGE`; the
    /// parse failures first, as Bazel parses every pattern before it
    /// evaluates any); a wildcard keeps the packages that load and skips
    /// the others, their errors recorded in loading order (natively a
    /// package with errors contributes no targets, docs/KEEP_GOING.md).
    fn select_keep_going(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        failures: &mut PatternFailures,
    ) -> Vec<Label> {
        let mut parsed = Vec::new();
        for pattern in patterns {
            match crate::target_pattern::parse(pattern, relative_directory) {
                Ok(pattern) => parsed.push(pattern),
                Err(message) => failures
                    .skipped
                    .push(format!("Skipping '{pattern}': {message}")),
            }
        }
        let mut selected = BTreeSet::new();
        for pattern in &parsed {
            let filter = !pattern.negative;
            self.pattern_load_errors = Some(Vec::new());
            let expanded = self.expand_pattern(pattern, filter, true);
            for error in self.pattern_load_errors.take().unwrap_or_default() {
                if !failures.loading.contains(&error) {
                    failures.loading.push(error);
                }
            }
            match expanded {
                Ok(targets) if pattern.negative => {
                    selected.retain(|label| !targets.contains(label));
                }
                Ok(targets) => selected.extend(targets),
                Err(error) => failures.skipped.push(format!(
                    "Skipping '{}': {}",
                    pattern.original,
                    error.into_message()
                )),
            }
        }
        selected.into_iter().collect()
    }

    /// TargetPatternPhaseFunction.determineTests (:408-500): the tests of
    /// the patterns in order (a positive pattern adds the expansion of its
    /// FILTER_TESTS targets, a negative one removes the expansion of its
    /// unfiltered targets), then the TestFilter; the tests and the
    /// ResolvedTargets' filtered set (non-empty: "All specified test
    /// targets were excluded by filters") and whether an expansion failed.
    fn determine_tests(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        options: &TargetPatternPhaseOptions<'_>,
        expansions: &mut TestExpansions,
    ) -> Result<(BTreeSet<Label>, BTreeSet<Label>, bool), String> {
        // Under --keep_going the patterns and packages the targets to build
        // skipped (and reported) are skipped here too.
        let parsed = patterns
            .iter()
            .map(|pattern| crate::target_pattern::parse(pattern, relative_directory))
            .filter(|parsed| !(self.keep_going && parsed.is_err()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut tests = BTreeSet::new();
        let mut filtered = BTreeSet::new();
        let mut has_error = false;
        for pattern in &parsed {
            if self.keep_going {
                self.pattern_load_errors = Some(Vec::new());
            }
            let expanded = self.expand_pattern(pattern, !pattern.negative, true);
            if self.keep_going {
                has_error |= self
                    .pattern_load_errors
                    .take()
                    .is_some_and(|errors| !errors.is_empty());
                if expanded.is_err() {
                    has_error = true;
                    continue;
                }
            }
            let mut targets = expanded.map_err(PatternError::into_message)?;
            if !pattern.negative {
                // FilterTests (pkgcache/FilteringPolicies.java:81-87).
                let mut kept = BTreeSet::new();
                for label in targets {
                    if self.test_filter_info(&label)?.is_some() || self.is_test_suite(&label)? {
                        kept.insert(label);
                    }
                }
                targets = kept;
            }
            // TestsForTargetPatternFunction.compute (:42-68).
            let mut expanded = BTreeSet::new();
            for label in targets {
                if self.is_test_suite(&label)? {
                    let (labels, error) =
                        self.tests_in_suite(&label, expansions, &mut Vec::new())?;
                    has_error |= error;
                    expanded.extend(labels);
                } else {
                    expanded.insert(label);
                }
            }
            if pattern.negative {
                for label in expanded {
                    if tests.remove(&label) {
                        filtered.insert(label);
                    }
                }
            } else {
                for label in expanded {
                    filtered.remove(&label);
                    tests.insert(label);
                }
            }
        }
        // testTargetsBuilder.filter(testFilter) (:497): TestFilter.apply
        // (pkgcache/TestFilter.java:128-170) over the test rules.
        let mut kept = BTreeSet::new();
        for label in tests {
            match self.test_filter_info(&label)? {
                Some(info) if !(options.test_filter)(&info) => {
                    filtered.insert(label);
                }
                _ => {
                    kept.insert(label);
                }
            }
        }
        Ok((kept, filtered, has_error))
    }

    fn is_test_suite(&mut self, label: &Label) -> Result<bool, String> {
        Ok(matches!(
            self.target(label)?,
            Target::Rule(rule) if matches!(rule.kind, RuleKind::Native("test_suite"))
        ))
    }

    /// Whether `label` is a test_suite rule of its loaded package.
    fn loaded_test_suite(&self, label: &Label) -> bool {
        self.packages
            .get(&label.package_id())
            .and_then(|package| package.get(label))
            .is_some_and(|rule| matches!(rule.kind, RuleKind::Native("test_suite")))
    }

    /// TestExpansionFunction.computeExpandedTests with `strict`
    /// (skyframe/TestExpansionFunction.java:58-136, TestsForTargetPattern's
    /// key): the test rules of `tests` (a member that is neither a test nor
    /// a test suite is the error event `in test_suite rule 'L': expecting a
    /// test or a test_suite rule but 'M' is not one.` at the suite), the
    /// test rules of `$implicit_tests` (the package's non-manual tests
    /// matching the suite's tags when `tests` is empty,
    /// TestSuiteImplicitTestsAccumulator), filtered by the suite's tags
    /// (TestTargetUtils.filterTests: `tags` plus `size`), plus the
    /// expansion of every test_suite in `tests` (not filtered again). One
    /// value per suite and build (`expansions`), so an event is reported
    /// once; the flag is the value's ResolvedTargets.hasError.
    fn tests_in_suite(
        &mut self,
        suite: &Label,
        expansions: &mut TestExpansions,
        active: &mut Vec<Label>,
    ) -> Result<(BTreeSet<Label>, bool), String> {
        use crate::test_command::{matches_filters, sort_tags_by_sense};
        if let Some(value) = expansions.values.get(suite) {
            return Ok(value.clone());
        }
        if active.contains(suite) {
            return Err(format!("cycle in test_suite expansion at {suite}"));
        }
        let Target::Rule(rule) = self.target(suite)? else {
            return Ok((BTreeSet::new(), false));
        };
        let native = rule.native.clone().unwrap_or_default();
        let mut has_error = false;
        // The tests with their filter tags, then the nested suites.
        let mut result = Vec::new();
        let mut suites = Vec::new();
        for text in native.labels("tests") {
            let label = self.resolve_rule_src(&rule, &text)?;
            match self.target(&label)? {
                Target::Rule(member) if test_suite_filter_tags(&member).is_some() => {
                    result.push((label, test_suite_filter_tags(&member)));
                }
                Target::Rule(member) if matches!(member.kind, RuleKind::Native("test_suite")) => {
                    suites.push(label);
                }
                _ => {
                    expansions.events.push(located(
                        rule.location.as_deref(),
                        format!(
                            "in test_suite rule '{suite}': expecting a test or a test_suite rule but '{label}' is not one."
                        ),
                    ));
                    has_error = true;
                }
            }
        }
        for label in self.implicit_tests(&rule) {
            if let Target::Rule(member) = self.target(&label)? {
                result.push((label, test_suite_filter_tags(&member)));
            }
        }
        let (required, excluded) = sort_tags_by_sense(&native.tags);
        let mut kept = BTreeSet::new();
        for (label, tags) in result {
            if let Some(tags) = tags
                && matches_filters(&tags, &required, &excluded, true)
            {
                kept.insert(label);
            }
        }
        active.push(suite.clone());
        for nested in suites {
            let (expanded, error) = self.tests_in_suite(&nested, expansions, active)?;
            has_error |= error;
            kept.extend(expanded);
        }
        active.pop();
        expansions
            .values
            .insert(suite.clone(), (kept.clone(), has_error));
        Ok((kept, has_error))
    }

    /// A test_suite's `$implicit_tests` (AttributeProvider.java:386-401 with
    /// TestSuiteImplicitTestsAccumulator, packages/
    /// TestSuiteImplicitTestsAccumulator.java:66-95): when `tests` is empty,
    /// the test rules of its (loaded) package without the `manual` tag whose
    /// `tags` and `size` match the suite's tags, sorted; otherwise none.
    fn implicit_tests(&self, suite: &Rule) -> Vec<Label> {
        use crate::test_command::{matches_filters, sort_tags_by_sense};
        let Some(native) = &suite.native else {
            return Vec::new();
        };
        if !native.labels("tests").is_empty() {
            return Vec::new();
        }
        let Some(package) = self.packages.get(&suite.label.package_id()) else {
            return Vec::new();
        };
        let (required, excluded) = sort_tags_by_sense(&native.tags);
        let mut tests: Vec<Label> = package
            .values()
            .filter(|rule| !rule.is_manual())
            .filter(|rule| {
                test_suite_filter_tags(rule)
                    .is_some_and(|tags| matches_filters(&tags, &required, &excluded, true))
            })
            .map(|rule| rule.label.clone())
            .collect();
        tests.sort();
        tests
    }

    /// RunCommand.isExecutable of `label` (Runnable).
    pub fn runnable(&mut self, label: &Label) -> Result<Runnable, String> {
        Ok(match self.target(label)? {
            Target::Source(_) | Target::Output(..) => Runnable::Yes,
            Target::Rule(rule) if forwards(&rule) => Runnable::Yes,
            Target::Rule(rule) => match &rule.custom {
                Some(custom) if custom.declaration.builtin.kind.test => Runnable::Test,
                Some(custom) if custom.declaration.builtin.kind.executable => Runnable::Yes,
                // Rule.isExecutable: the nonconfigurable `executable`
                // attribute (packages/Rule.java:856-864).
                None if genrule_is_executable(&rule) => Runnable::Yes,
                _ => Runnable::No,
            },
        })
    }

    /// Phase 2 of one planned target (in plan order): runs Starlark
    /// implementations and plans actions (ActionPlanning).
    //
    /// The dependency view of one edge (N58: shared with aspect
    /// applications): the resolved target's artifacts, providers (with the
    /// framework OutputGroupInfo), runfiles and the one shared `Target`
    /// value (N55) — everything a consumer's `ctx.attr` entry holds
    /// (DependencyResolver + ConfiguredTargetAndData).
    ///
    /// Memoized per edge key, the requested label in the edge's
    /// configuration (W1a, PLAN-L1 §2.1): the first edge builds the view
    /// (Self::dependency_view) after the dependency committed, when its
    /// every input is final; later edges share it. Errors are never
    /// memoized. `BAZEL_RS_VERIFY_VIEWS=1` rebuilds the view on every
    /// later edge and panics when the memoized one went stale.
    #[allow(clippy::too_many_arguments)]
    fn plan_dependency_view(
        &mut self,
        configured: &mut ConfiguredNodes,
        dry: &mut BTreeMap<ConfiguredTargetKey, Artifacts>,
        custom_providers: &mut BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
        provider_keys: &mut BTreeMap<ConfiguredTargetKey, Vec<String>>,
        runfiles_info: &mut BTreeMap<ConfiguredTargetKey, Arc<crate::runfiles::TargetRunfiles>>,
        artifact_index: &mut BTreeMap<File, Artifact>,
        declared: &mut Declared,
        native_output_groups: &mut BTreeMap<
            ConfiguredTargetKey,
            starlark::values::OwnedFrozenValue,
        >,
        validation: &mut BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
        target_identities: &mut BTreeMap<
            TargetIdentityKey,
            std::sync::Arc<crate::starlark_types::Target>,
        >,
        views: &mut DependencyViews,
        label: &Label,
        edge_config: ConfigId,
    ) -> Result<DependencyTarget, String> {
        let key = ConfiguredTargetKey::new(label.clone(), edge_config);
        if let Some(view) = views.get(&key).cloned() {
            // W0-A view counters: the W1a memo answered, nothing was built.
            crate::plan_timings::note_dependency_view(&(label, edge_config), false);
            if verify_views() {
                let rebuilt = self.dependency_view(
                    configured,
                    dry,
                    custom_providers,
                    provider_keys,
                    runfiles_info,
                    artifact_index,
                    declared,
                    native_output_groups,
                    validation,
                    target_identities,
                    label,
                    edge_config,
                );
                verify_view(&key, &view, rebuilt);
            }
            return Ok(view);
        }
        crate::plan_timings::note_dependency_view(&(label, edge_config), true);
        let view = self.dependency_view(
            configured,
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            label,
            edge_config,
        )?;
        views.insert(key, view.clone());
        Ok(view)
    }

    /// Build the dependency view of one edge key (the miss path of
    /// Self::plan_dependency_view, the per-edge code before W1a). The
    /// dependency's artifacts are registered in `artifact_index` (a source
    /// file's on its first view, W1b registration invariant); everything
    /// else a consumer reaches through the view — provider payloads,
    /// runfiles — was registered by its producer.
    #[allow(clippy::too_many_arguments)]
    fn dependency_view(
        &mut self,
        configured: &mut ConfiguredNodes,
        dry: &mut BTreeMap<ConfiguredTargetKey, Artifacts>,
        custom_providers: &mut BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        >,
        provider_keys: &mut BTreeMap<ConfiguredTargetKey, Vec<String>>,
        runfiles_info: &mut BTreeMap<ConfiguredTargetKey, Arc<crate::runfiles::TargetRunfiles>>,
        artifact_index: &mut BTreeMap<File, Artifact>,
        declared: &mut Declared,
        native_output_groups: &mut BTreeMap<
            ConfiguredTargetKey,
            starlark::values::OwnedFrozenValue,
        >,
        validation: &mut BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
        target_identities: &mut BTreeMap<
            TargetIdentityKey,
            std::sync::Arc<crate::starlark_types::Target>,
        >,
        label: &Label,
        edge_config: ConfigId,
    ) -> Result<DependencyTarget, String> {
        let artifacts = self.shared_artifacts(label, edge_config, dry, declared)?;
        let files = artifacts.iter().map(Artifact::file).collect::<Vec<_>>();
        for artifact in artifacts.iter() {
            register_artifact(artifact_index, &artifact.file, artifact);
        }
        drop(artifacts);
        let dependency = ConfiguredTargetKey::new(label.clone(), edge_config);
        let forwarded = self.forwarded_actual(configured, &dependency);
        let actual = forwarded.clone().unwrap_or_else(|| label.clone());
        let target = self.target(&actual)?;
        // The alias chain's end, in the same configuration
        // (Self::alias_chain), under the configuration a rule
        // transition analyzed it in when it delegated
        // (Self::delegated_config): two edges whose pre-transition
        // configurations converge see the actual's one analysis.
        let actual_key = ConfiguredTargetKey::new(
            actual.clone(),
            self.delegated_config(
                &ConfiguredTargetKey::new(actual.clone(), edge_config),
                edge_config,
            ),
        );
        let is_rule = matches!(&target, Target::Rule(rule) if !matches!(rule.kind, RuleKind::PackageGroup(_)));
        let runfiles = dependency_runfiles(&target, &files, runfiles_info.get(&actual_key));
        let mut providers = custom_providers
            .get(&dependency)
            .cloned()
            .unwrap_or_default();
        // Every rule target has an OutputGroupInfo
        // (RuleConfiguredTargetBuilder.java:155-173): a native rule's
        // is the HIDDEN_TOP_LEVEL group of its default runfiles plus
        // the `_validation` group propagated from its prerequisites
        // (Self::planned_validation; a Starlark rule carries its group in
        // its providers already), built once per configured target, both
        // composed of the existing sets (W1c).
        if is_rule && !providers.contains_key(crate::output_group_info::PROVIDER_ID) {
            let value = match native_output_groups.get(&actual_key) {
                Some(value) => value.clone(),
                None => {
                    let value = crate::output_group_info::freeze(
                        runfiles.default_runfiles().all_artifacts_depset(),
                        self.planned_validation(custom_providers, validation, &actual_key),
                    )?;
                    native_output_groups.insert(actual_key.clone(), value.clone());
                    value
                }
            };
            providers.insert(crate::output_group_info::PROVIDER_ID.to_owned(), value);
        }
        // An output-file target's OutputGroupInfo is its generating rule's
        // `_validation` group, the set itself (OutputFileConfiguredTarget.
        // java:109-118; the configuration follows the generating rule's
        // transition, Self::artifacts), present only when non-empty.
        if let Target::Output(generating, _) = &target {
            let owner = ConfiguredTargetKey::new(generating.label.clone(), edge_config);
            if let Some(set) = self.planned_validation(custom_providers, validation, &owner) {
                providers.insert(
                    crate::output_group_info::PROVIDER_ID.to_owned(),
                    crate::output_group_info::freeze_validation(set)?,
                );
            }
        }
        // InstrumentedFilesInfo.EMPTY (coverage disabled): filegroup
        // and genrule add it (Filegroup.java:63-108,
        // GenRuleBase.java:234-239); an output file answers with its
        // generating rule's or EMPTY (OutputFileConfiguredTarget.java:100-107).
        // In a coverage configuration RuleConfiguredTargetBuilder also
        // adds InstrumentedFilesCollector.forwardAll to every rule
        // without InstrumentedFilesInfo or ToolchainInfo
        // (RuleConfiguredTargetBuilder.java:179-186); every such
        // collected value is unmodelled: present, members explicit.
        let coverage = self
            .configurations
            .get(edge_config)
            .native
            .collect_code_coverage;
        let instrumented = match &target {
            Target::Rule(rule) if coverage => {
                is_rule && !providers.contains_key(crate::toolchain_info::PROVIDER_ID)
            }
            Target::Rule(rule) => {
                matches!(rule.kind, RuleKind::Filegroup | RuleKind::Genrule)
            }
            Target::Output(..) => true,
            Target::Source(_) => false,
        };
        if instrumented && !providers.contains_key(crate::instrumented_files::PROVIDER_ID) {
            let info = if coverage {
                crate::instrumented_files::coverage_gap_frozen(&actual.to_string())?
            } else {
                crate::instrumented_files::empty_frozen()?
            };
            providers.insert(crate::instrumented_files::PROVIDER_ID.to_owned(), info);
        }
        // The one shared Target value of this configured target
        // (N55), keyed as Bazel keys the configured target: a rule
        // or alias edge by the POST-DELEGATION configuration (a
        // rule transition delegates to the transitioned key,
        // TargetAndConfigurationProducer.java:244-251, so edges
        // whose pre-transition configurations converge share one
        // instance), a source file or package group by the label
        // with NO configuration (isConfigurable() false,
        // InputFile.java:88-90, PackageGroup.java:166-168: one
        // null-configuration configured target serves every
        // consumer), an output file by the generating rule's
        // (post-transition) configuration. An alias edge (its own
        // AliasConfiguredTarget) and an exec-configured rule edge
        // stay their own instances.
        let identity = match target_identities.entry({
            let identity_config = if forwarded.is_some() {
                // The alias edge itself keeps the edge's
                // configuration (its own AliasConfiguredTarget).
                Some(self.delegated_config(&dependency, edge_config))
            } else {
                match &target {
                    Target::Source(_) => None,
                    Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => None,
                    Target::Output(rule, _) => Some(self.delegated_config(
                        &ConfiguredTargetKey::new(rule.label.clone(), edge_config),
                        edge_config,
                    )),
                    _ => Some(self.delegated_config(&dependency, edge_config)),
                }
            };
            (label.clone(), identity_config)
        }) {
            std::collections::btree_map::Entry::Occupied(shared) => shared.get().clone(),
            std::collections::btree_map::Entry::Vacant(vacant) => {
                let provider_keys = provider_keys.get(&actual_key).cloned().unwrap_or_default();
                vacant
                    .insert(std::sync::Arc::new(analysis::dependency_target_data(
                        &label.to_string(),
                        &actual.to_string(),
                        &files,
                        matches!(target, Target::Source(_)),
                        is_rule,
                        matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_))),
                        matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::Filegroup)),
                        &providers,
                        &provider_keys,
                        &runfiles,
                    )))
                    .clone()
            }
        };
        Ok(DependencyTarget::new(analysis::DependencyView {
            label: actual.to_string(),
            files,
            is_source: matches!(target, Target::Source(_)),
            is_rule,
            is_package_group: matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_))),
            is_filegroup: matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::Filegroup)),
            providers,
            provider_keys: provider_keys.get(&actual_key).cloned().unwrap_or_default(),
            runfiles,
            rule_class: match &target {
                Target::Rule(rule) if !matches!(rule.kind, RuleKind::PackageGroup(_)) => {
                    Some(rule.rule_class_name().to_owned())
                }
                _ => None,
            },
            identity,
        }))
    }

    /// Plan every aspect application of `collection` on the configured
    /// target of `label`/`edge_config` (N58) and return the
    /// aspect-configured target's dependency view: the base configured
    /// target with each aspect's providers merged in
    /// (MergedConfiguredTarget.of — aspect providers shadow the base
    /// target's, OutputGroupInfo merges per group).
    ///
    /// Each application runs its aspect's implementation `f(target, ctx)`
    /// once, its actions registered under the application's own action
    /// owner (`label%aspect`, never the base target's), its aspect-applied
    /// dependencies planned recursively (AspectFunction's dependency
    /// resolution over the aspect path).
    fn plan_aspect_collection(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        label: &Label,
        edge_config: ConfigId,
        collection: &crate::aspects::AspectCollection,
    ) -> Result<(DependencyTarget, AspectApplications), String> {
        // The base view through the shared dependency machinery.
        let mut base = {
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
            } = state;
            self.plan_dependency_view(
                configured,
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
            )?
        };
        // The application target: the alias chain's end in the edge's
        // configuration, with a rule transition's delegation applied
        // (AspectKeyCreator over ConfiguredTargetKey); an output-file edge
        // applies its aspects to the generating rule when they ask for it
        // (apply_to_generating_rules).
        let requested = ConfiguredTargetKey::new(label.clone(), edge_config);
        let mut base_label = self
            .forwarded_actual(configured, &requested)
            .unwrap_or_else(|| label.clone());
        while let Some(actual) = configured
            .get(&ConfiguredTargetKey::new(base_label.clone(), edge_config))
            .and_then(|node| node.actual.clone())
        {
            base_label = actual;
        }
        if let Target::Output(generating, _) = self.target(&base_label)? {
            let applies = collection.used.iter().any(|(aspect, _)| {
                aspect.declaration.apply_to_generating_rules
                    || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
            });
            if applies {
                base_label = generating.label.clone();
            }
        }
        let base_key = ConfiguredTargetKey::new(
            base_label.clone(),
            self.delegated_config(&requested, edge_config),
        );
        // AspectCollection.createAspectKeys: each aspect over the base
        // target, in application order.
        let mut applications: Vec<(
            crate::aspects::AspectApplicationKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        )> = Vec::new();
        for (aspect, bases) in &collection.used {
            let key = crate::aspects::AspectApplicationKey {
                base: base_key.clone(),
                aspect: aspect.descriptor.clone(),
                bases: bases.clone(),
            };
            if state.aspect_providers.contains_key(&key) {
                applications.push((
                    key.clone(),
                    state
                        .aspect_providers
                        .get(&key)
                        .cloned()
                        .unwrap_or_default(),
                ));
                continue;
            }
            let providers = self.plan_aspect_application(
                state, configured, &base_key, &base, aspect, bases, collection,
            )?;
            state
                .aspect_providers
                .insert(key.clone(), providers.clone());
            applications.push((key, providers));
        }
        base = crate::aspects::merged_dependency(&base, &applications)?;
        // One shared Target value per aspect-configured target (N55's
        // sharing applied to the merged configured target): its providers
        // are the merged set, not the base target's.
        let identity = state
            .aspect_target_identities
            .entry((
                base_label.clone(),
                Some(base_key.config),
                collection.descriptors(),
            ))
            .or_insert_with(|| {
                std::sync::Arc::new(analysis::dependency_target_data(
                    &label.to_string(),
                    &base.label,
                    &base.files,
                    base.is_source,
                    base.is_rule,
                    base.is_package_group,
                    base.is_filegroup,
                    &base.providers,
                    &base.provider_keys,
                    &base.runfiles,
                ))
            })
            .clone();
        base.identity = identity;
        Ok((base, applications))
    }

    /// The planned dependencies of the label attributes of `owner` (the
    /// applied aspect itself, or a base aspect of its path restricted to
    /// `names`) in one application, keyed like a rule context's
    /// ([`analysis::attribute_dependency_key`], split keys for Starlark
    /// transitions), aspect-applied with the path's aspects that propagate
    /// over the attribute (Self::analyze_aspect_attribute_edges).
    #[allow(clippy::type_complexity)]
    fn plan_aspect_attribute_dependencies(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        owner: &crate::aspects::AspectDefinition,
        names: Option<&[String]>,
        attribute_configs: &BTreeMap<String, TransitionConfigurations>,
        path: &[std::sync::Arc<crate::aspects::AspectDefinition>],
    ) -> Result<
        (
            BTreeMap<String, DependencyTarget>,
            BTreeMap<String, crate::custom_rule::SplitAttribute>,
        ),
        String,
    > {
        let mut aspect_dependencies: BTreeMap<String, DependencyTarget> = BTreeMap::new();
        let mut aspect_split_dependencies = BTreeMap::new();
        for (name, spec) in &owner.declaration.attrs {
            if names.is_some_and(|names| !names.contains(name)) {
                continue;
            }
            let Some(branches) = attribute_configs.get(name) else {
                continue;
            };
            let propagating = crate::aspects::propagating_over_aspect_attribute(path, owner, name);
            let split = branches.len() != 1 || !branches[0].0.is_empty();
            let mut labels = Vec::new();
            for text in aspect_attribute_labels(spec) {
                let dependency = self.repositories.parse(
                    &text,
                    &owner.definition_label.repository,
                    &owner.definition_label.package,
                )?;
                labels.push(dependency.clone());
                for (_, attribute_config) in branches.iter() {
                    let attribute_config = *attribute_config;
                    let (view, _) = self.plan_aspect_dependency(
                        state,
                        configured,
                        &dependency,
                        attribute_config,
                        propagating.clone(),
                    )?;
                    aspect_dependencies.insert(
                        if split {
                            analysis::split_dependency_key(
                                &dependency.to_string(),
                                name,
                                attribute_config,
                            )
                        } else {
                            analysis::attribute_dependency_key(&dependency.to_string(), name, spec)
                        },
                        view,
                    );
                }
            }
            if split {
                aspect_split_dependencies
                    .insert(name.clone(), self.split_attribute(name, labels, branches)?);
            }
        }
        Ok((aspect_dependencies, aspect_split_dependencies))
    }

    /// Plan one aspect application (one AspectValue): the aspect's
    /// implementation over its `target` parameter and its dependencies (the
    /// aspect's own attributes plus the base rule's edges, aspect-applied
    /// where the aspect propagates), returning the aspect's providers.
    #[allow(clippy::too_many_arguments)]
    fn plan_aspect_application(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        base_key: &ConfiguredTargetKey,
        base: &DependencyTarget,
        aspect: &std::sync::Arc<crate::aspects::AspectDefinition>,
        bases: &[String],
        collection: &crate::aspects::AspectCollection,
    ) -> Result<AspectProviders, String> {
        let top = self.configurations.top();
        let key = crate::aspects::AspectApplicationKey {
            base: base_key.clone(),
            aspect: aspect.descriptor.clone(),
            bases: bases.to_vec(),
        };
        let fallback;
        let base_rule: &Rule = match self.configured_rules.get(base_key) {
            Some(rule) => rule,
            None => match self.target(&base_key.label)? {
                Target::Rule(rule) => {
                    fallback = rule;
                    &fallback
                }
                _ => {
                    return Err(format!(
                        "internal error: aspect {} applied to non-rule {}",
                        aspect.identity, base_key.label
                    ));
                }
            },
        };
        let base_rule = base_rule.clone();
        // The `target` parameter: the base configured target merged with the
        // directly required aspects' providers (AspectFunction.java:341-346),
        // also what the native ValidateTarget reads its `_validation`
        // outputs from.
        let mut base_aspect_maps = Vec::new();
        for base_aspect in bases {
            // The base aspect's own key carries its directly required
            // aspects (AspectCollection.buildAspectKey over AspectDeps).
            let base_aspect_key = crate::aspects::AspectApplicationKey {
                base: base_key.clone(),
                aspect: base_aspect.clone(),
                bases: collection
                    .used
                    .iter()
                    .find(|(used, _)| used.descriptor == *base_aspect)
                    .map(|(_, used_bases)| used_bases.clone())
                    .unwrap_or_default(),
            };
            let providers = state
                .aspect_providers
                .get(&base_aspect_key)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "internal error: aspect {base_aspect} of {} was not planned before {}",
                        base_key.label, aspect.identity
                    )
                })?;
            base_aspect_maps.push((base_aspect_key, providers));
        }
        let merged_target = crate::aspects::merged_dependency(base, &base_aspect_maps)?;
        // The native ValidateTarget (rules/core/ValidateTarget.java:60-77):
        // promotes the (merged) base target's `_validation` set to
        // `_validation_top_level` (the set itself, W1c); not applicable
        // when it is empty.
        if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
            let Some(validations) = merged_target
                .providers
                .get(crate::output_group_info::PROVIDER_ID)
                .and_then(|value| {
                    crate::output_group_info::validation(value.value())
                        .map(crate::output_group_info::shared_validation)
                })
            else {
                // ConfiguredAspect.NonApplicableAspect: no providers.
                return Ok(BTreeMap::new());
            };
            let info = crate::output_group_info::freeze_validation_top_level(validations)?;
            return Ok(BTreeMap::from([(
                crate::output_group_info::PROVIDER_ID.to_owned(),
                info,
            )]));
        }
        // A target that does not satisfy the aspect's required providers
        // (an aspect kept only because another one requires it) gets
        // ConfiguredAspect.NonApplicableAspect: no implementation, no
        // dependencies, no providers (AspectFunction.java:332-339).
        if !aspect.applies_to_rule(&base_rule) {
            return Ok(BTreeMap::new());
        }
        // The application's dependencies: the aspect's own attributes, each
        // in its attribute's configuration (phase 1 analyzed them there,
        // Self::aspect_attribute_configurations), and the base rule's edges
        // (aspect-applied where they propagate). The aspect's attributes are
        // a prerequisite collection of their own (AspectContext.
        // getMainAspectPrerequisitesCollection), never the base rule's.
        let attribute_configs = self
            .aspect_attribute_configs
            .get(&(base_key.clone(), aspect.identity.clone()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "internal analysis order error: aspect {} on {} was not analyzed",
                    aspect.identity, base_key.label
                )
            })?;
        let path = collection.path_aspects(&aspect.descriptor);
        let (aspect_dependencies, aspect_split_dependencies) = self
            .plan_aspect_attribute_dependencies(
                state,
                configured,
                aspect,
                None,
                &attribute_configs,
                &path,
            )?;
        // The base aspects' attributes `ctx.rule.attr` holds, planned as
        // phase 1 analyzed them (Self::analyze_aspect_attribute_edges).
        let mut base_aspect_attributes = Vec::new();
        for (owner, names) in crate::aspects::base_aspect_attributes(&path, &base_rule) {
            let memo = (base_key.clone(), aspect_attribute_memo_name(&owner, aspect));
            let owner_configs = self
                .aspect_attribute_configs
                .get(&memo)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "internal analysis order error: the attributes of aspect {} under {} on {} were not analyzed",
                        owner.identity, aspect.identity, base_key.label
                    )
                })?;
            let (dependencies, split_dependencies) = self.plan_aspect_attribute_dependencies(
                state,
                configured,
                &owner,
                Some(&names),
                &owner_configs,
                &path,
            )?;
            base_aspect_attributes.push(analysis::BaseAspectAttributes {
                aspect: owner,
                names,
                dependencies,
                split_dependencies,
            });
        }
        let mut dependencies: BTreeMap<String, DependencyTarget> = BTreeMap::new();
        let incoming: Vec<std::sync::Arc<crate::aspects::AspectDefinition>> = collection
            .used
            .iter()
            .map(|(used, _)| used.clone())
            .collect();
        for edge in self.rule_edges(&base_rule) {
            let attribute_aspects = base_rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute))
                .map(|spec| spec.declaration.aspects.clone())
                .unwrap_or_default();
            let attribute_aspects = self.loaded_attribute_aspects(&base_rule, &attribute_aspects);
            let propagating =
                crate::aspects::propagating_aspects(&attribute_aspects, &incoming, &edge.attribute);
            let edge_configs = self.analyzed_edge_configurations(base_key, &edge.cfg)?;
            // Aspects never apply in the no-config configuration
            // (AspectFunction.java:296-305: NonApplicableAspect).
            let propagating = if edge.cfg == EdgeCfg::NoConfig {
                Vec::new()
            } else {
                propagating
            };
            let dependency = self.resolve_rule_src(&base_rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                let (view, _) = self.plan_aspect_dependency(
                    state,
                    configured,
                    &dependency,
                    edge_config,
                    propagating.clone(),
                )?;
                let spec = base_rule
                    .custom
                    .as_ref()
                    .and_then(|custom| custom.schema.get(&edge.attribute));
                let map_key = match &edge.cfg {
                    EdgeCfg::Exec(_) => analysis::dependency_key(&dependency.to_string(), true),
                    EdgeCfg::Starlark(attribute) => {
                        if base_rule
                            .custom
                            .as_ref()
                            .is_some_and(|custom| custom.split_dependencies.contains_key(attribute))
                        {
                            analysis::split_dependency_key(
                                &dependency.to_string(),
                                attribute,
                                edge_config,
                            )
                        } else {
                            analysis::transition_dependency_key(&dependency.to_string(), attribute)
                        }
                    }
                    EdgeCfg::NoConfig => analysis::metadata_dependency_key(&dependency.to_string()),
                    _ => match spec {
                        Some(spec) if analysis::implicit_attribute(&edge.attribute, spec) => {
                            analysis::attribute_dependency_key(
                                &dependency.to_string(),
                                &edge.attribute,
                                spec,
                            )
                        }
                        _ => analysis::dependency_key(&dependency.to_string(), false),
                    },
                };
                dependencies.insert(map_key, view);
            }
        }
        // Run the implementation.
        let output_root = self.configurations.output_root(base_key.config);
        let action_env = Arc::new(
            self.configurations
                .get(base_key.config)
                .native
                .action_environment(),
        );
        let action_owner = key.action_owner(top);
        let loaded_functions = self.loaded_functions.clone();
        let outputs = analysis::ConfiguredOutputs {
            output_root: &output_root,
            action_owner: &action_owner,
            action_env,
            runfiles: self.runfiles_options,
            loaded_functions: Some(&loaded_functions),
        };
        // The aspect's RuleContext is built in the aspect's configuration,
        // the base configured target's (AspectFunction.java:296-310,
        // ConfiguredTargetFactory.createAspect): `ctx.bin_dir`, `ctx.var`,
        // `ctx.configuration`, `ctx.fragments` and the target platform are
        // the base target's configuration's, never a default one.
        // The aspect's own ToolchainCollection (resolved by phase 1,
        // Self::aspect_toolchain_contexts): ResolvedToolchainContext.load of
        // each context, described `aspect A applied to KIND rule L`
        // (AspectFunction.java:468-485), behind ctx.toolchains,
        // ctx.exec_groups and the action factory's exec groups; the
        // execution table over NO exec_properties (an aspect's RuleContext
        // takes none, ConfiguredTargetFactory.createAspect:702).
        let toolchains = self
            .aspect_toolchains
            .get(&(base_key.config, aspect.identity.clone()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "internal analysis order error: the toolchains of aspect {} in the configuration of {} were not resolved",
                    aspect.identity, base_key.label
                )
            })?;
        let target_description = format!(
            "aspect {} applied to {} rule {}",
            aspect.identity,
            base_rule.rule_class_name(),
            crate::rule_declarations::canonical_form(&base_rule.label)
        );
        let default_toolchains = self.load_toolchain_context(
            &base_rule,
            base_key.config,
            toolchains.default.as_ref(),
            &target_description,
            state.custom_providers,
        )?;
        let mut group_toolchains = Vec::new();
        for group in &toolchains.groups.groups {
            group_toolchains.push((
                group.name.clone(),
                crate::exec_groups::GroupToolchains::Resolved(Arc::new(
                    self.load_toolchain_context(
                        &base_rule,
                        base_key.config,
                        group.context.as_ref(),
                        &target_description,
                        state.custom_providers,
                    )?,
                )),
            ));
        }
        // RuleContext.fromToolchains (RuleContext.java:914-924): the
        // TemplateVariableInfo (`ctx.var`) of the aspect's resolved
        // toolchains, default context first.
        let mut template_variables = Vec::new();
        for context in std::iter::once(toolchains.default.as_ref())
            .chain(
                toolchains
                    .groups
                    .groups
                    .iter()
                    .map(|group| group.context.as_ref()),
            )
            .flatten()
        {
            for (_, implementation) in &context.resolved {
                if let Some(variables) = state
                    .custom_providers
                    .get(&ConfiguredTargetKey::new(
                        implementation.clone(),
                        base_key.config,
                    ))
                    .and_then(|providers| providers.get(crate::template_variable_info::PROVIDER_ID))
                    .and_then(|provider| crate::template_variable_info::variables(provider.value()))
                {
                    template_variables.push(variables);
                }
            }
        }
        // ctx.toolchains (StarlarkRuleContext.java:877-898): under automatic
        // exec groups the requested types of the automatic groups, each
        // resolved in its own group.
        let use_auto = toolchains.groups.use_auto_exec_groups;
        let ctx_toolchains = if use_auto {
            let mut merged = crate::toolchain_info::ToolchainContextData {
                target_description: target_description.clone(),
                requested: Vec::new(),
                toolchains: BTreeMap::new(),
                repositories: self.repositories.clone(),
            };
            for (name, toolchains) in &group_toolchains {
                if let crate::exec_groups::GroupToolchains::Resolved(data) = toolchains
                    && crate::exec_groups::is_automatic(name)
                {
                    for entry in &data.requested {
                        if !merged.requested.contains(entry) {
                            merged.requested.push(entry.clone());
                        }
                    }
                    for (type_label, info) in &data.toolchains {
                        merged
                            .toolchains
                            .entry(type_label.clone())
                            .or_insert_with(|| info.clone());
                    }
                }
            }
            merged
        } else {
            default_toolchains
        };
        let mut action_groups = vec![(
            crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            Vec::new(),
        )];
        action_groups.extend(
            toolchains
                .groups
                .groups
                .iter()
                .map(|group| (group.name.clone(), group.types.clone())),
        );
        let action_execution = self
            .execution_table(
                base_key.config,
                &[],
                toolchains.default.as_ref(),
                Some(&toolchains.groups),
                &self.display_form(&base_rule.label),
            )
            .map_err(|error| located(base_rule.location.as_deref(), error))?;
        let aspect_toolchains = analysis::AspectToolchains {
            toolchains: Arc::new(ctx_toolchains),
            exec_groups: Arc::new(crate::exec_groups::ExecGroupsData {
                target_description,
                groups: group_toolchains,
            }),
            action_exec_groups: crate::exec_groups::ActionExecGroups {
                groups: Some(action_groups),
                use_auto_exec_groups: use_auto,
                subrule: None,
                executions: Arc::from(action_execution.as_slice()),
            },
        };
        let mut analyzed_rule = base_rule.clone();
        if analyzed_rule.custom.is_some() {
            let platform = match self.platforms.get(&ConfiguredTargetKey::new(
                self.configurations
                    .get(base_key.config)
                    .native
                    .target_platform()
                    .clone(),
                base_key.config,
            )) {
                Some(Ok(platform)) => crate::platforms::TargetPlatform::Resolved(platform.clone()),
                _ => crate::platforms::TargetPlatform::Deferred,
            };
            let surface = Arc::new(self.ctx_surface(
                &base_rule,
                base_key.config,
                &output_root,
                platform.clone(),
                template_variables,
            ));
            let cpp = crate::custom_rule::CppConfigurationView::of(
                &self.configurations.get(base_key.config).native,
            );
            if let Some(custom) = analyzed_rule.custom.as_mut() {
                custom.cpp = cpp;
                custom.target_platform = platform;
                custom.surface = surface;
            }
        }
        let application = analysis::AspectApplication {
            aspect_split_dependencies,
            aspect: aspect.clone(),
            aspect_ids: collection.path_descriptions(&aspect.descriptor),
            aspect_path: collection.path_descriptions(&aspect.descriptor),
            target: merged_target,
            rule: analyzed_rule,
            dependencies: dependencies.clone(),
            aspect_dependencies,
            base_aspect_attributes,
            toolchains: aspect_toolchains,
            repositories: self.repositories.clone(),
        };
        let (analyzed, printed) = analysis::analyze_aspect_buffered(&application, &outputs);
        for line in printed {
            crate::ui::note(&line);
        }
        let analyzed = analyzed?;
        if self.cquery_capture.is_some() {
            cquery::register_outputs(
                analyzed.actions.iter().flat_map(|action| &action.outputs),
                base_key.config,
                state.artifact_index,
                state.declared,
                |_| false,
            );
            return Ok(analyzed.providers);
        }
        // Register the application's declared outputs and actions under the
        // application's own owner, so they never collide with the base
        // target's or another aspect's (AspectKey as ActionLookupKey).
        let PlanState {
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            workspace_status,
            input_groups,
            listed_tree_actions,
            ..
        } = state;
        let main_ignored = self.ignored("")?;
        // N63: plan the workspace status system nodes on first need.
        if Self::workspace_status_needed(&analyzed.actions) {
            self.ensure_workspace_status(
                workspace_status,
                nodes,
                producers,
                client_environment,
                status_client_environment,
                artifact_index,
            )?;
        }
        let shared = register_action_outputs(
            &analyzed.actions,
            (&action_owner, &base_key.label.to_string()),
            producers,
            artifact_index,
            declared,
            base_key.config,
            |_| false,
        )?;
        let mut commit = ActionCommit {
            artifact_index,
            workspace_status,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            input_groups,
            listed_tree_actions,
        };
        let owner = format!("aspect {}", aspect.identity);
        for (mut registered, shared) in analyzed.actions.into_iter().zip(shared) {
            // Another owner's equal action produces its outputs.
            if shared {
                continue;
            }
            if registered.repo_mapping.is_some() {
                return Err(format!(
                    "{owner}: py_internal.create_repo_mapping_manifest in an aspect {}",
                    crate::language_modules::NOT_IMPLEMENTED
                ));
            }
            // N63: resolve an actions.transform_{version,info}_file
            // registration into its template expansion.
            if registered.build_info.is_some() {
                self.build_info_actions.insert(registered.action.id.clone());
            }
            self.resolve_build_info(commit.workspace_status, &mut registered)?;
            let own = registered
                .inputs
                .iter()
                .map(|file| {
                    commit.artifact_index.get(file).cloned().ok_or_else(|| {
                        format!("{owner}: input has no declared artifact: {}", file.path)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut action = registered.action;
            // A spawn's propagated tags are its base rule's (an aspect's
            // RuleContext.getRule), set at registration (keyed,
            // Context::register_action); writes and transforms have none,
            // like a rule's. The base target's exec_properties are not an
            // aspect action's (ConfiguredTargetFactory.createAspect
            // setExecProperties(ImmutableMap.of()), analysis/
            // ConfiguredTargetFactory.java:702: genuine shares two
            // applications' equal spawns whatever their base targets'
            // exec_properties, tests/fixtures/shared_actions aspect_shared),
            // so the key has none; the remote gate stays conservative (they
            // keep the spawn off the remote cache).
            // The action's exec group (of the aspect's ToolchainCollection)
            // decides its execution platform and exec properties
            // (RuleContext.getActionOwner(execGroup)), like a rule's.
            let execution = action_execution
                .iter()
                .find(|execution| execution.exec_group == registered.exec_group);
            if let Some(execution) = execution {
                self.action_executions
                    .insert(action.id.clone(), execution.clone());
            }
            if action.write.is_none() && action.extra.transform.is_none() {
                self.set_remote_platform(
                    &mut action,
                    &base_rule,
                    base_key.config,
                    execution,
                    false,
                );
            }
            // Like a rule's action (PA-1 follow-up B6): its tools' runfiles
            // trees (shared input groups) and its deferred completion,
            // which the aspect commit used to drop.
            self.commit_action(
                action,
                own,
                &registered.runfiles_trees,
                registered.deferred,
                &owner,
                &main_ignored,
                &mut commit,
            )?;
        }
        Ok(analyzed.providers)
    }

    /// One dependency of an aspect application: the plain configured target
    /// view, or the aspect-applied view when `aspects` propagate to it.
    fn plan_aspect_dependency(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        label: &Label,
        edge_config: ConfigId,
        mut aspects: Vec<std::sync::Arc<crate::aspects::AspectDefinition>>,
    ) -> Result<(DependencyTarget, AspectApplications), String> {
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
            } = state;
            return Ok((
                self.plan_dependency_view(
                    configured,
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
                )?,
                Vec::new(),
            ));
        }
        // The target the aspects apply to: through aliases and to the
        // generating rule of an output file when the aspect asks for it
        // (apply_to_generating_rules).
        let requested = ConfiguredTargetKey::new(label.clone(), edge_config);
        let mut base_label = self
            .forwarded_actual(configured, &requested)
            .unwrap_or_else(|| label.clone());
        while let Some(actual) = configured
            .get(&ConfiguredTargetKey::new(base_label.clone(), edge_config))
            .and_then(|node| node.actual.clone())
        {
            base_label = actual;
        }
        let target = self.target(&base_label)?;
        let (_resolved_label, target) = match target {
            Target::Output(generating, _) => {
                // AspectFunction.java:327-341: an aspect applies to an
                // output-file target only through its generating rule, and
                // only when it asks for that (apply_to_generating_rules; the
                // native ValidateTarget applies to files).
                aspects.retain(|aspect| {
                    aspect.declaration.apply_to_generating_rules
                        || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
                });
                let applies = !aspects.is_empty();
                if !applies {
                    aspects.clear();
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
                    } = state;
                    return Ok((
                        self.plan_dependency_view(
                            configured,
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
                        )?,
                        Vec::new(),
                    ));
                }
                (generating.label.clone(), self.target(&generating.label)?)
            }
            _ => (base_label, target),
        };
        let Target::Rule(rule) = target else {
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
            } = state;
            return Ok((
                self.plan_dependency_view(
                    configured,
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
                )?,
                Vec::new(),
            ));
        };
        if matches!(rule.kind, RuleKind::PackageGroup(_)) {
            aspects.clear();
        }
        let collection = if aspects.is_empty() {
            crate::aspects::AspectCollection::default()
        } else {
            crate::aspects::aspect_collection(aspects.to_vec(), &rule)?
        };
        if collection.is_empty() {
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
            } = state;
            return Ok((
                self.plan_dependency_view(
                    configured,
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
                )?,
                Vec::new(),
            ));
        }
        self.plan_aspect_collection(state, configured, label, edge_config, &collection)
    }

    /// The pure compute of one custom rule (P1: no threads yet; the types
    /// enforce the boundary for P3's workers). Filesystem-free: it never
    /// touches the engine, planning state or the coordinator's maps.
    fn compute_target(snapshot: ComputeSnapshot) -> (ComputeSnapshot, ComputeOutcome) {
        let outputs = analysis::ConfiguredOutputs {
            output_root: &snapshot.output_root,
            action_owner: &snapshot.action_owner,
            action_env: snapshot.action_env.clone(),
            runfiles: snapshot.runfiles,
            loaded_functions: Some(&snapshot.loaded_functions),
        };
        let (analyzed, printed) = analysis::analyze_buffered(
            &snapshot.rule,
            &snapshot.dependency_targets,
            snapshot.repositories.clone(),
            &outputs,
        );
        (snapshot, ComputeOutcome { analyzed, printed })
    }

    /// Phase 1 of one plan entry (the P1 split of plan_target): compute
    /// the coordinator-side context and the owned snapshot of the target's
    /// analysis. Coordinator-only by construction (reads the engine and
    /// planning state, never runs an implementation).
    fn prepare_target(
        &mut self,
        state: &mut ActionPlanning,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        configured: &mut ConfiguredNodes,
    ) -> Result<PreparedTarget, String> {
        // Repository closures are inputs to execution-time manifest content,
        // not provider analysis. Cquery retains the real RepositorySet and
        // semantic support Files but never renders those manifests.
        let execution_plan = self.cquery_capture.is_none();
        let top = self.configurations.top();
        let main_ignored = self.ignored("")?;
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            native_output_groups,
            validation,
            repositories,
            target_identities,
            aspect_providers,
            aspect_target_identities,
            workspace_status,
            views,
            input_groups,
            listed_tree_actions,
            ..
        } = state;
        // The rule's dependency edges, built once per prepare (W1a).
        let edges = self.rule_edges(rule);
        let custom_rule = matches!(rule.kind, RuleKind::Custom);
        // The transitive repositories of this configured target: its own and
        // those of every (configured) dependency edge. A Starlark rule
        // computes them from its resolved dependencies and toolchains below
        // instead; this loop only reads planning state, so it is skipped
        // there (W1a).
        let mut transitive: BTreeSet<String> = if execution_plan {
            BTreeSet::from([rule.label.repository.clone()])
        } else {
            BTreeSet::new()
        };
        if !custom_rule && execution_plan {
            let edge_exec = self.exec_configurations.get(key).copied();
            for edge in &edges {
                let Ok(label) = self.resolve_rule_src(rule, &edge.label) else {
                    continue;
                };
                let edge_config = match &edge.cfg {
                    EdgeCfg::Target => Some(key.config),
                    EdgeCfg::Exec(None) => Some(edge_exec.unwrap_or(key.config)),
                    EdgeCfg::Exec(Some(group)) => Some(
                        self.group_exec_configurations
                            .get(&(key.clone(), group.clone()))
                            .copied()
                            .unwrap_or(key.config),
                    ),
                    EdgeCfg::NoConfig => None,
                    EdgeCfg::Starlark(_) => {
                        unreachable!("native rules have no Starlark attribute transitions")
                    }
                };
                transitive.extend(self.dependency_repositories(repositories, label, edge_config));
            }
        }
        let key = key.clone();
        let config = key.config;
        let output_root = self.configurations.output_root(config);
        // BuildConfigurationValue.getActionEnvironment of the target's
        // configuration (SHELL_ACTION_ENV; crate::action_env).
        let action_env = Arc::new(self.configurations.get(config).native.action_environment());
        let action_owner = key.action_owner(top);
        // The configuration of the target's exec-transition edges.
        let exec_config = self
            .exec_configurations
            .get(&key)
            .copied()
            .unwrap_or(config);
        let dependency_key = |label: Label| ConfiguredTargetKey::new(label, config);
        if matches!(rule.kind, RuleKind::PackageGroup(_)) {
            return Ok(PreparedTarget {
                key,
                config,
                output_root,
                action_env,
                action_owner,
                exec_config,
                main_ignored,
                transitive,
                kind: PreparedKind::PackageGroup,
            });
        }
        if custom_rule {
            let mut dependency_targets = BTreeMap::new();
            // Resolved dependencies only: `$config_dependencies` is never a
            // prerequisite (DependencyResolutionHelpers.java:408-416). The
            // resolved toolchain implementations are dependencies too
            // (their ToolchainInfo files are action inputs).
            // Each dependency with the configuration of its edge and its
            // key in the implementation's dependency map
            // (analysis::dependency_key).
            let mut dependency_labels: Vec<(Label, ConfigId, String, &str)> = Vec::new();
            // N58: attribute aspects of the rule's edges, by resolved label
            // (attr(aspects = [...]) propagation; an aspect list resolves to
            // its definitions once).
            let mut edge_aspects: BTreeMap<
                String,
                Vec<std::sync::Arc<crate::aspects::AspectDefinition>>,
            > = BTreeMap::new();
            for edge in &edges {
                let label = self.resolve_rule_src(rule, &edge.label)?;
                if let Some(spec) = rule
                    .custom
                    .as_ref()
                    .and_then(|custom| custom.schema.get(&edge.attribute))
                    && !spec.declaration.aspects.is_empty()
                {
                    let aspects = self.attribute_aspects(rule, &spec.declaration.aspects)?;
                    let entry = edge_aspects.entry(edge.attribute.clone()).or_default();
                    for aspect in aspects {
                        if !entry.iter().any(
                            |known: &std::sync::Arc<crate::aspects::AspectDefinition>| {
                                known.descriptor == aspect.descriptor
                            },
                        ) {
                            entry.push(aspect);
                        }
                    }
                }
                if rule
                    .custom
                    .as_ref()
                    .is_some_and(|custom| custom.split_dependencies.contains_key(&edge.attribute))
                {
                    continue;
                }
                let (edge_config, map_key) = match &edge.cfg {
                    EdgeCfg::Target => {
                        (config, analysis::dependency_key(&label.to_string(), false))
                    }
                    EdgeCfg::Exec(None) => (
                        exec_config,
                        analysis::dependency_key(&label.to_string(), true),
                    ),
                    // A named exec group's configuration (N35); the
                    // transition is a no-op without toolchain contexts.
                    EdgeCfg::Exec(Some(group)) => (
                        self.group_exec_configurations
                            .get(&(key.clone(), group.clone()))
                            .copied()
                            .unwrap_or(config),
                        analysis::dependency_key(&label.to_string(), true),
                    ),
                    EdgeCfg::NoConfig => (
                        self.configurations.intern_no_config(),
                        analysis::metadata_dependency_key(&label.to_string()),
                    ),
                    EdgeCfg::Starlark(attribute) => (
                        self.edge_configs
                            .get(&key)
                            .and_then(|configs| configs.get(attribute))
                            .filter(|branches| branches.len() == 1 && branches[0].0.is_empty())
                            .map(|branches| branches[0].1)
                            .ok_or_else(|| {
                                format!(
                                    "internal error: no transitioned configuration for {} attribute {attribute}",
                                    rule.label
                                )
                            })?,
                        analysis::transition_dependency_key(&label.to_string(), attribute),
                    ),
                };
                dependency_labels.push((label, edge_config, map_key, &edge.attribute));
            }
            if let Some(custom) = &rule.custom {
                for (name, split) in custom.split_dependencies.iter() {
                    dependency_labels.extend(
                        split
                            .prerequisites
                            .iter()
                            .map(|p| (p.label.clone(), p.config, p.map_key.clone(), name.as_str())),
                    );
                }
            }
            let mut add_target = |label: Label| {
                let map_key = label.to_string();
                if !dependency_labels
                    .iter()
                    .any(|(_, _, seen, _)| *seen == map_key)
                {
                    dependency_labels.push((label, config, map_key, ""));
                }
            };
            let target_exec_groups = self.exec_group_contexts.get(&key).cloned();
            for context in
                std::iter::once(self.toolchain_contexts.get(&key))
                    .chain(target_exec_groups.iter().flat_map(|groups| {
                        groups.groups.iter().map(|group| group.context.as_ref())
                    }))
                    .flatten()
            {
                for implementation in context.resolved_labels() {
                    add_target(implementation);
                }
            }
            // The target's exec_compatible_with constraint values
            // (ctx.attr.exec_compatible_with), analyzed by its toolchain
            // resolution, and its configured target_compatible_with
            // prerequisites (ctx.attr.target_compatible_with), analyzed by
            // its compatibility check.
            if let Some(custom) = &rule.custom {
                for name in ["exec_compatible_with", "target_compatible_with"] {
                    for text in custom.declaration.builtin.target.labels(name) {
                        add_target(self.resolve_rule_src(rule, &text)?);
                    }
                }
            }
            let mut transitive: BTreeSet<String> = BTreeSet::new();
            if execution_plan {
                transitive.insert(rule.label.repository.clone());
                for (label, edge_config, _, _) in &dependency_labels {
                    transitive.extend(self.dependency_repositories(
                        repositories,
                        label.clone(),
                        Some(*edge_config),
                    ));
                }
            }
            for (label, edge_config, map_key, attribute) in dependency_labels {
                let aspects = edge_aspects.get(attribute).cloned().unwrap_or_default();
                let view = if aspects.is_empty() {
                    self.plan_dependency_view(
                        configured,
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
                        &label,
                        edge_config,
                    )?
                } else {
                    // MergedConfiguredTarget.of's MergingException is
                    // reported at the consumer's location
                    // (MergedConfiguredTarget.java:210-213).
                    let location = rule.location.clone();
                    let mut planning = PlanState {
                        dry: &mut *dry,
                        custom_providers: &mut *custom_providers,
                        provider_keys: &mut *provider_keys,
                        runfiles_info: &mut *runfiles_info,
                        artifact_index: &mut *artifact_index,
                        declared: &mut *declared,
                        native_output_groups: &mut *native_output_groups,
                        validation: &mut *validation,
                        target_identities: &mut *target_identities,
                        aspect_providers: &mut *aspect_providers,
                        aspect_target_identities: &mut *aspect_target_identities,
                        nodes: &mut *nodes,
                        producers: &mut *producers,
                        source_inputs: &mut *source_inputs,
                        client_environment: &mut *client_environment,
                        status_client_environment: &mut *status_client_environment,
                        workspace_status: &mut *workspace_status,
                        views: &mut *views,
                        input_groups: &mut *input_groups,
                        listed_tree_actions: &mut *listed_tree_actions,
                    };
                    self.plan_aspect_dependency(
                        &mut planning,
                        configured,
                        &label,
                        edge_config,
                        aspects,
                    )
                    .map_err(|error| located(location.as_deref(), error))?
                    .0
                };
                dependency_targets.insert(map_key, view);
            }
            // Prerequisite validation runs here, before the snapshot (and
            // so before the compute, which may run on another thread in
            // P3): a speculative implementation never fetches in a build
            // that sequentially stopped at this error (review follow-up).
            self.starlark_prerequisite_errors(
                rule,
                &key,
                exec_config,
                configured,
                &dependency_targets,
            )?;
            // ctx.target_platform_has_constraint: a toolchain-resolving
            // rule sees its target platform; a deferred default host
            // platform is resolved only when the implementation asks,
            // then the (side-effect free) analysis runs again.
            let mut analyzed_rule = rule.clone();
            let mut action_execution = Vec::new();
            let uses_toolchains =
                uses_toolchain_resolution(rule) && !self.empty_configuration_rules.contains(&key);
            let target = dependency_key(
                self.configurations
                    .get(config)
                    .native
                    .target_platform()
                    .clone(),
            );
            if let Some(custom) = analyzed_rule.custom.as_mut() {
                // The configuration's C++ options (subrule ctx.fragments.cpp).
                // The target's own configuration: an exec configuration
                // carries the exec transition's compilation mode
                // (--host_compilation_mode) and strip=always
                // (crate::config_registry::exec_transition).
                custom.cpp = crate::custom_rule::CppConfigurationView::of(
                    &self.configurations.get(config).native,
                );
                custom.target_platform = match self.platforms.get(&target) {
                    _ if !uses_toolchains => crate::platforms::TargetPlatform::Absent,
                    Some(Ok(platform)) => {
                        crate::platforms::TargetPlatform::Resolved(platform.clone())
                    }
                    _ => crate::platforms::TargetPlatform::Deferred,
                };
                // ResolvedToolchainContext.load (analysis/ResolvedToolchainContext.java:48-110)
                // with ConfiguredTargetFunction.java:348-366: the
                // ToolchainInfo of each resolved implementation (through
                // an alias), else TargetNotToolchainException.
                // RuleContext.fromToolchains (analysis/RuleContext.java:914-924):
                // the TemplateVariableInfo (`ctx.var`) of the resolved
                // toolchains of every ToolchainCollection context
                // (ResolvedToolchainContext.load, :56-85), default first.
                let mut template_variables = Vec::new();
                if uses_toolchains {
                    let exec_groups = target_exec_groups.clone().unwrap_or_default();
                    let target_description = format!(
                        "{} rule {}",
                        rule.rule_class_name(),
                        crate::rule_declarations::canonical_form(&rule.label)
                    );
                    for context in std::iter::once(self.toolchain_contexts.get(&key))
                        .chain(
                            exec_groups
                                .groups
                                .iter()
                                .map(|group| group.context.as_ref()),
                        )
                        .flatten()
                    {
                        for (_, implementation) in &context.resolved {
                            if let Some(variables) = custom_providers
                                .get(&dependency_key(implementation.clone()))
                                .and_then(|providers| {
                                    providers.get(crate::template_variable_info::PROVIDER_ID)
                                })
                                .and_then(|provider| {
                                    crate::template_variable_info::variables(provider.value())
                                })
                            {
                                template_variables.push(variables);
                            }
                        }
                    }
                    let default = self.load_toolchain_context(
                        rule,
                        config,
                        self.toolchain_contexts.get(&key),
                        &target_description,
                        custom_providers,
                    )?;
                    let mut groups = Vec::new();
                    for group in &exec_groups.groups {
                        let toolchains = if group.builtin_only {
                            crate::exec_groups::GroupToolchains::BuiltinOnly
                        } else {
                            crate::exec_groups::GroupToolchains::Resolved(Arc::new(
                                self.load_toolchain_context(
                                    rule,
                                    config,
                                    group.context.as_ref(),
                                    &target_description,
                                    custom_providers,
                                )?,
                            ))
                        };
                        groups.push((group.name.clone(), toolchains));
                    }
                    // ctx.toolchains (StarlarkRuleContext.java:877-898):
                    // under automatic exec groups the requested types of the
                    // automatic groups, each resolved in its own group
                    // (RuleContext.getToolchainInfo, :1132-1150).
                    custom.toolchains = Some(Arc::new(if exec_groups.use_auto_exec_groups {
                        let mut merged = crate::toolchain_info::ToolchainContextData {
                            target_description: target_description.clone(),
                            requested: Vec::new(),
                            toolchains: BTreeMap::new(),
                            repositories: self.repositories.clone(),
                        };
                        for (name, toolchains) in &groups {
                            if let crate::exec_groups::GroupToolchains::Resolved(data) = toolchains
                                && crate::exec_groups::is_automatic(name)
                            {
                                for entry in &data.requested {
                                    if !merged.requested.contains(entry) {
                                        merged.requested.push(entry.clone());
                                    }
                                }
                                for (type_label, info) in &data.toolchains {
                                    merged
                                        .toolchains
                                        .entry(type_label.clone())
                                        .or_insert_with(|| info.clone());
                                }
                            }
                        }
                        merged
                    } else {
                        default
                    }));
                    custom.exec_groups = Some(Arc::new(crate::exec_groups::ExecGroupsData {
                        target_description,
                        groups,
                    }));
                    custom.use_auto_exec_groups = exec_groups.use_auto_exec_groups;
                    let mut action_groups = vec![(
                        crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
                        Vec::new(),
                    )];
                    action_groups.extend(
                        exec_groups
                            .groups
                            .iter()
                            .map(|group| (group.name.clone(), group.types.clone())),
                    );
                    custom.action_exec_groups = crate::exec_groups::ActionExecGroups {
                        groups: Some(action_groups),
                        use_auto_exec_groups: exec_groups.use_auto_exec_groups,
                        subrule: None,
                        executions: Arc::from([]),
                    };
                } else {
                    // No ToolchainCollection (RuleContext.getToolchainContexts
                    // is null); RuleContext.useAutoExecGroups still applies to
                    // the actions' toolchain parameter.
                    let use_auto = self.use_auto_exec_groups(rule, config)?;
                    custom.toolchains = None;
                    custom.exec_groups = None;
                    custom.use_auto_exec_groups = use_auto;
                    custom.action_exec_groups = crate::exec_groups::ActionExecGroups {
                        groups: None,
                        use_auto_exec_groups: use_auto,
                        subrule: None,
                        executions: Arc::from([]),
                    };
                }
                action_execution = self.action_execution_table(
                    rule,
                    config,
                    target_exec_groups.as_ref().filter(|_| uses_toolchains),
                )?;
                // Each action's key takes its exec group's execution
                // (crate::shared_actions).
                custom.action_exec_groups.executions = Arc::from(action_execution.as_slice());
                // The configured target's own key and configuration (an
                // exec, no-config or Starlark-transitioned configuration
                // included), never a build-global one.
                custom.surface = Arc::new(self.ctx_surface(
                    rule,
                    config,
                    &output_root,
                    match self.platforms.get(&target) {
                        Some(Ok(platform)) => {
                            crate::platforms::TargetPlatform::Resolved(platform.clone())
                        }
                        _ => crate::platforms::TargetPlatform::Deferred,
                    },
                    template_variables,
                ));
            }
            // print() output of a discarded attempt is dropped: Bazel
            // runs the implementation once, after its target platform is
            // known.
            let loaded_functions = self.loaded_functions.clone();
            // The compute boundary (review §1.7): everything below runs
            // from this owned snapshot, coordinator-free.
            let snapshot = ComputeSnapshot {
                rule: analyzed_rule,
                dependency_targets,
                output_root: output_root.clone(),
                action_owner: action_owner.clone(),
                action_env: action_env.clone(),
                runfiles: self.runfiles_options,
                loaded_functions,
                repositories: self.repositories.clone(),
            };
            return Ok(PreparedTarget {
                key,
                config,
                output_root,
                action_env,
                action_owner,
                exec_config,
                main_ignored,
                transitive,
                kind: PreparedKind::Custom(Box::new(CustomPrepared {
                    snapshot,
                    action_execution,
                })),
            });
        }
        Ok(PreparedTarget {
            key,
            config,
            output_root,
            action_env,
            action_owner,
            exec_config,
            main_ignored,
            transitive,
            kind: PreparedKind::Native,
        })
    }

    /// Phase 3 of one plan entry (the P1 split of plan_target): apply the
    /// prepared target and its compute outcome to the planning state, in
    /// the same order the sequential engine did (byte-identical output).
    /// Coordinator-only: every state mutation and every stdout/stderr
    /// write of a planned target happens here or in the phases below.
    fn commit_target(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        rule: &Arc<Rule>,
        prepared: PreparedTarget,
        computed: Option<ComputeOutcome>,
    ) -> Result<(), String> {
        let PreparedTarget {
            key,
            config,
            output_root,
            action_env,
            action_owner,
            exec_config,
            main_ignored,
            transitive,
            kind,
        } = prepared;
        // The transitive repositories of the configured target (Starlark
        // rules' refined closure; both sequential inserts collapse to
        // this final value, and only later targets read it).
        if self.cquery_capture.is_none() {
            state
                .repositories
                .insert(key.clone(), Arc::new(transitive.clone()));
        }
        match kind {
            PreparedKind::PackageGroup => {
                let ActionPlanning {
                    dry,
                    custom_providers,
                    ..
                } = state;
                let node = self.resolve_group(&rule.label, &mut Vec::new())?;
                let info = starlark::environment::Module::with_temp_heap(|module| {
                    let value = package_specification::alloc_info(&module.heap(), node);
                    module.set("__info", value);
                    let frozen = module.freeze().map_err(|error| {
                        format!("cannot freeze package group provider: {error:?}")
                    })?;
                    frozen.get("__info").map_err(|error| error.to_string())
                })?;
                custom_providers.insert(
                    key.clone(),
                    BTreeMap::from([(package_specification::PROVIDER_ID.to_owned(), info)]),
                );
                dry.insert(key, Arc::default());
                return Ok(());
            }
            PreparedKind::Custom(custom) => {
                let custom = *custom;
                self.commit_custom_target(
                    state,
                    configured,
                    key,
                    config,
                    output_root,
                    action_owner,
                    main_ignored,
                    transitive,
                    rule,
                    custom.snapshot,
                    custom.action_execution,
                    computed.expect("custom target computed"),
                )?;
                return Ok(());
            }
            PreparedKind::Native => {}
        }
        self.commit_native_target(
            state,
            configured,
            rule,
            key,
            config,
            output_root,
            action_env,
            action_owner,
            exec_config,
            main_ignored,
        )
    }

    /// The commit of one custom rule (the tail of the split plan_target):
    /// prerequisite validation, the NEEDS_TARGET_PLATFORM retry (inline at
    /// the commit slot, review §1.7), print emission (review §1.8), and
    /// every planning-state insert of the analyzed target.
    #[allow(clippy::too_many_arguments)]
    fn commit_custom_target(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: ConfiguredTargetKey,
        config: ConfigId,
        output_root: String,
        action_owner: String,
        main_ignored: Arc<IgnoredSubdirectories>,
        transitive: BTreeSet<String>,
        rule: &Rule,
        snapshot: ComputeSnapshot,
        action_execution: Vec<crate::exec_groups::ActionExecution>,
        computed: ComputeOutcome,
    ) -> Result<(), String> {
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            client_environment,
            status_client_environment,
            runfiles_nodes,
            workspace_status,
            input_groups,
            listed_tree_actions,
            ..
        } = state;
        let mut snapshot = snapshot;
        let mut outcome = computed;
        if outcome
            .analyzed
            .as_ref()
            .is_err_and(|error| error.contains(crate::platforms::NEEDS_TARGET_PLATFORM))
        {
            // The target's own configuration's target platform.
            let platform = self.target_platform(config, configured, &mut Vec::new())?;
            if let Some(custom) = snapshot.rule.custom.as_mut() {
                // A rule without toolchain resolution keeps its absent
                // platform (ctx.target_platform_has_constraint); the
                // configuration's platform serves `$(TARGET_CPU)`.
                if matches!(
                    custom.target_platform,
                    crate::platforms::TargetPlatform::Deferred
                ) {
                    custom.target_platform =
                        crate::platforms::TargetPlatform::Resolved(platform.clone());
                }
                let mut surface = (*custom.surface).clone();
                surface.target_platform = crate::platforms::TargetPlatform::Resolved(platform);
                custom.surface = Arc::new(surface);
            }
            let timer = crate::plan_timings::StageTimer::start(
                crate::plan_timings::PlanStage::CommitRetry,
                u64::MAX,
                0,
            );
            let retry = Engine::compute_target(snapshot);
            timer.finish();
            snapshot = retry.0;
            outcome = retry.1;
        }
        let outcome = if outcome
            .analyzed
            .as_ref()
            .is_err_and(|error| crate::repository_demand::is_needs_repository(error))
        {
            // The no-demand guard aborted the compute (review §1.2):
            // re-run it inline at this commit slot, where repository
            // demand may fetch. The aborted attempt's buffered lines are
            // dropped with it; the re-run is the sequential evaluation
            // and reproduces its output exactly.
            let timer = crate::plan_timings::StageTimer::start(
                crate::plan_timings::PlanStage::CommitRetry,
                u64::MAX,
                0,
            );
            let retried = Engine::compute_target(snapshot).1;
            timer.finish();
            retried
        } else {
            drop(snapshot);
            outcome
        };
        let ComputeOutcome { analyzed, printed } = outcome;
        for line in printed {
            crate::ui::note(&line);
        }
        let mut analyzed = analyzed?;
        restricted_to_error(rule)?;
        self.check_transition_allowlist(rule, &key, configured)?;
        // Moved, not copied: the analysis result is consumed here (MEM2).
        custom_providers.insert(key.clone(), std::mem::take(&mut analyzed.providers));
        provider_keys.insert(key.clone(), std::mem::take(&mut analyzed.provider_keys));
        for file in &analyzed.declared {
            let package_output = PathBuf::from(&output_root).join(rule.label.package_exec_path());
            let short_name = Path::new(&file.path)
                .strip_prefix(&package_output)
                .map_err(|_| format!("output outside declaring package: {}", file.path))?;
            rule.label.with_name(&short_name.to_string_lossy())?;
            let ignored = self.ignored(&rule.label.repository)?;
            self.validate_output_directory(
                &rule.label.package_id(),
                &short_name.to_string_lossy(),
                &ignored,
            )?;
        }
        // Semantic commit is distinct from execution-plan lowering. Cquery
        // needs providers, file identities and runfiles, but never action keys,
        // status commands, input digests, runfiles manifests or scheduler nodes.
        if self.cquery_capture.is_some() {
            let package_output = PathBuf::from(&output_root).join(rule.label.package_exec_path());
            cquery::register_outputs(
                analyzed.actions.iter().flat_map(|action| &action.outputs),
                config,
                artifact_index,
                declared,
                |file| {
                    Path::new(&file.path)
                        .strip_prefix(&package_output)
                        .is_ok_and(|name| rule.outs.iter().any(|out| Path::new(out) == name))
                },
            );
            // FilesToRunProvider exposes these semantic Files even though no
            // manifest/tree/mapping action is lowered or executed by cquery.
            // Their actual Option values already honor the runfiles options.
            if let Some(support) = &analyzed.runfiles.files_to_run {
                cquery::register_outputs(
                    support
                        .runfiles_manifest()
                        .into_iter()
                        .chain(support.repo_mapping_manifest()),
                    config,
                    artifact_index,
                    declared,
                    |_| false,
                );
            }
            for file in &analyzed.files {
                if crate::analysis_runtime::is_workspace_status_file(file) {
                    self.indexed_artifact(
                        file,
                        artifact_index,
                        workspace_status,
                        nodes,
                        producers,
                        client_environment,
                        status_client_environment,
                    )?;
                }
            }
            let defaults = analyzed
                .files
                .iter()
                .map(|file| {
                    artifact_index.get(file).cloned().ok_or_else(|| {
                        format!(
                            "{}: DefaultInfo file has no artifact: {}",
                            rule.label, file.path
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            dry.insert(key.clone(), Arc::new(defaults));
            runfiles_info.insert(key, Arc::new(analyzed.runfiles));
            return Ok(());
        }
        // N63: plan the workspace status system nodes before indexing this
        // target's outputs, so actions consuming bazel-out/{stable,volatile}
        // -status.txt (or a transform_{version,info}_file registration)
        // resolve their artifacts, and so does a status file among the
        // target's files to build (W1b: whichever target registered the
        // status files first, PLAN-L1 §2.1).
        if Self::workspace_status_needed(&analyzed.actions)
            || analyzed
                .files
                .iter()
                .any(crate::analysis_runtime::is_workspace_status_file)
        {
            self.ensure_workspace_status(
                workspace_status,
                nodes,
                producers,
                client_environment,
                status_client_environment,
                artifact_index,
            )?;
        }
        // First index EVERY action's outputs, so registration order is not
        // an execution order and forward generated dependencies are legal.
        let label = key.label.to_string();
        let package_output = PathBuf::from(&output_root).join(rule.label.package_exec_path());
        let shared = register_action_outputs(
            &analyzed.actions,
            (&label, &label),
            producers,
            artifact_index,
            declared,
            config,
            // A predeclared output is the rule's output-file target.
            |exec| {
                exec.strip_prefix(&package_output)
                    .is_ok_and(|name| rule.outs.iter().any(|out| Path::new(out) == name))
            },
        )?;
        let runfiles = Arc::new(std::mem::take(&mut analyzed.runfiles));
        runfiles_info.insert(key.clone(), runfiles.clone());
        if let Some(nodes_of_target) = self.plan_runfiles_support(
            &key,
            rule,
            &runfiles,
            &transitive,
            &action_owner,
            &main_ignored,
            RunfilesPlanning {
                artifact_index,
                declared,
                producers,
                nodes,
                source_inputs,
                workspace_status,
                client_environment,
                status_client_environment,
            },
        )? {
            runfiles_nodes.insert(key.clone(), nodes_of_target);
        }
        let mut commit = ActionCommit {
            artifact_index,
            workspace_status,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            input_groups,
            listed_tree_actions,
        };
        let owner = rule.label.to_string();
        for (mut registered, shared) in analyzed.actions.into_iter().zip(shared) {
            // Another owner's equal action produces its outputs: no node,
            // no source inputs, no execution of its own.
            if shared {
                continue;
            }
            // py_internal.create_repo_mapping_manifest: the
            // RepoMappingManifestAction's content over the target's
            // transitive repositories (like its runfiles support's).
            self.resolve_repo_mapping(&transitive, &mut registered)?;
            // N63: resolve an actions.transform_{version,info}_file
            // registration into its template expansion.
            if registered.build_info.is_some() {
                self.build_info_actions.insert(registered.action.id.clone());
            }
            self.resolve_build_info(commit.workspace_status, &mut registered)?;
            let own = registered
                .inputs
                .iter()
                .map(|file| {
                    commit.artifact_index.get(file).cloned().ok_or_else(|| {
                        format!(
                            "{}: input has no declared artifact: {}",
                            rule.label, file.path
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            // The action's exec group decides its execution platform and
            // exec properties (RuleContext.getActionOwner(execGroup)).
            let execution = action_execution
                .iter()
                .find(|execution| execution.exec_group == registered.exec_group);
            if let Some(execution) = execution {
                self.action_executions
                    .insert(registered.action.id.clone(), execution.clone());
            }
            let mut action = registered.action;
            // The spawn's propagated tags are set at registration
            // (crate::analysis_runtime::Context::register_action, keyed).
            if action.write.is_none() && action.extra.transform.is_none() {
                self.set_remote_platform(&mut action, rule, config, execution, true);
            }
            self.commit_action(
                action,
                own,
                &registered.runfiles_trees,
                registered.deferred,
                &owner,
                &main_ignored,
                &mut commit,
            )?;
        }
        let defaults = analyzed
            .files
            .iter()
            .map(|file| {
                artifact_index.get(file).cloned().ok_or_else(|| {
                    format!(
                        "{}: DefaultInfo file has no artifact: {}",
                        rule.label, file.path
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        dry.insert(key.clone(), Arc::new(defaults));
        Ok(())
    }

    /// Plan the node of one registered Starlark action — a rule's
    /// (commit_custom_target) or an aspect application's
    /// (plan_aspect_application) alike: `action` with its own inputs `own`
    /// (the registration's, resolved), its tools' runfiles trees and its
    /// execution-time completion `deferred`. `owner` names the registering
    /// target or aspect in the error of a tree entry without an artifact.
    ///
    /// N23f: the tools' runfiles trees are staged at their tree paths
    /// (SpawnInputExpander.addSingleRunfilesTreeToInputs,
    /// exec/SpawnInputExpander.java:87-135); empty files are empty
    /// parameter-file-like inputs (VirtualActionInput.EMPTY_MARKER). PA-1:
    /// each tree is expanded once per build into a shared input group that
    /// every action using the tool holds, like the one runfiles tree
    /// artifact Bazel's spawns hold; a tree listed twice is one group.
    #[allow(clippy::too_many_arguments)]
    fn commit_action(
        &self,
        mut action: Action,
        own: Vec<Artifact>,
        runfiles_trees: &[Arc<crate::analysis_runtime::ToolRunfilesTree>],
        deferred: Option<scheduler::Deferred>,
        owner: &str,
        main_ignored: &IgnoredSubdirectories,
        commit: &mut ActionCommit<'_>,
    ) -> Result<(), String> {
        if self.cquery_capture.is_some() {
            return Ok(());
        }
        let mut groups: Vec<Arc<scheduler::InputGroup>> = Vec::new();
        for tree in runfiles_trees {
            let group = match commit.input_groups.get(&tree.identity()) {
                Some(expanded) => {
                    // One identity is one content
                    // (crate::analysis_runtime::ToolRunfilesTree).
                    debug_assert!(
                        Arc::ptr_eq(&expanded.tree, tree) || expanded.tree.same_content(tree),
                        "internal error: runfiles tree {} of identity {} differs from the one expanded",
                        tree.directory(),
                        tree.identity()
                    );
                    Arc::clone(&expanded.group)
                }
                None => {
                    let (artifacts, empty_files) =
                        runfiles_tree_entries(tree, owner, &mut |file| {
                            commit.artifact(self, file)
                        })?;
                    let group = Arc::new(runfiles_input_group(
                        Path::new(tree.directory()),
                        artifacts,
                        empty_files,
                        main_ignored,
                    ));
                    commit.input_groups.insert(
                        tree.identity(),
                        ExpandedTree {
                            tree: Arc::clone(tree),
                            group: Arc::clone(&group),
                        },
                    );
                    group
                }
            };
            if !groups.iter().any(|known| Arc::ptr_eq(known, &group)) {
                groups.push(group);
            }
        }
        // The groups stay shared when listing them could change nothing
        // but the list (crate::scheduler::groups_apart): no entry would
        // be dropped as a duplicate and the listed action validates
        // exactly when its own part does. Otherwise they are listed in
        // the action, as without sharing.
        let shared_groups = !groups.is_empty() && {
            list_artifacts(&mut action, &own);
            scheduler::groups_apart(&action, &groups) && action::validate_structure(&action).is_ok()
        };
        if shared_groups {
            let sources = source_paths(&own);
            let id = action.id.clone();
            let mut planned = plan_action(action, own, main_ignored, commit.client_environment)?;
            commit
                .source_inputs
                .insert(id, SourceInputs::planned(sources, &planned, groups.clone()));
            planned.groups = groups;
            planned.deferred = deferred;
            commit.nodes.push(planned);
            return Ok(());
        }
        if !groups.is_empty() {
            *commit.listed_tree_actions += 1;
        }
        let mut inputs = own;
        for tree in runfiles_trees {
            let (artifacts, empty_files) =
                runfiles_tree_entries(tree, owner, &mut |file| commit.artifact(self, file))?;
            inputs.extend(artifacts);
            for destination in empty_files {
                if !action
                    .extra
                    .param_files
                    .iter()
                    .any(|(existing, _)| *existing == destination)
                {
                    action.extra.param_files.push((destination, Vec::new()));
                }
            }
        }
        let sources = source_paths(&inputs);
        let id = action.id.clone();
        let empties: Vec<(PathBuf, Vec<u8>)> = action
            .extra
            .param_files
            .iter()
            .filter(|(path, content)| {
                content.is_empty()
                    && runfiles_trees
                        .iter()
                        .any(|tree| path.starts_with(tree.directory()))
            })
            .cloned()
            .collect();
        let mut planned = plan_action(action, inputs, main_ignored, commit.client_environment)?;
        commit
            .source_inputs
            .insert(id, SourceInputs::planned(sources, &planned, Vec::new()));
        // A deferred command line recomputes the parameter files;
        // the runfiles empty files are kept.
        planned.deferred = match (deferred, empties.is_empty()) {
            (Some(deferred), false) => Some(deferred.with_extra_param_files(empties)),
            (deferred, _) => deferred,
        };
        commit.nodes.push(planned);
        Ok(())
    }

    /// The commit of one native target (genrule, filegroup, platform
    /// providers, aliases; the tail of the split plan_target): the
    /// coordinator-side analysis of native rules, which P3 runs inline.
    #[allow(clippy::too_many_arguments)]
    fn commit_native_target(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        rule: &Rule,
        key: ConfiguredTargetKey,
        config: ConfigId,
        output_root: String,
        action_env: Arc<crate::action_env::ActionEnvironment>,
        action_owner: String,
        exec_config: ConfigId,
        main_ignored: Arc<IgnoredSubdirectories>,
    ) -> Result<(), String> {
        let ActionPlanning {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            producers,
            nodes,
            source_inputs,
            native_output_groups,
            validation,
            client_environment,
            status_client_environment,
            workspace_status,
            target_identities,
            views,
            input_groups,
            listed_tree_actions,
            ..
        } = state;
        let dependency_key = |label: Label| ConfiguredTargetKey::new(label, config);
        // RuleConfiguredTargetBuilder.propagateTransitiveValidationOutputGroups
        // (:175-176, 294-296, 360-430): the native-rule collection of this
        // target's attribute prerequisites' `_validation` groups (the
        // Starlark-rule path is analysis::transitive_validation), composed
        // into one node of their sets (W1c). Implicit `$` attributes and tool
        // dependencies (exec/no-config edges: genrule `tools`,
        // `package_metadata`) never contribute; every dependency was planned
        // before its dependent (analysis postorder).
        {
            let mut members = Vec::new();
            let edges = self.rule_edges(rule);
            // Rule-class attribute order (AbstractAttributeMapper.
            // getAttributeNames over RuleClass.getAttributes): the built-in
            // `toolchains` attribute before the rule's own `srcs`.
            let mut ordered = edges
                .iter()
                .filter(|edge| edge.attribute == "toolchains")
                .collect::<Vec<_>>();
            ordered.extend(edges.iter().filter(|edge| edge.attribute != "toolchains"));
            for edge in ordered {
                if edge.attribute.starts_with('$')
                    || edge.attribute.starts_with('_')
                    || !matches!(edge.cfg, EdgeCfg::Target)
                {
                    continue;
                }
                let Ok(label) = self.resolve_rule_src(rule, &edge.label) else {
                    continue;
                };
                // OutputGroupInfo.get(prerequisite): an output file answers
                // with its generating rule's `_validation` group
                // (OutputFileConfiguredTarget.java:109-118, as its
                // dependency view does).
                let dep_key = match self.target(&label) {
                    Ok(Target::Output(generating, _)) => {
                        ConfiguredTargetKey::new(generating.label.clone(), config)
                    }
                    _ => ConfiguredTargetKey::new(label, config),
                };
                if let Some(set) = self.planned_validation(custom_providers, validation, &dep_key) {
                    members.push(set.node);
                }
            }
            if let Some(set) = crate::output_group_info::compose_validation(members) {
                validation.insert(key.clone(), set);
            }
        }
        // A native rule's source inputs join the index here (W1b
        // registration invariant); generated ones are registered already.
        let mut inputs = if self.cquery_capture.is_some() {
            self.semantic_inputs(rule, config, dry, declared)?
        } else {
            // Build and --nobuild retain strict execution-path normalization
            // after shared-action reconciliation. Do not use execute_build as
            // the analysis-only switch: --nobuild is not cquery.
            self.inputs(rule, config, dry, declared)?.0
        };
        for artifact in &inputs {
            register_artifact(artifact_index, &artifact.file, artifact);
        }
        // The genrule's location map (LocationTemplateContext over
        // CommandHelper.getLabelMap, LocationExpander.buildLocationMap with
        // neither data nor collectSrcs; analysis/LocationExpander.java:393-485):
        // the `srcs` files under the label as written
        // (BazelGenRule.collectSources), the tool dependencies' executable
        // (else their files) and the outputs.
        let mut location_map = crate::location_expander::LocationMap::default();
        let mut tools = Vec::new();
        let mut tool_trees = Vec::new();
        let mut template_variables: Vec<Vec<(String, String)>> = Vec::new();
        if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
            self.genrule_toolchains_errors(rule, config, configured, custom_providers)?;
            for src in &rule.srcs {
                let label = self.resolve_rule_src(rule, src)?;
                let artifacts = self.artifacts(&label, config, dry, declared)?;
                location_map.add(&label.to_string(), artifacts.iter().map(Artifact::file));
            }
            // GenRuleBase.java:96-110 with CommandHelper (analysis/CommandHelper.java:128-181):
            // `tools` (exec configuration), `toolchains` and the resolved
            // toolchains (target configuration) are tool dependencies: their
            // files to build are action inputs, never $(SRCS); each maps the
            // label as written to its executable, else its files to build.
            // buildLocationMap adds `tools` again under the actual label too.
            let mut tool_dependencies: Vec<(Label, ConfigId, bool)> = native
                .labels("tools")
                .iter()
                .map(|text| Ok((self.resolve_rule_src(rule, text)?, exec_config, true)))
                .collect::<Result<_, String>>()?;
            for text in native.labels("toolchains") {
                tool_dependencies.push((self.resolve_rule_src(rule, &text)?, config, false));
            }
            if let Some(context) = self.toolchain_contexts.get(&key) {
                for implementation in context.resolved_labels() {
                    tool_dependencies.push((implementation, config, false));
                }
            }
            for (label, edge_config, both_labels) in tool_dependencies {
                let mut artifacts = self.artifacts(&label, edge_config, dry, declared)?;
                let files = artifacts.iter().map(Artifact::file).collect::<Vec<_>>();
                let dependency = ConfiguredTargetKey::new(label.clone(), edge_config);
                let actual = self
                    .forwarded_actual(configured, &dependency)
                    .unwrap_or_else(|| label.clone());
                let target = self.target(&actual)?;
                let runfiles = dependency_runfiles(
                    &target,
                    &files,
                    runfiles_info.get(&ConfiguredTargetKey::new(actual.clone(), edge_config)),
                );
                let files_to_run = runfiles.files_to_run(&files);
                // A tool with runfiles support also brings its runfiles
                // tree and its executable, which may be missing from its
                // files to build (CommandHelper.java:156-166): staged at
                // `<executable>.runfiles` like a Starlark action's tool
                // (crate::analysis_runtime::tool_runfiles_tree), so a
                // launcher finds its runfiles next to `$0`.
                if let Some(tree) = crate::analysis_runtime::tool_runfiles_tree(&files_to_run) {
                    if let Some(executable) = files_to_run.executable()
                        && !files.contains(executable)
                    {
                        let artifact =
                            artifact_index.get(executable).cloned().ok_or_else(|| {
                                format!(
                                    "{}: tool executable has no declared artifact: {}",
                                    rule.label, executable.path
                                )
                            })?;
                        artifacts.push(artifact);
                    }
                    tool_trees.push(tree);
                }
                let located = match files_to_run.executable() {
                    Some(executable) => vec![executable.clone()],
                    None => files,
                };
                location_map.add(&label.to_string(), located.iter().cloned());
                if both_labels {
                    location_map.add(&actual.to_string(), located);
                }
                // RuleContext.getDefaultTemplateVariableProviders
                // (analysis/RuleContext.java:900-940): the `toolchains`
                // prerequisites' TemplateVariableInfo, then the resolved
                // toolchains' (ResolvedToolchainContext.java:56-85).
                if !both_labels
                    && let Some(variables) = custom_providers
                        .get(&dependency)
                        .and_then(|providers| {
                            providers.get(crate::template_variable_info::PROVIDER_ID)
                        })
                        .and_then(|provider| {
                            crate::template_variable_info::variables(provider.value())
                        })
                {
                    template_variables.push(variables);
                }
                for artifact in &artifacts {
                    register_artifact(artifact_index, &artifact.file, artifact);
                }
                tools.extend(artifacts);
            }
        }
        let outputs = rule
            .outs
            .iter()
            .map(|o| {
                rule.label
                    .with_name(o)
                    .map(|label| label.generated_exec_path(&output_root))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if matches!(rule.kind, RuleKind::Genrule) {
            if outputs.is_empty() {
                // GenRuleBase.create (rules/genrule/GenRuleBase.java:75-77) with
                // RuleContext.attributeError's prefix.
                return Err(located(
                    rule.location.as_deref(),
                    format!(
                        "in outs attribute of genrule rule {}: Genrules without outputs don't make sense",
                        crate::rule_declarations::canonical_form(&rule.label)
                    ),
                ));
            }
            if let Some(error) = genrule_executable_error(rule, outputs.len()) {
                return Err(error);
            }
            for (name, path) in rule.outs.iter().zip(&outputs) {
                let output = rule.label.with_name(name)?;
                location_map.add(
                    &output.to_string(),
                    [File::new(FileData {
                        is_directory: false,
                        is_symlink: false,
                        path: path.to_string_lossy().into_owned(),
                        short_path: output.generated_short_path(),
                        owner: rule.label.to_string(),
                        is_source: false,
                    })],
                );
            }
            // GenRuleBase.determineCommandTypeAndAttribute
            // (rules/genrule/GenRuleBase.java:262-284); cmd_bash makes
            // the rule unanalyzed earlier, cmd_bat/cmd_ps are Windows-only.
            let base_command = rule.cmd.as_deref().ok_or_else(|| {
                located(
                    rule.location.as_deref(),
                    format!(
                        "in cmd attribute of genrule rule {}: missing value for `cmd` attribute, you can also set `cmd_ps` or `cmd_bat` on Windows and `cmd_bash` on other platforms.",
                        crate::rule_declarations::canonical_form(&rule.label)
                    ),
                )
            })?;
            // The configuration's Make variables (ConfigurationMakeVariableContext);
            // a deferred default target platform is resolved only when
            // `$(TARGET_CPU)` is looked up.
            let platform_key = dependency_key(
                self.configurations
                    .get(config)
                    .native
                    .target_platform()
                    .clone(),
            );
            let mut surface = self.ctx_surface(
                rule,
                config,
                &output_root,
                match self.platforms.get(&platform_key) {
                    Some(Ok(platform)) => {
                        crate::platforms::TargetPlatform::Resolved(platform.clone())
                    }
                    _ => crate::platforms::TargetPlatform::Deferred,
                },
                template_variables,
            );
            let srcs = inputs
                .iter()
                .map(|a| a.exec.to_path_buf())
                .collect::<Vec<_>>();
            // RuleContext.getPackageDirectory below the output root
            // (GenRuleBase.java:353-355), without a trailing separator.
            let package_directory = rule.label.package_exec_path();
            let rule_dir = if package_directory.as_os_str().is_empty() {
                PathBuf::from(&output_root)
            } else {
                PathBuf::from(&output_root).join(package_directory)
            };
            let repositories = self.repositories.clone();
            let parse =
                |text: &str| repositories.parse(text, &rule.label.repository, &rule.label.package);
            let mut expanded = expand_genrule_command(
                base_command,
                &srcs,
                &outputs,
                &rule_dir,
                &surface,
                &location_map,
                &parse,
            );
            if expanded.as_ref().is_err_and(|error| {
                error
                    .to_string()
                    .contains(crate::platforms::NEEDS_TARGET_PLATFORM)
            }) {
                let platform = self.target_platform(config, configured, &mut Vec::new())?;
                surface.target_platform = crate::platforms::TargetPlatform::Resolved(platform);
                expanded = expand_genrule_command(
                    base_command,
                    &srcs,
                    &outputs,
                    &rule_dir,
                    &surface,
                    &location_map,
                    &parse,
                );
            }
            // Expander.expand reports an ExpansionException as an attribute
            // error of the command attribute (analysis/Expander.java:159-172);
            // GenRuleBase then stops (RuleContext.hasErrors).
            let command = expanded
                .map_err(|error| format!("{}: {error:#}", rule.label))?
                .map_err(|message| {
                    let label = crate::rule_declarations::canonical_form(&rule.label);
                    crate::rule_errors::located_events(
                        rule.location.as_deref(),
                        [crate::rule_errors::RuleEvents {
                            rule_class: "genrule",
                            label: &label,
                        }
                        .attribute_message("cmd", &message)],
                    )
                })?;
            restricted_to_error(rule)?;
            if self.cquery_capture.is_none() {
                let producer = OutputProducer::native(&key.label.to_string());
                for output in &outputs {
                    register_output(producers, output, &producer)?;
                }
            }
            // GenRuleBase.java:141-147, 170-175, 212-216: bash sources the
            // `$genrule_setup` script (set -euo pipefail), which is an input;
            // the argv is BashCommandConstructor.asExecArgv (`/bin/bash -c
            // CMD`, no `$0` padding), the environment the configuration's
            // action environment (GenRuleBase.java:210-219, crate::action_env,
            // N42). Commands longer than CommandHelper.maxCommandLength
            // (64000) run from a generated script file in Bazel, which the
            // native profile does not model (N41).
            if command.len() > 64_000 {
                return Err(located(
                    rule.location.as_deref(),
                    format!(
                        "in cmd attribute of genrule rule {}: the expanded command is {} characters; Bazel runs commands longer than 64000 characters from a generated .genrule_script.sh file, which the native profile does not model",
                        crate::rule_declarations::canonical_form(&rule.label),
                        command.len()
                    ),
                ));
            }
            if self.cquery_capture.is_none() {
                let mut execution_info = rule_execution_info(rule);
                if native_bool(rule, "local") {
                    execution_info.insert("local".to_owned());
                }
                let mut action = Action {
                    id: action_owner.clone(),
                    command: format!(
                        "source {}; {command}",
                        crate::builtin_tools::GENRULE_SETUP_EXEC_PATH
                    ),
                    outputs: outputs.clone(),
                    extra: action::ActionExtra {
                        inherited_environment: action_env.inherited.clone(),
                        // GenRuleAction.MNEMONIC (GenRuleAction.java:35).
                        mnemonic: "Genrule".into(),
                        ..Default::default()
                    },
                    environment: action_env.fixed.clone(),
                    execution_info,
                    embedded_inputs: vec![(
                        crate::builtin_tools::GENRULE_SETUP_EXEC_PATH.into(),
                        crate::builtin_tools::GENRULE_SETUP,
                    )],
                    ..Action::default()
                };
                self.set_remote_platform(&mut action, rule, config, None, true);
                let action_inputs = inputs.iter().cloned().chain(tools).collect::<Vec<_>>();
                // The tools' runfiles trees are shared input groups, as a
                // Starlark action's tools' (GenRuleAction's spawn expands the
                // runfiles tree artifacts CommandHelper resolved).
                let mut commit = ActionCommit {
                    artifact_index,
                    workspace_status,
                    nodes,
                    producers,
                    source_inputs,
                    client_environment,
                    status_client_environment,
                    input_groups,
                    listed_tree_actions,
                };
                self.commit_action(
                    action,
                    action_inputs,
                    &tool_trees,
                    None,
                    &key.label.to_string(),
                    &main_ignored,
                    &mut commit,
                )?;
            }
            let artifacts = outputs
                .into_iter()
                .enumerate()
                .map(|(index, exec)| {
                    Artifact::new(
                        exec.clone(),
                        exec,
                        Some((action_owner.clone(), index)),
                        rule.label.to_string(),
                        false,
                        false,
                    )
                })
                .collect::<Vec<_>>();
            for artifact in &artifacts {
                register_artifact(artifact_index, &artifact.file, artifact);
                declared.insert((config, artifact.exec.to_path_buf()), artifact.clone());
            }
            runfiles_info.insert(
                key.clone(),
                Arc::new(
                    crate::runfiles::TargetRunfiles::genrule(
                        &artifacts.iter().map(Artifact::file).collect::<Vec<_>>(),
                        genrule_is_executable(rule),
                    )
                    .map_err(|error| format!("{}: {error:#}", rule.label))?,
                ),
            );
            dry.insert(key, Arc::new(artifacts));
        } else {
            restricted_to_error(rule)?;
            // The native platform providers, visible to Starlark as
            // dep[platform_common.X] (crate::platform_providers).
            if let Some(node) = configured.get(&key)
                && !forwards(rule)
            {
                use crate::platform_providers::Provider;
                let provider = match (
                    &node.constraint_setting,
                    &node.constraint_value,
                    &node.platform,
                ) {
                    (Some(info), _, _) => Some(Provider::Setting(info.clone())),
                    (_, Some(info), _) => Some(Provider::Value(info.clone())),
                    (_, _, Some(info)) => Some(Provider::Platform(info.clone())),
                    _ => None,
                };
                if let Some(provider) = provider {
                    let (id, value) = crate::platform_providers::freeze(provider)?;
                    custom_providers.insert(key.clone(), BTreeMap::from([(id, value)]));
                }
                // ConfigFeatureFlagProvider, visible as
                // dep[config_common.FeatureFlagInfo].
                if let Some(flag) = &node.feature_flag {
                    let value = crate::feature_flags::freeze(flag.clone())?;
                    custom_providers.insert(
                        key.clone(),
                        BTreeMap::from([(crate::feature_flags::PROVIDER_ID.to_owned(), value)]),
                    );
                }
            }
            // An alias-like target forwards its actual's files (above) and
            // providers.
            if forwards(rule) {
                let actual = dependency_key(self.resolve_rule_src(rule, &rule.srcs[0])?);
                if let Some(providers) = custom_providers.get(&actual).cloned() {
                    custom_providers.insert(key.clone(), providers);
                }
                if let Some(runfiles) = runfiles_info.get(&actual).cloned() {
                    runfiles_info.insert(key.clone(), runfiles);
                }
            }
            if let (RuleKind::Filegroup, Some(native)) = (&rule.kind, &rule.native) {
                // Filegroup.create (rules/filegroup/Filegroup.java:52-62): an
                // output group named with OutputGroupInfo.INTERNAL_SUFFIX is
                // an attribute error; a non-empty one replaces the `srcs`
                // files as the files to build (and the single-file
                // executable, the data runfiles' first part):
                // getArtifactsForOutputGroup (:133-146) adds each `srcs`
                // prerequisite's group — OutputGroupInfo.get(dep), the
                // provider its dependents see (a source file has none, an
                // output file only its generating rule's `_validation`
                // group, a native rule its framework groups, an alias its
                // actual's) — to one stable-order set.
                let output_group = native.string("output_group").unwrap_or_default();
                if output_group.ends_with(crate::output_group_info::INTERNAL_SUFFIX) {
                    let label = crate::rule_declarations::canonical_form(&rule.label);
                    return Err(crate::rule_errors::located_events(
                        rule.location.as_deref(),
                        [crate::rule_errors::RuleEvents {
                            rule_class: "filegroup",
                            label: &label,
                        }
                        .attribute_message(
                            "output_group",
                            &format!(
                                "Output group {output_group} is not permitted for reference in filegroups."
                            ),
                        )],
                    ));
                }
                if !output_group.is_empty() {
                    let mut members = Vec::new();
                    for text in &rule.srcs {
                        let label = self.resolve_rule_src(rule, text)?;
                        let view = self.plan_dependency_view(
                            configured,
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
                            &label,
                            config,
                        )?;
                        if let Some(set) = view
                            .providers
                            .get(crate::output_group_info::PROVIDER_ID)
                            .and_then(|info| crate::output_group_info::groups(info.value()))
                            .and_then(|groups| groups.iter().find(|(name, _)| name == output_group))
                            .map(|(_, set)| set)
                        {
                            members.push(Arc::clone(&set.node));
                        }
                    }
                    let files = crate::output_group_info::compose_members(members)
                        .map(|set| crate::output_group_info::depset_files(&set))
                        .unwrap_or_default();
                    let mut selected = Vec::with_capacity(files.len());
                    for file in &files {
                        let artifact = self
                            .indexed_artifact(
                                file,
                                artifact_index,
                                workspace_status,
                                nodes,
                                producers,
                                client_environment,
                                status_client_environment,
                            )?
                            .ok_or_else(|| {
                                format!(
                                    "{}: output group '{output_group}' file has no declared artifact identity: {}",
                                    rule.label, file.path
                                )
                            })?;
                        selected.push(artifact);
                    }
                    inputs = selected;
                }
                // Each `srcs` and `data` prerequisite's files to build and
                // runfiles providers (Filegroup.java:80-104). A `data`
                // prerequisite's source files join the artifact index like
                // the `srcs` inputs above (they are runfiles).
                let mut prerequisites = |labels: &[String]| -> Result<Vec<_>, String> {
                    let mut found = Vec::new();
                    for text in labels {
                        let label = self.resolve_rule_src(rule, text)?;
                        let artifacts = self.artifacts(&label, config, dry, declared)?;
                        for artifact in artifacts.iter() {
                            register_artifact(artifact_index, &artifact.file, artifact);
                        }
                        let files = artifacts.iter().map(Artifact::file).collect::<Vec<_>>();
                        let actual = self
                            .forwarded_actual(configured, &dependency_key(label.clone()))
                            .unwrap_or_else(|| label.clone());
                        let target = self.target(&actual)?;
                        let runfiles = dependency_runfiles(
                            &target,
                            &files,
                            runfiles_info.get(&dependency_key(actual)),
                        );
                        found.push((files, runfiles));
                    }
                    Ok(found)
                };
                let srcs = prerequisites(&rule.srcs)?;
                let data = prerequisites(&native.labels("data"))?;
                let files = inputs.iter().map(Artifact::file).collect::<Vec<_>>();
                fn pairs(
                    prerequisites: &[(Vec<File>, Arc<crate::runfiles::TargetRunfiles>)],
                ) -> Vec<(&[File], &crate::runfiles::TargetRunfiles)> {
                    prerequisites
                        .iter()
                        .map(|(files, runfiles)| (files.as_slice(), &**runfiles))
                        .collect()
                }
                runfiles_info.insert(
                    key.clone(),
                    Arc::new(
                        crate::runfiles::TargetRunfiles::filegroup(
                            &files,
                            &pairs(&srcs),
                            &pairs(&data),
                        )
                        .map_err(|error| format!("{}: {error:#}", rule.label))?,
                    ),
                );
            }
            if let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) {
                // TestSuite.create (rules/test/TestSuite.java:51-90): no files
                // to build; the data runfiles of `tests` and `$implicit_tests`
                // whose test rules (TestTagsProvider: Rule.getRuleTags,
                // through an alias too) match the suite's tags without
                // `manual`; suites are never filtered.
                use crate::test_command::{matches_filters, sort_tags_by_sense};
                let (required, excluded) = sort_tags_by_sense(&native.tags);
                let mut labels = native.labels("tests");
                labels.extend(self.implicit_tests(rule).iter().map(ToString::to_string));
                let mut members = Vec::new();
                for text in labels {
                    let label = self.resolve_rule_src(rule, &text)?;
                    let actual = self
                        .forwarded_actual(configured, &dependency_key(label.clone()))
                        .unwrap_or_else(|| label.clone());
                    let target = self.target(&actual)?;
                    if let Target::Rule(member) = &target
                        && let Some(tags) = rule_tags_of(member)
                        && !matches_filters(&tags, &required, &excluded, true)
                    {
                        continue;
                    }
                    let files = self
                        .artifacts(&label, config, dry, declared)?
                        .iter()
                        .map(Artifact::file)
                        .collect::<Vec<_>>();
                    let runfiles = dependency_runfiles(
                        &target,
                        &files,
                        runfiles_info.get(&dependency_key(actual)),
                    );
                    members.push((files, runfiles));
                }
                let members = members
                    .iter()
                    .map(|(files, runfiles)| (files.as_slice(), &**runfiles))
                    .collect::<Vec<_>>();
                runfiles_info.insert(
                    key.clone(),
                    Arc::new(
                        crate::runfiles::TargetRunfiles::test_suite(&members)
                            .map_err(|error| format!("{}: {error:#}", rule.label))?,
                    ),
                );
            }
            dry.insert(key, Arc::new(inputs));
        }
        Ok(())
    }

    /// AliasConfiguredTarget: the end of the alias chain of an alias-like
    /// planned target (dependents see its label, kind and providers).
    fn forwarded_actual(
        &self,
        configured: &ConfiguredNodes,
        key: &ConfiguredTargetKey,
    ) -> Option<Label> {
        configured.get(key)?.actual.as_ref()?;
        Some(self.alias_chain(configured, key.config, &key.label).1)
    }

    /// Requested keys delegating to the key a rule transition analyzed them
    /// under see that key's files and providers, copied once the delegate is
    /// planned (dependents follow it in analysis order). Delegations recorded
    /// since the last call (also between early flushes, Self::plan_pending)
    /// join the pending ones first.
    fn flush_delegations(&self, state: &mut ActionPlanning) {
        for (requested, owner) in &self.rule_delegations {
            if !state.dry.contains_key(requested)
                && !state.delegations.iter().any(|(seen, _)| seen == requested)
            {
                state.delegations.push((requested.clone(), owner.clone()));
            }
        }
        flush_delegations(
            &mut state.delegations,
            &mut state.dry,
            &mut state.custom_providers,
            &mut state.provider_keys,
            &mut state.runfiles_info,
        );
    }

    /// Phase 2 of every planned target not planned yet, in plan order.
    fn plan_targets(
        &mut self,
        state: &mut ActionPlanning,
        plan: &Plan,
        configured: &mut ConfiguredNodes,
        // M5 span ownership, see plan_targets_timed callers.
        outer: bool,
    ) -> Result<(), String> {
        // M5 (docs/PARALLEL_ANALYSIS.md "P3 instrumentation"): per-stage
        // timers; inert unless BAZEL_RS_TIMING=1. `outer` marks the
        // phase-2 call in plan_build (span end) vs plan_pending flushes
        // driven inside phase 1, whose targets time identically; the
        // span starts at the first plan_targets entry either way.
        crate::plan_timings::note_span_start();
        let before = state.done;
        let result = self.plan_targets_timed(state, plan, configured);
        crate::plan_timings::record_batch(outer, state.done.saturating_sub(before) as u64);
        if outer {
            crate::plan_timings::record_span();
        }
        result
    }

    fn plan_targets_timed(
        &mut self,
        state: &mut ActionPlanning,
        plan: &Plan,
        configured: &mut ConfiguredNodes,
    ) -> Result<(), String> {
        while let Some((key, rule)) = plan.get(state.done) {
            let index = state.done as u64;
            state.done += 1;
            crate::progress::analysis_configured(state.done);
            self.flush_delegations(state);
            if !self.keep_going {
                self.plan_one_target(state, key, rule, configured, index)?;
                continue;
            }
            // --keep_going (docs/KEEP_GOING.md): a target above a failure is
            // skipped before its implementation could read the failed
            // dependency's (missing) providers; a target's own failure is
            // a root cause, reported once, and fails what is above it.
            if self.keep_going_state.dependency_failed(key) {
                self.keep_going_state.fail_propagated(key);
                continue;
            }
            match self.plan_one_target(state, key, rule, configured, index) {
                Ok(()) => self.keep_going_state.planned(key),
                Err(error) => self.keep_going_state.fail_own(key, error),
            }
        }
        self.flush_delegations(state);
        Ok(())
    }

    /// Phase 2 of one plan entry: prepare, compute, commit.
    fn plan_one_target(
        &mut self,
        state: &mut ActionPlanning,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        configured: &mut ConfiguredNodes,
        index: u64,
    ) -> Result<(), String> {
        use crate::plan_timings::{PlanStage, StageTimer};
        // --profile (crate::profile): the target's analysis on this thread.
        let profiled = crate::profile::active().then(std::time::Instant::now);
        {
            // The P1 split of plan_target (review §3): prepare builds the
            // coordinator context and the owned snapshot, compute runs the
            // pure implementation analysis, commit applies the effects in
            // the sequential order. Phase 1 remains fully sequential.
            let class = crate::plan_timings::class_id(rule.rule_class_name());
            let timer = StageTimer::start(PlanStage::Prepare, index, class);
            let prepared = self.prepare_target(state, key, rule, configured)?;
            timer.finish();
            let PreparedTarget {
                key,
                config,
                output_root,
                action_env,
                action_owner,
                exec_config,
                main_ignored,
                transitive,
                kind,
            } = prepared;
            let mut computed = None;
            let kind = match kind {
                PreparedKind::Custom(custom) => {
                    let mut custom = *custom;
                    let timer = StageTimer::start(PlanStage::Compute, index, class);
                    let (snapshot, outcome) = Engine::compute_target(custom.snapshot);
                    timer.finish();
                    custom.snapshot = snapshot;
                    computed = Some(outcome);
                    PreparedKind::Custom(Box::new(custom))
                }
                other => other,
            };
            let prepared = PreparedTarget {
                key,
                config,
                output_root,
                action_env,
                action_owner,
                exec_config,
                main_ignored,
                transitive,
                kind,
            };
            let timer = StageTimer::start(PlanStage::Commit, index, class);
            let committed = self.commit_target(state, configured, rule, prepared, computed);
            timer.finish();
            committed?;
        }
        if let Some(started) = profiled {
            crate::profile::span(
                "configured target",
                key.label.to_string(),
                started,
                vec![("rule", rule.rule_class_name().to_owned())],
            );
        }
        Ok(())
    }

    /// Plans the targets analyzed so far (all complete, in post-order) before
    /// the rest of phase 1 continues, so that a `config_setting` sees the
    /// providers its Starlark `flag_values` keys return (FeatureFlagInfo,
    /// ConfigSetting.java:651-687): in Bazel the key's implementation runs
    /// before the config_setting's (a Skyframe dependency).
    ///
    /// The planning state is absent while action planning itself runs (an
    /// implementation's analysis reaching a `config_setting` with Starlark
    /// keys, e.g. a deferred target platform's tree): a nested flush is not
    /// modelled and fails explicitly.
    fn plan_pending(
        &mut self,
        config_setting: &Rule,
        plan: &Plan,
        configured: &mut ConfiguredNodes,
    ) -> Result<(), String> {
        let Some(mut state) = self.action_planning.take() else {
            return Err(format!(
                "config_setting {}: the implementation of a Starlark flag_values key is needed during action planning (a nested flush of pending targets); this is not implemented in the native profile",
                crate::rule_declarations::canonical_form(&config_setting.label)
            ));
        };
        let timer = crate::plan_timings::StageTimer::start(
            crate::plan_timings::PlanStage::PendingFlush,
            u64::MAX,
            0,
        );
        let (errors_before, failed_before) = (
            self.keep_going_state.error_count(),
            self.keep_going_state.failed_count(),
        );
        let result = self.plan_targets(&mut state, plan, configured, false);
        timer.finish();
        self.action_planning = Some(state);
        // --keep_going: a failure of a target this flush planned fails the
        // analysis of the requested target that needed it (with the new
        // root cause, or the one a skipped target's dependency reported).
        if self.keep_going_state.failed_count() > failed_before {
            let error = self
                .keep_going_state
                .error_at(errors_before)
                .or_else(|| self.keep_going_state.last_error())
                .unwrap_or("a dependency failed to analyze");
            return Err(error.to_owned());
        }
        result
    }

    /// The FeatureFlagInfo a planned Starlark target returned.
    fn planned_feature_flag(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<crate::feature_flags::FeatureFlagValue> {
        let providers = self.action_planning.as_ref()?.custom_providers.get(key)?;
        crate::feature_flags::value_of(providers.get(crate::feature_flags::PROVIDER_ID)?.value())
    }

    /// Phase 1 of `plan_build`: the requested targets, then the top-level
    /// aspects' own attribute dependencies, analyzed in order.
    fn analyze_requested(
        &mut self,
        labels: &[Label],
        top: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        for label in labels {
            // A requested non-alias rule is first keyed in the untrimmed
            // top-level configuration, then in its test-trimmed one
            // (TestTrimmingTransitionFactory.create,
            // analysis/test/TestTrimmingTransitionFactory.java:92-135:
            // aliases may point to tests and are not trimmed).
            self.requested_trimmed_root = match self.target(label) {
                Ok(Target::Rule(rule))
                    if !forwards(&rule) && !matches!(rule.kind, RuleKind::PackageGroup(_)) =>
                {
                    Some(ConfiguredTargetKey::new(rule.label.clone(), top))
                }
                _ => None,
            };
            let mut active = Vec::new();
            if self.keep_going {
                self.keep_going_state.current = Some(label.clone());
            }
            let analyzed = self.analyze(label, top, &mut active, configured, plan);
            self.requested_trimmed_root = None;
            self.keep_going_state.current = None;
            let analyzed = analyzed.map_err(|error| match error.split_once(ANALYSIS_ABORTED) {
                // BuildView: `Analysis of target 'L' failed; build
                // aborted: MESSAGE` for the requested target.
                Some((_, message)) => format!(
                    "Analysis of target '{}' failed; build aborted: {message}",
                    crate::rule_declarations::canonical_form(label)
                ),
                None => error,
            });
            match analyzed {
                // --keep_going: the target is skipped (docs/KEEP_GOING.md).
                // A failure is the target's own unless the active chain it
                // left reaches below the target's own key (a dependency's
                // analysis failed: `Analysis failed`).
                Err(error) if self.keep_going => {
                    self.keep_going_state.error(error);
                    self.keep_going_state
                        .failed_targets
                        .insert(label.clone(), active.len() > 1);
                }
                analyzed => analyzed?,
            }
        }
        // N58: the top-level aspects' applications on each requested target
        // and along their `attr_aspects` closure are analyzed like any
        // dependency (ToplevelStarlarkAspectFunction over AspectFunction:
        // the AspectValue depends on the aspect's own attributes, each in
        // its attribute's configuration, Self::aspect_attribute_configurations),
        // their attribute dependencies validated from the aspect's `.bzl`
        // package.
        let aspects = self.top_level_aspects_list()?;
        if aspects.is_empty() {
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        for label in labels {
            if self.keep_going_state.failed_targets.contains_key(label) {
                continue;
            }
            if !matches!(self.target(label)?, Target::Rule(_) | Target::Output(..)) {
                continue;
            }
            let analyzed = self.analyze_aspect_applications(
                label,
                top,
                aspects.clone(),
                &mut Vec::new(),
                configured,
                plan,
                &mut seen,
            );
            match analyzed {
                Err(error) if self.keep_going => {
                    self.keep_going_state.error(error);
                    self.keep_going_state
                        .failed_targets
                        .insert(label.clone(), true);
                }
                analyzed => {
                    analyzed?;
                }
            }
        }
        Ok(())
    }

    /// Start phase-1 package prefetch (crate::prefetch) for a build's
    /// analysis: with more than one loading thread, outside the replay of
    /// local repository rules (a pinned repository set), once the module
    /// graph is resolved (workers never resolve it). The packages loaded so
    /// far (the target patterns') settle it and seed its queue.
    ///
    /// Also while the analysis snapshot records (W6R): what a worker
    /// observes reaches the recording with its unit's commit, a memo hit
    /// or a counting root (crate::observed_fs frames), so a recording is
    /// the threads-1 one; `BAZEL_RS_SNAPSHOT_PREFETCH=0` keeps prefetch off
    /// while recording (diagnostics; not part of the snapshot key).
    fn start_prefetch(&mut self, labels: &[Label]) {
        self.stop_prefetch();
        if self.loading_phase_threads <= 1
            || !self.repositories.prefetch_allowed()
            // The inline-fallback test hook loads every package itself.
            || std::env::var_os("BAZEL_RS_LOADING_NO_DEMAND").is_some()
            || std::env::var_os("BAZEL_RS_DEBUG_NO_PREFETCH").is_some()
            || (crate::observed_fs::recording()
                && std::env::var_os("BAZEL_RS_SNAPSHOT_PREFETCH").is_some_and(|value| value == "0"))
        {
            return;
        }
        // The repository store's path is computed on the coordinator (its
        // first computation is recorded, W6R); it is with the module graph.
        let _ = self.repositories.source_cache_path();
        let Some(prefetch) = crate::prefetch::Prefetcher::start(
            self.repositories.clone(),
            self.loading_phase_threads,
            self.packages
                .iter()
                .map(|(id, package)| (id.clone(), package.rules.clone())),
            loading_jitter_millis(),
        ) else {
            return;
        };
        // The requested targets' own dependencies come first.
        for label in labels {
            let Some(package) = self.packages.get(&label.package_id()) else {
                continue;
            };
            let rule = match package.get(label) {
                Some(rule) => Some(rule),
                None => self.generated.get(label).and_then(|(owner, _)| {
                    self.packages
                        .get(&owner.package_id())
                        .and_then(|package| package.get(owner))
                }),
            };
            if let Some(rule) = rule {
                prefetch.submit_rule(rule);
            }
        }
        self.prefetch = Some(prefetch);
        self.prefetch_followed.clear();
    }

    /// Stop phase-1 prefetch: workers finish their current unit; units
    /// nobody demanded are dropped unreported.
    fn stop_prefetch(&mut self) {
        if let Some(prefetch) = self.prefetch.take() {
            prefetch.shutdown();
        }
        self.prefetch_followed = HashSet::new();
        self.prefetch_platforms = None;
    }

    /// Phase-1 prefetch: the first analysis of `rule` queues the targets
    /// it names (what analysis demands next).
    fn prefetch_rule(&mut self, rule: &Rule) {
        if let Some(prefetch) = &self.prefetch
            && !self.prefetch_followed.contains(&rule.label)
        {
            self.prefetch_followed.insert(rule.label.clone());
            prefetch.submit_rule(rule);
        }
    }

    /// Analysis and action planning of a build: the actions reachable from
    /// the requested targets' artifacts, validated before any execution.
    fn plan_build(&mut self, labels: &[Label]) -> Result<BuildPlan, String> {
        crate::progress::analysis_targets(labels.len());
        crate::perf_mark("plan.start");
        // The workspace status is computed at most once per build
        // (OnceLock), and recomputed by the NEXT build of this Engine —
        // each invocation runs the status script afresh (Bazel runs the
        // workspace status action on every build, WorkspaceStatusAction
        // executeUnconditionally). The per-build ActionPlanning state is
        // likewise dropped so its OnceCells (client env) re-read.
        self.workspace_status_once = std::sync::OnceLock::new();
        self.action_planning = None;
        self.build_info_actions.clear();
        self.keep_going_state = keep_going::State::default();
        self.keep_going_plan = None;
        // TargetPatternPhaseFunction.maybeReportDeprecation
        // (skyframe/TargetPatternPhaseFunction.java:255-275): every requested
        // rule whose `deprecation` is explicitly specified (package defaults
        // excluded, "" included), before analysis.
        for label in labels {
            self.report_pattern_deprecation(label);
        }
        // Requested targets are analyzed in the top-level configuration;
        // generated files live under their own configuration's output root.
        let top = self.configurations.top();
        let mut plan = Plan::new();
        let mut configured = ConfiguredNodes::new();
        self.module_execution_platforms = None;
        self.registered_toolchains.clear();
        self.toolchain_cycle_reports.clear();
        self.registered_execution_platforms.clear();
        self.execution_platform_cycle_reports.clear();
        self.exec_configurations.clear();
        self.deferred_duplicates.clear();
        self.toolchain_contexts.clear();
        self.exec_group_contexts.clear();
        self.group_exec_configurations.clear();
        self.action_executions.clear();
        self.toolchain_implementations.clear();
        self.edge_configs.clear();
        self.rule_delegations.clear();
        self.configured_rules.clear();
        self.aspect_attribute_configs.clear();
        self.aspect_toolchains.clear();
        self.action_planning = Some(ActionPlanning::default());
        // BuildConfigurationKeyProducer resolves an explicit --platforms
        // entry when the top-level configuration is created, before analysis
        // (analysis/producers/BuildConfigurationKeyProducer.java:126-145). A
        // target platform that comes from --host_platform (empty --platforms)
        // is resolved by each toolchain-resolving rule
        // (DependencyContextProducerWithCompatibilityCheck.java:96-110), and a
        // non-default --host_platform by their toolchain resolution
        // (resolve_execution_platform); the default host platform only where
        // a constraint needs it (docs/PLATFORMS.md).
        let top_configuration = self.configurations.top_configuration().clone();
        let target = top_configuration.native.target_platform().clone();
        if target != crate::configuration::default_host_platform()
            && target != top_configuration.native.host_platform
        {
            self.platform_info(&target, top, &mut configured, &mut plan)
                .map_err(|error| error.target_text())?;
        }
        // Phase-1 prefetch (crate::prefetch) runs while the requested
        // targets are analyzed, and stops with phase 1 however it ends.
        self.start_prefetch(labels);
        let analyzed = self.analyze_requested(labels, top, &mut configured, &mut plan);
        self.stop_prefetch();
        analyzed?;
        crate::progress::analysis_planned(plan.len());
        crate::perf_mark("plan.phase1_analysis_done");
        // Alias-like targets and the end of their alias chain: dependents see
        // that target's label, kind and providers (AliasConfiguredTarget).
        let forwarded: BTreeMap<ConfiguredTargetKey, Label> = configured
            .iter()
            .filter(|(_, node)| node.actual.is_some())
            .map(|(key, _)| {
                (
                    key.clone(),
                    self.alias_chain(&configured, key.config, &key.label).1,
                )
            })
            .collect();
        self.ignored("")?;
        // Validate every command before executing any action (action
        // planning, crate::engine::ActionPlanning). The dry artifacts carry
        // the same exec paths as the actual generated files. Targets planned
        // early (a Starlark flag_values key, Self::plan_pending) are kept.
        let mut state = self.action_planning.take().unwrap_or_default();
        self.plan_targets(&mut state, &plan, &mut configured, true)?;
        // --keep_going: the requested targets whose analysis failed in
        // either phase (docs/KEEP_GOING.md), skipped from here on.
        let mut failed_labels = std::mem::take(&mut self.keep_going_state.failed_targets);
        if self.keep_going {
            for label in labels {
                if failed_labels.contains_key(label) {
                    continue;
                }
                let failure = self
                    .keep_going_state
                    .phase2_failure(label, |key| self.rule_delegations.get(key).cloned());
                if let Some(dependency) = failure {
                    failed_labels.insert(label.clone(), dependency);
                }
            }
        }
        if crate::plan_timings::enabled() {
            // M5 report inputs: plan index -> label, and the dep graph by
            // plan index (rule_edges labels resolved to plan positions).
            let labels: Vec<String> = plan
                .iter()
                .map(|(key, _)| crate::rule_declarations::canonical_form(&key.label))
                .collect();
            let positions: HashMap<&ConfiguredTargetKey, u64> = plan
                .iter()
                .enumerate()
                .map(|(index, (key, _))| (key, index as u64))
                .collect();
            let mut edges: HashMap<u64, Vec<u64>> = HashMap::new();
            for (index, (key, rule)) in plan.iter().enumerate() {
                let mut deps = Vec::new();
                for edge in self.rule_edges(rule) {
                    if let Ok(label) = self.resolve_rule_src(rule, &edge.label) {
                        let dep = ConfiguredTargetKey::new(label, key.config);
                        if let Some(dep_index) = positions.get(&dep) {
                            deps.push(*dep_index);
                        }
                    }
                }
                if !deps.is_empty() {
                    edges.insert(index as u64, deps);
                }
            }
            crate::plan_timings::report(&labels, &edges);
        }
        // W0-A: retained-memory breakdown (BAZEL_RS_TIMING_ALLOC=1 only).
        alloc_report::report_if_enabled(self, &state, &plan, &configured);
        // N58: the top-level aspects of `--aspects` (and the appended
        // ValidateTarget of `--experimental_use_validation_aspect`) applied
        // to each requested target (ToplevelStarlarkAspectFunction): the
        // applications are planned here, before the action graph is taken
        // apart, and their merged views answer the output-group request.
        let aspect_defs = self.top_level_aspects_list()?;
        // AspectsList.validateTopLevelAspectsParameters (AspectsList.java:
        // 124-158), over the expanded list (required aspects included): a
        // mandatory parameter attribute needs a value, and every
        // --aspects_parameters key must be a parameter of some top-level
        // aspect.
        for aspect in &aspect_defs {
            if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
                continue;
            }
            for (name, spec) in &aspect.declaration.attrs {
                if crate::aspects::is_parameter_attribute(name, spec)
                    && spec.mandatory
                    && !self.aspect_parameters.contains_key(name)
                {
                    return Err(format!(
                        "Missing mandatory attribute '{name}' for aspect '{}'.",
                        aspect.identity
                    ));
                }
            }
        }
        if !self.aspect_parameters.is_empty() {
            let mut known = BTreeSet::new();
            for aspect in &aspect_defs {
                if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
                    continue;
                }
                for (name, spec) in &aspect.declaration.attrs {
                    if crate::aspects::is_parameter_attribute(name, spec) {
                        known.insert(name.clone());
                    }
                }
            }
            let unknown = self
                .aspect_parameters
                .keys()
                .filter(|key| !known.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            if !unknown.is_empty() {
                return Err(format!(
                    "Parameters '[{}]' are not parameters of any of the top-level aspects but they are specified in --aspects_parameters.",
                    unknown.join(", ")
                ));
            }
        }
        let mut top_level_aspects: BTreeMap<Label, (DependencyTarget, AspectApplications)> =
            BTreeMap::new();
        if !aspect_defs.is_empty() {
            for label in labels {
                if failed_labels.contains_key(label) {
                    continue;
                }
                if !matches!(self.target(label)?, Target::Rule(_) | Target::Output(..)) {
                    continue;
                }
                let mut planning = state.plan_state();
                let planned = self.plan_aspect_dependency(
                    &mut planning,
                    &mut configured,
                    label,
                    top,
                    aspect_defs.clone(),
                );
                let planned = match planned {
                    Err(error) if self.keep_going => {
                        self.keep_going_state.error(error);
                        failed_labels.insert(label.clone(), true);
                        continue;
                    }
                    planned => planned?,
                };
                top_level_aspects.insert(label.clone(), planned);
            }
        }
        // --keep_going: what follows serves the targets that analyzed.
        let all_labels = labels;
        let analyzed_labels: Vec<Label> = if failed_labels.is_empty() {
            Vec::new()
        } else {
            labels
                .iter()
                .filter(|label| !failed_labels.contains_key(*label))
                .cloned()
                .collect()
        };
        let labels: &[Label] = if failed_labels.is_empty() {
            labels
        } else {
            &analyzed_labels
        };
        // The top-level aspects above also run rule implementations
        // (N58); implementations and action planning end here.
        if crate::plan_timings::enabled() {
            // After the top-level aspects: their actions hold groups too.
            report_input_groups(&state.nodes, state.listed_tree_actions);
        }
        crate::perf_mark("plan.phase2_actions_done");
        if self.cquery_capture.is_some() {
            // Full provider analysis (including top-level aspects) succeeded.
            // Nothing below this boundary is part of configured querying.
            crate::perf_mark("cquery.analysis_done");
            return Ok((Vec::new(), Vec::new(), Vec::new(), 0));
        }
        // N63 follow-up (BLOCKER 1): Bazel adds the workspace status
        // artifacts to artifactsToBuild UNCONDITIONALLY on every build
        // that executes (BuildView.createResult :647-650,675), so plan
        // the status node(s) once per executing invocation even when no
        // target consumed them (a failing script then fails a build whose
        // targets never touch the status). Skipped under --nobuild (no
        // execution phase). The artifacts are pushed onto `hidden` below
        // so the reachability prune keeps the node(s) without reporting
        // them as build outputs.
        let mut status_artifacts: Vec<Artifact> = Vec::new();
        if self.execute_build {
            let state_ref = state.plan_state();
            status_artifacts = self
                .ensure_workspace_status(
                    state_ref.workspace_status,
                    state_ref.nodes,
                    state_ref.producers,
                    state_ref.client_environment,
                    state_ref.status_client_environment,
                    state_ref.artifact_index,
                )?
                .to_vec();
        }
        let ActionPlanning {
            dry,
            runfiles_info,
            mut artifact_index,
            declared,
            mut nodes,
            source_inputs,
            runfiles_nodes,
            custom_providers,
            validation,
            mut producers,
            client_environment,
            status_client_environment,
            mut workspace_status,
            provider_keys,
            delegations,
            native_output_groups,
            done: _,
            repositories,
            aspect_providers,
            aspect_target_identities,
            target_identities,
            views,
            input_groups,
            listed_tree_actions: _,
        } = state;
        // The top-level lookups below: a workspace status File no target
        // registered (only possible under --nobuild, which plans no status
        // action above) is registered by the lookup that reaches it.
        let mut resolve = |engine: &Engine, file: &File| {
            engine.indexed_artifact(
                file,
                &mut artifact_index,
                &mut workspace_status,
                &mut nodes,
                &mut producers,
                &client_environment,
                &status_client_environment,
            )
        };
        // BuildRequest.getTopLevelArtifactContext (BuildRequest.java:412-421)
        // with OutputGroupInfo.determineOutputGroups: the command's requested
        // output groups. `default` is the files to build; every other
        // requested group's files come from the target's OutputGroupInfo
        // (TopLevelArtifactHelper.getAllArtifactsToBuild, :196-248). Groups
        // whose name starts with `_` (validation, hidden top level) are
        // built but never reported as the build's outputs (:228-236);
        // `temp_files` is empty without --save_temps.
        let selected = self.selected_output_groups();
        let reports = |group: &str| !group.starts_with('_');
        let mut requested: Vec<(ConfiguredTargetKey, Artifact)> = Vec::new();
        // The built-but-unreported artifacts of hidden requested groups.
        let mut unreported = Vec::new();
        type Resolve<'a> = dyn FnMut(&File) -> Result<Option<Artifact>, String> + 'a;
        let group_artifacts = |key: &ConfiguredTargetKey,
                               label: &Label,
                               group: &str,
                               files: Vec<File>,
                               requested: &mut Vec<(ConfiguredTargetKey, Artifact)>,
                               unreported: &mut Vec<(ConfiguredTargetKey, Artifact)>,
                               resolve: &mut Resolve<'_>|
         -> Result<(), String> {
            for file in files {
                let artifact = resolve(&file)?.ok_or_else(|| {
                    format!(
                        "{label}: output group '{group}' file has no declared artifact identity: {}",
                        file.path
                    )
                })?;
                if reports(group) {
                    requested.push((key.clone(), artifact));
                } else {
                    unreported.push((key.clone(), artifact));
                }
            }
            Ok(())
        };
        // N58 reporting: `Aspect NAME of LABEL up-to-date:` with each
        // aspect's own artifacts in the requested (reportable) groups
        // (BuildResultPrinter.outputAspects); validation aspects are built
        // but never reported (partitionAspectKeys).
        let mut aspect_reports: Vec<(String, Label, Vec<Artifact>)> = Vec::new();
        // Labels whose own (non-aspect) artifacts print a target result —
        // the `--show_result` budget counts these and the aspects with
        // artifacts (BuildResultPrinter.outputTargetsAndAspects).
        let mut target_result_labels: BTreeSet<Label> = BTreeSet::new();
        // The build event plan's output groups (crate::bep::plan): per
        // requested label, each reported group's indices into `requested`.
        let mut bep_groups: Vec<(Label, String, usize, usize)> = Vec::new();
        for label in labels {
            let key = self.configurations.top_key(label.clone());
            let merged = top_level_aspects.get(label);
            let merged_view = merged.map(|(view, _)| view);
            let before_default = requested.len();
            if selected.contains(crate::output_group_info::DEFAULT) {
                match merged_view {
                    // The merged files to build: the base target's plus every
                    // aspect's DefaultInfo.files (FileProvider merge).
                    Some(view) if !view.files.is_empty() => {
                        for file in &view.files {
                            let artifact = resolve(self, file)?.ok_or_else(|| {
                                format!(
                                    "{label}: aspect output has no declared artifact identity: {}",
                                    file.path
                                )
                            })?;
                            requested.push((key.clone(), artifact));
                        }
                    }
                    _ => {
                        for artifact in self.artifacts(label, top, &dry, &declared)? {
                            requested.push((key.clone(), artifact));
                            target_result_labels.insert(label.clone());
                        }
                    }
                }
            }
            bep_groups.push((
                label.clone(),
                crate::output_group_info::DEFAULT.to_owned(),
                before_default,
                requested.len(),
            ));
            let group_key = self.output_group_key(label, top)?;
            for group in &selected {
                if matches!(
                    group.as_str(),
                    crate::output_group_info::DEFAULT
                        | crate::output_group_info::TEMP_FILES
                        | crate::output_group_info::HIDDEN_TOP_LEVEL
                ) {
                    continue;
                }
                // A merged aspect-configured target answers through its
                // merged OutputGroupInfo (mergeOutputGroupProviders).
                let base_files =
                    self.planned_output_group(&custom_providers, &validation, &group_key, group);
                let files = match merged_view.and_then(|view| {
                    view.providers
                        .get(crate::output_group_info::PROVIDER_ID)
                        .and_then(|value| crate::output_group_info::groups(value.value()))
                        .and_then(|groups| {
                            groups
                                .iter()
                                .find(|(name, _)| name == group)
                                .map(|(_, depset)| crate::output_group_info::depset_files(depset))
                        })
                }) {
                    // The base target's own group answers for the target
                    // result line even when an aspect added to it.
                    Some(files) if base_files.is_empty() => files,
                    _ => base_files.clone(),
                };
                if !base_files.is_empty() {
                    target_result_labels.insert(label.clone());
                }
                let before_group = requested.len();
                group_artifacts(
                    &key,
                    label,
                    group,
                    files,
                    &mut requested,
                    &mut unreported,
                    &mut |file: &File| resolve(self, file),
                )?;
                if reports(group) {
                    bep_groups.push((label.clone(), group.clone(), before_group, requested.len()));
                }
            }
            // Per-aspect reports: each aspect's own artifacts in the
            // requested (reportable) groups, plus its DefaultInfo.files for
            // the default group (TopLevelArtifactHelper.getAllArtifactsToBuild
            // over the aspect value).
            if let Some((_, applications)) = merged {
                for (application, providers) in applications {
                    if application.aspect == crate::aspects::VALIDATION_ASPECT_IDENTITY {
                        continue;
                    }
                    let mut artifacts = Vec::new();
                    if selected.contains(crate::output_group_info::DEFAULT)
                        && let Some(value) =
                            providers.get(crate::providers::DEFAULT_INFO_PROVIDER_ID)
                        && let Some(files) = crate::starlark_types::default_files(value.value())
                            .map_err(|e| e.to_string())?
                    {
                        for file in files {
                            if let Some(artifact) = resolve(self, &file)? {
                                artifacts.push(artifact);
                            }
                        }
                    }
                    if let Some(groups) = providers
                        .get(crate::output_group_info::PROVIDER_ID)
                        .and_then(|value| crate::output_group_info::groups(value.value()))
                    {
                        for (name, depset) in groups {
                            if !selected.contains(name) || !reports(name) {
                                continue;
                            }
                            for file in crate::output_group_info::depset_files(depset) {
                                let artifact = resolve(self, &file)?.ok_or_else(|| {
                                    format!(
                                        "{label}: aspect output group '{name}' file has no declared artifact identity: {}",
                                        file.path
                                    )
                                })?;
                                artifacts.push(artifact);
                            }
                        }
                    }
                    // BuildResultPrinter.outputAspects names the aspect
                    // class (the descriptor without its parameters).
                    let class = aspect_defs
                        .iter()
                        .find(|aspect| aspect.descriptor == application.aspect)
                        .map_or_else(
                            || application.aspect.clone(),
                            |aspect| aspect.identity.clone(),
                        );
                    aspect_reports.push((class, label.clone(), artifacts));
                }
            }
        }
        // OutputGroupInfo.HIDDEN_TOP_LEVEL (RuleConfiguredTargetBuilder.java:156-171):
        // the default runfiles artifacts of requested targets are built too,
        // but not reported. The runfiles tree itself is not created (docs/RUNFILES.md).
        // An `--output_groups` override drops the group from the request
        // (determineOutputGroups), so the runfiles artifacts are not built.
        let runfiles_requested = selected.contains(crate::output_group_info::HIDDEN_TOP_LEVEL);
        // A hidden artifact names its target by the index of the target's
        // key in `owners` (MEM4): one key per target, not one copy per
        // runfile of every requested executable.
        let mut owners = KeyTable::default();
        let mut hidden: Vec<(usize, Artifact)> = unreported
            .into_iter()
            .map(|(key, artifact)| (owners.index(key), artifact))
            .collect();
        // The workspace status artifacts of this executing build (above):
        // built but never reported (BuildView adds them to
        // artifactsToBuild, not to the reported outputs). Only artifacts
        // with a producer participate in reachability; under --nobuild
        // this list is empty.
        if let Some(first) = labels.first() {
            let owner = owners.index(self.configurations.top_key(first.clone()));
            for artifact in status_artifacts {
                if artifact.producer.is_some() {
                    hidden.push((owner, artifact));
                }
            }
        }
        let mut seen: std::collections::HashSet<(usize, File)> = requested
            .iter()
            .map(|(key, artifact)| (owners.index(key.clone()), artifact.file()))
            .collect();
        seen.extend(
            hidden
                .iter()
                .map(|(owner, artifact)| (*owner, artifact.file())),
        );
        for label in labels {
            let key = self.configurations.top_key(label.clone());
            let actual = forwarded.get(&key).unwrap_or(label);
            let owner = owners.index(key.clone());
            if runfiles_requested
                && let Some(runfiles) =
                    runfiles_info.get(&self.configurations.top_key(actual.clone()))
            {
                for file in runfiles.default_runfiles().all_artifacts() {
                    let artifact = resolve(self, &file)?.ok_or_else(|| {
                        format!(
                            "{label}: runfiles file has no declared artifact identity: {}",
                            file.path
                        )
                    })?;
                    if seen.insert((owner, artifact.file())) {
                        hidden.push((owner, artifact));
                    }
                }
            }
            // N23f: the runfiles tree artifact of an executable
            // (RuleConfiguredTargetBuilder.java:144-158): RunfilesTreeAction
            // needs the MANIFEST (SymlinkTreeAction, --build_runfile_links),
            // or the input manifest, and the repository mapping manifest.
            // `run` always creates the tree (RunCommand.ensureRunfilesBuilt).
            let actual_key = self.configurations.top_key(actual.clone());
            if let Some(support) = runfiles_nodes.get(&actual_key) {
                let options = self.runfiles_options;
                let running = self.run_requests.contains(label) && self.run_trees;
                let mut artifacts = vec![support.repo_mapping.clone()];
                artifacts.extend(support.manifest.clone());
                if options.links() || running {
                    artifacts.extend(support.tree.clone());
                }
                for artifact in artifacts {
                    if seen.insert((owner, artifact.file())) {
                        hidden.push((owner, artifact));
                    }
                }
            }
            if self.run_requests.contains(label) {
                let files_to_run = runfiles_info
                    .get(&actual_key)
                    .and_then(|runfiles| runfiles.files_to_run.clone());
                let executable = match runfiles_nodes.get(&actual_key) {
                    Some(support) => Some(support.executable.clone()),
                    None => match files_to_run.as_ref().and_then(|f| f.executable()) {
                        Some(file) => Some(resolve(self, file)?.ok_or_else(|| {
                            format!(
                                "{label}: executable has no declared artifact: {}",
                                file.path
                            )
                        })?),
                        // A file target: its own (single) artifact.
                        None if !runfiles_info.contains_key(&actual_key) => {
                            let artifacts = self.artifacts(actual, top, &dry, &declared)?;
                            match artifacts.as_slice() {
                                [single] => Some(single.clone()),
                                _ => None,
                            }
                        }
                        None => None,
                    },
                };
                if let Some(executable) = &executable
                    && seen.insert((owner, executable.file()))
                {
                    hidden.push((owner, executable.clone()));
                }
                // The published runfiles entries (staged by content) are
                // built like the runfiles tree a test action consumes
                // (TestActionBuilder.java:255-257), also when neither the
                // tree nor the manifest is (--nobuild_runfile_manifests with
                // an --output_groups override dropping the runfiles group).
                for artifact in runfiles_nodes
                    .get(&actual_key)
                    .into_iter()
                    .flat_map(|support| &support.entries)
                    .filter_map(|(_, artifact)| artifact.as_ref())
                {
                    if seen.insert((owner, artifact.file())) {
                        hidden.push((owner, artifact.clone()));
                    }
                }
                let plan = RunPlan {
                    executable,
                    // Only a built tree is published (build_requests): `run`
                    // always builds it, `test` only with the target under
                    // --build_runfile_links (above); the test runner stages
                    // runfiles by content and never reads it.
                    tree: runfiles_nodes
                        .get(&actual_key)
                        .filter(|_| self.run_trees || self.runfiles_options.links())
                        .and_then(|support| support.tree.clone()),
                    entries: runfiles_nodes
                        .get(&actual_key)
                        .map(|support| support.entries.clone()),
                    settings: runfiles_info
                        .get(&actual_key)
                        .map(|runfiles| runfiles.run.clone())
                        .unwrap_or_default(),
                    config: top,
                    completion: Vec::new(),
                };
                self.run_plans.insert(label.clone(), plan);
            }
        }
        // PERF(TEST): what each run target's completion builds — the
        // producers of its requested and hidden top-level artifacts
        // (TargetCompletionValue; a test action waits for it,
        // TestCompletionFunction.java:55). The workspace status artifacts
        // ride on the first label's key but belong to no target.
        if !self.run_plans.is_empty() {
            let mut completions: HashMap<ConfiguredTargetKey, BTreeSet<&str>> = self
                .run_plans
                .keys()
                .map(|label| (self.configurations.top_key(label.clone()), BTreeSet::new()))
                .collect();
            let hidden_keyed = hidden
                .iter()
                .map(|(owner, artifact)| (&owners.keys[*owner], artifact));
            for (owner, artifact) in requested
                .iter()
                .map(|(key, a)| (key, a))
                .chain(hidden_keyed)
            {
                if let Some(completion) = completions.get_mut(owner)
                    && let Some((id, _)) = &artifact.producer
                    && !id.starts_with(crate::request::STATUS_ID_PREFIX)
                {
                    completion.insert(id);
                }
            }
            for (label, plan) in self.run_plans.iter_mut() {
                let key = self.configurations.top_key(label.clone());
                if let Some(completion) = completions.get(&key) {
                    plan.completion = completion.iter().map(|id| (*id).to_owned()).collect();
                }
            }
        }
        // Analyze everything first, but execute only the actions reachable from
        // requested outputs. Empty DefaultInfo must not execute unused actions.
        let by_id: BTreeMap<_, _> = nodes
            .iter()
            .map(|node| (node.action.id.clone(), node))
            .collect();
        let mut todo = requested
            .iter()
            .map(|(_, artifact)| artifact)
            .chain(hidden.iter().map(|(_, artifact)| artifact))
            .filter_map(|artifact| artifact.producer.as_ref().map(|(id, _)| id.clone()))
            .collect::<Vec<_>>();
        // Every reported aspect artifact is built too, also one of an output
        // group the base target shares (whose `requested` files are the base
        // target's only, above): CompletionFunction builds each top-level
        // aspect's own artifactsToBuild (getValueAndArtifactsToBuild,
        // CompletionFunction.java:362-376).
        todo.extend(
            aspect_reports
                .iter()
                .flat_map(|(_, _, artifacts)| artifacts)
                .filter_map(|artifact| artifact.producer.as_ref().map(|(id, _)| id.clone())),
        );
        let mut required = BTreeSet::new();
        while let Some(id) = todo.pop() {
            if required.insert(id.clone()) {
                let node = by_id
                    .get(&id)
                    .ok_or_else(|| format!("missing producing action: {id}"))?;
                todo.extend(
                    node.all_dependencies()
                        .map(|dependency| dependency.action_id.clone()),
                );
            }
        }
        nodes.retain(|node| required.contains(&node.action.id));
        // SourceManifestAction.newDeterministicWriter (:233-273): prefix
        // conflicts of an executed manifest action are events at the
        // owner's location, errors under ConflictPolicy.ERROR.
        for support in runfiles_nodes.values() {
            let executed = support
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.producer.as_ref())
                .is_some_and(|(id, _)| required.contains(id));
            if executed {
                for message in &support.conflicts {
                    let text = located(support.location.as_deref(), message.clone());
                    if support.conflict_error {
                        self.errors.push(text);
                    } else {
                        self.warnings.push(text);
                    }
                }
            }
        }
        // Missing declared source files fail only where needed: as requested
        // outputs or as inputs of an executed action (ArtifactFunction
        // "missing input file", skyframe/ArtifactFunction.java:79-90, 404-430;
        // ActionExecutionFunction.createSourceErrorCodeAndMessage,
        // skyframe/ActionExecutionFunction.java:1543-1583).
        let missing_sources = self.missing_sources.clone();
        let missing = |real: &Path| {
            missing_sources
                .get(real)
                .filter(|_| crate::observed_fs::symlink_metadata(real).is_err())
                .map(crate::rule_declarations::canonical_form)
        };
        // Source directories (source artifacts naming a directory) are
        // tracked by content like Bazel 9.3's default
        // (TrackSourceDirectoriesFlag; ArtifactFunction.createSourceValue,
        // skyframe/ArtifactFunction.java:274-364): an action input is keyed
        // by everything below it and staged as a real directory
        // (crate::action, ActionExtra::source_directories); a requested
        // output or runfile needs nothing built. An executing build
        // traverses each one it needs and fails like Bazel's traversal
        // (crate::source_directories), reported like a missing input file
        // (docs/SOURCE_DIRECTORIES.md).
        let source_directories = self.source_directories.clone();
        let is_directory =
            |real: &Path| source_directories.contains_key(real) && crate::observed_fs::is_dir(real);
        // PA-1: a tool runfiles tree's shared group holding a source
        // directory is listed in the node (exactly what planning lists
        // without sharing), so its destination is marked like an own input.
        let mut directory_groups: HashMap<*const scheduler::InputGroup, bool> = HashMap::new();
        for node in &mut nodes {
            let holds_directory = node.groups.iter().any(|group| {
                *directory_groups
                    .entry(Arc::as_ptr(group))
                    .or_insert_with(|| group.sources.iter().any(|real| is_directory(real)))
            });
            if holds_directory {
                *node = node.flattened();
            }
            let marked: Vec<PathBuf> = node
                .action
                .inputs
                .iter()
                .filter(|(real, _)| is_directory(real))
                .map(|(_, exec)| exec.to_path_buf())
                .collect();
            node.action.extra.source_directories.extend(marked);
        }
        let main_ignored = self.ignored("")?;
        // Each needed directory is traversed once; its separate error
        // events (a symlink chain) are reported once, before the errors.
        let mut traversals: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
        let mut events: Vec<String> = Vec::new();
        let execute_build = self.execute_build;
        let mut traversal_error = |real: &Path| -> Option<String> {
            if !execute_build || !is_directory(real) {
                return None;
            }
            traversals
                .entry(real.to_path_buf())
                .or_insert_with(|| {
                    // The owner label (the first of the labels naming it,
                    // N28d).
                    let label = source_directories.get(real)?.iter().next()?;
                    let root_relative = if label.package.is_empty() {
                        label.name.clone()
                    } else {
                        format!("{}/{}", label.package, label.name)
                    };
                    let boundaries = crate::source_directories::Boundaries {
                        ignored: &main_ignored,
                    };
                    let error = crate::source_directories::traversal_error(
                        real,
                        &crate::rule_declarations::canonical_form(label),
                        &root_relative,
                        label.repository.is_empty().then_some(&boundaries),
                    )?;
                    events.extend(error.events);
                    Some(error.message)
                })
                .clone()
        };
        // PA-1: a node's source inputs are its own followed by its shared
        // groups' (SourceInputs::listed). A group none of whose sources is
        // a source directory or a missing file adds nothing to either check
        // below, so only a node holding such a group lists them all.
        let mut flagged_groups: HashSet<*const scheduler::InputGroup> = HashSet::new();
        let mut seen_groups: HashSet<*const scheduler::InputGroup> = HashSet::new();
        for sources in nodes
            .iter()
            .filter_map(|node| source_inputs.get(&node.action.id))
        {
            for group in &sources.groups {
                if seen_groups.insert(Arc::as_ptr(group))
                    && group
                        .sources
                        .iter()
                        .any(|real| is_directory(real) || missing(real).is_some())
                {
                    flagged_groups.insert(Arc::as_ptr(group));
                }
            }
        }
        fn sources_of<'a>(
            node: &'a PlannedAction,
            source_inputs: &'a BTreeMap<String, SourceInputs>,
            flagged_groups: &HashSet<*const scheduler::InputGroup>,
        ) -> Vec<&'a Path> {
            match source_inputs.get(&node.action.id) {
                None => Vec::new(),
                Some(sources)
                    if sources
                        .groups
                        .iter()
                        .any(|group| flagged_groups.contains(&Arc::as_ptr(group))) =>
                {
                    sources.listed(node)
                }
                Some(sources) => sources.own(node),
            }
        }
        let source_paths_of = |node| sources_of(node, &source_inputs, &flagged_groups);
        // An input's failure: `missing input file 'L'` (SOURCE_INPUT_MISSING)
        // or the traversal error (SOURCE_INPUT_IO_EXCEPTION).
        enum InputFailure {
            Missing(String),
            InError(String),
        }
        // ActionExecutionFunction.createSourceErrorCodeAndMessage
        // (:1543-1582): `N input file(s) are in error or do not exist`.
        let summary = |failures: &[InputFailure]| {
            let in_error = failures
                .iter()
                .any(|failure| matches!(failure, InputFailure::InError(_)));
            let missing = failures
                .iter()
                .any(|failure| matches!(failure, InputFailure::Missing(_)));
            let kinds = [
                in_error.then_some("are in error"),
                missing.then_some("do not exist"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" or ");
            format!("{} input file(s) {kinds}", failures.len())
        };
        let failure_text = |failure: &InputFailure| match failure {
            InputFailure::Missing(label) => format!("missing input file '{label}'"),
            InputFailure::InError(message) => message.clone(),
        };
        let mut errors = Vec::new();
        // Requested targets and their runfiles: `LOC: TARGET: missing input
        // file 'L'` / `LOC: TARGET: error traversing directory ...` lines,
        // then `LOC: N input file(s) ...` (CompletionFunction).
        // An executable's runfiles reach the build through its runfiles
        // tree (RunfilesSupport: the hidden group holds the tree artifact,
        // whose SymlinkTreeAction has them as inputs), so a source
        // directory among them fails that action, not the target.
        let hidden_owners: BTreeSet<usize> = hidden.iter().map(|(owner, _)| *owner).collect();
        let tree_runfiles: BTreeSet<&ConfiguredTargetKey> = hidden_owners
            .into_iter()
            .map(|owner| &owners.keys[owner])
            .filter(|key| {
                let actual = forwarded.get(*key).unwrap_or(&key.label);
                runfiles_nodes.contains_key(&self.configurations.top_key(actual.clone()))
            })
            .collect();
        let mut by_target: Vec<(Label, Vec<InputFailure>)> = Vec::new();
        let targets = requested
            .iter()
            .map(|(key, artifact)| ((key, artifact), false))
            .chain(
                hidden
                    .iter()
                    .map(|(owner, artifact)| ((&owners.keys[*owner], artifact), true)),
            );
        for ((key, artifact), runfile) in targets {
            if artifact.producer.is_some() {
                continue;
            }
            let failure = match missing(&artifact.real) {
                Some(missing_label) => InputFailure::Missing(missing_label),
                None if runfile && tree_runfiles.contains(key) => continue,
                None => match traversal_error(&artifact.real) {
                    Some(message) => InputFailure::InError(message),
                    None => continue,
                },
            };
            match by_target.iter_mut().find(|(seen, _)| *seen == key.label) {
                Some((_, failures)) => failures.push(failure),
                None => by_target.push((key.label.clone(), vec![failure])),
            }
        }
        for (label, failures) in &by_target {
            let location = match self.target(label) {
                Ok(Target::Rule(rule) | Target::Output(rule, _)) => rule.location.clone(),
                _ => None,
            };
            let shown = crate::rule_declarations::canonical_form(label);
            for failure in failures {
                errors.push(located(
                    location.as_deref(),
                    format!("{shown}: {}", failure_text(failure)),
                ));
            }
            errors.push(located(location.as_deref(), summary(failures)));
        }
        // Actions: `LOC: PROGRESS failed: missing input file 'L'` (or the
        // traversal error) lines, then `LOC: PROGRESS failed: N input
        // file(s) ...`. The genrule progress message is `Executing genrule
        // L` (rules/genrule/GenRuleBase.java); custom actions use their
        // action id.
        let rules_by_label = plan
            .iter()
            .map(|(key, rule)| (key.action_owner(top), rule))
            .collect::<BTreeMap<_, _>>();
        // The actions whose inputs fail (--keep_going: they fail, and
        // what depends on them is not built).
        let mut missing_nodes: Vec<String> = Vec::new();
        for node in &nodes {
            // SourceManifestAction has no inputs (SourceManifestAction.java:
            // 168): its source directories are never traversed.
            let paths_only = matches!(
                node.action.extra.transform,
                Some(crate::action::Transform::RunfilesManifest { .. })
            );
            let failures = source_paths_of(node)
                .into_iter()
                .filter_map(|real| match missing(real) {
                    Some(label) => Some(InputFailure::Missing(label)),
                    None if paths_only => None,
                    None => traversal_error(real).map(InputFailure::InError),
                })
                .collect::<Vec<_>>();
            if failures.is_empty() {
                continue;
            }
            missing_nodes.push(node.action.id.clone());
            let owner = node.action.id.split('#').next().unwrap_or_default();
            let rule = rules_by_label.get(owner);
            let progress = match rule {
                Some(rule) if matches!(rule.kind, RuleKind::Genrule) => format!(
                    "Executing genrule {}",
                    crate::rule_declarations::canonical_form(&rule.label)
                ),
                _ => node.action.id.clone(),
            };
            let location = rule.and_then(|rule| rule.location.as_deref());
            for failure in &failures {
                errors.push(located(
                    location,
                    format!("{progress} failed: {}", failure_text(failure)),
                ));
            }
            errors.push(located(
                location,
                format!("{progress} failed: {}", summary(&failures)),
            ));
        }
        if !errors.is_empty() {
            errors.splice(0..0, events);
        }
        // --keep_going: the input failures are execution failures of the
        // actions and targets they name (ArtifactFunction /
        // ActionExecutionFunction errors under keepGoing): those actions and
        // every action above them do not run, the targets they name are not
        // built, everything else is.
        let mut input_unbuilt: Vec<Label> = Vec::new();
        let input_errors = if self.keep_going && !errors.is_empty() {
            let mut failed: HashSet<String> = missing_nodes.into_iter().collect();
            loop {
                let before = failed.len();
                for node in &nodes {
                    if !failed.contains(&node.action.id)
                        && node
                            .all_dependencies()
                            .any(|dependency| failed.contains(&dependency.action_id))
                    {
                        failed.insert(node.action.id.clone());
                    }
                }
                if failed.len() == before {
                    break;
                }
            }
            nodes.retain(|node| !failed.contains(&node.action.id));
            input_unbuilt = by_target.iter().map(|(label, _)| label.clone()).collect();
            std::mem::take(&mut errors)
        } else {
            Vec::new()
        };
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        // --keep_going (docs/KEEP_GOING.md): what each analyzed requested
        // target's completion builds (its requested, hidden and aspect
        // artifacts' producers, the workspace status apart), so execution
        // knows which top-level targets a failed action fails.
        if self.keep_going {
            let mut targets: BTreeMap<&Label, BTreeSet<&str>> = labels
                .iter()
                .map(|label| (label, BTreeSet::new()))
                .collect();
            let artifacts = requested
                .iter()
                .map(|(key, artifact)| (&key.label, artifact))
                .chain(
                    hidden
                        .iter()
                        .map(|(owner, artifact)| (&owners.keys[*owner].label, artifact)),
                )
                .chain(aspect_reports.iter().flat_map(|(_, label, artifacts)| {
                    artifacts.iter().map(move |artifact| (label, artifact))
                }));
            for (label, artifact) in artifacts {
                if let Some((id, _)) = &artifact.producer
                    && !id.starts_with(crate::request::STATUS_ID_PREFIX)
                    && let Some(ids) = targets.get_mut(label)
                {
                    ids.insert(id);
                }
            }
            let errors = self.keep_going_state.take_errors();
            if !errors.is_empty() || !failed_labels.is_empty() {
                crate::observed_fs::taint(crate::observed_fs::Taint::Errors, || {
                    format!(
                        "--keep_going analysis failed for {} target(s)",
                        failed_labels.len()
                    )
                });
            }
            self.keep_going_plan = Some(crate::request::KeepGoingPlan {
                top_level: all_labels.len(),
                errors,
                input_errors,
                unbuilt: input_unbuilt,
                failed: std::mem::take(&mut failed_labels).into_iter().collect(),
                targets: targets
                    .into_iter()
                    .map(|(label, ids)| {
                        (label.clone(), ids.into_iter().map(str::to_owned).collect())
                    })
                    .collect(),
            });
        }
        self.bep_groups = bep_groups;
        let built = (nodes, requested, aspect_reports, target_result_labels.len());
        if self.skip_teardown && !self.execute_build {
            // W7: the process exits right after this --nobuild command
            // (`set_skip_teardown`); the analysis and planning maps are
            // left to the OS instead of being torn down one node at a
            // time. An executing build drops them before execution.
            std::mem::forget((
                plan,
                configured,
                forwarded,
                top_level_aspects,
                (dry, runfiles_info, artifact_index, declared, source_inputs),
                (runfiles_nodes, custom_providers, validation, producers),
                (
                    client_environment,
                    status_client_environment,
                    workspace_status,
                ),
                (
                    provider_keys,
                    delegations,
                    native_output_groups,
                    repositories,
                ),
                (
                    aspect_providers,
                    aspect_target_identities,
                    target_identities,
                    views,
                    input_groups,
                ),
            ));
        }
        Ok(built)
    }
}

/// The runfiles providers a dependent sees: file targets have a
/// SingleExecutableFilesToRunProvider and no RunfilesProvider
/// (analysis/configuredtargets/FileConfiguredTarget.java:85-101). A rule's
/// analyzed providers are shared when they carry their FilesToRunProvider
/// (`with_files_to_run` then keeps them as they are).
fn dependency_runfiles(
    target: &Target,
    files: &[File],
    analyzed: Option<&Arc<crate::runfiles::TargetRunfiles>>,
) -> Arc<crate::runfiles::TargetRunfiles> {
    match target {
        Target::Source(_) | Target::Output(..) => Arc::new(
            files
                .first()
                .cloned()
                .map(crate::runfiles::TargetRunfiles::file)
                .unwrap_or_default(),
        ),
        Target::Rule(_) => match analyzed {
            Some(analyzed) if analyzed.files_to_run.is_some() => Arc::clone(analyzed),
            analyzed => Arc::new(
                analyzed
                    .map(|analyzed| crate::runfiles::TargetRunfiles::clone(analyzed))
                    .unwrap_or_default()
                    .with_files_to_run(files),
            ),
        },
    }
}

/// Copy the files and providers of each delegate key planned so far to the
/// requested key that delegates to it (a rule transition's key).
fn flush_delegations(
    delegations: &mut Vec<(ConfiguredTargetKey, ConfiguredTargetKey)>,
    dry: &mut BTreeMap<ConfiguredTargetKey, Artifacts>,
    custom_providers: &mut BTreeMap<
        ConfiguredTargetKey,
        BTreeMap<String, starlark::values::OwnedFrozenValue>,
    >,
    provider_keys: &mut BTreeMap<ConfiguredTargetKey, Vec<String>>,
    runfiles_info: &mut BTreeMap<ConfiguredTargetKey, Arc<crate::runfiles::TargetRunfiles>>,
) {
    delegations.retain(|(requested, owner)| {
        let Some(files) = dry.get(owner).cloned() else {
            return true;
        };
        dry.insert(requested.clone(), files);
        if let Some(providers) = custom_providers.get(owner).cloned() {
            custom_providers.insert(requested.clone(), providers);
        }
        if let Some(keys) = provider_keys.get(owner).cloned() {
            provider_keys.insert(requested.clone(), keys);
        }
        // N23a's RunfilesProvider/FilesToRunProvider follow the delegate too.
        if let Some(runfiles) = runfiles_info.get(owner).cloned() {
            runfiles_info.insert(requested.clone(), runfiles);
        }
        false
    });
}

/// Marks an error that aborts the analysis of a requested target; the
/// build prefixes `Analysis of target 'L' failed; build aborted: `.
const ANALYSIS_ABORTED: &str = "\u{0}analysis aborted:";

/// `No repository visible as '@R' from main repository` (or `from repository
/// '@@C'`) for a label text naming an apparent repository the context
/// repository does not see (RepositoryFetchFunction.java:149-153,
/// cmdline/RepositoryName.java:251-258).
fn unknown_apparent_repository(text: &str, context: &str, error: &str) -> Option<String> {
    if !error.starts_with("unknown apparent repository") {
        return None;
    }
    let name = text
        .strip_prefix('@')
        .filter(|rest| !rest.starts_with('@'))
        .and_then(|rest| rest.split_once("//"))
        .map(|(name, _)| name)?;
    Some(format!(
        "No repository visible as '@{name}' from {}",
        if context.is_empty() {
            "main repository".to_owned()
        } else {
            format!("repository '@@{context}'")
        }
    ))
}

/// The RecursivePkgFunction order of packages below a directory: sorted
/// directory entries, a directory's subpackages before the directory itself
/// (a STABLE_ORDER nested set iterates postorder).
fn subpackages_first(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = a.split('/').filter(|part| !part.is_empty());
    let mut right = b.split('/').filter(|part| !part.is_empty());
    loop {
        match (left.next(), right.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return crate::native_rules::java_string_cmp(x, y),
            // A descendant comes before its ancestor.
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => return std::cmp::Ordering::Equal,
        }
    }
}

/// TargetSuggester.suggestTargets (packages/TargetSuggester.java:41-140) with
/// SpellChecker.editDistance (net/starlark/java/spelling/SpellChecker.java:33-77).
fn suggest_targets(input: &str, words: &BTreeSet<String>) -> String {
    let lower = input.to_lowercase();
    let max = 5.min(input.encode_utf16().count().div_ceil(2));
    let mut by_distance: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for word in words {
        let candidate = word.to_lowercase();
        if let Some(distance) = starlark::errors::bazel_edit_distance(&lower, &candidate, max) {
            by_distance.entry(distance).or_default().push(word);
        }
    }
    let suggested = if let Some(words) = by_distance.get(&0).or_else(|| by_distance.get(&1)) {
        words.iter().take(10).copied().collect::<Vec<_>>()
    } else {
        let mut suggested = Vec::new();
        for distance in 2..max {
            for word in by_distance.get(&distance).into_iter().flatten() {
                if suggested.len() < 10 {
                    suggested.push(*word);
                }
            }
        }
        suggested
    };
    match suggested.as_slice() {
        [] => String::new(),
        [only] => format!(" (did you mean {only}?)"),
        [init @ .., last] => format!(" (did you mean {}, or {last}?)", init.join(", ")),
    }
}

/// CommonPrerequisiteValidator.generateVisibilityConflictMessage
/// (analysis/CommonPrerequisiteValidator.java:380-405, default
/// --noverbose_visibility_errors) reported by RuleContext.ruleError at the
/// consuming rule (`LOC: in CLASS rule L: `, EventHandlingErrorReporter.java:80-82).
/// `description` is describeTargetWithAliases(WITHOUT_KIND); `is_source`
/// whether the (alias chain's end) target is a source file. Implicit
/// (private Starlark) attributes are attributed to the consumer target here,
/// not to its rule's `.bzl` (divergence, docs/CONFIGURATION.md).
/// `owner` is the defining `.bzl` of an implicit dependency's rule class
/// (CommonPrerequisiteValidator.generateVisibilityConflictMessage,
/// analysis/CommonPrerequisiteValidator.java:376-397: `consumerOrOwnerLocation`).
fn visibility_error(
    rule: &Rule,
    description: &str,
    is_source: bool,
    owner: Option<&Label>,
) -> String {
    located(
        rule.location.as_deref(),
        visibility_message(rule, description, is_source, owner),
    )
}

fn visibility_message(
    rule: &Rule,
    description: &str,
    is_source: bool,
    owner: Option<&Label>,
) -> String {
    let mut message = format!(
        "in {} rule {}: Visibility error:\n{description} is not visible from\ntarget '{}'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility",
        rule.rule_class_name(),
        crate::rule_declarations::canonical_form(&rule.label),
        crate::rule_declarations::canonical_form(owner.unwrap_or(&rule.label)),
    );
    if is_source {
        message.push_str(
            ". To set the visibility of that source file target, use the exports_files() function",
        );
    }
    message
}

/// TargetUtils.formatMissingEdge (packages/TargetUtils.java:394-430) at the
/// consuming rule's location (MissingEdgeError, analysis/producers/MissingEdgeError.java:86-111;
/// DependencyProducer.java:279-290): an explicit dependency appends
/// ` and referenced by 'L'`; an implicit one (label_flag/label_setting's
/// late-bound `:alias`, private Starlark attributes) is `every rule of type
/// KIND implicitly depends upon the target 'D', but this target could not be
/// found because of: MESSAGE`. A missing package is first reported on its
/// own (the package lookup error event). Other errors pass through.
fn missing_edge(rule: &Rule, dependency: &Label, error: String) -> String {
    let package = error.starts_with("no such package '");
    if !package && !error.starts_with("no such target '") {
        return error;
    }
    let explicit = match (&rule.kind, &rule.custom) {
        (RuleKind::Native("label_flag" | "label_setting"), _) => false,
        (_, Some(custom)) => custom.attrs.iter().any(|(name, value)| {
            !name.starts_with('_')
                && crate::package::custom_dependency_labels(&BTreeMap::from([(
                    name.clone(),
                    value.clone(),
                )]))
                .iter()
                .any(|text| Label::parse_canonical(text).as_ref() == Ok(dependency))
        }),
        _ => true,
    };
    let edge = if explicit {
        format!(
            "{error} and referenced by '{}'",
            crate::rule_declarations::canonical_form(&rule.label)
        )
    } else {
        format!(
            "every rule of type {} implicitly depends upon the target '{}', but this target could not be found because of: {error}",
            rule.rule_class_name(),
            crate::rule_declarations::canonical_form(dependency)
        )
    };
    let edge = located(rule.location.as_deref(), edge);
    if package {
        format!("{error}\n{edge}")
    } else {
        edge
    }
}

/// [`located`] for each line of several events.
fn located_lines(location: Option<&str>, messages: &str) -> String {
    messages
        .lines()
        .map(|message| located(location, message.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Event text with Bazel's `LOCATION: ` prefix when the location is known.
/// ToolchainTypeRequirements of canonical `(label, mandatory)` pairs
/// (crate::toolchains::parse_label keeps deferred unknown repositories).
fn requirements(types: &[(String, bool)]) -> Result<Vec<ToolchainTypeRequirement>, String> {
    types
        .iter()
        .map(|(text, mandatory)| {
            Ok(ToolchainTypeRequirement {
                label: crate::toolchains::parse_label(text)?,
                mandatory: *mandatory,
                ignore_if_invalid: false,
            })
        })
        .collect()
}

fn located(location: Option<&str>, message: String) -> String {
    match location {
        Some(location) => format!("{location}: {message}"),
        None => message,
    }
}

/// A genrule with `executable = True` (a nonconfigurable boolean).
fn genrule_is_executable(rule: &Rule) -> bool {
    matches!(rule.kind, RuleKind::Genrule)
        && rule.native.as_deref().is_some_and(|native| {
            matches!(
                native.attrs.get("executable"),
                Some(crate::custom_rule::AttrValue::Bool(true))
            )
        })
}

/// GenRuleBase.create (rules/genrule/GenRuleBase.java:78-86): an executable
/// genrule with several outputs is an attribute error; the missing-`cmd`
/// error (:262-284) is reported with it.
fn genrule_executable_error(rule: &Rule, outputs: usize) -> Option<String> {
    if !genrule_is_executable(rule) || outputs <= 1 {
        return None;
    }
    let shown = crate::rule_declarations::canonical_form(&rule.label);
    let mut errors = format!(
        "in executable attribute of genrule rule {shown}: if genrules produce executables, they are allowed only one output. If you need the executable=1 argument, then you should split this genrule into genrules producing single outputs"
    );
    if rule.cmd.is_none() {
        errors.push_str(&format!(
            "\nin cmd attribute of genrule rule {shown}: missing value for `cmd` attribute, you can also set `cmd_ps` or `cmd_bat` on Windows and `cmd_bash` on other platforms."
        ));
    }
    Some(located_lines(rule.location.as_deref(), &errors))
}

/// RuleContextConstraintSemantics.validateAttributes
/// (analysis/constraints/RuleContextConstraintSemantics.java:354-367), run by
/// RuleConfiguredTargetBuilder.build after the rule's factory or
/// implementation (analysis/RuleConfiguredTargetBuilder.java:104-106,
/// 450-468): an explicit empty `restricted_to` is an attribute error.
fn restricted_to_error(rule: &Rule) -> Result<(), String> {
    let explicit_empty = |target: &crate::native_rules::NativeTarget| matches!(target.attrs.get("restricted_to"), Some(crate::custom_rule::AttrValue::Labels(labels)) if labels.is_empty());
    let empty = rule.native.as_deref().is_some_and(explicit_empty)
        || rule
            .custom
            .as_ref()
            .is_some_and(|custom| explicit_empty(&custom.declaration.builtin.target));
    if empty {
        return Err(located(
            rule.location.as_deref(),
            format!(
                "in restricted_to attribute of {} rule {}: attribute cannot be empty",
                rule.rule_class_name(),
                crate::rule_declarations::canonical_form(&rule.label)
            ),
        ));
    }
    Ok(())
}

fn rule_authority(rule: &Rule) -> PackageId {
    rule.macro_owner
        .as_ref()
        .map(|owner| owner.definition_package.clone())
        .unwrap_or_else(|| rule.label.package_id())
}

fn check_macro_namespace(rule: &Rule, target: &Label) -> Result<(), String> {
    if let Some(owner) = &rule.macro_owner {
        let valid = target.name == owner.name
            || target
                .name
                .strip_prefix(&owner.name)
                .is_some_and(|suffix| suffix.len() > 1 && suffix.starts_with(['_', '-', '.']));
        if !valid {
            return Err(format!(
                "target {target} is outside symbolic macro namespace {}: expected the macro name or its prefix followed by '_', '-', or '.' and a nonempty suffix",
                owner.name
            ));
        }
    }
    Ok(())
}

/// The build's producers of output paths, global across configurations
/// (Bazel's action graph: MapBasedActionGraph, ArtifactPrefixConflicts):
/// one shared [`OutputProducer`] per producing action (or native output).
type Producers = BTreeMap<PathBuf, Arc<OutputProducer>>;

/// The producer of output paths ([`register_output`]).
#[derive(Debug)]
struct OutputProducer {
    /// The owning action-lookup key's display: a configured target key's
    /// label, or `label[config]%aspect` of an aspect application (AspectKey
    /// as ActionLookupKey).
    owner: String,
    /// The owner's label as an action conflict names it
    /// (ActionConflictException's `Label:` line, Label.print), when it is
    /// not `owner` (an aspect application's base target).
    label: Option<String>,
    /// The producing Starlark action's id and owner-independent key
    /// (crate::shared_actions), which Actions.canBeShared may share with an
    /// equal action of another owner; `None` for the native producers
    /// (genrule, runfiles support, workspace status), which are never
    /// shared.
    action: Option<(String, crate::shared_actions::ActionKey)>,
}

impl OutputProducer {
    /// A native producer owned by `owner` (whose label it names).
    fn native(owner: &str) -> Arc<Self> {
        Arc::new(Self {
            owner: owner.to_owned(),
            label: None,
            action: None,
        })
    }

    /// The producer of a registered Starlark action's outputs, owned by
    /// `owner` of `label`.
    fn action(
        owner: &str,
        label: &str,
        registered: &crate::analysis_runtime::RegisteredAction,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner: owner.to_owned(),
            label: (label != owner).then(|| label.to_owned()),
            action: Some((registered.action.id.clone(), registered.key)),
        })
    }

    fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.owner)
    }
}

/// Register `producer` for `output`. Output paths are global across
/// configurations. An exact path another producer already registered is
/// that producer's action when the two can be shared (Actions.canBeShared
/// between owners, MapBasedActionGraph.registerAction
/// actions/MapBasedActionGraph.java:46-63; crate::shared_actions): the
/// first producer's action id is returned, and the caller plans no node
/// for its own action — its outputs are its own Artifacts produced by the
/// first owner's action, which the first target in commit order owns.
/// Otherwise an exact path is Bazel's action conflict (both owners named),
/// and a path below or above another producer's output is Bazel's prefix
/// conflict (Actions.findArtifactPrefixConflicts, both owners named).
fn register_output(
    producers: &mut Producers,
    output: &Path,
    producer: &Arc<OutputProducer>,
) -> Result<Option<String>, String> {
    if let Some(previous) = producers.get(output) {
        let path = output.to_string_lossy();
        let (attempted, first) = (producer.label(), previous.label());
        return match (&producer.action, &previous.action) {
            (Some((_, key)), Some((id, earlier))) => match key.sharing_between_owners(earlier) {
                crate::shared_actions::Sharing::Shared => Ok(Some(id.clone())),
                crate::shared_actions::Sharing::Conflict => Err(
                    crate::shared_actions::conflict_message(&path, attempted, first),
                ),
                crate::shared_actions::Sharing::Unmodelled(reason) => Err(
                    crate::shared_actions::unmodelled_message(&path, attempted, first, reason),
                ),
            },
            _ => Err(crate::shared_actions::conflict_message(
                &path, attempted, first,
            )),
        };
    }
    for ancestor in output.ancestors().skip(1) {
        if let Some(previous) = producers.get(ancestor) {
            return Err(crate::shared_actions::prefix_conflict_message(
                (&ancestor.to_string_lossy(), previous.label()),
                (&output.to_string_lossy(), producer.label()),
            ));
        }
    }
    if let Some((previous_path, previous)) = producers.range(output.to_path_buf()..).next()
        && previous_path.starts_with(output)
    {
        return Err(crate::shared_actions::prefix_conflict_message(
            (&output.to_string_lossy(), producer.label()),
            (&previous_path.to_string_lossy(), previous.label()),
        ));
    }
    producers.insert(output.to_path_buf(), Arc::clone(producer));
    Ok(None)
}

/// The first commit loop of a target's (or aspect application's) Starlark
/// actions: every output registered with its producer ([`register_output`],
/// owner and label `owner`) and indexed as the Artifact of its declared File
/// — produced by the action itself, or, when the action is equal to another
/// owner's producer of its outputs (Actions.canBeShared), by that action:
/// the outputs stay this owner's Artifacts (File.owner is the declaring
/// target, as Bazel keeps each target's own Artifact) and the action plans
/// no node. Returns, per action, whether it is shared. A shared output's
/// `declared` entry (an output-file target's lookup) stays the first
/// owner's unless `predeclared` says it is this owner's output-file target.
fn register_action_outputs(
    actions: &[crate::analysis_runtime::RegisteredAction],
    (owner, label): (&str, &str),
    producers: &mut Producers,
    artifact_index: &mut BTreeMap<File, Artifact>,
    declared: &mut Declared,
    config: ConfigId,
    predeclared: impl Fn(&Path) -> bool,
) -> Result<Vec<bool>, String> {
    let mut shared = Vec::with_capacity(actions.len());
    for registered in actions {
        let producer = OutputProducer::action(owner, label, registered);
        let mut first = None;
        for (index, file) in registered.outputs.iter().enumerate() {
            let exec = PathBuf::from(&file.path);
            let sharing = register_output(producers, &exec, &producer)?;
            // The key covers the outputs: an action shares all of them with
            // one action, or none. A partial share would plan no node for
            // the action and leave its other outputs without a producing
            // action, so it is an internal error in every build.
            if index > 0 && sharing != first {
                let shown = |shared: &Option<String>| {
                    shared
                        .as_ref()
                        .map_or_else(|| "its own".to_owned(), |id| format!("{id}'s"))
                };
                return Err(format!(
                    "internal error: action {} of {label}: output {} would be {} and its first output {} (a shared action shares every output; docs/SHAREABLE_ARTIFACTS.md)",
                    registered.action.id,
                    file.path,
                    shown(&sharing),
                    shown(&first)
                ));
            }
            first = first.or(sharing);
            let artifact = Artifact::new(
                exec.clone(),
                exec.clone(),
                Some((
                    first
                        .clone()
                        .unwrap_or_else(|| registered.action.id.clone()),
                    index,
                )),
                file.owner.clone(),
                file.is_directory,
                registered
                    .build_info
                    .as_ref()
                    .is_some_and(|build_info| build_info.volatile),
            )
            .with_symlink(file.is_symlink);
            verify_declared_file(file, &artifact);
            register_artifact(artifact_index, file, &artifact);
            if first.is_none() || predeclared(&exec) {
                declared.insert((config, exec), artifact);
            } else {
                declared.entry((config, exec)).or_insert(artifact);
            }
        }
        shared.push(first.is_some());
    }
    Ok(shared)
}

/// Whether Bazel plants the execroot entry for a source input. The execroot
/// symlink forest omits every top-level main-repository entry with a
/// `matchingEntry` (buildtool/SymlinkForest.java:425-442 with the main
/// repository's ignored subdirectories `withPrefix(workspace)`,
/// skyframe/SkyframeExecutor.java:3776-3793), so a declared source input
/// below such an entry is absent when the action runs. External repositories
/// are planted separately under `external/` and are unaffected.
/// TargetUtils.getExecutionInfo (packages/TargetUtils.java:53-63, 241-252):
/// the rule's tags that are execution requirements. Starlark actions get
/// them too (TargetUtils.getFilteredExecutionInfo, :271-301, with
/// --incompatible_allow_tags_propagation, default true).
pub(crate) fn rule_execution_info(rule: &Rule) -> BTreeSet<String> {
    let tags = match (&rule.native, &rule.custom) {
        (Some(native), _) => native.tags.clone(),
        (None, Some(custom)) => custom.declaration.builtin.target.tags.clone(),
        (None, None) => Vec::new(),
    };
    tags.into_iter()
        .filter(|tag| legal_execution_info_key(tag))
        .collect()
}

/// TargetUtils.legalExecInfoKeys (packages/TargetUtils.java:53-63): the
/// tags that are execution requirements, and the `execution_requirements`
/// keys a Starlark action keeps (TargetUtils.filter, :308-314).
pub(crate) fn legal_execution_info_key(key: &str) -> bool {
    [
        "block-",
        "requires-",
        "no-",
        "supports-",
        "disable-",
        "cpu:",
        "resources:",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
        || key == "local"
        || key == "worker-key-mnemonic"
}

/// A native rule's explicit boolean attribute (False when unset).
fn native_bool(rule: &Rule, name: &str) -> bool {
    rule.native.as_ref().is_some_and(|native| {
        matches!(
            native.attrs.get(name),
            Some(crate::custom_rule::AttrValue::Bool(true))
        )
    })
}

fn planted(artifact: &Artifact, main_ignored: &IgnoredSubdirectories) -> bool {
    if artifact.producer.is_some()
        || Label::parse_canonical(&artifact.owner).is_ok_and(|l| !l.repository.is_empty())
    {
        return true;
    }
    match artifact.exec.components().next() {
        Some(Component::Normal(top)) => main_ignored
            .matching_entry(&top.to_string_lossy())
            .is_none(),
        _ => true,
    }
}

/// Source (non-generated) input paths of an action, deduplicated in order.
fn source_paths(inputs: &[Artifact]) -> Vec<PathBuf> {
    let mut seen: HashSet<&Path> = HashSet::new();
    inputs
        .iter()
        .filter(|artifact| artifact.producer.is_none() && seen.insert(&artifact.real))
        .map(|artifact| artifact.real.to_path_buf())
        .collect()
}

/// `PERF-GROUPS` (BAZEL_RS_TIMING, PA-1): the planned actions holding shared
/// input groups (`holders`, `refs` group references), the distinct groups
/// and their entries, the entries the holders would list without sharing
/// (`listed_entries`), and the actions whose trees are listed instead
/// (`listed_tree_actions`, crate::scheduler::groups_apart).
fn report_input_groups(nodes: &[PlannedAction], listed_tree_actions: usize) {
    let entries = |group: &scheduler::InputGroup| {
        group.inputs.len() + group.dependencies.len() + group.empty_files.len()
    };
    let mut distinct: HashMap<*const scheduler::InputGroup, usize> = HashMap::new();
    let (mut holders, mut refs, mut listed) = (0usize, 0usize, 0usize);
    for node in nodes.iter().filter(|node| !node.groups.is_empty()) {
        holders += 1;
        for group in &node.groups {
            refs += 1;
            listed += entries(group);
            distinct.insert(Arc::as_ptr(group), entries(group));
        }
    }
    // The diagnostics stream (crate::snapshot::diag), like PERF-STAGE: never
    // part of what planning printed, which a snapshot hit replays.
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-GROUPS nodes={} holders={holders} refs={refs} distinct={} distinct_entries={} listed_entries={listed} listed_tree_actions={listed_tree_actions}",
        nodes.len(),
        distinct.len(),
        distinct.values().sum::<usize>(),
    ));
}

/// The source inputs of one planned action (ActionPlanning::source_inputs):
/// its own, then its shared input groups' (PA-1), which every action using
/// the group holds instead of a copy.
#[derive(Default)]
struct SourceInputs {
    /// [`source_paths`] of the action's own inputs, kept only when they are
    /// not the planned action's own listed sources (MEM2,
    /// [`SourceInputs::planned`]): every action kept a second copy of its
    /// source paths (~137 MB on ~/code's `bworker/wcb/...`) although they
    /// almost always are exactly its planned source inputs.
    own: Option<Vec<PathBuf>>,
    groups: Vec<Arc<scheduler::InputGroup>>,
}

/// A tool runfiles tree an action of the build used and its shared input
/// group (PA-1, ActionPlanning::input_groups).
struct ExpandedTree {
    tree: Arc<crate::analysis_runtime::ToolRunfilesTree>,
    group: Arc<scheduler::InputGroup>,
}

impl SourceInputs {
    /// The source inputs of `planned`, planned from `inputs` whose
    /// [`source_paths`] are `own`, with its shared `groups`. `own` is
    /// dropped when it equals what [`SourceInputs::own`] derives from the
    /// planned action (compared here, so the derivation is exact by
    /// construction: an unplanted source or an execution-path duplicate
    /// with another real path keeps the list).
    fn planned(
        own: Vec<PathBuf>,
        planned: &PlannedAction,
        groups: Vec<Arc<scheduler::InputGroup>>,
    ) -> Self {
        let derived = planned_source_paths(planned);
        let own = (!own.iter().map(PathBuf::as_path).eq(derived)).then_some(own);
        SourceInputs { own, groups }
    }

    /// [`source_paths`] of the action's own inputs.
    fn own<'a>(&'a self, planned: &'a PlannedAction) -> Vec<&'a Path> {
        match &self.own {
            Some(own) => own.iter().map(PathBuf::as_path).collect(),
            None => {
                let mut seen: HashSet<&Path> = HashSet::new();
                planned
                    .action
                    .inputs
                    .iter()
                    .map(|(real, _)| real.as_path())
                    .filter(|real| seen.insert(real))
                    .collect()
            }
        }
    }

    /// Every source path of the action's listed inputs, deduplicated in
    /// order: [`source_paths`] over the own inputs followed by the groups'.
    fn listed<'a>(&'a self, planned: &'a PlannedAction) -> Vec<&'a Path> {
        let mut paths = self.own(planned);
        if self.groups.is_empty() {
            return paths;
        }
        let mut seen: HashSet<&Path> = paths.iter().copied().collect();
        for group in &self.groups {
            for path in &group.sources {
                if seen.insert(path) {
                    paths.push(path);
                }
            }
        }
        paths
    }
}

/// The real paths of a planned action's own source inputs, deduplicated
/// in order: what [`source_paths`] gives for its inputs whenever every
/// source is planted and no two sources share an execution path.
fn planned_source_paths(planned: &PlannedAction) -> impl Iterator<Item = &Path> {
    let mut seen: HashSet<&Path> = HashSet::new();
    planned
        .action
        .inputs
        .iter()
        .map(|(real, _)| real.as_path())
        .filter(move |real| seen.insert(real))
}

/// The artifacts of a tool's runfiles tree staged at their tree paths
/// (SpawnInputExpander.addSingleRunfilesTreeToInputs,
/// exec/SpawnInputExpander.java:87-135; N23f) and the tree paths of its
/// empty files (parameter-file-like inputs without content,
/// VirtualActionInput.EMPTY_MARKER), in the tree's order. `resolve` is the
/// build's artifact lookup ([`Engine::indexed_artifact`]).
fn runfiles_tree_entries(
    tree: &crate::analysis_runtime::ToolRunfilesTree,
    owner: &str,
    resolve: &mut dyn FnMut(&File) -> Result<Option<Artifact>, String>,
) -> Result<(Vec<Artifact>, Vec<PathBuf>), String> {
    let mut artifacts = Vec::new();
    let mut empty_files = Vec::new();
    for (path, file) in tree.mapping() {
        let destination = Path::new(tree.directory()).join(path);
        match file {
            Some(file) => {
                let mut artifact = resolve(file)?.ok_or_else(|| {
                    format!(
                        "{owner}: tool runfile has no declared artifact: {}",
                        file.path
                    )
                })?;
                artifact.make_mut().exec = destination.into();
                artifacts.push(artifact);
            }
            None => empty_files.push(destination),
        }
    }
    Ok((artifacts, empty_files))
}

/// The shared input group of a tool's runfiles tree (PA-1): its entries
/// ([`runfiles_tree_entries`]) as [`plan_action`] lists inputs — a
/// generated artifact as a dependency, a planted source as a source input,
/// constant-metadata and tree artifacts marked — plus every source path and
/// whether its namespace is valid on its own
/// (crate::scheduler::group_namespace_valid). A tree's destinations are
/// distinct (one per tree path), so every entry is listed.
fn runfiles_input_group(
    root: &Path,
    artifacts: Vec<Artifact>,
    empty_files: Vec<PathBuf>,
    main_ignored: &IgnoredSubdirectories,
) -> scheduler::InputGroup {
    let tree_inputs: Vec<PathBuf> = artifacts
        .iter()
        .filter(|artifact| artifact.directory)
        .map(|artifact| artifact.exec.to_path_buf())
        .collect();
    let symlink_inputs: Vec<PathBuf> = artifacts
        .iter()
        .filter(|artifact| artifact.file.is_symlink)
        .map(|artifact| artifact.exec.to_path_buf())
        .collect();
    let namespace = scheduler::group_namespace_valid(
        root,
        artifacts
            .iter()
            .map(|artifact| artifact.exec.as_path())
            .chain(empty_files.iter().map(PathBuf::as_path))
            .collect(),
    );
    let sources = source_paths(&artifacts);
    let mut inputs = Vec::new();
    let mut dependencies = Vec::new();
    let mut constant_metadata = Vec::new();
    for artifact in artifacts {
        if artifact.constant_metadata {
            constant_metadata.push(artifact.exec.to_path_buf());
        }
        if let Some((action_id, output_index)) = &artifact.producer {
            dependencies.push(Dependency {
                action_id: action_id.clone(),
                output_index: *output_index,
                destination: artifact.exec.clone(),
            });
        } else if planted(&artifact, main_ignored) {
            inputs.push((artifact.real.clone(), artifact.exec.clone()));
        }
    }
    // A tree input is listed (a planted source or a generated input), as
    // validate_structure requires of the executed action (the predicate a
    // decoded group's flag is recomputed with, InputGroup::listed_valid).
    let valid = namespace
        && scheduler::tree_inputs_listed(&inputs, &dependencies, &tree_inputs)
        && scheduler::tree_inputs_listed(&inputs, &dependencies, &symlink_inputs);
    scheduler::InputGroup {
        root: root.to_path_buf(),
        inputs,
        dependencies,
        tree_inputs,
        symlink_inputs,
        constant_metadata,
        empty_files,
        sources,
        valid,
    }
}

/// The planning state one registered Starlark action's node is committed
/// into ([`Engine::commit_action`]): a rule commit's and an aspect
/// application's.
struct ActionCommit<'a> {
    artifact_index: &'a mut BTreeMap<File, Artifact>,
    workspace_status: &'a mut Option<WorkspaceStatusPlan>,
    nodes: &'a mut Vec<PlannedAction>,
    producers: &'a mut Producers,
    source_inputs: &'a mut BTreeMap<String, SourceInputs>,
    client_environment: &'a std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    status_client_environment: &'a std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    input_groups: &'a mut HashMap<u64, ExpandedTree>,
    listed_tree_actions: &'a mut usize,
}

impl ActionCommit<'_> {
    /// The Artifact `file` resolves to ([`Engine::indexed_artifact`]).
    fn artifact(&mut self, engine: &Engine, file: &File) -> Result<Option<Artifact>, String> {
        engine.indexed_artifact(
            file,
            self.artifact_index,
            self.workspace_status,
            self.nodes,
            self.producers,
            self.client_environment,
            self.status_client_environment,
        )
    }
}

/// The planning state a runfiles support's nodes are registered in, and
/// what registering the workspace status files on first need takes
/// ([`Engine::ensure_workspace_status`]).
struct RunfilesPlanning<'a> {
    artifact_index: &'a mut BTreeMap<File, Artifact>,
    declared: &'a mut Declared,
    producers: &'a mut Producers,
    nodes: &'a mut Vec<PlannedAction>,
    source_inputs: &'a mut BTreeMap<String, SourceInputs>,
    workspace_status: &'a mut Option<WorkspaceStatusPlan>,
    client_environment: &'a std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    status_client_environment: &'a std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
}

impl RunfilesPlanning<'_> {
    /// The Artifact `file` resolves to ([`Engine::indexed_artifact`]).
    fn artifact(&mut self, engine: &Engine, file: &File) -> Result<Option<Artifact>, String> {
        engine.indexed_artifact(
            file,
            self.artifact_index,
            self.workspace_status,
            self.nodes,
            self.producers,
            self.client_environment,
            self.status_client_environment,
        )
    }

    /// Register the only output of a native runfiles node.
    fn output(
        &mut self,
        key: &ConfiguredTargetKey,
        id: &str,
        file: &File,
    ) -> Result<Artifact, String> {
        let exec = PathBuf::from(&file.path);
        register_output(
            self.producers,
            &exec,
            &OutputProducer::native(&key.label.to_string()),
        )?;
        let artifact = Artifact::new(
            exec.clone(),
            exec.clone(),
            Some((id.to_owned(), 0)),
            file.owner.clone(),
            false,
            false,
        );
        register_artifact(self.artifact_index, file, &artifact);
        self.declared.insert((key.config, exec), artifact.clone());
        Ok(artifact)
    }
}

/// `file` with `suffix` appended to its paths (RunfilesSupport's derived
/// artifacts next to the executable).
fn suffixed_file(file: &File, suffix: &str) -> File {
    File::new(FileData {
        is_directory: false,
        is_symlink: false,
        path: format!("{}{suffix}", file.path),
        short_path: format!("{}{suffix}", file.short_path),
        owner: file.owner.clone(),
        is_source: false,
    })
}

impl Engine {
    /// RunfilesSupport.create (analysis/RunfilesSupport.java:280-347) for a
    /// target with runfiles support: RepoMappingManifestAction (a native
    /// write), SourceManifestAction (Transform::RunfilesManifest, with
    /// --build_runfile_manifests) and SymlinkTreeAction/RunfilesTreeAction
    /// (Transform::RunfilesTree). The tree node is always planned (`run`
    /// creates the tree without --build_runfile_links, RunCommand.java:996-1040);
    /// only reachable nodes execute.
    #[allow(clippy::too_many_arguments)]
    fn plan_runfiles_support(
        &self,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        target: &crate::runfiles::TargetRunfiles,
        transitive: &BTreeSet<String>,
        action_owner: &str,
        main_ignored: &IgnoredSubdirectories,
        mut planning: RunfilesPlanning<'_>,
    ) -> Result<Option<RunfilesNodes>, String> {
        let Some(files_to_run) = &target.files_to_run else {
            return Ok(None);
        };
        let (Some(runfiles), Some(executable), Some(repo_mapping)) = (
            files_to_run.runfiles(),
            files_to_run.executable(),
            files_to_run.repo_mapping_manifest(),
        ) else {
            return Ok(None);
        };
        let options = self.runfiles_options;
        let lookup =
            |planning: &mut RunfilesPlanning<'_>, file: &File| -> Result<Artifact, String> {
                planning.artifact(self, file)?.ok_or_else(|| {
                    format!(
                        "{}: runfiles file has no declared artifact identity: {}",
                        rule.label, file.path
                    )
                })
            };
        let executable_artifact = lookup(&mut planning, executable)?;
        // RepoMappingManifestAction.
        let repositories = &self.repositories;
        let content = crate::runfiles_tree::repo_mapping_manifest(
            runfiles,
            transitive,
            &|repository: &str| repositories.mapping(repository),
            options.compact_repo_mapping,
        )?;
        let id = format!("{action_owner}#repo_mapping");
        planning.nodes.push(PlannedAction {
            action: Action {
                id: id.clone(),
                command: String::new(),
                inputs: Vec::new(),
                outputs: vec![PathBuf::from(&repo_mapping.path)],
                write: Some(crate::action::WriteAction {
                    content,
                    executable: false,
                }),
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: false,
                extra: crate::action::ActionExtra::default(),
                ..Action::default()
            },
            dependencies: Vec::new(),
            deferred: None,
            groups: Vec::new(),
        });
        let repo_mapping_artifact = planning.output(key, &id, repo_mapping)?;
        // The tree mapping; conflicts are reported by the manifest action.
        let mut conflicts = Vec::new();
        let mapping = runfiles.runfiles_inputs(Some(repo_mapping), Some(&mut conflicts));
        let mut inputs: Vec<Artifact> = Vec::new();
        // One copy of each runfiles path, shared by the manifest and tree
        // actions' entries and the target's runfiles entries (MEM4).
        let mut entries: Vec<(Arc<str>, Option<SharedPath>)> = Vec::new();
        let mut mapped: Vec<(Arc<str>, Option<Artifact>)> = Vec::new();
        for (path, file) in mapping {
            let path: Arc<str> = path.into();
            match file {
                Some(file) => {
                    let artifact = if file == *repo_mapping {
                        repo_mapping_artifact.clone()
                    } else {
                        lookup(&mut planning, &file)?
                    };
                    entries.push((path.clone(), Some(artifact.exec.clone())));
                    mapped.push((path, Some(artifact.clone())));
                    inputs.push(artifact);
                }
                None => {
                    entries.push((path.clone(), None));
                    mapped.push((path, None));
                }
            }
        }
        let entries: crate::action::RunfilesEntries = entries.into();
        let action = |id: &str, outputs: Vec<PathBuf>, transform| Action {
            id: id.to_owned(),
            command: String::new(),
            inputs: Vec::new(),
            outputs,
            write: None,
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            shell_strict: false,
            extra: crate::action::ActionExtra {
                transform: Some(transform),
                ..crate::action::ActionExtra::default()
            },
            ..Action::default()
        };
        // Native transforms inherit no client environment (crate::action_env).
        let no_client_environment = std::cell::OnceCell::new();
        // SourceManifestAction: the input manifest.
        let input_manifest = suffixed_file(executable, ".runfiles_manifest");
        let manifest_artifact = if options.build_runfile_manifests {
            let id = format!("{action_owner}#runfiles_manifest");
            let planned = plan_action(
                action(
                    &id,
                    vec![PathBuf::from(&input_manifest.path)],
                    crate::action::Transform::RunfilesManifest {
                        entries: entries.clone(),
                    },
                ),
                inputs.clone(),
                main_ignored,
                &no_client_environment,
            )?;
            planning.source_inputs.insert(
                id.clone(),
                SourceInputs::planned(source_paths(&inputs), &planned, Vec::new()),
            );
            planning.nodes.push(planned);
            Some(planning.output(key, &id, &input_manifest)?)
        } else {
            None
        };
        // SymlinkTreeAction + RunfilesTreeAction. Without manifests (`run`
        // then fails, RunCommand.java:503-513) the input manifest is still
        // needed by the tree node, so the tree is only planned with them.
        let tree_file = suffixed_file(executable, ".runfiles/MANIFEST");
        let tree_id = format!("{action_owner}#runfiles_tree");
        let tree = match &manifest_artifact {
            Some(manifest) => {
                let mut tree_inputs = inputs.clone();
                tree_inputs.push(manifest.clone());
                tree_inputs.push(executable_artifact.clone());
                let planned = plan_action(
                    action(
                        &tree_id,
                        vec![PathBuf::from(&tree_file.path)],
                        crate::action::Transform::RunfilesTree {
                            entries,
                            manifest: manifest.exec.to_path_buf(),
                            executable: Some(executable_artifact.exec.to_path_buf()),
                            create: options.enable_runfiles,
                        },
                    ),
                    tree_inputs.clone(),
                    main_ignored,
                    &no_client_environment,
                )?;
                planning.source_inputs.insert(
                    tree_id.clone(),
                    SourceInputs::planned(source_paths(&tree_inputs), &planned, Vec::new()),
                );
                planning.nodes.push(planned);
                Some(planning.output(key, &tree_id, &tree_file)?)
            }
            None => None,
        };
        Ok(Some(RunfilesNodes {
            tree,
            manifest: manifest_artifact,
            repo_mapping: repo_mapping_artifact,
            executable: executable_artifact,
            entries: mapped,
            conflicts,
            conflict_error: runfiles.conflict_error(),
            location: rule.location.clone(),
        }))
    }

    /// Whether any registered action of a target needs the workspace status
    /// artifacts registered: an action input of a status file
    /// (ctx.info_file/ctx.version_file) or an
    /// actions.transform_{version,info}_file registration.
    fn workspace_status_needed(registered: &[crate::analysis_runtime::RegisteredAction]) -> bool {
        registered.iter().any(|action| {
            action
                .inputs
                .iter()
                .any(crate::analysis_runtime::is_workspace_status_file)
                || action.build_info.is_some()
        })
    }

    /// Plan the ONE BazelWorkspaceStatusAction of this build (N63
    /// follow-up), register its two artifacts as the status Files'
    /// (`artifact_index`, W1b) and return them, stable first. Bazel has
    /// one action with both outputs (BazelWorkspaceStatusModule.java
    /// :87-98, outputs ImmutableSet.of(stableStatus, volatileStatus)) and
    /// adds both artifacts to artifactsToBuild unconditionally on every
    /// executing build (BuildView.createResult :647-650,675;
    /// WorkspaceStatusAction.executeUnconditionally/isVolatile
    /// :186-194), so this runs once per build/test/run invocation that
    /// executes — regardless of consumers — and never under --nobuild
    /// (no execution phase) or on `query` (no BuildView at all). The
    /// computation itself is OnceLock-guarded (computed exactly once per
    /// invocation, also under N62 parallel planning).
    ///
    /// The native executor has no two-output write transform, so the one
    /// logical action is two write nodes sharing the computed status: the
    /// stable node (content-keyed: a cache hit while the stable status is
    /// unchanged, maybeUpdateContent :205-209) and the volatile node
    /// (content-keyed on the volatile bytes but constant metadata, so
    /// consumers never re-key on the changing timestamp). A failed status
    /// computation leaves ONE failing node (volatile-status.txt) — Bazel's
    /// one action fails once — so the build reports a single
    /// `Failed to determine workspace status: ...` line. Since the
    /// plan/execute split (L2-W4) both nodes are planned as placeholders
    /// (Transform::WorkspaceStatus) whatever the status, and the planned
    /// request's finalize (crate::request) writes the status into them or
    /// lays the failure out.
    #[allow(clippy::too_many_arguments)]
    fn ensure_workspace_status(
        &self,
        plan: &mut Option<WorkspaceStatusPlan>,
        nodes: &mut Vec<PlannedAction>,
        producers: &mut Producers,
        #[allow(unused_variables)] client_environment: &std::cell::OnceCell<
            BTreeMap<String, std::ffi::OsString>,
        >,
        status_client_environment: &std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
        artifact_index: &mut BTreeMap<File, Artifact>,
    ) -> Result<[Artifact; 2], String> {
        if self.cquery_capture.is_some() {
            let artifacts = [
                Artifact::workspace_status("stable-status.txt", None, false),
                Artifact::workspace_status("volatile-status.txt", None, true),
            ];
            for artifact in &artifacts {
                register_artifact(artifact_index, &artifact.file, artifact);
            }
            return Ok(artifacts);
        }
        if !self.execute_build {
            // --nobuild: no execution phase, no status action. Consumers
            // of the status files still resolve their artifacts below
            // (ctx.info_file is an analysis-time File value).
            if plan.is_none() {
                let client_env =
                    status_client_environment.get_or_init(crate::action_env::client_environment);
                let computed = self.workspace_status_computed(client_env).clone_status();
                *plan = Some(WorkspaceStatusPlan {
                    computed,
                    stable: Artifact::workspace_status("stable-status.txt", None, false),
                    volatile: Artifact::workspace_status("volatile-status.txt", None, true),
                });
            }
        } else if plan.is_none() {
            let client_env =
                status_client_environment.get_or_init(crate::action_env::client_environment);
            // The status is computed here, during planning, as before the
            // plan/execute split: the status command's stderr is the
            // build's stderr at this point, and build-info callbacks read
            // the result (resolve_build_info). The planned nodes are
            // placeholders (L2-W4): a planned request holds no status, its
            // finalize writes this one (crate::request; a failed status
            // becomes ONE failing #volatile node there, which the stable
            // file's consumers then depend on).
            let computed = self.workspace_status_computed(client_env);
            let owner = "@bazel_tools//tools:internal_platform";
            // The build-data-root execution paths of the status files
            // (SkyframeExecutor.java:1474-1496: bazel-out/stable-status.txt
            // and bazel-out/volatile-status.txt).
            let stable_path = PathBuf::from("bazel-out/stable-status.txt");
            let volatile_path = PathBuf::from("bazel-out/volatile-status.txt");
            let producer = OutputProducer::native(owner);
            register_output(producers, &stable_path, &producer)?;
            register_output(producers, &volatile_path, &producer)?;
            let placeholder = |id: &str, output: PathBuf, volatile: bool| -> PlannedAction {
                PlannedAction {
                    action: Action {
                        id: id.to_owned(),
                        outputs: vec![output],
                        extra: crate::action::ActionExtra {
                            transform: Some(crate::action::Transform::WorkspaceStatus { volatile }),
                            mnemonic: "BazelWorkspaceStatusAction".to_owned(),
                            ..crate::action::ActionExtra::default()
                        },
                        ..Action::default()
                    },
                    dependencies: Vec::new(),
                    deferred: None,
                    groups: Vec::new(),
                }
            };
            nodes.push(placeholder(
                crate::request::STABLE_STATUS_ID,
                stable_path,
                false,
            ));
            nodes.push(placeholder(
                crate::request::VOLATILE_STATUS_ID,
                volatile_path,
                true,
            ));
            // The planned status artifacts carry the status Files
            // (Artifact::workspace_status, 1a91e19), produced by the
            // placeholders; a failed status keeps them: finalize redirects
            // the stable file's consumers to the one failing #volatile node.
            *plan = Some(WorkspaceStatusPlan {
                computed: computed.clone_status(),
                stable: Artifact::workspace_status(
                    "stable-status.txt",
                    Some((crate::request::STABLE_STATUS_ID.to_owned(), 0)),
                    false,
                ),
                volatile: Artifact::workspace_status(
                    "volatile-status.txt",
                    Some((crate::request::VOLATILE_STATUS_ID.to_owned(), 0)),
                    true,
                ),
            });
        }
        let planned = plan.as_ref().expect("just planned");
        for (name, artifact) in [
            ("stable-status.txt", &planned.stable),
            ("volatile-status.txt", &planned.volatile),
        ] {
            // The File ctx.info_file / ctx.version_file hold is the status
            // artifact's own File (Artifact::workspace_status): the one File
            // every view hands out, as for a rule's declared output.
            let file = crate::analysis_runtime::workspace_status_file(name);
            verify_declared_file(&file, artifact);
            register_artifact(artifact_index, &file, artifact);
        }
        Ok([planned.stable.clone(), planned.volatile.clone()])
    }

    /// The Artifact `file` resolves to in the build's one index (W1b), or
    /// `None` when it is not registered. A workspace status File that no
    /// target registered yet (no action input or DefaultInfo file of its
    /// own needed it) is registered here, on the lookup that reaches it,
    /// and looked up again: a status file in a runfiles tree, an output
    /// group or an aspect's outputs resolves — to the planned status
    /// artifact, or the bare one under --nobuild — whichever other targets
    /// the build has, as Bazel's status artifacts always exist.
    #[allow(clippy::too_many_arguments)]
    fn indexed_artifact(
        &self,
        file: &File,
        artifact_index: &mut BTreeMap<File, Artifact>,
        workspace_status: &mut Option<WorkspaceStatusPlan>,
        nodes: &mut Vec<PlannedAction>,
        producers: &mut Producers,
        client_environment: &std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
        status_client_environment: &std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
    ) -> Result<Option<Artifact>, String> {
        if let Some(artifact) = artifact_index.get(file) {
            return Ok(Some(artifact.clone()));
        }
        if !crate::analysis_runtime::is_workspace_status_file(file) {
            return Ok(None);
        }
        self.ensure_workspace_status(
            workspace_status,
            nodes,
            producers,
            client_environment,
            status_client_environment,
            artifact_index,
        )?;
        Ok(artifact_index.get(file).cloned())
    }

    /// The content of a `py_internal.create_repo_mapping_manifest` write
    /// (RepoMappingManifestAction.newDeterministicWriter): the mappings of
    /// the target's transitive repositories relevant to the runfiles, like
    /// the runfiles support's `_repo_mapping` (crate::runfiles_tree).
    fn resolve_repo_mapping(
        &self,
        transitive: &BTreeSet<String>,
        registered: &mut crate::analysis_runtime::RegisteredAction,
    ) -> Result<(), String> {
        let Some(runfiles) = registered.repo_mapping.take() else {
            return Ok(());
        };
        let repositories = &self.repositories;
        let content = crate::runfiles_tree::repo_mapping_manifest(
            &runfiles,
            transitive,
            &|repository: &str| repositories.mapping(repository),
            self.runfiles_options.compact_repo_mapping,
        )?;
        match registered.action.write.as_mut() {
            Some(write) => write.content = content,
            None => {
                return Err(format!(
                    "internal error: the repo mapping manifest {:?} is no write",
                    registered.action.outputs
                ));
            }
        }
        Ok(())
    }

    /// Resolve one actions.transform_{version,info}_file registration
    /// (N63): evaluate the Starlark translation callback against the
    /// build's workspace status and rewrite the action into a
    /// TemplateKeyed expansion (BuildInfoFileWriteAction.execute
    /// :121-176 parses the status file, calls the callback and expands
    /// the template; the native computes the same substitutions during
    /// planning). The key is Bazel's computeKey (:189-197) — GUID,
    /// isVolatile, the callback's bzl digest — plus the template input's
    /// digest, EXCLUDING the substitutions: for the volatile variant the
    /// status input is constant metadata
    /// (StarlarkActionFactory.transformBuildInfoFile :510-519), so a
    /// changed timestamp produces new substitutions under the SAME key
    /// and the cached volatile_file.h is not rewritten (the volatile
    /// output is itself constant metadata, so consumers do not re-key
    /// either). The stable variant keys its status input regularly: a
    /// changed stable key re-keys and re-executes it, like Bazel's
    /// regular derived artifact.
    fn resolve_build_info(
        &self,
        plan: &Option<WorkspaceStatusPlan>,
        registered: &mut crate::analysis_runtime::RegisteredAction,
    ) -> Result<(), String> {
        let Some(build_info) = registered.build_info.take() else {
            return Ok(());
        };
        let computed = plan
            .as_ref()
            .map(|plan| &plan.computed)
            .ok_or_else(|| "build info action without a workspace status plan".to_owned())?;
        let status = match computed {
            crate::workspace_status::ComputedWorkspaceStatus::Ok(status) => status,
            crate::workspace_status::ComputedWorkspaceStatus::Failed(error) => {
                registered.action.extra.transform = Some(crate::action::Transform::Unsupported {
                    message: format!(
                        "TranslateBuildInfo {} failed: Failed to determine workspace status: {}",
                        if build_info.volatile {
                            "volatile-status.txt"
                        } else {
                            "stable-status.txt"
                        },
                        error.0
                    ),
                });
                return Ok(());
            }
        };
        let values = if build_info.volatile {
            &status.volatile_bytes
        } else {
            &status.stable_bytes
        };
        let parsed = crate::workspace_status::parse_values(values);
        // BuildInfoFileWriteAction.execute :128-141: one positional dict
        // argument, a fresh transient Starlark thread
        // ("translate_build_info_file").
        let substitutions = starlark::environment::Module::with_temp_heap(|module| {
            let mut eval = starlark::eval::Evaluator::new(&module);
            let dict = module.heap().alloc(starlark::values::dict::AllocDict(
                parsed.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            ));
            let function = module
                .heap()
                .access_owned_frozen_value(&build_info.function);
            let returned = eval
                .eval_function(function, &[dict], &[])
                .map_err(|error| error.to_string())?;
            let dict = starlark::values::dict::DictRef::from_value(returned)
                .ok_or("the transform_func callback must return a dict of strings to strings")?;
            let mut pairs = Vec::new();
            for (key, value) in dict.iter() {
                let key = key.unpack_str().ok_or(
                    "the transform_func callback must return a dict of strings to strings",
                )?;
                let value = value.unpack_str().ok_or(
                    "the transform_func callback must return a dict of strings to strings",
                )?;
                pairs.push((
                    crate::bazel_bytes::internal_bytes(key).into_owned(),
                    crate::bazel_bytes::internal_bytes(value).into_owned(),
                ));
            }
            Ok::<_, String>(pairs)
        })?;
        // The template is the registration's second input (the status file
        // first, like BuildInfoFileWriteAction's inputs).
        let template = registered
            .inputs
            .get(1)
            .map(|file| PathBuf::from(&file.path))
            .ok_or_else(|| "build info action has no template input".to_owned())?;
        // The keyed part of the callback result: for the volatile variant,
        // every substitution except the timestamp one(s) — BUILD_TIMESTAMP
        // (and the FORMATTED_DATE-derived values of the Java callback)
        // change every build and must not re-key the action (BLOCKER 2);
        // the SCM keys are what actually distinguish volatile outputs.
        // Stable: the whole result (also keyed regularly via the status
        // input; identical either way).
        let keyed_substitutions: Vec<(Vec<u8>, Vec<u8>)> = if build_info.volatile {
            substitutions
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_slice(),
                        b"{BUILD_TIMESTAMP}"
                            | b"{build.time}"
                            | b"{build.timestamp}"
                            | b"{build.timestamp.as.int}"
                    )
                })
                .cloned()
                .collect()
        } else {
            substitutions.clone()
        };
        registered.action.extra.transform = Some(crate::action::Transform::TemplateKeyed {
            template,
            substitutions,
            keyed_substitutions,
            executable: false,
            volatile: build_info.volatile,
            callback_digest: build_info.callback_digest.clone(),
        });
        Ok(())
    }
}

/// Every artifact of `inputs` listed in `action` at its execution path,
/// the way [`plan_action`] validates them; tree artifact inputs are staged
/// and keyed as directories.
fn list_artifacts(action: &mut Action, inputs: &[Artifact]) {
    action.inputs = inputs
        .iter()
        .map(|artifact| (artifact.real.clone(), artifact.exec.clone()))
        .collect();
    action.extra.tree_inputs = inputs
        .iter()
        .filter(|artifact| artifact.directory)
        .map(|artifact| artifact.exec.to_path_buf())
        .collect();
    // Unresolved symlink artifacts are read and staged as symlinks
    // (crate::action::ActionExtra::symlink_inputs).
    action.extra.symlink_inputs = inputs
        .iter()
        .filter(|artifact| artifact.file.is_symlink)
        .map(|artifact| artifact.exec.to_path_buf())
        .collect();
}

fn plan_action(
    mut action: Action,
    inputs: Vec<Artifact>,
    main_ignored: &IgnoredSubdirectories,
    client_environment: &std::cell::OnceCell<BTreeMap<String, std::ffi::OsString>>,
) -> Result<PlannedAction, String> {
    // The inherited values are resolved here, keyed and fixed for the
    // execution (ActionExecutionFunction.java:221-242 reads them per action).
    if !action.extra.inherited_environment.is_empty() {
        action.extra.client_environment = crate::action_env::ActionEnvironment::resolve_inherited(
            &action.extra.inherited_environment,
            client_environment.get_or_init(crate::action_env::client_environment),
        );
    }
    list_artifacts(&mut action, &inputs);
    action::validate_structure(&action)?;
    action.inputs.clear();
    // Inputs at one execution path must be one artifact without its owner
    // (Artifact.equalsWithoutOwner); later occurrences are dropped. A
    // source is identified by (path text, directory, owner), a generated
    // artifact by (path text, directory, producing action and index): two
    // targets' Artifacts of one shared action's output (crate::shared_actions)
    // are one input, as in Bazel's spawn input map. The fields are compared,
    // never the cached File: a staged tool runfile's `exec` is its
    // runfiles-tree destination (commit_custom_target), not the path its
    // File was computed from. Lookups only: dependencies and inputs keep the
    // declaration order.
    let mut first = Vec::with_capacity(inputs.len());
    {
        type Identity<'a> = (&'a std::ffi::OsStr, bool, Option<(&'a str, usize)>, &'a str);
        let mut seen: HashMap<&Path, Identity<'_>> = HashMap::new();
        for artifact in &inputs {
            let identity = (
                artifact.exec.as_os_str(),
                artifact.directory,
                artifact
                    .producer
                    .as_ref()
                    .map(|(id, index)| (id.as_str(), *index)),
                if artifact.producer.is_some() {
                    ""
                } else {
                    artifact.owner.as_str()
                },
            );
            match seen.entry(artifact.exec.as_path()) {
                std::collections::hash_map::Entry::Occupied(previous) => {
                    if *previous.get() != identity {
                        return Err(format!(
                            "different artifacts map to the same execution path: {}",
                            artifact.exec.display()
                        ));
                    }
                    first.push(false);
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(identity);
                    first.push(true);
                }
            }
        }
    }
    let mut dependencies = Vec::new();
    for (artifact, first) in inputs.into_iter().zip(first) {
        if !first {
            continue;
        }
        // Constant-metadata inputs (N63) key without their digest
        // (crate::action::prepare).
        if artifact.constant_metadata {
            action
                .extra
                .constant_metadata
                .push(artifact.exec.to_path_buf());
        }
        if let Some((action_id, output_index)) = &artifact.producer {
            dependencies.push(Dependency {
                action_id: action_id.clone(),
                output_index: *output_index,
                destination: artifact.exec.clone(),
            });
        } else if planted(&artifact, main_ignored) {
            action
                .inputs
                .push((artifact.real.clone(), artifact.exec.clone()));
        }
    }
    // The input list was sized for every input, generated ones included,
    // and is kept for the rest of the build (MEM4).
    action.inputs.shrink_to_fit();
    dependencies.shrink_to_fit();
    Ok(PlannedAction::new(action, dependencies))
}

/// Load-time gates and pre-dependency gaps: native rules that loading
/// accepts but analysis does not model, package-default gaps, and declared
/// rule-level features (toolchains, exec groups, rule transitions, late-bound
/// defaults) fail explicitly before configuration conditions or dependencies
/// are visited; nothing is silently analyzed (docs/NATIVE_BUILD_RULES.md,
/// docs/CONFIGURATION.md).
fn native_analysis_gate(rule: &Rule) -> Result<(), String> {
    if let Some(error) = rule
        .native
        .as_ref()
        .and_then(|native| crate::native_rules::analysis_error(&rule.label, native))
    {
        // RuleContext errors carry the rule's location.
        return Err(located(rule.location.as_deref(), error));
    }
    // config_setting analysis is modelled (crate::config_setting),
    // alias/label_flag/label_setting forward to their actual,
    // constraint_setting/constraint_value/platform provide their platform
    // providers (crate::platforms), toolchain_type/toolchain their
    // toolchain providers (crate::toolchains) and test_suite its tests'
    // runfiles (TestSuite.create); the other native classes fail
    // explicitly.
    if let RuleKind::Native(kind) = rule.kind
        && !matches!(
            kind,
            "config_setting"
                | "alias"
                | "label_flag"
                | "label_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain_type"
                | "toolchain"
                | "config_feature_flag"
                | "test_suite"
        )
    {
        return Err(format!(
            "{}: native rule {kind} analysis not implemented in the native profile",
            rule.label
        ));
    }
    if !rule.unanalyzed.is_empty() {
        return Err(format!(
            "{}: {} accepted at load time but not analyzed by the native profile: {}",
            rule.label,
            rule.kind.class_name(),
            rule.unanalyzed.join("; ")
        ));
    }
    Ok(())
}

/// N47: in a configuration a Starlark transition enabled
/// `--collect_code_coverage` in, a test rule's coverage support
/// (BaseRuleClasses' `:coverage_support`/`:lcov_merger` attributes and the
/// coverage test action, analysis/test/TestActionBuilder.java) and a
/// late-bound `configuration_field("coverage", "output_generator")` default
/// (CoverageConfiguration.outputGenerator, analysis/test/CoverageConfiguration.java:76-95)
/// are coverage instrumentation, which the native profile does not model:
/// explicit before any dependency edge.
fn coverage_gate(rule: &Rule, configuration: &Configuration) -> Result<(), String> {
    if !configuration.native.collect_code_coverage {
        return Ok(());
    }
    let Some(custom) = &rule.custom else {
        return Ok(());
    };
    let what = if custom.declaration.builtin.kind.test {
        Some(format!("analysis of test rule {}", rule.label))
    } else {
        custom
            .schema
            .iter()
            .find(|(_, spec)| {
                spec.declaration
                    .configuration_field
                    .as_ref()
                    .is_some_and(|field| field.fragment == "coverage")
            })
            .map(|(name, _)| {
                format!(
                    "{}: the late-bound attribute '{name}' (configuration_field(\"coverage\", ...))",
                    rule.label
                )
            })
    };
    match what {
        Some(what) => Err(located(
            rule.location.as_deref(),
            crate::ctx_surface::coverage_gap(&what),
        )),
        None => Ok(()),
    }
}

fn custom_gap_error(rule: &Rule, gaps: &[String]) -> String {
    format!(
        "analysis of {}: declared rule features {}: {}",
        rule.label,
        crate::rule_declarations::NOT_ANALYZED,
        gaps.join("; ")
    )
}

/// Gaps of the configured (select-resolved) attribute values, evaluated
/// after attribute validation and before any dependency edge is followed:
/// edges with transitions, aspects or file/rule restrictions and unmodelled
/// attribute values fail explicitly at the edge.
fn resolved_value_gate(rule: &Rule) -> Result<(), String> {
    if let Some(native) = &rule.native
        && matches!(
            native.rule_class.as_str(),
            "filegroup"
                | "genrule"
                | "config_setting"
                | "alias"
                | "label_flag"
                | "label_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain_type"
                | "toolchain"
                | "config_feature_flag"
                | "test_suite"
        )
    {
        let gaps = crate::native_rules::analysis_gaps(native);
        if !gaps.is_empty() {
            return Err(format!(
                "{}: {} accepted at load time but not analyzed by the native profile: {}",
                rule.label,
                rule.kind.class_name(),
                gaps.join("; ")
            ));
        }
    }
    if let Some(custom) = &rule.custom {
        let gaps = crate::custom_rule::value_gaps(custom);
        if !gaps.is_empty() {
            return Err(custom_gap_error(rule, &gaps));
        }
    }
    Ok(())
}

/// Artifact.joinExecPaths (actions/Artifact.java): exec paths joined with
/// a space, unescaped.
fn paths_string(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// CommandResolverContext.expandSingletonArtifact (GenRuleBase.java:391-400).
fn singleton(paths: &[PathBuf], variable: &str, kind: &str) -> Result<String, String> {
    match paths {
        [] => Err(format!("variable '{variable}' : no {kind}")),
        [path] => Ok(path.to_string_lossy().into_owned()),
        _ => Err(format!("variable '{variable}' : more than one {kind}")),
    }
}

/// The genrule command expansion (GenRuleBase.java:122-134): one
/// TemplateExpander pass (analysis/stringtemplate/TemplateExpander.java:55-177)
/// whose variables are CommandResolverContext.lookupVariable
/// (GenRuleBase.java:327-385: `SRCS`, `<`, `OUTS`, `@`, `RULEDIR`, `@D`)
/// before the configuration's Make variables (ConfigurationMakeVariableContext:
/// `--define`, the `toolchains`' and resolved toolchains' TemplateVariableInfo,
/// TARGET_CPU/COMPILATION_MODE/BINDIR/GENDIR), and whose functions are the
/// location functions with exec paths (LocationTemplateContext,
/// Expander.withExecLocationsNoSrcs). The outer error is a native failure
/// (the deferred target platform), the inner one an ExpansionException.
#[allow(clippy::too_many_arguments)]
fn expand_genrule_command(
    command: &str,
    srcs: &[PathBuf],
    outs: &[PathBuf],
    rule_dir: &Path,
    surface: &crate::ctx_surface::CtxSurface,
    locations: &crate::location_expander::LocationMap,
    parse: &dyn Fn(&str) -> Result<Label, String>,
) -> anyhow::Result<Result<String, String>> {
    let variable = |name: &str| -> anyhow::Result<Result<String, String>> {
        Ok(Ok(match name {
            "SRCS" => paths_string(srcs),
            "<" => return Ok(singleton(srcs, "$<", "input file")),
            "OUTS" => paths_string(outs),
            "@" => return Ok(singleton(outs, "$@", "output file")),
            "RULEDIR" => rule_dir.to_string_lossy().into_owned(),
            "@D" => match outs {
                [output] => output
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                _ => rule_dir.to_string_lossy().into_owned(),
            },
            _ => return crate::ctx_surface::lookup_variable(surface, &[], name),
        }))
    };
    let function = |name: &str, param: &str| -> anyhow::Result<Result<String, String>> {
        Ok(
            crate::location_expander::apply(name, param, true, parse, locations)
                .unwrap_or_else(|| Err(format!("$({name}) not defined"))),
        )
    };
    crate::ctx_surface::expand_template_with(command, &variable, &function, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compute-boundary types are `Send` (review §1.7): P1 has no
    /// threads, but the snapshot must already admit P3's workers.
    #[test]
    fn compute_boundary_types_are_send() {
        compute_boundary_is_send();
    }

    fn generated_out_txt() -> Artifact {
        Artifact::new(
            PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/out.txt"),
            PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/out.txt"),
            Some(("//pkg:gen#genrule".into(), 0)),
            "//pkg:gen".into(),
            false,
            false,
        )
    }

    /// W1b registration invariant: a File keeps its first registration; an
    /// equal re-registration is a no-op.
    #[test]
    fn register_artifact_keeps_one_artifact_per_file() {
        let artifact = generated_out_txt();
        let mut index = BTreeMap::new();
        register_artifact(&mut index, &artifact.file, &artifact);
        register_artifact(&mut index, &artifact.file, &artifact.clone());
        assert_eq!(index.len(), 1);
        assert!(same_artifact(&index[&artifact.file], &artifact));
    }

    /// Re-registering a File with a different Artifact is an internal error
    /// (checked in debug builds, which the tests are).
    #[test]
    #[should_panic(expected = "re-registered with a different artifact")]
    fn register_artifact_rejects_a_second_artifact_for_a_file() {
        let artifact = generated_out_txt();
        let mut index = BTreeMap::new();
        register_artifact(&mut index, &artifact.file, &artifact);
        let mut other = artifact.clone();
        other.make_mut().producer = Some(("//pkg:other#0".into(), 0));
        register_artifact(&mut index, &artifact.file, &other);
    }

    /// A producer of `outputs` whose action (id `id`) writes `content`,
    /// owned by `owner` ([`OutputProducer::action`]'s fields).
    fn writing_producer(
        owner: &str,
        id: &str,
        outputs: &[&str],
        content: &str,
    ) -> Arc<OutputProducer> {
        keyed_writing_producer(owner, id, outputs, content, true)
    }

    /// [`writing_producer`] of a shareable or unshareable action.
    fn keyed_writing_producer(
        owner: &str,
        id: &str,
        outputs: &[&str],
        content: &str,
        shareable: bool,
    ) -> Arc<OutputProducer> {
        let action = Action {
            id: id.to_owned(),
            outputs: outputs.iter().map(PathBuf::from).collect(),
            write: Some(crate::action::WriteAction {
                content: content.as_bytes().to_vec(),
                executable: false,
            }),
            ..Action::default()
        };
        let files = outputs
            .iter()
            .map(|path| {
                File::new(FileData {
                    path: (*path).to_owned(),
                    short_path: crate::label::generated_short_path(
                        crate::label::split_generated_root(path).unwrap().1,
                    ),
                    owner: owner.to_owned(),
                    is_source: false,
                    is_directory: false,
                    is_symlink: false,
                })
            })
            .collect::<Vec<_>>();
        let key = crate::shared_actions::ActionKey::of(
            &action,
            &crate::shared_actions::KeyedParts {
                inputs: &[],
                outputs: &files,
                runfiles_trees: &[],
                execution: crate::shared_actions::KeyedExecution::default(),
                deferred: false,
                build_info: false,
                shareable,
            },
        );
        Arc::new(OutputProducer {
            owner: owner.to_owned(),
            label: None,
            action: Some((id.to_owned(), key)),
        })
    }

    /// Actions.canBeShared between owners (MapBasedActionGraph): an exact
    /// path whose producer has an equal key is that producer's action (its
    /// id is returned, nothing is registered); a different key, a different
    /// output list or a native producer is Bazel's conflict naming the
    /// attempted and the previous owner; a path below or above a produced
    /// one is Bazel's prefix conflict (the shorter path first).
    #[test]
    fn register_output_shares_equal_actions_and_reports_conflicts() {
        let x = "bazel-out/k8-fastbuild/bin/p/x";
        let y = "bazel-out/k8-fastbuild/bin/p/y";
        let mut producers = Producers::new();
        let first = writing_producer("//p:a", "//p:a#0", &[x, y], "same");
        for output in [x, y] {
            assert_eq!(
                register_output(&mut producers, Path::new(output), &first),
                Ok(None)
            );
        }
        let equal = writing_producer("//p:b", "//p:b#3", &[x, y], "same");
        for output in [x, y] {
            assert_eq!(
                register_output(&mut producers, Path::new(output), &equal),
                Ok(Some("//p:a#0".to_owned()))
            );
        }
        assert!(producers.values().all(|producer| producer.owner == "//p:a"));
        let conflict = "file 'p/x' is generated by these conflicting actions:\nLabel: //p:c, //p:a";
        for other in [
            writing_producer("//p:c", "//p:c#0", &[x, y], "other"),
            writing_producer("//p:c", "//p:c#0", &[x], "same"),
            OutputProducer::native("//p:c"),
        ] {
            assert_eq!(
                register_output(&mut producers, Path::new(x), &other),
                Err(conflict.to_owned())
            );
        }
        // An unshareable action (a C++ compile) never shares with another
        // owner's, not even an equal one of the same label (another
        // configuration with this output root); a later shareable one
        // conflicts with it too.
        let z = "bazel-out/k8-fastbuild/bin/p/z";
        let unshareable = keyed_writing_producer("//p:e", "//p:e#0", &[z], "same", false);
        assert_eq!(
            register_output(&mut producers, Path::new(z), &unshareable),
            Ok(None)
        );
        let conflict = "file 'p/z' is generated by these conflicting actions:\nLabel: //p:e";
        for other in [
            keyed_writing_producer("//p:e", "//p:e#1", &[z], "same", false),
            writing_producer("//p:e", "//p:e#1", &[z], "same"),
        ] {
            assert_eq!(
                register_output(&mut producers, Path::new(z), &other),
                Err(conflict.to_owned())
            );
        }
        let below = writing_producer("//p:d", "//p:d#0", &[], "same");
        let prefix = |prefix: &str, prefix_owner: &str, longer: &str, longer_owner: &str| {
            Err(format!(
                "One of the output paths '{prefix}' (belonging to {prefix_owner}) and '{longer}' (belonging to {longer_owner}) is a prefix of the other. These actions cannot be simultaneously present; please rename one of the output files or build just one of them"
            ))
        };
        assert_eq!(
            register_output(
                &mut producers,
                Path::new("bazel-out/k8-fastbuild/bin/p/x/z"),
                &below
            ),
            prefix(
                "bazel-out/k8-fastbuild/bin/p/x",
                "//p:a",
                "bazel-out/k8-fastbuild/bin/p/x/z",
                "//p:d"
            )
        );
        assert_eq!(
            register_output(
                &mut producers,
                Path::new("bazel-out/k8-fastbuild/bin/p"),
                &below
            ),
            prefix(
                "bazel-out/k8-fastbuild/bin/p",
                "//p:d",
                "bazel-out/k8-fastbuild/bin/p/x",
                "//p:a"
            )
        );
        assert_eq!(producers.len(), 3);
    }

    /// A partial share (an equal producer of a later output but not of the
    /// first) would plan no node for the action and leave its first output
    /// without one: an internal error in every build, not a debug_assert.
    #[test]
    fn a_partial_share_is_an_internal_error() {
        let x = "bazel-out/k8-fastbuild/bin/p/x";
        let y = "bazel-out/k8-fastbuild/bin/p/y";
        // //p:a's equal action registered only for `y` (a state the key
        // rules out: it covers every output).
        let mut producers = Producers::new();
        let earlier = writing_producer("//p:a", "//p:a#0", &[x, y], "same");
        assert_eq!(
            register_output(&mut producers, Path::new(y), &earlier),
            Ok(None)
        );
        let key = earlier.action.as_ref().unwrap().1;
        let outputs = [x, y]
            .iter()
            .map(|path| {
                File::new(FileData {
                    path: (*path).to_owned(),
                    short_path: crate::label::split_generated_root(path)
                        .unwrap()
                        .1
                        .to_owned(),
                    owner: "//p:b".to_owned(),
                    is_source: false,
                    is_directory: false,
                    is_symlink: false,
                })
            })
            .collect::<Vec<_>>();
        let registered = crate::analysis_runtime::RegisteredAction {
            action: Action {
                id: "//p:b#0".to_owned(),
                outputs: outputs
                    .iter()
                    .map(|file| PathBuf::from(&file.path))
                    .collect(),
                ..Action::default()
            },
            inputs: Vec::new(),
            outputs,
            deferred: None,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            runfiles_trees: Vec::new(),
            build_info: None,
            key,
            repo_mapping: None,
        };
        let error = register_action_outputs(
            std::slice::from_ref(&registered),
            ("//p:b", "//p:b"),
            &mut producers,
            &mut BTreeMap::new(),
            &mut Declared::new(),
            crate::configuration::Configuration::default().id,
            |_| false,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "internal error: action //p:b#0 of //p:b: output bazel-out/k8-fastbuild/bin/p/y would be //p:a#0's and its first output its own (a shared action shares every output; docs/SHAREABLE_ARTIFACTS.md)"
        );
    }

    /// Review F2 of d2fd11c: a generated File's short path is its
    /// root-relative path with a leading `external` as `..`, whoever owns
    /// it, so a main-repository rule's shareable artifact below `external/`
    /// is the same File as the engine's Artifact of it; sources keep the
    /// owner rule.
    #[test]
    fn generated_short_paths_do_not_depend_on_the_owner() {
        let generated = |exec: &str, owner: &str| {
            let exec = PathBuf::from(exec);
            Artifact::new(
                exec.clone(),
                exec,
                Some(("action#0".into(), 0)),
                owner.into(),
                false,
                false,
            )
            .file()
        };
        let below = "bazel-out/k8-fastbuild/bin/external/somerepo/x.txt";
        for owner in ["//test:producer", "@@dep+//p:producer"] {
            let file = generated(below, owner);
            assert_eq!(file.short_path, "../somerepo/x.txt", "{owner}");
            assert!(!file.is_source);
        }
        assert_eq!(
            generated("bazel-out/k8-opt-exec/bin/test/x.txt", "//test:producer").short_path,
            "test/x.txt"
        );
        // The File a rule declares (Label::generated_short_path: declare_kind
        // and the predeclared outputs) is the one its Artifact computes, also
        // in a main-repository package below external/.
        let output = Label::parse("//external/dep:out.txt", "").unwrap();
        let exec = output.generated_exec_path(crate::label::GENERATED_EXEC_ROOT);
        let declared = File::new(FileData {
            is_directory: false,
            is_symlink: false,
            path: exec.to_string_lossy().into_owned(),
            short_path: output.generated_short_path(),
            owner: "//external/dep:rule".into(),
            is_source: false,
        });
        assert_eq!(
            generated(&declared.path, &declared.owner),
            declared,
            "a main-repository package below external/"
        );
        let source = Artifact::new(
            PathBuf::from("/root/external/dep/in"),
            PathBuf::from("external/dep/in"),
            None,
            "//external/dep:in".into(),
            false,
            false,
        );
        assert_eq!(source.file().short_path, "external/dep/in");
    }

    /// A committed output whose declared File differs from its Artifact's
    /// File is an internal error (checked in debug builds, which the tests
    /// are), never two Files for one artifact.
    #[test]
    #[should_panic(expected = "differs from its artifact's File")]
    fn a_declared_file_differing_from_its_artifact_is_an_internal_error() {
        let exec = PathBuf::from("bazel-out/k8-fastbuild/bin/external/somerepo/x.txt");
        let artifact = Artifact::new(
            exec.clone(),
            exec,
            Some(("//test:producer#0".into(), 0)),
            "//test:producer".into(),
            false,
            false,
        );
        let mut declared = artifact.file();
        verify_declared_file(&declared, &artifact);
        // The pre-F2 owner-dependent short path of the engine's File.
        declared.make_mut().short_path = "external/somerepo/x.txt".into();
        verify_declared_file(&declared, &artifact);
    }

    /// An index miss stays a miss (`None`: the caller reports its `... has no
    /// declared artifact ...` error where the File is used) — except for a
    /// workspace status File, which the lookup that reaches it registers
    /// (both status files; the bare artifacts under --nobuild, which plans
    /// no status action), so it resolves whichever lookup comes first.
    #[test]
    fn indexed_artifact_registers_only_the_workspace_status_files_on_a_miss() {
        let mut engine = Engine::new(PathBuf::new());
        engine.set_execute_build(false);
        let mut index = BTreeMap::new();
        let mut plan = None;
        let (mut nodes, mut producers) = (Vec::new(), BTreeMap::new());
        let (client, status) = (std::cell::OnceCell::new(), std::cell::OnceCell::new());
        let mut lookup = |file: &File, index: &mut BTreeMap<File, Artifact>| {
            engine
                .indexed_artifact(
                    file,
                    index,
                    &mut plan,
                    &mut nodes,
                    &mut producers,
                    &client,
                    &status,
                )
                .unwrap()
        };
        let generated = generated_out_txt();
        assert!(lookup(&generated.file(), &mut index).is_none());
        assert!(index.is_empty());
        register_artifact(&mut index, &generated.file, &generated);
        assert!(same_artifact(
            &lookup(&generated.file(), &mut index).unwrap(),
            &generated
        ));
        let volatile = crate::analysis_runtime::workspace_status_file("volatile-status.txt");
        let artifact = lookup(&volatile, &mut index).unwrap();
        assert_eq!(artifact.real, Path::new("bazel-out/volatile-status.txt"));
        assert!(artifact.producer.is_none() && artifact.constant_metadata);
        // The bare artifact's File is the status File itself.
        assert_eq!(artifact.file, volatile);
        let stable = crate::analysis_runtime::workspace_status_file("stable-status.txt");
        assert!(index.contains_key(&stable) && index.len() == 3);
        assert!(nodes.is_empty() && producers.is_empty());
    }

    /// The workspace status artifacts carry the status Files ctx.info_file
    /// and ctx.version_file hold (short_path `<name>`, derived, no owner,
    /// root `bazel-out`) — bare under --nobuild, planned when the build
    /// executes, also when the status computation failed — so a status file
    /// a consumer reaches through a dependency's files (the view's Artifact
    /// Files) equals its own, as in Bazel (tests/artifact_identity.rs
    /// `status_files_of_a_dependency_are_the_consumers_own`). They used to
    /// compute `bazel-out/<name>` as the short path, and a source File
    /// without a producer.
    #[test]
    fn workspace_status_artifacts_carry_the_status_files() {
        for (execute_build, embed_label) in [(false, ""), (true, ""), (true, "two\nlines")] {
            let mut engine = Engine::new(PathBuf::new());
            engine.set_execute_build(execute_build);
            engine.set_workspace_status(crate::workspace_status::WorkspaceStatusOptions {
                command: String::new(),
                embed_label: embed_label.to_owned(),
            });
            let mut index = BTreeMap::new();
            let (mut nodes, mut producers) = (Vec::new(), BTreeMap::new());
            let (client, status) = (std::cell::OnceCell::new(), std::cell::OnceCell::new());
            let planned = engine
                .ensure_workspace_status(
                    &mut None,
                    &mut nodes,
                    &mut producers,
                    &client,
                    &status,
                    &mut index,
                )
                .unwrap();
            // No status node under --nobuild; when executing, the two
            // placeholders (L2-W4) whatever the status: the planned
            // request's finalize writes the status into them or lays a
            // failed computation out (ONE failing #volatile node,
            // crate::request), not planning.
            assert_eq!(nodes.len(), if execute_build { 2 } else { 0 });
            assert!(nodes.iter().all(|node| matches!(
                node.action.extra.transform,
                Some(crate::action::Transform::WorkspaceStatus { .. })
            )));
            for ((name, id), artifact) in [
                ("stable-status.txt", crate::request::STABLE_STATUS_ID),
                ("volatile-status.txt", crate::request::VOLATILE_STATUS_ID),
            ]
            .into_iter()
            .zip(&planned)
            {
                let file = crate::analysis_runtime::workspace_status_file(name);
                assert_eq!(artifact.file, file, "{name}, executing: {execute_build}");
                assert_eq!((file.short_path.as_str(), file.is_source), (name, false));
                assert_eq!(artifact.exec, Path::new(&file.path));
                assert_eq!(
                    artifact.producer,
                    execute_build.then(|| (id.to_owned(), 0)),
                    "{name}, status label {embed_label:?}"
                );
                assert!(same_artifact(&index[&file], artifact));
            }
            assert_eq!(index.len(), 2);
        }
    }

    /// A memoized dependency view of `//pkg:dep` holding `providers`, and
    /// its key (the BAZEL_RS_VERIFY_VIEWS tests).
    fn view_with(
        providers: BTreeMap<String, starlark::values::OwnedFrozenValue>,
    ) -> (ConfiguredTargetKey, DependencyTarget) {
        let runfiles = Arc::new(crate::runfiles::TargetRunfiles::default());
        let identity = Arc::new(analysis::dependency_target_data(
            "//pkg:dep",
            "//pkg:dep",
            &[],
            false,
            true,
            false,
            false,
            &providers,
            &[],
            &runfiles,
        ));
        let key = ConfiguredTargetKey::new(
            Label::parse("//pkg:dep", "").unwrap(),
            crate::configuration::Configuration::default().id,
        );
        let view = DependencyTarget::new(analysis::DependencyView {
            label: "//pkg:dep".into(),
            files: Vec::new(),
            is_source: false,
            is_rule: true,
            is_package_group: false,
            is_filegroup: false,
            providers,
            provider_keys: Vec::new(),
            runfiles,
            rule_class: Some("dep".into()),
            identity,
        });
        (key, view)
    }

    /// BAZEL_RS_VERIFY_VIEWS compares provider values, not only ids: the
    /// same instances pass, and so does a framework OutputGroupInfo a view
    /// build synthesizes afresh with the same repr (an output file's
    /// `_validation` group).
    #[test]
    fn verify_view_accepts_the_same_provider_values() {
        let set = crate::starlark_types::Depset::from_files(vec![generated_out_txt().file()]);
        let committed = crate::instrumented_files::coverage_gap_frozen("//pkg:dep").unwrap();
        let (key, memoized) = view_with(BTreeMap::from([
            ("//pkg:defs.bzl%Info".to_owned(), committed),
            (
                crate::output_group_info::PROVIDER_ID.to_owned(),
                crate::output_group_info::freeze_validation(set.clone()).unwrap(),
            ),
        ]));
        verify_view(&key, &memoized, Ok(memoized.clone()));
        let mut rebuilt = memoized.clone();
        rebuilt.providers.insert(
            crate::output_group_info::PROVIDER_ID.to_owned(),
            crate::output_group_info::freeze_validation(set).unwrap(),
        );
        verify_view(&key, &memoized, Ok(rebuilt));
    }

    /// A committed provider whose value changed under the same id makes the
    /// memoized view stale.
    #[test]
    #[should_panic(expected = "is stale: provider //pkg:defs.bzl%Info differs")]
    fn verify_view_rejects_a_changed_provider_value() {
        let (key, memoized) = view_with(BTreeMap::from([(
            "//pkg:defs.bzl%Info".to_owned(),
            crate::instrumented_files::coverage_gap_frozen("//pkg:dep").unwrap(),
        )]));
        let mut rebuilt = memoized.clone();
        rebuilt.providers.insert(
            "//pkg:defs.bzl%Info".to_owned(),
            crate::instrumented_files::coverage_gap_frozen("//pkg:dep").unwrap(),
        );
        verify_view(&key, &memoized, Ok(rebuilt));
    }

    #[test]
    fn recursive_package_order_lists_subpackages_first() {
        let mut packages = vec!["tc", "tc/b", "tc/a", "tc/a/deep", "tc/a-b", "tc/B"];
        packages.sort_by(|a, b| subpackages_first(a, b));
        assert_eq!(
            packages,
            ["tc/B", "tc/a/deep", "tc/a", "tc/a-b", "tc/b", "tc"]
        );
        let mut root = vec!["", "x", "a/b", "a"];
        root.sort_by(|a, b| subpackages_first(a, b));
        assert_eq!(root, ["a/b", "a", "x", ""]);
    }

    #[test]
    fn english_lists_follow_string_util() {
        assert_eq!(join_english_list::<&str>(&[], "or"), "nothing");
        assert_eq!(join_english_list(&["a"], "or"), "a");
        assert_eq!(join_english_list(&["a", "b"], "and"), "a and b");
        assert_eq!(join_english_list(&["a", "b", "c"], "or"), "a, b, or c");
    }

    #[test]
    fn alias_descriptions_follow_alias_provider() {
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        assert_eq!(
            describe_with_aliases(&[], &label("//p:x"), "target"),
            "target '//p:x'"
        );
        assert_eq!(
            describe_with_aliases(&[label("//a:a")], &label("@@r+//p:x"), "source file"),
            "alias '//a:a' referring to source file '@@r+//p:x'"
        );
        assert_eq!(
            describe_with_aliases(
                &[label("//a:a"), label("//a:b"), label("//a:c")],
                &label("//p:x"),
                "target"
            ),
            "alias '//a:a' referring to target '//p:x' through '//a:b' -> '//a:c'"
        );
    }

    #[test]
    fn only_main_repository_sources_below_ignored_top_level_entries_are_unplanted() {
        let ignored = IgnoredSubdirectories::new(
            vec!["pkg".into(), "src/ts/node_modules".into()],
            vec!["**/node_modules".into()],
        );
        let source = |exec: &str, owner: &str| {
            Artifact::new(
                PathBuf::from("/ws").join(exec),
                PathBuf::from(exec),
                None,
                owner.into(),
                false,
                false,
            )
        };
        for (artifact, expected) in [
            (source("node_modules/b.txt", "//:node_modules/b.txt"), false),
            (source("pkg/sub/c.txt", "//pkg/sub:c.txt"), false),
            (source("pkgs/c.txt", "//pkgs:c.txt"), true),
            // Nested ignored directories hang below a planted top-level entry.
            (
                source("src/ts/node_modules/x.js", "//src/ts:node_modules/x.js"),
                true,
            ),
            (
                source("external/ext+/node_modules/x", "@@ext+//:node_modules/x"),
                true,
            ),
            (
                Artifact::new(
                    PathBuf::from("/ws").join("node_modules/o.txt"),
                    PathBuf::from("node_modules/o.txt"),
                    Some(("//:gen".into(), 0)),
                    "//:gen".into(),
                    false,
                    false,
                ),
                true,
            ),
        ] {
            assert_eq!(
                planted(&artifact, &ignored),
                expected,
                "{:?}",
                artifact.exec
            );
        }
        assert!(!planted(
            &source("a.txt", "//:a.txt"),
            &IgnoredSubdirectories::new(vec![String::new()], Vec::new())
        ));
    }
    use crate::package::MacroOwner;
    use crate::visibility::Visibility;

    fn test_label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn test_rule(label: &str, owner: Option<(&str, &str)>) -> Rule {
        Rule {
            label: test_label(label),
            kind: RuleKind::Filegroup,
            srcs: Vec::new(),
            outs: Vec::new(),
            cmd: None,
            visibility: Visibility::Private,
            custom: None,
            macro_owner: owner.map(|(definition, name)| MacroOwner {
                definition_package: test_label(definition).package_id(),
                name: name.into(),
            }),
            native: None,
            unanalyzed: Vec::new(),
            package_metadata: Vec::new(),
            location: None,
            explicit_visibility: false,
            call_stack: Arc::from([]),
            generator: None,
        }
    }

    // Isolate analysis policy from macro evaluation: these are the same owned
    // declaration snapshots that package evaluation hands to the engine.
    fn test_engine(root: PathBuf, rules: Vec<Rule>) -> Engine {
        let mut engine = Engine::new(root);
        for rule in rules {
            for out in &rule.outs {
                engine.generated.insert(
                    rule.label.with_name(out).unwrap(),
                    (rule.label.clone(), out.clone()),
                );
            }
            let package = engine.packages.entry(rule.label.package_id()).or_default();
            if let Some(owner) = &rule.macro_owner {
                package.macro_names.insert(owner.name.clone());
            }
            package.rules.insert(rule.label.clone(), Arc::new(rule));
        }
        engine
    }

    /// CommonPrerequisiteValidator's visibility text (test rules have no location).
    fn assert_not_visible(error: &str, dependency: &str, consumer: &str, source: bool) {
        let suffix = if source {
            ". To set the visibility of that source file target, use the exports_files() function"
        } else {
            ""
        };
        assert_eq!(
            error,
            format!(
                "in filegroup rule {consumer}: Visibility error:\ntarget '{dependency}' is not visible from\ntarget '{consumer}'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility{suffix}"
            )
        );
    }

    fn analyze_roots(engine: &mut Engine, roots: &[&str]) -> Result<Vec<Rule>, String> {
        let mut done = ConfiguredNodes::new();
        let mut plan = Plan::new();
        let top = engine.configurations.top();
        for root in roots {
            engine.analyze(
                &test_label(root),
                top,
                &mut Vec::new(),
                &mut done,
                &mut plan,
            )?;
        }
        Ok(plan
            .into_iter()
            .map(|(_, rule)| Arc::unwrap_or_clone(rule))
            .collect())
    }

    fn group(label: &str, specs: &[&str], includes: &[&str]) -> Rule {
        let mut rule = test_rule(label, None);
        rule.kind = RuleKind::PackageGroup(Arc::new(package_specification::GroupDefinition {
            own_specs: package_specification::parse_specs(
                &specs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &rule.label.package_id(),
                &RepositorySet::new(PathBuf::new()),
            )
            .unwrap(),
            includes: includes.iter().map(|s| test_label(s)).collect(),
        }));
        rule.visibility = Visibility::Public;
        rule
    }

    #[test]
    fn package_group_cached_diamond_keeps_independent_exclusions_and_identity() {
        let mut engine = test_engine(
            PathBuf::new(),
            vec![
                group("//g:leaf", &["//foo/private"], &[]),
                group("//g:left", &["//foo/...", "-//foo/private"], &["//g:leaf"]),
                group("//g:right", &["//other"], &["//g:leaf"]),
                group("//g:root", &[], &["//g:left", "//g:right", "//g:left"]),
            ],
        );
        let root = engine
            .resolve_group(&test_label("//g:root"), &mut Vec::new())
            .unwrap();
        assert_eq!(engine.group_nodes.len(), 4);
        for member in ["//foo/private:x", "//foo/public:x", "//other:x"] {
            assert!(root.contains(&test_label(member).package_id()));
        }
        assert!(!root.contains(&test_label("//foobar:x").package_id()));
        let left = engine
            .resolve_group(&test_label("//g:left"), &mut Vec::new())
            .unwrap();
        let right = engine
            .resolve_group(&test_label("//g:right"), &mut Vec::new())
            .unwrap();
        assert!(!Arc::ptr_eq(&left, &right));
        assert!(left.contains(&test_label("//foo/public:x").package_id()));
        assert!(!right.contains(&test_label("//foo/public:x").package_id()));
        assert!(Arc::ptr_eq(
            &root,
            &engine
                .resolve_group(&test_label("//g:root"), &mut Vec::new())
                .unwrap()
        ));
    }

    #[test]
    fn package_group_large_shared_declarations_and_cached_edges_never_copy_payloads() {
        let leaf_label = test_label("//g:leaf");
        let root_label = test_label("//g:root");
        let large_package = format!("//{}", "p".repeat(512 * 1024));
        let leaf = group("//g:leaf", &[&large_package], &[]);
        let mut root = group("//g:root", &[], &[]);
        let RuleKind::PackageGroup(definition) = &mut root.kind else {
            unreachable!()
        };
        const EDGES: usize = 4096;
        Arc::get_mut(definition).unwrap().includes = vec![leaf_label.clone(); EDGES];
        let mut engine = test_engine(PathBuf::new(), vec![leaf, root]);
        let RuleKind::PackageGroup(declaration) =
            &engine.packages[&leaf_label.package_id()][&leaf_label].kind
        else {
            unreachable!()
        };
        let declaration = Arc::clone(declaration);
        let Target::Rule(first) = engine.target(&leaf_label).unwrap() else {
            unreachable!()
        };
        let RuleKind::PackageGroup(first) = &first.kind else {
            unreachable!()
        };
        let first = Arc::clone(first);
        assert!(Arc::ptr_eq(&declaration, &first));
        for _ in 0..128 {
            let Target::Rule(next) = engine.target(&leaf_label).unwrap() else {
                unreachable!()
            };
            let RuleKind::PackageGroup(next) = &next.kind else {
                unreachable!()
            };
            assert!(Arc::ptr_eq(&first, next));
        }
        // GroupDefinition deliberately has no Clone implementation: every
        // generic Target/Rule snapshot above must share the declaration payload.
        let declaration_owners = Arc::strong_count(&declaration);
        let leaf_node = engine.resolve_group(&leaf_label, &mut Vec::new()).unwrap();
        let root_node = engine.resolve_group(&root_label, &mut Vec::new()).unwrap();
        assert_eq!(engine.group_nodes.len(), 2);
        assert_eq!(Arc::strong_count(&leaf_node), EDGES + 2);
        assert_eq!(Arc::strong_count(&declaration), declaration_owners);
        assert!(root_node.contains(&test_label(&format!("{large_package}:x")).package_id()));

        // Once the engine's cumulative node budget is exhausted, both cached
        // resolutions must still succeed without charging or copying payloads.
        while engine.group_budget.node().is_ok() {}
        for _ in 0..128 {
            assert!(Arc::ptr_eq(
                &leaf_node,
                &engine.resolve_group(&leaf_label, &mut Vec::new()).unwrap()
            ));
            assert!(Arc::ptr_eq(
                &root_node,
                &engine.resolve_group(&root_label, &mut Vec::new()).unwrap()
            ));
        }
        assert_eq!(Arc::strong_count(&declaration), declaration_owners);
        assert_eq!(Arc::strong_count(&leaf_node), EDGES + 2);
    }

    #[test]
    fn package_group_cached_height_rejects_257_path_nodes_child_first() {
        let mut rules = vec![group("//g:n0", &[], &[])];
        for n in 1..=256 {
            rules.push(group(
                &format!("//g:n{n}"),
                &[],
                &[&format!("//g:n{}", n - 1)],
            ));
        }
        let mut engine = test_engine(PathBuf::new(), rules);
        for n in 0..256 {
            engine
                .resolve_group(&test_label(&format!("//g:n{n}")), &mut Vec::new())
                .unwrap();
        }
        let error = engine
            .resolve_group(&test_label("//g:n256"), &mut Vec::new())
            .unwrap_err();
        assert!(error.contains("depth limit 256"), "{error}");
        assert_eq!(engine.group_nodes.len(), 256);
        assert!(!engine.group_nodes.contains_key(&test_label("//g:n256")));
    }

    #[test]
    fn package_group_edge_budget_precedes_allocation_and_cache_publication() {
        let mut huge = group("//g:huge", &[], &[]);
        let RuleKind::PackageGroup(definition) = &mut huge.kind else {
            unreachable!()
        };
        Arc::get_mut(definition).unwrap().includes = vec![test_label("//g:leaf"); 100_001];
        let mut engine = test_engine(PathBuf::new(), vec![huge, group("//g:leaf", &[], &[])]);
        let error = engine
            .resolve_group(&test_label("//g:huge"), &mut Vec::new())
            .unwrap_err();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        assert!(engine.group_nodes.is_empty());

        // Distinct successfully published nodes share one engine-wide budget.
        let long_package = format!("//{}", "p".repeat(1024 * 1024));
        let rules = (0..9)
            .map(|n| group(&format!("//g:n{n}"), &[&long_package], &[]))
            .collect();
        let mut engine = test_engine(PathBuf::new(), rules);
        for n in 0..7 {
            engine
                .resolve_group(&test_label(&format!("//g:n{n}")), &mut Vec::new())
                .unwrap();
        }
        assert!(
            engine
                .resolve_group(&test_label("//g:n7"), &mut Vec::new())
                .unwrap_err()
                .contains("byte limit")
        );
        assert_eq!(engine.group_nodes.len(), 7);
        // Cached hits require no new persistent allocation budget.
        assert!(
            engine
                .resolve_group(&test_label("//g:n0"), &mut Vec::new())
                .is_ok()
        );
    }

    #[test]
    fn package_group_query_excludes_nonrules_and_includes_never_declare_files() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/data", "not an implicit source");
        f.write(
            "app/BUILD",
            "package_group(name='g', includes=[':data'])\nfilegroup(name='ordinary')",
        );
        let mut engine = Engine::new(f.0.clone());
        for pattern in ["//app:all", "//app/..."] {
            assert_eq!(
                engine.select(&[pattern.into()], "").unwrap(),
                vec![test_label("//app:ordinary")]
            );
        }
        assert_eq!(
            engine.select(&["//app:g".into()], "").unwrap(),
            vec![test_label("//app:g")]
        );
        assert!(
            engine
                .select(&["//app:data".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        assert!(
            engine
                .build(&[test_label("//app:g")], &f.0.join("cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("cache").exists());
    }

    #[test]
    fn package_group_visibility_validates_roots_sources_and_all_refs_before_grants() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for invalid in ["missing", "ordinary", "cycle"] {
            for source in [false, true] {
                for incoming in [false, true] {
                    let mut root = test_rule("//app:root", None);
                    let visibility = Visibility::from_labels(vec![
                        test_label("//app:__pkg__"),
                        test_label(&format!("//app:{invalid}")),
                    ])
                    .unwrap();
                    root.visibility = visibility.clone();
                    let mut consumer = test_rule("//app:consumer", None);
                    consumer
                        .srcs
                        .push(if source { ":data" } else { ":root" }.into());
                    let mut engine = test_engine(
                        f.0.clone(),
                        vec![
                            root,
                            consumer,
                            test_rule("//app:ordinary", None),
                            group("//app:cycle", &[], &["//app:cycle"]),
                        ],
                    );
                    engine
                        .packages
                        .get_mut(&test_label("//app:root").package_id())
                        .unwrap()
                        .exports
                        .insert(test_label("//app:data"), visibility);
                    let target = if incoming {
                        "//app:consumer"
                    } else if source {
                        "//app:data"
                    } else {
                        "//app:root"
                    };
                    assert!(engine.select(&[target.into()], "").is_ok());
                    let error = engine
                        .build(&[test_label(target)], &f.0.join("cache"))
                        .unwrap_err();
                    assert!(
                        error.contains(match invalid {
                            "missing" => "not declared",
                            "ordinary" => "does not refer to a package group",
                            _ => "cycle",
                        }),
                        "{target}: {error}"
                    );
                    assert!(!f.0.join("cache").exists());
                }
            }
        }
    }

    #[test]
    fn package_group_visibility_uses_effective_macro_and_source_authorities() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for requested in ["//producer:demo", "//producer:demo.out", "//app:data"] {
            for allowed in ["//defs", "//app", "@@dep//defs"] {
                let mut producer = test_rule("//producer:demo", Some(("//owner:m.bzl", "demo")));
                producer.visibility =
                    Visibility::from_labels(vec![test_label("//g:allow")]).unwrap();
                producer.outs.push("demo.out".into());
                let mut consumer = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
                consumer.srcs.push(requested.into());
                let mut engine = test_engine(
                    f.0.clone(),
                    vec![producer, consumer, group("//g:allow", &[allowed], &[])],
                );
                engine
                    .packages
                    .get_mut(&test_label("//app:demo").package_id())
                    .unwrap()
                    .exports
                    .insert(
                        test_label("//app:data"),
                        Visibility::from_labels(vec![test_label("//g:allow")]).unwrap(),
                    );
                let result = analyze_roots(&mut engine, &["//app:demo"]);
                assert_eq!(
                    result.is_ok(),
                    allowed == "//defs",
                    "{requested} {allowed}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn package_group_kind_checks_precede_callbacks_and_native_inputs_reject_groups() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write(
            "app/defs.bzl",
            "def impl(ctx):\n    fail('WRONG_KIND_CALLBACK_RAN')\nr = rule(implementation=impl)\n",
        );
        for build in [
            "package_group(name='root', includes=[':wrong'])",
            "filegroup(name='root', visibility=[':wrong'])",
        ] {
            f.write(
                "app/BUILD",
                &format!("load(':defs.bzl', 'r')\nr(name='wrong')\n{build}"),
            );
            let mut engine = Engine::new(f.0.clone());
            let error = engine
                .build(&[test_label("//app:root")], &f.0.join("cache"))
                .unwrap_err();
            assert!(
                error.contains("does not refer to a package group"),
                "{error}"
            );
            assert!(!error.contains("WRONG_KIND_CALLBACK_RAN"), "{error}");
        }
        for declaration in [
            "filegroup(name='root', srcs=[':g'])",
            "genrule(name='root', srcs=[':g'], outs=['out'], cmd='touch $@')",
        ] {
            f.write(
                "app/BUILD",
                &format!("package_group(name='g')\n{declaration}"),
            );
            let mut engine = Engine::new(f.0.clone());
            assert!(
                engine
                    .build(&[test_label("//app:root")], &f.0.join("cache"))
                    .unwrap_err()
                    .contains("misplaced")
            );
        }
        assert!(!f.0.join("cache").exists());
    }

    #[test]
    fn package_group_macro_namespace_is_checked_on_includes_and_cache_hits() {
        let mut owned = group("//app:outside", &["public"], &[]);
        owned.macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        let mut engine = test_engine(
            PathBuf::new(),
            vec![owned, group("//app:root", &[], &["//app:outside"])],
        );
        assert!(engine.select(&["//app:outside".into()], "").is_ok());
        assert!(
            engine
                .resolve_group(&test_label("//app:root"), &mut Vec::new())
                .unwrap_err()
                .contains("symbolic macro namespace")
        );
        assert!(engine.group_nodes.is_empty());
        let mut engine = test_engine(
            PathBuf::new(),
            vec![group("//app:outside", &["public"], &[])],
        );
        engine
            .resolve_group(&test_label("//app:outside"), &mut Vec::new())
            .unwrap();
        Arc::make_mut(
            engine
                .package_mut(&test_label("//app:outside").package_id())
                .rules
                .get_mut(&test_label("//app:outside"))
                .unwrap(),
        )
        .macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        assert!(
            engine
                .resolve_group(&test_label("//app:outside"), &mut Vec::new())
                .unwrap_err()
                .contains("symbolic macro namespace")
        );
    }

    #[test]
    fn package_group_provider_materializes_independent_frozen_instances_and_empty_files() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/defs.bzl", r#"
def impl(ctx):
    a = ctx.attr.a[PackageSpecificationInfo]
    b = ctx.attr.b[PackageSpecificationInfo]
    a == b or fail("native equality")
    (a.contains("//allowed:x") and not b.contains("//allowed:x")) or fail("merged data")
    (b.contains("//different:x") and not a.contains("//different:x")) or fail("merged data")
    ctx.attr.a.files.to_list() == [] or fail("group artifacts")
    ctx.attr.a[DefaultInfo].files.to_list() == [] or fail("default artifacts")
    out = ctx.actions.declare_file("result")
    ctx.actions.write(out, str(a.contains("//allowed:x")) + ":" + str(b.contains("//allowed:x")))
    return [DefaultInfo(files=depset([out]))]
check = rule(implementation=impl, attrs={"a":attr.label(providers=[PackageSpecificationInfo]), "b":attr.label(providers=[PackageSpecificationInfo])})
def emit(name, visibility):
    native.package_group(name=name, packages=["//allowed"])
m = macro(implementation=emit)
"#);
        f.write("app/BUILD", "load(':defs.bzl', 'check', 'm')\nm(name='a')\npackage_group(name='b', packages=['//different'])\ncheck(name='consumer', a=':a', b=':b')\npackage(default_visibility=[':missing'])");
        // The consumer's bad default is intentional: no callback/action may run
        // until its outgoing visibility references have been validated.
        let mut engine = Engine::new(f.0.clone());
        let standalone = engine
            .build(&[test_label("//app:a")], &f.0.join("empty-cache"))
            .unwrap();
        assert!(standalone.outputs.is_empty());
        assert_eq!((standalone.executed, standalone.cache_hits), (0, 0));
        assert!(
            engine
                .build(&[test_label("//app:consumer")], &f.0.join("bad-cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("bad-cache").exists());
        f.write("app/BUILD", "load(':defs.bzl', 'check', 'm')\nm(name='a')\npackage_group(name='b', packages=['//different'])\ncheck(name='consumer', a=':a', b=':b')");
        let mut engine = Engine::new(f.0.clone());
        for warm in [false, true] {
            let result = engine
                .build(&[test_label("//app:consumer")], &f.0.join("cache"))
                .unwrap();
            assert_eq!(std::fs::read(&result.outputs[0].1).unwrap(), b"True:False");
            assert_eq!(
                (result.executed, result.cache_hits),
                if warm { (0, 1) } else { (1, 0) }
            );
        }
        assert_eq!(engine.group_nodes.len(), 2);
    }

    #[test]
    fn package_group_source_default_visibility_and_public_normalization() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/data", "source");
        f.write("app/BUILD", "filegroup(name='declares_source', srcs=['data'])\npackage(default_visibility=[':missing'])");
        let mut engine = Engine::new(f.0.clone());
        assert!(engine.select(&["//app:data".into()], "").is_ok());
        assert!(
            engine
                .build(&[test_label("//app:data")], &f.0.join("bad-cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("bad-cache").exists());
        f.write("app/defs.bzl", "def impl(name, visibility):\n    native.filegroup(name=name, visibility=visibility)\nm=macro(implementation=impl)");
        f.write("app/BUILD", "load(':defs.bzl','m')\nm(name='root', visibility=['//nonexistent:group', '//visibility:public'])");
        let mut engine = Engine::new(f.0.clone());
        assert!(
            engine
                .build(&[test_label("//app:root")], &f.0.join("cache"))
                .unwrap()
                .outputs
                .is_empty()
        );
        assert!(engine.group_nodes.is_empty());
    }

    #[test]
    fn macro_authority_checks_both_directions_even_within_one_physical_package() {
        for macro_producer in [true, false] {
            let owner = ("//defs:macro.bzl", "demo");
            let producer = test_rule("//app:demo_secret", macro_producer.then_some(owner));
            let mut consumer = test_rule("//app:demo_use", (!macro_producer).then_some(owner));
            consumer.srcs.push("//app:demo_secret".into());
            let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
            // A direct root is not itself subject to visibility. It also must
            // not let a subsequent unauthorized edge bypass the check via done.
            let error =
                analyze_roots(&mut engine, &["//app:demo_secret", "//app:demo_use"]).unwrap_err();
            assert_not_visible(&error, "//app:demo_secret", "//app:demo_use", false);
        }
    }

    #[test]
    fn shared_macro_definition_authority_allows_private_cross_package_edges() {
        let owner = Some(("//defs:macro.bzl", "demo"));
        let producer = test_rule("//one:demo_secret", owner);
        let mut consumer = test_rule("//two:demo_use", owner);
        consumer.srcs.push("//one:demo_secret".into());
        let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
        let plan = analyze_roots(&mut engine, &["//two:demo_use"]).unwrap();
        assert_eq!(plan[0].label, test_label("//one:demo_secret"));
        assert_eq!(plan[1].label, test_label("//two:demo_use"));
    }

    #[test]
    fn authority_keeps_canonical_repositories_distinct_and_honors_explicit_grants() {
        let producer = test_rule("//app:demo_secret", Some(("@@dep//defs:m.bzl", "demo")));
        let mut consumer = test_rule("//app:demo_use", Some(("//defs:m.bzl", "demo")));
        consumer.srcs.push("//app:demo_secret".into());
        let mut engine = test_engine(PathBuf::new(), vec![producer.clone(), consumer.clone()]);
        assert!(
            analyze_roots(&mut engine, &["//app:demo_use"])
                .unwrap_err()
                .contains("not visible")
        );
        let mut producer = producer;
        producer.visibility =
            Visibility::Private.with_package(&test_label("//defs:x").package_id());
        let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
        assert_eq!(
            analyze_roots(&mut engine, &["//app:demo_use"])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn generated_output_visibility_uses_generator_authority() {
        for macro_producer in [true, false] {
            let owner = ("//defs:m.bzl", "demo");
            let mut producer = test_rule("//app:demo_make", macro_producer.then_some(owner));
            producer.outs.push("demo.out".into());
            let mut consumer = test_rule("//app:demo_use", (!macro_producer).then_some(owner));
            consumer.srcs.push("//app:demo.out".into());
            let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
            assert_not_visible(
                &analyze_roots(&mut engine, &["//app:demo_make", "//app:demo_use"]).unwrap_err(),
                "//app:demo.out",
                "//app:demo_use",
                false,
            );
        }
    }

    #[test]
    fn macro_namespace_requires_separator_and_nonempty_suffix_for_every_rule_kind() {
        for kind in [RuleKind::Filegroup, RuleKind::Genrule, RuleKind::Custom] {
            for (name, valid) in [
                ("demo", true),
                ("demo_x", true),
                ("demo-x", true),
                ("demo.x", true),
                ("demo_", false),
                ("demo-", false),
                ("demo.", false),
                ("demox", false),
                ("demo/x", false),
                ("other", false),
            ] {
                let mut rule = test_rule(&format!("//app:{name}"), Some(("//defs:m.bzl", "demo")));
                rule.kind = kind.clone();
                let mut engine = test_engine(PathBuf::new(), vec![rule]);
                let result = analyze_roots(&mut engine, &[&format!("//app:{name}")]);
                assert_eq!(result.is_ok(), valid, "{kind:?}: {name}");
                if let Err(error) = result {
                    assert!(error.contains("symbolic macro namespace"), "{error}");
                }
            }
        }
    }

    #[test]
    fn outside_namespace_output_is_queryable_and_emitted_but_not_a_build_target() {
        let f = Fixture::new();
        let mut rule = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
        rule.kind = RuleKind::Genrule;
        rule.outs.push("outside.out".into());
        rule.cmd = Some("printf macro-output > $@".into());
        let mut engine = test_engine(f.0.clone(), vec![rule.clone()]);
        assert_eq!(
            engine.select(&["//app:all".into()], "").unwrap(),
            vec![rule.label.clone()]
        );
        assert_eq!(
            engine.select(&["//app:outside.out".into()], "").unwrap(),
            vec![test_label("//app:outside.out")]
        );
        let built = engine
            .build(&[rule.label.clone()], &f.0.join("cache"))
            .unwrap();
        assert_eq!(built.executed, 1);
        assert_eq!(std::fs::read(&built.outputs[0].1).unwrap(), b"macro-output");
        for roots in [
            vec!["//app:outside.out"],
            vec!["//app:demo", "//app:outside.out"],
            vec!["//app:outside.out", "//app:demo"],
        ] {
            let mut engine = test_engine(f.0.clone(), vec![rule.clone()]);
            let roots = roots.into_iter().map(test_label).collect::<Vec<_>>();
            let error = engine
                .build(&roots, &f.0.join("rejected-cache"))
                .unwrap_err();
            assert!(error.contains("//app:outside.out"), "{error}");
            assert!(error.contains("symbolic macro namespace"), "{error}");
            assert!(!f.0.join("rejected-cache").exists());
        }
    }

    #[test]
    fn valid_output_cannot_hide_invalid_generator_and_unselected_rule_does_not_poison_build() {
        let mut invalid = test_rule("//app:outside", Some(("//defs:m.bzl", "demo")));
        invalid.outs.push("demo.out".into());
        let valid = test_rule("//app:demo_ok", Some(("//defs:m.bzl", "demo")));
        let mut engine = test_engine(PathBuf::new(), vec![invalid, valid]);
        assert_eq!(engine.select(&["//app:all".into()], "").unwrap().len(), 2);
        assert!(
            engine
                .select(&["//app:outside".into(), "//app:demo.out".into()], "")
                .is_ok()
        );
        assert_eq!(
            analyze_roots(&mut engine, &["//app:demo_ok"])
                .unwrap()
                .len(),
            1
        );
        let error = analyze_roots(&mut engine, &["//app:demo.out"]).unwrap_err();
        assert!(error.contains("//app:outside"), "{error}");
        assert!(error.contains("symbolic macro namespace"), "{error}");
    }

    #[test]
    fn macro_only_source_reference_does_not_create_source_even_with_same_authority() {
        let f = Fixture::new();
        f.write("app/data", "source");
        let mut rule = test_rule("//app:demo", Some(("//app:m.bzl", "demo")));
        rule.srcs.push(":data".into());
        let mut engine = test_engine(f.0.clone(), vec![rule]);
        assert!(
            engine
                .select(&["//app:data".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        assert!(
            analyze_roots(&mut engine, &["//app:demo"])
                .unwrap_err()
                .contains("not declared")
        );
    }

    fn source_path(engine: &mut Engine, label: &str) -> Result<PathBuf, String> {
        match engine.target(&test_label(label))? {
            Target::Source(path) => Ok(path),
            Target::Rule(rule) | Target::Output(rule, _) => Err(format!("rule {}", rule.label)),
        }
    }

    #[test]
    fn source_targets_use_the_package_input_index_and_are_memoized_unless_linked() {
        let f = Fixture::new();
        f.write("app/data", "source");
        f.write("app/dir/nested", "x");
        f.write("app/macro_only", "x");
        std::os::unix::fs::symlink("data", f.0.join("app/link")).unwrap();
        let mut ordinary = test_rule("//app:files", None);
        ordinary
            .srcs
            .extend([":data", ":missing", ":dir", ":link", "//app:canonical"].map(str::to_owned));
        ordinary.package_metadata = vec!["//app:metadata".into()];
        let mut macro_rule = test_rule("//app:demo", Some(("//app:m.bzl", "demo")));
        macro_rule.srcs.push(":macro_only".into());
        let mut engine = test_engine(f.0.clone(), vec![ordinary, macro_rule]);
        // Repeated lookups answer exactly like the first (the linear scan's
        // predicate, precomputed once for the package).
        for _ in 0..2 {
            assert_eq!(
                source_path(&mut engine, "//app:data").unwrap(),
                f.0.join("app/data")
            );
            assert_eq!(
                source_path(&mut engine, "//app:missing").unwrap(),
                f.0.join("app/missing")
            );
            assert_eq!(
                source_path(&mut engine, "//app:dir").unwrap(),
                f.0.join("app/dir")
            );
            assert_eq!(
                source_path(&mut engine, "//app:link").unwrap(),
                f.0.join("app/data")
            );
            assert_eq!(
                source_path(&mut engine, "//app:canonical").unwrap(),
                f.0.join("app/canonical")
            );
            assert_eq!(
                source_path(&mut engine, "//app:metadata").unwrap(),
                f.0.join("app/metadata")
            );
            assert_eq!(
                source_path(&mut engine, "//app:files").unwrap_err(),
                "rule //app:files"
            );
            for undeclared in ["//app:macro_only", "//app:undeclared"] {
                let error = source_path(&mut engine, undeclared).unwrap_err();
                assert!(error.contains("not declared in package 'app'"), "{error}");
            }
        }
        assert_eq!(engine.declared_inputs.len(), 1);
        assert_eq!(
            engine.missing_sources,
            ["canonical", "metadata", "missing"]
                .map(|name| (
                    f.0.join("app").join(name),
                    test_label(&format!("//app:{name}"))
                ))
                .into_iter()
                .collect::<BTreeMap<_, _>>()
        );
        assert_eq!(engine.source_directories.len(), 1);
        for memoized in ["//app:data", "//app:missing", "//app:dir", "//app:metadata"] {
            assert!(engine.source_targets.contains_key(&test_label(memoized)));
        }
        // A symlinked source resolves under the link policy of the
        // repositories materialized so far: it is resolved on every lookup.
        assert!(
            !engine
                .source_targets
                .contains_key(&test_label("//app:link"))
        );
        assert!(
            !engine
                .source_targets
                .contains_key(&test_label("//app:undeclared"))
        );
    }

    #[test]
    fn source_paths_keep_first_occurrences_in_order_without_generated_inputs() {
        let artifact = |real: &str, producer: Option<usize>| {
            Artifact::new(
                real.into(),
                real.into(),
                producer.map(|index| ("//app:gen".to_owned(), index)),
                "//app:x".into(),
                false,
                false,
            )
        };
        let inputs = [
            artifact("b", None),
            artifact("a", None),
            artifact("gen", Some(0)),
            artifact("b", None),
            artifact("c", None),
            artifact("a", None),
        ];
        assert_eq!(
            source_paths(&inputs),
            ["b", "a", "c"].map(PathBuf::from).to_vec()
        );
    }

    /// MEM2: a planned action's source paths are derived from its planned
    /// inputs exactly when that reproduces [`source_paths`] of the inputs it
    /// was planned from; otherwise (an unplanted source) the list is kept.
    #[test]
    fn source_inputs_are_derived_from_the_planned_action_only_when_equal() {
        let artifact = |real: &str, exec: &str, producer: Option<usize>| {
            Artifact::new(
                real.into(),
                exec.into(),
                producer.map(|index| ("//app:gen".to_owned(), index)),
                "//app:x".into(),
                false,
                false,
            )
        };
        let plan = |inputs: Vec<Artifact>, ignored: &IgnoredSubdirectories| {
            let sources = source_paths(&inputs);
            let action = Action {
                id: "//app:x#0".into(),
                outputs: vec!["bazel-out/bin/app/out".into()],
                ..Action::default()
            };
            let planned =
                plan_action(action, inputs, ignored, &std::cell::OnceCell::new()).unwrap();
            (
                SourceInputs::planned(sources.clone(), &planned, Vec::new()),
                planned,
                sources,
            )
        };
        // Planted sources, one staged twice (same real path at another
        // execution path), and a generated input: derived.
        let (derived, planned, sources) = plan(
            vec![
                artifact("/ws/app/b", "app/b", None),
                artifact("/ws/app/a", "app/a", None),
                artifact("bazel-out/bin/app/gen", "bazel-out/bin/app/gen", Some(0)),
                artifact("/ws/app/b", "app/tree/b", None),
            ],
            &IgnoredSubdirectories::default(),
        );
        assert!(derived.own.is_none());
        assert_eq!(derived.own(&planned), sources.iter().collect::<Vec<_>>());
        assert_eq!(
            sources,
            ["/ws/app/b", "/ws/app/a"].map(PathBuf::from).to_vec()
        );
        // A main-repository source below an ignored directory is not
        // planted (not in the action's inputs) but still checked: kept.
        let ignored = IgnoredSubdirectories::new(vec!["ignored".into()], Vec::new());
        let (kept, planned, sources) = plan(
            vec![
                artifact("/ws/app/a", "app/a", None),
                artifact("/ws/ignored/c", "ignored/c", None),
            ],
            &ignored,
        );
        assert_eq!(planned.action.inputs.len(), 1);
        assert_eq!(kept.own.as_ref(), Some(&sources));
        assert_eq!(kept.own(&planned), sources.iter().collect::<Vec<_>>());
    }

    #[test]
    fn source_analysis_runs_once_per_label_and_failures_are_not_memoized() {
        let f = Fixture::new();
        f.write("app/data", "source");
        f.write("app/grouped", "source");
        let mut consumer = test_rule("//app:files", None);
        consumer
            .srcs
            .extend([":data", ":data", ":grouped"].map(str::to_owned));
        let mut engine = test_engine(f.0.clone(), vec![consumer]);
        // The source's own visibility names a package group that does not exist.
        engine
            .package_mut(&test_label("//app:grouped").package_id())
            .exports
            .insert(
                test_label("//app:grouped"),
                Visibility::Labels(vec![test_label("//app:no_group")]),
            );
        let top = engine.configurations.top();
        let analyze = |engine: &mut Engine, label: &str| {
            engine.analyze(
                &test_label(label),
                top,
                &mut Vec::new(),
                &mut ConfiguredNodes::new(),
                &mut Plan::new(),
            )
        };
        analyze(&mut engine, "//app:data").unwrap();
        assert!(engine.analyzed_sources.contains(&test_label("//app:data")));
        analyze(&mut engine, "//app:data").unwrap();
        let first = analyze(&mut engine, "//app:grouped").unwrap_err();
        assert!(first.contains("no_group"), "{first}");
        assert_eq!(analyze(&mut engine, "//app:grouped").unwrap_err(), first);
        assert_eq!(engine.analyzed_sources.len(), 1);
    }

    #[test]
    fn explicit_export_or_unbuilt_ordinary_reference_establishes_macro_source() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for exported in [true, false] {
            let mut macro_rule = test_rule("//app:demo", Some(("//app:m.bzl", "demo")));
            macro_rule.srcs.push(":data".into());
            let mut engine = test_engine(f.0.clone(), vec![macro_rule]);
            let source = test_label("//app:data");
            let package = engine.package_mut(&source.package_id());
            if exported {
                package.exports.insert(source.clone(), Visibility::Private);
            } else {
                let mut ordinary = test_rule("//app:unselected", None);
                ordinary.srcs.push(":data".into());
                package
                    .rules
                    .insert(ordinary.label.clone(), Arc::new(ordinary));
            }
            assert_eq!(
                engine.select(&["//app:data".into()], "").unwrap(),
                vec![source]
            );
            let plan = analyze_roots(&mut engine, &["//app:demo"]).unwrap();
            assert_eq!(plan.len(), 1);
            assert_eq!(plan[0].label, test_label("//app:demo"));
        }
    }

    #[test]
    fn reserved_macro_name_cannot_be_an_implicit_source_but_rules_and_outputs_win() {
        let f = Fixture::new();
        f.write("app/demo", "source");
        let mut ordinary = test_rule("//app:ordinary", None);
        ordinary.srcs.push(":demo".into());
        let mut engine = test_engine(f.0.clone(), vec![ordinary]);
        let label = test_label("//app:demo");
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .macro_names
            .insert("demo".into());
        assert!(
            engine
                .select(&["//app:demo".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        let rule = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
        engine
            .package_mut(&label.package_id())
            .rules
            .insert(label.clone(), Arc::new(rule));
        assert!(matches!(engine.target(&label), Ok(Target::Rule(_))));
        let output = test_label("//app:reserved_output");
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .macro_names
            .insert(output.name.clone());
        engine
            .generated
            .insert(output.clone(), (label, output.name.clone()));
        assert!(matches!(engine.target(&output), Ok(Target::Output(_, _))));
    }

    #[test]
    fn macro_source_visibility_uses_physical_source_and_definition_consumer_authorities() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for exported in [true, false] {
            let mut consumer = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
            consumer.srcs.push(":data".into());
            let mut ordinary = test_rule("//app:ordinary", None);
            ordinary.srcs.push(":data".into());
            let mut engine = test_engine(f.0.clone(), vec![consumer, ordinary]);
            let source = test_label("//app:data");
            if exported {
                engine
                    .packages
                    .get_mut(&source.package_id())
                    .unwrap()
                    .exports
                    .insert(source.clone(), Visibility::Private);
            }
            assert_not_visible(
                &analyze_roots(&mut engine, &["//app:demo"]).unwrap_err(),
                "//app:data",
                "//app:demo",
                true,
            );
            let package = engine.package_mut(&source.package_id());
            let grant = Visibility::Private.with_package(&test_label("//defs:x").package_id());
            if exported {
                package.exports.insert(source, grant);
            } else {
                package.default_visibility = grant;
            }
            assert_eq!(
                analyze_roots(&mut engine, &["//app:demo"]).unwrap().len(),
                1
            );
        }
    }

    #[allow(unsafe_code)] // the test sets the process env (edition 2024)
    /// N63 follow-up 2: two `build()` calls on ONE Engine recompute the
    /// workspace status per build (the OnceLock is per build, reset in
    /// plan_build): a changed SOURCE_DATE_EPOCH between the builds lands
    /// in the second build's volatile-status.txt. Reading the env var is
    /// process-global, so this test owns it exclusively (no other test in
    /// this binary sets SOURCE_DATE_EPOCH; the status unit tests use the
    /// pure function with an explicit client env).
    #[test]
    fn workspace_status_recomputes_on_each_build_of_one_engine() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write(
            "BUILD",
            "genrule(name='g',outs=['g.txt'],cmd='printf x > $@')\n",
        );
        let cache = f.0.join("cache");
        // Inject each build's client environment deterministically into
        // the (fresh, per-build) ActionPlanning status cell — the real
        // process env is read once per Engine by the general cell and
        // cannot be varied per build in-process reliably.
        let _guard = env_lock().lock().unwrap();
        let volatile_bytes = |cache: &std::path::Path| -> Vec<u8> {
            // The volatile content of THIS build: materialized (hardlinked
            // from its cache entry at execution). Several cache entries
            // may hold volatile-status.txt; the one whose bytes parse
            // with the newest BUILD_TIMESTAMP is the latest write.
            let actions = cache.join("actions");
            let mut best: Option<(i64, Vec<u8>)> = None;
            for entry in std::fs::read_dir(&actions).unwrap().flatten() {
                let path = entry.path().join("outputs/bazel-out/volatile-status.txt");
                if path.exists() {
                    let bytes = std::fs::read(&path).unwrap();
                    let text = String::from_utf8_lossy(&bytes);
                    let stamp: i64 = text
                        .lines()
                        .find_map(|l| l.strip_prefix("BUILD_TIMESTAMP "))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if best.as_ref().is_none_or(|(s, _)| stamp >= *s) {
                        best = Some((stamp, bytes));
                    }
                }
            }
            best.expect("volatile status output materialized").1
        };
        let mut engine = Engine::new(f.0.clone());
        // ActionPlanning (and both its env cells) is created fresh by
        // every plan_build, so setting the process env before each build
        // deterministically varies the second build's status.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1700000000") };
        let first = engine.build(&[test_label("//:g")], &cache).unwrap();
        assert_eq!(first.executed, 1);
        let one = volatile_bytes(&cache);
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1800000000") };
        let second = engine.build(&[test_label("//:g")], &cache).unwrap();
        assert_eq!(second.executed, 0, "the genrule itself stays cached");
        let two = volatile_bytes(&cache);
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        assert_ne!(one, two, "the second build recomputed the status");
        assert!(
            String::from_utf8_lossy(&one).contains("BUILD_TIMESTAMP 1700000000\n"),
            "{}",
            String::from_utf8_lossy(&one)
        );
        assert!(
            String::from_utf8_lossy(&two).contains("BUILD_TIMESTAMP 1800000000\n"),
            "{}",
            String::from_utf8_lossy(&two)
        );
    }

    /// Serializes the env-mutating workspace status test above against
    /// any other test thread that builds with the default client env.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn macro_dynamic_artifact_filename_is_not_a_namespace_target() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write(
            "app/BUILD",
            "load(':defs.bzl','write')\nwrite(name='demo')\n",
        );
        f.write("app/defs.bzl", "def _impl(ctx):\n    out = ctx.actions.declare_file('outside.txt')\n    ctx.actions.write(out, str(ctx.label))\n    return [DefaultInfo(files=depset([out]))]\nwrite = rule(implementation=_impl)\n");
        let label = test_label("//app:demo");
        let mut engine = Engine::new(f.0.clone());
        engine.load_package(&label.package_id()).unwrap();
        Arc::make_mut(
            engine
                .package_mut(&label.package_id())
                .rules
                .get_mut(&label)
                .unwrap(),
        )
        .macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        let result = engine.build(&[label], &f.0.join("cache")).unwrap();
        assert_eq!(result.executed, 1);
        assert_eq!(
            std::fs::read(&result.outputs[0].1).unwrap(),
            b"@@//app:demo"
        );
        assert!(engine.select(&["//app:outside.txt".into()], "").is_err());
    }

    #[test]
    fn external_artifact_short_paths_are_metadata_only() {
        for (exec, producer, expected) in [
            ("external/dep+/p/in", None, "../dep+/p/in"),
            (
                "bazel-out/k8-fastbuild/bin/external/dep+/p/out",
                Some(("action".into(), 0)),
                "../dep+/p/out",
            ),
        ] {
            let artifact = Artifact::new(
                PathBuf::from("/real/local/root/p/in"),
                PathBuf::from(exec),
                producer,
                "@@dep+//p:target".into(),
                false,
                false,
            );
            assert_eq!(artifact.file().short_path, expected);
            assert_eq!(artifact.file().path, exec);
        }
        let main = Artifact::new(
            PathBuf::from("/root/external/dep/in"),
            PathBuf::from("external/dep/in"),
            None,
            "//external/dep:in".into(),
            false,
            false,
        );
        assert_eq!(main.file().short_path, "external/dep/in");
    }

    #[test]
    fn custom_canonical_main_sources_are_not_rebased_to_external() {
        let engine = Engine::new(PathBuf::new());
        let mut rule = Rule {
            label: Label::parse_canonical("@@dep+//p:target").unwrap(),
            kind: RuleKind::Custom,
            srcs: vec![],
            outs: vec![],
            cmd: None,
            visibility: Default::default(),
            custom: None,
            macro_owner: None,
            native: None,
            unanalyzed: Vec::new(),
            package_metadata: Vec::new(),
            location: None,
            explicit_visibility: false,
            call_stack: Arc::from([]),
            generator: None,
        };
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            ""
        );
        rule.kind = RuleKind::Filegroup;
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            "dep+"
        );
    }

    fn source_file(path: &Path) -> File {
        File::new(FileData {
            path: path.to_string_lossy().into_owned(),
            short_path: path.to_string_lossy().into_owned(),
            owner: String::new(),
            is_source: true,
            is_directory: false,
            is_symlink: false,
        })
    }

    #[test]
    fn external_make_locations_and_rule_directory_use_execution_paths() {
        let label = Label::parse_canonical("@@dep+//p:target").unwrap();
        let input = label.with_name("in").unwrap().source_exec_path();
        let output = label
            .with_name("nested/out")
            .unwrap()
            .generated_exec_path(crate::label::GENERATED_EXEC_ROOT);
        let mut locations = crate::location_expander::LocationMap::default();
        locations.add("@@dep+//p:in", [source_file(&input)]);
        let repositories = RepositorySet::new(PathBuf::new());
        let parse = |text: &str| repositories.parse(text, &label.repository, &label.package);
        let expanded = expand_genrule_command(
            "$(location //p:in) $(location :in) $(RULEDIR) $(@D)",
            &[],
            &[output],
            &PathBuf::from(crate::label::GENERATED_EXEC_ROOT).join(label.package_exec_path()),
            &crate::ctx_surface::CtxSurface::default(),
            &locations,
            &parse,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            expanded,
            "external/dep+/p/in external/dep+/p/in bazel-out/k8-fastbuild/bin/external/dep+/p bazel-out/k8-fastbuild/bin/external/dep+/p/nested"
        );
    }

    #[test]
    fn location_expansion_shell_escapes_each_path_but_srcs_do_not() {
        let label = Label::parse_canonical("//p:target").unwrap();
        let spaced = label.with_name("a b (1).pdf").unwrap().source_exec_path();
        let quoted = label.with_name("it's").unwrap().source_exec_path();
        let plain = label.with_name("plain.txt").unwrap().source_exec_path();
        let mut locations = crate::location_expander::LocationMap::default();
        locations.add("//p:a b.pdf", [source_file(&spaced)]);
        locations.add(
            "//p:group",
            [
                source_file(&spaced),
                source_file(&quoted),
                source_file(&plain),
            ],
        );
        let parse = |text: &str| Label::parse(text, "p");
        let expanded = expand_genrule_command(
            // The first ')' ends the reference (TemplateExpander.scanVariable).
            "$(location :a b.pdf)|$(locations :group)|$(SRCS)",
            &[spaced, plain],
            &[],
            Path::new("bazel-out/k8-fastbuild/bin/p"),
            &crate::ctx_surface::CtxSurface::default(),
            &locations,
            &parse,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            expanded,
            "'p/a b (1).pdf'|'p/a b (1).pdf' 'p/it'\\''s' p/plain.txt|p/a b (1).pdf p/plain.txt"
        );
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = crate::test_temp::root().join(format!(
                "engine-repositories-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A workspace whose `//:top` depends on `//:leaf` in the target and
    /// (cfg = "exec") the exec configuration, every rule resolving a
    /// toolchain, with the explicit host platform //p:linux.
    fn two_configuration_engine(
        build: &str,
        registrations: &str,
        extra_defs: &str,
    ) -> (Fixture, Engine) {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            &format!("module(name='main')\nregister_toolchains('//tc:all'{registrations})\n"),
        );
        f.write(
            "os/BUILD",
            "constraint_setting(name='os', visibility=['//visibility:public'])\nconstraint_value(name='linux', constraint_setting=':os', visibility=['//visibility:public'])\n",
        );
        f.write(
            "p/BUILD",
            "platform(name='linux', constraint_values=['//os:linux'], visibility=['//visibility:public'])\n",
        );
        f.write("r/BUILD", "");
        f.write(
            "r/defs.bzl",
            &format!("def _impl_impl(ctx):\n    return [platform_common.ToolchainInfo(name='tc'), DefaultInfo()]\nimpl = rule(implementation=_impl_impl)\ndef _gen_impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, ' '.join([f.path for f in ctx.files.tool + ctx.files.dep]) + ctx.toolchains['//tc:type'].name)\n    return [DefaultInfo(files=depset([out] + ctx.files.tool + ctx.files.dep))]\ngen = rule(implementation=_gen_impl, toolchains=['//tc:type'], attrs={{'tool': attr.label(cfg='exec'), 'dep': attr.label()}})\n{extra_defs}"),
        );
        f.write(
            "tc/BUILD",
            "load('//r:defs.bzl', 'impl')\ntoolchain_type(name='type', visibility=['//visibility:public'])\nimpl(name='impl')\ntoolchain(name='tc', toolchain_type=':type', toolchain=':impl')\n",
        );
        f.write("BUILD", build);
        let mut engine = Engine::new(f.0.clone());
        let mut options = crate::configuration::CommandLineOptions::default();
        options.parse("--host_platform=//p:linux", None).unwrap();
        engine.set_configuration(
            options
                .configuration(&crate::configuration::parse_canonical_option_label)
                .unwrap(),
        );
        (f, engine)
    }

    #[test]
    fn one_engine_analyzes_a_target_in_two_configurations() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='leaf')\ngen(name='top', tool=':leaf', dep=':leaf')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:top".into()], "").unwrap();
        let (nodes, requested, _, _) = engine.plan_build(&labels).unwrap();
        let top = engine.configurations.top();
        let key = |name: &str, config| ConfiguredTargetKey::new(test_label(name), config);
        let exec = engine.exec_configurations[&key("//:top", top)];
        assert_ne!(exec, top);
        assert_eq!(engine.configurations.mnemonic(exec), "linux-opt-exec");
        // Separate actions: `L#n` in the top-level configuration,
        // `L[config8]#n` in the exec configuration; separate output roots,
        // so the producers of the two leaf outputs never conflict.
        let actions = nodes
            .iter()
            .map(|node| {
                (
                    node.action.id.clone(),
                    node.action.outputs[0].display().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let exec_leaf = format!("//:leaf[{}]#0", &exec.to_string()[..8]);
        assert_eq!(
            actions,
            BTreeMap::from([
                (
                    "//:leaf#0".to_owned(),
                    "bazel-out/k8-fastbuild/bin/leaf.txt".to_owned()
                ),
                (
                    exec_leaf.clone(),
                    "bazel-out/linux-opt-exec/bin/leaf.txt".to_owned()
                ),
                (
                    "//:top#0".to_owned(),
                    "bazel-out/k8-fastbuild/bin/top.txt".to_owned()
                ),
                // The workspace status nodes of every executing build
                // (N63 follow-up, BLOCKER 1).
                (
                    "BazelWorkspaceStatusAction#stable".to_owned(),
                    "bazel-out/stable-status.txt".to_owned()
                ),
                (
                    "BazelWorkspaceStatusAction#volatile".to_owned(),
                    "bazel-out/volatile-status.txt".to_owned()
                ),
            ])
        );
        // The consumer sees both configured leaves (its write content).
        let top_action = nodes
            .iter()
            .find(|node| node.action.id == "//:top#0")
            .unwrap();
        assert_eq!(
            top_action.action.write.as_ref().unwrap().content,
            b"bazel-out/linux-opt-exec/bin/leaf.txt bazel-out/k8-fastbuild/bin/leaf.txttc"
        );
        // The requested target's files: its output and both leaves'.
        assert_eq!(
            requested
                .iter()
                .map(|(requested, artifact)| {
                    assert_eq!(*requested, key("//:top", top));
                    artifact.exec.display().to_string()
                })
                .collect::<Vec<_>>(),
            [
                "bazel-out/k8-fastbuild/bin/top.txt",
                "bazel-out/linux-opt-exec/bin/leaf.txt",
                "bazel-out/k8-fastbuild/bin/leaf.txt"
            ]
        );
        // Toolchain contexts and registered toolchains per configuration.
        assert_eq!(
            engine
                .toolchain_contexts
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            {
                let mut keys = vec![
                    key("//:leaf", top),
                    key("//:leaf", exec),
                    key("//:top", top),
                ];
                keys.sort();
                keys
            }
        );
        assert_eq!(
            engine
                .registered_toolchains
                .keys()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([top, exec])
        );
        assert!(engine.computing_toolchains.is_empty());
        // Exec of exec is the same configuration; nothing else was interned.
        assert_eq!(
            engine
                .configurations
                .intern_exec(exec, test_label("//p:linux"), &BTreeSet::new()),
            Ok(exec)
        );
    }

    /// The exec transition is computed once per (parent configuration,
    /// execution platform) (Engine::exec_transitions): every rule with exec
    /// edges in that configuration gets the memoized configuration (debug
    /// builds recompute it on each hit), exec of exec is the exec
    /// configuration itself, and a new top-level configuration clears it.
    #[test]
    fn exec_configurations_are_memoized_per_parent_and_platform() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='leaf')\ngen(name='a', tool=':leaf')\ngen(name='b', tool=':leaf')\ngen(name='top', tool=':a', dep=':b')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:top".into()], "").unwrap();
        engine.plan_build(&labels).unwrap();
        let top = engine.configurations.top();
        let key = |name: &str, config| ConfiguredTargetKey::new(test_label(name), config);
        let exec = engine.exec_configurations[&key("//:top", top)];
        assert_ne!(exec, top);
        // `//:b` (top-level) and `//:a` (exec, through `//:top`'s tool).
        assert_eq!(engine.exec_configurations[&key("//:b", top)], exec);
        assert_eq!(engine.exec_configurations[&key("//:a", exec)], exec);
        let linux = test_label("//p:linux");
        assert_eq!(
            engine.exec_transitions,
            HashMap::from([((top, linux.clone()), exec), ((exec, linux), exec)])
        );
        engine.set_configuration(Configuration::default());
        assert!(engine.exec_transitions.is_empty());
    }

    /// N35 (docs/EXEC_GROUPS.md): each action runs in the exec group it
    /// chose (`exec_group = ...`, or under automatic exec groups the group
    /// of its `toolchain`), with that group's execution platform and
    /// combined exec properties (ExecGroupCollection.computeCombinedExecProperties:
    /// platform default, target default, platform group, target group); a
    /// `config.exec(exec_group = NAME)` edge uses the group's execution
    /// platform.
    #[test]
    fn actions_run_in_their_exec_groups_with_combined_properties() {
        let (f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'eg', 'gen')\ngen(name='leaf')\neg(name='e', tool=':leaf', exec_properties={'k': 't', 'link.k': 'tl', 'link.only': 'o'})\n",
            "",
            "def _eg_impl(ctx):\n    outs = []\n    for name, kw in [('a', {}), ('b', {'exec_group': 'link'}), ('c', {'toolchain': '//tc:type'}), ('d', {'toolchain': None})]:\n        out = ctx.actions.declare_file(ctx.label.name + '.' + name)\n        ctx.actions.run_shell(outputs=[out], command='touch ' + out.path, **kw)\n        outs.append(out)\n    return [DefaultInfo(files=depset(outs))]\neg = rule(implementation=_eg_impl, toolchains=['//tc:type'], exec_groups={'link': exec_group()}, attrs={'_use_auto_exec_groups': attr.bool(default=True), 'tool': attr.label(cfg=config.exec(exec_group='link'))})\n",
        );
        f.write(
            "p/BUILD",
            "platform(name='linux', constraint_values=['//os:linux'], exec_properties={'p': 'v', 'link.p': 'vl', '//tc:type.p': 'va'}, visibility=['//visibility:public'])\n",
        );
        let labels = engine.select_for_build(&["//:e".into()], "").unwrap();
        engine.plan_build(&labels).unwrap();
        let top = engine.configurations.top();
        let key = ConfiguredTargetKey::new(test_label("//:e"), top);
        let pairs = |items: &[(&str, &str)]| {
            items
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect::<Vec<_>>()
        };
        let execution = |index: usize| engine.action_executions[&format!("//:e#{index}")].clone();
        let expected =
            |group: &str, properties: &[(&str, &str)]| crate::exec_groups::ActionExecution {
                exec_group: group.to_owned(),
                platform: test_label("//p:linux"),
                properties: pairs(properties),
            };
        assert_eq!(
            execution(0),
            expected("default-exec-group", &[("p", "v"), ("k", "t")])
        );
        assert_eq!(
            execution(1),
            expected("link", &[("p", "vl"), ("k", "tl"), ("only", "o")])
        );
        assert_eq!(
            execution(2),
            expected("//tc:type", &[("p", "va"), ("k", "t")])
        );
        assert_eq!(
            execution(3),
            expected("default-exec-group", &[("p", "v"), ("k", "t")])
        );
        // The automatic exec group of //tc:type and the declared group.
        let groups = &engine.exec_group_contexts[&key];
        assert!(groups.use_auto_exec_groups);
        assert_eq!(
            groups
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            ["link", "//tc:type"]
        );
        // config.exec(exec_group = "link"): the group's execution platform.
        let exec = engine.group_exec_configurations[&(key.clone(), "link".to_owned())];
        assert_eq!(
            engine
                .configurations
                .intern_exec(top, test_label("//p:linux"), &BTreeSet::new()),
            Ok(exec)
        );
        assert!(!engine.exec_configurations.contains_key(&key));
    }

    /// The registered-toolchain guards are per configuration: computing
    /// the top-level registry analyzes a registered target whose exec edge
    /// needs the exec configuration's registry, which is computed nested
    /// instead of being reported as a toolchain cycle.
    #[test]
    fn nested_exec_registries_are_not_toolchain_cycles() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen', 'plain')\nconfig_setting(name='opt', values={'compilation_mode': 'opt'})\ngen(name='leaf')\nplain(name='reg', tool=select({':opt': None, '//conditions:default': ':leaf'}))\ngen(name='top')\n",
            ", '//:reg'",
            "def _plain_impl(ctx):\n    return []\nplain = rule(implementation=_plain_impl, attrs={'tool': attr.label(cfg='exec')})\n",
        );
        let labels = engine.select_for_build(&["//:top".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        assert!(!error.contains("Misconfigured toolchains"), "{error}");
        assert!(
            error.contains("invalid registered toolchain '//:reg': target does not provide the DeclaredToolchainInfo provider"),
            "{error}"
        );
        assert!(engine.computing_toolchains.is_empty());
        // The exec configuration's registry was computed (nested) and kept.
        assert_eq!(engine.registered_toolchains.len(), 2);
    }

    #[test]
    fn cycles_through_exec_edges_print_each_configuration() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='a', tool=':b')\ngen(name='b', tool=':a')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:a".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        let top = engine.configurations.top();
        let exec = engine.exec_configurations[&ConfiguredTargetKey::new(test_label("//:a"), top)];
        // The requested rule's untrimmed top-level key precedes its own.
        let untrimmed = engine.configurations.get(top).untrimmed_id();
        assert_ne!(untrimmed, top);
        assert!(
            error.ends_with(&format!(
                ": in gen rule //:b: cycle in dependency graph:\n    //:a ({untrimmed})\n    //:a ({top})\n.-> //:b ({exec})\n|   //:a ({exec})\n`-- //:b ({exec})"
            )),
            "{error}"
        );
        // A requested rule inside its own cycle: only the untrimmed key is
        // on the path.
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='a', dep=':b')\ngen(name='b', dep=':a')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:a".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        let top = engine.configurations.top();
        let untrimmed = engine.configurations.get(top).untrimmed_id();
        assert!(
            error.ends_with(&format!(
                ": in gen rule //:a: cycle in dependency graph:\n    //:a ({untrimmed})\n.-> //:a ({top})\n|   //:b ({top})\n`-- //:a ({top})"
            )),
            "{error}"
        );
    }

    #[test]
    fn repository_patterns_and_native_build_keep_equal_packages_separate() {
        let f = Fixture::new();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='../dep')\n");
        f.write("dep/MODULE.bazel", "module(name='dep')\n");
        let build =
            "genrule(name='copy',srcs=['in'],outs=['out'],cmd='cat $(location :in) > $@')\n";
        f.write("main/p/BUILD", build);
        f.write("dep/p/BUILD", build);
        f.write("main/p/in", "main contents");
        f.write("dep/p/in", "dependency contents");
        let mut engine = Engine::new(f.0.join("main"));
        let main = engine.select(&["//...".into()], "").unwrap();
        assert_eq!(
            main.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["//p:copy"]
        );
        let external = engine.select(&["@alias//p:all".into()], "").unwrap();
        assert_eq!(
            external.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["@@dep+//p:copy"]
        );
        for pattern in ["@alias//...", "@@dep+//p/..."] {
            assert_eq!(engine.select(&[pattern.into()], "").unwrap(), external);
        }
        let labels = main.into_iter().chain(external).collect::<Vec<_>>();
        let summary = engine.build(&labels, &f.0.join("cache")).unwrap();
        assert_eq!(summary.executed, 2);
        for (label, path) in summary.outputs {
            let expected = if label.repository.is_empty() {
                "main contents"
            } else {
                "dependency contents"
            };
            assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
        }
    }

    #[test]
    fn genrule_expansion_follows_command_resolver_context() {
        let srcs = vec![PathBuf::from("pkg/in")];
        let outs = vec![PathBuf::from("out/file")];
        let surface = crate::ctx_surface::CtxSurface {
            defines: vec![("SRCS".into(), "define".into()), ("D".into(), "d".into())],
            template_variables: vec![vec![("TOOL".into(), "$(D)/bin".into())]],
            ..Default::default()
        };
        let mut locations = crate::location_expander::LocationMap::default();
        locations.add(
            "//pkg:in",
            [File::new(FileData {
                path: "pkg/in".into(),
                short_path: "pkg/in".into(),
                owner: "//pkg:in".into(),
                is_source: true,
                is_directory: false,
                is_symlink: false,
            })],
        );
        let parse = |text: &str| Label::parse(text, "pkg");
        let expand = |command: &str, srcs: &[PathBuf]| {
            expand_genrule_command(
                command,
                srcs,
                &outs,
                Path::new("bazel-out/k8-fastbuild/bin/pkg"),
                &surface,
                &locations,
                &parse,
            )
            .unwrap()
        };
        assert_eq!(
            expand(
                "cat $< > $@; echo $$HOME $(SRCS) $(@D) $(TOOL) $(location :in) $(COMPILATION_MODE) $(RULEDIR)",
                &srcs
            ),
            Ok("cat pkg/in > out/file; echo $HOME pkg/in out d/bin pkg/in fastbuild bazel-out/k8-fastbuild/bin/pkg".to_owned())
        );
        for (command, message) in [
            (
                "$HOME",
                "'$HOME' syntax is not supported; use '$(HOME)' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell",
            ),
            ("$(UNKNOWN)", "$(UNKNOWN) not defined"),
            ("$(nope :in)", "$(nope) not defined"),
            (
                "$(location :not_declared)",
                "label '//pkg:not_declared' in $(location) expression is not a declared prerequisite of this rule",
            ),
            ("$(SRCS", "unterminated variable reference"),
            ("$", "unterminated $"),
        ] {
            assert_eq!(expand(command, &srcs), Err(message.to_owned()), "{command}");
        }
        assert_eq!(
            expand("$<", &[]),
            Err("variable '$<' : no input file".to_owned())
        );
    }
}
