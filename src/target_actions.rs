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
#[derive(Debug, Allocative)]
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

#[derive(Debug, Allocative)]
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
                "FileWrite".to_owned(),
                None,
                Content::Text(native_string(&write.content)),
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
        } else if action.extra.transform.is_some() {
            (action.extra.mnemonic.clone(), None, Content::None, None)
        } else {
            (
                action.extra.mnemonic.clone(),
                Some(starlark_argv(action)),
                Content::None,
                None,
            )
        };
        ActionView {
            mnemonic,
            argv,
            content,
            substitutions,
            inputs: registered.inputs.clone(),
            outputs: registered.outputs.clone(),
            env: action
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

/// SpawnAction.getStarlarkArgv (:229-235): `getArguments()`, the whole
/// command line, parameter files included: an argument naming one of the
/// action's parameter files (`@path`, `--flagfile=path`) is replaced by the
/// arguments the file holds (ParameterFile.writeParameterFile: one per
/// line, shell-quoted in the `shell` format). A `flag_per_line` file's
/// `--flag=value` lines stay single arguments.
fn starlark_argv(action: &crate::action::Action) -> Vec<String> {
    let argv = crate::action::arguments(action);
    if action.extra.param_files.is_empty() {
        return argv;
    }
    let mut expanded = Vec::with_capacity(argv.len());
    for argument in argv {
        let file = action.extra.param_files.iter().find(|(path, _)| {
            let path = path.to_string_lossy();
            argument.len() > path.len() && argument.ends_with(path.as_ref())
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
pub(crate) use owned::TargetActions;

starlark_simple_value!(TargetActions);
starlark_simple_value!(ActionValue);

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
