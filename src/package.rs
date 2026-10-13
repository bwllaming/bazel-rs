//! Native BUILD evaluation, local macros, and frozen custom-rule declarations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use starlark::any::ProvidesStaticType;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Arguments, Evaluator, ReturnFileLoader};
use starlark::starlark_module;
use starlark::syntax::ast::{Argument, Expr};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;

use crate::build_settings::BuildSetting;
use crate::custom_rule::{AttrSpec, AttrValue, CustomInvocation};
use crate::label::{Label, PackageId};
use crate::package_specification::{self, GroupDefinition};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;
use crate::visibility::Visibility;

#[derive(Clone, Debug)]
pub enum RuleKind {
    Filegroup,
    Genrule,
    Custom,
    #[allow(private_interfaces)]
    PackageGroup(Arc<GroupDefinition>),
}

#[derive(Clone, Debug)]
pub struct MacroOwner {
    pub definition_package: PackageId,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub label: Label,
    pub kind: RuleKind,
    pub srcs: Vec<String>,
    pub outs: Vec<String>,
    pub cmd: Option<String>,
    pub visibility: Visibility,
    pub custom: Option<CustomInvocation>,
    pub macro_owner: Option<MacroOwner>,
}

#[derive(Clone, Debug, Default)]
pub struct Package {
    pub rules: BTreeMap<Label, Rule>,
    pub exports: BTreeMap<Label, Visibility>,
    pub default_visibility: Visibility,
    pub macro_names: BTreeSet<String>,
}

impl std::ops::Deref for Package {
    type Target = BTreeMap<Label, Rule>;
    fn deref(&self) -> &Self::Target {
        &self.rules
    }
}

// The upstream derive generates an unsafe marker-trait implementation. This
// collector contains only owned 'static Rust data; no handwritten unsafe code.
#[allow(unsafe_code)]
mod collector {
    use super::*;
    #[derive(ProvidesStaticType)]
    pub(super) struct Declarations {
        pub package: String,
        pub repository: String,
        pub repositories: Arc<RepositorySet>,
        pub root: PathBuf,
        pub rules: RefCell<Vec<Rule>>,
        pub exports: RefCell<BTreeMap<Label, Visibility>>,
        pub default_visibility: RefCell<Visibility>,
        pub defaulted_rules: RefCell<BTreeSet<Label>>,
        pub package_called: Cell<bool>,
        pub custom: RefCell<Vec<PendingCustom>>,
        pub active_macro: RefCell<Option<MacroOwner>>,
        pub macro_names: RefCell<BTreeSet<String>>,
        // An index of actual rule/output/export declarations, not inferred inputs.
        pub declared_names: RefCell<BTreeSet<String>>,
        pub group_budget: RefCell<HostBudget>,
    }
}
use collector::Declarations;

#[derive(Debug)]
struct PendingCustom {
    index: usize,
    slot: String,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
    build_setting: Option<BuildSetting>,
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .map(|context| context.repositories.clone())
}

pub(crate) fn custom_label_context(
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<(Arc<RepositorySet>, PackageId)> {
    let context = declarations(eval)?;
    Ok((context.repositories.clone(), context.package_id()))
}

pub(crate) fn register_custom<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    name: &str,
    visibility: Option<Value<'v>>,
    implementation: Value<'v>,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
    build_setting: Option<BuildSetting>,
) -> anyhow::Result<()> {
    let context = declarations(eval)?;
    let mut srcs = Vec::new();
    let mut outs = Vec::new();
    for value in attrs.values() {
        match value {
            AttrValue::Label(Some(value)) => srcs.push(value.clone()),
            AttrValue::Labels(values) => srcs.extend(values.iter().cloned()),
            AttrValue::Output(Some(value)) => outs.push(value.clone()),
            AttrValue::Outputs(values) => outs.extend(values.iter().cloned()),
            _ => {}
        }
    }
    let index = context.rules.borrow().len();
    context.declare(
        name,
        RuleKind::Custom,
        srcs,
        outs,
        None,
        parse_visibility(visibility, context)?,
    )?;
    // Not a valid Starlark identifier: BUILD assignments cannot overwrite roots.
    let slot = format!("__custom_implementation${index}");
    eval.module().set(&slot, implementation);
    context.custom.borrow_mut().push(PendingCustom {
        index,
        slot,
        attrs,
        schema,
        build_setting,
    });
    Ok(())
}

impl Declarations {
    fn check_macro_collision(&self, name: &str) -> anyhow::Result<()> {
        if self.macro_names.borrow().contains(name)
            && self
                .active_macro
                .borrow()
                .as_ref()
                .is_none_or(|owner| owner.name != name)
        {
            bail!("target {name:?} conflicts with an existing macro");
        }
        Ok(())
    }

    fn forbid_in_macro(&self, function: &str) -> anyhow::Result<()> {
        if self.active_macro.borrow().is_some() {
            bail!("{function}() is not allowed inside a symbolic macro");
        }
        Ok(())
    }

    fn package_id(&self) -> PackageId {
        PackageId {
            repository: self.repository.clone(),
            package: self.package.clone(),
        }
    }

    fn declare(
        &self,
        name: &str,
        kind: RuleKind,
        srcs: Vec<String>,
        outs: Vec<String>,
        cmd: Option<String>,
        visibility: Option<Visibility>,
    ) -> anyhow::Result<NoneType> {
        let label = local_label(name, &self.package_id(), "rule name")?;
        self.check_macro_collision(name)?;
        for src in &srcs {
            (if matches!(kind, RuleKind::Custom) {
                Label::parse_canonical(src).map(|_| ())
            } else {
                Label::validate_syntax(src, &self.package)
            })
            .map_err(|e| anyhow!("invalid srcs entry {src:?} in {label}: {e}"))?;
        }
        if matches!(kind, RuleKind::Genrule) && outs.is_empty() {
            bail!("genrule {label}: outs must not be empty");
        }
        for out in &outs {
            self.check_macro_collision(out)?;
            label
                .with_name(out)
                .map_err(|e| anyhow!("invalid output path {out:?}: {e}"))?;
        }
        let macro_owner = self.active_macro.borrow().clone();
        let visibility = if let Some(owner) = &macro_owner {
            visibility
                .unwrap_or(Visibility::Private)
                .with_package(&owner.definition_package)
        } else {
            if visibility.is_none() {
                self.defaulted_rules.borrow_mut().insert(label.clone());
            }
            visibility.unwrap_or_else(|| self.default_visibility.borrow().clone())
        };
        {
            let mut names = self.declared_names.borrow_mut();
            names.insert(name.to_owned());
            names.extend(outs.iter().cloned());
        }
        self.rules.borrow_mut().push(Rule {
            label,
            kind,
            srcs,
            outs,
            cmd,
            visibility,
            custom: None,
            macro_owner,
        });
        Ok(NoneType)
    }
}

fn local_label(name: &str, package: &PackageId, what: &str) -> anyhow::Result<Label> {
    // Prefixing ':' ensures names cannot change the package or repository.
    Label {
        repository: package.repository.clone(),
        package: package.package.clone(),
        name: "__package__".into(),
    }
    .with_name(name)
    .map_err(|e| anyhow!("invalid {what} {name:?}: {e}"))
}

fn strings(value: Value<'_>, attribute: &str) -> anyhow::Result<Vec<String>> {
    let list = ListRef::from_value(value)
        .ok_or_else(|| anyhow!("{attribute} must be a list of strings"))?;
    list.iter()
        .map(|item| {
            item.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("{attribute} must be a list of strings"))
        })
        .collect()
}

fn parse_visibility(
    value: Option<Value<'_>>,
    context: &Declarations,
) -> anyhow::Result<Option<Visibility>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    Visibility::from_labels(visibility_labels(value, context)?)
        .map(Some)
        .map_err(anyhow::Error::msg)
}

fn visibility_labels(value: Value<'_>, context: &Declarations) -> anyhow::Result<Vec<Label>> {
    let items = if let Some(list) = ListRef::from_value(value) {
        list.content()
    } else if let Some(tuple) = TupleRef::from_value(value) {
        tuple.content()
    } else {
        bail!("visibility must be a list or tuple of strings or Labels");
    };
    let mut budget = crate::repository_budget::HostBudget::default();
    items
        .iter()
        .map(|value| {
            if let Some((repository, package, name)) = crate::starlark_types::label_parts(*value) {
                for part in [repository, package, name] {
                    budget.string(part).map_err(anyhow::Error::msg)?;
                }
                Ok(Label {
                    repository: repository.to_owned(),
                    package: package.to_owned(),
                    name: name.to_owned(),
                })
            } else if let Some(text) = value.unpack_str() {
                let bytes = context
                    .repositories
                    .resolution_size_bound(text, &context.repository, &context.package)
                    .map_err(anyhow::Error::msg)?;
                budget.bytes(bytes).map_err(anyhow::Error::msg)?;
                budget.node().map_err(anyhow::Error::msg)?;
                context
                    .repositories
                    .parse(text, &context.repository, &context.package)
                    .map_err(anyhow::Error::msg)
            } else {
                bail!("visibility entries must be strings or Labels")
            }
        })
        .collect()
}

struct MacroFrame<'a> {
    slot: &'a RefCell<Option<MacroOwner>>,
    previous: Option<MacroOwner>,
}

impl<'a> MacroFrame<'a> {
    fn enter(slot: &'a RefCell<Option<MacroOwner>>, owner: MacroOwner) -> Self {
        Self {
            slot,
            previous: slot.replace(Some(owner)),
        }
    }
}

impl Drop for MacroFrame<'_> {
    fn drop(&mut self) {
        self.slot.replace(self.previous.take());
    }
}

pub(crate) fn invoke_symbolic_macro<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    definition_label: &str,
    exported: Option<&str>,
    implementation: Value<'v>,
    args: &Arguments<'v, '_>,
) -> anyhow::Result<Value<'v>> {
    // Copy the independent extra reference: never borrow eval or a RefMut over
    // the callback, whose helpers reenter this same declaration collector.
    let extra = eval.extra;
    let context = extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .ok_or_else(|| anyhow!("symbolic macros may only be invoked during BUILD evaluation"))?;
    if exported.is_none() {
        bail!("symbolic macro must be exported before invocation");
    }
    if context.active_macro.borrow().is_some() {
        bail!("nested symbolic macro invocations are unsupported");
    }
    args.no_positional_args(eval.heap())
        .map_err(|e| anyhow!(e.to_string()))?;
    let named = args.names_map().map_err(|e| anyhow!(e.to_string()))?;
    let mut name = None;
    let mut visibility = None;
    for (key, value) in named.iter() {
        match key.as_str() {
            // Deliberately reject None cleanly (upstream 9.3 crashes here).
            "name" => {
                name = Some(
                    value
                        .unpack_str()
                        .ok_or_else(|| anyhow!("macro name must be a string"))?,
                )
            }
            "visibility" => visibility = Some(*value),
            key => bail!("unknown macro attribute {key:?}"),
        }
    }
    let name = name.ok_or_else(|| anyhow!("missing mandatory macro attribute name"))?;
    local_label(name, &context.package_id(), "macro name")?;
    let raw = match visibility.filter(|value| !value.is_none()) {
        Some(value) => visibility_labels(value, context)?,
        None => context.default_visibility.borrow().declared_labels(),
    };
    let labels = crate::visibility::macro_visibility_labels(raw, &context.package_id())
        .map_err(anyhow::Error::msg)?;
    let owner = MacroOwner {
        definition_package: Label::parse_canonical(definition_label)
            .map_err(anyhow::Error::msg)?
            .package_id(),
        name: name.to_owned(),
    };
    if context.declared_names.borrow().contains(name) || context.macro_names.borrow().contains(name)
    {
        bail!("macro {name:?} conflicts with an existing target or macro");
    }
    // Freeze a fresh list on an isolated heap, never the caller's containers.
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let values = labels
            .iter()
            .map(|label| crate::starlark_types::alloc_label(module.heap(), &label.to_string()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        module.set("visibility", module.heap().alloc(values));
        let frozen = module
            .freeze()
            .map_err(|e| anyhow!("cannot freeze macro visibility: {e:?}"))?;
        frozen.get("visibility").map_err(|e| anyhow!(e.to_string()))
    })?;
    let visibility = eval.heap().access_owned_frozen_value(&owned);
    context.macro_names.borrow_mut().insert(name.to_owned());
    let _frame = MacroFrame::enter(&context.active_macro, owner);
    let result = eval
        .eval_function(
            implementation,
            &[],
            &[
                ("name", eval.heap().alloc(name)),
                ("visibility", visibility),
            ],
        )
        .map_err(|e| anyhow!(e.to_string()))?;
    if !result.is_none() {
        bail!("symbolic macro {name:?} must return None");
    }
    Ok(Value::new_none())
}

fn declarations<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Declarations> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .ok_or_else(|| anyhow!("rule declaration outside package evaluation"))
}

#[starlark_module]
fn rule_globals(builder: &mut GlobalsBuilder) {
    fn package_group<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] packages: Option<Value<'v>>,
        #[starlark(require = named)] includes: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        let specs = packages
            .map(|value| {
                crate::custom_rule::sequence_ref(value).ok_or_else(|| {
                    anyhow!("package_group packages must be a list or tuple of strings")
                })
            })
            .transpose()?
            .unwrap_or(&[]);
        let edges = includes
            .map(|value| {
                crate::custom_rule::sequence_ref(value).ok_or_else(|| {
                    anyhow!("package_group includes must be a list or tuple of strings or Labels")
                })
            })
            .transpose()?
            .unwrap_or(&[]);
        // Charge all borrowed data cumulatively, including mapped-label expansion,
        // before copying either sequence or any string into host-owned storage.
        let mut budget = context.group_budget.borrow_mut();
        for part in [name, &context.repository, &context.package] {
            budget.string(part).map_err(anyhow::Error::msg)?;
        }
        for value in specs {
            let text = value
                .unpack_str()
                .ok_or_else(|| anyhow!("package_group packages entries must be strings"))?;
            budget.string(text).map_err(anyhow::Error::msg)?;
            let text = text.strip_prefix('-').unwrap_or(text);
            if text.starts_with('/') || text.starts_with('@') {
                budget
                    .bytes(
                        context
                            .repositories
                            .resolution_size_bound(text, &context.repository, &context.package)
                            .map_err(anyhow::Error::msg)?
                            .saturating_add(2),
                    )
                    .map_err(anyhow::Error::msg)?;
            }
        }
        for value in edges {
            budget.node().map_err(anyhow::Error::msg)?;
            if let Some((repository, package, name)) = crate::starlark_types::label_parts(*value) {
                for part in [repository, package, name] {
                    budget.string(part).map_err(anyhow::Error::msg)?;
                }
            } else if let Some(text) = value.unpack_str() {
                budget.string(text).map_err(anyhow::Error::msg)?;
                budget
                    .bytes(
                        context
                            .repositories
                            .resolution_size_bound(text, &context.repository, &context.package)
                            .map_err(anyhow::Error::msg)?,
                    )
                    .map_err(anyhow::Error::msg)?;
            } else {
                bail!("package_group includes entries must be strings or Labels");
            }
        }
        drop(budget);
        let specs = specs
            .iter()
            .map(|value| value.unpack_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let own_specs = package_specification::parse_specs(
            &specs,
            &context.package_id(),
            &context.repositories,
        )
        .map_err(anyhow::Error::msg)?;
        let includes = edges
            .iter()
            .map(|value| {
                if let Some((repository, package, name)) =
                    crate::starlark_types::label_parts(*value)
                {
                    Ok(Label {
                        repository: repository.to_owned(),
                        package: package.to_owned(),
                        name: name.to_owned(),
                    })
                } else {
                    context
                        .repositories
                        .parse(
                            value.unpack_str().unwrap(),
                            &context.repository,
                            &context.package,
                        )
                        .map_err(anyhow::Error::msg)
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        context.declare(
            name,
            RuleKind::PackageGroup(Arc::new(GroupDefinition {
                own_specs,
                includes,
            })),
            Vec::new(),
            Vec::new(),
            None,
            Some(Visibility::Public),
        )
    }

    fn filegroup<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.declare(
            name,
            RuleKind::Filegroup,
            srcs.map(|v| strings(v, "srcs"))
                .transpose()?
                .unwrap_or_default(),
            Vec::new(),
            None,
            parse_visibility(visibility, context)?,
        )
    }

    fn genrule<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] outs: Value<'v>,
        #[starlark(require = named)] cmd: &str,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.declare(
            name,
            RuleKind::Genrule,
            srcs.map(|v| strings(v, "srcs"))
                .transpose()?
                .unwrap_or_default(),
            strings(outs, "outs")?,
            Some(cmd.to_owned()),
            parse_visibility(visibility, context)?,
        )
    }
}

#[starlark_module]
fn package_globals(builder: &mut GlobalsBuilder) {
    fn package<'v>(
        #[starlark(require = named)] default_visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.forbid_in_macro("package")?;
        if context.package_called.replace(true) {
            bail!("package() may only be called once");
        }
        if default_visibility.is_none() {
            bail!("package() requires at least one argument");
        }
        if default_visibility.is_some_and(|v| v.is_none()) {
            bail!("package default_visibility must be a list");
        }
        *context.default_visibility.borrow_mut() =
            parse_visibility(default_visibility, context)?.unwrap_or_default();
        Ok(NoneType)
    }

    fn exports_files<'v>(
        srcs: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        #[starlark(require = named)] licenses: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        if context.active_macro.borrow().is_some() {
            bail!("exports_files() inside symbolic macros is unsupported");
        }
        if licenses.is_some_and(|value| !value.is_none()) {
            bail!("exports_files licenses metadata is not implemented");
        }
        // exports_files is PUBLIC by default, independent of package defaults.
        let visibility = parse_visibility(visibility, context)?.unwrap_or(Visibility::Public);
        for name in strings(srcs, "exports_files srcs")? {
            let label = local_label(&name, &context.package_id(), "exported file")?;
            context.check_macro_collision(&name)?;
            context.declared_names.borrow_mut().insert(name);
            if let Some(previous) = context
                .exports
                .borrow_mut()
                .insert(label.clone(), visibility.clone())
                && previous != visibility
            {
                bail!("inconsistent visibility for repeated exported file: {label}");
            }
        }
        Ok(NoneType)
    }

    fn glob<'v>(
        include: Option<Value<'v>>,
        exclude: Option<Value<'v>>,
        #[starlark(default = 1)] exclude_directories: i32,
        #[starlark(default = false)] allow_empty: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<String>> {
        let context = declarations(eval)?;
        context.forbid_in_macro("glob")?;
        let includes = include
            .map(|v| strings(v, "glob include"))
            .transpose()?
            .unwrap_or_default();
        let excludes = exclude
            .map(|v| strings(v, "glob exclude"))
            .transpose()?
            .unwrap_or_default();
        crate::glob::glob(
            &context.root,
            &context.package,
            &includes,
            &excludes,
            exclude_directories != 0,
            allow_empty,
        )
        .map_err(anyhow::Error::msg)
    }

    fn package_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        Ok(declarations(eval)?.package.clone())
    }
}

#[derive(Clone, Copy)]
enum BzlPhase {
    Build,
    Module,
}

/// Only the extension-context branch is implemented. BUILD introspection is a
/// separate surface, so these functions are not installed in BUILD native.
#[starlark_module]
fn module_native_globals(builder: &mut GlobalsBuilder) {
    fn existing_rule(
        #[starlark(require = pos)] name: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = name;
        crate::extension_ctx::require_extension_introspection(eval, "existing_rule")?;
        Ok(NoneType)
    }

    fn existing_rules(
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<starlark::values::FrozenValue> {
        crate::extension_ctx::require_extension_introspection(eval, "existing_rules")?;
        Ok(starlark::values::FrozenValue::new_empty_dict())
    }
}

/// BUILD-loaded extension globals. These deliberately omit native.bazel_version.
pub fn bzl_globals() -> Globals {
    bzl_globals_for(BzlPhase::Build)
}

/// MODULE-loaded extensions and repository definitions retain these bindings
/// through initialization, transitive loads and frozen callback execution.
pub fn module_bzl_globals() -> Globals {
    bzl_globals_for(BzlPhase::Module)
}

fn bzl_globals_for(phase: BzlPhase) -> Globals {
    let mut native = GlobalsBuilder::new()
        .with(rule_globals)
        .with(package_globals);
    if matches!(phase, BzlPhase::Module) {
        native.set("bazel_version", crate::COMPATIBILITY_VERSION);
        module_native_globals(&mut native);
    }
    let native = native.build();
    let mut bzl = GlobalsBuilder::extended_by(&[
        LibraryExtension::StructType,
        LibraryExtension::Print,
        LibraryExtension::SetType,
    ]);
    bzl.frozen_heap().add_reference(native.heap());
    bzl.set("native", AllocStruct(native.iter()));
    crate::custom_rule::rule_globals(&mut bzl);
    crate::build_settings::install(&mut bzl);
    crate::template_variable_info::install(&mut bzl);
    crate::run_environment_info::install(&mut bzl);
    package_specification::install(&mut bzl);
    crate::providers::globals(&mut bzl);
    crate::repository_rule::globals(&mut bzl);
    crate::module_extension::globals(&mut bzl);
    crate::symbolic_macro::globals(&mut bzl);
    bzl.build()
}

/// Evaluate a BUILD file with native Starlark; never invoke upstream Bazel.
///
/// The caller supplies a main-repository package path (empty for root). Rule and output
/// names follow the same strict relative-path grammar as `Label`. The local `.bzl`
/// loader freezes modules and resolves relative loads in their declaring package.
/// The call stack is limited to 128 frames; there is no total CPU or
/// heap budget, so BUILD files must be trusted.
pub fn evaluate(root: &Path, package: &str) -> Result<Package, String> {
    evaluate_in(
        Arc::new(RepositorySet::new(root.to_path_buf())),
        &PackageId {
            repository: String::new(),
            package: package.to_owned(),
        },
    )
}

pub fn evaluate_in(
    repositories: Arc<RepositorySet>,
    package: &PackageId,
) -> Result<Package, String> {
    local_label("__package__", package, "package path").map_err(|e| e.to_string())?;
    let root = repositories.root(&package.repository)?;
    let build = crate::workspace::package_build_file(root, &package.package)?;
    let source = std::fs::read_to_string(&build)
        .map_err(|e| format!("cannot read {}: {e}", build.display()))?;
    evaluate_at(repositories, &build.to_string_lossy(), package, source)
}

fn evaluate_at(
    repositories: Arc<RepositorySet>,
    filename: &str,
    package: &PackageId,
    source: String,
) -> Result<Package, String> {
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(filename, source, &dialect).map_err(|e| format!("{e:#}"))?;
    // Bazel BUILD files disallow call splats even though ordinary .bzl files
    // support them. Inspect the real AST, never the textual spelling in strings.
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        if let Expr::Call(_, arguments) = &expr.node
            && arguments
                .args
                .iter()
                .any(|arg| matches!(arg.node, Argument::Args(_) | Argument::KwArgs(_)))
        {
            return Err(format!(
                "{}: *args and **kwargs calls are not allowed in BUILD files",
                ast.file_span(expr.span)
            ));
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType])
        .with(rule_globals)
        .with(package_globals)
        .build();
    let bzl_globals = bzl_globals();
    let loaded = crate::loading::load_modules_with_functions_in(
        repositories.clone(),
        package,
        &ast,
        &bzl_globals,
    )?;
    let modules = loaded
        .modules
        .iter()
        .map(|(name, module)| (name.as_str(), module))
        .collect::<HashMap<_, _>>();
    let loader = ReturnFileLoader { modules: &modules };
    let declarations = Declarations {
        package: package.package.clone(),
        repository: package.repository.clone(),
        root: repositories.root(&package.repository)?.to_path_buf(),
        repositories,
        rules: RefCell::new(Vec::new()),
        exports: RefCell::new(BTreeMap::new()),
        default_visibility: RefCell::new(Visibility::Private),
        defaulted_rules: RefCell::new(BTreeSet::new()),
        package_called: Cell::new(false),
        custom: RefCell::new(Vec::new()),
        active_macro: RefCell::new(None),
        macro_names: RefCell::new(BTreeSet::new()),
        declared_names: RefCell::new(BTreeSet::new()),
        group_budget: RefCell::new(HostBudget::default()),
    };
    let frozen = Module::with_temp_heap(|module| {
        let mut evaluator = Evaluator::new(&module);
        evaluator.extra = Some(&declarations);
        evaluator.set_loader(&loader);
        evaluator
            .set_max_callstack_size(128)
            .map_err(|e| e.to_string())?;
        evaluator.eval_module(ast, &globals).map_err(|e| {
            format!(
                "{e:#}\nSupported rules: filegroup, genrule, and local custom rules. select and non-local repositories remain unsupported."
            )
        })?;
        drop(evaluator);
        module
            .freeze_named(starlark::values::FrozenHeapName::User(Box::new(
                filename.to_owned(),
            )))
            .map_err(|e| format!("cannot freeze BUILD module: {e:?}"))
    })?;
    let mut declared_rules = declarations.rules.into_inner();
    for pending in declarations.custom.into_inner() {
        let (implementation, _) = frozen
            .get_any_visibility(&pending.slot)
            .map_err(|e| e.to_string())?;
        declared_rules[pending.index].custom = Some(CustomInvocation {
            implementation,
            top_level_functions: loaded.functions.clone(),
            attrs: pending.attrs,
            schema: pending.schema,
            build_setting: pending.build_setting,
        });
    }
    let mut rules = validate_rules(declared_rules).map_err(|e| format!("{filename}: {e}"))?;
    let default_visibility = declarations.default_visibility.into_inner();
    for label in declarations.defaulted_rules.into_inner() {
        if let Some(rule) = rules.get_mut(&label) {
            rule.visibility = default_visibility.clone();
        }
    }
    let exports = declarations.exports.into_inner();
    for label in exports.keys() {
        if rules.contains_key(label) || rules.values().any(|rule| rule.outs.contains(&label.name)) {
            return Err(format!(
                "exported file {label} collides with a rule or generated output"
            ));
        }
    }
    Ok(Package {
        rules,
        exports,
        default_visibility,
        macro_names: declarations.macro_names.into_inner(),
    })
}

#[cfg(test)]
fn evaluate_source(
    filename: &str,
    package: &str,
    source: String,
) -> Result<BTreeMap<Label, Rule>, String> {
    evaluate_at(
        Arc::new(RepositorySet::new(PathBuf::from("."))),
        filename,
        &PackageId {
            repository: String::new(),
            package: package.to_owned(),
        },
        source,
    )
    .map(|package| package.rules)
}

fn validate_rules(declarations: Vec<Rule>) -> Result<BTreeMap<Label, Rule>, String> {
    let mut rules = BTreeMap::new();
    for rule in declarations {
        if rules.contains_key(&rule.label) {
            return Err(format!("duplicate rule name: {}", rule.label));
        }
        rules.insert(rule.label.clone(), rule);
    }
    let mut outputs = BTreeMap::new();
    for rule in rules.values() {
        for out in &rule.outs {
            let output = local_label(out, &rule.label.package_id(), "output path")
                .map_err(|e| e.to_string())?;
            if rules.contains_key(&output) {
                return Err(format!("output {output} collides with a rule name"));
            }
            if let Some(previous) = outputs.insert(out.clone(), &rule.label) {
                return Err(format!(
                    "duplicate output {output}, declared by {previous} and {}",
                    rule.label
                ));
            }
        }
    }
    for (out, owner) in &outputs {
        for (index, _) in out.match_indices('/') {
            if let Some(other) = outputs.get(&out[..index]) {
                return Err(format!(
                    "output path conflict: {out:?} from {owner} is nested under output {:?} from {other}",
                    &out[..index]
                ));
            }
        }
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sets_in_build_and_loaded_bzl_keep_order_aliases_and_sequence_boundaries() {
        let package = custom_fixture(
            "Set = set\nBASE = Set(['b.txt', 'a.txt', 'b.txt'])\ndef files():\n    result = set(BASE)\n    alias = result\n    result |= set(['c.txt'])\n    (list(alias) == ['b.txt', 'a.txt', 'c.txt'] and list(BASE) == ['b.txt', 'a.txt']) or fail('aliases')\n    return list(result)\n",
            "load(':defs.bzl', 'Set', 'BASE', 'files')\n(Set == set and type(Set) == 'builtin_function_or_method') or fail('constructor')\nfilegroup(name='direct', srcs=list(set(['b.txt', 'a.txt', 'b.txt'])))\nfilegroup(name='loaded', srcs=files())",
        ).unwrap();
        assert_eq!(
            package[&Label::parse(":direct", "pkg").unwrap()].srcs,
            ["b.txt", "a.txt"]
        );
        assert_eq!(
            package[&Label::parse(":loaded", "pkg").unwrap()].srcs,
            ["b.txt", "a.txt", "c.txt"]
        );
        rejected(
            "filegroup(name='x', srcs=set(['a.txt']))",
            "list of strings",
        );
        // Enabling sets must not expose the unrelated .bzl extensions in BUILD.
        rejected("x = struct()", "struct");
        rejected("print('x')", "print");
        let error = custom_fixture(
            "def impl(ctx):\n    pass\nr = rule(implementation=impl, attrs={'dep': attr.label(providers=[set])})",
            "load(':defs.bzl', 'r')",
        ).unwrap_err();
        assert!(error.contains("Provider constructors"), "{error}");
    }

    #[test]
    fn sets_in_both_bzl_phases_survive_transitive_load_and_frozen_callbacks() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/BUILD"), "").unwrap();
        std::fs::write(
            root.0.join("pkg/base.bzl"),
            "Set = set\nBASE = Set(['b', 'a', 'b'])\n",
        )
        .unwrap();
        std::fs::write(root.0.join("pkg/defs.bzl"), r#"
load(':base.bzl', 'Set', 'BASE')
Alias = Set
(Alias == set and repr(set) == '<built-in function set>' and dir(set) == [] and not hasattr(set, 'type')) or fail('constructor')
not hasattr(native, 'set') or fail('native.set')
def copy():
    result = Alias(BASE)
    result.add('c')
    return list(result), list(BASE), Alias == set
def mutate():
    BASE.add('c')
"#).unwrap();
        for globals in [bzl_globals(), module_bzl_globals()] {
            let ast = AstModule::parse(
                "pkg/BUILD",
                "load(':defs.bzl', 'copy', 'mutate', 'Alias')".into(),
                &Dialect::Standard,
            )
            .unwrap();
            let loaded = crate::loading::load_modules(&root.0, "pkg", &ast, &globals).unwrap();
            let defs = &loaded[":defs.bzl"];
            let copy = defs.get("copy").unwrap();
            let mutate = defs.get("mutate").unwrap();
            let alias = defs.get("Alias").unwrap();
            drop(loaded);
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                let result = eval
                    .eval_function(module.heap().access_owned_frozen_value(&copy), &[], &[])
                    .unwrap();
                assert_eq!(
                    result.to_repr(),
                    "([\"b\", \"a\", \"c\"], [\"b\", \"a\"], True)"
                );
                assert!(
                    eval.eval_function(module.heap().access_owned_frozen_value(&mutate), &[], &[])
                        .is_err()
                );
                module.set("Alias", module.heap().access_owned_frozen_value(&alias));
                for source in ["{Alias: 1}", "set([Alias])", "Alias.type", "Alias[int]"] {
                    let ast =
                        AstModule::parse("use.bzl", source.into(), &Dialect::Standard).unwrap();
                    assert!(eval.eval_module(ast, &globals).is_err(), "{source}");
                }
            });
        }
    }

    #[test]
    fn run_environment_available_in_both_bzl_modes() {
        for globals in [super::bzl_globals(), super::module_bzl_globals()] {
            starlark::environment::Module::with_temp_heap(|module| {
                let ast = starlark::syntax::AstModule::parse(
                    "defs.bzl",
                    "R = RunEnvironmentInfo\nx = R({'x': 'v'}, ('HOME',))\n(type(R) == 'Provider' and x.environment == {'x': 'v'}) or fail('metadata')".into(),
                    &starlark::syntax::Dialect::Standard,
                ).unwrap();
                starlark::eval::Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap();
            });
        }
        rejected("x = RunEnvironmentInfo()", "RunEnvironmentInfo");
        assert!(
            crate::bzlmod::evaluate_source("MODULE.bazel", "x = RunEnvironmentInfo()".into())
                .unwrap_err()
                .contains("RunEnvironmentInfo")
        );
        custom_fixture("R = RunEnvironmentInfo\ndef check():\n    (R({'x': 'v'}).environment == {'x': 'v'}) or fail('BUILD callback')\n", "load(':defs.bzl', 'R', 'check')\nx = R({}, ['HOME'])\ncheck()\n(x.inherited_environment == ['HOME']) or fail('loaded alias')").unwrap();
        let error = custom_fixture(
            "m = macro(implementation=RunEnvironmentInfo)",
            "load(':defs.bzl', 'm')",
        )
        .unwrap_err();
        assert!(error.contains("Starlark function"), "{error}");
        for key in [
            "RunEnvironmentInfo()",
            "'RunEnvironmentInfo'",
            "'builtin:RunEnvironmentInfo'",
        ] {
            let error = custom_fixture(&format!("def impl(ctx):\n    pass\nr = rule(implementation=impl, attrs={{'dep': attr.label(providers=[{key}])}})"), "load(':defs.bzl', 'r')").unwrap_err();
            assert!(error.contains("Provider constructors"), "{error}");
        }
    }

    #[test]
    fn run_environment_frozen_repository_and_extension_callbacks_keep_bzl_globals() {
        let frozen = Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                label: Label::parse("//:defs.bzl", "").unwrap(),
                repositories: Arc::new(RepositorySet::new(".".into())),
            };
            let ast = AstModule::parse(
                "defs.bzl",
                r#"
def impl(ctx):
    return RunEnvironmentInfo({"x": "v"}, ["HOME"])
e = module_extension(implementation=impl)
r = repository_rule(implementation=impl)
# Callable-native validation accepts this key without pretending it is a Def.
native_callable = module_extension(implementation=RunEnvironmentInfo)
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &module_bzl_globals()).unwrap();
            }
            module.freeze().unwrap()
        });
        let extension = crate::module_extension::unpack(&frozen.get("e").unwrap()).unwrap();
        let repository = crate::repository_rule::unpack(&frozen.get("r").unwrap()).unwrap();
        let native =
            crate::module_extension::unpack(&frozen.get("native_callable").unwrap()).unwrap();
        assert!(crate::run_environment_info::is_constructor(
            native.implementation.value()
        ));
        assert!(native.implementation.value().parameters_spec().is_none());
        drop(frozen);
        for implementation in [extension.implementation, repository.implementation] {
            Module::with_temp_heap(|module| {
                let callback = module.heap().access_owned_frozen_value(&implementation);
                let result = Evaluator::new(&module)
                    .eval_function(callback, &[Value::new_none()], &[])
                    .unwrap();
                assert!(crate::run_environment_info::is_instance(result));
                assert_eq!(
                    result.to_repr(),
                    "struct(environment = {\"x\": \"v\"}, inherited_environment = [\"HOME\"])"
                );
            });
        }
    }

    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn eval(source: &str) -> Result<BTreeMap<Label, Rule>, String> {
        evaluate_source("pkg/BUILD", "pkg", source.to_owned())
    }

    fn rejected(source: &str, expected: &str) {
        let error = eval(source).unwrap_err();
        assert!(error.contains(expected), "expected {expected:?} in {error}");
    }

    #[test]
    fn evaluates_literal_rules_and_defaults() {
        let rules = eval(
            r#"
filegroup(name = "empty")
filegroup(name = "inputs", srcs = ["input.txt", "//other:files"])
genrule(name = "make", srcs = [":inputs"], outs = ["dir/out"], cmd = "cat $(SRCS) > $@")
genrule(name = "constant", outs = ["constant.txt"], cmd = "echo hi > $@")
"#,
        )
        .unwrap();
        assert_eq!(rules.len(), 4);
        let empty = &rules[&Label::parse(":empty", "pkg").unwrap()];
        assert!(matches!(empty.kind, RuleKind::Filegroup));
        assert!(empty.srcs.is_empty() && empty.outs.is_empty() && empty.cmd.is_none());
        let generated = &rules[&Label::parse(":make", "pkg").unwrap()];
        assert!(matches!(generated.kind, RuleKind::Genrule));
        assert_eq!(generated.srcs, [":inputs"]);
        assert_eq!(generated.outs, ["dir/out"]);
        assert_eq!(generated.cmd.as_deref(), Some("cat $(SRCS) > $@"));
        assert!(
            rules[&Label::parse(":constant", "pkg").unwrap()]
                .srcs
                .is_empty()
        );
    }

    #[test]
    fn preserves_declarations_from_comprehensions() {
        let rules = eval(
            r#"
unused = [filegroup(name = "input_" + n, srcs = [n + ".txt"]) for n in ["a", "b", "c"]]
unused = [genrule(name = "make_" + n, outs = [n + ".out"], cmd = "echo " + n + " > $@") for n in ["a", "b", "c"]]
"#,
        ).unwrap();
        assert_eq!(rules.len(), 6);
        assert_eq!(
            rules[&Label::parse(":input_b", "pkg").unwrap()].srcs,
            ["b.txt"]
        );
    }

    #[test]
    fn attributes_are_strictly_checked() {
        for source in [
            "filegroup(name='x', testonly=True)",
            "filegroup(name='x', **{'tags': []})",
            "genrule(name='x', outs=['y'], cmd='true', tools=[])",
            "genrule(name='x', outs=['y'], cmd='true', executable=True)",
            "genrule(name='x', cmd='true')",
            "genrule(name='x', outs=['y'])",
            "filegroup('x')",
            "filegroup(name=42)",
            "genrule(name='x', outs=['y'], cmd=42)",
        ] {
            assert!(eval(source).is_err(), "accepted {source}");
        }
        for value in ["None", "'x'", "('x',)", "[1]", "{'x': 1}"] {
            rejected(
                &format!("filegroup(name='x', srcs={value})"),
                "list of strings",
            );
            rejected(
                &format!("genrule(name='x', outs={value}, cmd='true')"),
                "list of strings",
            );
        }
        rejected(
            "genrule(name='x', outs=[], cmd='true')",
            "outs must not be empty",
        );
    }

    #[test]
    fn rejects_unsafe_names_and_paths() {
        for name in [
            "", "..", "../x", "/x", "x/", "x//y", "x/./y", "x/../y", "//else:x", ":x", "a\\b",
        ] {
            rejected(&format!("filegroup(name={name:?})"), "rule name");
            rejected(
                &format!("genrule(name='ok', outs=[{name:?}], cmd='true')"),
                "output path",
            );
        }
        rejected(
            "filegroup(name='x', srcs=['../escape'])",
            "invalid srcs entry",
        );
    }

    #[test]
    fn duplicate_and_conflicting_declarations_fail() {
        rejected(
            "filegroup(name='x')\nfilegroup(name='x')",
            "duplicate rule name",
        );
        rejected(
            "genrule(name='x', outs=['y', 'y'], cmd='true')",
            "duplicate output",
        );
        rejected(
            "genrule(name='x', outs=['z'], cmd='true')\ngenrule(name='y', outs=['z'], cmd='true')",
            "duplicate output",
        );
        rejected(
            "genrule(name='x', outs=['x'], cmd='true')",
            "collides with a rule name",
        );
        for source in [
            "filegroup(name='out')\ngenrule(name='x', outs=['out'], cmd='true')",
            "genrule(name='x', outs=['out'], cmd='true')\nfilegroup(name='out')",
        ] {
            rejected(source, "collides with a rule name");
        }
        rejected(
            "genrule(name='x', outs=['a', 'a/b'], cmd='true')",
            "output path conflict",
        );
    }

    #[test]
    fn loading_and_unsupported_bazel_features_fail() {
        rejected(
            "load('@repo//:defs.bzl', 'my_rule')\nmy_rule(name='x')",
            "external repository",
        );
        for source in [
            "filegroup(name='x', srcs=select({'//conditions:default': []}))",
            "native.filegroup(name='x')",
            "cc_binary(name='x')",
        ] {
            rejected(source, "unsupported");
        }
        assert!(eval("filegroup(name=").is_err());
    }

    #[test]
    fn uses_build_not_bzl_syntax() {
        for source in [
            "def declare():\n    filegroup(name='x')\ndeclare()",
            "f = lambda: 1",
            "for n in ['x']:\n    filegroup(name=n)",
            "if True:\n    filegroup(name='x')",
        ] {
            assert!(eval(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn supports_public_private_and_package_visibility() {
        for (attribute, expected) in [
            ("", false),
            (", visibility=['//visibility:private']", false),
            (", visibility=['//visibility:public']", true),
        ] {
            for declaration in [
                format!("filegroup(name='x'{attribute})"),
                format!("genrule(name='x', outs=['y'], cmd='true'{attribute})"),
            ] {
                let rules = eval(&declaration).unwrap();
                assert_eq!(
                    rules[&Label::parse(":x", "pkg").unwrap()].visibility == Visibility::Public,
                    expected
                );
            }
        }
        for value in [
            "['//visibility:public', '//visibility:private']",
            "['//visibility:public', '//visibility:public']",
            "'//visibility:public'",
            "[1]",
        ] {
            rejected(
                &format!("filegroup(name='x', visibility={value})"),
                "visibility",
            );
            rejected(
                &format!("genrule(name='x', outs=['y'], cmd='true', visibility={value})"),
                "visibility",
            );
        }
    }

    #[test]
    fn evaluates_docker_config_smoke_target() {
        let rules = evaluate_source(
            "src/docker/BUILD.bazel",
            "src/docker",
            "filegroup(name='config', srcs=['config.json'], visibility=['//visibility:public'])"
                .to_owned(),
        )
        .unwrap();
        let rule = &rules[&Label::parse("//src/docker:config", "").unwrap()];
        assert_eq!(rule.visibility, Visibility::Public);
        assert_eq!(rule.srcs, ["config.json"]);
    }

    #[test]
    fn records_owned_attribute_snapshots() {
        let rules = eval("s = ['a']\nfilegroup(name='x', srcs=s)\ns.append('b')").unwrap();
        assert_eq!(rules[&Label::parse(":x", "pkg").unwrap()].srcs, ["a"]);
    }

    #[test]
    fn package_group_declarations_snapshot_typed_data_and_intrinsic_public() {
        let rules = eval("p = ['//allowed']\ni = [':child']\npackage_group(name='g', packages=p, includes=i)\np.append('//late')\ni.append(':late')\npackage_group(name='empty')\npackage(default_visibility=['//visibility:private'])").unwrap();
        let g = &rules[&Label::parse(":g", "pkg").unwrap()];
        let RuleKind::PackageGroup(definition) = &g.kind else {
            panic!("not group")
        };
        assert_eq!(
            definition.includes,
            [Label::parse(":child", "pkg").unwrap()]
        );
        assert!(
            definition
                .own_specs
                .contains(&Label::parse("//allowed:x", "").unwrap().package_id())
        );
        assert!(
            !definition
                .own_specs
                .contains(&Label::parse("//late:x", "").unwrap().package_id())
        );
        for rule in rules.values() {
            assert_eq!(rule.visibility, Visibility::Public);
            assert!(
                rule.srcs.is_empty()
                    && rule.outs.is_empty()
                    && rule.cmd.is_none()
                    && rule.custom.is_none()
            );
        }
    }

    #[test]
    fn package_group_strict_signature_collisions_and_cumulative_budget() {
        for call in [
            "package_group()",
            "package_group('g')",
            "package_group(name=1)",
            "package_group(name='g', packages=None)",
            "package_group(name='g', packages='//p')",
            "package_group(name='g', packages=[1])",
            "package_group(name='g', includes=None)",
            "package_group(name='g', includes=':g')",
            "package_group(name='g', includes=[1])",
            "package_group(name='g', visibility=[])",
            "package_group(name='g', licenses=[])",
            "package_group(name='g', tags=[])",
        ] {
            assert!(eval(call).is_err(), "accepted {call}");
        }
        for declaration in [
            "filegroup(name='g')",
            "package_group(name='g')",
            "genrule(name='r', outs=['g'], cmd='true')",
            "exports_files(['g'])",
        ] {
            for source in [
                format!("{declaration}\npackage_group(name='g')"),
                format!("package_group(name='g')\n{declaration}"),
            ] {
                let error = eval(&source).unwrap_err();
                assert!(
                    error.contains("collides") || error.contains("duplicate"),
                    "{source}: {error}"
                );
            }
        }
        for source in [
            "package_group(name='g', packages=['private']*100001)",
            "package_group(name='g', includes=[':g']*100001)",
            "package_group(name='a', packages=['private']*60000)\npackage_group(name='b', packages=['private']*60000)",
        ] {
            let error = eval(source).unwrap_err();
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
    }

    #[test]
    fn package_group_bzl_globals_phase_gate_and_lexical_label_distinction() {
        for globals in [bzl_globals(), module_bzl_globals()] {
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(AstModule::parse("defs.bzl", "type(PackageSpecificationInfo) == 'Provider' or fail('key')\nKey = PackageSpecificationInfo".into(), &Dialect::Standard).unwrap(), &globals).unwrap();
                let error = eval
                    .eval_module(
                        AstModule::parse(
                            "defs.bzl",
                            "native.package_group(name='g')".into(),
                            &Dialect::Standard,
                        )
                        .unwrap(),
                        &globals,
                    )
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("outside package evaluation"), "{error}");
            });
        }
        rejected("x = PackageSpecificationInfo", "PackageSpecificationInfo");
        let root = TempRoot::new();
        std::fs::create_dir_all(root.0.join("defs")).unwrap();
        std::fs::write(root.0.join("defs/BUILD"), "").unwrap();
        std::fs::write(root.0.join("defs/groups.bzl"), "def declare():\n    native.package_group(name='g', packages=('//allowed',), includes=(':child', Label(':child')))\n").unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "load('//defs:groups.bzl','declare')\ndeclare()",
        )
        .unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let RuleKind::PackageGroup(definition) = &package[&Label::parse(":g", "pkg").unwrap()].kind
        else {
            panic!("not group")
        };
        assert_eq!(
            definition.includes,
            [
                Label::parse("//pkg:child", "").unwrap(),
                Label::parse("//defs:child", "").unwrap()
            ]
        );
    }

    #[test]
    fn package_group_macro_public_and_visibility_group_snapshots() {
        let package = custom_fixture("def impl(name, visibility):\n    native.package_group(name=name, packages=['private'])\n    native.filegroup(name=name+'_rule', visibility=visibility)\nm = macro(implementation=impl)", "load(':defs.bzl','m')\nm(name='demo', visibility=['//groups:g', '//groups:g'])\nm(name='public', visibility=['//missing:g', '//visibility:public'])\npackage(default_visibility=['//groups:late'])").unwrap();
        for name in ["demo", "public"] {
            let group = &package[&Label::parse(&format!(":{name}"), "pkg").unwrap()];
            assert!(matches!(group.kind, RuleKind::PackageGroup(_)));
            assert_eq!(group.visibility, Visibility::Public);
            assert_eq!(group.macro_owner.as_ref().unwrap().name, name);
        }
        let declared = package[&Label::parse(":demo_rule", "pkg").unwrap()]
            .visibility
            .declared_labels();
        assert_eq!(
            declared,
            [
                Label::parse("//groups:g", "").unwrap(),
                Label::parse("//groups:g", "").unwrap(),
                Label::parse("//pkg:__pkg__", "").unwrap()
            ]
        );
        assert_eq!(
            package[&Label::parse(":public_rule", "pkg").unwrap()].visibility,
            Visibility::Public
        );
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bazel-rs-package-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("pkg")).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_build_files_and_prefers_build_bazel() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/BUILD"), "filegroup(name='fallback')").unwrap();
        assert!(
            evaluate(&root.0, "pkg")
                .unwrap()
                .contains_key(&Label::parse(":fallback", "pkg").unwrap())
        );
        std::fs::write(
            root.0.join("pkg/BUILD.bazel"),
            "filegroup(name='preferred')",
        )
        .unwrap();
        let rules = evaluate(&root.0, "pkg").unwrap();
        assert_eq!(rules.len(), 1);
        assert!(rules.contains_key(&Label::parse(":preferred", "pkg").unwrap()));
    }

    #[test]
    fn local_bzl_loading_does_not_resolve_unused_invalid_module() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("MODULE.bazel"), "invalid MODULE !!!").unwrap();
        std::fs::write(
            root.0.join("pkg/defs.bzl"),
            "def macro(name):\n    native.filegroup(name=name)\n",
        )
        .unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "load(':defs.bzl', 'macro')\nmacro(name='local')\n",
        )
        .unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        assert!(package.contains_key(&Label::parse("//pkg:local", "").unwrap()));
        assert!(!root.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn rejects_invalid_packages_and_missing_build_files() {
        let root = TempRoot::new();
        for package in [
            "../escape",
            "/absolute",
            "pkg/../other",
            "pkg//sub",
            "pkg/.",
        ] {
            assert!(evaluate(&root.0, package).unwrap_err().contains("package"));
        }
        assert!(evaluate(&root.0, "missing").is_err());
    }

    #[test]
    fn root_package_is_supported() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("BUILD.bazel"), "filegroup(name='root')").unwrap();
        assert!(
            evaluate(&root.0, "")
                .unwrap()
                .contains_key(&Label::parse("//:root", "").unwrap())
        );
    }

    fn custom_fixture(defs: &str, build: &str) -> Result<Package, String> {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/defs.bzl"), defs).unwrap();
        std::fs::write(root.0.join("pkg/BUILD.bazel"), build).unwrap();
        evaluate(&root.0, "pkg")
    }

    #[test]
    fn symbolic_macro_expands_native_declarations() {
        let package = custom_fixture(
            "def impl(name, visibility):\n    native.genrule(name=name, outs=[name + '.out'], cmd='echo macro-ok > $@', visibility=visibility)\nm = macro(implementation=impl)\n",
            "load(':defs.bzl', 'm')\nm(name='demo')",
        ).unwrap();
        let rule = &package[&Label::parse(":demo", "pkg").unwrap()];
        assert_eq!(rule.outs, ["demo.out"]);
        assert_eq!(rule.cmd.as_deref(), Some("echo macro-ok > $@"));
        assert_eq!(rule.macro_owner.as_ref().unwrap().name, "demo");
        assert_eq!(package.macro_names, BTreeSet::from(["demo".into()]));
    }

    const SYMBOLIC_ZERO: &str =
        "def impl(name, visibility):\n    pass\nm = macro(implementation=impl)\n";

    fn symbolic_error(defs: &str, calls: &str, expected: &str) {
        let error = custom_fixture(defs, &format!("load(':defs.bzl', 'm')\n{calls}")).unwrap_err();
        assert!(
            error.to_lowercase().contains(&expected.to_lowercase()),
            "expected {expected:?}: {error}"
        );
    }

    #[test]
    fn symbolic_macro_invocation_validation_and_return_contract() {
        for (call, expected) in [
            ("m()", "mandatory"),
            ("m('x')", "positional"),
            ("m(name=None)", "string"),
            ("m(name=1)", "string"),
            ("m(name='')", "macro name"),
            ("m(name='../bad')", "macro name"),
            ("m(name='x', unknown=None)", "unknown"),
            (
                "m(name='x', visibility='//visibility:public')",
                "visibility",
            ),
            ("m(name='x', visibility=[1])", "visibility"),
        ] {
            symbolic_error(SYMBOLIC_ZERO, call, expected);
        }
        symbolic_error(
            "def impl(name, visibility):\n    return 1\nm = macro(implementation=impl)",
            "m(name='x')",
            "must return None",
        );
        symbolic_error(
            "def impl():\n    pass\nm = macro(implementation=impl)",
            "m(name='x')",
            "parameter",
        );
        symbolic_error(
            "def impl(name, visibility):\n    m(name=name + '_nested')\nm = macro(implementation=impl)",
            "m(name='x')",
            "nested symbolic",
        );
        symbolic_error(
            "def impl(name, visibility):\n    pass\nbox = [macro(implementation=impl)]\ndef m():\n    box[0](name='x')",
            "m()",
            "exported",
        );
        symbolic_error(
            "def impl(name, visibility):\n    pass\ndef m():\n    return macro(implementation=impl)",
            "m()",
            ".bzl initialization",
        );
        rejected("macro(implementation=None)", "macro");
        let package = custom_fixture(
            SYMBOLIC_ZERO,
            "load(':defs.bzl', 'm')\nx = m(name='dir/name')\nx == None or fail('return')",
        )
        .unwrap();
        assert!(package.macro_names.contains("dir/name"));
        assert!(package.rules.is_empty());
    }

    #[test]
    fn symbolic_macro_exact_name_reservations_all_declaration_orders() {
        let defs = format!(
            "{SYMBOLIC_ZERO}\ndef rule_impl(ctx):\n    return []\nr = rule(implementation=rule_impl, attrs={{'out':attr.output()}})\ndef custom(name):\n    r(name='custom', out=name)\n"
        );
        for declaration in [
            "filegroup(name='demo')",
            "package_group(name='demo')",
            "genrule(name='other', outs=['demo'], cmd='true')",
            "exports_files(['demo'])",
            "custom(name='demo')",
        ] {
            for calls in [
                format!("{declaration}\nm(name='demo')"),
                format!("m(name='demo')\n{declaration}"),
            ] {
                let error =
                    custom_fixture(&defs, &format!("load(':defs.bzl', 'm', 'custom')\n{calls}"))
                        .unwrap_err();
                assert!(error.contains("conflicts"), "{calls}: {error}");
            }
        }
        symbolic_error(SYMBOLIC_ZERO, "m(name='demo')\nm(name='demo')", "conflicts");
        let package = custom_fixture(SYMBOLIC_ZERO, "load(':defs.bzl', 'm')\nm(name='demo')\nm(name='demo_child')\nfilegroup(name='demo_other')").unwrap();
        assert_eq!(package.macro_names.len(), 2);
        assert!(
            package
                .rules
                .values()
                .all(|rule| rule.macro_owner.is_none())
        );
        for declaration in [
            "native.filegroup(name='demo')",
            "native.package_group(name='demo')",
            "native.genrule(name='other', outs=['demo'], cmd='true')",
            "r(name='other', out='demo')",
        ] {
            let defs = format!(
                "{SYMBOLIC_ZERO}\nzero = m\ndef rule_impl(ctx):\n    return []\nr = rule(implementation=rule_impl, attrs={{'out':attr.output()}})\ndef emit(name, visibility):\n    {declaration}\nx = macro(implementation=emit)"
            );
            let error = custom_fixture(
                &defs,
                "load(':defs.bzl', 'zero', 'x')\nzero(name='demo')\nx(name='othermacro')",
            )
            .unwrap_err();
            assert!(error.contains("conflicts"), "{declaration}: {error}");
        }
        // Owning a reservation permits both an own-name rule and an own-name output.
        let package = custom_fixture("def impl(name, visibility):\n    native.genrule(name=name+'_rule', outs=[name], cmd='true')\nm=macro(implementation=impl)", "load(':defs.bzl', 'm')\nm(name='demo')").unwrap();
        assert_eq!(package.rules.len(), 1);
    }

    #[test]
    fn symbolic_macro_closures_custom_rules_and_late_defaults() {
        let root = TempRoot::new();
        std::fs::create_dir_all(root.0.join("defs")).unwrap();
        std::fs::write(root.0.join("defs/BUILD"), "").unwrap();
        std::fs::write(
            root.0.join("defs/macros.bzl"),
            r#"
def rule_impl(ctx):
    return []
r = rule(implementation=rule_impl)
def helper(name):
    native.filegroup(name=name+"_private")
    r(name=name+"_custom")
def factory():
    captured = "alive"
    def impl(name, visibility):
        captured == "alive" or fail("lost closure")
        native.package_name() == "pkg" or fail("wrong package")
        helper(name)
        native.filegroup(name=name, visibility=visibility)
    return macro(implementation=impl)
m = factory()
"#,
        )
        .unwrap();
        std::fs::write(root.0.join("pkg/BUILD"), "load('//defs:macros.bzl', 'm')\nfilegroup(name='ordinary')\nm(name='demo')\npackage(default_visibility=['//visibility:public'])\nm(name='public')\nm(name='none', visibility=None)\nm(name='empty', visibility=[])").unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let owner = PackageId {
            repository: "".into(),
            package: "defs".into(),
        };
        let caller = PackageId {
            repository: "".into(),
            package: "pkg".into(),
        };
        for rule in package.rules.values() {
            if rule.label.name == "ordinary" {
                assert!(rule.macro_owner.is_none());
                assert_eq!(rule.visibility, Visibility::Public);
                continue;
            }
            let frame = rule.macro_owner.as_ref().unwrap();
            assert_eq!(frame.definition_package, owner);
            assert!(rule.label.name.starts_with(&frame.name));
            assert!(rule.visibility.allows_in(&owner, &owner));
            if rule.label.name.ends_with("_private") || rule.label.name.ends_with("_custom") {
                assert!(!rule.visibility.allows_in(&owner, &caller));
            } else if matches!(rule.label.name.as_str(), "public" | "none") {
                assert_eq!(rule.visibility, Visibility::Public);
            } else {
                assert_ne!(rule.visibility, Visibility::Public);
                assert!(rule.visibility.allows_in(&owner, &caller));
            }
        }
        assert!(
            package[&Label::parse(":demo_custom", "pkg").unwrap()]
                .custom
                .is_some()
        );
    }

    #[test]
    fn symbolic_macro_visibility_frozen_sorted_typed_and_input_not_frozen() {
        let defs = r#"
def impl(name, visibility):
    type(visibility) == "list" or fail("not list")
    [type(v) for v in visibility] == ["Label"] * len(visibility) or fail("not labels")
    expected = {
        "duplicates": ["@@//pkg:__pkg__", "@@//pkg:__pkg__", "@@//z:__pkg__", "@@//z:__pkg__"],
        "public": ["@@//visibility:public"],
        "recursive": ["@@//:__subpackages__", "@@//pkg:__pkg__"],
        "private": ["@@//pkg:__pkg__"],
    }
    [str(v) for v in visibility] == expected[name] or fail(str(visibility))
m = macro(implementation=impl)
def legacy():
    data = [Label("//z:__pkg__"), "//z:__pkg__", "//visibility:private", "//pkg:__pkg__", "//pkg:__pkg__"]
    m(name="duplicates", visibility=data)
    data.append("still mutable")
    len(data) == 6 or fail("caller list changed")
    m(name="public", visibility=("//z:__pkg__", "//visibility:public", "//visibility:private"))
    m(name="recursive", visibility=["//:__subpackages__"])
    m(name="private", visibility=[])
"#;
        custom_fixture(defs, "load(':defs.bzl', 'legacy')\nlegacy()").unwrap();
        symbolic_error(
            "def impl(name, visibility):\n    visibility.append(Label('//other:__pkg__'))\nm=macro(implementation=impl)",
            "m(name='x')",
            "immutable",
        );
    }

    #[test]
    fn symbolic_macro_restricted_helpers_and_legacy_regression() {
        for (statement, expected) in [
            (
                "native.glob(['*'], allow_empty=True)",
                "glob() is not allowed",
            ),
            (
                "native.package(default_visibility=['//visibility:public'])",
                "package() is not allowed",
            ),
            (
                "native.exports_files(['source'])",
                "exports_files() inside symbolic macros is unsupported",
            ),
        ] {
            let defs = format!(
                "def helper():\n    {statement}\ndef impl(name, visibility):\n    helper()\nm=macro(implementation=impl)\ndef legacy():\n    helper()\n    native.filegroup(name='ordinary')"
            );
            symbolic_error(&defs, "m(name='x')", expected);
            let package = custom_fixture(&defs, "load(':defs.bzl', 'legacy')\nlegacy()").unwrap();
            assert!(
                package
                    .rules
                    .values()
                    .all(|rule| rule.macro_owner.is_none())
            );
        }
    }

    #[test]
    fn symbolic_macro_ordinary_visibility_accepts_labels_and_tuples_only_here() {
        let package = custom_fixture("def m():\n    native.filegroup(name='x', visibility=(Label('//other:__pkg__'), ':__pkg__'))", "load(':defs.bzl', 'm')\nm()").unwrap();
        let rule = &package[&Label::parse(":x", "pkg").unwrap()];
        assert!(rule.visibility.allows("pkg", "other"));
        rejected("filegroup(name='x', srcs=('a',))", "list of strings");
        rejected(
            "filegroup(name='x', visibility=('//visibility:public', '//visibility:private'))",
            "cannot be combined",
        );
    }

    #[test]
    fn symbolic_macro_callback_error_restores_frame_and_guards_before_mutation() {
        for (body, call, reserved) in [
            ("fail('callback failure')", "m(name='x')", true),
            ("return 7", "m(name='x')", true),
            (
                "native.package(default_visibility=['//visibility:public'])",
                "m(name='x')",
                true,
            ),
            ("native.exports_files(['source'])", "m(name='x')", true),
            ("native.glob(['../invalid'])", "m(name='x')", true),
            ("fail('must not run')", "m(name='x', unknown=None)", false),
            ("fail('must not run')", "m(name='x', visibility=[1])", false),
            ("fail('must not run')", "m(name='../bad')", false),
        ] {
            let repositories = Arc::new(RepositorySet::new(PathBuf::from(".")));
            let extension = crate::loading::ExtensionContext {
                label: Label::parse("//defs:macro.bzl", "").unwrap(),
                repositories: repositories.clone(),
            };
            let frozen = Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&extension);
                eval.eval_module(
                    AstModule::parse(
                        "//defs:macro.bzl",
                        format!(
                            "def impl(name, visibility):\n    {body}\nm=macro(implementation=impl)"
                        ),
                        &Dialect::Standard,
                    )
                    .unwrap(),
                    &bzl_globals(),
                )
                .unwrap();
                drop(eval);
                module.freeze().unwrap()
            });
            let context = Declarations {
                package: "pkg".into(),
                repository: "".into(),
                repositories,
                root: PathBuf::from("/nonexistent-symbolic-macro-test-root"),
                rules: RefCell::new(Vec::new()),
                exports: RefCell::new(BTreeMap::new()),
                default_visibility: RefCell::new(Visibility::Private),
                defaulted_rules: RefCell::new(BTreeSet::new()),
                package_called: Cell::new(false),
                custom: RefCell::new(Vec::new()),
                active_macro: RefCell::new(None),
                macro_names: RefCell::new(BTreeSet::new()),
                declared_names: RefCell::new(BTreeSet::new()),
                group_budget: RefCell::new(HostBudget::default()),
            };
            Module::with_temp_heap(|module| {
                module.set(
                    "m",
                    module
                        .heap()
                        .access_owned_frozen_value(&frozen.get("m").unwrap()),
                );
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let globals = GlobalsBuilder::standard()
                    .with(rule_globals)
                    .with(package_globals)
                    .build();
                let error = eval
                    .eval_module(
                        AstModule::parse("pkg/BUILD", call.to_owned(), &Dialect::Standard).unwrap(),
                        &globals,
                    )
                    .unwrap_err()
                    .to_string();
                assert!(context.active_macro.borrow().is_none(), "{body}: {error}");
                assert_eq!(
                    context.macro_names.borrow().contains("x"),
                    reserved,
                    "{call}"
                );
                assert!(!context.package_called.get());
                assert_eq!(*context.default_visibility.borrow(), Visibility::Private);
                assert!(context.exports.borrow().is_empty());
                assert!(context.rules.borrow().is_empty());
                if body.contains("glob") {
                    assert!(error.contains("not allowed"), "{error}");
                }
                eval.eval_module(AstModule::parse("pkg/BUILD", "filegroup(name='ordinary')\npackage(default_visibility=['//visibility:public'])".into(), &Dialect::Standard).unwrap(), &globals).unwrap();
                assert!(context.rules.borrow()[0].macro_owner.is_none());
                assert!(context.package_called.get());
            });
        }
        let previous = MacroOwner {
            definition_package: PackageId {
                repository: "".into(),
                package: "previous".into(),
            },
            name: "previous".into(),
        };
        let slot = RefCell::new(Some(previous.clone()));
        {
            let _frame = MacroFrame::enter(
                &slot,
                MacroOwner {
                    name: "active".into(),
                    ..previous
                },
            );
            assert_eq!(slot.borrow().as_ref().unwrap().name, "active");
        }
        assert_eq!(slot.borrow().as_ref().unwrap().name, "previous");
    }

    #[test]
    fn build_setting_declarations_retain_metadata_and_typed_defaults() {
        use crate::build_settings::BuildSettingKind;
        let package = custom_fixture(
            r#"
def impl(ctx):
    fail("implementation must not run during loading")
b = rule(implementation=impl, build_setting=config.bool())
i = rule(implementation=impl, build_setting=config.int(flag=True), cfg=None)
s = rule(implementation=impl, build_setting=config.string(flag=True))
ss = rule(implementation=impl, build_setting=config.string_list(flag=True, repeatable=True))
ordinary = rule(implementation=impl, build_setting=None, cfg=None, attrs={"help": attr.string(), "build_setting_default": attr.bool()})
"#,
            r#"
load(":defs.bzl", "b", "i", "s", "ss", "ordinary")
b(name="b", build_setting_default=False)
b(name="b0", build_setting_default=0)
b(name="b1", build_setting_default=1)
b(name="bt", build_setting_default=True)
i(name="i", build_setting_default=-2147483648)
i(name="imax", build_setting_default=2147483647)
s(name="s", build_setting_default="value", help="description")
ss(name="ss", build_setting_default=["first", "first", "second"])
ordinary(name="ordinary", help="ordinary help", build_setting_default=True)
"#,
        ).unwrap();
        for (name, kind, flag, repeatable, value) in [
            (
                "b",
                BuildSettingKind::Bool,
                false,
                false,
                AttrValue::Bool(false),
            ),
            (
                "b0",
                BuildSettingKind::Bool,
                false,
                false,
                AttrValue::Bool(false),
            ),
            (
                "b1",
                BuildSettingKind::Bool,
                false,
                false,
                AttrValue::Bool(true),
            ),
            (
                "bt",
                BuildSettingKind::Bool,
                false,
                false,
                AttrValue::Bool(true),
            ),
            (
                "i",
                BuildSettingKind::Int,
                true,
                false,
                AttrValue::Int(i32::MIN),
            ),
            (
                "imax",
                BuildSettingKind::Int,
                true,
                false,
                AttrValue::Int(i32::MAX),
            ),
            (
                "s",
                BuildSettingKind::String,
                true,
                false,
                AttrValue::String("value".into()),
            ),
            (
                "ss",
                BuildSettingKind::StringList,
                true,
                true,
                AttrValue::Strings(vec!["first".into(), "first".into(), "second".into()]),
            ),
        ] {
            let custom = package[&Label::parse(&format!(":{name}"), "pkg").unwrap()]
                .custom
                .as_ref()
                .unwrap();
            assert_eq!(
                custom.build_setting,
                Some(BuildSetting {
                    kind,
                    flag,
                    repeatable
                })
            );
            assert_eq!(custom.attrs["build_setting_default"], value);
            assert!(custom.schema["build_setting_default"].mandatory);
            assert_eq!(
                custom.schema["help"].kind,
                crate::custom_rule::AttrKind::String
            );
            assert_eq!(
                custom.attrs["help"],
                AttrValue::String(if name == "s" { "description" } else { "" }.into())
            );
        }
        let ordinary = package[&Label::parse(":ordinary", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        assert_eq!(ordinary.build_setting, None);
        assert_eq!(
            ordinary.attrs["help"],
            AttrValue::String("ordinary help".into())
        );
        assert_eq!(
            ordinary.attrs["build_setting_default"],
            AttrValue::Bool(true)
        );
    }

    #[test]
    fn build_setting_defaults_enforce_mandatory_and_exact_type_contracts() {
        for (constructor, argument, expected) in [
            ("bool", "", "missing mandatory attribute"),
            ("int", "", "missing mandatory attribute"),
            ("string", "", "missing mandatory attribute"),
            ("string_list", "", "missing mandatory attribute"),
            (
                "bool",
                ", build_setting_default=None",
                "missing mandatory attribute",
            ),
            (
                "int",
                ", build_setting_default=None",
                "missing mandatory attribute",
            ),
            (
                "string",
                ", build_setting_default=None",
                "missing mandatory attribute",
            ),
            (
                "string_list",
                ", build_setting_default=None",
                "missing mandatory attribute",
            ),
            ("bool", ", build_setting_default=2", "expected a bool"),
            ("bool", ", build_setting_default=-1", "expected a bool"),
            ("bool", ", build_setting_default='true'", "expected a bool"),
            (
                "int",
                ", build_setting_default=True",
                "expected a 32-bit int",
            ),
            (
                "int",
                ", build_setting_default=2147483648",
                "expected a 32-bit int",
            ),
            (
                "int",
                ", build_setting_default=-2147483649",
                "expected a 32-bit int",
            ),
            ("string", ", build_setting_default=1", "expected a string"),
            (
                "string_list",
                ", build_setting_default=['ok', 1]",
                "expected a list of strings",
            ),
            (
                "string_list",
                ", build_setting_default=('ok', 1)",
                "expected a list of strings",
            ),
            (
                "string",
                ", build_setting_default='', help=1",
                "expected a string",
            ),
        ] {
            let defs = format!(
                "def impl(ctx):\n    return []\nr = rule(implementation=impl, build_setting=config.{constructor}())"
            );
            let build = format!("load(':defs.bzl', 'r')\nr(name='x'{argument})");
            let error = custom_fixture(&defs, &build).unwrap_err();
            assert!(error.contains(expected), "{constructor}{argument}: {error}");
        }
        for (setting, attribute) in [
            ("None", "build_setting_default"),
            ("config.bool()", "ordinary_bool"),
        ] {
            let defs = format!(
                "def impl(ctx):\n    return []\nr = rule(implementation=impl, build_setting={setting}, attrs={{'{attribute}': attr.bool()}})"
            );
            let build = format!("load(':defs.bzl', 'r')\nr(name='x', {attribute}=1)");
            let error = custom_fixture(&defs, &build).unwrap_err();
            assert!(error.contains("expected a bool"), "{error}");
        }
    }

    #[test]
    fn build_setting_list_defaults_snapshot_each_macro_invocation() {
        let package = custom_fixture(
            r#"
def impl(ctx):
    return []
r = rule(implementation=impl, build_setting=config.string_list())
def declare():
    defaults = ["before"]
    r(name="first", build_setting_default=defaults)
    defaults.append("after")
    r(name="second", build_setting_default=defaults)
    defaults.append("later")
"#,
            "load(':defs.bzl', 'declare')\ndeclare()",
        )
        .unwrap();
        for (name, expected) in [
            ("first", vec!["before".into()]),
            ("second", vec!["before".into(), "after".into()]),
        ] {
            let custom = package[&Label::parse(&format!(":{name}"), "pkg").unwrap()]
                .custom
                .as_ref()
                .unwrap();
            assert_eq!(
                custom.attrs["build_setting_default"],
                AttrValue::Strings(expected)
            );
        }
    }

    #[test]
    fn custom_declarations_snapshot_defaults_labels_and_outputs_without_running_implementation() {
        let package = custom_fixture(
            r#"
def impl(ctx):
    fail("implementation must not run during package evaluation/query")
defaults = ["initial"]
r = rule(implementation=impl, attrs={
    "s": attr.string(default="hello"),
    "ss": attr.string_list(default=defaults),
    "i": attr.int(default=12),
    "b": attr.bool(default=True),
    "dep": attr.label(default="//other:x"),
    "deps": attr.label_list(allow_files=True),
    "out": attr.output(),
    "outputs": attr.output_list(),
})
defaults.append("late")
"#,
            r#"
load(":defs.bzl", "r")
deps = [":source", "plain.txt"]
r(name="x", deps=deps, out="result", outputs=["dir/second"])
deps.append("later")
"#,
        )
        .unwrap();
        let rule = &package[&Label::parse(":x", "pkg").unwrap()];
        assert!(matches!(rule.kind, RuleKind::Custom));
        assert_eq!(rule.srcs, ["//other:x", "//pkg:source", "//pkg:plain.txt"]);
        assert_eq!(rule.outs, ["result", "dir/second"]);
        let custom = rule.custom.as_ref().unwrap();
        assert_eq!(custom.attrs["s"], AttrValue::String("hello".into()));
        assert_eq!(
            custom.attrs["ss"],
            AttrValue::Strings(vec!["initial".into()])
        );
        assert_eq!(custom.attrs["i"], AttrValue::Int(12));
        assert_eq!(custom.attrs["b"], AttrValue::Bool(true));
        assert!(custom.schema["deps"].allow_files);
    }

    #[test]
    fn custom_attributes_enforce_types_mandatory_and_named_arguments() {
        let defs = r#"
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "s": attr.string(mandatory=True),
    "ss": attr.string_list(),
    "i": attr.int(),
    "b": attr.bool(),
    "dep": attr.label(),
    "deps": attr.label_list(),
    "out": attr.output(),
    "outputs": attr.output_list(),
})
"#;
        for (call, expected) in [
            ("r(name='x')", "mandatory"),
            ("r(name='x', s=1)", "string"),
            ("r(name='x', s='', ss=(1,))", "list of strings"),
            ("r(name='x', s='', ss=[1])", "list of strings"),
            ("r(name='x', s='', i=True)", "int"),
            ("r(name='x', s='', b=1)", "bool"),
            ("r(name='x', s='', dep=1)", "string"),
            ("r(name='x', s='', deps=[1])", "list of strings"),
            ("r(name='x', s='', out='../escape')", "output path"),
            ("r(name='x', s='', outputs=[1])", "list of strings"),
            ("r(name='x', s='', unknown=1)", "unknown attribute"),
            ("r('x', s='')", "positional"),
            ("r(s='')", "name"),
            ("r(name=1, s='')", "name"),
        ] {
            let error =
                custom_fixture(defs, &format!("load(':defs.bzl', 'r')\n{call}")).unwrap_err();
            assert!(
                error.contains(expected),
                "{call}: expected {expected:?}: {error}"
            );
        }
    }

    #[test]
    fn custom_common_visibility_defaults_duplicates_and_output_collisions() {
        let defs = "def impl(ctx):\n    return []\nr = rule(implementation=impl, attrs={'out': attr.output()})";
        let package = custom_fixture(defs, "load(':defs.bzl', 'r')\nr(name='x')\nr(name='y', visibility=['//visibility:private'])\npackage(default_visibility=['//visibility:public'])").unwrap();
        assert_eq!(
            package[&Label::parse(":x", "pkg").unwrap()].visibility,
            Visibility::Public
        );
        assert_eq!(
            package[&Label::parse(":y", "pkg").unwrap()].visibility,
            Visibility::Private
        );
        for (calls, expected) in [
            ("r(name='x')\nr(name='x')", "duplicate rule"),
            ("r(name='x', out='x')", "collides"),
            (
                "r(name='x', out='z')\nr(name='y', out='z')",
                "duplicate output",
            ),
            ("r(name='x', visibility=42)", "visibility"),
        ] {
            let error =
                custom_fixture(defs, &format!("load(':defs.bzl', 'r')\n{calls}")).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
    }

    #[test]
    fn custom_owned_implementation_survives_modules_and_preserves_nested_helpers() {
        let package = custom_fixture(
            r#"
def helper(value):
    return "prefix:" + value
def factory():
    captured = ["alive"]
    def nested_helper():
        return helper(captured[0])
    def implementation(ctx):
        return nested_helper()
    return rule(implementation=implementation)
r = factory()
def macro(name):
    return r(name=name)
"#,
            "load(':defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let implementation = package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .implementation
            .clone();
        drop(package);
        Module::with_temp_heap(|module| {
            let function = module.heap().access_owned_frozen_value(&implementation);
            let mut eval = Evaluator::new(&module);
            let value = eval
                .eval_function(function, &[Value::new_none()], &[])
                .unwrap();
            assert_eq!(value.unpack_str(), Some("prefix:alive"));
        });
    }

    #[test]
    fn custom_definition_globals_are_not_build_globals() {
        rejected("x = attr.string()", "attr");
        rejected("x = rule(implementation=print)", "rule");
    }

    #[test]
    fn custom_invocations_retain_top_level_function_identities_for_analysis() {
        let package = custom_fixture(
            r#"
def _mapper(value):
    return value
def _impl(ctx):
    return _mapper
r = rule(implementation=_impl)
"#,
            "load(':defs.bzl', 'r')\nr(name='x')\nr(name='y')",
        )
        .unwrap();
        let first = package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        let second = package[&Label::parse(":y", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        assert_eq!(first.top_level_functions.len(), 2);
        assert!(
            first
                .top_level_functions
                .iter()
                .any(|f| f.value().ptr_eq(first.implementation.value()))
        );
        for (a, b) in first
            .top_level_functions
            .iter()
            .zip(&second.top_level_functions)
        {
            assert!(a.value().ptr_eq(b.value()));
        }
        let invocation = first.clone();
        drop(package);
        Module::with_temp_heap(|module| {
            let function = module
                .heap()
                .access_owned_frozen_value(&invocation.implementation);
            let mapper = Evaluator::new(&module)
                .eval_function(function, &[Value::new_none()], &[])
                .unwrap();
            assert!(
                invocation
                    .top_level_functions
                    .iter()
                    .any(|f| f.value().ptr_eq(mapper))
            );
        });
    }

    #[test]
    fn custom_relative_defaults_use_lexical_extension_but_arguments_use_build_package() {
        let root = TempRoot::new();
        for package in ["defs", "helpers"] {
            std::fs::create_dir_all(root.0.join(package)).unwrap();
            std::fs::write(root.0.join(package).join("BUILD"), "").unwrap();
        }
        std::fs::write(
            root.0.join("helpers/attrs.bzl"),
            r#"
from_top = attr.label(default=":top-helper")
label_alias = attr.label
def leaf():
    return attr.label(default=":helper")
def nested():
    return leaf()
def label_factory():
    return Label(":helper-object")
"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("defs/rules.bzl"),
            r#"
load("//helpers:attrs.bzl", "from_top", "label_alias", "nested", "label_factory")
def impl(ctx):
    return []
object_label = Label(":object")
r = rule(implementation=impl, attrs={
    "local": attr.label(default=":local"),
    "helper": nested(),
    "top": from_top,
    "alias": label_alias(default=":alias-local"),
    "object": attr.label(default=object_label),
    "helper_object": attr.label(default=label_factory()),
    "defaults": attr.label_list(default=[":local-list", object_label, "@//other:absolute"]),
    "direct": attr.label(),
    "explicit": attr.label(),
    "mixed": attr.label_list(),
})
def macro(name):
    r(name=name, direct=":caller", explicit=object_label, mixed=[":caller-list", object_label])
"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "load('//defs:rules.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let attrs = &package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .attrs;
        for (name, label) in [
            ("local", "//defs:local"),
            ("helper", "//helpers:helper"),
            ("top", "//helpers:top-helper"),
            ("alias", "//defs:alias-local"),
            ("object", "//defs:object"),
            ("helper_object", "//helpers:helper-object"),
            ("direct", "//pkg:caller"),
            ("explicit", "//defs:object"),
        ] {
            assert_eq!(attrs[name], AttrValue::Label(Some(label.into())), "{name}");
        }
        assert_eq!(
            attrs["defaults"],
            AttrValue::Labels(vec![
                "//defs:local-list".into(),
                "//defs:object".into(),
                "//other:absolute".into()
            ])
        );
        assert_eq!(
            attrs["mixed"],
            AttrValue::Labels(vec!["//pkg:caller-list".into(), "//defs:object".into()])
        );
    }

    #[test]
    fn external_defaults_and_macro_labels_are_lexical_but_strings_use_build_context() {
        let main = TempRoot::new();
        let external = TempRoot::new();
        std::fs::write(main.0.join("MODULE.bazel"), format!("module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path={:?})\n", external.0.to_string_lossy())).unwrap();
        std::fs::write(
            external.0.join("MODULE.bazel"),
            "module(name='dep',repo_name='self_dep')",
        )
        .unwrap();
        std::fs::write(
            external.0.join("pkg/BUILD"),
            "exports_files(['input'])\nfilegroup(name='native',srcs=[':input'])",
        )
        .unwrap();
        std::fs::create_dir_all(external.0.join("helpers")).unwrap();
        std::fs::write(external.0.join("helpers/BUILD"), "").unwrap();
        std::fs::write(external.0.join("helpers/attrs.bzl"), "def defaults():\n    return attr.label(default='//helpers:input')\ndef object_label():\n    return Label('@self_dep//helpers:input')\n").unwrap();
        std::fs::write(
            external.0.join("pkg/defs.bzl"),
            r#"
load("//helpers:attrs.bzl", "defaults", "object_label")
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "helper": defaults(),
    "local": attr.label(default=":input"),
    "main": attr.label(default=Label("@@//pkg:input")),
    "explicit": attr.label(),
    "object": attr.label(),
    "absolute": attr.label(),
})
def macro(name):
    r(name=name, explicit=":input", object=object_label(), absolute=Label("//pkg:input"))
"#,
        )
        .unwrap();
        std::fs::write(
            main.0.join("pkg/BUILD"),
            "load('@alias//pkg:defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let package = evaluate(&main.0, "pkg").unwrap();
        let custom = package[&Label::parse("//pkg:x", "").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        for (name, label) in [
            ("helper", "@@dep+//helpers:input"),
            ("local", "@@dep+//pkg:input"),
            ("main", "//pkg:input"),
            ("explicit", "//pkg:input"),
            ("object", "@@dep+//helpers:input"),
            ("absolute", "@@dep+//pkg:input"),
        ] {
            assert_eq!(
                custom.attrs[name],
                AttrValue::Label(Some(label.into())),
                "{name}"
            );
        }
        let repositories = Arc::new(RepositorySet::new(main.0.clone()));
        let package = evaluate_in(
            repositories,
            &PackageId {
                repository: "dep+".into(),
                package: "pkg".into(),
            },
        )
        .unwrap();
        let native = &package[&Label::parse_canonical("@@dep+//pkg:native").unwrap()];
        assert_eq!(native.srcs, [":input"]);
        assert!(
            package
                .exports
                .contains_key(&Label::parse_canonical("@@dep+//pkg:input").unwrap())
        );
    }

    #[test]
    fn custom_private_defaults_cannot_be_overridden() {
        let defs = r#"
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "_tool": attr.label(default=":tool"),
    "_flags": attr.string_list(default=["private"]),
})
"#;
        let package = custom_fixture(defs, "load(':defs.bzl', 'r')\nr(name='x')").unwrap();
        let attrs = &package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .attrs;
        assert_eq!(attrs["_tool"], AttrValue::Label(Some("//pkg:tool".into())));
        assert_eq!(attrs["_flags"], AttrValue::Strings(vec!["private".into()]));
        for argument in ["_tool=':other'", "_flags=['override']"] {
            let error = custom_fixture(
                defs,
                &format!("load(':defs.bzl', 'r')\nr(name='x', {argument})"),
            )
            .unwrap_err();
            assert!(error.contains("private attribute"), "{error}");
        }
    }

    #[test]
    fn custom_rule_definition_is_rejected_inside_build_macros() {
        let error = custom_fixture(
            r#"
def impl(ctx):
    return []
def macro(name):
    r = rule(implementation=impl)
    r(name=name)
"#,
            "load(':defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap_err();
        assert!(error.contains("extension initialization"), "{error}");
    }
}
