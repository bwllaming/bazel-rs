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
    /// Runfiles trees of the spawn's tools (N23f).
    pub runfiles_trees: Vec<ToolRunfilesTree>,
    /// An actions.transform_{version,info}_file registration (N63): the
    /// Starlark translation callback (StarlarkActionFactory
    /// .transformBuildInfoFile :501-534 keeps the function the same way)
    /// and which status file it translates. Planning evaluates the callback
    /// against the build's workspace status and rewrites the action into a
    /// template expansion (BuildInfoFileWriteAction.execute :121-176).
    pub build_info: Option<BuildInfoAction>,
}

/// The translation callback of an `actions.transform_version_file` /
/// `actions.transform_info_file` action (true: the volatile status file).
#[derive(Clone, Debug)]
pub struct BuildInfoAction {
    pub function: starlark::values::OwnedFrozenValue,
    pub volatile: bool,
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
        pub dependency_files: BTreeSet<File>,
        pub declared: RefCell<Vec<File>>,
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
        self.sync_default_executable();
        let output = self.label.with_name(name).map_err(anyhow::Error::msg)?;
        let file = File {
            is_directory: directory,
            path: output
                .generated_exec_path(&self.output_root)
                .to_string_lossy()
                .into_owned(),
            short_path: output.short_path().to_string_lossy().into_owned(),
            owner: self.label.to_string(),
            is_source: false,
        };
        let mut declared = self.declared.borrow_mut();
        // A path declared both as a file and as a directory is reported when
        // the implementation returns (StarlarkProviderValidationUtil), like Bazel.
        if !declared.contains(&file) {
            ensure!(
                !self
                    .dependency_files
                    .iter()
                    .any(|other| other.path == file.path),
                "declared output {} conflicts with a dependency artifact",
                file.path
            );
            declared.push(file.clone());
        }
        Ok(file)
    }

    pub(crate) fn known_file(&self, file: &File) -> bool {
        if is_workspace_status_file(file) {
            return true;
        }
        self.sync_default_executable();
        self.dependency_files.contains(file)
            || self.declared.borrow().contains(file)
            // N23f: the dependencies' runfiles and repository mapping manifests.
            || self.runfiles.manifests.contains(file)
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
        } = registration;
        ensure!(
            !outputs.is_empty(),
            "an action must have at least one output"
        );
        self.sync_default_executable();
        let declared = self.declared.borrow();
        let mut seen = BTreeSet::new();
        let mut actions = self.actions.borrow_mut();
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
            ensure!(
                !actions.iter().any(|action| action.outputs.contains(output)),
                "output {} has more than one generating action",
                output.path
            );
        }
        for input in &inputs {
            // The workspace status files are produced by the build's
            // BazelWorkspaceStatusAction (N63), planned as a system node
            // whose artifacts resolve these inputs.
            ensure!(
                is_workspace_status_file(input) || self.known_file(input),
                "action input {} is not a dependency or declared output",
                input.path
            );
        }
        extra.tree_outputs = outputs
            .iter()
            .filter(|file| file.is_directory)
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
        actions.push(RegisteredAction {
            action,
            inputs,
            outputs,
            deferred,
            exec_group,
            runfiles_trees,
            build_info,
        });
        Ok(())
    }
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
    /// Runfiles trees of the spawn's tools (N23f).
    pub runfiles_trees: Vec<ToolRunfilesTree>,
    /// An actions.transform_{version,info}_file registration (N63).
    pub build_info: Option<BuildInfoAction>,
}

/// The runfiles tree of an action tool or executable, staged into the spawn
/// below `<executable>.runfiles` (SpawnInputExpander.addSingleRunfilesTreeToInputs,
/// exec/SpawnInputExpander.java:87-135): every mapping entry, never the
/// MANIFEST (:212).
#[derive(Clone, Debug)]
pub struct ToolRunfilesTree {
    /// The exec path of the runfiles directory.
    pub directory: String,
    pub mapping: crate::runfiles::RunfilesMapping,
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
}

/// Register a Starlark spawn: `executable` is Bazel's executable command-line
/// part (for the parameter-file threshold), `program`/`command` the native spawn.
#[allow(clippy::too_many_arguments)]
fn register_spawn(
    context: &Context,
    options: SpawnOptions<'_>,
    mut inputs: Vec<File>,
    mut runfiles_trees: Vec<ToolRunfilesTree>,
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
    let extra = ActionExtra {
        inherited_environment: environment.inherited,
        execution_info: match options.execution_requirements {
            Some(value) if !value.is_none() => string_dict(value, "execution_requirements")?,
            _ => BTreeMap::new(),
        },
        mnemonic: options.mnemonic,
        ..ActionExtra::default()
    };
    let environment = environment.fixed;
    let (arguments, param_files, deferred) = if spec.deferred() {
        let spec = Arc::new(spec);
        let deferred = Deferred(Arc::new(
            move |listings: &crate::scheduler::TreeListings, action: &mut Action| {
                let lookup = |file: &File| tree_listing(listings, file);
                let (arguments, param_files) = spec
                    .expand(Some(&lookup))
                    .map_err(|e| format!("{progress} failed: {e}"))?;
                action.arguments = arguments;
                action.extra.param_files = param_files;
                Ok(())
            },
        ));
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
    })
}

/// The runfiles tree a FilesToRunProvider with runfiles support attaches to
/// a spawn (StarlarkAction.Builder.addTool(FilesToRunProvider) adds the
/// runfiles tree artifact, RuleConfiguredTargetBuilder.java:144-152).
pub(crate) fn tool_runfiles_tree(
    files_to_run: &crate::runfiles::FilesToRun,
) -> Option<ToolRunfilesTree> {
    let runfiles = files_to_run.runfiles.as_ref().filter(|r| !r.is_empty())?;
    let executable = files_to_run.executable.as_ref()?;
    Some(ToolRunfilesTree {
        directory: format!("{}.runfiles", executable.path),
        mapping: runfiles.runfiles_inputs(files_to_run.repo_mapping_manifest.as_ref(), None),
    })
}

fn tree_listing(
    listings: &crate::scheduler::TreeListings,
    file: &File,
) -> Result<Vec<std::path::PathBuf>, String> {
    listings
        .get(std::path::Path::new(&file.path))
        .cloned()
        .ok_or_else(|| {
            format!(
                "Failed to expand directory {file}. Either add the directory as an input of the action or set 'expand_directories = False' in the 'add_all' or 'add_joined' call to have the path of the directory added to the command line instead of its contents."
            )
        })
}

fn declared_name(context: &Context, filename: &str, sibling: Value<'_>) -> anyhow::Result<String> {
    if sibling.is_none() {
        return Ok(filename.to_owned());
    }
    let sibling = unpack_file(sibling)?;
    ensure!(
        context.known_file(&sibling),
        "sibling must be a dependency or declared File"
    );
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
    /// StarlarkActionFactory.declareSymlink (:194-225): unresolved symlink
    /// artifacts are not modelled (explicit).
    fn declare_symlink<'v>(
        this: Value<'v>,
        filename: &str,
        #[starlark(default = NoneType)] sibling: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        action_receiver(this, eval, false)?;
        let _ = sibling;
        bail!(
            "actions.declare_symlink('{filename}'): unresolved symlink artifacts are not supported by the native profile (docs/ACTIONS_PARAMS.md)"
        )
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
            let snapshot = Arc::new(snapshot);
            let deferred = Deferred(Arc::new(
                move |listings: &crate::scheduler::TreeListings, action: &mut Action| {
                    let lookup = |file: &File| tree_listing(listings, file);
                    let arguments = crate::command_line::expand_groups(
                        &snapshot.groups,
                        snapshot.flag_per_line,
                        Some(&lookup),
                    )?;
                    if let Some(write) = action.write.as_mut() {
                        write.content =
                            crate::command_line::param_file_bytes(&arguments, snapshot.file_type);
                    }
                    Ok(())
                },
            ));
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
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        // Scheduling hints with no effect on a local spawn's results.
        let _ = resource_set;
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
                progress_message: progress_message.map(str::to_owned),
                mnemonic: mnemonic
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Action".to_owned()),
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
) -> anyhow::Result<(Vec<File>, Vec<ToolRunfilesTree>)> {
    const WANT: &str = "expected value of type 'File, FilesToRunProvider or Depset of Files' for a member of parameter 'tools' but";
    let mut files = Vec::new();
    let mut trees = Vec::new();
    let attach = |files: &mut Vec<File>, trees: &mut Vec<ToolRunfilesTree>, file: &File| {
        if let Some(files_to_run) = context.runfiles.executables.borrow().get(file) {
            files.extend(files_to_run.files.iter().cloned());
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
            files.extend(files_to_run.files.iter().cloned());
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
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        #[starlark(require = named)] input_manifests: Option<Value<'v>>,
        #[starlark(require = named)] exec_group: Option<Value<'v>>,
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        let _ = resource_set;
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
                extra_inputs.extend(files_to_run.files.iter().cloned());
                trees.extend(tool_runfiles_tree(files_to_run));
            }
            let file = match crate::runfiles::FilesToRun::from_value(executable) {
                Some(files_to_run) => files_to_run.executable.clone().ok_or_else(|| {
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
                progress_message: progress_message.map(str::to_owned),
                mnemonic: mnemonic
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Action".to_owned()),
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
    /// StarlarkActionFactory.symlink (:254-354) with target_file: the output
    /// observes the target's bytes and permission bits. target_path needs an
    /// unresolved symlink output (declare_symlink), which is unsupported.
    fn symlink<'v>(
        this: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named, default = NoneType)] target_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] target_path: Value<'v>,
        #[starlark(require = named, default = NoneType)] target_type: Value<'v>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        if target_file.is_none() == target_path.is_none() {
            bail!("Exactly one of \"target_file\" or \"target_path\" is required");
        }
        let output = unpack_file(output)?;
        if target_file.is_none() {
            // Every declarable native output is a file or directory.
            bail!(
                "symlink() with \"target_path\" param requires that \"output\" be declared as a symlink, not a file or directory (did you mean to use declare_symlink() instead of declare_file() or declare_directory()?)"
            );
        }
        if !target_type.is_none() {
            bail!("\"target_type\" cannot be used with \"target_file\"");
        }
        let target = unpack_file(target_file)?;
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
        // AbstractAction.getProgressMessage (actions/AbstractAction.java:380-400):
        // "Creating symlink %{output}" unless given.
        let progress = match progress_message {
            Some(value) if !value.is_none() => value
                .unpack_str()
                .ok_or_else(|| anyhow!("progress_message must be a string or None"))?
                .to_owned(),
            _ => "Creating symlink %{output}".to_owned(),
        }
        .replace("%{label}", &context.label.to_string())
        .replace("%{output}", crate::runfiles::root_relative(&output))
        .replace("%{input}", crate::runfiles::root_relative(&target));
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
    let template = unpack_file(template)?;
    let output = context.declare(output_file_name)?;
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
            // BuildInfoFileWriteAction.computeKey (:185-191): GUID,
            // isVolatile and the callback's bzl digest — never the status
            // content. The native placeholder key is replaced by the
            // evaluated template substitutions in planning; the volatile
            // output's constant-metadata handling keeps consumers stable.
            transform: Some(Transform::BuildInfo { volatile }),
            ..ActionExtra::default()
        },
        deferred: None,
        exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
        shell_pad: false,
        executable_file: false,
        runfiles_trees: Vec::new(),
        build_info: Some(BuildInfoAction { function, volatile }),
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

    fn write_context() -> Context {
        Context {
            label: Label::parse_canonical("//:target").unwrap(),
            rule_class: "rule".into(),
            action_env: Default::default(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
        }
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
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
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
