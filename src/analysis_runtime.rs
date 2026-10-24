//! Analysis capabilities and owned scope metadata, below rule orchestration.
//! No Starlark values are hidden in the scope stack.
// Public action signatures and their generated trampolines are keyword-rich.
#![allow(clippy::too_many_arguments)]
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, ensure};

use crate::action::{Action, ActionExtra, Transform, WriteAction};
use crate::command_line::CommandLineSpec;
use crate::label::Label;
use crate::repository::RepositorySet;
use crate::scheduler::Deferred;
use crate::starlark_types::File;
use crate::starlark_types::{alloc_file, file_list, unpack_file};
use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::OwnedFrozenValue;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::{
    Heap, NoSerialize, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLike, starlark_value,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::AtomicBool;

/// The verified top-level `def` functions of every package loaded in one
/// build (crate::engine registers each package's transitive load graph,
/// crate::loading::LoadedModules::functions). Args `map_each` accepts any
/// global StarlarkFunction (Args.validateMapEach, analysis/starlark/Args.java:366-385:
/// `sfn.isGlobal()`); a function can reach a rule's analysis from another
/// package's evaluation of its `.bzl` (a toolchain or dependency provider,
/// e.g. rules_go's `go.toolchain.actions.stdlib`), so the package's own
/// list is not enough. Keyed by value identity; a hash match is confirmed
/// with `ptr_eq`.
#[derive(Debug, Default)]
pub struct LoadedFunctions {
    by_identity: Mutex<std::collections::HashMap<u64, Vec<OwnedFrozenValue>>>,
}

impl LoadedFunctions {
    fn key(value: Value<'_>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.identity().hash(&mut hasher);
        hasher.finish()
    }

    /// Register the top-level functions of one loaded package.
    pub fn register(&self, functions: &[OwnedFrozenValue]) {
        let mut map = self
            .by_identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for function in functions {
            let entry = map.entry(Self::key(function.value())).or_default();
            if !entry
                .iter()
                .any(|known| known.value().ptr_eq(function.value()))
            {
                entry.push(function.clone());
            }
        }
    }

    /// The registered function `value` is, if any.
    pub fn find(&self, value: Value<'_>) -> Option<OwnedFrozenValue> {
        let map = self
            .by_identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(&Self::key(value))?
            .iter()
            .find(|known| known.value().ptr_eq(value))
            .cloned()
    }
}

/// A workspace status artifact (BazelWorkspaceStatusModule.java:280-281):
/// a derived artifact of the build-data root `bazel-out` without an owner
/// label (WorkspaceStatusValue.BUILD_INFO_KEY).
pub(crate) fn workspace_status_file(name: &str) -> File {
    File {
        path: format!("bazel-out/{name}"),
        short_path: name.to_owned(),
        owner: String::new(),
        is_source: false,
        is_directory: false,
        is_symlink: false,
    }
}

pub(crate) fn is_workspace_status_file(file: &File) -> bool {
    file.owner.is_empty()
        && !file.is_source
        && matches!(
            file.path.as_str(),
            "bazel-out/stable-status.txt" | "bazel-out/volatile-status.txt"
        )
}

#[derive(Clone, Debug)]
pub struct RegisteredAction {
    pub action: Action,
    pub inputs: Vec<File>,
    pub outputs: Vec<File>,
    /// Execution-time completion (tree artifact expansion).
    pub deferred: Option<Deferred>,
    /// The exec group the action runs in (StarlarkAction.Builder.setExecGroup;
    /// `default-exec-group` unless `ctx.actions.run`/`run_shell` chose one,
    /// crate::exec_groups::ActionExecGroups::determine).
    pub exec_group: String,
    /// Runfiles trees of the spawn's tools (N23f), shared ([`tool_runfiles_tree`]).
    pub runfiles_trees: Vec<Arc<ToolRunfilesTree>>,
    /// An actions.transform_{version,info}_file registration (N63): the
    /// Starlark translation callback (StarlarkActionFactory
    /// .transformBuildInfoFile :501-534 keeps the function the same way)
    /// and which status file it translates. Planning evaluates the callback
    /// against the build's workspace status and rewrites the action into a
    /// template expansion (BuildInfoFileWriteAction.execute :121-176).
    pub build_info: Option<BuildInfoAction>,
    /// The owner-independent identity Actions.canBeShared compares, computed
    /// once at registration (crate::shared_actions): within this target
    /// ([`Context::register_action`]) and against another target's producer
    /// of an output (crate::engine `register_output`).
    pub key: crate::shared_actions::ActionKey,
    /// A `py_internal.create_repo_mapping_manifest` registration
    /// ([`Context::register_repo_mapping_manifest`]): the runfiles whose
    /// RepoMappingManifestAction the write is; its content needs the
    /// target's transitive repositories, so the commit computes it
    /// (crate::engine).
    pub repo_mapping: Option<crate::runfiles::Runfiles>,
}

/// The translation callback of an `actions.transform_version_file` /
/// `actions.transform_info_file` action (true: the volatile status file).
#[derive(Clone, Debug)]
pub struct BuildInfoAction {
    pub function: starlark::values::OwnedFrozenValue,
    pub volatile: bool,
    /// The callback's key digest (BuildInfoFileWriteAction.computeKey
    /// :185-197 keys the callback's bzl transitive digest): the native
    /// stand-in is the defining .bzl file's identity (repository, package,
    /// file) plus the function name — the frozen top-level function value
    /// carries no source path in this runtime, so the file identity is
    /// recorded at registration from the function's own module global.
    pub callback_digest: String,
}

// Marker derive only. These fields own Rust data, never hidden heap Values.
#[allow(unsafe_code)]
mod owned {
    use super::*;
    #[derive(ProvidesStaticType)]
    pub(crate) struct Context {
        pub label: Label,
        /// The analyzed rule's class (cc_internal.rule_class).
        pub rule_class: String,
        /// The output root of the configured target's configuration
        /// (crate::config_registry::Configurations::output_root).
        pub output_root: String,
        /// The owner part of the target's action ids
        /// (crate::config_registry::ConfiguredTargetKey::action_owner).
        pub action_owner: String,
        pub declared: RefCell<Vec<File>>,
        /// The exec paths of the declared outputs that are shareable
        /// artifacts (`ctx.actions.declare_shareable_artifact`): below an
        /// artifact root, not the package, so the package-relative output
        /// checks never apply to them.
        pub shareable: RefCell<BTreeSet<String>>,
        pub actions: RefCell<Vec<RegisteredAction>>,
        pub active: Arc<AtomicBool>,
        pub top_level_functions: std::sync::Arc<[OwnedFrozenValue]>,
        /// The build's registry of every loaded package's top-level
        /// functions (`None` outside an engine build).
        pub loaded_functions: Option<Arc<LoadedFunctions>>,
        pub repositories: Arc<RepositorySet>,
        pub scope: Arc<ScopeState>,
        /// The rule's subrule inputs: lifted attribute values and toolchain
        /// state (crate::subrule::RuleEnvironment).
        pub subrule: crate::subrule::RuleEnvironment,
        /// ctx.outputs.executable and the executable-attribute
        /// FilesToRunProviders (crate::runfiles).
        pub runfiles: crate::runfiles::ContextRunfiles,
        /// The exec groups an action may run in (crate::exec_groups).
        pub exec_groups: crate::exec_groups::ActionExecGroups,
        /// The shell action environment of the target's configuration
        /// (BuildConfigurationValue.getActionEnvironment; crate::action_env).
        pub action_env: Arc<crate::action_env::ActionEnvironment>,
        /// The rule's tags that are execution requirements
        /// (TargetUtils.getExecutionInfo, crate::engine::rule_execution_info):
        /// every spawn of the target gets them
        /// (TargetUtils.getFilteredExecutionInfo with
        /// --incompatible_allow_tags_propagation), and they are keyed.
        pub propagated_tags: BTreeSet<String>,
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct Actions<'v> {
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        /// The repr of the owning rule or subrule context.
        pub context: String,
        /// A subrule's action factory: the subrule's requested toolchain
        /// types (StarlarkSubrule.SubruleContext.maybeOverride*, :464-481).
        #[allocative(skip)]
        #[trace(static)]
        pub subrule: Option<Vec<Label>>,
        /// The rule or aspect context value this factory belongs to, set once
        /// that value exists (`cc_internal.actions2ctx_cheat`,
        /// CcStarlarkInternal.java:486-494). `None` on a subrule factory and
        /// until the context value is allocated.
        pub rule_ctx: RefCell<Option<Value<'v>>>,
    }
}
use owned::Actions;
pub(crate) use owned::Context;

impl Drop for Context {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.scope.finish();
    }
}

impl Context {
    /// An accessed `ctx.outputs.executable` is a declared output
    /// (RuleContext.createOutputArtifact).
    fn sync_default_executable(&self) {
        if let Some(default) = &self.runfiles.default_executable
            && default.created()
        {
            let mut declared = self.declared.borrow_mut();
            if !declared.contains(&default.file) {
                declared.push(default.file.clone());
            }
        }
    }

    /// The declared outputs, including an accessed `ctx.outputs.executable`.
    pub(crate) fn take_declared(&self) -> Vec<File> {
        self.sync_default_executable();
        std::mem::take(&mut *self.declared.borrow_mut())
    }

    pub(crate) fn declare(&self, name: &str) -> anyhow::Result<File> {
        self.declare_kind(name, false)
    }

    /// Declare a regular file or a tree artifact (directory) output.
    pub(crate) fn declare_kind(&self, name: &str, directory: bool) -> anyhow::Result<File> {
        self.declare_artifact(name, directory, false)
    }

    /// Declare an unresolved symlink output (StarlarkActionFactory.declareSymlink,
    /// :194-225): a symlink artifact, distinct from a file or directory
    /// declared at the same path (a different SpecialArtifact, so such a pair
    /// leaves one of them without a generating action, like Bazel).
    pub(crate) fn declare_symlink(&self, name: &str) -> anyhow::Result<File> {
        self.declare_artifact(name, false, true)
    }

    fn declare_artifact(&self, name: &str, directory: bool, symlink: bool) -> anyhow::Result<File> {
        self.sync_default_executable();
        let output = self.label.with_name(name).map_err(anyhow::Error::msg)?;
        let file = File {
            is_directory: directory,
            is_symlink: symlink,
            path: output
                .generated_exec_path(&self.output_root)
                .to_string_lossy()
                .into_owned(),
            short_path: output.generated_short_path(),
            owner: self.label.to_string(),
            is_source: false,
        };
        let mut declared = self.declared.borrow_mut();
        // A path declared both as a file and as a directory is reported when
        // the implementation returns (StarlarkProviderValidationUtil), like Bazel.
        // An output path another target already produces is decided at the
        // commit (crate::engine register_output): the other target's equal
        // action is shared (Actions.canBeShared, crate::shared_actions),
        // anything else is Bazel's action conflict (W1b).
        if !declared.contains(&file) {
            declared.push(file.clone());
        }
        Ok(file)
    }

    /// A shareable artifact (StarlarkActionFactory.createShareableArtifact,
    /// analysis/starlark/StarlarkActionFactory.java:1267-1276 over
    /// RuleContext.getShareableArtifact :711-713 and
    /// CachingAnalysisEnvironment.getDerivedArtifact :262-267): `path` is
    /// relative to the artifact root `root` (the exec path of a derived
    /// root), not to the package, and normalized like PathFragment.create.
    /// The rule owns it; the same path declared twice (or the same exec
    /// path declared with `declare_file`) is the same artifact
    /// (dedupAndTrackArtifactAndOrigin :241-259). Its short path follows
    /// the one rule of generated Files (crate::label::generated_short_path:
    /// `external/r/x` is `../r/x` for a main-repository rule too), so the
    /// engine's Artifact of it is this very File (review F2 of d2fd11c).
    pub(crate) fn declare_shareable(&self, root: &str, path: &str) -> anyhow::Result<File> {
        self.sync_default_executable();
        let relative = shareable_relative_path(path)?;
        let file = File {
            is_directory: false,
            is_symlink: false,
            path: if relative.is_empty() {
                root.to_owned()
            } else {
                format!("{root}/{relative}")
            },
            short_path: crate::label::generated_short_path(&relative),
            owner: self.label.to_string(),
            is_source: false,
        };
        let mut declared = self.declared.borrow_mut();
        // As for declare_kind: an output path another target already
        // produces is shared or a conflict at the commit (crate::engine
        // register_output; W1b).
        if !declared.contains(&file) {
            declared.push(file.clone());
            // A package-relative declaration of the same exec path is the
            // same artifact; only a fresh shareable declaration skips the
            // package checks.
            self.shareable.borrow_mut().insert(file.path.clone());
        }
        Ok(file)
    }

    /// Whether a declared output is a shareable artifact
    /// ([`Context::declare_shareable`]).
    pub(crate) fn is_shareable(&self, file: &File) -> bool {
        self.shareable.borrow().contains(&file.path)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register(
        &self,
        outputs: Vec<File>,
        inputs: Vec<File>,
        command: String,
        write: Option<WriteAction>,
        program: Option<String>,
        arguments: Vec<String>,
        environment: BTreeMap<String, String>,
        shell_pad: bool,
        executable_file: bool,
    ) -> anyhow::Result<()> {
        self.register_action(Registration {
            outputs,
            inputs,
            command,
            write,
            program,
            arguments,
            environment,
            extra: ActionExtra::default(),
            shell_pad,
            executable_file,
            deferred: None,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            runfiles_trees: Vec::new(),
            build_info: None,
            shareable: true,
        })
    }

    pub(crate) fn register_action(&self, registration: Registration) -> anyhow::Result<()> {
        let Registration {
            outputs,
            inputs,
            command,
            write,
            program,
            arguments,
            environment,
            mut extra,
            shell_pad,
            executable_file,
            deferred,
            exec_group,
            runfiles_trees,
            build_info,
            shareable,
        } = registration;
        ensure!(
            !outputs.is_empty(),
            "an action must have at least one output"
        );
        self.sync_default_executable();
        let declared = self.declared.borrow();
        let mut seen = BTreeSet::new();
        let mut actions = self.actions.borrow_mut();
        // The first output that already has a generating action of this
        // target (checked once the action is built).
        let mut generated = None;
        for output in &outputs {
            ensure!(
                !output.is_source
                    && output.owner == self.label.to_string()
                    && declared.contains(output),
                "action output {} is not declared by {}",
                output.path,
                self.label
            );
            ensure!(
                seen.insert(output.path.clone()),
                "duplicate action output {}",
                output.path
            );
            if generated.is_none()
                && let Some(index) = actions
                    .iter()
                    .position(|action| action.outputs.contains(output))
            {
                generated = Some((index, output.clone()));
            }
        }
        // Inputs are Artifacts, whatever path the File took to reach this
        // implementation (a dependency, a provider field, a closure; Bazel
        // has no input restriction): the engine resolves each through the
        // build's artifact index at commit (W1b), the workspace status
        // files through the build's BazelWorkspaceStatusAction (N63).
        extra.tree_outputs = outputs
            .iter()
            .filter(|file| file.is_directory)
            .map(|file| file.path.clone().into())
            .collect();
        extra.symlink_outputs = outputs
            .iter()
            .filter(|file| file.is_symlink)
            .map(|file| file.path.clone().into())
            .collect();
        let mut action = Action {
            id: format!("{}#{}", self.action_owner, actions.len()),
            command,
            inputs: Vec::new(),
            outputs: outputs
                .iter()
                .map(|file| file.path.clone().into())
                .collect(),
            write,
            program,
            shell_strict: false,
            shell_pad,
            executable_file,
            arguments,
            environment,
            extra,
            ..Action::default()
        };
        // Validate with the inputs at their execution paths; the engine maps
        // them to artifacts (plan_action) and validates again.
        action.inputs = inputs
            .iter()
            .map(|file| (file.path.clone().into(), file.path.clone().into()))
            .collect();
        action.extra.tree_inputs = inputs
            .iter()
            .filter(|file| file.is_directory)
            .map(|file| file.path.clone().into())
            .collect();
        crate::action::validate_structure(&action).map_err(anyhow::Error::msg)?;
        action.inputs.clear();
        action.extra.tree_inputs.clear();
        // A spawn carries the rule's execution-requirement tags
        // (TargetUtils.getFilteredExecutionInfo); writes and native
        // transforms have no execution info in Bazel's key.
        if action.write.is_none() && action.extra.transform.is_none() {
            action.execution_info = self.propagated_tags.clone();
        }
        let key = crate::shared_actions::ActionKey::of(
            &action,
            &crate::shared_actions::KeyedParts {
                inputs: &inputs,
                outputs: &outputs,
                runfiles_trees: &runfiles_trees,
                execution: self.exec_groups.keyed_execution(&exec_group),
                deferred: deferred.is_some(),
                build_info: build_info.is_some(),
                shareable,
            },
        );
        let registered = RegisteredAction {
            action,
            inputs,
            outputs,
            deferred,
            exec_group,
            runfiles_trees,
            build_info,
            key,
            repo_mapping: None,
        };
        if let Some((index, output)) = generated {
            // Actions.assignOwnersAndThrowIfConflictToleratingSharedActions
            // over one target's actions (actions/Actions.java:170-284): an
            // output generated twice is fine when the two actions can be
            // shared (Actions.canBeShared :85-115, crate::shared_actions),
            // so the second registration is the first; otherwise
            // ActionConflictException's message, whose further lines name
            // JVM action classes and keys. Equal in every compared part,
            // but with a part the native key does not cover, the actions
            // may be identical (and shared by Bazel) or not (a conflict):
            // neither is guessed.
            let label = self.label.to_string();
            return match registered.key.sharing(&actions[index].key) {
                crate::shared_actions::Sharing::Shared => Ok(()),
                crate::shared_actions::Sharing::Conflict => bail!(
                    "{}",
                    crate::shared_actions::conflict_message(&output.path, &label, &label)
                ),
                crate::shared_actions::Sharing::Unmodelled(reason) => bail!(
                    "{}",
                    crate::shared_actions::unmodelled_message(&output.path, &label, &label, reason)
                ),
            };
        }
        actions.push(registered);
        Ok(())
    }
}

impl Context {
    /// PyBuiltins.repoMappingAction (rules/python/PyBuiltins.java:157-181): a
    /// RepoMappingManifestAction (analysis/RepoMappingManifestAction.java, a
    /// file write) of `runfiles` to `output`. The content lists the
    /// mappings of the target's transitive repositories relevant to the
    /// runfiles (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest),
    /// known at the commit, which fills it in (crate::engine); the
    /// registration's placeholder content names the runfiles' entries, so
    /// two registrations of one output compare like their runfiles.
    pub(crate) fn register_repo_mapping_manifest(
        &self,
        output: File,
        runfiles: crate::runfiles::Runfiles,
    ) -> anyhow::Result<()> {
        let mut placeholder = b"RepoMappingManifest\n".to_vec();
        for file in runfiles.artifacts() {
            placeholder.extend_from_slice(format!("{}\t{}\n", file.owner, file.path).as_bytes());
        }
        for root in [false, true] {
            for (path, file) in runfiles.symlink_entries(root) {
                placeholder.extend_from_slice(
                    format!("{root}\t{path}\t{}\t{}\n", file.owner, file.path).as_bytes(),
                );
            }
        }
        self.register(
            vec![output.clone()],
            Vec::new(),
            String::new(),
            Some(WriteAction {
                content: placeholder,
                executable: false,
            }),
            None,
            Vec::new(),
            BTreeMap::new(),
            false,
            false,
        )?;
        let mut actions = self.actions.borrow_mut();
        let registered = actions
            .iter_mut()
            .rev()
            .find(|action| action.outputs.contains(&output))
            .ok_or_else(|| anyhow!("internal error: no action writes {}", output.path))?;
        if registered.repo_mapping.is_none() {
            registered.repo_mapping = Some(runfiles);
        }
        Ok(())
    }
}

/// `py_internal.create_repo_mapping_manifest(ctx=, runfiles=, output=)`: the
/// action registered on `ctx`'s analysis (its `actions` factory must be the
/// current target's).
pub(crate) fn create_repo_mapping_manifest<'v>(
    ctx: Value<'v>,
    runfiles: &crate::runfiles::Runfiles,
    output: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> starlark::Result<()> {
    let actions = ctx.get_attr_error("actions", eval.heap())?;
    let context = action_receiver(actions, eval, false).map_err(starlark::Error::new_other)?;
    let output = unpack_file(output).map_err(starlark::Error::new_other)?;
    context
        .register_repo_mapping_manifest(output, runfiles.clone())
        .map_err(starlark::Error::new_other)
}

/// One action registration (crate::analysis_runtime::Context::register_action).
pub(crate) struct Registration {
    pub outputs: Vec<File>,
    pub inputs: Vec<File>,
    pub command: String,
    pub write: Option<WriteAction>,
    pub program: Option<String>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub extra: ActionExtra,
    /// An empty `$0` before the shell's positional parameters
    /// (crate::action::Action::shell_pad, N41).
    pub shell_pad: bool,
    /// `program` is a File's execution path (crate::action::Action::executable_file).
    pub executable_file: bool,
    pub deferred: Option<Deferred>,
    /// The exec group (N35): `default-exec-group` unless run/run_shell chose
    /// one (crate::exec_groups::ActionExecGroups::determine).
    pub exec_group: String,
    /// Runfiles trees of the spawn's tools (N23f), shared ([`tool_runfiles_tree`]).
    pub runfiles_trees: Vec<Arc<ToolRunfilesTree>>,
    /// An actions.transform_{version,info}_file registration (N63).
    pub build_info: Option<BuildInfoAction>,
    /// ActionAnalysisMetadata.isShareable: whether an equal action of
    /// another owner may share this one's outputs (Actions.canBeShared,
    /// crate::shared_actions). Every action is but a C++ compile
    /// (CppCompileAction, not shareable unless asked:
    /// CppCompileActionBuilder.java:99).
    pub shareable: bool,
}

/// The runfiles tree of an action tool or executable, staged into the spawn
/// below `<executable>.runfiles` (SpawnInputExpander.addSingleRunfilesTreeToInputs,
/// exec/SpawnInputExpander.java:87-135): every mapping entry, never the
/// MANIFEST (:212).
///
/// One shared value per tool (PA-1): every action that uses a tool holds
/// the same `Arc` ([`tool_runfiles_tree`]), as Bazel's spawns hold the one
/// runfiles tree artifact of the tool; the planner expands it into one
/// shared input group (crate::scheduler::InputGroup).
///
/// Constructor-only ([`ToolRunfilesTree::of`]; private fields): the
/// identity is the FilesToRunProvider's
/// (crate::runfiles::FilesToRun::identity) and the directory and mapping
/// are computed from that provider, which never changes once constructed,
/// so one identity is one tree content — what the memos keyed by identity
/// rely on ([`tool_runfiles_tree`], the planner's input groups in
/// crate::engine; both check it on a hit in debug builds). Like the
/// provider identity, a tree identity is meaningful in this process only
/// and must never be persisted.
#[derive(Debug)]
pub struct ToolRunfilesTree {
    identity: u64,
    directory: String,
    mapping: crate::runfiles::RunfilesMapping,
    /// crate::shared_actions::tree_digest of the directory and mapping.
    digest: [u8; 32],
}

impl ToolRunfilesTree {
    /// The runfiles tree of `files_to_run`: `None` without an executable
    /// or with empty runfiles.
    fn of(files_to_run: &crate::runfiles::FilesToRun) -> Option<Self> {
        let runfiles = files_to_run
            .runfiles()
            .filter(|runfiles| !runfiles.is_empty())?;
        let executable = files_to_run.executable()?;
        Some(Self::new(
            files_to_run.identity(),
            format!("{}.runfiles", executable.path),
            runfiles.runfiles_inputs(files_to_run.repo_mapping_manifest(), None),
        ))
    }

    fn new(identity: u64, directory: String, mapping: crate::runfiles::RunfilesMapping) -> Self {
        let digest = crate::shared_actions::tree_digest(&directory, &mapping);
        Self {
            identity,
            directory,
            mapping,
            digest,
        }
    }

    /// A tree of the given parts (unit tests of its consumers).
    #[cfg(test)]
    pub(crate) fn for_test(
        identity: u64,
        directory: &str,
        mapping: crate::runfiles::RunfilesMapping,
    ) -> Self {
        Self::new(identity, directory.to_owned(), mapping)
    }

    /// The identity of the FilesToRunProvider the tree is of: equal
    /// identities are the same tree (in this process).
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// The exec path of the runfiles directory.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Runfiles path -> File (`None`: an empty file), in path order.
    pub fn mapping(&self) -> &crate::runfiles::RunfilesMapping {
        &self.mapping
    }

    /// The tree's part of the key of every action using it
    /// (crate::shared_actions::tree_digest; PA-1 follow-up B4): computed
    /// once with the tree.
    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Whether `other` has this tree's content: what a memo hit by
    /// identity guarantees (debug-build checks).
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        self.directory == other.directory && self.mapping == other.mapping
    }
}

/// The runfiles trees computed in this process, by FilesToRunProvider
/// identity ([`tool_runfiles_tree`]). Weak: a tree lives as long as an
/// action registration or the planner's input groups hold it (the planner
/// holds every tree it expanded until the end of planning), and is computed
/// again after that (a memo only saves work).
fn tool_trees() -> &'static Mutex<std::collections::HashMap<u64, std::sync::Weak<ToolRunfilesTree>>>
{
    static TREES: std::sync::OnceLock<
        Mutex<std::collections::HashMap<u64, std::sync::Weak<ToolRunfilesTree>>>,
    > = std::sync::OnceLock::new();
    TREES.get_or_init(Default::default)
}

pub(crate) fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow!("ctx.actions and subrules are only available during rule analysis"))
}

pub(crate) fn is_top_level_mapper(eval: &Evaluator<'_, '_, '_>, value: Value<'_>) -> bool {
    owned_top_level_function(eval, value).is_some()
}

/// The frozen top-level function a map_each value is, usable after analysis
/// (execution-time expansion of tree artifacts).
pub(crate) fn owned_top_level_function(
    eval: &Evaluator<'_, '_, '_>,
    value: Value<'_>,
) -> Option<OwnedFrozenValue> {
    context(eval).ok().and_then(|context| {
        context
            .top_level_functions
            .iter()
            .find(|function| function.value().ptr_eq(value))
            .cloned()
            .or_else(|| {
                context
                    .loaded_functions
                    .as_ref()
                    .and_then(|functions| functions.find(value))
            })
    })
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    context(eval)
        .ok()
        .map(|context| context.repositories.clone())
}

/// StarlarkActionFactory.repr (analysis/starlark/StarlarkActionFactory.java:1284-1287):
/// "actions for" immediately followed by the context's repr.
impl fmt::Display for Actions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "actions for{}", self.context)
    }
}

#[cfg(test)]
pub(crate) fn alloc_actions<'v>(heap: Heap<'v>, scope: ScopeHandle) -> Value<'v> {
    alloc_actions_for(
        heap,
        scope,
        crate::analysis_ctx::rule_context_repr("//:test"),
    )
}

/// `context` is the repr of the owning rule or subrule context.
pub(crate) fn alloc_actions_for<'v>(
    heap: Heap<'v>,
    scope: ScopeHandle,
    context: String,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(Actions {
        scope,
        context,
        subrule: None,
        rule_ctx: RefCell::new(None),
    })
}

/// A subrule's action factory with the subrule's requested toolchain types.
pub(crate) fn alloc_subrule_actions<'v>(
    heap: Heap<'v>,
    scope: ScopeHandle,
    context: String,
    toolchains: Vec<Label>,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(Actions {
        scope,
        context,
        subrule: Some(toolchains),
        rule_ctx: RefCell::new(None),
    })
}

/// The rule or aspect context an action factory belongs to
/// (`cc_internal.actions2ctx_cheat`).
pub(crate) fn actions_rule_ctx<'v>(actions: Value<'v>) -> Option<Value<'v>> {
    actions
        .downcast_ref::<Actions<'v>>()
        .and_then(|factory| *factory.rule_ctx.borrow())
}

/// Bind `ctx` as the owning context of an action factory
/// (`Actions.rule_ctx`; actions2ctx_cheat).
pub(crate) fn set_rule_ctx<'v>(actions: &Value<'v>, ctx: Value<'v>) {
    if let Some(factory) = actions.downcast_ref::<Actions<'v>>() {
        *factory.rule_ctx.borrow_mut() = Some(ctx);
    }
}

/// The exec group arguments of `ctx.actions.run`/`run_shell`
/// (StarlarkActionFactoryApi: `exec_group` string or None, `toolchain`
/// Label, string or None, default unbound) with the subrule overrides.
struct GroupArguments {
    exec_group: Option<String>,
    toolchain: crate::exec_groups::ToolchainArgument,
    groups: crate::exec_groups::ActionExecGroups,
}

fn group_arguments(
    this: Value<'_>,
    context: &Context,
    function: &str,
    exec_group: Option<Value<'_>>,
    toolchain: Option<Value<'_>>,
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<GroupArguments> {
    use crate::exec_groups::ToolchainArgument;
    let exec_group = match exec_group {
        None => None,
        Some(value) if value.is_none() => None,
        Some(value) => Some(
            value
                .unpack_str()
                .ok_or_else(|| {
                    anyhow!(
                        "in call to {function}(), parameter 'exec_group' got value of type '{}', want 'string or NoneType'",
                        value.get_type()
                    )
                })?
                .to_owned(),
        ),
    };
    let toolchain = match toolchain {
        None => ToolchainArgument::Unbound,
        Some(value) if value.is_none() => ToolchainArgument::None,
        Some(value) => {
            if let Some(label) = crate::starlark_types::unpack_label(value) {
                ToolchainArgument::Label(
                    Label::parse_canonical(&label).map_err(anyhow::Error::msg)?,
                )
            } else if let Some(text) = value.unpack_str() {
                // LabelConverter.forBzlEvaluatingThread: the calling .bzl's
                // repository mapping and package
                // (StarlarkActionFactory.determineExecGroup, :861-871).
                ToolchainArgument::Label(crate::toolchain_info::parse_bzl_label(
                    &context.repositories,
                    text,
                    &|| eval.call_stack_top_location(),
                )?)
            } else {
                bail!(
                    "in call to {function}(), parameter 'toolchain' got value of type '{}', want 'Label, string, or NoneType'",
                    value.get_type()
                );
            }
        }
    };
    let receiver = this
        .downcast_ref::<Actions>()
        .ok_or_else(|| anyhow!("expected actions receiver"))?;
    let mut groups = context.exec_groups.clone();
    groups.subrule = receiver.subrule.clone();
    let toolchain = groups.override_arguments(exec_group.as_deref(), toolchain)?;
    Ok(GroupArguments {
        exec_group,
        toolchain,
        groups,
    })
}

impl GroupArguments {
    /// checkToolchainParameterIsSet for an executable or tool that is not an
    /// executable attribute's (StarlarkActionFactory.java:433-454, 764-800).
    fn check_tool(&self) -> anyhow::Result<()> {
        self.groups
            .check_toolchain_parameter(self.exec_group.as_deref(), &self.toolchain)
    }

    fn determine(&self) -> anyhow::Result<String> {
        self.groups
            .determine(self.exec_group.as_deref(), &self.toolchain)
    }
}

fn action_receiver<'a>(
    this: Value<'_>,
    eval: &'a Evaluator<'_, '_, '_>,
    ancestor: bool,
) -> anyhow::Result<&'a Context> {
    let receiver = this
        .downcast_ref::<Actions>()
        .ok_or_else(|| anyhow!("expected actions receiver"))?;
    let context = context(eval)?;
    if ancestor {
        receiver.scope.live(&context.scope)?;
    } else {
        receiver.scope.current(&context.scope)?;
    }
    Ok(context)
}

starlark::methods_static!(ACTION_METHODS = all_action_methods);

fn all_action_methods(builder: &mut MethodsBuilder) {
    action_methods(builder);
    direct_action_methods(builder);
    file_action_methods(builder);
}

#[starlark_value(type = "actions")]
impl<'v> StarlarkValue<'v> for Actions<'v> {
    fn get_methods() -> Option<&'static Methods> {
        Some(ACTION_METHODS.methods())
    }
}

/// A `string or NoneType` action parameter (StarlarkActionFactoryApi:
/// `mnemonic`/`progress_message` are declared string-or-None): an absent
/// or explicit-None value is None; anything else must be a string.
fn string_or_none(
    value: Option<Value<'_>>,
    function: &str,
    parameter: &str,
) -> anyhow::Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_none() => Ok(None),
        Some(value) => value
            .unpack_str()
            .map(|text| Some(text.to_owned()))
            .ok_or_else(|| {
                anyhow!(
                    "in call to {function}(), parameter '{parameter}' got value of type '{}', want 'string or NoneType'",
                    value.get_type()
                )
            }),
    }
}

fn string_dict(value: Value<'_>, what: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let dict = DictRef::from_value(value)
        .ok_or_else(|| anyhow!("{what} must be a string-to-string dict"))?;
    dict.iter()
        .map(|(key, value)| {
            Ok((
                key.unpack_str()
                    .ok_or_else(|| anyhow!("{what} keys must be strings"))?
                    .to_owned(),
                value
                    .unpack_str()
                    .ok_or_else(|| anyhow!("{what} values must be strings"))?
                    .to_owned(),
            ))
        })
        .collect()
}

/// SpawnAction.createActionEnvironment (analysis/actions/SpawnAction.java:584-614)
/// in the configured target's configuration (crate::action_env): `env` alone,
/// or the configuration's shell environment (its inherited names that `env`
/// sets removed) overridden by `env`.
fn spawn_environment(
    context: &Context,
    env: Option<Value<'_>>,
    use_default_shell_env: bool,
) -> anyhow::Result<crate::action_env::ActionEnvironment> {
    let env = match env {
        Some(env) if !env.is_none() => string_dict(env, "env")?,
        _ => BTreeMap::new(),
    };
    Ok(context.action_env.for_spawn(use_default_shell_env, env))
}

/// Common Starlark spawn options of run/run_shell
/// (StarlarkActionFactory.registerStarlarkAction, :724-860).
struct SpawnOptions<'v> {
    outputs: Value<'v>,
    inputs: Option<Value<'v>>,
    tools: Option<Value<'v>>,
    arguments: Option<Value<'v>>,
    env: Option<Value<'v>>,
    use_default_shell_env: bool,
    execution_requirements: Option<Value<'v>>,
    input_manifests: Option<Value<'v>>,
    /// The exec group arguments, already type-checked (N35).
    groups: GroupArguments,
    progress_message: Option<String>,
    /// The spawn mnemonic (StarlarkActionFactory.getMnemonic with the
    /// "Action" default, :811; crate::action::ActionExtra::mnemonic).
    mnemonic: String,
    /// `run(unused_inputs_list = ...)` (docs/UNUSED_INPUTS_LIST.md).
    unused_inputs_list: Option<File>,
}

/// Register a Starlark spawn: `executable` is Bazel's executable command-line
/// part (for the parameter-file threshold), `program`/`command` the native spawn.
#[allow(clippy::too_many_arguments)]
fn register_spawn(
    context: &Context,
    options: SpawnOptions<'_>,
    mut inputs: Vec<File>,
    mut runfiles_trees: Vec<Arc<ToolRunfilesTree>>,
    executable: Vec<String>,
    command: String,
    program: Option<String>,
    function: &str,
    (shell_pad, executable_file): (bool, bool),
) -> anyhow::Result<()> {
    // `input_manifests`: "Legacy argument. Ignored." (sequence or None,
    // starlarkbuildapi/StarlarkActionFactoryApi.java:536-548).
    if let Some(manifests) = options.input_manifests
        && !manifests.is_none()
        && crate::custom_rule::sequence_ref(manifests).is_none()
    {
        bail!(
            "in call to {function}(), parameter 'input_manifests' got value of type '{}', want 'sequence or NoneType'",
            manifests.get_type()
        );
    }
    ensure!(
        ListRef::from_value(options.outputs).is_some(),
        "outputs must be a list of Files"
    );
    let outputs = file_list(options.outputs)?;
    ensure!(!outputs.is_empty(), "param 'outputs' may not be empty");
    inputs.extend(
        options
            .inputs
            .map(file_list)
            .transpose()?
            .unwrap_or_default(),
    );
    if let Some(tools) = options.tools {
        let (files, trees) = action_tools(context, tools, function, &options.groups)?;
        inputs.extend(files);
        runfiles_trees.extend(trees);
    }
    // StarlarkActionFactory.determineExecGroup after the tools (N35).
    let exec_group = options.groups.determine()?;
    let parts = match options.arguments {
        Some(arguments) => crate::args::command_line_parts(arguments)?,
        None => Vec::new(),
    };
    // VectorArg.expandDirectories (StarlarkCustomCommandLine.java:456-485): an
    // expanded directory that is not an input fails when the action executes,
    // prefixed by the action's progress message (AbstractAction.java:380-400,
    // default "Action %{output}" for Starlark spawns).
    let progress = options
        .progress_message
        .as_deref()
        .unwrap_or("Action %{output}")
        .replace("%{label}", &context.label.to_string())
        .replace("%{output}", crate::runfiles::root_relative(&outputs[0]));
    let spec = CommandLineSpec {
        executable,
        parts,
        primary_output: outputs[0].path.clone(),
        max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
    };
    let environment = spawn_environment(context, options.env, options.use_default_shell_env)?;
    let mut execution_info = match options.execution_requirements {
        Some(value) if !value.is_none() => string_dict(value, "execution_requirements")?,
        _ => BTreeMap::new(),
    };
    if let Some(list) = &options.unused_inputs_list {
        // StarlarkAction.Builder.createSpawnAction (StarlarkAction.java:
        // 177-189): "Always download unused_inputs_list file from remote
        // cache" (ExecutionRequirements.REMOTE_EXECUTION_INLINE_OUTPUTS).
        execution_info.insert("internal-inline-outputs".to_owned(), list.path.clone());
    }
    let extra = ActionExtra {
        inherited_environment: environment.inherited,
        // As given: the shared-action key filters it to the legal keys and
        // merges the propagated tags like TargetUtils.getFilteredExecutionInfo
        // (crate::shared_actions).
        execution_info,
        mnemonic: options.mnemonic,
        ..ActionExtra::default()
    };
    let environment = environment.fixed;
    let (arguments, param_files, deferred) = if spec.deferred() {
        if spec.deferred_map_each() {
            crate::observed_fs::taint(crate::observed_fs::Taint::MapEach, || {
                format!("{} ({progress})", context.label)
            });
        }
        let deferred = Deferred::new(crate::scheduler::DeferredSpec::CommandLine {
            spec,
            progress,
            extra_param_files: Vec::new(),
        });
        (Vec::new(), Vec::new(), Some(deferred))
    } else {
        let (arguments, files) = spec.expand(None).map_err(anyhow::Error::msg)?;
        (arguments, files, None)
    };
    context.register_action(Registration {
        outputs,
        inputs,
        command,
        write: None,
        program,
        arguments,
        environment,
        extra: ActionExtra {
            param_files,
            ..extra
        },
        shell_pad,
        executable_file,
        deferred,
        exec_group,
        runfiles_trees,
        build_info: None,
        shareable: true,
    })
}

/// The runfiles tree a FilesToRunProvider with runfiles support attaches to
/// a spawn (StarlarkAction.Builder.addTool(FilesToRunProvider) adds the
/// runfiles tree artifact, RuleConfiguredTargetBuilder.java:144-152).
/// Computed once per provider (by identity) while any action or the
/// planner holds it: a tool used by thousands of actions (the Python
/// interpreter of every PyCompile action) is one mapping, not one per
/// action. The memo relies on one identity being one content
/// ([`ToolRunfilesTree`]); debug builds recompute the tree on every hit and
/// check.
pub(crate) fn tool_runfiles_tree(
    files_to_run: &crate::runfiles::FilesToRun,
) -> Option<Arc<ToolRunfilesTree>> {
    // A provider without a tree has no memo entry to look up.
    files_to_run.runfiles().filter(|r| !r.is_empty())?;
    files_to_run.executable()?;
    let identity = files_to_run.identity();
    let lock = || tool_trees().lock().unwrap_or_else(|e| e.into_inner());
    let known = lock().get(&identity).and_then(std::sync::Weak::upgrade);
    if let Some(tree) = known {
        debug_assert!(
            ToolRunfilesTree::of(files_to_run).is_some_and(|fresh| fresh.same_content(&tree)),
            "internal error: the memoized runfiles tree of FilesToRunProvider {identity} ({}) differs from its provider's",
            tree.directory
        );
        return Some(tree);
    }
    // Computed outside the lock: two threads may both compute one tree, the
    // same value either way.
    let tree = Arc::new(ToolRunfilesTree::of(files_to_run)?);
    let mut trees = lock();
    // Forget the trees nobody holds any more before the table grows.
    if trees.len() >= 1024 && trees.len().is_power_of_two() {
        trees.retain(|_, tree| tree.strong_count() > 0);
    }
    trees.insert(identity, Arc::downgrade(&tree));
    Some(tree)
}

fn declared_name(context: &Context, filename: &str, sibling: Value<'_>) -> anyhow::Result<String> {
    if sibling.is_none() {
        return Ok(filename.to_owned());
    }
    let sibling = unpack_file(sibling)?;
    // Any File may anchor the declaration (StarlarkActionFactory.declareFile
    // :142-163 has no "known File" gate, nor has the native profile since
    // W1b); the result must stay in the current package, checked natively
    // through the sibling's owner below.
    let owner = Label::parse_canonical(&sibling.owner).map_err(anyhow::Error::msg)?;
    ensure!(
        owner.package_id() == context.label.package_id(),
        "sibling must be in the current package"
    );
    let exec = if sibling.is_source {
        std::path::Path::new(&sibling.path)
    } else {
        // Relative to the sibling's own configuration root.
        crate::label::split_generated_root(&sibling.path)
            .map(|(_, relative)| std::path::Path::new(relative))
            .ok_or_else(|| anyhow!("invalid generated sibling execution path"))?
    };
    let relative = exec
        .strip_prefix(context.label.package_exec_path())
        .map_err(|_| anyhow!("sibling is outside the current package"))?;
    Ok(relative
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""))
        .join(filename)
        .to_string_lossy()
        .into_owned())
}

/// The root-relative path of a shareable artifact: PathFragment.create's
/// normalization (crate::ignored::normalize_path_fragment), then
/// ArtifactFactory.validatePath (actions/ArtifactFactory.java:303-324). An
/// absolute path or one with uplevel references fails a Preconditions check
/// there, which genuine Bazel 9.3 does not catch (an internal
/// IllegalArgumentException crashes the server); natively an explicit error.
fn shareable_relative_path(path: &str) -> anyhow::Result<String> {
    let normalized = crate::ignored::normalize_path_fragment(path);
    ensure!(
        !normalized.starts_with('/'),
        "declare_shareable_artifact('{path}'): the path must be relative to the artifact root (Bazel 9.3 fails with an internal IllegalArgumentException)"
    );
    ensure!(
        !normalized.split('/').any(|segment| segment == ".."),
        "declare_shareable_artifact('{path}'): the path must not contain uplevel references (Bazel 9.3 fails with an internal IllegalArgumentException)"
    );
    ensure!(
        !normalized.is_empty(),
        "declare_shareable_artifact('{path}'): the path names the artifact root itself, which cannot be an output of the native profile"
    );
    Ok(normalized)
}

/// An explicit `artifact_root` of declare_shareable_artifact: a derived
/// root (`ctx.bin_dir`, `ctx.genfiles_dir` or a generated File's `root`).
/// Genuine Bazel 9.3 casts the value to ArtifactRoot without a check (a
/// ClassCastException for any other value) and fails validatePath's
/// Preconditions for a source root (IllegalArgumentException); both crash
/// the server, natively they are explicit errors.
fn shareable_artifact_root(path: &str, value: Value<'_>) -> anyhow::Result<String> {
    if let Some(root) = crate::ctx_surface::Root::from_value(value) {
        return Ok(root.path.clone());
    }
    match crate::starlark_types::file_root(value) {
        Some((root, false)) => Ok(root.to_owned()),
        Some((_, true)) => bail!(
            "declare_shareable_artifact('{path}'): artifact_root is a source root; a shareable artifact needs a derived root (Bazel 9.3 fails with an internal IllegalArgumentException)"
        ),
        None => bail!(
            "declare_shareable_artifact('{path}'): artifact_root must be a root, not '{}' (Bazel 9.3 fails with an internal ClassCastException)",
            value.get_type()
        ),
    }
}

#[starlark_module]
fn action_methods(builder: &mut MethodsBuilder) {
    fn args<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        // KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE: expired-child
        // factories crash Bazel37. Reject safely, but permit live ancestors.
        let context = action_receiver(this, eval, true)?;
        Ok(crate::args::alloc_args(eval.heap(), context.active.clone()))
    }
    fn declare_file<'v>(
        this: Value<'v>,
        filename: &str,
        #[starlark(default = NoneType)] sibling: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = action_receiver(this, eval, false)?;
        let name = declared_name(context, filename, sibling)?;
        Ok(alloc_file(eval.heap(), context.declare(&name)?))
    }
    /// StarlarkActionFactory.declareDirectory (:166-192): a tree artifact.
    fn declare_directory<'v>(
        this: Value<'v>,
        filename: &str,
        #[starlark(default = NoneType)] sibling: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = action_receiver(this, eval, false)?;
        let name = declared_name(context, filename, sibling)?;
        Ok(alloc_file(eval.heap(), context.declare_kind(&name, true)?))
    }
    /// StarlarkActionFactory.createShareableArtifact (:1267-1276;
    /// StarlarkActionFactoryApi.java:1142-1151: `path` and `artifact_root`
    /// are positional-only, `artifact_root` defaults to unbound): restricted
    /// to the internal Starlark API allowlist
    /// (BuiltinRestriction.failIfCalledOutsideDefaultAllowlist, first), then
    /// a derived artifact `path` below `artifact_root` (default: the bin
    /// directory of the target's configuration, RuleContext.getBinDirectory
    /// :319-321) owned by the rule (Context::declare_shareable).
    fn declare_shareable_artifact<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] artifact_root: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        crate::subrule_fragments::private_api(eval).map_err(|e| anyhow!("{e}"))?;
        let context = action_receiver(this, eval, false)?;
        let root = match artifact_root {
            None => context.output_root.clone(),
            Some(value) => shareable_artifact_root(path, value)?,
        };
        Ok(alloc_file(
            eval.heap(),
            context.declare_shareable(&root, path)?,
        ))
    }
    /// StarlarkActionFactory.declareSymlink (:194-225): an unresolved
    /// symlink artifact (`--allow_unresolved_symlinks` defaults to true and
    /// is not a modelled option), relative to the package or to `sibling`'s
    /// directory. ctx.actions.symlink(target_path = ...) or a spawn creates
    /// it; executing such an action is an explicit native gap
    /// (docs/ACTIONS_PARAMS.md).
    fn declare_symlink<'v>(
        this: Value<'v>,
        filename: &str,
        #[starlark(default = NoneType)] sibling: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = action_receiver(this, eval, false)?;
        let name = declared_name(context, filename, sibling)?;
        Ok(alloc_file(eval.heap(), context.declare_symlink(&name)?))
    }
    fn write<'v>(
        this: Value<'v>,
        output: Value<'v>,
        content: Value<'v>,
        #[starlark(default = false)] is_executable: bool,
        #[starlark(require = named)] mnemonic: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        let _ = mnemonic;
        let output = unpack_file(output)?;
        ensure!(
            !output.is_directory,
            "actions.write output {} is a directory; write requires a file output",
            output.path
        );
        if let Some(text) = content.unpack_str() {
            // FileWriteAction writes Bazel's internal bytes (bazel_bytes.rs).
            context.register(
                vec![output],
                Vec::new(),
                String::new(),
                Some(WriteAction {
                    content: crate::bazel_bytes::internal_bytes(text).into_owned(),
                    executable: is_executable,
                }),
                None,
                Vec::new(),
                BTreeMap::new(),
                false,
                false,
            )?;
            return Ok(NoneType);
        }
        // ParameterFileWriteAction (StarlarkActionFactory.write, :356-410):
        // the Args' file type; its directories are inputs and expand when
        // the action executes.
        let snapshot = crate::args::snapshot(content)?
            .ok_or_else(|| anyhow!("write content must be a string or Args"))?;
        let directories = snapshot.directories();
        let (content, deferred) = if directories.is_empty() {
            (
                crate::args::serialize_write_content_of(&snapshot).map_err(anyhow::Error::msg)?,
                None,
            )
        } else {
            if crate::command_line::has_deferred_map_each(&snapshot.groups) {
                crate::observed_fs::taint(crate::observed_fs::Taint::MapEach, || {
                    format!("{} (write {})", context.label, output.path)
                });
            }
            let deferred = Deferred::new(crate::scheduler::DeferredSpec::WriteArgs {
                snapshot,
                extra_param_files: Vec::new(),
            });
            (Vec::new(), Some(deferred))
        };
        context.register_action(Registration {
            outputs: vec![output],
            inputs: directories,
            command: String::new(),
            write: Some(WriteAction {
                content,
                executable: is_executable,
            }),
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            extra: ActionExtra::default(),
            shell_pad: false,
            executable_file: false,
            deferred,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            runfiles_trees: Vec::new(),
            build_info: None,
            shareable: true,
        })?;
        Ok(NoneType)
    }
    fn run_shell<'v>(
        this: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        #[starlark(require = named)] input_manifests: Option<Value<'v>>,
        #[starlark(require = named)] exec_group: Option<Value<'v>>,
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<Value<'v>>,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        // Scheduling hints with no effect on a local spawn's results.
        let _ = resource_set;
        let mnemonic = string_or_none(mnemonic, "run_shell", "mnemonic")?;
        let progress_message = string_or_none(progress_message, "run_shell", "progress_message")?;
        let groups = group_arguments(this, context, "run_shell", exec_group, toolchain, eval)?;
        // runShell resolves the execution platform of a string command
        // first (StarlarkActionFactory.java:634-642, getExecutionPlatform:589-599).
        if let Some(group) = &groups.exec_group {
            groups.groups.verify_exec_group(group)?;
        }
        // ShellCommand (analysis/actions/ShellCommand.java:37-48): padded with
        // "" only when `arguments` is nonempty.
        let pad = arguments.is_some_and(|value| {
            ListRef::from_value(value).is_some_and(|list| !list.is_empty())
                || starlark::values::tuple::TupleRef::from_value(value)
                    .is_some_and(|tuple| tuple.len() != 0)
        });
        let mut executable = vec!["/bin/bash".to_owned(), "-c".to_owned(), command.to_owned()];
        if pad {
            executable.push(String::new());
        }
        register_spawn(
            context,
            SpawnOptions {
                outputs,
                inputs,
                tools,
                arguments,
                env,
                use_default_shell_env,
                execution_requirements,
                input_manifests,
                groups,
                progress_message,
                mnemonic: mnemonic.unwrap_or_else(|| "Action".to_owned()),
                // run_shell has no such parameter (StarlarkActionFactory.java:675).
                unused_inputs_list: None,
            },
            Vec::new(),
            Vec::new(),
            executable,
            command.to_owned(),
            None,
            "run_shell",
            (pad, false),
        )?;
        Ok(NoneType)
    }
}

/// The `tools` of run/run_shell (analysis/starlark/StarlarkActionFactory.java:765-806):
/// Files, FilesToRunProviders and depsets of Files. A FilesToRunProvider
/// with runfiles (or a File of an executable attribute whose provider has
/// them, StarlarkRuleContext.getExecutableRunfiles) attaches its runfiles
/// tree (N23f). Each tool that is not an executable attribute's is checked
/// against the action's exec group arguments (N35, GroupArguments::check_tool).
fn action_tools(
    context: &Context,
    tools: Value<'_>,
    function: &str,
    groups: &GroupArguments,
) -> anyhow::Result<(Vec<File>, Vec<Arc<ToolRunfilesTree>>)> {
    const WANT: &str = "expected value of type 'File, FilesToRunProvider or Depset of Files' for a member of parameter 'tools' but";
    let mut files = Vec::new();
    let mut trees = Vec::new();
    let attach = |files: &mut Vec<File>, trees: &mut Vec<Arc<ToolRunfilesTree>>, file: &File| {
        if let Some(files_to_run) = context.runfiles.executables.borrow().get(file) {
            files.extend(files_to_run.files().iter().cloned());
            trees.extend(tool_runfiles_tree(files_to_run));
        }
    };
    let items: Vec<Value<'_>> = if let Some(list) = ListRef::from_value(tools) {
        list.iter().collect()
    } else if let Some(tuple) = starlark::values::tuple::TupleRef::from_value(tools) {
        tuple.iter().collect()
    } else if crate::starlark_types::is_depset(tools) {
        // Depset.cast(tools, Object.class, "tools").toList(): File elements.
        let depset_files = file_list(tools).map_err(|_| {
            anyhow!(
                "{WANT} got a depset of '{}' instead",
                crate::starlark_types::depset_element_type(tools).unwrap_or("unknown")
            )
        })?;
        for file in &depset_files {
            if !context.runfiles.is_executable(file) {
                groups.check_tool()?;
            }
        }
        for file in depset_files {
            attach(&mut files, &mut trees, &file);
            files.push(file);
        }
        return Ok((files, trees));
    } else {
        bail!(
            "in call to {function}(), parameter 'tools' got value of type '{}', want 'sequence or depset'",
            tools.get_type()
        );
    };
    for item in items {
        if let Some(file) = File::from_value(item) {
            if !context.runfiles.is_executable(file) {
                groups.check_tool()?;
            }
            files.push(file.clone());
            attach(&mut files, &mut trees, file);
        } else if let Some(files_to_run) = crate::runfiles::FilesToRun::from_value(item) {
            if !context.runfiles.is_from_dependencies(files_to_run) {
                groups.check_tool()?;
            }
            files.extend(files_to_run.files().iter().cloned());
            trees.extend(tool_runfiles_tree(files_to_run));
        } else if crate::starlark_types::is_depset(item) {
            groups.check_tool()?;
            if let Some(kind) = crate::starlark_types::depset_element_type(item)
                && kind != "File"
            {
                bail!("{WANT} got a depset of '{kind}', expected a depset of 'File'");
            }
            files.extend(file_list(item)?);
        } else {
            bail!("{WANT} got {} instead", item.get_type());
        }
    }
    Ok((files, trees))
}

#[starlark_module]
fn direct_action_methods(builder: &mut MethodsBuilder) {
    fn run<'v>(
        this: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        #[starlark(require = named)] executable: Value<'v>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] unused_inputs_list: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        #[starlark(require = named)] input_manifests: Option<Value<'v>>,
        #[starlark(require = named)] exec_group: Option<Value<'v>>,
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<Value<'v>>,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // `unused_inputs_list`: File or NoneType (StarlarkActionFactoryApi
        // run, :423-438), bound before the call runs.
        let unused_inputs_list = match unused_inputs_list {
            Some(value) if !value.is_none() => Some(File::from_value(value).cloned().ok_or_else(
                || {
                    anyhow!(
                        "in call to run(), parameter 'unused_inputs_list' got value of type '{}', want 'File or NoneType'",
                        value.get_type()
                    )
                },
            )?),
            _ => None,
        };
        let context = action_receiver(this, eval, false)?;
        let _ = resource_set;
        let mnemonic = string_or_none(mnemonic, "run", "mnemonic")?;
        let progress_message = string_or_none(progress_message, "run", "progress_message")?;
        let groups = group_arguments(this, context, "run", exec_group, toolchain, eval)?;
        // The executable first (StarlarkActionFactory.run, :433-458).
        if let Some(file) = File::from_value(executable) {
            if !context.runfiles.is_executable(file) {
                groups.check_tool()?;
            }
        } else if let Some(files_to_run) = crate::runfiles::FilesToRun::from_value(executable)
            && !context.runfiles.is_from_dependencies(files_to_run)
        {
            groups.check_tool()?;
        }
        let mut extra_inputs = Vec::new();
        // A File (or FilesToRunProvider) executable runs by its execution
        // path from the execution root, argv[0] spelled like Bazel's
        // (crate::action::Action::executable_file, N41).
        let executable_file = executable.unpack_str().is_none();
        let mut trees = Vec::new();
        let (program, bazel_executable) = if let Some(program) = executable.unpack_str() {
            (program.to_owned(), program.to_owned())
        } else {
            // StarlarkAction.Builder.setExecutable(FilesToRunProvider) also
            // adds the provider's files to run (and its runfiles tree) as
            // tools; a File of an executable attribute uses its provider
            // (StarlarkActionFactory.java:433-441).
            let provider = match crate::runfiles::FilesToRun::from_value(executable) {
                Some(files_to_run) => Some(files_to_run.clone()),
                None => context
                    .runfiles
                    .executables
                    .borrow()
                    .get(&unpack_file(executable)?)
                    .cloned(),
            };
            if let Some(files_to_run) = &provider {
                extra_inputs.extend(files_to_run.files().iter().cloned());
                trees.extend(tool_runfiles_tree(files_to_run));
            }
            let file = match crate::runfiles::FilesToRun::from_value(executable) {
                Some(files_to_run) => files_to_run.executable().cloned().ok_or_else(|| {
                    anyhow!("actions.run executable FilesToRunProvider has no executable")
                })?,
                None => unpack_file(executable)?,
            };
            let path = file.path.clone();
            extra_inputs.push(file);
            (path.clone(), path)
        };
        register_spawn(
            context,
            SpawnOptions {
                outputs,
                inputs,
                tools,
                arguments,
                env,
                use_default_shell_env,
                execution_requirements,
                input_manifests,
                groups,
                progress_message,
                mnemonic: mnemonic.unwrap_or_else(|| "Action".to_owned()),
                unused_inputs_list,
            },
            extra_inputs,
            trees,
            vec![bazel_executable],
            String::new(),
            Some(program),
            "run",
            (false, executable_file),
        )?;
        Ok(NoneType)
    }
}

#[starlark_module]
fn file_action_methods(builder: &mut MethodsBuilder) {
    /// StarlarkActionFactory.symlink (:254-354). With target_file the output
    /// observes the target's bytes and permission bits (SymlinkAction). With
    /// target_path the output is an unresolved symlink artifact
    /// (declare_symlink) and the action an UnresolvedSymlinkAction
    /// (analysis/actions/UnresolvedSymlinkAction.java): mnemonic
    /// `UnresolvedSymlink`, keyed by the literal target and target type,
    /// no inputs; executing it is an explicit native gap
    /// (Transform::UnresolvedSymlink). `use_exec_root_for_source`
    /// (undocumented, default unbound) is restricted to the internal API
    /// allowlist whenever it is passed (:270-275); genuine links a source
    /// target through the execution root then (SymlinkAction.toArtifact
    /// :110-122), which the native profile has no persistent counterpart
    /// of: its symlink names the source file itself, the same file the
    /// execution root's entry is.
    fn symlink<'v>(
        this: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named, default = NoneType)] target_file: Value<'v>,
        #[starlark(require = named)] target_path: Option<Value<'v>>,
        #[starlark(require = named)] target_type: Option<Value<'v>>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        #[starlark(require = named)] use_exec_root_for_source: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // The `String or None` parameters are checked when the call binds
        // (StarlarkActionFactoryApi.symlink), before the body.
        let target_path = string_or_none(target_path, "symlink", "target_path")?;
        let target_type = string_or_none(target_type, "symlink", "target_type")?;
        let progress_message = string_or_none(progress_message, "symlink", "progress_message")?;
        let context = action_receiver(this, eval, false)?;
        if let Some(value) = use_exec_root_for_source {
            crate::subrule_fragments::private_api(eval).map_err(|e| anyhow!("{e}"))?;
            // `(Boolean) useExecRootForSourceObject`, unchecked.
            ensure!(
                value.unpack_bool().is_some(),
                "symlink(): use_exec_root_for_source must be a bool, not '{}' (Bazel 9.3 fails with an internal ClassCastException)",
                value.get_type()
            );
        }
        if target_file.is_none() == target_path.is_none() {
            bail!("Exactly one of \"target_file\" or \"target_path\" is required");
        }
        let output = unpack_file(output)?;
        // AbstractAction.getProgressMessage (actions/AbstractAction.java:380-400):
        // "Creating symlink %{output}" unless given. `progress_message`
        // is declared string-or-NoneType (StarlarkActionFactoryApi), so
        // an explicit None is the absent value, like run/run_shell.
        let progress = progress_message
            .unwrap_or_else(|| "Creating symlink %{output}".to_owned())
            .replace("%{label}", &context.label.to_string())
            .replace("%{output}", crate::runfiles::root_relative(&output));
        if let Some(target_path) = target_path {
            if !output.is_symlink {
                bail!(
                    "symlink() with \"target_path\" param requires that \"output\" be declared as a symlink, not a file or directory (did you mean to use declare_symlink() instead of declare_file() or declare_directory()?)"
                );
            }
            if is_executable {
                bail!("\"is_executable\" cannot be True when using \"target_path\"");
            }
            // SymlinkTargetType (vfs/SymlinkTargetType.java): UNSPECIFIED
            // unless "file" or "directory".
            let target_type = match target_type.as_deref() {
                None => crate::action::SymlinkTargetType::Unspecified,
                Some("file") => crate::action::SymlinkTargetType::File,
                Some("directory") => crate::action::SymlinkTargetType::Directory,
                Some(_) => bail!("\"target_type\" must be one of \"file\" or \"directory\""),
            };
            context.register_action(Registration {
                outputs: vec![output],
                inputs: Vec::new(),
                command: String::new(),
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                extra: ActionExtra {
                    // UnresolvedSymlinkAction.getMnemonic (:107-110).
                    mnemonic: "UnresolvedSymlink".to_owned(),
                    transform: Some(Transform::UnresolvedSymlink {
                        target: target_path,
                        target_type,
                        progress: progress.replace("%{input}", ""),
                    }),
                    ..ActionExtra::default()
                },
                deferred: None,
                exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
                shell_pad: false,
                executable_file: false,
                runfiles_trees: Vec::new(),
                build_info: None,
                shareable: true,
            })?;
            return Ok(NoneType);
        }
        if target_type.is_some() {
            bail!("\"target_type\" cannot be used with \"target_file\"");
        }
        let target = unpack_file(target_file)?;
        if output.is_symlink {
            bail!(
                "symlink() with \"target_file\" param requires that \"output\" be declared as a file or directory, not a symlink (did you mean to use declare_file() or declare_directory() instead of declare_symlink()?)"
            );
        }
        if target.is_directory != output.is_directory {
            let kind = |directory: bool| if directory { "directory" } else { "file" };
            bail!(
                "symlink() with \"target_file\" {0} param requires that \"output\" be declared as a {0} (did you mean to use declare_{0}() instead of declare_{1}()?)",
                kind(target.is_directory),
                kind(output.is_directory)
            );
        }
        if is_executable && output.is_directory {
            bail!("symlink() with \"output\" directory param cannot be executable");
        }
        let progress = progress.replace("%{input}", crate::runfiles::root_relative(&target));
        context.register_action(Registration {
            outputs: vec![output],
            inputs: vec![target.clone()],
            command: String::new(),
            write: None,
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            extra: ActionExtra {
                transform: Some(Transform::Symlink {
                    target: target.path.clone().into(),
                    require_executable: is_executable,
                    progress,
                }),
                ..ActionExtra::default()
            },
            deferred: None,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            shell_pad: false,
            executable_file: false,
            runfiles_trees: Vec::new(),
            build_info: None,
            shareable: true,
        })?;
        Ok(NoneType)
    }

    /// StarlarkActionFactory.expandTemplate (:1219-1254).
    fn expand_template<'v>(
        this: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named)] substitutions: Option<Value<'v>>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named)] computed_substitutions: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        let template = unpack_file(template)?;
        let output = unpack_file(output)?;
        ensure!(
            !template.is_directory && !output.is_directory,
            "expand_template template and output must be files, not directories"
        );
        let mut entries: Vec<(String, String)> = match substitutions {
            Some(value) => {
                let dict = DictRef::from_value(value).ok_or_else(|| {
                    anyhow!(
                        "in call to expand_template(), parameter 'substitutions' got value of type '{}', want 'dict'",
                        value.get_type()
                    )
                })?;
                dict.iter()
                    .map(|(key, value)| {
                        let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str())
                        else {
                            bail!(
                                "got dict<{}, {}> for 'substitutions', want dict<string, string>",
                                key.get_type(),
                                value.get_type()
                            );
                        };
                        Ok((key.to_owned(), value.to_owned()))
                    })
                    .collect::<anyhow::Result<_>>()?
            }
            None => Vec::new(),
        };
        if let Some(value) = computed_substitutions {
            for (key, value) in crate::template_dict::substitutions(value)? {
                // ImmutableMap.Builder.buildOrThrow duplicate-key failure.
                if let Some((_, previous)) = entries.iter().find(|(other, _)| *other == key) {
                    bail!(
                        "Multiple entries with same key: {key}=Substitution({key} -> {value}) and {key}=Substitution({key} -> {previous})"
                    );
                }
                entries.push((key, value));
            }
        }
        context.register_action(Registration {
            outputs: vec![output],
            inputs: vec![template.clone()],
            command: String::new(),
            write: None,
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            extra: ActionExtra {
                transform: Some(Transform::Template {
                    template: template.path.into(),
                    substitutions: entries
                        .iter()
                        .map(|(key, value)| {
                            (
                                crate::bazel_bytes::internal_bytes(key).into_owned(),
                                crate::bazel_bytes::internal_bytes(value).into_owned(),
                            )
                        })
                        .collect(),
                    executable: is_executable,
                }),
                ..ActionExtra::default()
            },
            deferred: None,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            shell_pad: false,
            executable_file: false,
            runfiles_trees: Vec::new(),
            build_info: None,
            shareable: true,
        })?;
        Ok(NoneType)
    }

    fn template_dict<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = action_receiver(this, eval, true)?;
        Ok(crate::template_dict::alloc_template_dict(
            eval.heap(),
            context.active.clone(),
        ))
    }

    /// StarlarkActionFactory.transformVersionFile (:475-486): builtin-only;
    /// the declared output is registered, executing it (stamping) is explicit.
    fn transform_version_file<'v>(
        this: Value<'v>,
        #[starlark(require = named)] transform_func: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        transform_build_info(this, transform_func, template, output_file_name, true, eval)
    }

    /// StarlarkActionFactory.transformInfoFile (:488-499).
    fn transform_info_file<'v>(
        this: Value<'v>,
        #[starlark(require = named)] transform_func: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        transform_build_info(
            this,
            transform_func,
            template,
            output_file_name,
            false,
            eval,
        )
    }
}

/// StarlarkActionFactory.transformBuildInfoFile (:501-534): the declared
/// output is the build-info file of the rule's package; the action takes
/// the workspace status artifact (volatile for transform_version_file,
/// stable for transform_info_file) and the template as inputs. The output
/// of the volatile transform is a constant-metadata artifact
/// (getConstantMetadataArtifact) whose changing bytes do not rebuild
/// consumers; the native marks it in planning (N63).
fn transform_build_info<'v>(
    this: Value<'v>,
    transform_func: Value<'v>,
    template: Value<'v>,
    output_file_name: &str,
    volatile: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    crate::subrule_fragments::private_api(eval).map_err(|e| anyhow!("{e}"))?;
    let context = action_receiver(this, eval, false)?;
    ensure!(
        transform_func.function_name().is_some(),
        "transform_func must be a function"
    );
    // The callback outlives analysis: BuildInfoFileWriteAction keeps the
    // StarlarkFunction and calls it at execution; the native keeps the
    // frozen top-level function and planning evaluates it against the
    // build's workspace status.
    let function = owned_top_level_function(eval, transform_func).ok_or_else(|| {
        anyhow!("transform_func must be a top-level function of a loaded .bzl file")
    })?;
    // BuildInfoFileWriteAction.computeKey (:189-197) keys the callback's
    // bzl transitive digest; the native's stand-in is the callback's
    // identity: the frozen top-level function's name (the value carries
    // no source path in this runtime; identical callbacks share the
    // bazel_tools .bzl, so name + volatility + template identity
    // discriminate every registration this profile can produce).
    let callback_digest = transform_func
        .function_name()
        .unwrap_or_default()
        .to_owned();
    let template = unpack_file(template)?;
    let output = context.declare(output_file_name)?;
    crate::observed_fs::taint(crate::observed_fs::Taint::BuildInfo, || {
        format!("{} ({})", context.label, output.path)
    });
    let status = crate::analysis_runtime::workspace_status_file(if volatile {
        "volatile-status.txt"
    } else {
        "stable-status.txt"
    });
    context.register_action(Registration {
        outputs: vec![output.clone()],
        inputs: vec![status, template],
        command: String::new(),
        write: None,
        program: None,
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        extra: ActionExtra {
            // BuildInfoFileWriteAction.computeKey (:189-197): GUID,
            // isVolatile and the callback's bzl digest — never the status
            // content or the substitutions. Planning rewrites this
            // placeholder into Transform::TemplateKeyed with exactly that
            // key (crate::engine::resolve_build_info).
            transform: Some(Transform::BuildInfo { volatile }),
            ..ActionExtra::default()
        },
        deferred: None,
        exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
        shell_pad: false,
        executable_file: false,
        runfiles_trees: Vec::new(),
        build_info: Some(BuildInfoAction {
            function,
            volatile,
            callback_digest,
        }),
        shareable: true,
    })?;
    Ok(alloc_file(eval.heap(), output))
}

#[derive(Debug)]
struct ScopeFrame {
    identity: String,
    allowed: BTreeSet<String>,
    token: u64,
}

#[derive(Debug)]
pub(crate) struct ScopeState {
    frames: Mutex<Vec<ScopeFrame>>,
    next: AtomicU64,
}

/// Arc identity is the analysis owner. Keeping an escaped handle alive also keeps
/// that identity alive, so allocator reuse cannot make a foreign owner match.
#[derive(Clone, Debug)]
pub(crate) struct ScopeHandle {
    owner: Arc<ScopeState>,
    token: u64,
}

pub(crate) struct ScopeGuard {
    handle: ScopeHandle,
}

impl ScopeState {
    pub(crate) fn new(identity: String, allowed: BTreeSet<String>) -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::new(vec![ScopeFrame {
                identity,
                allowed,
                token: 0,
            }]),
            next: AtomicU64::new(1),
        })
    }

    pub(crate) fn root(self: &Arc<Self>) -> ScopeHandle {
        ScopeHandle {
            owner: self.clone(),
            token: 0,
        }
    }

    pub(crate) fn finish(&self) {
        self.frames().clear();
    }

    pub(crate) fn enter(
        self: &Arc<Self>,
        identity: String,
        allowed: BTreeSet<String>,
    ) -> anyhow::Result<ScopeGuard> {
        {
            let frames = self.frames();
            let caller = frames
                .last()
                .ok_or_else(|| anyhow!("subrule is only available during active rule analysis"))?;
            ensure!(
                caller.allowed.contains(&identity),
                "subrule {identity} must be declared in subrules of its immediate caller {}",
                caller.identity
            );
        }
        let token = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |token| {
                token.checked_add(1)
            })
            .map_err(|_| anyhow!("analysis scope token limit exceeded"))?;
        self.frames().push(ScopeFrame {
            identity,
            allowed,
            token,
        });
        Ok(ScopeGuard {
            handle: ScopeHandle {
                owner: self.clone(),
                token,
            },
        })
    }

    fn frames(&self) -> MutexGuard<'_, Vec<ScopeFrame>> {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ScopeGuard {
    pub(crate) fn handle(&self) -> ScopeHandle {
        self.handle.clone()
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let mut frames = self.handle.owner.frames();
        if frames
            .last()
            .is_some_and(|frame| frame.token == self.handle.token)
        {
            frames.pop();
        }
    }
}

impl ScopeHandle {
    pub(crate) fn check_current(&self) -> anyhow::Result<()> {
        self.current(&self.owner)
    }

    pub(crate) fn current(&self, owner: &Arc<ScopeState>) -> anyhow::Result<()> {
        self.check(owner, false)
    }

    pub(crate) fn live(&self, owner: &Arc<ScopeState>) -> anyhow::Result<()> {
        self.check(owner, true)
    }

    fn check(&self, owner: &Arc<ScopeState>, ancestor: bool) -> anyhow::Result<()> {
        ensure!(
            Arc::ptr_eq(&self.owner, owner),
            "analysis scope receiver belongs to a different analysis owner"
        );
        let frames = owner.frames();
        let valid = if ancestor {
            frames.iter().any(|frame| frame.token == self.token)
        } else {
            frames.last().is_some_and(|frame| frame.token == self.token)
        };
        ensure!(
            valid,
            "analysis scope receiver is locked or no longer active"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Module;
    use starlark::syntax::{AstModule, Dialect};

    /// PA-1: one tool's runfiles tree is one shared value while held; a
    /// provider of the same runfiles under another identity has its own.
    #[test]
    fn a_tools_runfiles_tree_is_shared_by_identity() {
        let file = |path: &str, is_source: bool| File {
            path: path.into(),
            short_path: path
                .trim_start_matches("bazel-out/k8-fastbuild/bin/")
                .into(),
            owner: "//p:t".into(),
            is_source,
            is_directory: false,
            is_symlink: false,
        };
        let mut builder = crate::runfiles::RunfilesBuilder::default();
        builder.add_artifact(file("p/data.txt", true));
        let runfiles = builder.build().unwrap();
        // A FullFilesToRunProvider, then another one of the same contents.
        let full = || {
            crate::runfiles::FilesToRun::full_for_test(
                file("bazel-out/k8-fastbuild/bin/p/tool", false),
                runfiles.clone(),
            )
        };
        let provider = full();
        let first = tool_runfiles_tree(&provider).unwrap();
        let again = tool_runfiles_tree(&provider.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(first.identity(), provider.identity());
        assert_eq!(
            first.directory(),
            "bazel-out/k8-fastbuild/bin/p/tool.runfiles"
        );
        assert_eq!(
            first.mapping().keys().collect::<Vec<_>>(),
            ["_main/p/data.txt"]
        );
        let separate = tool_runfiles_tree(&full()).unwrap();
        assert!(!Arc::ptr_eq(&first, &separate));
        assert_ne!(separate.identity(), first.identity());
        assert!(separate.same_content(&first));
        // Released by its last holder, a tree is computed again.
        let identity = first.identity();
        drop((first, again));
        let fresh = tool_runfiles_tree(&provider).unwrap();
        assert_eq!(fresh.identity(), identity);
        // No runfiles: no tree.
        assert!(
            tool_runfiles_tree(&crate::runfiles::FilesToRun::single(file("p/x", true))).is_none()
        );
    }

    fn write_context() -> Context {
        Context {
            label: Label::parse_canonical("//:target").unwrap(),
            rule_class: "rule".into(),
            action_env: Default::default(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            declared: RefCell::new(Vec::new()),
            shareable: RefCell::new(BTreeSet::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
            propagated_tags: Default::default(),
        }
    }

    #[test]
    fn shareable_paths_normalize_like_path_fragments_and_reject_misuse() {
        assert_eq!(shareable_relative_path("a/./b//c/").unwrap(), "a/b/c");
        assert_eq!(shareable_relative_path("a/b/../c").unwrap(), "a/c");
        for (path, error) in [
            ("../up", "uplevel references"),
            ("a/../../up", "uplevel references"),
            ("/abs", "relative to the artifact root"),
            ("", "the artifact root itself"),
            ("a/..", "the artifact root itself"),
        ] {
            let message = shareable_relative_path(path).unwrap_err().to_string();
            assert!(message.contains(error), "{path}: {message}");
        }
        // The short path is the one rule of every generated File
        // (crate::label::generated_short_path, unit-tested there).
        assert_eq!(
            crate::label::generated_short_path("external/repo/x.h"),
            "../repo/x.h"
        );
    }

    #[test]
    fn shareable_artifacts_are_root_relative_deduplicated_and_shared() {
        let context = write_context();
        let root = crate::label::GENERATED_EXEC_ROOT;
        Module::with_temp_heap(|module| {
            module.set("a", alloc_actions(module.heap(), context.scope.root()));
            let globals = crate::package::bzl_globals();
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(
                AstModule::parse(
                    "shareable.star",
                    r#"
x = a.declare_shareable_artifact('elsewhere/x.txt')
y = a.declare_shareable_artifact('elsewhere/./sub/../x.txt')
(x == y and x.short_path == 'elsewhere/x.txt') or fail(x)
a.write(x, 'same')
a.write(y, 'same')
e = a.declare_shareable_artifact('external/repo/e.h')
e.short_path == '../repo/e.h' or fail(e.short_path)
f = a.declare_file('f.txt')
g = a.declare_shareable_artifact('f.txt')
f == g or fail(g)
a.write(g, 'f')
"#
                    .into(),
                    &Dialect::Standard,
                )
                .unwrap(),
                &globals,
            )
            .unwrap();
            // A different action on a generated output is a conflict.
            let error = eval
                .eval_module(
                    AstModule::parse(
                        "conflict.star",
                        "a.write(x, 'other')".into(),
                        &Dialect::Standard,
                    )
                    .unwrap(),
                    &globals,
                )
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(
                    "file 'elsewhere/x.txt' is generated by these conflicting actions:\nLabel: //:target"
                ),
                "{error}"
            );
        });
        let declared = context.declared.borrow();
        let paths = declared
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                format!("{root}/elsewhere/x.txt"),
                format!("{root}/external/repo/e.h"),
                format!("{root}/f.txt"),
            ]
        );
        // The package-relative declaration came first: only the fresh
        // shareable declarations skip the package checks.
        assert!(context.is_shareable(&declared[0]));
        assert!(context.is_shareable(&declared[1]));
        assert!(!context.is_shareable(&declared[2]));
        // The identical second write is the first (shared).
        assert_eq!(context.actions.borrow().len(), 2);
    }

    #[test]
    fn identical_actions_with_uncompared_parts_are_explicit_errors() {
        let context = write_context();
        Module::with_temp_heap(|module| {
            module.set("a", alloc_actions(module.heap(), context.scope.root()));
            let globals = crate::package::bzl_globals();
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            let mut run = |name: &str, code: &str| {
                eval.eval_module(
                    AstModule::parse(name, code.to_owned(), &Dialect::Standard).unwrap(),
                    &globals,
                )
                .map_err(|error| error.to_string())
            };
            run(
                "setup.star",
                "d = a.declare_directory('d')\n\
                 a.run_shell(outputs = [d], command = 'mkdir -p $1', arguments = [d.path])\n\
                 o = a.declare_file('o')\n\
                 args = a.args()\n\
                 args.add_all([d])\n\
                 a.write(o, args)\n",
            )
            .unwrap();
            // The same Args content again: its directory expands when the
            // action executes, so the key Bazel compares is not known.
            let error = run("same.star", "a.write(o, args)").unwrap_err();
            assert!(
                error.contains(
                    "file 'o' is generated by two actions of //:target: sharing identical actions with command lines or contents expanding tree artifacts is not implemented in the native profile"
                ),
                "{error}"
            );
            // A difference in a compared part is Bazel's conflict.
            let error = run("other.star", "a.write(o, 'text')").unwrap_err();
            assert!(
                error.contains(
                    "file 'o' is generated by these conflicting actions:\nLabel: //:target"
                ),
                "{error}"
            );
        });
        assert_eq!(context.actions.borrow().len(), 2);
    }

    #[test]
    fn write_accepts_default_args_bytes_and_preserves_string_bytes_and_executable_flag() {
        let context = write_context();
        Module::with_temp_heap(|module| {
            module.set("a", alloc_actions(module.heap(), context.scope.root()));
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(AstModule::parse("write.star", r#"
args = a.args()
args.add_all(['plain', 'space value', "single'quote", 'double"quote', 'back\\slash', '', 'line1\nline2', '雪é🙂'])
a.write(a.declare_file('mixed.out'), args)
a.write(a.declare_file('mixed.exec'), args, is_executable=True)
a.write(a.declare_file('string.out'), 'hi\x00hé\n')
a.write(a.declare_file('empty.out'), a.args())
"#.into(), &Dialect::Standard).unwrap(), &crate::package::bzl_globals()).unwrap();
        });
        let actions = context.actions.borrow();
        assert_eq!(actions.len(), 4);
        let expected = "plain\n'space value'\n'single'\\''quote'\n'double\"quote'\n'back\\slash'\n''\n'line1\nline2'\n'雪é🙂'\n".as_bytes();
        for action in &actions[..2] {
            assert_eq!(action.action.write.as_ref().unwrap().content, expected);
            assert!(action.action.command.is_empty());
            assert!(action.inputs.is_empty());
            assert_eq!(action.outputs[0].owner, "//:target");
        }
        assert!(!actions[0].action.write.as_ref().unwrap().executable);
        assert!(actions[1].action.write.as_ref().unwrap().executable);
        assert_eq!(
            actions[2].action.write.as_ref().unwrap().content,
            "hi\0hé\n".as_bytes()
        );
        assert!(actions[3].action.write.as_ref().unwrap().content.is_empty());
    }

    #[test]
    fn write_guards_before_content_validation_and_never_registers_invalid_payloads() {
        let context = write_context();
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set("a", alloc_actions(heap, context.scope.root()));
            module.set(
                "expired",
                crate::args::alloc_args(heap, Arc::new(AtomicBool::new(false))),
            );
            let globals = crate::package::bzl_globals();
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(
                AstModule::parse(
                    "write.star",
                    "out = a.declare_file('out')".into(),
                    &Dialect::Standard,
                )
                .unwrap(),
                &globals,
            )
            .unwrap();
            for payload in [
                "[]",
                "()",
                "set()",
                "{}",
                "1",
                "None",
                "struct(content='coerced')",
                "depset([])",
                "expired",
            ] {
                let source = format!("a.write(out, {payload})");
                let child = context.scope.enter("S".into(), BTreeSet::new()).unwrap();
                let error = eval
                    .eval_module(
                        AstModule::parse("write.star", source.clone(), &Dialect::Standard).unwrap(),
                        &globals,
                    )
                    .unwrap_err();
                assert!(
                    error.to_string().contains("scope receiver"),
                    "{payload}: {error}"
                );
                drop(child);
                let error = eval
                    .eval_module(
                        AstModule::parse("write.star", source, &Dialect::Standard).unwrap(),
                        &globals,
                    )
                    .unwrap_err();
                let expected = if payload == "expired" {
                    "analysis has ended"
                } else {
                    "string or Args"
                };
                assert!(error.to_string().contains(expected), "{payload}: {error}");
                assert!(context.actions.borrow().is_empty());
                assert_eq!(context.declared.borrow().len(), 1);
            }
            let child = context.scope.enter("S".into(), BTreeSet::new()).unwrap();
            let escaped = alloc_actions(heap, child.handle());
            module.set("escaped", escaped);
            module.set(
                "escaped_write",
                escaped.get_attr_error("write", heap).unwrap(),
            );
            drop(child);
            for later in [false, true] {
                let guard =
                    later.then(|| context.scope.enter("S".into(), BTreeSet::new()).unwrap());
                for source in ["escaped.write(None, [])", "escaped_write(None, expired)"] {
                    let error = eval
                        .eval_module(
                            AstModule::parse("write.star", source.into(), &Dialect::Standard)
                                .unwrap(),
                            &globals,
                        )
                        .unwrap_err();
                    assert!(
                        error.to_string().contains("scope receiver"),
                        "{source}: {error}"
                    );
                }
                drop(guard);
            }
            assert!(context.actions.borrow().is_empty());
            assert_eq!(context.declared.borrow().len(), 1);
        });
    }

    #[test]
    fn actual_subrule_returned_args_are_consumed_by_parent_write() {
        let root = tempfile::Builder::new()
            .prefix("write-returned-args-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        // Preserve the genuine returned-Args fixture's write, not a run_shell
        // substitute. This asserts native registration; CLI owner tests execution.
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
def _sub(ctx, parent=None, mode="", old=None, receiver=None, out=None):
    f=ctx.actions.declare_file(ctx.label.name+".seed")
    ctx.actions.write(f,"seed-data\n")
    print("SUBRULE_ENTER",str(ctx.label))
    args=ctx.actions.args()
    args.add("from-subrule")
    return struct(file=f,args=args)
S=subrule(implementation=_sub,attrs={},toolchains=[],fragments=[],subrules=[])
def _rule(ctx):
    result=S()
    result.args.add("from-parent")
    out=ctx.actions.declare_file(ctx.label.name+".out")
    ctx.actions.write(out,result.args)
    return [DefaultInfo(files=depset([result.file,out]))]
r=rule(implementation=_rule,subrules=[S])
"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("BUILD.bazel"),
            "load(':defs.bzl', 'r')\nr(name='args_escape')\n",
        )
        .unwrap();
        let package = crate::package::evaluate(root.path(), "").unwrap();
        let rule = &package[&Label::parse_canonical("//:args_escape").unwrap()];
        let result = crate::analysis::analyze(rule, &BTreeMap::new()).unwrap();
        assert_eq!(result.actions.len(), 2);
        assert_eq!(result.files.len(), 2);
        assert_eq!(
            result.actions[0].action.write.as_ref().unwrap().content,
            b"seed-data\n"
        );
        assert_eq!(
            result.actions[1].action.write.as_ref().unwrap().content,
            b"from-subrule\nfrom-parent\n"
        );
        assert!(
            result
                .files
                .iter()
                .all(|file| file.owner == "//:args_escape")
        );
    }

    #[test]
    fn action_receivers_check_type_owner_and_scope_before_mutation_or_expansion() {
        use starlark::environment::Module;
        let make_context = || Context {
            rule_class: "rule".into(),
            label: Label::parse_canonical("//:target").unwrap(),
            action_env: Default::default(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            declared: RefCell::new(Vec::new()),
            shareable: RefCell::new(BTreeSet::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
            propagated_tags: Default::default(),
        };
        let owner = make_context();
        let foreign = make_context();
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let actions = alloc_actions(heap, owner.scope.root());
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&foreign);
            assert!(
                action_receiver(actions, &eval, false)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("different analysis owner")
            );
            assert!(action_receiver(actions, &eval, true).is_err());
            eval.extra = Some(&owner);
            assert!(action_receiver(Value::new_none(), &eval, false).is_err());
            let methods = ["declare_file", "write", "run_shell", "run"];
            let bound = methods
                .iter()
                .map(|name| actions.get_attr_error(name, heap).unwrap())
                .collect::<Vec<_>>();
            let factory = actions.get_attr_error("args", heap).unwrap();
            let child = owner.scope.enter("S".into(), BTreeSet::new()).unwrap();
            let child_actions = alloc_actions(heap, child.handle());
            let escaped_factory = child_actions.get_attr_error("args", heap).unwrap();
            // These values pass binding but fail deeper validation. Scope must
            // win over File/Args expansion errors and leave all state untouched.
            for (i, function) in bound.into_iter().enumerate() {
                let invalid = heap.alloc(vec![42]);
                let error = match i {
                    0 => eval.eval_function(function, &[heap.alloc("bad")], &[]),
                    1 => eval.eval_function(function, &[Value::new_none(), heap.alloc("bad")], &[]),
                    2 => eval.eval_function(
                        function,
                        &[],
                        &[
                            ("outputs", invalid),
                            ("command", heap.alloc("bad")),
                            ("arguments", invalid),
                        ],
                    ),
                    _ => eval.eval_function(
                        function,
                        &[],
                        &[
                            ("outputs", invalid),
                            ("executable", Value::new_none()),
                            ("arguments", invalid),
                        ],
                    ),
                }
                .unwrap_err();
                assert!(
                    error.to_string().contains("scope receiver"),
                    "{}: {error}",
                    methods[i]
                );
            }
            let args = eval.eval_function(factory, &[], &[]).unwrap();
            drop(child);
            assert!(
                eval.eval_function(escaped_factory, &[], &[])
                    .unwrap_err()
                    .to_string()
                    .contains("scope receiver")
            );
            let add = args.get_attr_error("add", heap).unwrap();
            eval.eval_function(add, &[heap.alloc("still live")], &[])
                .unwrap();
            assert!(owner.declared.borrow().is_empty());
            assert!(owner.actions.borrow().is_empty());
            owner.scope.finish();
            owner.active.store(false, Ordering::Release);
            assert!(eval.eval_function(factory, &[], &[]).is_err());
            assert!(
                eval.eval_function(add, &[heap.alloc("expired")], &[])
                    .is_err()
            );
        });
    }

    #[test]
    fn current_live_owner_and_non_reused_tokens() {
        let owner = ScopeState::new("rule".into(), BTreeSet::from(["S".into()]));
        let parent = owner.root();
        let other = ScopeState::new("other".into(), BTreeSet::new());
        assert!(parent.current(&other).is_err());
        assert!(parent.live(&other).is_err());
        let first = owner.enter("S".into(), BTreeSet::new()).unwrap();
        let escaped = first.handle();
        assert!(parent.current(&owner).is_err());
        parent.live(&owner).unwrap();
        escaped.current(&owner).unwrap();
        assert!(owner.enter("S".into(), BTreeSet::new()).is_err());
        drop(first);
        parent.current(&owner).unwrap();
        assert!(escaped.live(&owner).is_err());
        let second = owner.enter("S".into(), BTreeSet::new()).unwrap();
        assert_ne!(escaped.token, second.handle().token);
        assert!(escaped.current(&owner).is_err());
        drop(second);
        owner.next.store(u64::MAX, Ordering::Relaxed);
        assert!(owner.enter("S".into(), BTreeSet::new()).is_err());
        parent.current(&owner).unwrap();
        owner.finish();
        assert!(parent.live(&owner).is_err());
    }
}
