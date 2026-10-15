//! Analysis capabilities and owned scope metadata, below rule orchestration.
//! No Starlark values are hidden in the scope stack.
// Public action signatures and their generated trampolines are keyword-rich.
#![allow(clippy::too_many_arguments)]
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, ensure};

use crate::action::{Action, WriteAction};
use crate::label::Label;
use crate::repository::RepositorySet;
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

#[derive(Clone, Debug)]
pub struct RegisteredAction {
    pub action: Action,
    pub inputs: Vec<File>,
    pub outputs: Vec<File>,
}

// Marker derive only. These fields own Rust data, never hidden heap Values.
#[allow(unsafe_code)]
mod owned {
    use super::*;
    #[derive(ProvidesStaticType)]
    pub(crate) struct Context {
        pub label: Label,
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
        pub top_level_functions: Vec<OwnedFrozenValue>,
        pub repositories: Arc<RepositorySet>,
        pub scope: Arc<ScopeState>,
        /// The rule's subrule inputs: lifted attribute values and toolchain
        /// state (crate::subrule::RuleEnvironment).
        pub subrule: crate::subrule::RuleEnvironment,
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct Actions {
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        /// The repr of the owning rule or subrule context.
        pub context: String,
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
    pub(crate) fn declare(&self, name: &str) -> anyhow::Result<File> {
        let output = self.label.with_name(name).map_err(anyhow::Error::msg)?;
        let file = File {
            path: output
                .generated_exec_path(&self.output_root)
                .to_string_lossy()
                .into_owned(),
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

    pub(crate) fn known_file(&self, file: &File) -> bool {
        self.dependency_files.contains(file) || self.declared.borrow().contains(file)
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

pub(crate) fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow!("ctx.actions and subrules are only available during rule analysis"))
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

/// StarlarkActionFactory.repr (analysis/starlark/StarlarkActionFactory.java:1284-1287):
/// "actions for" immediately followed by the context's repr.
impl fmt::Display for Actions {
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
    heap.alloc_complex_no_freeze(Actions { scope, context })
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
        content: Value<'v>,
        #[starlark(default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = action_receiver(this, eval, false)?;
        let content = if let Some(text) = content.unpack_str() {
            text.as_bytes().to_vec()
        } else {
            crate::args::serialize_write_content(content)?
        };
        context.register(
            vec![unpack_file(output)?],
            Vec::new(),
            String::new(),
            Some(WriteAction {
                content,
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
        let context = action_receiver(this, eval, false)?;
        let _ = (mnemonic, progress_message);
        ensure!(
            !use_default_shell_env,
            "use_default_shell_env=True is unsupported"
        );
        ensure!(
            ListRef::from_value(outputs).is_some(),
            "outputs must be a list of Files"
        );
        context.register(
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
        let context = action_receiver(this, eval, false)?;
        let _ = (mnemonic, progress_message);
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
        context.register(
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
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
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
            label: Label::parse_canonical("//:target").unwrap(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(RepositorySet::new(".".into())),
            scope: ScopeState::new("rule".into(), BTreeSet::from(["S".into()])),
            subrule: Default::default(),
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
