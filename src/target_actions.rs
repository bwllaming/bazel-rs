//! `Target.actions` (AbstractConfiguredTarget.getValue `actions`,
//! RuleConfiguredTarget.rawGetStarlarkProvider,
//! analysis/configuredtargets/RuleConfiguredTarget.java:270-281): the
//! actions a rule target registered, as Starlark `Action` values
//! (starlarkbuildapi/ActionApi.java). bazel_skylib's `analysistest`
//! retrieves them with an aspect (`_action_retrieving_aspect`), rules_testing
//! through its `target_actions`. The analysis records a compact view of
//! each registered action (crate::analysis) under the reserved provider key
//! [`PROVIDER_ID`], which no exported provider can name; consumers that list
//! a target's providers skip it.

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::sync::Arc;

use crate::analysis_runtime::RegisteredAction;
use crate::config_registry::ConfiguredTargetKey;
use crate::repository::RepositorySet;
use crate::starlark_types::{Depset, File, Item, Order};

/// The native rule classes that register actions, which are not retained
/// as `Target.actions` (every other native rule registers none).
pub(crate) const NATIVE_RULES_WITH_ACTIONS: &[&str] =
    &["genrule", "genquery", "starlark_doc_extract"];

/// The reserved provider key of a target's actions.
pub(crate) const PROVIDER_ID: &str = "builtin:Target.actions";

/// The shared marker of a Starlark rule target whose actions are not kept
/// (no analysis test of the request names it): one frozen value for the
/// whole process, so a target pays one map entry, never its actions.
pub(crate) fn not_kept() -> starlark::values::OwnedFrozenValue {
    static NOT_KEPT: std::sync::OnceLock<starlark::values::OwnedFrozenValue> =
        std::sync::OnceLock::new();
    NOT_KEPT
        .get_or_init(|| {
            starlark::environment::Module::with_temp_heap(|module| {
                module.set("value", Value::new_none());
                module
                    .freeze()
                    .expect("freezing None cannot fail")
                    .get("value")
                    .expect("the frozen module has the value")
            })
        })
        .clone()
}

/// What `Action` exposes of one registered action.
#[derive(Clone, Debug, Allocative)]
pub(crate) struct ActionView {
    mnemonic: String,
    /// SpawnAction.getStarlarkArgv: the spawn's argv; `None` for writes and
    /// template expansions.
    argv: Option<Vec<String>>,
    content: Content,
    /// TemplateExpansionAction.getStarlarkSubstitutions.
    substitutions: Option<Vec<(String, String)>>,
    inputs: Vec<File>,
    outputs: Vec<File>,
    /// ActionEnvironment.getFixedEnv.
    env: Vec<(String, String)>,
}

#[derive(Clone, Debug, Allocative)]
enum Content {
    None,
    /// FileWriteAction.getFileContents.
    Text(String),
    /// TemplateExpansionAction.getFileContents: the template read when the
    /// content is asked for (an observed read), the substitutions applied
    /// in order.
    Template {
        template: File,
        substitutions: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// The internal (Latin-1) bytes as a native string: UTF-8 where valid, a
/// lone byte as its private-use stand-in (crate::bazel_bytes).
fn native_string(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        for byte in chunk.invalid() {
            text.push(starlark::eval::bazel_byte_char(*byte));
        }
    }
    text
}

impl ActionView {
    fn of(registered: &RegisteredAction) -> Self {
        let action = &registered.action;
        let template = match &action.extra.transform {
            Some(crate::action::Transform::Template { substitutions, .. }) => Some(substitutions),
            _ => None,
        };
        let (mnemonic, argv, content, substitutions) = if let Some(write) = &action.write {
            (
                if action.extra.mnemonic.is_empty() {
                    "FileWrite".to_owned()
                } else {
                    action.extra.mnemonic.clone()
                },
                None,
                // ParameterFileWriteAction.getStarlarkContent (:186-197):
                // none when it has (tree artifact) inputs, whose contents
                // analysis does not know.
                if registered.inputs.is_empty() {
                    Content::Text(native_string(&write.content))
                } else {
                    Content::None
                },
                None,
            )
        } else if let Some(substitutions) = template {
            (
                "TemplateExpand".to_owned(),
                None,
                match registered.inputs.first() {
                    Some(template) => Content::Template {
                        template: template.clone(),
                        substitutions: substitutions.clone(),
                    },
                    None => Content::None,
                },
                Some(
                    substitutions
                        .iter()
                        .map(|(key, value)| (native_string(key), native_string(value)))
                        .collect(),
                ),
            )
        } else if let Some(transform) = &action.extra.transform {
            (
                transform_mnemonic(action, transform),
                None,
                Content::None,
                None,
            )
        } else {
            (
                action.extra.mnemonic.clone(),
                Some(starlark_argv(registered)),
                Content::None,
                None,
            )
        };
        ActionView {
            mnemonic,
            argv,
            content,
            substitutions,
            // A Starlark spawn's inputs in Bazel's order (the declared
            // inputs, then the tools; B57), else as registered.
            inputs: match &registered.starlark_inputs {
                Some(inputs) => inputs.to_vec(),
                None => registered.inputs.clone(),
            },
            outputs: registered.outputs.clone(),
            env: action
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

impl ActionView {
    /// A natively planned action's view (no content or substitutions).
    pub(crate) fn native(
        mnemonic: &str,
        argv: Option<Vec<String>>,
        inputs: Vec<File>,
        outputs: Vec<File>,
        env: Vec<(String, String)>,
    ) -> Self {
        let mut seen = std::collections::HashSet::new();
        let inputs = inputs
            .into_iter()
            .filter(|file| seen.insert(file.path.clone()))
            .collect();
        ActionView {
            mnemonic: mnemonic.to_owned(),
            argv,
            content: Content::None,
            substitutions: None,
            inputs,
            outputs,
            env,
        }
    }
}

/// The mnemonic of a native transform a rule registered (aquery's,
/// crate::engine::aquery): SymlinkAction.getMnemonic (`Symlink`,
/// `ExecutableSymlink`; cc_common's SolibSymlinkAction `SolibSymlink`),
/// UnresolvedSymlinkAction's `UnresolvedSymlink`, else the action's own.
fn transform_mnemonic(
    action: &crate::action::Action,
    transform: &crate::action::Transform,
) -> String {
    if !action.extra.mnemonic.is_empty() {
        return action.extra.mnemonic.clone();
    }
    match transform {
        crate::action::Transform::Symlink {
            require_executable,
            progress,
            ..
        } => {
            if progress.starts_with("SolibSymlink ") {
                "SolibSymlink"
            } else if *require_executable {
                "ExecutableSymlink"
            } else {
                "Symlink"
            }
        }
        crate::action::Transform::UnresolvedSymlink { .. } => "UnresolvedSymlink",
        _ => crate::profile::mnemonic(action),
    }
    .to_owned()
}

/// The actions RunfilesSupport registers for an executable after the rule's
/// own (RuleConfiguredTargetBuilder.build; analysis/RunfilesSupport.java:
/// 280-347, 467-545): RepoMappingManifestAction, with runfiles manifests
/// SourceManifestAction (the input manifest) and with runfile links
/// SymlinkTreeAction (the MANIFEST from the input manifest, the
/// configuration's action environment; its other inputs only on Windows),
/// then RunfilesTreeAction (the runfiles tree artifact over the runfiles'
/// artifacts, the manifest and the repository mapping manifest).
pub(crate) fn runfiles_support_views(
    files_to_run: &crate::runfiles::FilesToRun,
    options: &crate::runfiles_tree::RunfilesOptions,
    action_env: &crate::action_env::ActionEnvironment,
) -> Vec<ActionView> {
    let (Some(executable), Some(runfiles), Some(repo_mapping), Some((_, tree))) = (
        files_to_run.executable(),
        files_to_run.runfiles(),
        files_to_run.repo_mapping_manifest(),
        files_to_run.runfiles_tree(),
    ) else {
        return Vec::new();
    };
    let mut views = vec![ActionView::native(
        "RepoMappingManifest",
        None,
        Vec::new(),
        vec![repo_mapping.clone()],
        Vec::new(),
    )];
    if options.build_runfile_manifests {
        let input_manifest = File::new(crate::starlark_types::FileData {
            path: format!("{}.runfiles_manifest", executable.path),
            short_path: format!("{}.runfiles_manifest", executable.short_path),
            owner: repo_mapping.owner.clone(),
            is_source: false,
            is_directory: false,
            is_symlink: false,
        });
        views.push(ActionView::native(
            "SourceSymlinkManifest",
            None,
            Vec::new(),
            vec![input_manifest.clone()],
            Vec::new(),
        ));
        if options.links()
            && let Some(manifest) = files_to_run.runfiles_manifest()
        {
            views.push(ActionView::native(
                "SymlinkTree",
                None,
                vec![input_manifest],
                vec![manifest.clone()],
                action_env
                    .fixed
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ));
        }
    }
    let mut contents = runfiles.all_artifacts();
    contents.extend(files_to_run.runfiles_manifest().cloned());
    contents.push(repo_mapping.clone());
    views.push(ActionView::native(
        "RunfilesTree",
        None,
        contents,
        vec![tree.clone()],
        Vec::new(),
    ));
    views
}

/// SpawnAction.getStarlarkArgv (:229-235): `getArguments()`, the whole
/// command line, parameter files included: an argument naming one of the
/// action's parameter files (`@path`, `--flagfile=path`) is replaced by the
/// arguments the file holds (ParameterFile.writeParameterFile: one per
/// line, shell-quoted in the `shell` format). A `flag_per_line` file's
/// `--flag=value` lines stay single arguments.
///
/// A Java compilation (JavaCompileAction.getArguments, :697-700) is its
/// whole command line, the flags and the full classpath line, without the parameter
/// file JavaBuilder reads them from at execution. A command line deferred
/// to execution (Args over tree artifacts) is expanded as genuine's
/// analysis does without input metadata: directories stay their paths, or
/// what map_each maps them to with a NoopExpander
/// (StarlarkCustomCommandLine.java:577-595, crate::command_line::expand_groups).
fn starlark_argv(registered: &RegisteredAction) -> Vec<String> {
    let action = &registered.action;
    if analysis_mapped(registered) {
        return mapped_starlark_argv(registered);
    }
    let mut argv = crate::action::arguments(action);
    if let Some(java) = &action.extra.java_compile {
        argv.extend(java.flags.iter().cloned());
        if !java.classpath.is_empty() {
            argv.push("--classpath".to_owned());
            argv.extend(
                java.classpath
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
        // `--experimental_java_classpath=javabuilder`'s flags
        // (JavaCompileAction.getFullClasspathLine).
        argv.extend(
            java.mode.full_classpath_flags(
                &java
                    .dependency_jdeps
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            ),
        );
        return argv;
    }
    let mut param_files = std::borrow::Cow::Borrowed(&action.extra.param_files);
    if let Some(deferred) = &registered.deferred
        && let crate::scheduler::DeferredSpec::CommandLine { spec, .. } = &*deferred.0
    {
        // SpawnAction.getArguments without input metadata; an expansion
        // error (a map_each failing on a directory) leaves the executable.
        if let Ok((arguments, files)) = spec.expand(None) {
            argv.extend(arguments);
            param_files = std::borrow::Cow::Owned(files);
        }
    }
    inline_param_files(argv, &param_files)
}

/// Whether `Target.actions` shows the spawn's path-mapped argv
/// (SpawnAction.getArguments: PathMappers.create without input metadata,
/// crate::path_mapping): a spawn of a `strip` configuration whose effective
/// output paths mode is STRIP (its requirements and rule tags with this
/// command's `--modify_execution_info`) and whose inputs and outputs do
/// not collide by path.
fn analysis_mapped(registered: &RegisteredAction) -> bool {
    let action = &registered.action;
    if action.extra.path_mapping.is_none() || action.extra.java_compile.is_some() {
        return false;
    }
    let (requirements, tags) = crate::execution_options::modified(
        &action.extra.mnemonic,
        &action.extra.execution_info,
        &action.execution_info,
        action.extra.execution_info_modifiers.as_deref(),
    );
    let effective = crate::path_mapping::effective(&action.extra.mnemonic, |key| {
        requirements.contains_key(key) || tags.contains(key)
    });
    let inputs: &[File] = match &registered.starlark_inputs {
        Some(inputs) => inputs,
        None => &registered.inputs,
    };
    effective
        && crate::path_mapping::strippable_by_paths(
            inputs
                .iter()
                .chain(&registered.outputs)
                .map(|file| file.path.as_str()),
        )
}

/// [`starlark_argv`] of a mapped spawn: CommandLines.allArguments with the
/// StrippingPathMapper (the mapped executable, arguments and parameter
/// file contents).
fn mapped_starlark_argv(registered: &RegisteredAction) -> Vec<String> {
    let action = &registered.action;
    let mapping = action
        .extra
        .path_mapping
        .as_deref()
        .expect("a mapped spawn has its mapping");
    let mut argv = crate::action::mapped_arguments(action);
    let mut param_files = std::borrow::Cow::Borrowed(&mapping.param_files);
    if let Some(deferred) = &registered.deferred
        && let crate::scheduler::DeferredSpec::CommandLine {
            spec,
            mapped_executable: Some(executable),
            ..
        } = &*deferred.0
    {
        // At analysis a deferred command line's arguments are empty.
        if let Ok((arguments, files)) = spec.expand_mapped(None, executable) {
            argv.extend(arguments);
            param_files = std::borrow::Cow::Owned(files);
        }
    }
    inline_param_files(argv, &param_files)
}

/// The arguments with each parameter file argument replaced by the
/// file's lines.
fn inline_param_files(
    argv: Vec<String>,
    param_files: &[(std::path::PathBuf, Vec<u8>)],
) -> Vec<String> {
    if param_files.is_empty() {
        return argv;
    }
    let mut expanded = Vec::with_capacity(argv.len());
    for argument in argv {
        // `--flagfile=path`, `@path`, or the bare path (`use_param_file("%s")`).
        let file = param_files.iter().find(|(path, _)| {
            let path = path.to_string_lossy();
            argument.ends_with(path.as_ref())
        });
        match file {
            Some((_, content)) => {
                let text = String::from_utf8_lossy(content);
                expanded.extend(text.lines().map(shell_unquote));
            }
            None => expanded.push(argument),
        }
    }
    expanded
}

/// The inverse of the `shell` parameter file quoting (ShellEscaper): a
/// `'…'` line with `'\''` for each quote; any other line as written.
fn shell_unquote(line: &str) -> String {
    match line
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        Some(inner) if line.len() >= 2 => inner.replace("'\\''", "'"),
        _ => line.to_owned(),
    }
}

// Only derive-generated markers require this exception: owned data only.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// The actions of one configured target.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct TargetActions {
        pub actions: Arc<[ActionView]>,
        #[allocative(skip)]
        pub repositories: Option<Arc<RepositorySet>>,
    }

    /// The `Target.actions` of a Starlark rule target whose views the
    /// analysis did not keep (B61): its key, and the views once a reader
    /// asked for them ([`Lazy::views`]: the engine analyzes the target again
    /// with its views kept and fills them here, then runs the reader again).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct Lazy {
        #[allocative(skip)]
        pub key: ConfiguredTargetKey,
        #[allocative(skip)]
        pub views: std::sync::OnceLock<TargetActions>,
    }

    /// One `Action` (ActionApi).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ActionValue {
        pub actions: Arc<[ActionView]>,
        pub index: usize,
        /// The template expansion's content, read when the value was made.
        pub content: Option<String>,
    }
}
use owned::ActionValue;
pub(crate) use owned::{Lazy, TargetActions};

starlark_simple_value!(TargetActions);
starlark_simple_value!(ActionValue);
starlark_simple_value!(Lazy);

impl fmt::Display for Lazy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<actions>")
    }
}

#[starlark_value(type = "TargetActions")]
impl<'v> StarlarkValue<'v> for Lazy {}

impl Lazy {
    /// The marker of the target `key` (allocated on its analysis heap).
    pub(crate) fn alloc<'v>(heap: Heap<'v>, key: ConfiguredTargetKey) -> Value<'v> {
        heap.alloc(Lazy {
            key,
            views: std::sync::OnceLock::new(),
        })
    }

    /// The views, once filled.
    pub(crate) fn views(&self) -> Option<&TargetActions> {
        self.views.get()
    }

    /// Fill the views (the target analyzed again with its views kept). A
    /// second fill keeps the first: both are the same analysis's.
    pub(crate) fn fill(&self, views: TargetActions) {
        let _ = self.views.set(views);
    }
}

/// The sentinel of an analysis that read the `Target.actions` of a target
/// whose views were not kept and not filled yet ([`Lazy`]): the engine
/// fills them ([`take_wanted`]) and analyzes it again. Never shown: a
/// caller that cannot fill them reports [`unwanted`]'s text.
pub(crate) const NEEDS_VIEWS: &str = "\u{0}analysis reads Target.actions not kept yet: ";

/// A [`Lazy`] an analysis read before it was filled: the target's key and
/// the marker it read.
#[derive(Clone)]
pub(crate) struct Wanted {
    pub key: ConfiguredTargetKey,
    pub marker: starlark::values::OwnedFrozenValue,
}

thread_local! {
    /// The targets whose views the running analysis asked for.
    static WANTED: std::cell::RefCell<Vec<Wanted>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Those of the last analysis that asked ([`take_wanted`]).
    static LAST_WANTED: std::cell::RefCell<Vec<Wanted>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `target.actions` (or `hasattr(target, "actions")`) on the [`Lazy`]
/// `marker` of `key`, not filled: the running analysis will answer
/// [`NEEDS_VIEWS`].
pub(crate) fn note_wanted(key: &ConfiguredTargetKey, marker: &starlark::values::OwnedFrozenValue) {
    WANTED.with(|wanted| {
        let mut wanted = wanted.borrow_mut();
        if !wanted.iter().any(|known| known.key == *key) {
            wanted.push(Wanted {
                key: key.clone(),
                marker: marker.clone(),
            });
        }
    });
}

/// The scope of one analysis (a rule implementation, an aspect
/// application): what it asks for, apart from an enclosing one's.
pub(crate) struct WantScope(Vec<Wanted>);

impl WantScope {
    pub(crate) fn enter() -> Self {
        LAST_WANTED.with(|last| last.borrow_mut().clear());
        WantScope(WANTED.with(|wanted| std::mem::take(&mut *wanted.borrow_mut())))
    }

    /// The analysis ends with `result`: the [`NEEDS_VIEWS`] sentinel when it
    /// asked for views (kept for [`take_wanted`]), else `result`.
    pub(crate) fn finish<T>(self, result: Result<T, String>) -> Result<T, String> {
        let asked = WANTED.with(|wanted| std::mem::replace(&mut *wanted.borrow_mut(), self.0));
        if asked.is_empty() {
            return result;
        }
        let message = match &result {
            Ok(_) => String::new(),
            Err(error) => error.clone(),
        };
        let labels = asked
            .iter()
            .map(|wanted| crate::rule_declarations::canonical_form(&wanted.key.label))
            .collect::<Vec<_>>()
            .join(", ");
        LAST_WANTED.with(|last| *last.borrow_mut() = asked);
        Err(format!("{NEEDS_VIEWS}{labels}\u{0}{message}"))
    }
}

/// The targets whose views the last analysis on this thread that answered
/// [`NEEDS_VIEWS`] asked for.
pub(crate) fn take_wanted() -> Vec<Wanted> {
    LAST_WANTED.with(|last| std::mem::take(&mut *last.borrow_mut()))
}

/// Whether `error` is the [`NEEDS_VIEWS`] sentinel.
pub(crate) fn needs_views(error: &str) -> bool {
    error.contains(NEEDS_VIEWS)
}

/// The explicit error of a reader whose [`NEEDS_VIEWS`] could not be
/// answered (a target asked for could not be analyzed again for its
/// views): never the reader's own "no field" error, which would look like
/// its bug (genuine answers any rule target's actions).
pub(crate) fn unwanted(error: String) -> String {
    let Some(at) = error.find(NEEDS_VIEWS) else {
        return error;
    };
    let rest = &error[at + NEEDS_VIEWS.len()..];
    let labels = rest.split_once('\u{0}').map_or(rest, |(labels, _)| labels);
    format!(
        "{}reading the Target.actions of {labels}: analyzing it again for the actions the analysis did not keep failed, which {}",
        &error[..at],
        crate::language_modules::NOT_IMPLEMENTED
    )
}

impl fmt::Display for TargetActions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} actions>", self.actions.len())
    }
}

#[starlark_value(type = "TargetActions")]
impl<'v> StarlarkValue<'v> for TargetActions {}

impl TargetActions {
    pub(crate) fn new(actions: &[RegisteredAction], repositories: Arc<RepositorySet>) -> Self {
        TargetActions {
            actions: actions.iter().map(ActionView::of).collect(),
            repositories: Some(repositories),
        }
    }

    /// The registered actions followed by natively planned ones
    /// ([`runfiles_support_views`]).
    pub(crate) fn with_native(
        actions: &[RegisteredAction],
        native: Vec<ActionView>,
        repositories: Option<Arc<RepositorySet>>,
    ) -> Self {
        TargetActions {
            actions: actions.iter().map(ActionView::of).chain(native).collect(),
            repositories,
        }
    }

    /// Frozen, for a native rule target's provider map.
    pub(crate) fn frozen(self) -> starlark::values::OwnedFrozenValue {
        starlark::environment::Module::with_temp_heap(|module| {
            let value = module.heap().alloc(self);
            module.set("value", value);
            module
                .freeze()
                .expect("freezing owned target actions cannot fail")
                .get("value")
                .expect("the frozen module has the value")
        })
    }

    /// The actions of a merged configured target: each value's actions in
    /// order (the aspects', then the base target's). `None` when a value is
    /// not kept actions (the base target's "not kept" marker).
    pub(crate) fn merged<'a>(
        values: impl Iterator<Item = &'a starlark::values::OwnedFrozenValue>,
    ) -> Option<starlark::values::OwnedFrozenValue> {
        use starlark::values::ValueLike as _;
        let mut actions = Vec::new();
        let mut repositories = None;
        for value in values {
            let kept = value.value().downcast_ref::<TargetActions>()?;
            actions.extend(kept.actions.iter().cloned());
            if repositories.is_none() {
                repositories = kept.repositories.clone();
            }
        }
        Some(starlark::environment::Module::with_temp_heap(|module| {
            let value = module.heap().alloc(TargetActions {
                actions: actions.into(),
                repositories,
            });
            module.set("value", value);
            module
                .freeze()
                .expect("freezing owned target actions cannot fail")
                .get("value")
                .expect("the frozen module has the value")
        }))
    }

    /// No actions (a failed target of a configuration allowing analysis
    /// failures), frozen.
    pub(crate) fn empty_frozen() -> starlark::values::OwnedFrozenValue {
        starlark::environment::Module::with_temp_heap(|module| {
            let value = module.heap().alloc(TargetActions {
                actions: Arc::from([]),
                repositories: None,
            });
            module.set("value", value);
            module
                .freeze()
                .expect("freezing owned target actions cannot fail")
                .get("value")
                .expect("the frozen module has the value")
        })
    }

    /// The Starlark list of `Action` values.
    pub(crate) fn alloc_list<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let values = (0..self.actions.len())
            .map(|index| {
                let content = match &self.actions[index].content {
                    Content::None => None,
                    Content::Text(text) => Some(text.clone()),
                    Content::Template {
                        template,
                        substitutions,
                    } => self.repositories.as_ref().and_then(|repositories| {
                        template_content(template, substitutions, repositories)
                    }),
                };
                heap.alloc(ActionValue {
                    actions: self.actions.clone(),
                    index,
                    content,
                })
            })
            .collect::<Vec<_>>();
        heap.alloc(values)
    }
}

/// LocalTemplateExpansionStrategy.getExpandedTemplateUnsafe of a source
/// template (a generated one has no content at analysis time).
fn template_content(
    template: &File,
    substitutions: &[(Vec<u8>, Vec<u8>)],
    repositories: &RepositorySet,
) -> Option<String> {
    if !template.is_source {
        return None;
    }
    let label = crate::label::Label::parse_canonical(&template.owner).ok()?;
    let root = repositories.root(&label.repository).ok()?;
    let mut bytes = crate::observed_fs::read(root.join(&label.package).join(&label.name)).ok()?;
    for (key, value) in substitutions {
        if key.is_empty() {
            continue;
        }
        let mut out = Vec::with_capacity(bytes.len());
        let mut rest = &bytes[..];
        while let Some(position) = rest.windows(key.len()).position(|window| window == key) {
            out.extend_from_slice(&rest[..position]);
            out.extend_from_slice(value);
            rest = &rest[position + key.len()..];
        }
        out.extend_from_slice(rest);
        bytes = out;
    }
    Some(native_string(&bytes))
}

fn files_depset<'v>(heap: Heap<'v>, files: &[File]) -> Value<'v> {
    let items = files.iter().cloned().map(Item::File).collect();
    match Depset::compose_framework(items, Vec::new(), Order::Default, None) {
        Ok(depset) => heap.alloc(depset),
        Err(_) => Value::new_none(),
    }
}

impl fmt::Display for ActionValue {
    // AbstractAction.repr: prettyPrint, `action 'PROGRESS'`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let view = &self.actions[self.index];
        write!(f, "action '{}'", view.mnemonic)
    }
}

const ACTION_FIELDS: &[&str] = &[
    "args",
    "argv",
    "content",
    "env",
    "inputs",
    "mnemonic",
    "outputs",
    "substitutions",
];

#[starlark_value(type = "Action")]
impl<'v> StarlarkValue<'v> for ActionValue {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let view = &self.actions[self.index];
        Some(match name {
            "mnemonic" => heap.alloc(view.mnemonic.as_str()),
            "argv" => match &view.argv {
                Some(argv) => heap.alloc(argv.clone()),
                None => Value::new_none(),
            },
            // Args objects of the action are not retained natively.
            "args" => Value::new_none(),
            "content" => match &self.content {
                Some(content) => heap.alloc(content.as_str()),
                None => Value::new_none(),
            },
            "substitutions" => match &view.substitutions {
                Some(entries) => heap.alloc(AllocDict(
                    entries
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                )),
                None => Value::new_none(),
            },
            "inputs" => files_depset(heap, &view.inputs),
            "outputs" => files_depset(heap, &view.outputs),
            "env" => heap.alloc(AllocDict(
                view.env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        ACTION_FIELDS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }
}
