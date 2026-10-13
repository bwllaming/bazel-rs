//! Filesystem-free analysis of the supported Starlark custom-rule subset.
// Public Starlark signatures mirror Bazel's keyword-rich action APIs; the
// attribute macro also generates matching high-arity call trampolines.
#![allow(clippy::too_many_arguments)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{
    FrozenHeapName, Heap, NoSerialize, OwnedFrozenValue, StarlarkPagable, StarlarkValue, Value,
    starlark_value,
};

use crate::action::{Action, WriteAction};
use crate::custom_rule::{AttrSpec, AttrValue};
use crate::label::Label;
use crate::package::Rule;
use crate::repository::RepositorySet;
use crate::starlark_types::{
    File, alloc_file, alloc_target, default_files, file_list, unpack_file,
};

#[derive(Clone, Debug)]
pub struct DependencyTarget {
    pub label: String,
    pub files: Vec<File>,
    pub is_source: bool,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub provider_files: Vec<File>,
}

#[derive(Clone, Debug)]
pub struct RegisteredAction {
    pub action: Action,
    pub inputs: Vec<File>,
    pub outputs: Vec<File>,
}

#[derive(Debug)]
pub struct AnalysisResult {
    pub files: Vec<File>,
    pub declared: Vec<File>,
    pub actions: Vec<RegisteredAction>,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub provider_files: Vec<File>,
}

// The upstream marker derive generates unsafe impls; all data here is owned,
// with no Starlark Values or references hidden from tracing.
#[allow(unsafe_code)]
mod owned_types {
    use super::*;

    #[derive(ProvidesStaticType)]
    pub(super) struct Context {
        pub label: Label,
        pub dependency_files: BTreeSet<File>,
        pub declared: RefCell<Vec<File>>,
        pub actions: RefCell<Vec<RegisteredAction>>,
        pub active: Arc<AtomicBool>,
        pub top_level_functions: Vec<OwnedFrozenValue>,
        pub repositories: Arc<RepositorySet>,
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
    pub(super) struct Actions;
}
use owned_types::{Actions, Context};

impl Drop for Context {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

impl Context {
    fn declare(&self, name: &str) -> anyhow::Result<File> {
        let output = self.label.with_name(name).map_err(anyhow::Error::msg)?;
        let file = File {
            path: output.generated_exec_path().to_string_lossy().into_owned(),
            short_path: output.short_path().to_string_lossy().into_owned(),
            owner: self.label.to_string(),
            is_source: false,
        };
        let mut declared = self.declared.borrow_mut();
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

    fn known_file(&self, file: &File) -> bool {
        self.dependency_files.contains(file) || self.declared.borrow().contains(file)
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        outputs: Vec<File>,
        inputs: Vec<File>,
        command: String,
        write: Option<WriteAction>,
        program: Option<String>,
        arguments: Vec<String>,
        environment: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        ensure!(
            !outputs.is_empty(),
            "an action must have at least one output"
        );
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
            ensure!(
                self.known_file(input),
                "action input {} is not a dependency or declared output",
                input.path
            );
        }
        let action = Action {
            id: format!("{}#{}", self.label, actions.len()),
            command,
            inputs: Vec::new(),
            outputs: outputs
                .iter()
                .map(|file| file.path.clone().into())
                .collect(),
            write,
            program,
            shell_strict: false,
            arguments,
            environment,
        };
        crate::action::validate_structure(&action).map_err(anyhow::Error::msg)?;
        actions.push(RegisteredAction {
            action,
            inputs,
            outputs,
        });
        Ok(())
    }
}

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow!("ctx.actions is only available during rule analysis"))
}

pub(crate) fn is_top_level_mapper(eval: &Evaluator<'_, '_, '_>, value: Value<'_>) -> bool {
    context(eval).is_ok_and(|context| {
        context
            .top_level_functions
            .iter()
            .any(|function| function.value().ptr_eq(value))
    })
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    context(eval)
        .ok()
        .map(|context| context.repositories.clone())
}

impl fmt::Display for Actions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ctx.actions")
    }
}

starlark::starlark_simple_value!(Actions);
starlark::methods_static!(ACTION_METHODS = all_action_methods);

fn all_action_methods(builder: &mut MethodsBuilder) {
    action_methods(builder);
    direct_action_methods(builder);
}

#[starlark_value(type = "actions")]
impl<'v> StarlarkValue<'v> for Actions {
    fn get_methods() -> Option<&'static Methods> {
        Some(ACTION_METHODS.methods())
    }
}

fn string_list(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    crate::args::expand_arguments(value)
}

fn environment(value: Value<'_>) -> anyhow::Result<BTreeMap<String, String>> {
    let dict =
        DictRef::from_value(value).ok_or_else(|| anyhow!("env must be a string-to-string dict"))?;
    dict.iter()
        .map(|(key, value)| {
            Ok((
                key.unpack_str()
                    .ok_or_else(|| anyhow!("env keys must be strings"))?
                    .to_owned(),
                value
                    .unpack_str()
                    .ok_or_else(|| anyhow!("env values must be strings"))?
                    .to_owned(),
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn action_methods(builder: &mut MethodsBuilder) {
    fn args<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(crate::args::alloc_args(
            eval.heap(),
            context(eval)?.active.clone(),
        ))
    }
    fn declare_file<'v>(
        this: Value<'v>,
        filename: &str,
        #[starlark(default = NoneType)] sibling: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let context = context(eval)?;
        let name = if sibling.is_none() {
            filename.to_owned()
        } else {
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
            let path = std::path::Path::new(&sibling.path);
            let exec = if sibling.is_source {
                path
            } else {
                path.strip_prefix("bazel-out/rust-fastbuild/bin")
                    .map_err(|_| anyhow!("invalid generated sibling execution path"))?
            };
            let relative = exec
                .strip_prefix(context.label.package_exec_path())
                .map_err(|_| anyhow!("sibling is outside the current package"))?;
            relative
                .parent()
                .unwrap_or_else(|| std::path::Path::new(""))
                .join(filename)
                .to_string_lossy()
                .into_owned()
        };
        Ok(alloc_file(eval.heap(), context.declare(&name)?))
    }

    fn write<'v>(
        this: Value<'v>,
        output: Value<'v>,
        content: &str,
        #[starlark(default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        context(eval)?.register(
            vec![unpack_file(output)?],
            Vec::new(),
            String::new(),
            Some(WriteAction {
                content: content.as_bytes().to_vec(),
                executable: is_executable,
            }),
            None,
            Vec::new(),
            BTreeMap::new(),
        )?;
        Ok(NoneType)
    }

    fn run_shell<'v>(
        this: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, mnemonic, progress_message);
        ensure!(
            !use_default_shell_env,
            "use_default_shell_env=True is unsupported"
        );
        ensure!(
            ListRef::from_value(outputs).is_some(),
            "outputs must be a list of Files"
        );
        context(eval)?.register(
            file_list(outputs)?,
            inputs.map(file_list).transpose()?.unwrap_or_default(),
            command.to_owned(),
            None,
            None,
            arguments.map(string_list).transpose()?.unwrap_or_default(),
            env.map(environment).transpose()?.unwrap_or_default(),
        )?;
        Ok(NoneType)
    }
}

#[allow(clippy::too_many_arguments)]
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
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, mnemonic, progress_message);
        ensure!(
            !use_default_shell_env,
            "use_default_shell_env=True is unsupported"
        );
        ensure!(
            ListRef::from_value(outputs).is_some(),
            "outputs must be a list of Files"
        );
        let mut inputs = inputs.map(file_list).transpose()?.unwrap_or_default();
        inputs.extend(tools.map(file_list).transpose()?.unwrap_or_default());
        let program = if let Some(program) = executable.unpack_str() {
            program.to_owned()
        } else {
            let file = unpack_file(executable)?;
            let program = format!("./{}", file.path);
            inputs.push(file);
            program
        };
        context(eval)?.register(
            file_list(outputs)?,
            inputs,
            String::new(),
            None,
            Some(program),
            arguments.map(string_list).transpose()?.unwrap_or_default(),
            env.map(environment).transpose()?.unwrap_or_default(),
        )?;
        Ok(NoneType)
    }
}

fn dependency<'a>(
    label: &str,
    name: &str,
    spec: &AttrSpec,
    dependencies: &'a BTreeMap<String, DependencyTarget>,
) -> anyhow::Result<&'a DependencyTarget> {
    let target = dependencies
        .get(label)
        .ok_or_else(|| anyhow!("attribute {name}: dependency {label} was not resolved"))?;
    ensure!(
        !target.is_source || spec.allow_files || spec.allow_single_file,
        "attribute {name}: source file {label} is not allowed (allow_files=False)"
    );
    if spec.allow_single_file {
        ensure!(
            target.files.len() == 1,
            "attribute {name}: {label} must provide exactly one file, got {}",
            target.files.len()
        );
    }
    Ok(target)
}

fn make_ctx<'v>(
    rule: &Rule,
    context: &Context,
    dependencies: &BTreeMap<String, DependencyTarget>,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a custom rule", rule.label))?;
    let mut attrs = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut single_files = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    attrs.insert("name".to_owned(), heap.alloc(rule.label.name.as_str()));
    for (name, value) in &invocation.attrs {
        let spec = invocation
            .schema
            .get(name)
            .ok_or_else(|| anyhow!("attribute {name} has no schema"))?;
        let attr = match value {
            AttrValue::String(value) => heap.alloc(value.as_str()),
            AttrValue::Strings(value) => heap.alloc(value.clone()),
            AttrValue::StringListDict(entries) => {
                crate::attribute_values::alloc_string_list_dict(heap, entries)?
            }
            AttrValue::Int(value) => heap.alloc(*value),
            AttrValue::Bool(value) => heap.alloc(*value),
            AttrValue::Label(value) => {
                let target = value
                    .as_ref()
                    .map(|label| dependency(label, name, spec, dependencies))
                    .transpose()?;
                let target_files = target
                    .map(|target| target.files.clone())
                    .unwrap_or_default();
                files.insert(
                    name.clone(),
                    heap.alloc(
                        target_files
                            .iter()
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                if spec.allow_single_file {
                    single_files.insert(
                        name.clone(),
                        target_files
                            .first()
                            .map(|file| alloc_file(heap, file.clone()))
                            .unwrap_or_else(Value::new_none),
                    );
                }
                target
                    .map(|target| {
                        crate::starlark_types::alloc_target_with_providers(
                            heap,
                            &target.label,
                            target.files.clone(),
                            target.providers.clone(),
                        )
                    })
                    .unwrap_or_else(Value::new_none)
            }
            AttrValue::Labels(labels) => {
                let targets = labels
                    .iter()
                    .map(|label| dependency(label, name, spec, dependencies))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                files.insert(
                    name.clone(),
                    heap.alloc(
                        targets
                            .iter()
                            .flat_map(|target| target.files.iter())
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                heap.alloc(
                    targets
                        .iter()
                        .map(|target| {
                            crate::starlark_types::alloc_target_with_providers(
                                heap,
                                &target.label,
                                target.files.clone(),
                                target.providers.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }
            AttrValue::Output(value) => {
                let output = value
                    .as_ref()
                    .map(|name| context.declare(name))
                    .transpose()?
                    .map(|file| alloc_file(heap, file))
                    .unwrap_or_else(Value::new_none);
                outputs.insert(name.clone(), output);
                // Output attributes belong to ctx.outputs, not ctx.attr.
                continue;
            }
            AttrValue::Outputs(values) => {
                let output = heap.alloc(
                    values
                        .iter()
                        .map(|name| context.declare(name).map(|file| alloc_file(heap, file)))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                );
                outputs.insert(name.clone(), output);
                continue;
            }
        };
        attrs.insert(name.clone(), attr);
    }
    let label = alloc_target(heap, &rule.label.to_string(), Vec::new())
        .get_attr_error("label", heap)
        .map_err(|error| anyhow!(error.to_string()))?;
    let backing = heap.alloc(AllocStruct([
        ("attr", heap.alloc(AllocStruct(attrs))),
        ("files", heap.alloc(AllocStruct(files))),
        ("file", heap.alloc(AllocStruct(single_files))),
        ("outputs", heap.alloc(AllocStruct(outputs))),
        ("label", label),
        ("actions", heap.alloc(Actions)),
    ]));
    if invocation.build_setting.is_some() {
        let default = invocation
            .attrs
            .get("build_setting_default")
            .ok_or_else(|| anyhow!("build-setting invocation has no typed default"))?;
        Ok(crate::build_setting_ctx::wrap(
            heap,
            backing,
            default.clone(),
        ))
    } else {
        Ok(backing)
    }
}

pub fn analyze(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> Result<AnalysisResult, String> {
    analyze_in(
        rule,
        dependencies,
        Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
    )
}

pub fn analyze_in(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
) -> Result<AnalysisResult, String> {
    analyze_inner(rule, dependencies, repositories)
        .map_err(|error| format!("analysis of {}: {error:#}", rule.label))
}

fn analyze_inner(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
) -> anyhow::Result<AnalysisResult> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("not a custom rule"))?;
    for (name, spec) in &invocation.schema {
        if spec.non_empty
            && matches!(invocation.attrs.get(name), Some(AttrValue::StringListDict(entries)) if entries.is_empty())
        {
            return Err(anyhow!("attribute {name}: attribute must be non empty"));
        }
    }
    let context = Context {
        label: rule.label.clone(),
        dependency_files: dependencies
            .values()
            .flat_map(|target| target.files.iter().chain(&target.provider_files).cloned())
            .collect(),
        declared: RefCell::new(Vec::new()),
        actions: RefCell::new(Vec::new()),
        active: Arc::new(AtomicBool::new(true)),
        top_level_functions: invocation.top_level_functions.clone(),
        repositories,
    };
    for output in &rule.outs {
        context.declare(output)?;
    }
    let (files, providers, provider_files) =
        Module::with_temp_heap(|module| -> anyhow::Result<_> {
            let ctx = make_ctx(rule, &context, dependencies, module.heap())?;
            // Bazel's implicit DefaultInfo includes only predeclared outputs.
            let predeclared = context.declared.borrow().clone();
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.extra = Some(&context);
            let implementation = module
                .heap()
                .access_owned_frozen_value(&invocation.implementation);
            let returned = eval
                .eval_function(implementation, &[ctx], &[])
                .map_err(|error| anyhow!(error.to_string()))?;
            let files = default_files(returned)?.unwrap_or(predeclared);
            let custom = crate::providers::returned_custom_providers(returned)?;
            let mut slots = Vec::new();
            let mut provider_files = BTreeSet::new();
            for (identity, provider) in custom {
                provider_files.extend(crate::providers::provider_files(provider)?);
                let slot = format!("$provider:{}", slots.len());
                module.set(&slot, provider);
                slots.push((identity, slot));
            }
            drop(eval);
            let frozen = module
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "analysis:{}",
                    rule.label
                ))))
                .map_err(|error| anyhow!("cannot freeze analysis providers: {error:?}"))?;
            let providers = slots
                .into_iter()
                .map(|(identity, slot)| {
                    frozen
                        .get_any_visibility(&slot)
                        .map(|(value, _)| (identity, value))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            Ok((
                files,
                providers,
                provider_files.into_iter().collect::<Vec<_>>(),
            ))
        })?;
    for file in files.iter().chain(&provider_files) {
        ensure!(
            context.known_file(file),
            "DefaultInfo contains unknown file {}",
            file.path
        );
    }
    let declared = std::mem::take(&mut *context.declared.borrow_mut());
    let actions = std::mem::take(&mut *context.actions.borrow_mut());
    for file in &declared {
        if !actions.iter().any(|action| action.outputs.contains(file)) {
            bail!("declared output {} has no generating action", file.path);
        }
    }
    let mut unique = BTreeSet::new();
    let files = files
        .into_iter()
        .filter(|file| unique.insert(file.clone()))
        .collect();
    Ok(AnalysisResult {
        files,
        declared,
        actions,
        providers,
        provider_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Globals;
    use starlark::syntax::{AstModule, Dialect};

    fn empty_context() -> Context {
        Context {
            label: Label::parse("//pkg:rule", "").unwrap(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
        }
    }

    fn evaluate(source: &str) -> anyhow::Result<Context> {
        let context = empty_context();
        Module::with_temp_heap(|module| -> anyhow::Result<()> {
            module.set(
                "ctx",
                module
                    .heap()
                    .alloc(AllocStruct([("actions", module.heap().alloc(Actions))])),
            );
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            let ast = AstModule::parse("analysis-test.star", source.to_owned(), &Dialect::Standard)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.eval_module(ast, &Globals::standard())
                .map_err(|error| anyhow!(error.to_string()))?;
            Ok(())
        })?;
        Ok(context)
    }

    #[test]
    fn declarations_are_owned_canonical_and_idempotent() {
        let context = empty_context();
        let first = context.declare("dir/result").unwrap();
        assert_eq!(first, context.declare("dir/result").unwrap());
        assert_eq!(first.path, "bazel-out/rust-fastbuild/bin/pkg/dir/result");
        assert_eq!(first.short_path, "pkg/dir/result");
        assert_eq!(first.owner, "//pkg:rule");
        assert!(!first.is_source);
        assert_eq!(context.declared.borrow().len(), 1);
        for name in [
            "",
            "../out",
            "/out",
            "//other:out",
            ":out",
            "a//b",
            "a/./b",
            "a\\b",
        ] {
            assert!(context.declare(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn write_records_exact_bytes_without_shell_or_io() {
        let context = evaluate("out = ctx.actions.declare_file('out')\nctx.actions.write(out, 'hi\\x00hé\\n', is_executable=True)\n").unwrap();
        let actions = context.actions.borrow();
        let action = &actions[0];
        assert_eq!(action.action.id, "//pkg:rule#0");
        assert!(action.action.command.is_empty());
        assert!(action.action.inputs.is_empty());
        assert!(action.inputs.is_empty());
        let write = action.action.write.as_ref().unwrap();
        assert_eq!(write.content, "hi\0hé\n".as_bytes());
        assert!(write.executable);
    }

    #[test]
    fn run_shell_records_arguments_environment_and_local_edges() {
        let context = evaluate(r#"first = ctx.actions.declare_file('first')
second = ctx.actions.declare_file('second')
ctx.actions.run_shell(outputs=[second], inputs=[first], command='cat "$1" > "$2"', arguments=[first.path, second.path], env={'WORD': 'a b'}, mnemonic='Copy', progress_message='copying')
ctx.actions.write(first, 'hello')
"#).unwrap();
        let actions = context.actions.borrow();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].inputs, actions[1].outputs);
        assert_eq!(actions[0].action.environment["WORD"], "a b");
        assert_eq!(actions[0].action.arguments.len(), 2);
        assert!(actions[0].action.inputs.is_empty());
        assert!(actions[0].action.write.is_none());
    }

    #[test]
    fn duplicate_producers_and_unsupported_options_are_rejected() {
        for (body, expected) in [
            (
                "ctx.actions.write(out, 'a')\nctx.actions.write(out, 'b')",
                "more than one",
            ),
            (
                "ctx.actions.run_shell(outputs=[], command='true')",
                "at least one",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', use_default_shell_env=True)",
                "unsupported",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', tools=[])",
                "tools",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', arguments=[1])",
                "strings",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', env={'X': 1})",
                "strings",
            ),
        ] {
            let source = format!("out = ctx.actions.declare_file('out')\n{body}\n");
            let error = match evaluate(&source) {
                Ok(_) => panic!("accepted {body}"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(expected), "expected {expected:?} in {error}");
        }
    }

    #[test]
    fn foreign_outputs_and_unknown_inputs_are_rejected() {
        let context = empty_context();
        let output = context.declare("out").unwrap();
        let mut foreign = output.clone();
        foreign.owner = "//other:rule".into();
        assert!(
            context
                .register(
                    vec![foreign.clone()],
                    Vec::new(),
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new()
                )
                .is_err()
        );
        assert!(
            context
                .register(
                    vec![output],
                    vec![foreign],
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new()
                )
                .is_err()
        );
        assert!(context.actions.borrow().is_empty());
    }
}
