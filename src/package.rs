//! Native BUILD evaluation, local macros, and frozen custom-rule declarations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Arguments, Evaluator, ReturnFileLoader};
use starlark::starlark_module;
use starlark::syntax::ast::{Argument, AstLiteral, Expr};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;

use crate::build_settings::BuildSetting;
use crate::custom_rule::{AttrSpec, AttrValue, CustomInvocation, InvocationDeclaration};
use crate::label::{Label, PackageId};
use crate::native_rules::NativeTarget;
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
    /// Another registered Bazel 9.3 native rule class (crate::native_rules);
    /// loaded and queryable, never analyzed.
    Native(&'static str),
}

impl RuleKind {
    /// Bazel's rule class name (`query --output=label_kind` spelling).
    pub fn class_name(&self) -> &str {
        match self {
            Self::Filegroup => "filegroup",
            Self::Genrule => "genrule",
            Self::Custom => "custom",
            Self::PackageGroup(_) => "package_group",
            Self::Native(name) => name,
        }
    }
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
    /// Retained native attribute values (filegroup, genrule and the other
    /// registered native classes; docs/NATIVE_BUILD_RULES.md).
    pub native: Option<Arc<NativeTarget>>,
    /// Load-time facts native analysis does not model; analysis of this rule
    /// fails explicitly when nonempty.
    pub unanalyzed: Vec<String>,
    /// The effective `package_metadata` (alias `applicable_licenses`)
    /// labels: the explicit value, else the package's
    /// `default_package_metadata` (BaseRuleClasses.packageMetadataDefault,
    /// analysis/BaseRuleClasses.java:100-107), as canonical label texts.
    /// Analyzed in the no-config configuration (BaseRuleClasses.java:291-297,
    /// crate::engine). Empty for rule classes without the attribute.
    pub package_metadata: Vec<String>,
    /// `Rule.getLocation()`: the BUILD-level call (`/abs/BUILD:L:C`, the
    /// call's left parenthesis), as Bazel prefixes rule events.
    pub location: Option<String>,
    /// Whether `visibility` was given (Rule.isAttributeValueExplicitlySpecified),
    /// for `query --noimplicit_deps`.
    pub explicit_visibility: bool,
    /// Rule.reconstructCallStack (packages/Rule.java:322-329), for
    /// `query --output=build`.
    pub call_stack: Vec<(String, String)>,
    /// The computed `generator_*` values of a rule created by a macro.
    pub generator: Option<Generator>,
}

impl Rule {
    /// TargetUtils.hasManualTag (packages/TargetUtils.java:105-107).
    pub fn is_manual(&self) -> bool {
        self.native
            .as_ref()
            .is_some_and(|native| native.is_manual())
            || self
                .custom
                .as_ref()
                .is_some_and(|custom| custom.declaration.builtin.target.is_manual())
    }

    /// Rule.isTestOnly (packages/Rule.java:794-800): the effective `testonly`
    /// value, falling back to the class override (`test_suite`, Starlark test
    /// rules) or the package default (BaseRuleClasses.java:68-74).
    pub fn is_test_only(&self, args: &PackageArgs) -> bool {
        if let Some(custom) = &self.custom {
            let builtin = &custom.declaration.builtin;
            return match builtin.target.attrs.get("testonly") {
                Some(AttrValue::Bool(value)) => *value,
                _ => crate::native_rules::starlark_builtins(builtin.kind)
                    .testonly_default
                    .unwrap_or_else(|| args.default_testonly.unwrap_or(false)),
            };
        }
        let Some(native) = &self.native else {
            return false;
        };
        let Some(class) = crate::native_rules::rule_class(&native.rule_class) else {
            return false;
        };
        if class.attribute("testonly").is_none() {
            return false;
        }
        match native.attrs.get("testonly") {
            Some(AttrValue::Bool(value)) => *value,
            _ => class
                .testonly_default
                .unwrap_or_else(|| args.default_testonly.unwrap_or(false)),
        }
    }

    /// Rule.getDeprecationWarning (packages/Rule.java:789-791): the explicit
    /// `deprecation` or the package default (BaseRuleClasses.java:76-82).
    pub fn deprecation_warning(&self, args: &PackageArgs) -> Option<String> {
        let explicit = if let Some(custom) = &self.custom {
            custom.declaration.builtin.target.string("deprecation")
        } else if let Some(native) = &self.native {
            native.string("deprecation")
        } else {
            return None;
        };
        explicit
            .map(str::to_owned)
            .or_else(|| args.default_deprecation.clone())
    }

    /// Rule class name for diagnostics (RuleContext.getRuleClassNameForLogging,
    /// analysis/RuleContext.java:1908-1911).
    pub fn rule_class_name(&self) -> &str {
        match (&self.custom, &self.native) {
            (Some(custom), _) => &custom.declaration.rule_class,
            (None, Some(native)) => &native.rule_class,
            (None, None) => self.kind.class_name(),
        }
    }
}

/// Package defaults from REPO.bazel `repo()` merged with `package()`
/// (packages/PackageArgs.java:164-204, mergeWith :210-243).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageArgs {
    pub default_visibility: Option<Visibility>,
    pub default_testonly: Option<bool>,
    pub default_deprecation: Option<String>,
    /// PackageArgs.features: a FeatureSet merged layer by layer, the finer
    /// (later) layer winning (PackageArgs.java:126-129, 175-177, 220-222;
    /// FeatureSet.merge, packages/FeatureSet.java:83-90).
    pub features: crate::ctx_surface::FeatureSet,
    pub license: Option<Vec<String>>,
    pub default_compatible_with: Option<Vec<Label>>,
    pub default_restricted_to: Option<Vec<Label>>,
    pub default_package_metadata: Option<Vec<Label>>,
    pub default_hdrs_check: Option<String>,
    pub transitive_visibility: Option<Label>,
}

impl PackageArgs {
    fn merge(&mut self, other: PackageArgs) {
        if other.default_visibility.is_some() {
            self.default_visibility = other.default_visibility;
        }
        if other.default_testonly.is_some() {
            self.default_testonly = other.default_testonly;
        }
        if other.default_deprecation.is_some() {
            self.default_deprecation = other.default_deprecation;
        }
        // PackageArgs.mergeWith (PackageArgs.java:220-222): FeatureSet.merge,
        // the package() layer over the REPO.bazel layer.
        if other.features != crate::ctx_surface::FeatureSet::default() {
            self.features = crate::ctx_surface::FeatureSet::merge(&self.features, &other.features);
        }
        if other.license.is_some() {
            self.license = other.license;
        }
        if other.default_compatible_with.is_some() {
            self.default_compatible_with = other.default_compatible_with;
        }
        if other.default_restricted_to.is_some() {
            self.default_restricted_to = other.default_restricted_to;
        }
        if other.default_package_metadata.is_some() {
            self.default_package_metadata = other.default_package_metadata;
        }
        if other.default_hdrs_check.is_some() {
            self.default_hdrs_check = other.default_hdrs_check;
        }
        if other.transitive_visibility.is_some() {
            self.transitive_visibility = other.transitive_visibility;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Package {
    /// Shared, immutable rule declarations: lookups clone the `Arc`, and
    /// analysis copies a rule only to configure it.
    pub rules: BTreeMap<Label, Arc<Rule>>,
    pub exports: BTreeMap<Label, Visibility>,
    pub default_visibility: Visibility,
    pub macro_names: BTreeSet<String>,
    /// Effective REPO.bazel/package() arguments.
    pub args: PackageArgs,
    /// The BUILD file (`Package.getBuildFile`), absolute.
    pub build_file: PathBuf,
    /// The BUILD file's direct `.bzl` loads and the load graph below them
    /// (Package.Declarations.visitLoadGraph), for `query buildfiles/loadfiles`.
    pub direct_loads: Vec<Label>,
    pub load_graph: HashMap<Label, Vec<Label>>,
}

impl std::ops::Deref for Package {
    type Target = BTreeMap<Label, Arc<Rule>>;
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
        /// The repository's ignored subdirectories, filtered for the package.
        pub ignored: crate::ignored::IgnoredSubdirectories,
        pub rules: RefCell<Vec<Rule>>,
        pub exports: RefCell<BTreeMap<Label, Visibility>>,
        /// TargetRecorder's name-conflict state (crate::package::Recorder).
        pub recorder: RefCell<Recorder>,
        /// Set while a callback Bazel runs on a transient thread without a
        /// package context (the rule(outputs=<function>) callback) executes.
        pub transient: Cell<bool>,
        pub default_visibility: RefCell<Visibility>,
        pub defaulted_rules: RefCell<BTreeSet<Label>>,
        pub package_called: Cell<bool>,
        pub package_args: RefCell<PackageArgs>,
        pub custom: RefCell<Vec<PendingCustom>>,
        pub active_macro: RefCell<Option<MacroOwner>>,
        pub macro_names: RefCell<BTreeSet<String>>,
        // An index of actual rule/output/export declarations, not inferred inputs.
        pub declared_names: RefCell<BTreeSet<String>>,
        pub group_budget: RefCell<HostBudget>,
        /// Generator call site of the active symbolic macro.
        pub macro_site: RefCell<Option<super::CallSite>>,
        /// Call expressions of the BUILD file (generator names, locations).
        pub calls: super::CallExpressions,
        /// The last legacy glob I/O error (TargetDefinitionContext.setIOException,
        /// packages/TargetDefinitionContext.java:499-504): the package fails
        /// with it whatever else happens (:823-826).
        pub glob_io_error: RefCell<Option<String>>,
        /// The first Skyframe glob symlink issue (GlobsFunction).
        pub glob_symlink_issue: RefCell<Option<crate::source_links::SymlinkIssue>>,
        /// The package's glob() outcomes by arguments and link policy
        /// (GlobCache, packages/GlobCache.java: one traversal per pattern
        /// and package evaluation). Lookups only: never iterated.
        pub globs: RefCell<HashMap<super::GlobKey, crate::glob::GlobOutcome>>,
    }

    /// REPO.bazel evaluation state (packages/RepoThreadContext.java).
    #[derive(ProvidesStaticType)]
    pub(super) struct RepoFile {
        pub repositories: Arc<RepositorySet>,
        pub repository: String,
        pub called: Cell<bool>,
        pub ignored_directories: RefCell<Option<Vec<String>>>,
        /// RepoFileValue keeps repo() kwargs raw; only RepoPackageArgsFunction
        /// converts them. Ignored-directory computation must not convert.
        pub process_args: bool,
        pub args: RefCell<PackageArgs>,
    }
}
use collector::{Declarations, RepoFile};

#[derive(Debug)]
struct PendingCustom {
    index: usize,
    slot: String,
    subrule_slots: Vec<String>,
    /// (`""` for the rule's `cfg`, else the attribute name, module slot).
    transition_slots: Vec<(String, String)>,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
    build_setting: Option<BuildSetting>,
    declaration: InvocationDeclaration,
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

/// Dependency labels of Starlark attribute values, in attribute-name order:
/// label, label list and label-keyed dictionary keys (also of defaults).
/// Configured targets recompute this from resolved values
/// (crate::configured_attributes::configure).
pub(crate) fn custom_dependency_labels(attrs: &BTreeMap<String, AttrValue>) -> Vec<String> {
    let mut srcs = Vec::new();
    for value in attrs.values() {
        match value {
            AttrValue::Label(Some(value)) => srcs.push(value.clone()),
            AttrValue::Labels(values) => srcs.extend(values.iter().cloned()),
            AttrValue::LabelKeyedStringDict(entries) => {
                srcs.extend(entries.iter().map(|(label, _)| label.clone()))
            }
            // DictType.visitLabels (packages/Type.java:504-514): label values.
            AttrValue::StringKeyedLabelDict(entries) => {
                srcs.extend(entries.iter().map(|(_, label)| label.clone()))
            }
            AttrValue::LabelListDict(entries) => srcs.extend(
                entries
                    .iter()
                    .flat_map(|(_, labels)| labels.iter().cloned()),
            ),
            _ => {}
        }
    }
    srcs
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register_custom<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    name: &str,
    visibility: Option<Value<'v>>,
    implementation: Value<'v>,
    subrules: Vec<Value<'v>>,
    transitions: Vec<(String, Value<'v>)>,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
    build_setting: Option<BuildSetting>,
    mut declaration: InvocationDeclaration,
) -> anyhow::Result<()> {
    let context = declarations(eval)?;
    let (location, generator) = rule_origin(eval, context, name);
    declaration.builtin.generator = generator.clone();
    let visibility = rule_visibility(visibility, context, name)?;
    // ctx.attr.visibility is the explicit, simplified attribute value.
    declaration.builtin.visibility = visibility.as_ref().map(|visibility| {
        visibility
            .declared_labels()
            .iter()
            .map(ToString::to_string)
            .collect()
    });
    let srcs = custom_dependency_labels(&attrs);
    // rule(outputs=...) implicit outputs are package output files too, before
    // the explicit attr.output/attr.output_list ones (Rule.java:526-528, 573-610):
    // the order of the rule's outputs and its default files to build.
    let mut outs = declaration
        .implicit_outputs
        .iter()
        .map(|(_, output)| output.clone())
        .collect::<Vec<_>>();
    for value in attrs.values() {
        match value {
            AttrValue::Output(Some(value)) => outs.push(value.clone()),
            AttrValue::Outputs(values) => outs.extend(values.iter().cloned()),
            _ => {}
        }
    }
    let index = context.rules.borrow().len();
    // Rule.getLabels (packages/Rule.java:438-442): every non-output, non-nodep
    // label, select() keys and branches included.
    let inputs = declaration
        .configurable_labels
        .iter()
        .chain(&declaration.builtin.target.input_labels)
        .cloned()
        .collect();
    let site = TargetSite {
        callee: declaration.rule_class.clone(),
        kind: format!("{} rule", declaration.rule_class),
        location,
        inputs,
    };
    let call_stack = java_call_stack(eval);
    context.declare(name, RuleKind::Custom, srcs, outs, None, visibility, site)?;
    if let Some(rule) = context.rules.borrow_mut().last_mut() {
        rule.call_stack = call_stack;
        rule.generator = generator;
    }
    // Not a valid Starlark identifier: BUILD assignments cannot overwrite roots.
    let slot = format!("__custom_implementation${index}");
    eval.module().set(&slot, implementation);
    let subrule_slots = subrules
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            let slot = format!("__custom_subrule${index}${i}");
            eval.module().set(&slot, value);
            slot
        })
        .collect();
    let transition_slots = transitions
        .into_iter()
        .enumerate()
        .map(|(i, (key, value))| {
            let slot = format!("__custom_transition${index}${i}");
            eval.module().set(&slot, value);
            (key, slot)
        })
        .collect();
    context.custom.borrow_mut().push(PendingCustom {
        index,
        slot,
        subrule_slots,
        transition_slots,
        attrs,
        schema,
        build_setting,
        declaration,
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

    #[allow(clippy::too_many_arguments)]
    fn declare(
        &self,
        name: &str,
        kind: RuleKind,
        srcs: Vec<String>,
        outs: Vec<String>,
        cmd: Option<String>,
        visibility: Option<Visibility>,
        site: TargetSite,
    ) -> anyhow::Result<NoneType> {
        let label = local_label(name, &self.package_id(), "rule name")?;
        self.check_macro_collision(name)?;
        // Custom and native attribute labels are canonicalized at loading,
        // except native labels of not-visible apparent repositories, whose
        // resolution error is deferred to analysis (native_rules::deferred).
        for src in &srcs {
            (if src.starts_with('@') && !src.starts_with("@@") {
                Label::validate_syntax(src, &self.package)
            } else {
                Label::parse_canonical(src).map(|_| ())
            })
            .map_err(|e| anyhow!("invalid srcs entry {src:?} in {label}: {e}"))?;
        }
        for out in &outs {
            self.check_macro_collision(out)?;
            label
                .with_name(out)
                .map_err(|e| anyhow!("invalid output path {out:?}: {e}"))?;
        }
        let macro_owner = self.active_macro.borrow().clone();
        let explicit_visibility = visibility.is_some();
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
        // TargetRecorder.addRule (packages/TargetRecorder.java:425-436): the
        // conflict is thrown at the declaring call (RuleFactory.java:325-326,
        // StarlarkRuleClassFunctions.java:1896-1897), so evaluation stops here.
        let inputs = srcs
            .iter()
            .filter_map(|text| Label::parse_canonical(text).ok())
            .chain(site.inputs.iter().cloned())
            .filter(|input| input.package_id() == label.package_id())
            .map(|input| input.name)
            .collect::<Vec<_>>();
        self.recorder
            .borrow_mut()
            .record_rule(name, &site.kind, site.location.as_deref(), &outs, &inputs)
            .map_err(|error| anyhow!("Error in {}: {error}", site.callee))?;
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
            native: None,
            unanalyzed: Vec::new(),
            package_metadata: Vec::new(),
            location: site.location,
            explicit_visibility,
            call_stack: Vec::new(),
            generator: None,
        });
        Ok(NoneType)
    }
}

/// The BUILD-level call being evaluated, as Bazel's StarlarkThread call stack
/// entry 0 sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CallSite {
    /// `Location.toString()` of the call's left parenthesis (Java Starlark
    /// call locations are `CallExpression.getLparenLocation`).
    pub location: String,
    /// The callee's name when it is a legacy macro (a `.bzl` function that
    /// called the rule): `CallStack.Node.functionName` of the interior stack.
    pub function: Option<String>,
    /// `f(name = "literal")` of that call (PackageFactory.checkBuildSyntax
    /// recordGeneratorName, packages/PackageFactory.java:637-648).
    pub name_literal: Option<String>,
}

/// Per BUILD call expression (keyed by its span): the left parenthesis
/// position and the `name = "literal"` keyword (PackageFactory.checkBuildSyntax
/// recordGeneratorName, packages/PackageFactory.java:637-648).
pub(crate) type CallExpressions = HashMap<(u32, u32), (u32, Option<String>)>;

/// Index every call expression of a BUILD file, nested calls included.
fn call_expressions(ast: &AstModule) -> CallExpressions {
    let file = ast.file_span(ast.statement().span).file;
    let source = file.source().as_bytes();
    let mut calls = HashMap::new();
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        if let Expr::Call(callee, arguments) = &expr.node {
            let mut lparen = callee.span.end().get() as usize;
            while lparen < source.len() && source[lparen] != b'(' {
                lparen += 1;
            }
            let name = arguments
                .args
                .iter()
                .rev()
                .find_map(|argument| match &argument.node {
                    Argument::Named(name, value) if name.node == "name" => match &value.node {
                        Expr::Literal(AstLiteral::String(text)) => Some(text.node.clone()),
                        _ => None,
                    },
                    _ => None,
                });
            calls.insert(
                (expr.span.begin().get(), expr.span.end().get()),
                (lparen as u32, name),
            );
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    calls
}

/// Computed `generator_*` values of a rule (Rule.java:395-405 with
/// RuleFactory.getGeneratorName, packages/RuleFactory.java:238-274).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Generator {
    /// The BUILD call's `name` literal, else the target name.
    pub name: String,
    /// The outermost function called from BUILD (legacy macro), or the
    /// symbolic macro's exported name.
    pub function: String,
    /// Rule.getRelativeLocation (packages/Rule.java:353-366): the location from
    /// the first occurrence of the package name; `None` when absent.
    pub location: Option<String>,
}

/// `Rule.getLocation()` and the computed generator values of a rule
/// instantiated now (TargetDefinitionContext.createRule,
/// packages/TargetDefinitionContext.java:543-559).
fn rule_origin(
    eval: &Evaluator<'_, '_, '_>,
    context: &Declarations,
    name: &str,
) -> (Option<String>, Option<Generator>) {
    let site = call_site(eval, &context.calls);
    let location = site.as_ref().map(|site| site.location.clone());
    let generator_site = if context.active_macro.borrow().is_some() {
        context.macro_site.borrow().clone()
    } else {
        site.filter(|site| site.function.is_some())
    };
    let generator = generator_site.map(|site| Generator {
        name: site.name_literal.unwrap_or_else(|| name.to_owned()),
        function: site.function.unwrap_or_default(),
        // String.indexOf: the empty root package name matches at 0.
        location: site
            .location
            .find(context.package.as_str())
            .map(|position| site.location[position..].to_owned()),
    });
    (location, generator)
}

/// StarlarkThread.getCallStack (net/starlark/java/eval/StarlarkThread.java:603-610)
/// at a call of a built-in, without the built-in's own innermost frame
/// (RuleFactory.createRule, packages/RuleFactory.java:96-98;
/// StarlarkRuleClassFunctions.java:915-918): `(function name, location)`
/// entries, outermost first, starting with `<toplevel>`. Starlark-rust frames
/// are `(callee, call site in the caller)`; Java locations are call left
/// parentheses (CallExpression.getLparenLocation) and `.bzl` files print as
/// paths.
pub(crate) fn java_call_stack(eval: &Evaluator<'_, '_, '_>) -> Vec<(String, String)> {
    let repositories = crate::loading::repositories(eval);
    let frames = eval.call_stack().frames;
    let mut stack = Vec::new();
    let mut caller = "<toplevel>".to_owned();
    for frame in &frames {
        let Some(span) = frame.location.as_ref() else {
            continue;
        };
        let source = span.file.source().as_bytes();
        let mut lparen = span.span.begin().get() as usize;
        while lparen < source.len() && source[lparen] != b'(' {
            lparen += 1;
        }
        let position = starlark::codemap::Pos::new(lparen.min(source.len()) as u32);
        let resolved = span
            .file
            .resolve_span(starlark::codemap::Span::new(position, position));
        let filename = span.filename();
        let path = Label::parse_canonical(filename)
            .ok()
            .and_then(|label| {
                let root = repositories.as_ref()?.root(&label.repository).ok()?;
                Some(
                    root.join(&label.package)
                        .join(&label.name)
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .unwrap_or_else(|| filename.to_owned());
        stack.push((
            std::mem::replace(&mut caller, frame.name.clone()),
            format!(
                "{path}:{}:{}",
                resolved.begin.line + 1,
                resolved.begin.column + 1
            ),
        ));
    }
    stack
}

/// Starlark-rust frames are `(callee, call site in the caller)`, outermost
/// first; the first frame's site is the BUILD-level call (Java's call stack
/// entry 0 location). A rule called from a function whose current location is
/// in a `.bzl` file was created by a legacy macro
/// (RuleFactory.getGeneratorName, packages/RuleFactory.java:238-274).
fn call_site(eval: &Evaluator<'_, '_, '_>, calls: &CallExpressions) -> Option<CallSite> {
    let frames = eval.call_stack().frames;
    let first = frames.first()?;
    let span = first.location.as_ref()?;
    let (lparen, name_literal) = calls
        .get(&(span.span.begin().get(), span.span.end().get()))
        .cloned()
        .unwrap_or((span.span.begin().get(), None));
    let position = starlark::codemap::Pos::new(lparen);
    let resolved = span
        .file
        .resolve_span(starlark::codemap::Span::new(position, position));
    let in_macro = frames.get(1).is_some_and(|frame| {
        frame
            .location
            .as_ref()
            .is_some_and(|location| location.filename().ends_with(".bzl"))
    });
    Some(CallSite {
        location: format!(
            "{}:{}:{}",
            span.filename(),
            resolved.begin.line + 1,
            resolved.begin.column + 1
        ),
        function: in_macro.then(|| first.name.clone()),
        name_literal,
    })
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
    if crate::rule_declarations::select_pieces(value).is_some() {
        bail!(
            "{attribute}: got value of type 'select', want 'sequence' (select() is not accepted here)"
        );
    }
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

/// A rule's `visibility` argument. A misspelled //visibility label is reported
/// as `LABEL MESSAGE` (AttributeProvider.java:277-286), for native and Starlark
/// rules alike.
fn rule_visibility(
    value: Option<Value<'_>>,
    context: &Declarations,
    name: &str,
) -> anyhow::Result<Option<Visibility>> {
    let parsed = (|| {
        let Some(value) = value.filter(|value| !value.is_none()) else {
            return Ok(None);
        };
        let mut labels = visibility_labels(value, context)?;
        // `visibility` is orderIndependent (BaseRuleClasses.java:232-236): the
        // converted list is sorted (BuildType.java:248-253) before
        // RuleVisibility.validateAndSimplify (AttributeProvider.java:277-286).
        labels.sort_by(crate::native_rules::label_cmp);
        Visibility::from_labels(labels)
            .map(Some)
            .map_err(anyhow::Error::msg)
    })();
    parsed.map_err(|error| {
        let text = error.to_string();
        if text.starts_with("Invalid visibility label") {
            anyhow!(
                "{} {text}",
                crate::rule_declarations::canonical_form(&Label {
                    repository: context.repository.clone(),
                    package: context.package.clone(),
                    name: name.to_owned(),
                })
            )
        } else {
            error
        }
    })
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
                // BuildType.LABEL_LIST conversion (Label.parseWithPackageContext):
                // an apparent repository the package's repository does not see
                // is an invalid label; a __pkg__/__subpackages__ scope in it
                // matches no package and a package_group in it fails only when
                // the visibility check loads it.
                context
                    .repositories
                    .parse_deferred(text, &context.repository, &context.package)
                    .map_err(anyhow::Error::msg)
            } else {
                bail!("visibility entries must be strings or Labels")
            }
        })
        .collect()
}

struct MacroSiteFrame<'a> {
    slot: &'a RefCell<Option<CallSite>>,
    previous: Option<CallSite>,
}

impl<'a> MacroSiteFrame<'a> {
    fn enter(slot: &'a RefCell<Option<CallSite>>, site: Option<CallSite>) -> Self {
        Self {
            slot,
            previous: slot.replace(site),
        }
    }
}

impl Drop for MacroSiteFrame<'_> {
    fn drop(&mut self) {
        self.slot.replace(self.previous.take());
    }
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
    if context.transient.get() {
        bail!("{TRANSIENT}");
    }
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
    // MacroInstance generator name and parent call stack
    // (TargetDefinitionContext.createMacro, packages/TargetDefinitionContext.java:570-595):
    // the BUILD call site, and the macro callable as the interior function.
    let site = call_site(eval, &context.calls).map(|site| CallSite {
        function: exported.map(str::to_owned),
        ..site
    });
    let _site = MacroSiteFrame::enter(&context.macro_site, site);
    let _frame = MacroFrame::enter(&context.active_macro, owner);
    // Bazel runs the implementation in a new Starlark thread
    // (packages/MacroClass.java:390-420), so the calls that instantiated
    // the macro do not make calls within it recursive.
    let name = eval.heap().alloc(name);
    let result = eval
        .with_new_recursion_scope(|eval| {
            eval.eval_function(
                implementation,
                &[],
                &[("name", name), ("visibility", visibility)],
            )
        })
        .map_err(|e| anyhow!(e.to_string()))?;
    if !result.is_none() {
        bail!("symbolic macro {name:?} must return None");
    }
    Ok(Value::new_none())
}

fn declarations<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Declarations> {
    let context = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .ok_or_else(|| anyhow!("rule declaration outside package evaluation"))?;
    if context.transient.get() {
        bail!("{TRANSIENT}");
    }
    Ok(context)
}

/// TargetDefinitionContext.fromOrFail (packages/TargetDefinitionContext.java:187-248)
/// on a thread without a package context.
const TRANSIENT: &str = "package functions (rules, native.*, glob, existing_rules, ...) can only be used while evaluating a BUILD file or a legacy or symbolic macro, not in an implicit outputs function (StarlarkCallbackHelper runs it on a transient thread without a package context)";

/// Run `callback` without the package context, like Bazel's
/// StarlarkCallbackHelper transient thread (StarlarkCallbackHelper.java:64-77):
/// package functions fail inside it.
pub(crate) fn without_package_context<'v, 'a, 'e, R>(
    eval: &mut Evaluator<'v, 'a, 'e>,
    callback: impl FnOnce(&mut Evaluator<'v, 'a, 'e>) -> R,
) -> R {
    // Copy the independent extra reference; never borrow eval over the callback.
    let extra = eval.extra;
    let context = extra.and_then(|extra| extra.downcast_ref::<Declarations>());
    let previous = context.map(|context| context.transient.replace(true));
    let result = callback(eval);
    if let (Some(context), Some(previous)) = (context, previous) {
        context.transient.set(previous);
    }
    result
}

// The upstream derives generate unsafe marker-trait implementations for these
// owned, heap-free callables; there is no handwritten unsafe code.
#[allow(unsafe_code)]
mod native_functions {
    use allocative::Allocative;
    use starlark::eval::{Arguments, Evaluator};
    use starlark::starlark_simple_value;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, starlark_value,
    };
    use std::fmt;

    /// A registered native rule class (RuleFactory.BuiltinRuleFunction,
    /// packages/RuleFactory.java:293-355).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct NativeRuleFunction(pub String);
    impl fmt::Display for NativeRuleFunction {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<built-in rule {}>", self.0)
        }
    }
    starlark_simple_value!(NativeRuleFunction);
    // RuleFunction is @StarlarkBuiltin(name = "rule") (starlarkbuildapi/RuleFunctionApi);
    // genuine `type(genrule)` is "rule".
    #[starlark_value(type = "rule")]
    impl<'v> StarlarkValue<'v> for NativeRuleFunction {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            // BuiltinRuleFunction.call (RuleFactory.java:304-308).
            if args.positions(eval.heap())?.next().is_some() {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "unexpected positional arguments"
                )));
            }
            let named = args
                .names_map()?
                .iter()
                .map(|(key, value)| (key.as_str().to_owned(), *value))
                .collect::<Vec<_>>();
            super::declare_native(eval, &self.0, &named).map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }

    /// `cc_toolchain_suite` is the builtins lambda
    /// `lambda name, **kwargs: native.filegroup(name = name)`
    /// (src/main/starlark/builtins_bzl/bazel/exports.bzl:53-54): a Starlark
    /// function (type `function`) whose repr names its `_builtins` module
    /// (net/starlark/java/eval/StarlarkFunction.java:222-231).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct CcToolchainSuite;
    impl fmt::Display for CcToolchainSuite {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<function lambda from @@_builtins//:bazel/exports.bzl>")
        }
    }
    starlark_simple_value!(CcToolchainSuite);
    #[starlark_value(type = "function")]
    impl<'v> StarlarkValue<'v> for CcToolchainSuite {
        /// Starlark lambda binding errors follow StarlarkFunction argument
        /// processing (`lambda() missing 1 required positional argument: name`).
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            let positional: Vec<_> = args.positions(eval.heap())?.collect();
            let named = args.names_map()?;
            if positional.len() > 1 {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "lambda() accepts no more than 1 positional argument but got {}",
                    positional.len()
                )));
            }
            let keyword = named
                .iter()
                .find(|(key, _)| key.as_str() == "name")
                .map(|(_, value)| *value);
            let name = match (positional.first(), keyword) {
                (Some(_), Some(_)) => {
                    return Err(starlark::Error::new_other(anyhow::anyhow!(
                        "lambda() got multiple values for parameter 'name'"
                    )));
                }
                (Some(name), None) => *name,
                (None, Some(name)) => name,
                (None, None) => {
                    return Err(starlark::Error::new_other(anyhow::anyhow!(
                        "lambda() missing 1 required positional argument: name"
                    )));
                }
            };
            super::declare_native(eval, "filegroup", &[("name".to_owned(), name)])
                .map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }

    /// `_removed_rule_failure(**_kwargs)` injected by builtins
    /// (src/main/starlark/builtins_bzl/bazel/exports.bzl:39-58).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct RemovedRule;
    impl fmt::Display for RemovedRule {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // StarlarkFunction.repr with the module's BazelModuleContext label
            // (net/starlark/java/eval/StarlarkFunction.java:222-231).
            f.write_str("<function _removed_rule_failure from @@_builtins//:bazel/exports.bzl>")
        }
    }
    starlark_simple_value!(RemovedRule);
    #[starlark_value(type = "function")]
    impl<'v> StarlarkValue<'v> for RemovedRule {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            let positional = args.positions(eval.heap())?.count();
            if positional > 0 {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "_removed_rule_failure() accepts no more than 0 positional arguments but got {positional}"
                )));
            }
            Err(starlark::Error::new_other(anyhow::anyhow!(
                "{}",
                crate::native_rules::REMOVED_RULE_MESSAGE
            )))
        }
    }
}

/// Register every native rule class (BUILD globals and `native`).
fn native_rule_globals(builder: &mut GlobalsBuilder) {
    for name in crate::native_rules::RULE_CLASSES {
        builder.set(
            name,
            native_functions::NativeRuleFunction((*name).to_owned()),
        );
    }
    for name in crate::native_rules::REMOVED_RULES {
        builder.set(name, native_functions::RemovedRule);
    }
    builder.set("cc_toolchain_suite", native_functions::CcToolchainSuite);
}

/// Instantiate a native rule class at loading.
fn declare_native<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    class: &str,
    arguments: &[(String, Value<'v>)],
) -> anyhow::Result<()> {
    let context = declarations(eval)?;
    let class = crate::native_rules::rule_class(class)
        .ok_or_else(|| anyhow!("unregistered native rule class {class}"))?;
    let instance = crate::native_rules::instantiate(
        &class,
        arguments,
        &context.repositories,
        &context.package_id(),
    )?;
    let visibility = rule_visibility(instance.visibility, context, &instance.name)?;
    let target = instance.target;
    // Plain (non-select) srcs/cmd only; select()s are resolved per
    // configuration and value-dependent analysis gaps are evaluated on the
    // resolved values (crate::configured_attributes, crate::engine).
    let (kind, srcs, outs, cmd) = match class.name {
        "filegroup" => (RuleKind::Filegroup, target.labels("srcs"), Vec::new(), None),
        "genrule" => (
            RuleKind::Genrule,
            target.labels("srcs"),
            target.strings("outs"),
            target.string("cmd").map(str::to_owned),
        ),
        name => (
            RuleKind::Native(name),
            Vec::new(),
            class
                .implicit_outputs
                .iter()
                .map(|template| template.replace("%{name}", &instance.name))
                .collect(),
            None,
        ),
    };
    let (location, generator) = rule_origin(eval, context, &instance.name);
    let call_stack = java_call_stack(eval);
    let site = TargetSite {
        callee: class.name.to_owned(),
        kind: format!("{} rule", class.name),
        location,
        inputs: target.input_labels.clone(),
    };
    context.declare(&instance.name, kind, srcs, outs, cmd, visibility, site)?;
    let mut rules = context.rules.borrow_mut();
    let rule = rules.last_mut().expect("declared rule");
    rule.native = Some(Arc::new(target));
    rule.call_stack = call_stack;
    rule.generator = generator;
    Ok(())
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
                    // A label list (Label.parseWithPackageContext): an
                    // unknown repository fails only when the include is loaded.
                    context
                        .repositories
                        .parse_deferred(
                            value.unpack_str().unwrap(),
                            &context.repository,
                            &context.package,
                        )
                        .map_err(anyhow::Error::msg)
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (location, _) = rule_origin(eval, context, name);
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
            // PackageGroup.getLocation: the call's left parenthesis.
            TargetSite {
                callee: "package_group".to_owned(),
                kind: "package group".to_owned(),
                location,
                inputs: Vec::new(),
            },
        )
    }
}

#[starlark_module]
fn package_globals(builder: &mut GlobalsBuilder) {
    /// PackageCallable.packageCallable (packages/PackageCallable.java:50-77).
    fn package<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.forbid_in_macro("package")?;
        if context.package_called.replace(true) {
            bail!("'package' can only be used once per BUILD file");
        }
        if kwargs.is_empty() {
            bail!("at least one argument must be given to the 'package' function");
        }
        let mut args = PackageArgs::default();
        for (name, value) in kwargs.iter() {
            process_package_arg(
                name,
                *value,
                &format!("package() argument '{name}'"),
                &context.repositories,
                &context.package_id(),
                &mut args,
            )?;
        }
        if let Some(visibility) = args.default_visibility.clone() {
            *context.default_visibility.borrow_mut() = visibility;
        }
        context.package_args.borrow_mut().merge(args);
        Ok(NoneType)
    }

    /// StarlarkNativeModule.packageDefaultVisibility (StarlarkNativeModule.java:623-634).
    fn package_default_visibility<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<Value<'v>>> {
        let context = declarations(eval)?;
        let labels = context
            .default_visibility
            .borrow()
            .with_package(&context.package_id())
            .declared_labels();
        labels
            .iter()
            .map(|label| crate::starlark_types::alloc_label(eval.heap(), &label.to_string()))
            .collect()
    }

    /// StarlarkNativeModule.repoName (StarlarkNativeModule.java:643-648).
    fn repo_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        Ok(declarations(eval)?.repository.clone())
    }

    /// StarlarkNativeModule.repositoryName (StarlarkNativeModule.java:636-641).
    fn repository_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        Ok(format!("@{}", declarations(eval)?.repository))
    }

    /// StarlarkNativeModule.packageRelativeLabel (StarlarkNativeModule.java:650-667).
    fn package_relative_label<'v>(
        input: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = declarations(eval)?;
        if crate::starlark_types::label_parts(input).is_some() {
            return Ok(input);
        }
        let Some(text) = input.unpack_str() else {
            bail!(
                "in call to package_relative_label(), parameter 'input' got value of type '{}', want 'string or Label'",
                input.get_type()
            );
        };
        crate::bazel_label::Parts::parse(text)
            .map_err(|error| anyhow!("invalid label in native.package_relative_label: {error}"))?;
        let label = context
            .repositories
            .parse(text, &context.repository, &context.package)
            .map_err(|error| anyhow!("invalid label in native.package_relative_label: {error}"))?;
        crate::starlark_types::alloc_label(eval.heap(), &label.to_string())
    }

    /// StarlarkNativeModule.moduleName (StarlarkNativeModule.java:669-675).
    fn module_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        let context = declarations(eval)?;
        Ok(context
            .repositories
            .associated_module(&context.repository)
            .map_err(anyhow::Error::msg)?
            .0)
    }

    /// StarlarkNativeModule.moduleVersion (StarlarkNativeModule.java:677-683).
    fn module_version(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        let context = declarations(eval)?;
        Ok(context
            .repositories
            .associated_module(&context.repository)
            .map_err(anyhow::Error::msg)?
            .1)
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
            // createInputFile -> TargetRecorder.addTarget
            // (StarlarkNativeModule.java:586-610): an existing source file is
            // idempotent, any other target conflicts.
            let (location, _) = rule_origin(eval, context, &name);
            context
                .recorder
                .borrow_mut()
                .record_source(&name, location.as_deref())
                .map_err(|error| anyhow!("Error in exports_files: {error}"))?;
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
        let key = GlobKey {
            includes,
            excludes,
            exclude_directories: exclude_directories != 0,
            allow_empty,
            links: context.repositories.link_policy(&context.repository),
        };
        let cached = context.globs.borrow().get(&key).cloned();
        let outcome = match cached {
            Some(outcome) => outcome,
            None => {
                #[cfg(test)]
                GLOB_WALKS.with(|walks| walks.set(walks.get() + 1));
                let outcome = crate::glob::glob(
                    &context.root,
                    &context.package,
                    &key.includes,
                    &key.excludes,
                    key.exclude_directories,
                    key.allow_empty,
                    &context.ignored,
                    &key.links,
                )
                .map_err(anyhow::Error::msg)?;
                context.globs.borrow_mut().insert(key, outcome.clone());
                outcome
            }
        };
        // StarlarkNativeModule.runGlobOperation (StarlarkNativeModule.java:851-876):
        // an I/O error is an error event plus the package's IOException; the
        // call returns an empty list and evaluation continues.
        if let Some(error) = outcome.io_error {
            *context.glob_io_error.borrow_mut() = Some(error);
            return Ok(Vec::new());
        }
        if let Some(issue) = outcome.symlink_issue {
            context.glob_symlink_issue.borrow_mut().get_or_insert(issue);
        }
        Ok(outcome.paths)
    }

    fn package_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        Ok(declarations(eval)?.package.clone())
    }
}

#[cfg(test)]
thread_local! {
    /// glob() calls that walked the file system on this thread.
    static GLOB_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The arguments of a package's `glob()` call and the link policy it ran
/// under: equal keys have equal outcomes within one package evaluation.
#[derive(PartialEq, Eq, Hash)]
struct GlobKey {
    includes: Vec<String>,
    excludes: Vec<String>,
    exclude_directories: bool,
    allow_empty: bool,
    links: crate::source_links::LinkPolicy,
}

/// BUILD-file-only globals (BuildGlobals, not shared with `native`).
#[starlark_module]
fn build_only_globals(builder: &mut GlobalsBuilder) {
    /// BuildGlobals.licenses (packages/BuildGlobals.java:100-132).
    fn licenses<'v>(
        #[starlark(require = pos)] license_strings: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        if crate::custom_rule::sequence_ref(license_strings).is_none()
            || crate::custom_rule::sequence_ref(license_strings)
                .unwrap()
                .iter()
                .any(|value| value.unpack_str().is_none())
        {
            bail!(
                "in call to licenses(), parameter 'license_strings' got value of type '{}', want 'sequence of strings'",
                license_strings.get_type()
            );
        }
        let license = crate::native_rules::convert_license(license_strings, "'licenses' operand")?;
        context.package_args.borrow_mut().license = Some(license);
        Ok(NoneType)
    }
}

/// PackageArgs.processParam (packages/PackageArgs.java:160-204).
fn process_package_arg(
    name: &str,
    value: Value<'_>,
    what: &str,
    repositories: &RepositorySet,
    package: &PackageId,
    args: &mut PackageArgs,
) -> anyhow::Result<()> {
    use crate::custom_rule::AttrValue;
    use crate::native_rules::{NativeType, convert_value};
    let convert = |ty| convert_value(repositories, package, ty, value, what);
    let labels = |ty| -> anyhow::Result<Vec<Label>> {
        match convert(ty)? {
            AttrValue::Labels(values) => values
                .iter()
                .map(|text| Label::parse_canonical(text).map_err(anyhow::Error::msg))
                .collect(),
            _ => unreachable!("label list conversion"),
        }
    };
    // PackageArgs.throwIfHasDupes (PackageArgs.java:152-158).
    let no_dupes = |labels: Vec<Label>| -> anyhow::Result<Vec<Label>> {
        let mut seen = BTreeSet::new();
        let mut dupes = BTreeSet::new();
        for label in &labels {
            if !seen.insert(label.clone()) {
                dupes.insert(crate::rule_declarations::canonical_form(label));
            }
        }
        if !dupes.is_empty() {
            bail!(
                "duplicate label(s) in {name}: {}",
                dupes.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        Ok(labels)
    };
    let string = || -> anyhow::Result<String> {
        match convert(NativeType::String)? {
            AttrValue::String(text) => Ok(text),
            _ => unreachable!("string conversion"),
        }
    };
    match name {
        "default_visibility" => {
            args.default_visibility = Some(
                Visibility::from_labels(labels(NativeType::LabelList)?)
                    .map_err(anyhow::Error::msg)?,
            )
        }
        "default_testonly" => {
            let AttrValue::Bool(value) = convert(NativeType::Bool)? else {
                unreachable!("bool conversion")
            };
            args.default_testonly = Some(value);
        }
        "default_deprecation" => args.default_deprecation = Some(string()?),
        "features" => {
            let AttrValue::Strings(features) = convert(NativeType::StringList)? else {
                unreachable!("string list conversion")
            };
            // Builder.mergeFeatures(FeatureSet.parse(...)) (PackageArgs.java:175-177).
            args.features = crate::ctx_surface::FeatureSet::merge(
                &args.features,
                &crate::ctx_surface::FeatureSet::parse(features.iter().map(String::as_str)),
            );
        }
        "licenses" => {
            args.license = Some(crate::native_rules::convert_license(value, what)?);
        }
        "default_compatible_with" => {
            args.default_compatible_with = Some(no_dupes(labels(NativeType::LabelList)?)?)
        }
        "default_restricted_to" => {
            args.default_restricted_to = Some(no_dupes(labels(NativeType::LabelList)?)?)
        }
        "default_applicable_licenses" | "default_package_metadata" => {
            if args.default_package_metadata.is_some() {
                bail!(
                    "Can not set both default_package_metadata and default_applicable_licenses. Move all declarations to default_package_metadata."
                );
            }
            args.default_package_metadata = Some(no_dupes(labels(NativeType::LabelList)?)?);
        }
        "default_hdrs_check" => args.default_hdrs_check = Some(string()?),
        "transitive_visibility" => {
            let AttrValue::Label(Some(label)) = convert(NativeType::Label)? else {
                unreachable!("label conversion")
            };
            args.transitive_visibility =
                Some(Label::parse_canonical(&label).map_err(anyhow::Error::msg)?);
        }
        _ => bail!("unexpected keyword argument: {name}"),
    }
    Ok(())
}

#[starlark_module]
fn repo_file_globals(builder: &mut GlobalsBuilder) {
    /// RepoFileGlobals.repoCallable (packages/RepoFileGlobals.java), with the
    /// arguments processed like RepoPackageArgsFunction (:117-130).
    fn repo<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = repo_file(eval)?;
        if context.called.replace(true) {
            bail!("'repo' can only be called once in the REPO.bazel file");
        }
        if context.ignored_directories.borrow().is_some() {
            bail!("if repo() is called, it must be called before any other functions");
        }
        if kwargs.is_empty() {
            bail!("at least one argument must be given to the 'repo' function");
        }
        if !context.process_args {
            return Ok(NoneType);
        }
        let package = PackageId {
            repository: context.repository.clone(),
            package: String::new(),
        };
        let mut args = PackageArgs::default();
        for (name, value) in kwargs.iter() {
            process_package_arg(
                name,
                *value,
                &format!("repo() argument '{name}'"),
                &context.repositories,
                &package,
                &mut args,
            )?;
        }
        *context.args.borrow_mut() = args;
        Ok(NoneType)
    }

    /// RepoFileGlobals.ignoreDirectories (packages/RepoFileGlobals.java:35-61):
    /// positional `dirs` of type sequence, `Sequence.cast(dirs, String.class,
    /// "dirs")`, then the once-only check (with Bazel's `ignored_` spelling).
    ///
    /// Argument binding follows BuiltinFunction (net/starlark/java/eval/
    /// BuiltinFunction.java:150-160, 330-345, 455-480): too many positionals,
    /// then named arguments, then the missing positional parameter.
    fn ignore_directories<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        if args.items.len() > 1 {
            bail!(
                "ignore_directories() accepts no more than 1 positional argument but got {}",
                args.items.len()
            );
        }
        if let Some((name, _)) = kwargs.iter().next() {
            if name == "dirs" {
                bail!(
                    "ignore_directories() got named argument for positional-only parameter 'dirs'"
                );
            }
            bail!("ignore_directories() got unexpected keyword argument '{name}'");
        }
        let Some(&dirs) = args.items.first() else {
            bail!("ignore_directories() missing 1 required positional argument: dirs");
        };
        let dirs = if dirs.get_type() == "range" {
            // RangeList is a Sequence of ints: only an empty one casts.
            if dirs.length().map_err(|e| anyhow!("{e}"))? != 0 {
                bail!("at index 0 of dirs, got element of type int, want string");
            }
            Vec::new()
        } else if crate::custom_rule::sequence_ref(dirs).is_some() {
            crate::rule_declarations::cast_strings(dirs, "dirs")?
        } else {
            bail!(
                "in call to ignore_directories(), parameter 'dirs' got value of type '{}', want 'sequence'",
                dirs.get_type()
            );
        };
        let context = repo_file(eval)?;
        if context.ignored_directories.borrow().is_some() {
            bail!("'ignored_directories()' can only be called once");
        }
        *context.ignored_directories.borrow_mut() = Some(dirs);
        Ok(NoneType)
    }
}

/// Evaluated REPO.bazel (skyframe/RepoFileFunction.java:65-181): package
/// defaults (converted only when `process_args`, like RepoPackageArgsFunction)
/// and `ignore_directories()` patterns.
struct RepoFileValue {
    args: PackageArgs,
    ignored_directories: Vec<String>,
}

fn repo_file_value(
    repositories: &Arc<RepositorySet>,
    repository: &str,
    process_args: bool,
) -> Result<RepoFileValue, String> {
    let display = if repository.is_empty() {
        "the main repo".to_owned()
    } else {
        format!("@@{repository}")
    };
    let Some(path) = crate::workspace::repo_file(repositories.root(repository)?)? else {
        return Ok(RepoFileValue {
            args: PackageArgs::default(),
            ignored_directories: Vec::new(),
        });
    };
    // RepoFileFunction.java:110-116: the IOException message is exactly this.
    let source = std::fs::read_to_string(&path)
        .map_err(|_| format!("error reading REPO.bazel file at {}", path.display()))?;
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(&path.to_string_lossy(), source, &dialect)
        .map_err(|e| format!("error parsing REPO.bazel file for {display}: {e:#}"))?;
    crate::repository_text::validate_source(&ast)
        .map_err(|e| format!("error parsing REPO.bazel file for {display}: {e}"))?;
    let context = RepoFile {
        repositories: repositories.clone(),
        repository: repository.to_owned(),
        called: Cell::new(false),
        ignored_directories: RefCell::new(None),
        process_args,
        args: RefCell::new(PackageArgs::default()),
    };
    let globals = GlobalsBuilder::standard().with(repo_file_globals).build();
    Module::with_temp_heap(|module| {
        let mut evaluator = Evaluator::new(&module);
        evaluator.extra = Some(&context);
        evaluator
            .eval_module(ast, &globals)
            .map(|_| ())
            .map_err(|e| format!("error evaluating REPO.bazel file for {display}: {e:#}"))
    })?;
    Ok(RepoFileValue {
        args: context.args.into_inner(),
        ignored_directories: context.ignored_directories.into_inner().unwrap_or_default(),
    })
}

/// Package defaults from a repository's REPO.bazel `repo()` call
/// (RepoPackageArgsFunction over RepoFileValue).
pub(crate) fn repo_args(
    repositories: &Arc<RepositorySet>,
    repository: &str,
) -> Result<PackageArgs, String> {
    repo_file_value(repositories, repository, true).map(|value| value.args)
}

/// `RepoFileValue.ignoredDirectories()` for IgnoredSubdirectoriesFunction:
/// repo() arguments are not converted (no repository mapping is needed).
pub(crate) fn repo_ignored_directories(
    repositories: &Arc<RepositorySet>,
    repository: &str,
) -> Result<Vec<String>, String> {
    repo_file_value(repositories, repository, false).map(|value| value.ignored_directories)
}

fn repo_file<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a RepoFile> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<RepoFile>())
        .ok_or_else(|| anyhow!("repo() can only be used while evaluating a REPO.bazel file"))
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
/// The globals of BUILD files (BuildGlobals plus the `native` members).
pub(crate) fn build_file_globals() -> Globals {
    let mut globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType])
        .with(rule_globals)
        .with(native_rule_globals)
        .with(package_globals)
        .with(build_only_globals);
    install_build_native_methods(&mut globals);
    crate::json::install(&mut globals);
    crate::rule_declarations::install_build(&mut globals);
    globals.build()
}

pub fn bzl_globals() -> Globals {
    bzl_globals_for(BzlPhase::Build)
}

/// MODULE-loaded extensions and repository definitions retain these bindings
/// through initialization, transitive loads and frozen callback execution.
pub fn module_bzl_globals() -> Globals {
    bzl_globals_for(BzlPhase::Module)
}

/// BUILD-file top-levels shared with `native` are methods bound to a
/// StarlarkNativeModule (StarlarkNativeModule.java:84-90 via
/// StarlarkGlobalsImpl.java:76-78; names from StarlarkNativeModuleApi.java:52-337),
/// so they repr as `<built-in method NAME of native value>`.
fn install_build_native_methods(globals: &mut GlobalsBuilder) {
    const NATIVE_METHODS: [&str; 13] = [
        "glob",
        "existing_rule",
        "existing_rules",
        "package_group",
        "exports_files",
        "package_name",
        "package_default_visibility",
        "repository_name",
        "repo_name",
        "package_relative_label",
        "module_name",
        "module_version",
        "subpackages",
    ];
    let methods = GlobalsBuilder::new()
        .with(|builder| builder.set_builtin_receiver("native"))
        .with(rule_globals)
        .with(package_globals)
        .build();
    globals.frozen_heap().add_reference(methods.heap());
    for (name, value) in methods.iter() {
        if NATIVE_METHODS.contains(&name) {
            globals.set(name, value);
        }
    }
}

fn bzl_globals_for(phase: BzlPhase) -> Globals {
    // `native` members are builtin methods of the native module value
    // (StarlarkNativeModuleApi extends StarlarkValue; BuiltinFunction.java:192-199).
    let mut native = GlobalsBuilder::new()
        .with(|builder| builder.set_builtin_receiver("native"))
        .with(rule_globals)
        .with(native_rule_globals)
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
    crate::starlark_types::install_struct_provider(&mut bzl);
    crate::custom_rule::rule_globals(&mut bzl);
    crate::build_settings::install(&mut bzl);
    crate::rule_declarations::install(&mut bzl);
    crate::template_variable_info::install(&mut bzl);
    crate::run_environment_info::install(&mut bzl);
    crate::instrumented_files::install(&mut bzl);
    package_specification::install(&mut bzl);
    crate::providers::globals(&mut bzl);
    crate::repository_rule::globals(&mut bzl);
    crate::module_extension::globals(&mut bzl);
    crate::symbolic_macro::globals(&mut bzl);
    crate::subrule::globals(&mut bzl);
    crate::json::install(&mut bzl);
    // BazelBuildApiGlobals (Environment.BZL): every .bzl context, never BUILD.
    crate::bzl_visibility::globals(&mut bzl);
    crate::language_modules::install(&mut bzl);
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
    // PackageLookupFunction: a package inside an ignored directory is
    // DELETED_PACKAGE_VALUE, before any BUILD file is looked up.
    let ignored = repositories.ignored_subdirectories(&package.repository)?;
    if ignored.matching_entry(&package.package).is_some() {
        return Err(crate::ignored::deleted_package_error(
            &package.repository,
            &package.package,
        ));
    }
    if let Some(error) = no_build_file(root, package) {
        return Err(error);
    }
    let build = crate::workspace::package_build_file(root, &package.package)?;
    let source = std::fs::read_to_string(&build)
        .map_err(|e| format!("cannot read {}: {e}", build.display()))?;
    evaluate_at(
        repositories,
        &build.to_string_lossy(),
        package,
        source,
        &ignored,
    )
}

/// PackageLookupFunction.explainNoBuildFileValue
/// (skyframe/PackageLookupFunction.java:125-155) in NoSuchPackageException
/// form (packages/NoSuchPackageException.java:66-68): the package directory
/// or its BUILD file does not exist. Other lookup failures keep their
/// native diagnostics.
fn no_build_file(root: &std::path::Path, package: &PackageId) -> Option<String> {
    let directory = root.join(&package.package);
    let missing = match std::fs::metadata(&directory) {
        Ok(metadata) => {
            metadata.is_dir()
                && !directory.join("BUILD.bazel").is_file()
                && !directory.join("BUILD").is_file()
        }
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    if !missing {
        return None;
    }
    let education = "Add a BUILD file to a directory to mark it as a package.";
    Some(if package.repository.is_empty() {
        format!(
            "no such package '{}': BUILD file not found in any of the following directories. {education}\n - {}",
            package.package, package.package
        )
    } else {
        format!(
            "no such package '@@{}//{}': BUILD file not found in directory '{}' of external repository @@{}. {education}",
            package.repository, package.package, package.package, package.repository
        )
    })
}

fn evaluate_at(
    repositories: Arc<RepositorySet>,
    filename: &str,
    package: &PackageId,
    source: String,
    ignored: &crate::ignored::IgnoredSubdirectories,
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
    // Lexer errors (invalid escapes) fail the BUILD file like Bazel's parser.
    crate::repository_text::validate_source(&ast)?;
    crate::guarded_globals::check_universe(&ast)?;
    // Both global environments are immutable: built once per invocation.
    let (globals, bzl_globals) = repositories.build_phase_globals();
    let loaded =
        crate::loading::load_build_modules_in(repositories.clone(), package, &ast, &bzl_globals)?;
    let modules = loaded
        .modules
        .iter()
        .map(|(name, module)| (name.as_str(), module))
        .collect::<HashMap<_, _>>();
    let direct_loads = loaded.direct_loads.clone();
    let load_graph = loaded.load_graph.clone();
    let loader = ReturnFileLoader { modules: &modules };
    let repo_defaults = repo_args(&repositories, &package.repository)?;
    let declarations = Declarations {
        package: package.package.clone(),
        repository: package.repository.clone(),
        root: repositories.root(&package.repository)?.to_path_buf(),
        ignored: ignored.filter_for_directory(&package.package),
        repositories,
        rules: RefCell::new(Vec::new()),
        exports: RefCell::new(BTreeMap::new()),
        recorder: RefCell::new(Recorder::default()),
        transient: Cell::new(false),
        default_visibility: RefCell::new(
            repo_defaults.default_visibility.clone().unwrap_or_default(),
        ),
        defaulted_rules: RefCell::new(BTreeSet::new()),
        package_called: Cell::new(false),
        package_args: RefCell::new(repo_defaults),
        custom: RefCell::new(Vec::new()),
        active_macro: RefCell::new(None),
        macro_names: RefCell::new(BTreeSet::new()),
        declared_names: RefCell::new(BTreeSet::new()),
        group_budget: RefCell::new(HostBudget::default()),
        macro_site: RefCell::new(None),
        calls: call_expressions(&ast),
        glob_io_error: RefCell::new(None),
        glob_symlink_issue: RefCell::new(None),
        globs: RefCell::default(),
    };
    let evaluated = Module::with_temp_heap(|module| {
        let mut evaluator = Evaluator::new(&module);
        evaluator.extra = Some(&declarations);
        evaluator.set_loader(&loader);
        evaluator
            .set_max_callstack_size(128)
            .map_err(|e| e.to_string())?;
        evaluator
            .eval_module(ast, &globals)
            .map_err(|e| format!("{e:#}\n{NATIVE_LOADING_FOOTER}"))?;
        drop(evaluator);
        module
            .freeze_named(starlark::values::FrozenHeapName::User(Box::new(
                filename.to_owned(),
            )))
            .map_err(|e| format!("cannot freeze BUILD module: {e:?}"))
    });
    // PackageFunction.java:551-591: a legacy glob I/O error fails the package
    // first (TargetDefinitionContext.beforeBuild, :822-826); otherwise a
    // Skyframe glob symlink issue does (:567-585).
    let package_display = if package.repository.is_empty() {
        package.package.clone()
    } else {
        format!("@@{}//{}", package.repository, package.package)
    };
    if let Some(error) = declarations.glob_io_error.borrow_mut().take() {
        return Err(format!("no such package '{package_display}': {error}"));
    }
    let frozen = evaluated?;
    if let Some(issue) = declarations.glob_symlink_issue.borrow_mut().take() {
        return Err(crate::glob::symlink_issue_error(&issue, &package_display));
    }
    let mut declared_rules = declarations.rules.into_inner();
    // One registry per package, shared by its Starlark rule targets.
    let top_level_functions: Arc<[starlark::values::OwnedFrozenValue]> = loaded.functions.into();
    for pending in declarations.custom.into_inner() {
        let (implementation, _) = frozen
            .get_any_visibility(&pending.slot)
            .map_err(|e| e.to_string())?;
        declared_rules[pending.index].custom = Some(CustomInvocation {
            implementation,
            subrules: pending
                .subrule_slots
                .iter()
                .map(|slot| {
                    frozen
                        .get_any_visibility(slot)
                        .map(|(value, _)| value)
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<_, _>>()?,
            top_level_functions: top_level_functions.clone(),
            transitions: pending
                .transition_slots
                .iter()
                .map(|(key, slot)| {
                    frozen
                        .get_any_visibility(slot)
                        .map(|(value, _)| (key.clone(), value))
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<_, _>>()?,
            attrs: pending.attrs,
            schema: pending.schema,
            build_setting: pending.build_setting,
            setting_value: None,
            target_platform: crate::platforms::TargetPlatform::Absent,
            toolchains: None,
            exec_groups: None,
            use_auto_exec_groups: false,
            action_exec_groups: Default::default(),
            cpp: Default::default(),
            surface: Default::default(),
            declaration: pending.declaration,
        });
    }
    // RuleClass.checkForDuplicateLabels (packages/RuleClass.java:2080, 2107-2129)
    // reports every rule error; the package is then in error.
    let duplicates = declared_rules
        .iter()
        .flat_map(duplicate_label_errors)
        .collect::<Vec<_>>();
    if !duplicates.is_empty() {
        return Err(duplicates.join("\n"));
    }
    let mut rules = validate_rules(declared_rules).map_err(|e| format!("{filename}: {e}"))?;
    let args = declarations.package_args.into_inner();
    for rule in rules.values_mut() {
        rule.unanalyzed.extend(package_default_gaps(rule, &args));
        rule.package_metadata = effective_package_metadata(rule, &args);
        // Computed defaults are resolved against the final package arguments.
        let testonly = rule.is_test_only(&args);
        let deprecation = rule.deprecation_warning(&args);
        if let Some(custom) = rule.custom.as_mut() {
            custom.declaration.builtin.testonly = testonly;
            custom.declaration.builtin.deprecation = deprecation;
        }
    }
    let default_visibility = declarations.default_visibility.into_inner();
    for label in declarations.defaulted_rules.into_inner() {
        if let Some(rule) = rules.get_mut(&label) {
            // Rule.getDefaultVisibility (packages/Rule.java:694-710): with the
            // DEFAULT_PUBLIC config_setting visibility policy
            // (--incompatible_enforce_config_setting_visibility=true and
            // --incompatible_config_setting_private_default_visibility=false
            // defaults, pkgcache/PackageOptions.java:117-140), a config_setting
            // without explicit visibility is public.
            rule.visibility = if matches!(rule.kind, RuleKind::Native("config_setting")) {
                Visibility::Public
            } else {
                default_visibility.clone()
            };
        }
    }
    let exports = declarations.exports.into_inner();
    Ok(Package {
        rules: rules
            .into_iter()
            .map(|(label, rule)| (label, Arc::new(rule)))
            .collect(),
        exports,
        default_visibility,
        macro_names: declarations.macro_names.into_inner(),
        args,
        build_file: PathBuf::from(filename),
        direct_loads,
        load_graph,
    })
}

/// RuleClass.checkForDuplicateLabels (packages/RuleClass.java:2107-2129) with
/// AggregatingAttributeMapper.checkForDuplicateLabels
/// (packages/AggregatingAttributeMapper.java:243-320): for every LABEL_LIST
/// and LABEL_LIST_DICT attribute in rule-class attribute order, a label that
/// appears twice in the plain value, in one select() branch or in one dict
/// value list is `LOC: Label 'L' is duplicated in the 'A' attribute of rule
/// 'N'` at the rule's location. NODEP label lists and outputs are exempt.
fn duplicate_label_errors(rule: &Rule) -> Vec<String> {
    use crate::custom_rule::ConfigurablePart;
    use crate::native_rules::NativeType;
    fn lists(value: &AttrValue) -> Vec<&[String]> {
        match value {
            AttrValue::Labels(labels) => vec![labels.as_slice()],
            AttrValue::StringListDict(entries) | AttrValue::LabelListDict(entries) => entries
                .iter()
                .map(|(_, labels)| labels.as_slice())
                .collect(),
            _ => Vec::new(),
        }
    }
    /// The plain value, every plain element and every select() branch.
    fn possibilities<'a>(
        plain: Option<&'a AttrValue>,
        parts: Option<&'a Vec<ConfigurablePart>>,
    ) -> Vec<&'a [String]> {
        let mut all = plain.map(lists).unwrap_or_default();
        for part in parts.into_iter().flatten() {
            match part {
                ConfigurablePart::Plain(value) => all.extend(lists(value)),
                ConfigurablePart::Select { branches, .. } => {
                    for value in branches.iter().filter_map(|(_, value)| value.as_ref()) {
                        all.extend(lists(value));
                    }
                }
            }
        }
        all
    }
    let label_list =
        |ty: NativeType| matches!(ty, NativeType::LabelList | NativeType::LabelListDict);
    let mut checked: Vec<(String, Vec<&[String]>)> = Vec::new();
    if let Some(native) = &rule.native
        && let Some(class) = crate::native_rules::rule_class(&native.rule_class)
    {
        for attribute in class
            .attrs
            .iter()
            .filter(|attribute| label_list(attribute.ty))
        {
            checked.push((
                attribute.name.to_owned(),
                possibilities(
                    native.attrs.get(attribute.name),
                    native.configurable.get(attribute.name),
                ),
            ));
        }
    }
    if let Some(custom) = &rule.custom {
        let builtin = &custom.declaration.builtin;
        for attribute in crate::native_rules::starlark_builtins(builtin.kind)
            .attrs()
            .filter(|attribute| label_list(attribute.ty))
        {
            checked.push((
                attribute.name.to_owned(),
                possibilities(
                    builtin.target.attrs.get(attribute.name),
                    builtin.target.configurable.get(attribute.name),
                ),
            ));
        }
        for name in &custom.declaration.attr_order {
            if custom.schema.get(name).is_some_and(|spec| {
                matches!(
                    spec.kind,
                    crate::custom_rule::AttrKind::LabelList
                        | crate::custom_rule::AttrKind::LabelListDict
                )
            }) {
                checked.push((
                    name.clone(),
                    possibilities(
                        custom.attrs.get(name),
                        custom.declaration.configurable.get(name),
                    ),
                ));
            }
        }
    }
    let shown = |text: &str| {
        Label::parse_canonical(text)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or_else(|_| text.to_owned())
    };
    let mut errors = Vec::new();
    for (name, lists) in checked {
        let mut duplicates: Vec<&String> = Vec::new();
        for list in lists {
            let mut seen = BTreeSet::new();
            for label in list {
                if !seen.insert(label) && !duplicates.contains(&label) {
                    duplicates.push(label);
                }
            }
        }
        for label in duplicates {
            let message = format!(
                "Label '{}' is duplicated in the '{name}' attribute of rule '{}'",
                shown(label),
                rule.label.name
            );
            errors.push(match &rule.location {
                Some(location) => format!("{location}: {message}"),
                None => message,
            });
        }
    }
    errors
}

/// Appended to every BUILD evaluation error. Validators compare it exactly
/// (scripts/validate_label_passthrough.py, scripts/validate_string_dict.py).
pub const NATIVE_LOADING_FOOTER: &str = "Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() definitions. select() resolves against modelled config_setting values (docs/CONFIGURATION.md).";

/// Whether `rule`'s class has the attribute `name`, and whether `rule` sets it
/// explicitly.
fn builtin_attribute(rule: &Rule, name: &str) -> (bool, bool) {
    let class = rule
        .native
        .as_ref()
        .and_then(|native| crate::native_rules::rule_class(&native.rule_class));
    let builtins = rule
        .custom
        .as_ref()
        .map(|custom| crate::native_rules::starlark_builtins(custom.declaration.builtin.kind));
    let has = |name: &str| match (&builtins, &class) {
        (Some(builtins), _) => builtins.attribute(name).is_some(),
        (None, Some(class)) => class.attribute(name).is_some(),
        (None, None) => true,
    };
    let explicit = rule
        .native
        .as_ref()
        .is_some_and(|native| native.explicit(name))
        || rule
            .custom
            .as_ref()
            .is_some_and(|custom| custom.declaration.builtin.target.explicit(name));
    (has(name), explicit)
}

/// The effective `package_metadata` of `rule` ([`Rule::package_metadata`]).
/// AttributeProvider's pruning of a package metadata rule's own default
/// (packages/AttributeProvider.java:347-372, RuleClass.isPackageMetadataRule
/// :2404-2425) is unreachable for this attribute: it has a computed default,
/// so the earlier `hasComputedDefault()` branch (:334-340) always applies (and
/// the repository check compares the canonical name to `rules_license`,
/// which a Bzlmod `rules_license+` never equals). rules_license's own
/// macros set `applicable_licenses = []` (rules/license.bzl:132). A metadata
/// rule that receives its own package default is a dependency cycle.
fn effective_package_metadata(rule: &Rule, args: &PackageArgs) -> Vec<String> {
    if matches!(rule.kind, RuleKind::PackageGroup(_)) {
        return Vec::new();
    }
    let (has, explicit) = builtin_attribute(rule, "package_metadata");
    if !has {
        return Vec::new();
    }
    if explicit {
        return rule
            .native
            .as_ref()
            .map(|native| native.labels("package_metadata"))
            .or_else(|| {
                rule.custom
                    .as_ref()
                    .map(|custom| custom.declaration.builtin.target.labels("package_metadata"))
            })
            .unwrap_or_default();
    }
    args.default_package_metadata
        .iter()
        .flatten()
        .map(crate::rule_declarations::canonical_form)
        .collect()
}

/// Computed package defaults that would change a rule's analysis in Bazel
/// (RuleClass.java:286-296 default environments). Explicit values win.
/// `default_testonly`/`default_deprecation` need no gap: the testonly check and
/// the deprecation warning use the effective values (crate::engine), and
/// `default_package_metadata` is analyzed ([`effective_package_metadata`]).
fn package_default_gaps(rule: &Rule, args: &PackageArgs) -> Vec<String> {
    if matches!(rule.kind, RuleKind::PackageGroup(_)) {
        return Vec::new();
    }
    let mut gaps = Vec::new();
    for (attribute, value) in [
        ("compatible_with", &args.default_compatible_with),
        ("restricted_to", &args.default_restricted_to),
    ] {
        let (has, explicit) = builtin_attribute(rule, attribute);
        if value.as_ref().is_some_and(|labels| !labels.is_empty()) && has && !explicit {
            gaps.push(format!(
                "package default_{attribute} (constraint environments)"
            ));
        }
    }
    if args.transitive_visibility.is_some() {
        gaps.push("package transitive_visibility".to_owned());
    }
    gaps
}

#[cfg(test)]
fn evaluate_source(
    filename: &str,
    package: &str,
    source: String,
) -> Result<BTreeMap<Label, Rule>, String> {
    let repositories = Arc::new(RepositorySet::new(PathBuf::from(".")));
    let ignored = repositories.ignored_subdirectories("")?;
    evaluate_at(
        repositories,
        filename,
        &PackageId {
            repository: String::new(),
            package: package.to_owned(),
        },
        source,
        &ignored,
    )
    .map(|package| {
        package
            .rules
            .into_iter()
            .map(|(label, rule)| (label, Arc::unwrap_or_clone(rule)))
            .collect()
    })
}

/// Where and as what a target is declared (TargetRecorder conflict checks).
struct TargetSite {
    /// The called function, for Bazel's `Error in F:` frame.
    callee: String,
    /// `Target.getTargetKind()`: `CLASS rule` or `package group`.
    kind: String,
    /// The call's location (`/abs/BUILD:L:C`).
    location: Option<String>,
    /// Rule.getLabels beyond `srcs` (select() keys/branches, built-ins).
    inputs: Vec<Label>,
}

/// A package target already recorded (TargetRecorder's targetMap).
#[derive(Debug)]
enum Recorded {
    Rule { kind: String, location: String },
    Output { rule: String, location: String },
    Source { location: String },
}

impl Recorded {
    /// `existing.getTargetKind()` or `generated file from rule 'R'`, then
    /// `, defined at LOCATION` (TargetRecorder.checkForExistingTargetName,
    /// TargetRecorder.java:693-715).
    fn describe(&self) -> String {
        match self {
            Self::Rule { kind, location } => format!("{kind}, defined at {location}"),
            Self::Output { rule, location } => {
                format!("generated file from rule '{rule}', defined at {location}")
            }
            Self::Source { location } => format!("source file, defined at {location}"),
        }
    }
}

/// TargetRecorder's name-conflict state (packages/TargetRecorder.java): the
/// targets declared so far, and output-file path prefixes.
#[derive(Debug, Default)]
pub struct Recorder {
    targets: BTreeMap<String, Recorded>,
    /// Output-file directory prefixes with the (output, rule) recording them.
    prefixes: BTreeMap<String, (String, String)>,
}

const UNKNOWN_LOCATION: &str = "<unknown location>";

impl Recorder {
    /// TargetRecorder.checkRuleAndOutputs then addRuleInternal
    /// (TargetRecorder.java:540-600): the rule name, each output (duplicates
    /// in the rule, earlier targets, path-prefix overlaps), then same-package
    /// input names that are also outputs.
    fn record_rule(
        &mut self,
        name: &str,
        kind: &str,
        location: Option<&str>,
        outs: &[String],
        inputs: &[String],
    ) -> Result<(), String> {
        let location = location.unwrap_or(UNKNOWN_LOCATION);
        if let Some(existing) = self.targets.get(name) {
            return Err(format!(
                "{kind} '{name}' conflicts with existing {}",
                existing.describe()
            ));
        }
        let mut own: Vec<&str> = Vec::new();
        let mut prefixes = Vec::new();
        for out in outs {
            let out = out.as_str();
            if own.contains(&out) {
                return Err(format!(
                    "rule '{name}' has more than one generated file named '{out}'"
                ));
            }
            own.push(out);
            if let Some(existing) = self.targets.get(out) {
                return Err(format!(
                    "generated file '{out}' in rule '{name}' conflicts with existing {}",
                    existing.describe()
                ));
            }
            let overlap = |existing: &str, owner: &str| {
                if owner == name {
                    format!("rule '{name}' has conflicting output files '{out}' and '{existing}'")
                } else {
                    format!(
                        "output file '{out}' of rule '{name}' conflicts with output file '{existing}' of rule '{owner}'"
                    )
                }
            };
            if let Some((existing, owner)) = self.prefixes.get(out).or_else(|| {
                prefixes
                    .iter()
                    .find(|(prefix, _)| prefix == out)
                    .map(|(_, entry)| entry)
            }) {
                return Err(overlap(existing, owner));
            }
            for (slash, _) in out.match_indices('/') {
                let prefix = &out[..slash];
                if own.contains(&prefix) {
                    return Err(overlap(prefix, name));
                }
                if let Some(Recorded::Output { rule, .. }) = self.targets.get(prefix) {
                    return Err(overlap(prefix, rule));
                }
                if !self.prefixes.contains_key(prefix)
                    && !prefixes.iter().any(|(existing, _)| existing == prefix)
                {
                    prefixes.push((prefix.to_owned(), (out.to_owned(), name.to_owned())));
                }
            }
        }
        for input in inputs {
            if own.contains(&input.as_str()) {
                return Err(format!(
                    "rule '{name}' has file '{input}' as both an input and an output"
                ));
            }
        }
        if own.contains(&name) {
            // Rule.validateOutputLabel only warns (packages/Rule.java:638-644)
            // and the generated file then shadows the rule target.
            return Err(format!(
                "rule '{name}' declares an output file named like the rule itself: genuine Bazel 9.3 warns (target '{name}' is both a rule and a file; please choose another name for the rule) and lets the generated file replace the rule target, which the native profile does not model"
            ));
        }
        self.prefixes.extend(prefixes);
        self.targets.insert(
            name.to_owned(),
            Recorded::Rule {
                kind: kind.to_owned(),
                location: location.to_owned(),
            },
        );
        for out in own {
            self.targets.insert(
                out.to_owned(),
                Recorded::Output {
                    rule: name.to_owned(),
                    location: location.to_owned(),
                },
            );
        }
        Ok(())
    }

    /// TargetDefinitionContext.createInputFile (packages/TargetDefinitionContext.java:672-689).
    fn record_source(&mut self, name: &str, location: Option<&str>) -> Result<(), String> {
        match self.targets.get(name) {
            Some(Recorded::Source { .. }) => Ok(()),
            Some(existing) => Err(format!(
                "source file '{name}' conflicts with existing {}",
                existing.describe()
            )),
            None => {
                self.targets.insert(
                    name.to_owned(),
                    Recorded::Source {
                        location: location.unwrap_or(UNKNOWN_LOCATION).to_owned(),
                    },
                );
                Ok(())
            }
        }
    }
}

/// The package's rules by label; conflicts were rejected at each declaring
/// call ([`Recorder`]).
fn validate_rules(declarations: Vec<Rule>) -> Result<BTreeMap<Label, Rule>, String> {
    let mut rules = BTreeMap::new();
    for rule in declarations {
        if rules.contains_key(&rule.label) {
            return Err(format!("duplicate rule name: {}", rule.label));
        }
        rules.insert(rule.label.clone(), rule);
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_is_installed_in_build_without_unrelated_bzl_extensions() {
        let rules = eval(r#"
(type(json) == 'json' and dir(json) == ['decode', 'encode', 'encode_indent', 'indent']) or fail('json surface')
(json.encode(json.decode('{"z":0,"a":[1]}')) == '{"a":[1],"z":0}') or fail('json round trip')
(json.indent('{}') == '{}' and json.encode_indent({}) == '{}') or fail('json indentation')
filegroup(name='json', srcs=json.decode('["a.txt","b.txt"]'))
"#).unwrap();
        assert_eq!(
            rules[&Label::parse(":json", "pkg").unwrap()].srcs,
            ["//pkg:a.txt", "//pkg:b.txt"]
        );
        rejected("x = struct()", "struct");
        rejected("print('x')", "print");
        rejected("x = native.json", "native");
        let error = crate::bzlmod::evaluate_source("MODULE.bazel", "x = json.decode('{}')".into())
            .unwrap_err();
        assert!(error.contains("json"), "{error}");
    }

    #[test]
    fn json_in_both_bzl_phases_survives_transitive_load_and_frozen_callback() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/BUILD"), "").unwrap();
        std::fs::write(
            root.0.join("pkg/base.bzl"),
            "JSON = json\nENCODE = json.encode\n",
        )
        .unwrap();
        std::fs::write(root.0.join("pkg/defs.bzl"), r#"
load(':base.bzl', 'JSON', 'ENCODE')
(type(JSON) == 'json' and dir(JSON) == ['decode', 'encode', 'encode_indent', 'indent']) or fail('json surface')
not hasattr(native, 'json') or fail('native.json')
def callback():
    result = JSON.decode('{"z":0,"a":[1]}')
    result['callback'] = True
    return ENCODE(result)
"#).unwrap();
        for globals in [bzl_globals(), module_bzl_globals()] {
            let ast = AstModule::parse(
                "pkg/BUILD",
                "load(':defs.bzl', 'callback')".into(),
                &Dialect::Standard,
            )
            .unwrap();
            let loaded = crate::loading::load_modules(&root.0, "pkg", &ast, &globals).unwrap();
            let callback = loaded[":defs.bzl"].get("callback").unwrap();
            drop(loaded);
            Module::with_temp_heap(|module| {
                let result = Evaluator::new(&module)
                    .eval_function(module.heap().access_owned_frozen_value(&callback), &[], &[])
                    .unwrap();
                assert_eq!(
                    result.unpack_str(),
                    Some("{\"a\":[1],\"callback\":true,\"z\":0}")
                );
            });
        }
    }

    #[test]
    fn sets_in_build_and_loaded_bzl_keep_order_aliases_and_sequence_boundaries() {
        let package = custom_fixture(
            "Set = set\nBASE = Set(['b.txt', 'a.txt', 'b.txt'])\ndef files():\n    result = set(BASE)\n    alias = result\n    result |= set(['c.txt'])\n    (list(alias) == ['b.txt', 'a.txt', 'c.txt'] and list(BASE) == ['b.txt', 'a.txt']) or fail('aliases')\n    return list(result)\n",
            "load(':defs.bzl', 'Set', 'BASE', 'files')\n(Set == set and type(Set) == 'builtin_function_or_method') or fail('constructor')\nfilegroup(name='direct', srcs=list(set(['b.txt', 'a.txt', 'b.txt'])))\nfilegroup(name='loaded', srcs=files())",
        ).unwrap();
        assert_eq!(
            package[&Label::parse(":direct", "pkg").unwrap()].srcs,
            ["//pkg:b.txt", "//pkg:a.txt"]
        );
        assert_eq!(
            package[&Label::parse(":loaded", "pkg").unwrap()].srcs,
            ["//pkg:b.txt", "//pkg:a.txt", "//pkg:c.txt"]
        );
        // Bazel's ListType.convert accepts any Iterable, including sets.
        let rules = eval("filegroup(name='x', srcs=set(['a.txt']))").unwrap();
        assert_eq!(
            rules[&Label::parse(":x", "pkg").unwrap()].srcs,
            ["//pkg:a.txt"]
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
                byte_strings: false,
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
        assert_eq!(generated.srcs, ["//pkg:inputs"]);
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
            ["//pkg:b.txt"]
        );
    }

    #[test]
    fn attributes_are_strictly_checked() {
        // Bazel 9.3 schemas (native_rules.rs): common attributes load, but
        // analysis-relevant ones are recorded as unanalyzed. testonly only
        // drives the prerequisite check the engine runs.
        let rules = eval("filegroup(name='x', testonly=True, tags=['manual'])\ngenrule(name='g', outs=['y'], cmd='true', tools=[':x'], executable=True)\ngenrule(name='n', cmd='true', outs=[])\ngenrule(name='o', outs=['z'])").unwrap();
        let x = &rules[&Label::parse(":x", "pkg").unwrap()];
        assert!(x.is_manual());
        assert!(x.is_test_only(&PackageArgs::default()));
        assert!(x.unanalyzed.is_empty(), "{:?}", x.unanalyzed);
        let g = &rules[&Label::parse(":g", "pkg").unwrap()];
        // Value gaps are evaluated on the configured target (crate::engine).
        assert!(g.unanalyzed.is_empty(), "{:?}", g.unanalyzed);
        let gaps = crate::native_rules::analysis_gaps(g.native.as_ref().unwrap());
        // `tools` is analyzed (the exec transition); `executable` is a gap.
        assert_eq!(gaps, ["attribute 'executable' = True"]);
        assert!(rules[&Label::parse(":n", "pkg").unwrap()].outs.is_empty());
        assert!(rules[&Label::parse(":o", "pkg").unwrap()].cmd.is_none());
        for (source, expected) in [
            ("filegroup(name='x', **{'tags': []})", "*args and **kwargs"),
            (
                "genrule(name='x', cmd='true')",
                "missing value for mandatory attribute 'outs' in 'genrule' rule",
            ),
            ("filegroup('x')", "unexpected positional arguments"),
            (
                "filegroup(name=42)",
                "filegroup 'name' attribute must be a string",
            ),
            ("filegroup()", "filegroup rule has no 'name' attribute"),
            (
                "genrule(name='x', outs=['y'], cmd=42)",
                "//pkg:x: expected value of type 'string' for attribute 'cmd' of 'genrule', but got 42 (int)",
            ),
            (
                "filegroup(name='x', bogus=None)",
                "//pkg:x: no such attribute 'bogus' in 'filegroup' rule",
            ),
            (
                "filegroup(name='x', src=[])",
                "no such attribute 'src' in 'filegroup' rule (did you mean 'srcs'?)",
            ),
            (
                "genrule(name='x', outs=select({'//conditions:default': ['y']}), cmd='true')",
                "//pkg:x: attribute \"outs\" is not configurable",
            ),
            (
                "filegroup(name='x', tags=select({'//conditions:default': []}))",
                "attribute \"tags\" is not configurable",
            ),
        ] {
            rejected(source, expected);
        }
        for value in ["'x'", "1", "True"] {
            rejected(
                &format!("filegroup(name='x', srcs={value})"),
                "expected value of type 'list(label)' for attribute 'srcs' of 'filegroup'",
            );
            rejected(
                &format!("genrule(name='x', outs={value}, cmd='true')"),
                "expected value of type 'list(output)' for attribute 'outs' of 'genrule'",
            );
        }
        rejected(
            "filegroup(name='x', srcs=[1])",
            "expected value of type 'string' for element 0 of attribute 'srcs' of 'filegroup', but got 1 (int)",
        );
        // None is an unset value; tuples are sequences.
        let rules =
            eval("filegroup(name='x', srcs=None)\nfilegroup(name='y', srcs=('a',))").unwrap();
        assert!(rules[&Label::parse(":x", "pkg").unwrap()].srcs.is_empty());
        assert_eq!(rules[&Label::parse(":y", "pkg").unwrap()].srcs, ["//pkg:a"]);
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
            "invalid label '../escape' in element 0 of attribute 'srcs' of 'filegroup'",
        );
    }

    #[test]
    fn duplicate_and_conflicting_declarations_fail() {
        // TargetRecorder texts (packages/TargetRecorder.java:540-600,
        // 693-715, 785-805); genuine Bazel 9.3 capture in
        // tests/fixtures/implicit_outputs/reference_receipt.json.
        rejected(
            "filegroup(name='x')\nfilegroup(name='x')",
            "filegroup rule 'x' conflicts with existing filegroup rule, defined at ",
        );
        rejected(
            "genrule(name='x', outs=['y', 'y'], cmd='true')",
            "rule 'x' has more than one generated file named 'y'",
        );
        rejected(
            "genrule(name='x', outs=['z'], cmd='true')\ngenrule(name='y', outs=['z'], cmd='true')",
            "generated file 'z' in rule 'y' conflicts with existing generated file from rule 'x', defined at ",
        );
        rejected(
            "genrule(name='x', outs=['x'], cmd='true')",
            "rule 'x' declares an output file named like the rule itself",
        );
        rejected(
            "filegroup(name='out')\ngenrule(name='x', outs=['out'], cmd='true')",
            "generated file 'out' in rule 'x' conflicts with existing filegroup rule, defined at ",
        );
        rejected(
            "genrule(name='x', outs=['out'], cmd='true')\nfilegroup(name='out')",
            "filegroup rule 'out' conflicts with existing generated file from rule 'x', defined at ",
        );
        rejected(
            "genrule(name='x', outs=['a', 'a/b'], cmd='true')",
            "rule 'x' has conflicting output files 'a/b' and 'a'",
        );
        rejected(
            "genrule(name='x', outs=['a/b'], cmd='true')\ngenrule(name='y', outs=['a'], cmd='true')",
            "output file 'a' of rule 'y' conflicts with output file 'a/b' of rule 'x'",
        );
        rejected(
            "genrule(name='x', srcs=['in'], outs=['in'], cmd='true')",
            "rule 'x' has file 'in' as both an input and an output",
        );
        rejected(
            "exports_files(['e'])\ngenrule(name='x', outs=['e'], cmd='true')",
            "Error in genrule: generated file 'e' in rule 'x' conflicts with existing source file, defined at ",
        );
        // createInputFile -> addTarget (TargetDefinitionContext.java:672-689).
        rejected(
            "genrule(name='x', outs=['e'], cmd='true')\nexports_files(['e'])",
            "Error in exports_files: source file 'e' conflicts with existing generated file from rule 'x', defined at ",
        );
        rejected(
            "filegroup(name='f')\nexports_files(['f'])",
            "Error in exports_files: source file 'f' conflicts with existing filegroup rule, defined at ",
        );
        eval("exports_files(['e'])\nexports_files(['e'])").unwrap();
        // The conflict is thrown at the declaring call: evaluation stops there.
        let error = eval("filegroup(name='x')\nfilegroup(name='x')\nfail('later')").unwrap_err();
        assert!(
            error.contains(
                "Error in filegroup: filegroup rule 'x' conflicts with existing filegroup rule"
            ) && !error.contains("later"),
            "{error}"
        );
    }

    #[test]
    fn loading_and_unsupported_bazel_features_fail() {
        rejected(
            "load('@repo//:defs.bzl', 'my_rule')\nmy_rule(name='x')",
            "external repository",
        );
        rejected("native.filegroup(name='x')", "native");
        rejected(
            "cc_binary(name='x')",
            "This rule has been removed from Bazel. Please add a `load()` statement for it.",
        );
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
        // RuleVisibility.validateAndSimplify (RuleVisibility.java:185-215):
        // public subsumes, private is dropped next to other labels.
        for (value, public) in [
            ("['//visibility:public', '//visibility:private']", true),
            ("['//visibility:public', '//visibility:public']", true),
            ("['//other:__pkg__', '//visibility:public']", true),
            ("['//visibility:private', '//other:__pkg__']", false),
        ] {
            let rules = eval(&format!("filegroup(name='x', visibility={value})")).unwrap();
            let visibility = &rules[&Label::parse(":x", "pkg").unwrap()].visibility;
            assert_eq!(*visibility == Visibility::Public, public, "{value}");
            if !public {
                assert_eq!(
                    visibility.declared_labels(),
                    [Label::parse("//other:__pkg__", "").unwrap()]
                );
            }
        }
        rejected(
            "filegroup(name='x', visibility=['//visibility:pubic'])",
            "//pkg:x Invalid visibility label '//visibility:pubic'; did you mean //visibility:public or //visibility:private?",
        );
        let rules = eval("package(default_visibility=['//visibility:public', '//go:__subpackages__'])\nfilegroup(name='x')\n([str(v) for v in package_default_visibility()] == ['@@//visibility:public']) or fail('pdv')").unwrap();
        assert_eq!(
            rules[&Label::parse(":x", "pkg").unwrap()].visibility,
            Visibility::Public
        );
        for value in ["'//visibility:public'", "[1]"] {
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
        assert_eq!(rule.srcs, ["//src/docker:config.json"]);
    }

    #[test]
    fn records_owned_attribute_snapshots() {
        let rules = eval("s = ['a']\nfilegroup(name='x', srcs=s)\ns.append('b')").unwrap();
        assert_eq!(rules[&Label::parse(":x", "pkg").unwrap()].srcs, ["//pkg:a"]);
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
                    error.contains("collides") || error.contains("conflicts with existing"),
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
        // StarlarkFunction.java:385-397 for the implementation call.
        symbolic_error(
            "def impl():\n    pass\nm = macro(implementation=impl)",
            "m(name='x')",
            "impl() got unexpected keyword arguments: name, visibility",
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
            "trying to mutate a frozen list value",
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
        let rules =
            eval("filegroup(name='x', visibility=('//visibility:public', '//visibility:private'))")
                .unwrap();
        assert_eq!(
            rules[&Label::parse(":x", "pkg").unwrap()].visibility,
            Visibility::Public
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
                byte_strings: false,
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
                ignored: Default::default(),
                rules: RefCell::new(Vec::new()),
                exports: RefCell::new(BTreeMap::new()),
                recorder: RefCell::new(Recorder::default()),
                transient: Cell::new(false),
                default_visibility: RefCell::new(Visibility::Private),
                defaulted_rules: RefCell::new(BTreeSet::new()),
                package_called: Cell::new(false),
                package_args: RefCell::new(PackageArgs::default()),
                custom: RefCell::new(Vec::new()),
                active_macro: RefCell::new(None),
                macro_names: RefCell::new(BTreeSet::new()),
                declared_names: RefCell::new(BTreeSet::new()),
                group_budget: RefCell::new(HostBudget::default()),
                macro_site: RefCell::new(None),
                calls: HashMap::new(),
                glob_io_error: RefCell::new(None),
                glob_symlink_issue: RefCell::new(None),
                globs: RefCell::default(),
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
                    .with(native_rule_globals)
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
            // Type.BOOLEAN.convert texts verified against genuine Bazel 9.3.0.
            (
                "bool",
                ", build_setting_default=2",
                "//pkg:x: expected one of [False, True, 0, 1] for attribute 'build_setting_default' of 'r', but got 2 (int)",
            ),
            (
                "bool",
                ", build_setting_default=-1",
                "//pkg:x: expected one of [False, True, 0, 1] for attribute 'build_setting_default' of 'r', but got -1 (int)",
            ),
            (
                "bool",
                ", build_setting_default='true'",
                "//pkg:x: expected one of [False, True, 0, 1] for attribute 'build_setting_default' of 'r', but got \"true\" (string)",
            ),
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
        // Ordinary bool attributes are Type.BOOLEAN too: genuine Bazel 9.3.0
        // accepts 0 and 1 and rejects 2 with the same conversion text.
        for (setting, attribute, extra) in [
            ("None", "build_setting_default", ""),
            (
                "config.bool()",
                "ordinary_bool",
                ", build_setting_default=True",
            ),
        ] {
            let defs = format!(
                "def impl(ctx):\n    return []\nr = rule(implementation=impl, build_setting={setting}, attrs={{'{attribute}': attr.bool()}})"
            );
            let build = format!(
                "load(':defs.bzl', 'r')\nr(name='x', {attribute}=0{extra})\nr(name='y', {attribute}=1{extra})"
            );
            let package = custom_fixture(&defs, &build).unwrap();
            for (name, value) in [("x", false), ("y", true)] {
                let custom = package[&Label::parse(&format!(":{name}"), "pkg").unwrap()]
                    .custom
                    .as_ref()
                    .unwrap();
                assert_eq!(custom.attrs[attribute], AttrValue::Bool(value), "{name}");
            }
            let build = format!("load(':defs.bzl', 'r')\nr(name='x', {attribute}=2{extra})");
            let error = custom_fixture(&defs, &build).unwrap_err();
            assert!(
                error.contains(&format!(
                    "//pkg:x: expected one of [False, True, 0, 1] for attribute '{attribute}' of 'r', but got 2 (int)"
                )),
                "{error}"
            );
        }
        let defs = "def impl(ctx):\n    return []\nr = rule(implementation=impl, attrs={'ordinary_bool': attr.bool()})";
        custom_fixture(defs, "load(':defs.bzl', 'r')\nr(name='x', ordinary_bool=1)").unwrap();
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
            (
                "r(name='x', s='', b=2)",
                "//pkg:x: expected one of [False, True, 0, 1] for attribute 'b' of 'r', but got 2 (int)",
            ),
            (
                "r(name='x', s='', b=select({'//conditions:default': 2}))",
                "//pkg:x: expected one of [False, True, 0, 1] for each branch in select expression of attribute 'b' of 'r' (including '//conditions:default'), but got 2 (int)",
            ),
            (
                "r(name='x', s='', b='true')",
                "//pkg:x: expected one of [False, True, 0, 1] for attribute 'b' of 'r', but got \"true\" (string)",
            ),
            ("r(name='x', s='', dep=1)", "string"),
            ("r(name='x', s='', deps=[1])", "list of strings"),
            ("r(name='x', s='', out='../escape')", "output path"),
            ("r(name='x', s='', outputs=[1])", "list of strings"),
            (
                "r(name='x', s='', unknown=1)",
                "//pkg:x: no such attribute 'unknown' in 'r' rule",
            ),
            (
                "r(name='x', s='', sss=[])",
                "//pkg:x: no such attribute 'sss' in 'r' rule (did you mean 'ss'?)",
            ),
            (
                "r(name='x', s='', licenses=['notice'])",
                "//pkg:x: no such attribute 'licenses' in 'r' rule",
            ),
            (
                "r(name='x', s='', size='small')",
                "//pkg:x: no such attribute 'size' in 'r' rule",
            ),
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
            (
                "r(name='x')\nr(name='x')",
                "r rule 'x' conflicts with existing r rule",
            ),
            ("r(name='x', out='x')", "named like the rule itself"),
            (
                "r(name='x', out='z')\nr(name='y', out='z')",
                "generated file 'z' in rule 'y' conflicts with existing generated file from rule 'x'",
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
    fn identical_globs_walk_once_per_package_evaluation_with_identical_results() {
        let root = TempRoot::new();
        for name in ["a.txt", "b.txt", "sub/c.txt", "x.in"] {
            let path = root.0.join("pkg").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, name).unwrap();
        }
        std::fs::write(
            root.0.join("pkg/BUILD.bazel"),
            "filegroup(name='a', srcs=glob(['**/*.txt']))\nfilegroup(name='b', srcs=glob(['**/*.txt']))\nfilegroup(name='c', srcs=glob(['**/*.txt'], exclude=['b.txt']))\nfilegroup(name='d', srcs=glob(['**/*.txt'], allow_empty=True))\nfilegroup(name='e', srcs=glob(['*.in']) + glob(['**/*.txt']))\n",
        )
        .unwrap();
        let walks = GLOB_WALKS.with(std::cell::Cell::get);
        let package = evaluate(&root.0, "pkg").unwrap();
        // Distinct (include, exclude, exclude_directories, allow_empty)
        // arguments walk; repeated ones reuse the package's outcome.
        assert_eq!(GLOB_WALKS.with(std::cell::Cell::get) - walks, 4);
        let srcs = |name: &str| {
            package[&Label::parse(name, "pkg").unwrap()]
                .srcs
                .iter()
                .map(|src| src.strip_prefix("//pkg:").unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let all = ["a.txt", "b.txt", "sub/c.txt"].map(str::to_owned).to_vec();
        assert_eq!(srcs(":a"), all);
        assert_eq!(srcs(":b"), all);
        assert_eq!(srcs(":c"), ["a.txt", "sub/c.txt"]);
        assert_eq!(srcs(":d"), all);
        assert_eq!(srcs(":e"), ["x.in", "a.txt", "b.txt", "sub/c.txt"]);
        // Each evaluation has its own cache.
        evaluate(&root.0, "pkg").unwrap();
        assert_eq!(GLOB_WALKS.with(std::cell::Cell::get) - walks, 8);
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
        // One shared registry per package, not a copy per target.
        assert!(Arc::ptr_eq(
            &first.top_level_functions,
            &second.top_level_functions
        ));
        assert!(
            first
                .top_level_functions
                .iter()
                .any(|f| f.value().ptr_eq(first.implementation.value()))
        );
        for (a, b) in first
            .top_level_functions
            .iter()
            .zip(second.top_level_functions.iter())
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
        assert_eq!(native.srcs, ["@@dep+//pkg:input"]);
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
            let key = argument.split('=').next().unwrap();
            // Private ($x) attributes are UNDOCUMENTED: never suggested
            // (Attribute.java:444-449).
            assert!(
                error.contains(&format!("//pkg:x: no such attribute '{key}' in 'r' rule"))
                    && !error.contains("did you mean '$"),
                "{error}"
            );
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

    fn native_of<'a>(rules: &'a BTreeMap<Label, Rule>, name: &str) -> &'a NativeTarget {
        rules[&Label::parse(&format!(":{name}"), "pkg").unwrap()]
            .native
            .as_ref()
            .unwrap()
    }

    #[test]
    fn native_rule_classes_register_with_kinds_defaults_and_selects() {
        let rules = eval(
            r#"
alias(name='a', actual=select({':linux': ':x', '//conditions:default': ':y'}))
alias(name='simplified', actual=select({'//conditions:default': ':x'}))
config_setting(name='linux', values={'cpu': 'k8'}, define_values={'a': 'b'}, flag_values={':flag': 'on'}, constraint_values=['//os:linux'])
config_setting(name='empty')
toolchain_type(name='tt', no_match_error='none')
toolchain(name='t', toolchain_type=':tt', toolchain=':impl', target_settings=select({':linux': [':linux'], '//conditions:default': []}), exec_compatible_with=['//os:linux'], use_target_platform_constraints=False)
constraint_setting(name='cs', default_constraint_value=':cv')
constraint_value(name='cv', constraint_setting=':cs')
platform(name='p', constraint_values=[':cv'], parents=[], exec_properties={'k': 'v'}, flags=['--x'], required_settings=[':linux'], missing_toolchain_error='m')
label_flag(name='flag', build_setting_default=':x')
label_setting(name='setting', build_setting_default=':x', scope='target')
genquery(name='q', expression='deps(:x)', scope=[':x'])
starlark_doc_extract(name='doc', src='defs.bzl')
test_suite(name='suite', tests=[':a'])
java_plugins_flag_alias(name='java_plugins_flag_alias')
cc_libc_top_alias(name='libc')
cc_toolchain_suite('suite_fg', toolchains={'k8': ':cc'}, tags=['manual'], visibility=['//visibility:public'])
filegroup(name='fg', srcs=['a'] + select({':linux': ['b'], '//conditions:default': None}))
genrule(name='g', outs=['g.out'], cmd=select({':linux': 'a', '//conditions:default': 'b'}))
"#,
        )
        .unwrap();
        let kind = |name: &str| {
            rules[&Label::parse(&format!(":{name}"), "pkg").unwrap()]
                .kind
                .class_name()
                .to_owned()
        };
        for (name, expected) in [
            ("a", "alias"),
            ("linux", "config_setting"),
            ("tt", "toolchain_type"),
            ("t", "toolchain"),
            ("cs", "constraint_setting"),
            ("cv", "constraint_value"),
            ("p", "platform"),
            ("flag", "label_flag"),
            ("setting", "label_setting"),
            ("q", "genquery"),
            ("doc", "starlark_doc_extract"),
            ("suite", "test_suite"),
            ("java_plugins_flag_alias", "java_plugins_flag_alias"),
            ("libc", "cc_libc_top_alias"),
            ("suite_fg", "filegroup"),
            ("fg", "filegroup"),
            ("g", "genrule"),
        ] {
            assert_eq!(kind(name), expected, "{name}");
        }
        // select() is retained where configurable; an unconditional select is
        // simplified (BuildType.java:165-211).
        assert!(native_of(&rules, "a").configurable.contains_key("actual"));
        assert_eq!(
            native_of(&rules, "simplified").attrs["actual"],
            AttrValue::Label(Some("//pkg:x".into()))
        );
        assert!(
            native_of(&rules, "t")
                .configurable
                .contains_key("target_settings")
        );
        // Default tags: manual for config/platform/toolchain classes only.
        for name in ["linux", "t", "cs", "cv", "p"] {
            assert!(
                rules[&Label::parse(&format!(":{name}"), "pkg").unwrap()].is_manual(),
                "{name}"
            );
        }
        for name in ["a", "tt", "flag", "q", "suite_fg"] {
            assert!(
                !rules[&Label::parse(&format!(":{name}"), "pkg").unwrap()].is_manual(),
                "{name}"
            );
        }
        // The cc_toolchain_suite lambda forwards only the name.
        let suite = &rules[&Label::parse(":suite_fg", "pkg").unwrap()];
        assert_eq!(suite.visibility, Visibility::Private);
        assert!(suite.native.as_ref().unwrap().attrs.is_empty());
        assert!(suite.unanalyzed.is_empty());
        assert_eq!(
            rules[&Label::parse(":doc", "pkg").unwrap()].outs,
            ["doc.binaryproto", "doc.textproto"]
        );
        // Select branch and key labels are assumed inputs; filegroup/genrule
        // selects are retained for configured resolution, not gaps.
        let fg = &rules[&Label::parse(":fg", "pkg").unwrap()];
        assert!(fg.srcs.is_empty());
        assert_eq!(
            fg.native.as_ref().unwrap().input_labels,
            [
                Label::parse(":a", "pkg").unwrap(),
                Label::parse(":linux", "pkg").unwrap(),
                Label::parse(":b", "pkg").unwrap()
            ]
        );
        assert!(fg.unanalyzed.is_empty());
        assert!(
            fg.native
                .as_ref()
                .unwrap()
                .configurable
                .contains_key("srcs")
        );
        let g = &rules[&Label::parse(":g", "pkg").unwrap()];
        assert!(g.unanalyzed.is_empty() && g.cmd.is_none());
        assert!(g.native.as_ref().unwrap().configurable.contains_key("cmd"));
        assert!(native_of(&rules, "empty").attrs.is_empty());
    }

    #[test]
    fn native_rule_load_errors_use_bazel_messages() {
        for (source, expected) in [
            (
                "alias(name='a', acutal=':b')",
                "//pkg:a: no such attribute 'acutal' in 'alias' rule (did you mean 'actual'?)",
            ),
            (
                "alias(name='a', licenses=['notice'], actual=':b')",
                "//pkg:a: no such attribute 'licenses' in 'alias' rule",
            ),
            (
                "alias(name='a')",
                "//pkg:a: missing value for mandatory attribute 'actual' in 'alias' rule",
            ),
            (
                "alias(name='a', actual=None)",
                "missing value for mandatory attribute 'actual'",
            ),
            (
                "alias(name='a', actual=1)",
                "//pkg:a: expected value of type 'string' for attribute 'actual' of 'alias', but got 1 (int)",
            ),
            ("alias('a', actual=':b')", "unexpected positional arguments"),
            (
                "alias(name='a', actual=select({':x': ':y'}) + select({':z': ':w'}))",
                "//pkg:a: type 'label' doesn't support select concatenation",
            ),
            (
                "toolchain(name='t', toolchain=':i', toolchain_type=select({'//conditions:default': ':tt'}))",
                "//pkg:t: attribute \"toolchain_type\" is not configurable",
            ),
            (
                "toolchain(name='t', toolchain=':i', toolchain_type=':tt', exec_compatible_with=select({'//conditions:default': []}))",
                "attribute \"exec_compatible_with\" is not configurable",
            ),
            (
                "toolchain(name='t', toolchain=':i', toolchain_type=':tt', compatible_with=[])",
                "no such attribute 'compatible_with' in 'toolchain' rule",
            ),
            (
                "config_setting(name='c', values=select({'//conditions:default': {}}))",
                "//pkg:c: attribute \"values\" is not configurable",
            ),
            (
                "config_setting(name='c', values={'a': 1})",
                "//pkg:c: expected value of type 'string' for dict value element, but got 1 (int)",
            ),
            (
                "config_setting(name='c', flag_values={':a': '1', '//pkg:a': '2'})",
                "//pkg:c: duplicate labels in attribute 'flag_values' of 'config_setting': //pkg:a (as [\":a\", \"//pkg:a\"])",
            ),
            (
                "constraint_value(name='v')",
                "missing value for mandatory attribute 'constraint_setting' in 'constraint_value' rule",
            ),
            (
                "platform(name='p', package_metadata=[])",
                "no such attribute 'package_metadata' in 'platform' rule",
            ),
            (
                "platform(name='p', flags=select({'//conditions:default': []}))",
                "attribute \"flags\" is not configurable",
            ),
            (
                "label_flag(name='f')",
                "missing value for mandatory attribute 'build_setting_default' in 'label_flag' rule",
            ),
            (
                "label_flag(name='f', build_setting_default=select({'//conditions:default': ':x'}))",
                "attribute \"build_setting_default\" is not configurable",
            ),
            (
                "label_flag(name='f', build_setting_default=':x', scope='bogus')",
                "//pkg:f: invalid value in 'scope' attribute: has to be one of 'universal', 'target', or 'project' instead of 'bogus'",
            ),
            (
                "genquery(name='q', expression='x', scope=[':a'], strict=2)",
                "//pkg:q: expected one of [False, True, 0, 1] for attribute 'strict' of 'genquery', but got 2 (int)",
            ),
            (
                "genrule(name='g', outs=['o'], stamp=2)",
                "expected value of type 'tristate' for TriState values is not one of [-1, 0, 1], but got 2 (int)",
            ),
            (
                "genrule(name='g', outs=['o'], local=4294967296)",
                "for attribute 'local' of 'genrule', but got 4294967296 (int)",
            ),
            (
                "test_suite(name='s', tests=select({'//conditions:default': []}))",
                "attribute \"tests\" is not configurable",
            ),
            (
                "filegroup(name='f', licenses=['bogus'])",
                "//pkg:f: invalid license type: 'bogus'",
            ),
            (
                "filegroup(name='f', srcs=select({':a': 1}))",
                "//pkg:f: expected value of type 'list(label)' for each branch in select expression of attribute 'srcs' of 'filegroup' (including '//pkg:a'), but got 1 (int)",
            ),
            (
                "filegroup(name='f', visibility=select({'//conditions:default': []}))",
                "attribute \"visibility\" is not configurable",
            ),
            (
                "filegroup(name=1)",
                "filegroup 'name' attribute must be a string",
            ),
            (
                "starlark_doc_extract(name='d')",
                "missing value for mandatory attribute 'src' in 'starlark_doc_extract' rule",
            ),
            (
                "cc_toolchain_alias(name='x')",
                "This rule has been removed from Bazel.",
            ),
            (
                "cc_library('x')",
                "_removed_rule_failure() accepts no more than 0 positional arguments but got 1",
            ),
        ] {
            rejected(source, expected);
        }
        // config_setting ignores licenses entirely (setIgnoreLicenses,
        // AttributeProvider.java:245-249); an empty config_setting loads.
        eval("config_setting(name='c', licenses=['bogus'])\nconfig_setting(name='e')").unwrap();
    }

    #[test]
    fn native_rules_are_in_the_native_struct_with_bazel_reprs() {
        custom_fixture(
            r#"
def check():
    for name in ['alias', 'config_setting', 'toolchain', 'toolchain_type', 'constraint_setting', 'constraint_value', 'platform', 'label_flag', 'label_setting', 'genquery', 'starlark_doc_extract', 'test_suite', 'java_plugins_flag_alias', 'cc_libc_top_alias', 'cc_toolchain_suite', 'cc_toolchain_alias', 'filegroup', 'genrule']:
        hasattr(native, name) or fail('missing native.' + name)
    repr(native.alias) == '<built-in rule alias>' or fail(repr(native.alias))
    repr(native.glob) == '<built-in method glob of native value>' or fail(repr(native.glob))
    repr(native.package_group) == '<built-in method package_group of native value>' or fail(repr(native.package_group))
    repr(attr.string) == '<built-in method string of attr value>' or fail(repr(attr.string))
    repr(rule) == '<built-in function rule>' or fail(repr(rule))
    native.alias(name = 'from_macro', actual = select({native.package_relative_label(':c'): ':a', '//conditions:default': ':b'}))
    native.package_relative_label(':x') == Label('//pkg:x') or fail('relative label')
    native.repo_name() == '' or fail('repo name')
    native.repository_name() == '@' or fail('repository name')
"#,
            "load(':defs.bzl', 'check')\ncheck()",
        )
        .unwrap();
        rejected("x = native.alias", "native");
    }

    #[test]
    fn package_arguments_follow_package_args_java() {
        let rules = eval(
            r#"
package(default_visibility=['//visibility:public'], default_testonly=1, default_deprecation='old', features=['-f'], licenses=['notice'], default_compatible_with=[], default_restricted_to=[], default_package_metadata=[':m'], default_hdrs_check='strict')
licenses(['notice', 'reciprocal'])
filegroup(name='f')
filegroup(name='explicit', testonly=False, package_metadata=[])
"#,
        )
        .unwrap();
        // N28c: the default is the effective package_metadata (analyzed in
        // the no-config configuration), not a gap.
        assert!(
            rules[&Label::parse(":f", "pkg").unwrap()]
                .unanalyzed
                .is_empty()
        );
        assert_eq!(
            rules[&Label::parse(":f", "pkg").unwrap()].package_metadata,
            ["//pkg:m"]
        );
        assert!(
            rules[&Label::parse(":explicit", "pkg").unwrap()]
                .package_metadata
                .is_empty()
        );
        let f = &rules[&Label::parse(":f", "pkg").unwrap()];
        let args = PackageArgs {
            default_testonly: Some(true),
            default_deprecation: Some("old".into()),
            ..PackageArgs::default()
        };
        assert!(f.is_test_only(&args));
        assert_eq!(f.deprecation_warning(&args).as_deref(), Some("old"));
        let explicit = &rules[&Label::parse(":explicit", "pkg").unwrap()];
        assert!(!explicit.is_test_only(&args));
        assert!(
            rules[&Label::parse(":explicit", "pkg").unwrap()]
                .unanalyzed
                .is_empty()
        );
        assert_eq!(
            rules[&Label::parse(":f", "pkg").unwrap()].visibility,
            Visibility::Public
        );
        for (source, expected) in [
            (
                "package()",
                "at least one argument must be given to the 'package' function",
            ),
            ("package(foo=1)", "unexpected keyword argument: foo"),
            (
                "package(default_testonly=True)\npackage(default_testonly=True)",
                "'package' can only be used once per BUILD file",
            ),
            (
                "package(default_visibility=None)",
                "expected value of type 'list(label)' for package() argument 'default_visibility', but got None (NoneType)",
            ),
            (
                "package(default_applicable_licenses=[':a'], default_package_metadata=[':b'])",
                "Can not set both default_package_metadata and default_applicable_licenses.",
            ),
            (
                "package(default_compatible_with=[':a', '//pkg:a'])",
                "duplicate label(s) in default_compatible_with: //pkg:a",
            ),
            (
                "package(licenses=['bogus'])",
                "invalid license type: 'bogus'",
            ),
            ("licenses(['bogus'])", "invalid license type: 'bogus'"),
            (
                "licenses('notice')",
                "in call to licenses(), parameter 'license_strings' got value of type 'string'",
            ),
        ] {
            rejected(source, expected);
        }
        rejected("x = native.licenses", "native");
    }

    #[test]
    fn repo_file_defaults_apply_before_package_and_ignored_directories_delete_packages() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("REPO.bazel"), "repo(default_visibility=['//visibility:public'], default_package_metadata=['//:license'])\n").unwrap();
        std::fs::write(root.0.join("pkg/BUILD"), "filegroup(name='f')").unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let rule = &package[&Label::parse(":f", "pkg").unwrap()];
        assert_eq!(rule.visibility, Visibility::Public);
        assert!(rule.unanalyzed.is_empty());
        assert_eq!(rule.package_metadata, ["//:license"]);
        std::fs::write(root.0.join("pkg/BUILD"), "package(default_visibility=['//visibility:private'], default_package_metadata=[])\nfilegroup(name='f')").unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let rule = &package[&Label::parse(":f", "pkg").unwrap()];
        assert_eq!(rule.visibility, Visibility::Private);
        assert!(rule.unanalyzed.is_empty());
        assert!(rule.package_metadata.is_empty());
        for (repo, expected) in [
            (
                "repo()",
                "at least one argument must be given to the 'repo' function",
            ),
            (
                "repo(default_testonly=True)\nrepo(default_testonly=True)",
                "'repo' can only be called once in the REPO.bazel file",
            ),
            (
                "ignore_directories([])\nrepo(default_testonly=True)",
                "if repo() is called, it must be called before any other functions",
            ),
            ("repo(bogus=1)", "unexpected keyword argument: bogus"),
            (
                "repo(default_visibility=1)",
                "for repo() argument 'default_visibility'",
            ),
            (
                "ignore_directories(['pkg'])",
                "no such package 'pkg': Package is considered deleted due to --deleted_packages",
            ),
            (
                "ignore_directories(['**'])",
                "no such package 'pkg': Package is considered deleted due to --deleted_packages",
            ),
            (
                "ignore_directories([])\nignore_directories(['x'])",
                "'ignored_directories()' can only be called once",
            ),
            (
                "ignore_directories('pkg')",
                "in call to ignore_directories(), parameter 'dirs' got value of type 'string', want 'sequence'",
            ),
            (
                "ignore_directories([1])",
                "at index 0 of dirs, got element of type int, want string",
            ),
            (
                "ignore_directories(range(2))",
                "at index 0 of dirs, got element of type int, want string",
            ),
            (
                "ignore_directories(dirs=[])",
                "ignore_directories() got named argument for positional-only parameter 'dirs'",
            ),
            (
                "ignore_directories()",
                "ignore_directories() missing 1 required positional argument: dirs",
            ),
            (
                "ignore_directories(['a'], ['b'])",
                "ignore_directories() accepts no more than 1 positional argument but got 2",
            ),
            (
                "ignore_directories(['a'], other=1)",
                "ignore_directories() got unexpected keyword argument 'other'",
            ),
            (
                "repo(bogus=1)\nignore_directories(['pkg'])",
                "no such package 'pkg'",
            ),
            (
                "load(':x.bzl', 'y')",
                "error parsing REPO.bazel file for the main repo",
            ),
        ] {
            std::fs::write(root.0.join("REPO.bazel"), repo).unwrap();
            let error = evaluate(&root.0, "pkg").unwrap_err();
            assert!(error.contains(expected), "{repo}: {error}");
        }
        // Non-matching entries leave the package and its globs alone, and
        // repo() still has to come first.
        std::fs::create_dir_all(root.0.join("pkg/node_modules/dep")).unwrap();
        std::fs::write(root.0.join("pkg/node_modules/dep/a.txt"), "").unwrap();
        std::fs::write(root.0.join("pkg/b.txt"), "").unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "filegroup(name='f', srcs=glob(['**/*.txt']))",
        )
        .unwrap();
        let srcs = |root: &TempRoot| {
            evaluate(&root.0, "pkg").unwrap()[&Label::parse(":f", "pkg").unwrap()]
                .srcs
                .clone()
        };
        std::fs::write(
            root.0.join("REPO.bazel"),
            "repo(default_testonly=False)\nignore_directories(['other', 'pkg/x'])",
        )
        .unwrap();
        assert_eq!(srcs(&root), ["//pkg:b.txt", "//pkg:node_modules/dep/a.txt"]);
        std::fs::write(
            root.0.join("REPO.bazel"),
            "ignore_directories(['**/node_modules'])",
        )
        .unwrap();
        assert_eq!(srcs(&root), ["//pkg:b.txt"]);
        std::fs::write(root.0.join("REPO.bazel"), "").unwrap();
        std::fs::write(root.0.join(".bazelignore"), "pkg/node_modules/\n").unwrap();
        assert_eq!(srcs(&root), ["//pkg:b.txt"]);
        std::fs::write(root.0.join(".bazelignore"), "/pkg\n").unwrap();
        assert_eq!(
            evaluate(&root.0, "pkg").unwrap_err(),
            format!(
                "Invalid path in {}: '/pkg': cannot be an absolute path",
                root.0.join(".bazelignore").display()
            )
        );
        std::fs::write(root.0.join(".bazelignore"), "# pkg\npkg/sub\n").unwrap();
        assert_eq!(srcs(&root), ["//pkg:b.txt", "//pkg:node_modules/dep/a.txt"]);
        std::fs::remove_file(root.0.join(".bazelignore")).unwrap();
        // A directory named REPO.bazel exists, so reading it fails.
        std::fs::remove_file(root.0.join("REPO.bazel")).unwrap();
        std::fs::create_dir(root.0.join("REPO.bazel")).unwrap();
        assert!(
            evaluate(&root.0, "pkg")
                .unwrap_err()
                .starts_with("error reading REPO.bazel file at ")
        );
    }

    #[test]
    fn module_identity_helpers_report_root_module() {
        let root = TempRoot::new();
        std::fs::write(
            root.0.join("MODULE.bazel"),
            "module(name='demo', version='1.2.3')\n",
        )
        .unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "filegroup(name=module_name() + '_' + module_version())\n([str(v) for v in package_default_visibility()] == ['@@//pkg:__pkg__']) or fail('default visibility')\npackage(default_visibility=['//visibility:public'])\n([str(v) for v in package_default_visibility()] == ['@@//visibility:public']) or fail('public visibility')",
        )
        .unwrap();
        assert!(
            evaluate(&root.0, "pkg")
                .unwrap()
                .contains_key(&Label::parse(":demo_1.2.3", "pkg").unwrap())
        );
        std::fs::write(root.0.join("MODULE.bazel"), "").unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "filegroup(name='m' + module_name() + module_version())",
        )
        .unwrap();
        assert!(
            evaluate(&root.0, "pkg")
                .unwrap()
                .contains_key(&Label::parse(":m", "pkg").unwrap())
        );
    }

    #[test]
    fn unknown_apparent_repositories_defer_to_analysis() {
        // Without a root MODULE.bazel and with an unknown repository alike.
        let rules = eval(
            "filegroup(name='f', srcs=['@missing//:x'])\nalias(name='a', actual='@missing//:y')",
        )
        .unwrap();
        assert_eq!(
            rules[&Label::parse(":f", "pkg").unwrap()].srcs,
            ["@missing//:x"]
        );
        assert_eq!(
            native_of(&rules, "a").attrs["actual"],
            AttrValue::Label(Some("@missing//:y".into()))
        );
        rejected(
            "filegroup(name='f', srcs=['@missing//:../x'])",
            "invalid label '@missing//:../x'",
        );
    }

    #[test]
    fn nodep_attributes_do_not_create_assumed_input_files() {
        let rules = eval(
            r#"
toolchain(name='t', toolchain=':impl', toolchain_type=':tt', target_settings=[':ts'])
constraint_setting(name='s', default_constraint_value=':dv')
platform(name='p', allowed_toolchain_types=[':att'], required_settings=[':rs'])
platform(name='q', allowed_toolchain_types=select({':key': [':branch'], '//conditions:default': []}))
filegroup(name='f', transitive_configs=[':tc'], srcs=[':src'])
label_flag(name='lf', build_setting_default=':lfd')
label_setting(name='ls', build_setting_default=':lsd')
"#,
        )
        .unwrap();
        let inputs = |name: &str| {
            native_of(&rules, name)
                .input_labels
                .iter()
                .map(|label| label.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(inputs("t"), ["ts", "tt"]);
        assert!(inputs("s").is_empty());
        assert_eq!(inputs("p"), ["rs"]);
        // Select keys are visited through $config_dependencies; NODEP branch
        // values are not.
        assert_eq!(inputs("q"), ["key"]);
        assert_eq!(inputs("f"), ["src"]);
        assert_eq!(inputs("lf"), ["lfd"]);
        assert_eq!(inputs("ls"), ["lsd"]);
    }

    #[test]
    fn capture_aligned_messages_for_names_licenses_lambda_and_rule_types() {
        for (source, expected) in [
            (
                "filegroup(name='a:b')",
                "illegal rule name: a:b: invalid target name 'a:b': target names may not contain ':'",
            ),
            (
                "filegroup(name='../x')",
                "illegal rule name: ../x: invalid target name '../x': target names may not contain up-level references '..'",
            ),
            (
                "filegroup(name='')",
                "illegal rule name: : invalid target name '': empty target name",
            ),
            (
                "filegroup(name='f', licenses=1)",
                "//pkg:f: expected value of type 'list(string)' for attribute 'licenses' of 'filegroup', but got 1 (int)",
            ),
            (
                "cc_toolchain_suite(toolchains={})",
                "lambda() missing 1 required positional argument: name",
            ),
            (
                "cc_toolchain_suite('a', 'b')",
                "lambda() accepts no more than 1 positional argument but got 2",
            ),
            (
                "cc_toolchain_suite('a', name='b')",
                "lambda() got multiple values for parameter 'name'",
            ),
        ] {
            rejected(source, expected);
        }
        eval("[(type(r) == 'rule') or fail(repr(r)) for r in [alias, config_setting, toolchain, toolchain_type, constraint_setting, constraint_value, platform, label_flag, label_setting, genquery, starlark_doc_extract, test_suite, java_plugins_flag_alias, cc_libc_top_alias, filegroup, genrule]]\n(type(cc_toolchain_suite) == 'function' and type(cc_library) == 'function') or fail('functions')").unwrap();
    }
}
