//! Native, recording-only MODULE.bazel evaluation (not dependency resolution).
//!
//! No extension is loaded, repository fetched, patch command run, or toolchain
//! resolved here. Labels remain apparent labels until the resolver supplies a
//! repository mapping. Direct repository-rule calls are recorded as innate
//! extension usages; loading/executing their rules and includes are unsupported.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use serde_json::Value as Json;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::syntax::ast::{Argument, AstExpr, AstStmt, Expr, Stmt};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{FrozenValue, Heap, Value, ValueLike};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::LazyLock;

use crate::repository_budget::HostBudget;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleFile {
    /// None means no module() directive; legal for an anonymous root module.
    pub module: Option<ModuleInfo>,
    pub dependencies: Vec<Dependency>,
    /// Resolver-profile additions, separate from the unchanged source declarations.
    pub implicit_dependencies: Vec<Dependency>,
    /// Keyed by module name, with at most one override of any kind per module.
    pub overrides: BTreeMap<String, ModuleOverride>,
    pub extension_usages: Vec<ExtensionUsage>,
    pub registrations: Vec<Registration>,
    /// MODULE-declared command-line aliases, in the declaring module's
    /// apparent label namespace (ModuleFileGlobals.normalizeLabelString).
    pub flag_aliases: BTreeMap<String, String>,
    /// Captured before evaluation, but enforced only when executing a direct
    /// repository declaration. Metadata-only evaluation remains unchanged.
    pub source_profile_error: Option<String>,
}

impl ModuleFile {
    /// Declaration order followed by profile additions; nodep/dev filtering is
    /// the resolver's responsibility, not a mutation of recorded metadata.
    pub fn effective_dependencies(&self) -> impl Iterator<Item = &Dependency> {
        self.dependencies.iter().chain(&self.implicit_dependencies)
    }
}

/// Chosen from the discovery key, never from the source's module() declaration.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BuiltinInjection {
    Disabled,
    /// The explicit-module profile: no implicit dependency is recorded, but
    /// the name stays reserved for the built-in exactly as in Bazel.
    ReserveBazelTools,
    AppendBazelTools,
    ActualBazelTools,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModuleRole {
    #[default]
    Root,
    Dependency,
}

/// Evaluation-time filtering, before direct repository names are reserved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvaluationContext {
    pub role: ModuleRole,
    pub ignore_dev_dependencies: bool,
    /// `--experimental_isolated_extension_usages` (crate::module_graph::ResolverOptions).
    pub isolated_extension_usages: bool,
    /// `print()` is a no-op: a module from a registry (ModuleFileFunction.java:244-246,
    /// "We don't want them to be able to spam the console during resolution").
    /// The root module, its include()d segments, built-in modules and modules
    /// with a non-registry override print `DEBUG: FILE:LINE:COL: TEXT`
    /// (:376-378 and :670-674, Event.makeDebugPrintHandler).
    pub print_is_noop: bool,
}
impl EvaluationContext {
    /// Bazel 9.3 uses this same predicate for ignoring overrides.
    pub fn effective_ignore_dev_dependencies(self) -> bool {
        self.role == ModuleRole::Dependency || self.ignore_dev_dependencies
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleInfo {
    pub name: String,
    pub version: String,
    /// Declared metadata only; Bazel 9.3 treats this field as a no-op (level 0).
    pub compatibility_level: i32,
    /// None is omitted; Some("") is explicitly defaulted to the module name.
    pub repo_name: Option<String>,
    pub bazel_compatibility: Vec<String>,
}
impl ModuleInfo {
    pub fn effective_repo_name(&self) -> &str {
        self.repo_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }
}

/// Distinguishes omitted, explicit empty/string, and explicit None (nodep).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DependencyRepoName {
    #[default]
    Default,
    Explicit(String),
    Nodep,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    /// Declared metadata only; Bazel 9.3 treats this field as a no-op (-1).
    pub max_compatibility_level: i32,
    pub repo_name: DependencyRepoName,
    pub dev_dependency: bool,
}
impl Default for Dependency {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: String::new(),
            max_compatibility_level: -1,
            repo_name: DependencyRepoName::Default,
            dev_dependency: false,
        }
    }
}
impl Dependency {
    pub fn effective_repo_name(&self) -> Option<&str> {
        match &self.repo_name {
            DependencyRepoName::Nodep => None,
            DependencyRepoName::Explicit(s) if !s.is_empty() => Some(s),
            _ => Some(&self.name),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatchOptions {
    pub patches: Vec<String>,
    pub patch_cmds: Vec<String>,
    pub patch_strip: i32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleOverride {
    LocalPath {
        path: String,
    },
    SingleVersion {
        version: String,
        registry: String,
        patches: PatchOptions,
    },
    MultipleVersion {
        versions: Vec<String>,
        registry: String,
    },
    Git {
        remote: String,
        commit: String,
        tag: String,
        branch: String,
        init_submodules: bool,
        strip_prefix: String,
        patches: PatchOptions,
    },
    Archive {
        urls: Vec<String>,
        integrity: String,
        sha256: String,
        strip_prefix: String,
        archive_type: String,
        patches: PatchOptions,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionIdentity {
    /// Apparent label, not a resolved canonical repository identity.
    pub label: String,
    pub extension_name: String,
    /// Each isolate=True call gets a distinct file-local identity.
    pub isolation: Option<usize>,
}
impl ExtensionIdentity {
    /// Innate names contain a required space; real extension names are identifiers.
    /// The raw concatenation is opaque: do not split or canonicalize it here.
    pub fn is_repo_rule(&self) -> bool {
        self.label == "//:MODULE.bazel"
            && self.extension_name.contains(' ')
            && self.isolation.is_none()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionDeclaration {
    pub dev_dependency: bool,
    /// Module-wide order of use_extension/repository-rule declarations.
    pub ordinal: usize,
    /// Bazel `thread.getCallerLocation()` text: `file:line:col` of the call's
    /// left parenthesis (1-based line and column, columns in characters).
    pub location: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionUsage {
    pub identity: ExtensionIdentity,
    pub declarations: Vec<ExtensionDeclaration>,
    pub tags: Vec<ExtensionTag>,
    pub imports: Vec<RepositoryImport>,
    /// The root module's `inject_repo`/`override_repo` entries in call order
    /// (ModuleThreadContext.ExtensionUsageBuilder.repoOverrides). Empty for
    /// every module whose calls return early (dev dependencies ignored).
    pub repo_overrides: Vec<RepoOverride>,
}
/// ModuleExtensionUsage.RepoOverride: the extension's repositories see the
/// root module's repository `overriding` under the name `overridden`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoOverride {
    pub overridden: String,
    pub overriding: String,
    /// `override_repo`: the extension must generate `overridden`;
    /// `inject_repo`: it must not.
    pub must_exist: bool,
    /// The call's location, as [`ExtensionDeclaration::location`].
    pub location: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionTag {
    pub name: String,
    /// Zero-based module-wide invocation order of retained ordinary and innate
    /// tags, before typed tag binding; unique across all usages in this MODULE.
    pub ordinal: usize,
    /// Owned after MODULE evaluation; nested mutable values retain their final
    /// contents, rather than being deep-copied when the tag is called.
    pub attributes: BTreeMap<String, Json>,
    /// Keyword argument names in call order: Bazel's Tag.attributeValues is
    /// the call's kwargs Dict (ModuleFileGlobals TagCallable), whose order the
    /// lockfile usages digest observes (SingleExtensionUsagesValue.hashForEvaluation).
    pub attribute_order: Vec<String>,
    pub declaration: usize,
    pub dev_dependency: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryImport {
    pub local_name: String,
    pub exported_name: String,
    pub declaration: usize,
    pub dev_dependency: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RegistrationKind {
    #[default]
    Toolchains,
    ExecutionPlatforms,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub kind: RegistrationKind,
    pub labels: Vec<String>,
    pub dev_dependency: bool,
}

#[derive(Default)]
struct State {
    file: ModuleFile,
    non_module_called: bool,
    /// Repository name -> (how, location) of its first usage
    /// (ModuleThreadContext.RepoNameUsage, :105-113).
    repo_names: BTreeMap<String, (&'static str, String)>,
    next_isolation: usize,
    next_declaration: usize,
    pending_attributes: Vec<PendingAttributes>,
}

/// Values live in traced module slots, never in this untraced collector.
struct PendingAttributes {
    slot: String,
    usage: usize,
    tag: usize,
}

// Upstream derives generate unsafe marker-trait implementations. All our values
// contain only owned/static data or integer handles; no handwritten unsafe code.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_simple_value;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };

    #[derive(Default, ProvidesStaticType)]
    pub(super) struct Collector {
        pub state: RefCell<State>,
        pub evaluation_context: EvaluationContext,
        /// The caller location [`super::module_print`] records for the
        /// standard `print` it delegates to (this collector is its handler).
        pub print_location: RefCell<Option<String>>,
    }

    /// Event.makeDebugPrintHandler (Event.java): `DEBUG: LOCATION: TEXT`
    /// (a DEBUG event: dropped under `--quiet`, crate::ui).
    impl starlark::PrintHandler for Collector {
        fn println(&self, text: &str) -> starlark::Result<()> {
            match self.print_location.borrow_mut().take() {
                Some(location) if !location.is_empty() => {
                    crate::note!("DEBUG: {location}: {text}")
                }
                _ => crate::note!("DEBUG: {text}"),
            }
            Ok(())
        }
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Builtin(pub &'static str);
    // BuiltinFunction.repr (net/starlark/java/eval/BuiltinFunction.java:192-203):
    // ModuleFileGlobals (bazel/bzlmod/ModuleFileGlobals.java:61) is no StarlarkValue.
    impl fmt::Display for Builtin {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<built-in function {}>", self.0)
        }
    }
    starlark_simple_value!(Builtin);
    // type() of a BuiltinFunction (net/starlark/java/eval/BuiltinFunction.java).
    #[starlark_value(type = "builtin_function_or_method")]
    impl<'v> StarlarkValue<'v> for Builtin {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            invoke(self.0, args, eval).map_err(starlark::Error::new_other)
        }
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ExtensionProxy {
        pub usage: usize,
        pub declaration: usize,
    }
    impl fmt::Display for ExtensionProxy {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override (ModuleFileGlobals.java:549; StarlarkValue.java:50-52).
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$ModuleExtensionProxy>")
        }
    }
    starlark_simple_value!(ExtensionProxy);
    #[starlark_value(type = "module_extension_proxy")]
    impl<'v> StarlarkValue<'v> for ExtensionProxy {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            Some(heap.alloc(TagCallable {
                proxy: self.clone(),
                name: name.to_owned(),
            }))
        }
        fn has_attr(&self, _name: &str, _heap: Heap<'v>) -> bool {
            true
        }
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct RepoRuleProxy {
        pub raw_identity: String,
    }
    impl fmt::Display for RepoRuleProxy {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override (ModuleFileGlobals.java:833; StarlarkValue.java:50-52).
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$RepoRuleProxy>")
        }
    }
    starlark_simple_value!(RepoRuleProxy);
    #[starlark_value(type = "repo_rule_proxy")]
    impl<'v> StarlarkValue<'v> for RepoRuleProxy {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            record_repo_rule(&self.raw_identity, args, eval).map_err(starlark::Error::new_other)
        }
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    struct TagCallable {
        proxy: ExtensionProxy,
        name: String,
    }
    impl fmt::Display for TagCallable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override (ModuleFileGlobals.java:579; StarlarkValue.java:50-52).
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$ModuleExtensionProxy$TagCallable>")
        }
    }
    starlark_simple_value!(TagCallable);
    // TagCallable has no @StarlarkBuiltin, so type() is its simple class name
    // (ModuleFileGlobals.java:579).
    #[starlark_value(type = "TagCallable")]
    impl<'v> StarlarkValue<'v> for TagCallable {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            let record = || -> anyhow::Result<()> {
                args.no_positional_args(eval.heap())
                    .map_err(|e| anyhow!("{e}"))?;
                let attribute_order = args
                    .names_map()
                    .map_err(|e| anyhow!("{e}"))?
                    .keys()
                    .map(|name| name.as_str().to_owned())
                    .collect();
                let attributes = Kwargs::new(args)?.0;
                let collector = context(eval)?;
                let mut state = collector.state.borrow_mut();
                let ordinal = state.pending_attributes.len();
                let usage = &mut state.file.extension_usages[self.proxy.usage];
                // Ignored proxies have detached usages in Bazel. Keep declaration
                // metadata for our recorded profile, but do not retain or convert
                // their unused tag values. use_repo still reserves import names.
                if usage.declarations[self.proxy.declaration].dev_dependency
                    && collector
                        .evaluation_context
                        .effective_ignore_dev_dependencies()
                {
                    return Ok(());
                }
                let tag = usage.tags.len();
                usage.tags.push(ExtensionTag {
                    name: self.name.clone(),
                    ordinal,
                    attributes: BTreeMap::new(),
                    attribute_order,
                    declaration: self.proxy.declaration,
                    dev_dependency: usage.declarations[self.proxy.declaration].dev_dependency,
                });
                retain_tag_attributes(eval, &mut state, self.proxy.usage, tag, attributes);
                Ok(())
            };
            record().map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }
}
use values::{Builtin, Collector, ExtensionProxy, RepoRuleProxy};

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Collector> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Collector>())
        .ok_or_else(|| anyhow!("MODULE directive requires a MODULE.bazel evaluation context"))
}

/// Read only the specified file. No include, load, or repository access occurs.
pub fn evaluate(path: &Path) -> Result<ModuleFile, String> {
    evaluate_with_context(path, EvaluationContext::default())
}

/// Compatibility wrapper: the boolean is the effective ignore-dev predicate,
/// including override filtering. Prefer evaluate_with_context for explicit roles.
pub fn evaluate_for_resolution(
    path: &Path,
    ignore_dev_dependencies: bool,
) -> Result<ModuleFile, String> {
    evaluate_with_context(
        path,
        EvaluationContext {
            role: ModuleRole::Root,
            ignore_dev_dependencies,
            isolated_extension_usages: false,
            print_is_noop: false,
        },
    )
}

pub fn evaluate_with_context(
    path: &Path,
    context: EvaluationContext,
) -> Result<ModuleFile, String> {
    evaluate_with_builtins(path, context, BuiltinInjection::Disabled)
}

pub(crate) fn evaluate_with_builtins(
    path: &Path,
    context: EvaluationContext,
    injection: BuiltinInjection,
) -> Result<ModuleFile, String> {
    let source = crate::observed_fs::read_to_string(path)
        .map_err(|e| format!("cannot read MODULE file {}: {e}", path.display()))?;
    evaluate_source_with_builtins(&path.to_string_lossy(), source, context, injection)
}

pub fn evaluate_source(filename: &str, source: String) -> Result<ModuleFile, String> {
    evaluate_source_with_context(filename, source, EvaluationContext::default())
}

/// Compatibility wrapper with an effective ignore-dev predicate, not just overrides.
/// Prefer evaluate_source_with_context for explicit root/dependency roles.
pub fn evaluate_source_for_resolution(
    filename: &str,
    source: String,
    ignore_dev_dependencies: bool,
) -> Result<ModuleFile, String> {
    evaluate_source_with_context(
        filename,
        source,
        EvaluationContext {
            role: ModuleRole::Root,
            ignore_dev_dependencies,
            isolated_extension_usages: false,
            print_is_noop: false,
        },
    )
}

pub fn evaluate_source_with_context(
    filename: &str,
    source: String,
    evaluation_context: EvaluationContext,
) -> Result<ModuleFile, String> {
    evaluate_source_with_builtins(
        filename,
        source,
        evaluation_context,
        BuiltinInjection::Disabled,
    )
}

pub(crate) fn evaluate_source_with_builtins(
    filename: &str,
    source: String,
    evaluation_context: EvaluationContext,
    injection: BuiltinInjection,
) -> Result<ModuleFile, String> {
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(filename, source, &dialect).map_err(|e| e.to_string())?;
    validate_module_syntax(&ast)?;
    // MODULE.bazel lexer errors fail every command (Bazel exit 48), not only
    // a later demanded repository.
    crate::repository_text::validate_source(&ast)?;
    crate::guarded_globals::check_universe(&ast)?;
    let source_profile_error = crate::repository_text::validate_execution_source(&ast).err();
    // `print` is [`module_print`]: the standard one (Bazel's debugPrint
    // texts) behind ModuleFileFunction's per-module print handler.
    let mut globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType]).with(module_print);
    for name in [
        "module",
        "bazel_dep",
        "local_path_override",
        "single_version_override",
        "multiple_version_override",
        "git_override",
        "archive_override",
        "use_extension",
        "use_repo",
        "register_toolchains",
        "register_execution_platforms",
        "include",
        "use_repo_rule",
        "Label",
        "inject_repo",
        "override_repo",
        "flag_alias",
    ] {
        globals.set(name, Builtin(name));
    }
    let globals = globals.build();
    let collector = Collector {
        evaluation_context,
        ..Default::default()
    };
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&collector);
        eval.set_print_handler(&collector);
        eval.set_max_callstack_size(128)
            .map_err(|e| e.to_string())?;
        // MODULE strings flow into repository attributes (Latin-1 chars).
        let model = starlark::values::string::enter_latin1_char_model(true);
        eval.eval_module(ast, &globals).map_err(|e| e.to_string())?;
        drop(model);
        finish_tag_attributes(&module, &mut collector.state.borrow_mut())
            .map_err(|e| format!("cannot record MODULE attributes in {filename}: {e}"))?;
        Ok::<_, String>(())
    })?;
    let mut state = collector.state.into_inner();
    if let BuiltinInjection::AppendBazelTools | BuiltinInjection::ReserveBazelTools = injection {
        reserve_builtin(&state, "bazel_tools")?;
    }
    check_repo_overrides(
        &state,
        matches!(
            injection,
            BuiltinInjection::AppendBazelTools | BuiltinInjection::ReserveBazelTools
        ),
    )?;
    if let BuiltinInjection::AppendBazelTools = injection {
        state.file.implicit_dependencies.push(Dependency {
            name: "bazel_tools".to_owned(),
            repo_name: DependencyRepoName::Explicit("bazel_tools".to_owned()),
            ..Default::default()
        });
    }
    let mut file = state.file;
    file.source_profile_error = source_profile_error;
    Ok(file)
}

/// MODULE.bazel `print` (ModuleFileFunction.execModuleFile, :670-674): a
/// no-op for a module from a registry ([`EvaluationContext::print_is_noop`]),
/// else the standard `print` (same parameters, so the same binding errors;
/// MethodLibrary.print's debugPrint texts) with Event.makeDebugPrintHandler's
/// `DEBUG: FILE:LINE:COL: ` prefix, the call's left parenthesis as for every
/// MODULE builtin ([`caller_location`]).
#[starlark_module]
fn module_print(builder: &mut GlobalsBuilder) {
    fn print<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(require = named, default = " ")] sep: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let collector = context(eval).map_err(starlark::Error::new_other)?;
        if collector.evaluation_context.print_is_noop {
            return Ok(NoneType);
        }
        collector
            .print_location
            .replace(Some(caller_location(eval)));
        let sep = eval.heap().alloc_str(sep).to_value();
        eval.eval_function(standard_print().to_value(), &args.items, &[("sep", sep)])?;
        Ok(NoneType)
    }
}

/// The interpreter's own `print` ([`LibraryExtension::Print`]).
fn standard_print() -> FrozenValue {
    static GLOBALS: LazyLock<Globals> =
        LazyLock::new(|| GlobalsBuilder::extended_by(&[LibraryExtension::Print]).build());
    GLOBALS
        .iter()
        .find_map(|(name, value)| (name == "print").then_some(value))
        .unwrap_or_else(|| unreachable!("LibraryExtension::Print defines print"))
}

/// Bazel's DotBazelFileSyntaxChecker restricts call syntax even in unreachable
/// expressions. Its resolver also prohibits rebinding file-global names. Check
/// the AST before evaluation: runtime argument binding cannot enforce either.
fn validate_module_syntax(ast: &AstModule) -> Result<(), String> {
    fn check_expr(ast: &AstModule, expr: &AstExpr) -> Result<(), String> {
        if let Expr::Call(_, arguments) = &expr.node {
            for argument in &arguments.args {
                let message = match &argument.node {
                    Argument::Args(_) => Some("*args are not allowed in MODULE.bazel"),
                    Argument::KwArgs(value) if !matches!(&value.node, Expr::Dict(_)) => {
                        Some("**kwargs must be a dict literal in MODULE.bazel")
                    }
                    _ => None,
                };
                if let Some(message) = message {
                    return Err(format!("{}: {message}", ast.file_span(argument.span)));
                }
            }
        }
        expr.node.visit_expr_err(|child| check_expr(ast, child))
    }

    fn check_bindings(
        ast: &AstModule,
        statement: &AstStmt,
        first_bindings: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        let target = match &statement.node {
            Stmt::Statements(statements) => {
                for statement in statements {
                    check_bindings(ast, statement, first_bindings)?;
                }
                return Ok(());
            }
            Stmt::Assign(assign) => &assign.lhs,
            Stmt::AssignModify(target, _, _) => target,
            // def/for/if/load are rejected by the parser's MODULE dialect.
            // Comprehension targets are local, not file-global bindings.
            _ => return Ok(()),
        };
        let mut result = Ok(());
        target.node.visit_lvalue(|identifier| {
            if result.is_err() {
                return;
            }
            let name = &identifier.node.ident;
            let location = ast.file_span(identifier.span).to_string();
            if let Some(first) = first_bindings.get(name) {
                result = Err(format!("{location}: global binding {name:?} may not be reassigned; first bound at {first}"));
            } else {
                first_bindings.insert(name.clone(), location);
            }
        });
        result
    }

    ast.statement()
        .node
        .visit_expr_result(|expr| check_expr(ast, expr))?;
    check_bindings(ast, ast.statement(), &mut BTreeMap::new())
}

struct Kwargs<'v>(BTreeMap<String, Value<'v>>);
impl<'v> Kwargs<'v> {
    fn new(args: &Arguments<'v, '_>) -> anyhow::Result<Self> {
        Ok(Self(
            args.names_map()
                .map_err(|e| anyhow!("{e}"))?
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), *v))
                .collect(),
        ))
    }
    fn required(&mut self, name: &str) -> anyhow::Result<Value<'v>> {
        self.0
            .remove(name)
            .ok_or_else(|| anyhow!("missing required argument {name:?}"))
    }
    fn string(&mut self, name: &str, default: Option<&str>) -> anyhow::Result<String> {
        match self.0.remove(name) {
            Some(v) => string(v, name),
            None => default
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("missing required argument {name:?}")),
        }
    }
    fn boolean(&mut self, name: &str) -> anyhow::Result<bool> {
        self.0
            .remove(name)
            .map(|v| {
                v.unpack_bool()
                    .ok_or_else(|| anyhow!("{name} must be a bool"))
            })
            .transpose()
            .map(|v| v.unwrap_or(false))
    }
    fn integer(&mut self, name: &str, default: i32, min: i32) -> anyhow::Result<i32> {
        let n = self
            .0
            .remove(name)
            .map(|v| {
                v.unpack_i32()
                    .ok_or_else(|| anyhow!("{name} must be a 32-bit int"))
            })
            .transpose()?
            .unwrap_or(default);
        if n < min {
            bail!("{name} must be at least {min}");
        }
        Ok(n)
    }
    fn strings(&mut self, name: &str) -> anyhow::Result<Vec<String>> {
        self.0
            .remove(name)
            .map(|v| strings(v, name))
            .transpose()
            .map(|v| v.unwrap_or_default())
    }
    fn finish(self) -> anyhow::Result<()> {
        if let Some(name) = self.0.keys().next() {
            bail!("unknown or unsupported argument {name:?}");
        }
        Ok(())
    }
    fn patches(&mut self) -> anyhow::Result<PatchOptions> {
        Ok(PatchOptions {
            patches: self.strings("patches")?,
            patch_cmds: self.strings("patch_cmds")?,
            patch_strip: self.integer("patch_strip", 0, 0)?,
        })
    }
    fn version(&mut self, name: &str) -> anyhow::Result<String> {
        let v = self.string(name, Some(""))?;
        validate_version(&v)?;
        Ok(v)
    }
    fn registry(&mut self) -> anyhow::Result<String> {
        let r = self.string("registry", Some(""))?;
        if !r.is_empty() {
            validate_url(&r, "registry")?;
        }
        Ok(r)
    }
}

fn invoke<'v>(
    name: &str,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let collector = context(eval)?;
    match name {
        "include" => bail!(
            "include() is unsupported until the resolver supplies include filesystem semantics"
        ),
        "Label" => bail!("Label() is unsupported in MODULE evaluation; use label strings"),
        // ModuleFileGlobals.overrideRepo/injectRepo (:723-728, :783-788) mark a
        // non-module call and return early where dev dependencies are ignored:
        // every non-root module (hermetic_launcher's inject_repo under
        // rules_rs) and --ignore_dev_dependency. The root module's calls are
        // recorded below.
        "inject_repo" | "override_repo"
            if collector
                .evaluation_context
                .effective_ignore_dev_dependencies() =>
        {
            collector.state.borrow_mut().non_module_called = true;
            return Ok(Value::new_none());
        }
        _ => {}
    }
    let mut kwargs = Kwargs::new(args)?;
    let mut state = collector.state.borrow_mut();
    if name == "module" {
        if state.file.module.is_some() {
            bail!("module() may only be called once");
        }
        if state.non_module_called {
            bail!("module() must be called before other MODULE directives");
        }
    } else {
        state.non_module_called = true;
    }

    if name == "use_repo_rule" {
        if !kwargs.0.is_empty() {
            bail!(
                "use_repo_rule accepts exactly two positional-only string arguments; no keyword arguments"
            );
        }
        let pos: Vec<_> = args
            .positions(eval.heap())
            .map_err(|e| anyhow!("{e}"))?
            .collect();
        if pos.len() != 2 {
            bail!("use_repo_rule requires exactly two positional-only string arguments");
        }
        // Upstream postpones label parsing and symbol validation until loading.
        let bzl = string(pos[0], "bzl_file")?;
        let rule = string(pos[1], "rule_name")?;
        return Ok(eval.heap().alloc(RepoRuleProxy {
            raw_identity: format!("{bzl} {rule}"),
        }));
    }
    if name == "use_extension" {
        // `isolate` is enableOnlyWithFlag = "-experimental_isolated_extension_usages"
        // (ModuleFileGlobals.java:436-449): passing it at all without the flag
        // fails while binding arguments (BuiltinFunction.java:369-375).
        if kwargs.0.contains_key("isolate")
            && !collector.evaluation_context.isolated_extension_usages
        {
            bail!(
                "in call to use_extension(), parameter 'isolate' is experimental and thus unavailable with the current flags. It may be enabled by setting --experimental_isolated_extension_usages"
            );
        }
        let pos: Vec<_> = args
            .positions(eval.heap())
            .map_err(|e| anyhow!("{e}"))?
            .collect();
        if pos.len() > 2 {
            bail!("use_extension accepts at most two positional arguments");
        }
        for (key, value) in ["extension_bzl_file", "extension_name"]
            .into_iter()
            .zip(pos)
        {
            if kwargs.0.insert(key.to_owned(), value).is_some() {
                bail!("argument {key} supplied twice");
            }
        }
        let label = kwargs.string("extension_bzl_file", None)?;
        validate_extension_label(&label)?;
        let extension_name = kwargs.string("extension_name", None)?;
        if !identifier(&extension_name) {
            bail!("invalid extension name {extension_name:?}");
        }
        let dev_dependency = kwargs.boolean("dev_dependency")?;
        let isolate = kwargs.boolean("isolate")?;
        kwargs.finish()?;
        let isolation = if isolate {
            let id = state.next_isolation;
            state.next_isolation += 1;
            Some(id)
        } else {
            None
        };
        let identity = ExtensionIdentity {
            label,
            extension_name,
            isolation,
        };
        let usage = match state
            .file
            .extension_usages
            .iter()
            .position(|u| u.identity == identity)
        {
            Some(i) => i,
            None => {
                state.file.extension_usages.push(ExtensionUsage {
                    identity,
                    ..Default::default()
                });
                state.file.extension_usages.len() - 1
            }
        };
        let ordinal = state.next_declaration;
        state.next_declaration += 1;
        let declarations = &mut state.file.extension_usages[usage].declarations;
        let declaration = declarations.len();
        declarations.push(ExtensionDeclaration {
            dev_dependency,
            ordinal,
            location: caller_location(eval),
        });
        return Ok(eval.heap().alloc(ExtensionProxy { usage, declaration }));
    }
    if matches!(name, "inject_repo" | "override_repo") {
        // ModuleFileGlobals.injectRepo/overrideRepo for the root module: a
        // positional name overrides the same name, a keyword its key, in call
        // order (ModuleThreadContext.ExtensionUsageBuilder.addRepoOverride).
        let mut pos = args.positions(eval.heap()).map_err(|e| anyhow!("{e}"))?;
        let proxy = pos
            .next()
            .ok_or_else(|| anyhow!("{name} requires an extension proxy"))?;
        let proxy = proxy
            .downcast_ref::<ExtensionProxy>()
            .ok_or_else(|| anyhow!("{name} requires an extension proxy"))?;
        let mut overrides = Vec::new();
        for v in pos {
            let s = string(v, "repository name")?;
            overrides.push((s.clone(), s));
        }
        drop(kwargs);
        for (overridden, value) in args.names_map().map_err(|e| anyhow!("{e}"))?.iter() {
            overrides.push((
                overridden.as_str().to_owned(),
                string(*value, "overriding repository name")?,
            ));
        }
        let location = caller_location(eval);
        let usage = &mut state.file.extension_usages[proxy.usage];
        for (overridden, overriding) in overrides {
            validate_repo_name(&overridden)?;
            validate_repo_name(&overriding)?;
            if let Some(previous) = usage
                .repo_overrides
                .iter()
                .find(|previous| previous.overridden == overridden)
            {
                bail!(
                    "The repo exported as '{overridden}' by module extension '{}' is already overridden with '{}' at {}",
                    usage.identity.extension_name,
                    previous.overriding,
                    previous.location
                );
            }
            usage.repo_overrides.push(RepoOverride {
                overridden,
                overriding,
                must_exist: name == "override_repo",
                location: location.clone(),
            });
        }
        return Ok(Value::new_none());
    }
    if name == "use_repo" {
        let mut pos = args.positions(eval.heap()).map_err(|e| anyhow!("{e}"))?;
        let proxy = pos
            .next()
            .ok_or_else(|| anyhow!("use_repo requires an extension proxy"))?;
        let proxy = proxy
            .downcast_ref::<ExtensionProxy>()
            .ok_or_else(|| anyhow!("use_repo requires an extension proxy"))?;
        let mut imports = Vec::new();
        for v in pos {
            let s = string(v, "repository name")?;
            imports.push((s.clone(), s));
        }
        // Keyword imports in call order (ModuleFileGlobals.useRepo iterates the
        // kwargs Dict, which keeps insertion order); the order is the main
        // repository mapping's entry order (RepositoryMapping.getInverse).
        drop(kwargs);
        for (local, value) in args.names_map().map_err(|e| anyhow!("{e}"))?.iter() {
            imports.push((
                local.as_str().to_owned(),
                string(*value, "exported repository name")?,
            ));
        }
        for (local, exported) in imports {
            validate_repo_name(&local)?;
            validate_repo_name(&exported)?;
            reserve_repo(
                &mut state,
                &local,
                "by a use_repo() call",
                caller_location(eval),
            )?;
            let usage = &mut state.file.extension_usages[proxy.usage];
            usage.imports.push(RepositoryImport {
                local_name: local,
                exported_name: exported,
                declaration: proxy.declaration,
                dev_dependency: usage.declarations[proxy.declaration].dev_dependency,
            });
        }
        return Ok(Value::new_none());
    }
    if matches!(name, "register_toolchains" | "register_execution_platforms") {
        let labels = args
            .positions(eval.heap())
            .map_err(|e| anyhow!("{e}"))?
            .map(|v| string(v, "target pattern"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for label in &labels {
            if !label.starts_with("//") && !label.starts_with('@') {
                // ModuleFileGlobals.checkAllAbsolutePatterns
                // (bazel/bzlmod/ModuleFileGlobals.java:198-208).
                bail!(
                    "Expected absolute target patterns (must begin with '//' or '@') for '{name}' argument, but got '{label}' as an argument"
                );
            }
        }
        let dev_dependency = kwargs.boolean("dev_dependency")?;
        kwargs.finish()?;
        state.file.registrations.push(Registration {
            labels,
            dev_dependency,
            kind: if name == "register_toolchains" {
                RegistrationKind::Toolchains
            } else {
                RegistrationKind::ExecutionPlatforms
            },
        });
        return Ok(Value::new_none());
    }
    args.no_positional_args(eval.heap())
        .map_err(|e| anyhow!("{e}"))?;
    match name {
        "flag_alias" => {
            let name = kwargs.string("name", None)?;
            let raw = kwargs.string("starlark_flag", None)?;
            kwargs.finish()?;
            let own = state
                .file
                .module
                .as_ref()
                .map_or("", ModuleInfo::effective_repo_name);
            let label = crate::flag_alias::normalize_module_label(
                &raw,
                own,
                collector.evaluation_context.role == ModuleRole::Root,
            )
            .map_err(|e| anyhow!("invalid label {raw:?}: {e}"))?;
            if state
                .file
                .flag_aliases
                .insert(name.clone(), label)
                .is_some()
            {
                bail!("Multiple entries with same key: {name}");
            }
        }
        "module" => {
            let name = kwargs.string("name", Some(""))?;
            if !name.is_empty() {
                validate_module_name(&name)?;
            }
            let version = kwargs.version("version")?;
            let compatibility_level = kwargs.integer("compatibility_level", 0, i32::MIN)?;
            let repo_name = kwargs
                .0
                .remove("repo_name")
                .map(|v| string(v, "repo_name"))
                .transpose()?;
            let bazel_compatibility = kwargs.strings("bazel_compatibility")?;
            for constraint in &bazel_compatibility {
                validate_bazel_constraint(constraint)?;
            }
            kwargs.finish()?;
            let info = ModuleInfo {
                name,
                version,
                compatibility_level,
                repo_name,
                bazel_compatibility,
            };
            if !info.effective_repo_name().is_empty() {
                validate_repo_name(info.effective_repo_name())?;
                // ModuleFileGlobals.module (:175-181).
                let how = if info.repo_name.as_deref().is_none_or(str::is_empty) {
                    "as the current module name"
                } else {
                    "as the module's own repo name"
                };
                reserve_repo(
                    &mut state,
                    info.effective_repo_name(),
                    how,
                    caller_location(eval),
                )?;
            }
            state.file.module = Some(info);
        }
        "bazel_dep" => {
            let name = kwargs.string("name", None)?;
            validate_module_name(&name)?;
            let version = kwargs.version("version")?;
            let max_compatibility_level =
                kwargs.integer("max_compatibility_level", -1, i32::MIN)?;
            let repo_name = match kwargs.0.remove("repo_name") {
                None => DependencyRepoName::Default,
                Some(v) if v.is_none() => DependencyRepoName::Nodep,
                Some(v) => DependencyRepoName::Explicit(string(v, "repo_name")?),
            };
            let dev_dependency = kwargs.boolean("dev_dependency")?;
            kwargs.finish()?;
            let dep = Dependency {
                name,
                version,
                max_compatibility_level,
                repo_name,
                dev_dependency,
            };
            if let Some(repo) = dep.effective_repo_name() {
                validate_repo_name(repo)?;
                reserve_repo(&mut state, repo, "by a bazel_dep", caller_location(eval))?;
            }
            state.file.dependencies.push(dep);
        }
        _ => {
            let module_name = kwargs.string("module_name", None)?;
            validate_module_name(&module_name)?;
            let value = match name {
                "local_path_override" => {
                    let path = kwargs.string("path", None)?;
                    if path.is_empty() {
                        bail!("local_path_override path must not be empty");
                    }
                    ModuleOverride::LocalPath { path }
                }
                "single_version_override" => ModuleOverride::SingleVersion {
                    version: kwargs.version("version")?,
                    registry: kwargs.registry()?,
                    patches: kwargs.patches()?,
                },
                "multiple_version_override" => {
                    let versions = strings(kwargs.required("versions")?, "versions")?;
                    if versions.len() < 2 {
                        bail!("multiple_version_override requires at least 2 versions");
                    }
                    for v in &versions {
                        validate_version(v)?;
                    }
                    ModuleOverride::MultipleVersion {
                        versions,
                        registry: kwargs.registry()?,
                    }
                }
                "git_override" => {
                    let remote = kwargs.string("remote", None)?;
                    if remote.is_empty() {
                        bail!("git_override remote must not be empty");
                    }
                    let commit = kwargs.string("commit", Some(""))?;
                    let tag = kwargs.string("tag", Some(""))?;
                    let branch = kwargs.string("branch", Some(""))?;
                    if [&commit, &tag, &branch]
                        .iter()
                        .filter(|v| !v.is_empty())
                        .count()
                        != 1
                    {
                        bail!("git_override requires exactly one of commit, tag, branch");
                    }
                    ModuleOverride::Git {
                        remote,
                        commit,
                        tag,
                        branch,
                        init_submodules: kwargs.boolean("init_submodules")?,
                        strip_prefix: kwargs.string("strip_prefix", Some(""))?,
                        patches: kwargs.patches()?,
                    }
                }
                "archive_override" => {
                    let urls = kwargs.required("urls")?;
                    let urls = if let Some(s) = urls.unpack_str() {
                        vec![s.to_owned()]
                    } else {
                        strings(urls, "urls")?
                    };
                    if urls.is_empty() {
                        bail!("archive_override urls must not be empty");
                    }
                    for url in &urls {
                        validate_url(url, "archive URL")?;
                    }
                    let sha256 = kwargs.string("sha256", Some(""))?;
                    if !sha256.is_empty()
                        && (sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()))
                    {
                        bail!("sha256 must contain 64 hexadecimal digits");
                    }
                    ModuleOverride::Archive {
                        urls,
                        sha256,
                        integrity: kwargs.string("integrity", Some(""))?,
                        strip_prefix: kwargs.string("strip_prefix", Some(""))?,
                        archive_type: kwargs.string("archive_type", Some(""))?,
                        patches: kwargs.patches()?,
                    }
                }
                _ => bail!("unsupported MODULE directive {name}"),
            };
            kwargs.finish()?;
            // ModuleThreadContext.addOverride (Bazel 9.3): ignored overrides
            // return before duplicate checking, but after directive validation.
            if collector
                .evaluation_context
                .effective_ignore_dev_dependencies()
            {
                return Ok(Value::new_none());
            }
            if state.file.overrides.contains_key(&module_name) {
                bail!("duplicate override for module {module_name:?}");
            }
            state.file.overrides.insert(module_name, value);
        }
    }
    Ok(Value::new_none())
}

/// AttributeValues/TagCallable retain shallow keyword bindings until MODULE
/// evaluation ends. A fresh outer dict prevents **kwargs rebinding from changing
/// those bindings, while its original nested Values remain live and GC-traced.
fn retain_tag_attributes<'v>(
    eval: &Evaluator<'v, '_, '_>,
    state: &mut State,
    usage: usize,
    tag: usize,
    attributes: BTreeMap<String, Value<'v>>,
) {
    // Not a valid Starlark identifier: MODULE assignments cannot overwrite roots.
    let slot = format!("__module_tag_attributes${}", state.pending_attributes.len());
    eval.module()
        .set(&slot, eval.heap().alloc(AllocDict(attributes)));
    state
        .pending_attributes
        .push(PendingAttributes { slot, usage, tag });
}

/// Convert only after successful evaluation and before the temporary heap drops.
/// Both ordinary tags and innate repository tags use the same bounded types.
fn finish_tag_attributes(module: &Module, state: &mut State) -> anyhow::Result<()> {
    // One budget for the entire MODULE's host conversion, including repeated
    // references: each retained declaration produces a distinct owned copy.
    let mut host = HostBudget::default();
    for pending in std::mem::take(&mut state.pending_attributes) {
        let value = module
            .get(&pending.slot)
            .ok_or_else(|| anyhow!("internal error: missing MODULE attribute root"))?;
        let dict = DictRef::from_value(value)
            .ok_or_else(|| anyhow!("internal error: MODULE attribute root is not a dict"))?;
        host.node().map_err(anyhow::Error::msg)?;
        let mut attributes = BTreeMap::new();
        for (key, value) in dict.iter() {
            let key = key
                .unpack_str()
                .ok_or_else(|| anyhow!("tag attribute name must be a string"))?;
            host.string(key).map_err(anyhow::Error::msg)?;
            let value = owned_json(value, 0, &mut host)?;
            attributes.insert(key.to_owned(), value);
        }
        state.file.extension_usages[pending.usage].tags[pending.tag].attributes = attributes;
    }
    Ok(())
}

fn record_repo_rule<'v>(
    raw_identity: &str,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let collector = context(eval)?;
    args.no_positional_args(eval.heap())
        .map_err(|e| anyhow!("{e}"))?;
    // RepoRuleProxy.call: the extra keyword arguments in call order, then
    // `name` (kwargs.putEntry), without `dev_dependency`.
    let mut attribute_order: Vec<String> = args
        .names_map()
        .map_err(|e| anyhow!("{e}"))?
        .keys()
        .map(|name| name.as_str().to_owned())
        .filter(|name| name != "name" && name != "dev_dependency")
        .collect();
    attribute_order.push("name".to_owned());
    let mut kwargs = Kwargs::new(args)?;
    let name_value = kwargs.required("name")?;
    let name = string(name_value, "name")?;
    let dev_dependency = kwargs.boolean("dev_dependency")?;
    validate_repo_name(&name)?;
    // Unlike bazel_dep/use_repo, an ignored direct call reserves no name and
    // creates no usage. Even unsupported attribute values are not inspected.
    if dev_dependency
        && collector
            .evaluation_context
            .effective_ignore_dev_dependencies()
    {
        return Ok(Value::new_none());
    }
    kwargs.0.insert("name".to_owned(), name_value);
    let mut state = collector.state.borrow_mut();
    reserve_repo(&mut state, &name, "by a repo rule", caller_location(eval))?;
    let identity = ExtensionIdentity {
        label: "//:MODULE.bazel".to_owned(),
        extension_name: raw_identity.to_owned(),
        isolation: None,
    };
    let index = match state
        .file
        .extension_usages
        .iter()
        .position(|u| u.identity == identity)
    {
        Some(index) => index,
        None => {
            state.file.extension_usages.push(ExtensionUsage {
                identity,
                ..Default::default()
            });
            state.file.extension_usages.len() - 1
        }
    };
    let ordinal = state.pending_attributes.len();
    let declaration_ordinal = state.next_declaration;
    state.next_declaration += 1;
    let usage = &mut state.file.extension_usages[index];
    let declaration = usage.declarations.len();
    let tag = usage.tags.len();
    usage.declarations.push(ExtensionDeclaration {
        dev_dependency,
        ordinal: declaration_ordinal,
        location: caller_location(eval),
    });
    usage.tags.push(ExtensionTag {
        name: "repo".to_owned(),
        ordinal,
        attributes: BTreeMap::new(),
        attribute_order,
        declaration,
        dev_dependency,
    });
    usage.imports.push(RepositoryImport {
        local_name: name.clone(),
        exported_name: name,
        declaration,
        dev_dependency,
    });
    retain_tag_attributes(eval, &mut state, index, tag, kwargs.0);
    Ok(Value::new_none())
}

/// Bazel's StarlarkThread.getCallerLocation for a builtin call is the call's
/// left-parenthesis location (Eval.java:714-715), rendered `file:line:col`.
fn caller_location(eval: &Evaluator<'_, '_, '_>) -> String {
    let Some(call) = eval.call_stack_top_location() else {
        return String::new();
    };
    // The recorded span covers the entire call expression. MODULE callees are
    // parenthesis-free names, so the first '(' is the call's own parenthesis.
    let offset = call.source_span().find('(').unwrap_or(0);
    let begin = call.span.begin().get() + offset as u32;
    let position = call
        .file
        .resolve_span(starlark::codemap::Span::new(
            starlark::codemap::Pos::new(begin),
            starlark::codemap::Pos::new(begin),
        ))
        .begin;
    format!(
        "{}:{}:{}",
        call.filename(),
        position.line + 1,
        position.column + 1
    )
}

fn reserve_repo(
    state: &mut State,
    name: &str,
    how: &'static str,
    location: String,
) -> anyhow::Result<()> {
    if state.repo_names.contains_key(name) {
        bail!("repository name {name:?} is already used");
    }
    state.repo_names.insert(name.to_owned(), (how, location));
    Ok(())
}

/// ModuleThreadContext.buildModule (Bazel 9.3 :383-401): the built-in
/// dependency's name usage (Location.BUILTIN, `<builtin>`) collides with any
/// usage the MODULE file recorded, including ignored dev declarations.
fn reserve_builtin(state: &State, name: &str) -> Result<(), String> {
    match state.repo_names.get(name) {
        None => Ok(()),
        Some((how, location)) => Err(format!(
            "The repo name '{name}' cannot be defined as a built-in dependency at <builtin> as it is already defined {how} at {location}, '{name}' is a built-in dependency and cannot be used by any 'bazel_dep' or 'use_repo' directive"
        )),
    }
}

/// ModuleThreadContext.ExtensionUsageBuilder.buildUsage and buildModule
/// (Bazel 9.3 :292-319, :411-427): an override names a repository the module
/// can see (`bazel_tools` included when it is built in); an injected name is
/// never also imported from its extension; and no overriding repository is
/// itself overridden through an import. Errors carry the location of the call
/// Bazel attaches them to.
fn check_repo_overrides(state: &State, builtin_visible: bool) -> Result<(), String> {
    let mut overridden_imports: BTreeMap<&str, &RepoOverride> = BTreeMap::new();
    let mut overriding: Vec<(&str, &RepoOverride)> = Vec::new();
    for usage in &state.file.extension_usages {
        let extension = usage.identity.extension_name.as_str();
        for repo_override in &usage.repo_overrides {
            let visible = state.repo_names.contains_key(&repo_override.overriding)
                || (builtin_visible && repo_override.overriding == "bazel_tools");
            if !visible {
                let mut names: Vec<&str> = state.repo_names.keys().map(String::as_str).collect();
                if builtin_visible {
                    names.push("bazel_tools");
                }
                return Err(format!(
                    "{}: The repo exported as '{}' by module extension '{extension}' is overridden with '{}', but no repo is visible under this name{}",
                    repo_override.location,
                    repo_override.overridden,
                    repo_override.overriding,
                    crate::native_rules::did_you_mean(&repo_override.overriding, &names),
                ));
            }
            if let Some(import) = usage
                .imports
                .iter()
                .find(|import| import.exported_name == repo_override.overridden)
            {
                if !repo_override.must_exist {
                    let import_location = state
                        .repo_names
                        .get(&import.local_name)
                        .map_or("<unknown>", |(_, location)| location.as_str());
                    return Err(format!(
                        "{import_location}: Cannot import repo '{}' that has been injected into module extension '{extension}' at {}. Please refer to @{} directly.",
                        repo_override.overridden, repo_override.location, repo_override.overriding,
                    ));
                }
                overridden_imports.insert(import.local_name.as_str(), repo_override);
            }
            overriding.push((extension, repo_override));
        }
    }
    for (extension, repo_override) in overriding {
        if let Some(chained) = overridden_imports.get(repo_override.overriding.as_str()) {
            return Err(format!(
                "{}: The repo '{}' used as an override for '{}' in module extension '{extension}' is itself overridden with '{}' at {}, which is not supported.",
                repo_override.location,
                repo_override.overriding,
                repo_override.overridden,
                chained.overriding,
                chained.location,
            ));
        }
    }
    Ok(())
}
fn string(value: Value<'_>, name: &str) -> anyhow::Result<String> {
    value
        .unpack_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{name} must be a string"))
}
fn strings(value: Value<'_>, name: &str) -> anyhow::Result<Vec<String>> {
    if let Some(list) = ListRef::from_value(value) {
        list.iter().map(|v| string(v, name)).collect()
    } else if let Some(tuple) = TupleRef::from_value(value) {
        tuple.iter().map(|v| string(v, name)).collect()
    } else {
        bail!("{name} must be a list or tuple of strings")
    }
}
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}
fn validate_module_name(s: &str) -> anyhow::Result<()> {
    if !s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !s
            .as_bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        bail!("invalid module name {s:?}");
    }
    Ok(())
}
/// Validates user-provided repository names, unlike the broader canonical repository grammar.
pub(crate) fn validate_repo_name(s: &str) -> anyhow::Result<()> {
    if !s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("invalid repository name {s:?}");
    }
    Ok(())
}
/// Bazel's relaxed SemVer grammar, with u64 numeric identifiers. The original
/// version (including build metadata) is retained; selection normalizes later.
fn validate_version(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    let (main, build) = s.split_once('+').map_or((s, None), |(a, b)| (a, Some(b)));
    let (release, pre) = main
        .split_once('-')
        .map_or((main, None), |(a, b)| (a, Some(b)));
    let valid_chars = |p: &str, hyphens: bool| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || (hyphens && b == b'-'))
    };
    let identifiers = |p: &str| {
        p.split('.').all(|v| {
            !v.is_empty() && (!v.bytes().all(|b| b.is_ascii_digit()) || v.parse::<u64>().is_ok())
        })
    };
    if !valid_chars(release, false)
        || !identifiers(release)
        || pre.is_some_and(|v| !valid_chars(v, true) || !identifiers(v))
        || build.is_some_and(|v| !valid_chars(v, true))
    {
        bail!("invalid module version {s:?}");
    }
    Ok(())
}
fn validate_bazel_constraint(s: &str) -> anyhow::Result<()> {
    let version = [">=", "<=", ">", "<", "-"]
        .iter()
        .find_map(|p| s.strip_prefix(p));
    if !version.is_some_and(|v| {
        let parts: Vec<_> = v.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }) {
        bail!("invalid bazel_compatibility constraint {s:?}");
    }
    Ok(())
}
fn validate_url(s: &str, what: &str) -> anyhow::Result<()> {
    let Some((scheme, rest)) = s.split_once("://") else {
        bail!("invalid {what}: expected http, https, or file URL, got {s:?}");
    };
    if !matches!(scheme, "http" | "https" | "file")
        || rest.is_empty()
        || s.chars().any(char::is_whitespace)
        || (scheme != "file" && rest.starts_with('/'))
    {
        bail!("invalid {what} URL {s:?}");
    }
    Ok(())
}
fn validate_extension_label(s: &str) -> anyhow::Result<()> {
    // Full label/repository canonicalization belongs to the resolver. Relative
    // root-package labels are accepted, as they are by Bazel's use_extension.
    let body = if let Some(body) = s.strip_prefix('@') {
        let (_, body) = body
            .split_once("//")
            .ok_or_else(|| anyhow!("invalid extension label {s:?}"))?;
        body
    } else if let Some(body) = s.strip_prefix("//") {
        body
    } else if let Some(body) = s.strip_prefix(':') {
        body
    } else {
        s
    };
    if body.is_empty() || !body.ends_with(".bzl") || s.chars().any(char::is_whitespace) {
        bail!("invalid extension label {s:?}: expected a .bzl file label");
    }
    Ok(())
}
fn owned_json(value: Value<'_>, depth: usize, host: &mut HostBudget) -> anyhow::Result<Json> {
    if depth > 64 {
        bail!("extension tag attributes are cyclic or exceed nesting depth 64");
    }
    host.node().map_err(anyhow::Error::msg)?;
    if value.is_none() {
        return Ok(Json::Null);
    }
    if let Some(s) = value.unpack_str() {
        host.bytes(s.len()).map_err(anyhow::Error::msg)?;
        return Ok(Json::String(s.to_owned()));
    }
    if let Some(b) = value.unpack_bool() {
        return Ok(Json::Bool(b));
    }
    if matches!(value.get_type(), "int" | "float") {
        let number = value.to_json_value()?;
        if !number.is_number() {
            bail!("non-finite extension tag numbers are unsupported");
        }
        return Ok(number);
    }
    if let Some(list) = ListRef::from_value(value) {
        let mut values = Vec::new();
        for value in list.iter() {
            values.push(owned_json(value, depth + 1, host)?);
        }
        return Ok(Json::Array(values));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        let mut values = Vec::new();
        for value in tuple.iter() {
            values.push(owned_json(value, depth + 1, host)?);
        }
        return Ok(Json::Array(values));
    }
    if let Some(dict) = DictRef::from_value(value) {
        // A key that is not a string reaches the attribute's type check, which
        // reports Bazel's `dict key element` conversion error
        // (repository_rule::DICT_ENTRIES transport, packages/Type.java:574-577).
        if dict.iter().any(|(k, _)| k.unpack_str().is_none()) {
            let mut entries = Vec::new();
            for (k, v) in dict.iter() {
                let key = match k.unpack_str() {
                    Some(key) => {
                        host.string(key).map_err(anyhow::Error::msg)?;
                        Json::String(key.to_owned())
                    }
                    None => {
                        let repr = k.to_repr();
                        host.string(&repr).map_err(anyhow::Error::msg)?;
                        let mut marker = serde_json::Map::new();
                        marker.insert(
                            crate::repository_rule::KEY_REPR.to_owned(),
                            Json::String(repr),
                        );
                        marker.insert(
                            crate::repository_rule::KEY_TYPE.to_owned(),
                            Json::String(k.get_type().to_owned()),
                        );
                        Json::Object(marker)
                    }
                };
                let value = owned_json(v, depth + 1, host)?;
                entries.push(Json::Array(vec![key, value]));
            }
            let mut map = serde_json::Map::new();
            map.insert(
                crate::repository_rule::DICT_ENTRIES.to_owned(),
                Json::Array(entries),
            );
            return Ok(Json::Object(map));
        }
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key = k
                .unpack_str()
                .ok_or_else(|| anyhow!("tag dictionary key must be a string"))?;
            host.string(key).map_err(anyhow::Error::msg)?;
            let value = owned_json(v, depth + 1, host)?;
            map.insert(key.to_owned(), value);
        }
        return Ok(Json::Object(map));
    }
    bail!(
        "unsupported extension tag attribute type {}; expected primitives, lists, tuples, or string-keyed dictionaries",
        value.get_type()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(s: &str) -> ModuleFile {
        evaluate_source("MODULE.bazel", s.to_owned()).unwrap()
    }

    #[test]
    fn module_file_values_have_bazel_reprs() {
        // BuiltinFunction.java:192-203; ModuleFileGlobals.java:549,579,833
        // classes without repr overrides (StarlarkValue.java:50-52).
        run(r#"
(repr(bazel_dep) == '<built-in function bazel_dep>') or fail(repr(bazel_dep))
e = use_extension('//:ext.bzl', 'ext')
(repr(e) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$ModuleExtensionProxy>') or fail(repr(e))
(repr(e.tag) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$ModuleExtensionProxy$TagCallable>') or fail(repr(e.tag))
r = use_repo_rule('//:repo.bzl', 'repo')
(repr(r) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$RepoRuleProxy>') or fail(repr(r))
"#);
    }

    #[test]
    fn sets_in_module_globals_record_only_explicit_sequence_conversions() {
        let file = run(r#"
Set = set
names = Set(['b', 'a', 'b'])
alias = names
names.update(set(['c']))
(list(alias) == ['b', 'a', 'c'] and type(Set) == 'builtin_function_or_method' and dir(Set) == [] and not hasattr(Set, 'type')) or fail('set globals')
module(name='root')
[bazel_dep(name=name, version='1') for name in names]
e = use_extension('//:ext.bzl', 'ext')
e.tag(names=list(names))
"#);
        assert_eq!(
            file.dependencies
                .iter()
                .map(|dep| dep.name.as_str())
                .collect::<Vec<_>>(),
            ["b", "a", "c"]
        );
        assert_eq!(
            file.extension_usages[0].tags[0].attributes["names"],
            serde_json::json!(["b", "a", "c"])
        );
        for source in [
            "e=use_extension('//:ext.bzl','ext')\ne.tag(names=set(['a']))",
            "r=use_repo_rule('//:repo.bzl','repo')\nr(name='repo', names=set(['a']))",
        ] {
            fails(source, "unsupported extension tag attribute");
        }
        for source in ["x = {set: 1}", "x = set([set])"] {
            fails(source, "unhashable type: 'builtin_function_or_method'");
        }
        fails("x = set.type", "type");
        // EvalUtils.index (EvalUtils.java:498-500).
        fails(
            "x = set[int]",
            "type 'builtin_function_or_method' has no operator [](builtin_function_or_method)",
        );
        fails("names=set()\nnames |= set(['c'])", "may not be reassigned");
        fails("load('//:defs.bzl', 'Set')", "load");
        fails("def f():\n    return set()", "def");
    }

    #[test]
    fn builtin_finalization_preserves_declared_metadata_and_appends_in_order() {
        for source in [
            "",
            "module(name='root')",
            "module(name='root')\nbazel_dep(name='z',version='1')\nbazel_dep(name='a',repo_name=None)\nbazel_dep(name='dev',dev_dependency=True)",
        ] {
            for context in contexts() {
                let declared = in_context(source, context).unwrap();
                assert!(declared.implicit_dependencies.is_empty());
                let mut file = evaluate_source_with_builtins(
                    "MODULE.bazel",
                    source.to_owned(),
                    context,
                    BuiltinInjection::AppendBazelTools,
                )
                .unwrap();
                assert_eq!(file.dependencies, declared.dependencies);
                assert_eq!(
                    file.effective_dependencies().count(),
                    declared.dependencies.len() + 1
                );
                let builtin = file.effective_dependencies().last().unwrap();
                assert_eq!(builtin.name, "bazel_tools");
                assert_eq!(builtin.version, "");
                assert_eq!(
                    builtin.repo_name,
                    DependencyRepoName::Explicit("bazel_tools".to_owned())
                );
                assert_eq!(builtin.max_compatibility_level, -1);
                assert!(!builtin.dev_dependency);
                assert_eq!(
                    file.effective_dependencies()
                        .take(declared.dependencies.len())
                        .collect::<Vec<_>>(),
                    declared.dependencies.iter().collect::<Vec<_>>()
                );
                file.implicit_dependencies.clear();
                assert_eq!(file, declared);
            }
        }
    }

    /// Bazel 9.3 ModuleThreadContext.buildModule (:383-401) with the colliding
    /// usage's `how` (ModuleFileGlobals :175-181, :317, :666-676, :867).
    fn builtin_collision(how: &str, location: &str) -> String {
        format!(
            "The repo name 'bazel_tools' cannot be defined as a built-in dependency at <builtin> as it is already defined {how} at {location}, 'bazel_tools' is a built-in dependency and cannot be used by any 'bazel_dep' or 'use_repo' directive"
        )
    }

    #[test]
    fn builtin_finalization_reserves_names_including_ignored_declarations() {
        for (source, how, location) in [
            (
                "module(name='bazel_tools')",
                "as the current module name",
                "MODULE.bazel:1:7",
            ),
            (
                "module(name='root',repo_name='bazel_tools')",
                "as the module's own repo name",
                "MODULE.bazel:1:7",
            ),
            (
                "bazel_dep(name='bazel_tools',version='1')",
                "by a bazel_dep",
                "MODULE.bazel:1:10",
            ),
            (
                "bazel_dep(name='other',repo_name='bazel_tools',dev_dependency=True)",
                "by a bazel_dep",
                "MODULE.bazel:1:10",
            ),
            (
                "e=use_extension('//:e.bzl','e')\nuse_repo(e,'bazel_tools')",
                "by a use_repo() call",
                "MODULE.bazel:2:9",
            ),
            (
                "e=use_extension('//:e.bzl','e',dev_dependency=True)\nuse_repo(e,bazel_tools='exported')",
                "by a use_repo() call",
                "MODULE.bazel:2:9",
            ),
            (
                "r=use_repo_rule('//:r.bzl','r')\nr(name='bazel_tools')",
                "by a repo rule",
                "MODULE.bazel:2:2",
            ),
        ] {
            for context in contexts() {
                for injection in [
                    BuiltinInjection::AppendBazelTools,
                    BuiltinInjection::ReserveBazelTools,
                ] {
                    let error = evaluate_source_with_builtins(
                        "MODULE.bazel",
                        source.to_owned(),
                        context,
                        injection,
                    )
                    .unwrap_err();
                    assert_eq!(error, builtin_collision(how, location), "{source}");
                }
                // Profile-free recording still permits these names without injecting anything.
                assert!(
                    in_context(source, context)
                        .unwrap()
                        .implicit_dependencies
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn reserve_only_profile_records_no_implicit_dependency() {
        for context in contexts() {
            let file = evaluate_source_with_builtins(
                "MODULE.bazel",
                "module(name='root')\nbazel_dep(name='a',version='1')".to_owned(),
                context,
                BuiltinInjection::ReserveBazelTools,
            )
            .unwrap();
            assert!(file.implicit_dependencies.is_empty());
            assert_eq!(
                file,
                in_context(
                    "module(name='root')\nbazel_dep(name='a',version='1')",
                    context
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn actual_builtin_has_no_implicit_self_dependency() {
        let source = "module(name='bazel_tools')\nbazel_dep(name='rules_license',version='1.0.0')";
        let context = EvaluationContext {
            role: ModuleRole::Dependency,
            ignore_dev_dependencies: false,
            isolated_extension_usages: false,
            print_is_noop: false,
        };
        let actual = evaluate_source_with_builtins(
            "@bazel_tools//:MODULE.bazel",
            source.to_owned(),
            context,
            BuiltinInjection::ActualBazelTools,
        )
        .unwrap();
        assert_eq!(actual, in_context(source, context).unwrap());
        assert!(actual.implicit_dependencies.is_empty());
        // A root with the same declaration is not the actual builtin key.
        assert!(
            evaluate_source_with_builtins(
                "MODULE.bazel",
                source.to_owned(),
                EvaluationContext::default(),
                BuiltinInjection::AppendBazelTools,
            )
            .unwrap_err()
            .contains("already defined as the current module name at MODULE.bazel:1:7")
        );
    }

    #[test]
    fn builtin_finalization_follows_deferred_attribute_validation() {
        let source = "module(name='bazel_tools')\ne=use_extension('//:e.bzl','e')\ne.tag(value=e)";
        let error = evaluate_source_with_builtins(
            "MODULE.bazel",
            source.to_owned(),
            EvaluationContext::default(),
            BuiltinInjection::AppendBazelTools,
        )
        .unwrap_err();
        assert!(
            error.contains("unsupported extension tag attribute"),
            "{error}"
        );
    }

    #[test]
    fn repository_source_profile_is_deferred_and_captured_from_evaluated_module() {
        let file = run("module(name='main')\nunused = str(len('é'))\n");
        assert_eq!(file.module.unwrap().name, "main");
        assert!(file.source_profile_error.is_some());
        // Lexer errors are eager (Bazel fails every command, exit 48).
        for (expression, message) in [
            (
                "'\\x41'",
                "MODULE.bazel:2:12-13: invalid escape sequence: \\x.",
            ),
            (
                "'\\400'",
                "MODULE.bazel:2:14-15: octal escape sequence out of range",
            ),
        ] {
            let error = evaluate_source(
                "MODULE.bazel",
                format!("module(name='main')\nunused = {expression}\n"),
            )
            .unwrap_err();
            assert!(error.contains(message), "{expression}: {error}");
        }
        // Octal escapes denote the same Latin-1 char in Bazel and natively.
        let file = run(
            "module(name='main')\n# Unicode café comment is not a value\nx = r'\\377'\ny = '\\\\377'\nz = '\\377'\n",
        );
        assert!(file.source_profile_error.is_none());
        // Bazel's MODULE universe has no chr: a resolver error, not deferred.
        let error = evaluate_source(
            "MODULE.bazel",
            "module(name='main')\nunused = str(len(chr(233)))\n".to_owned(),
        )
        .unwrap_err();
        assert!(
            error.contains("MODULE.bazel:2:18") && error.contains("name 'chr' is not defined"),
            "{error}"
        );
    }
    fn fails(s: &str, expected: &str) {
        let error = evaluate_source("MODULE.bazel", s.to_owned()).unwrap_err();
        assert!(
            error.contains(expected),
            "{s}\nexpected {expected:?}, got {error}"
        );
    }

    #[test]
    fn assignments_comprehensions_and_owned_tag_data() {
        let f = run(r#"
module(name="root", version="1.2-pre.3+build", bazel_compatibility=[">=7.0.0"])
versions = ["1.0", "2.0"]
[bazel_dep(name="dep" + str(i), version=v) for i, v in enumerate(versions)]
x = use_extension("@dep0//:ext.bzl", "download")
attrs = {"nested": [True, None, 42, {"url": "https://invalid.invalid"}]}
x.download(name="pkg", versions=[v + ".1" for v in versions], attrs=attrs)
attrs["nested"].append("later mutation")
use_repo(x, "pkg", local="other")
"#);
        assert_eq!(f.module.unwrap().compatibility_level, 0);
        assert_eq!(f.dependencies.len(), 2);
        assert_eq!(f.dependencies[1].version, "2.0");
        let u = &f.extension_usages[0];
        assert_eq!(u.tags[0].name, "download");
        assert_eq!(
            u.tags[0].attributes["versions"],
            serde_json::json!(["1.0.1", "2.0.1"])
        );
        assert_eq!(
            u.tags[0].attributes["attrs"]["nested"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(u.imports[1].local_name, "local");
        assert_eq!(u.imports[1].exported_name, "other");
    }

    #[test]
    fn ordered_dicts_and_nested_tuples_retain_final_module_values_on_both_routes() {
        for declaration in [
            "call=use_repo_rule('raw','rule')",
            "x=use_extension('//:e.bzl','e')\ncall=x.tag",
        ] {
            let file = run(&format!(
                "{declaration}\nwords=['before']\npayload={{'z': (words, ('nested',)), 'a': ()}}\ncall(name='first', z=(payload,), a=[('list tuple',)])\nwords.append('between')\ncall(name='second', z=(payload,))\nwords.append('after')"
            ));
            let tags = &file.extension_usages[0].tags;
            assert_eq!(
                tags[0]
                    .attributes
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["a", "name", "z"]
            );
            assert_eq!(tags[0].attributes["a"], serde_json::json!([["list tuple"]]));
            for tag in tags {
                let payload = &tag.attributes["z"][0];
                assert_eq!(
                    payload
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    ["z", "a"]
                );
                assert_eq!(
                    payload["z"],
                    serde_json::json!([["before", "between", "after"], ["nested"]])
                );
                assert_eq!(payload["a"], serde_json::json!([]));
            }
        }
    }

    #[test]
    fn module_attribute_conversion_charges_tuple_nodes_and_borrowed_strings() {
        for declaration in [
            "call=use_repo_rule('raw','rule')",
            "x=use_extension('//:e.bzl','e')\ncall=x.tag",
        ] {
            for value in [
                "('x' * (16 * 1024 * 1024),)",
                "{'x' * (16 * 1024 * 1024): ()}",
                "('x' * (1024 * 1024),) * 8",
            ] {
                fails(
                    &format!("{declaration}\ncall(name='repo', value={value})"),
                    "host preparation weighted byte limit",
                );
            }
            fails(
                &format!("{declaration}\ncall(name='repo', value=(None,) * 100000)"),
                "host preparation node limit",
            );
            fails(
                &format!("{declaration}\nxs=[]\nt=(xs,)\ncall(name='repo', value=t)\nxs.append(t)"),
                "cyclic or exceed nesting depth",
            );
        }
    }

    #[test]
    fn module_host_budget_is_aggregate_across_attributes_calls_and_usages() {
        let prefix = "x=use_extension('//:e.bzl','e')\nr=use_repo_rule('raw','rule')\ntext='x' * (5 * 1024 * 1024)";
        // Each individual payload fits; retaining it twice must not reset the budget.
        run(&format!("{prefix}\nx.tag(value=(text,))"));
        for calls in [
            "x.tag(a=(text,), z=(text,))",
            "x.tag(value=(text,))\nx.tag(value=(text,))",
            "x.tag(value=(text,))\nr(name='repo', value=(text,))",
        ] {
            fails(
                &format!("{prefix}\n{calls}"),
                "host preparation weighted byte limit",
            );
        }
        fails(
            "x=use_extension('//:e.bzl','e')\nx.tag(**{'x' * (16 * 1024 * 1024): None})",
            "host preparation weighted byte limit",
        );
    }

    #[test]
    fn declarations_record_module_wide_order_and_left_paren_locations() {
        let f = run(
            "module(name='m')\ne = use_extension('//:a.bzl', 'a')\nr = use_repo_rule('//:r.bzl', 'rule')\nr (name = 'x')\nd  =  use_extension ( '@m//:b.bzl', 'b', dev_dependency = True)\nf=use_extension('//:a.bzl','a',dev_dependency=True)\n",
        );
        let declarations: Vec<_> = f
            .extension_usages
            .iter()
            .flat_map(|usage| &usage.declarations)
            .map(|declaration| (declaration.ordinal, declaration.location.as_str()))
            .collect();
        assert_eq!(
            declarations,
            [
                (0, "MODULE.bazel:2:18"),
                (3, "MODULE.bazel:6:16"),
                (1, "MODULE.bazel:4:3"),
                (2, "MODULE.bazel:5:21"),
            ]
        );
    }

    #[test]
    fn dependency_defaults_aliases_and_nodep_are_distinct() {
        let f = run(
            "bazel_dep(name='a')\nbazel_dep(name='b',repo_name='')\nbazel_dep(name='c',repo_name=None,dev_dependency=True)\nbazel_dep(name='d',repo_name='alias',max_compatibility_level=3)",
        );
        assert_eq!(f.dependencies[0].repo_name, DependencyRepoName::Default);
        assert_eq!(f.dependencies[0].max_compatibility_level, -1);
        assert_eq!(
            f.dependencies[1].repo_name,
            DependencyRepoName::Explicit(String::new())
        );
        assert_eq!(f.dependencies[1].effective_repo_name(), Some("b"));
        assert_eq!(f.dependencies[2].effective_repo_name(), None);
        assert!(f.dependencies[2].dev_dependency);
        assert_eq!(f.dependencies[3].effective_repo_name(), Some("alias"));
    }

    #[test]
    fn isolate_requires_experimental_isolated_extension_usages() {
        // BuiltinFunction.java:369-375 with ModuleFileGlobals.java:436-449:
        // also an explicit isolate=False fails without the flag.
        for value in ["True", "False"] {
            let error = evaluate_source(
                "MODULE.bazel",
                format!("e = use_extension('//:ext.bzl', 'ext', isolate={value})"),
            )
            .unwrap_err();
            assert!(
                error.contains("in call to use_extension(), parameter 'isolate' is experimental and thus unavailable with the current flags. It may be enabled by setting --experimental_isolated_extension_usages"),
                "{error}"
            );
        }
        assert!(
            evaluate_source(
                "MODULE.bazel",
                "e = use_extension('//:ext.bzl', 'ext')".into()
            )
            .is_ok()
        );
    }

    #[test]
    fn merged_extensions_preserve_declaration_dev_flags_and_isolation() {
        let run = |s: &str| {
            evaluate_source_with_context(
                "MODULE.bazel",
                s.to_owned(),
                EvaluationContext {
                    isolated_extension_usages: true,
                    ..EvaluationContext::default()
                },
            )
            .unwrap()
        };
        let f = run(r#"
a = use_extension("//:ext.bzl", "ext", dev_dependency=True)
b = use_extension("//:ext.bzl", "ext")
a.tag(x=1)
b.tag(x=2)
use_repo(a, dev="a")
use_repo(b, prod="b")
c = use_extension("//:ext.bzl", "ext", isolate=True)
d = use_extension("//:ext.bzl", "ext", isolate=True)
c.tag()
d.tag()
"#);
        assert_eq!(f.extension_usages.len(), 3);
        let u = &f.extension_usages[0];
        assert_eq!(u.declarations.len(), 2);
        assert!(u.tags[0].dev_dependency);
        assert!(!u.tags[1].dev_dependency);
        assert_eq!(u.tags[1].declaration, 1);
        assert!(u.imports[0].dev_dependency);
        assert!(!u.imports[1].dev_dependency);
        assert_ne!(
            f.extension_usages[1].identity,
            f.extension_usages[2].identity
        );
    }

    #[test]
    fn tag_ordinals_preserve_interleaving_across_raw_extension_usages() {
        let f = run(r#"
module(name="root")
foo = use_extension("//:ext.bzl", "ext")
bar = use_extension("@root//:ext.bzl", "ext")
foo.t1()
bar.t2()
foo.t3()
"#);
        assert_eq!(f.extension_usages.len(), 2);
        assert_eq!(f.extension_usages[0].identity.label, "//:ext.bzl");
        assert_eq!(f.extension_usages[1].identity.label, "@root//:ext.bzl");
        let mut tags = f
            .extension_usages
            .iter()
            .flat_map(|usage| &usage.tags)
            .collect::<Vec<_>>();
        assert_eq!(
            tags.iter().map(|tag| tag.ordinal).collect::<Vec<_>>(),
            [0, 2, 1]
        );
        tags.sort_by_key(|tag| tag.ordinal);
        assert_eq!(
            tags.iter().map(|tag| tag.name.as_str()).collect::<Vec<_>>(),
            ["t1", "t2", "t3"]
        );
    }

    #[test]
    fn tag_ordinals_include_innate_calls_without_snapshotting_mutable_attributes() {
        let f = run(r#"
foo = use_extension("//:ext.bzl", "ext")
rule = use_repo_rule("//:repo.bzl", "rule")
attrs = {"nested": ["before"]}
foo.t1(attrs=attrs)
rule(name="first", attrs=attrs)
attrs["nested"].append("between")
foo.t2(attrs=attrs)
rule(name="second", attrs=attrs)
attrs["nested"].append("after")
"#);
        assert_eq!(f.extension_usages.len(), 2);
        assert!(!f.extension_usages[0].identity.is_repo_rule());
        assert_direct_usage(
            &f.extension_usages[1],
            &[("first", false), ("second", false)],
        );
        let mut tags = f
            .extension_usages
            .iter()
            .flat_map(|usage| &usage.tags)
            .collect::<Vec<_>>();
        assert_eq!(
            tags.iter().map(|tag| tag.ordinal).collect::<Vec<_>>(),
            [0, 2, 1, 3]
        );
        tags.sort_by_key(|tag| tag.ordinal);
        assert_eq!(
            tags.iter().map(|tag| tag.name.as_str()).collect::<Vec<_>>(),
            ["t1", "repo", "t2", "repo"]
        );
        for tag in tags {
            assert_eq!(
                tag.attributes["attrs"],
                serde_json::json!({"nested": ["before", "between", "after"]})
            );
        }
    }

    #[test]
    fn tag_ordinals_skip_ignored_dev_tags_and_direct_calls() {
        for context in contexts().into_iter().skip(1) {
            let f = in_context(
                r#"
dev = use_extension("//:ext.bzl", "ext", dev_dependency=True)
foo = use_extension("//:ext.bzl", "ext")
rule = use_repo_rule("//:repo.bzl", "rule")
ignored = use_repo_rule("//:ignored.bzl", "rule")
dev.skipped(value=dev)
ignored(name="first", dev_dependency=True, value=ignored)
foo.t1()
dev.skipped(value=dev)
rule(name="first", dev_dependency=True, value=rule)
rule(name="first")
ignored(name="first", dev_dependency=True, value=ignored)
foo.t2()
dev.skipped(value=dev)
"#,
                context,
            )
            .unwrap();
            assert_eq!(f.extension_usages.len(), 2);
            let ordinary = &f.extension_usages[0];
            assert_eq!(ordinary.declarations.len(), 2);
            assert_eq!(ordinary.tags.len(), 2);
            assert_eq!(ordinary.tags[0].name, "t1");
            assert_eq!(ordinary.tags[0].ordinal, 0);
            assert_eq!(ordinary.tags[1].name, "t2");
            assert_eq!(ordinary.tags[1].ordinal, 2);
            assert!(ordinary.tags.iter().all(|tag| tag.declaration == 1));
            assert_direct_usage(&f.extension_usages[1], &[("first", false)]);
            assert_eq!(f.extension_usages[1].tags[0].ordinal, 1);
        }
    }

    #[test]
    fn overrides_record_without_running_commands_or_accessing_paths() {
        let f = run(r#"
local_path_override(module_name="local", path="/does/not/exist")
single_version_override(module_name="single", version="1.0", registry="file:///no-registry", patches=["//:fix.patch"], patch_cmds=["exit 99"], patch_strip=1)
multiple_version_override(module_name="multi", versions=["1", "2"])
git_override(module_name="git", remote="https://invalid.invalid/repo", branch="main", init_submodules=True)
archive_override(module_name="archive", urls="https://invalid.invalid/a.tar.gz", strip_prefix="src", archive_type="tar.gz")
"#);
        assert_eq!(f.overrides.len(), 5);
        let ModuleOverride::SingleVersion {
            patches, registry, ..
        } = &f.overrides["single"]
        else {
            panic!()
        };
        assert_eq!(patches.patch_cmds, ["exit 99"]);
        assert_eq!(registry, "file:///no-registry");
        let ModuleOverride::Archive { urls, .. } = &f.overrides["archive"] else {
            panic!()
        };
        assert_eq!(urls, &["https://invalid.invalid/a.tar.gz"]);
    }

    #[test]
    fn registration_kinds_order_and_dev_flags() {
        let f = run(
            "register_toolchains('//tc:all','@sdk//:tc',dev_dependency=True)\nregister_execution_platforms('//platforms:all')",
        );
        assert_eq!(f.registrations[0].labels, ["//tc:all", "@sdk//:tc"]);
        assert!(f.registrations[0].dev_dependency);
        assert_eq!(
            f.registrations[1].kind,
            RegistrationKind::ExecutionPlatforms
        );
        assert!(!f.registrations[1].dev_dependency);
    }

    #[test]
    fn duplicate_and_out_of_order_directives_fail() {
        fails("module()\nmodule()", "only be called once");
        fails(
            "bazel_dep(name='a')\nmodule()",
            "before other MODULE directives",
        );
        fails(
            "local_path_override(module_name='a',path='a')\ngit_override(module_name='a',remote='x',tag='v1')",
            "duplicate override",
        );
        fails(
            "bazel_dep(name='a')\nbazel_dep(name='b',repo_name='a')",
            "already used",
        );
        fails(
            "x=use_extension('//:e.bzl','e')\nuse_repo(x,'a',a='b')",
            "already used",
        );
    }

    #[test]
    fn invalid_types_names_versions_registry_and_unknown_arguments_fail() {
        for (source, expected) in [
            (
                "bazel_dep(name='a',typo=True)",
                "unknown or unsupported argument",
            ),
            ("bazel_dep(name='a',dev_dependency=1)", "must be a bool"),
            ("bazel_dep(name='a',repo_name=42)", "must be a string"),
            ("module(compatibility_level=True)", "must be a 32-bit int"),
            ("module(repo_name=None)", "must be a string"),
            ("bazel_dep(name='Bad')", "invalid module name"),
            (
                "bazel_dep(name='a',version='1..2')",
                "invalid module version",
            ),
            (
                "bazel_dep(name='a',version='1/2')",
                "invalid module version",
            ),
            (
                "bazel_dep(name='a',version='18446744073709551616')",
                "invalid module version",
            ),
            (
                "module(bazel_compatibility=['7.0.0'])",
                "invalid bazel_compatibility",
            ),
            (
                "single_version_override(module_name='a',registry='not-url')",
                "invalid registry",
            ),
            (
                "single_version_override(module_name='a',patch_strip=-1)",
                "at least 0",
            ),
            (
                "multiple_version_override(module_name='a',versions=['1'])",
                "at least 2",
            ),
            (
                "git_override(module_name='a',remote='r',tag='t',commit='c')",
                "exactly one",
            ),
            (
                "archive_override(module_name='a',urls=[1])",
                "must be a string",
            ),
            (
                "archive_override(module_name='a',urls=[])",
                "must not be empty",
            ),
            (
                "archive_override(module_name='a',urls='https://a/a',sha256='x')",
                "64 hexadecimal",
            ),
            (
                "register_toolchains(':relative')",
                "absolute target pattern",
            ),
            ("use_repo('not a proxy','a')", "extension proxy"),
            ("x=use_extension('//:x.bzl','x')\nx.tag(1)", "positional"),
            (
                "x=use_extension('//:x.bzl','x')\nx.tag(value=x)",
                "unsupported extension tag attribute",
            ),
        ] {
            fails(source, expected);
        }
        // A non-string dict key is recorded for the attribute's type check,
        // which reports Bazel's `dict key element` conversion error.
        let file = run("x=use_extension('//:x.bzl','x')\nx.tag(value={'a':'b', 1:'x'})");
        assert_eq!(
            file.extension_usages[0].tags[0].attributes["value"],
            serde_json::json!({crate::repository_rule::DICT_ENTRIES: [
                ["a", "b"],
                [{crate::repository_rule::KEY_REPR: "1", crate::repository_rule::KEY_TYPE: "int"}, "x"],
            ]})
        );
    }

    #[test]
    fn module_syntax_kwargs_require_an_outer_dict_literal() {
        for expression in [
            "kwargs",
            "dict(name='repo')",
            "{k:v for k,v in [('name','repo')]}",
            "({'name':'repo'} if True else {})",
            "[{'name':'repo'}][0]",
        ] {
            fails(
                &format!(
                    "r=use_repo_rule('raw','rule')\nkwargs={{'name':'repo'}}\nr(**{expression})"
                ),
                "**kwargs must be a dict literal",
            );
        }
    }

    #[test]
    fn module_syntax_star_args_are_forbidden_even_when_empty_or_unreachable() {
        for source in [
            "module(*[])",
            "module(*())",
            "use_repo_rule(*['raw','rule'])",
            "xs=[]\nxs.append(*[1])",
            "[str(*[1]) for i in []]",
            "fail('must not evaluate')\n[str(*[]) for i in []]",
        ] {
            fails(source, "*args are not allowed");
        }
    }

    #[test]
    fn module_syntax_checks_nested_dead_calls_before_evaluation() {
        for source in [
            "kwargs={}\n[str(dict(**kwargs)) for i in []]",
            "kwargs={}\nxs=[]\n[xs.append(dict(**kwargs)) for i in []]",
            "kwargs={}\n[dict(**{}) for i in [] if dict(**kwargs)]",
            "kwargs={}\ndict(**{'value':[dict(**kwargs) for i in []]})",
            "kwargs={}\nfail('must not evaluate')\n[dict(**kwargs) for i in []]",
            "kwargs={}\nxs=[]\nxs[dict(**kwargs)]=1",
        ] {
            fails(source, "**kwargs must be a dict literal");
        }
    }

    #[test]
    fn module_syntax_global_duplicate_bindings_are_static_errors() {
        for source in [
            "x=1\nx=2",
            "xs=[]\nxs+=[]",
            "x=1\n(x,y)=(2,3)",
            "(x,(y,x))=(1,(2,3))",
            "[x,y]=[1,2]\ny=3",
            "x=1; x=2",
            "fail('must not evaluate')\nx=1\nx=2",
        ] {
            fails(source, "global binding");
        }
        let error = evaluate_source("syntax.MODULE.bazel", "x=1\nx=2".to_owned()).unwrap_err();
        assert!(error.contains("syntax.MODULE.bazel:1:"), "{error}");
        assert!(error.contains("syntax.MODULE.bazel:2:"), "{error}");
    }

    #[test]
    fn module_syntax_dict_literals_mutations_and_comprehension_scopes_remain_legal() {
        let f = run(r#"
key="values"
xs=["before"]
holder={"values":xs,"count":0}
r=use_repo_rule("raw","rule")
r(name="repo",**{key:holder["values"],"computed":[x for x in [1,2]]})
holder["values"]=["replacement"]
holder["count"]+=1
xs.append("after")
xs[0]="updated"
xs.append("last")
x="global"
ys=[x for x in [1,2]]
zs={str(x):x for x in [3,4]}
ws=[(x,y) for x,y in [(5,6)]]
y="new global"
module="shadow predeclared"
r(name="second",x=x,y=y,module=module,ys=ys,zs=zs,ws=[str(v) for v in ws],count=holder["count"])
"#);
        let tags = &f.extension_usages[0].tags;
        assert_eq!(
            tags[0].attributes["values"],
            serde_json::json!(["updated", "after", "last"])
        );
        assert_eq!(tags[0].attributes["computed"], serde_json::json!([1, 2]));
        assert_eq!(tags[1].attributes["x"], "global");
        assert_eq!(tags[1].attributes["y"], "new global");
        assert_eq!(tags[1].attributes["module"], "shadow predeclared");
        assert_eq!(tags[1].attributes["count"], 1);
    }

    #[test]
    fn prohibited_language_and_unsupported_features_fail_explicitly() {
        for source in [
            "load('//:evil.bzl','evil')",
            "def f():\n    pass",
            "for x in []:\n    pass",
            "if True:\n    module()",
            "f=lambda x: x",
        ] {
            assert!(
                evaluate_source("MODULE.bazel", source.to_owned()).is_err(),
                "{source}"
            );
        }
        fails(
            "include('//:other.MODULE.bazel')",
            "include() is unsupported",
        );
        fails("Label('//:x')", "Label() is unsupported");
        for source in ["native.foo()", "rule()", "provider()"] {
            assert!(evaluate_source("MODULE.bazel", source.to_owned()).is_err());
        }
    }

    #[test]
    fn cyclic_tag_values_report_error_instead_of_recursing_forever() {
        fails(
            "x=use_extension('//:x.bzl','x')\na=[]\na.append(a)\nx.tag(value=a)",
            "cyclic or exceed nesting depth",
        );
    }

    fn contexts() -> [EvaluationContext; 4] {
        [
            EvaluationContext::default(),
            EvaluationContext {
                role: ModuleRole::Root,
                ignore_dev_dependencies: true,
                isolated_extension_usages: false,
                print_is_noop: false,
            },
            EvaluationContext {
                role: ModuleRole::Dependency,
                ignore_dev_dependencies: false,
                isolated_extension_usages: false,
                print_is_noop: false,
            },
            EvaluationContext {
                role: ModuleRole::Dependency,
                ignore_dev_dependencies: true,
                isolated_extension_usages: false,
                print_is_noop: false,
            },
        ]
    }

    fn in_context(source: &str, context: EvaluationContext) -> Result<ModuleFile, String> {
        evaluate_source_with_context("MODULE.bazel", source.to_owned(), context)
    }

    #[test]
    fn module_print_binds_like_print_in_every_context() {
        for print_is_noop in [false, true] {
            let context = EvaluationContext {
                print_is_noop,
                ..EvaluationContext::default()
            };
            in_context("print('a', 1, [2], sep = '|')\nprint()", context).unwrap();
            let error = in_context("print('a', end = '')", context).unwrap_err();
            assert!(error.contains("end"), "{error}");
            let error = in_context("print('a', sep = 1)", context).unwrap_err();
            assert!(error.contains("sep"), "{error}");
            let error = in_context("x = type(print)\nprint(x)\nprint(x.foo)", context).unwrap_err();
            assert!(error.contains("foo"), "{error}");
        }
    }

    #[test]
    fn repo_overrides_are_ignored_where_dev_dependencies_are() {
        // hermetic_launcher 0.0.8 MODULE.bazel:84-90 (a rules_rs dependency).
        let source = "bazel_dep(name='bzip2', version='1.0.8.bcr.3')\n\
            crate=use_extension('//:ext.bzl','crate',dev_dependency=True)\n\
            crate.annotation(crate='bzip2-sys', deps=['@bzip2//:bz2'])\n\
            inject_repo(crate, 'bzip2')\n\
            override_repo(crate, foo='bzip2')\n";
        for context in contexts() {
            let file = in_context(source, context).unwrap();
            let usage = &file.extension_usages[0];
            if context.effective_ignore_dev_dependencies() {
                assert!(usage.tags.is_empty());
                assert!(usage.repo_overrides.is_empty());
                continue;
            }
            assert_eq!(usage.tags.len(), 1);
            let recorded: Vec<_> = usage
                .repo_overrides
                .iter()
                .map(|o| {
                    (
                        o.overridden.as_str(),
                        o.overriding.as_str(),
                        o.must_exist,
                        o.location.as_str(),
                    )
                })
                .collect();
            assert_eq!(
                recorded,
                [
                    ("bzip2", "bzip2", false, "MODULE.bazel:4:12"),
                    ("foo", "bzip2", true, "MODULE.bazel:5:14"),
                ]
            );
        }
    }

    #[test]
    fn root_repo_overrides_follow_bazel_validation() {
        let prefix = "bazel_dep(name='dep', version='1.0')\nx=use_extension('//:ext.bzl','x')\n";
        // A positional name overrides itself, a keyword its key, in call order.
        let file = run(&format!(
            "{prefix}inject_repo(x, 'dep', other='dep')\noverride_repo(x, gen='dep')"
        ));
        let recorded: Vec<_> = file.extension_usages[0]
            .repo_overrides
            .iter()
            .map(|o| (o.overridden.as_str(), o.overriding.as_str(), o.must_exist))
            .collect();
        assert_eq!(
            recorded,
            [
                ("dep", "dep", false),
                ("other", "dep", false),
                ("gen", "dep", true),
            ]
        );
        // ModuleThreadContext.addRepoOverride (:244-261).
        fails(
            &format!("{prefix}inject_repo(x, 'dep')\ninject_repo(x, dep='dep')"),
            "The repo exported as 'dep' by module extension 'x' is already overridden with 'dep' at MODULE.bazel:3:12",
        );
        fails(
            &format!("{prefix}inject_repo(x, '@dep')"),
            "invalid repository name",
        );
        fails(
            &format!("{prefix}inject_repo('x')"),
            "inject_repo requires an extension proxy",
        );
        fails(
            &format!("{prefix}override_repo(x, gen=1)"),
            "overriding repository name must be a string",
        );
        // ExtensionUsageBuilder.buildUsage (:292-319).
        fails(
            &format!("{prefix}inject_repo(x, 'nope')"),
            "MODULE.bazel:3:12: The repo exported as 'nope' by module extension 'x' is overridden with 'nope', but no repo is visible under this name",
        );
        fails(
            &format!("{prefix}use_repo(x, local='gen')\ninject_repo(x, gen='dep')"),
            "MODULE.bazel:3:9: Cannot import repo 'gen' that has been injected into module extension 'x' at MODULE.bazel:4:12. Please refer to @dep directly.",
        );
        // An imported name can be overridden (the import then follows it), but
        // an overridden import cannot override another repository (:411-427).
        run(&format!(
            "{prefix}use_repo(x, local='gen')\noverride_repo(x, gen='dep')"
        ));
        fails(
            &format!(
                "{prefix}use_repo(x, 'gen')\noverride_repo(x, gen='dep')\ny=use_extension('//:ext.bzl','y')\ninject_repo(y, z='gen')"
            ),
            "MODULE.bazel:6:12: The repo 'gen' used as an override for 'z' in module extension 'y' is itself overridden with 'dep' at MODULE.bazel:4:14, which is not supported.",
        );
    }

    #[test]
    fn ignored_extension_tags_skip_value_capture_but_import_names_stay_reserved() {
        let prefix =
            "e=use_extension('//:missing.bzl','ext',dev_dependency=True)\ne.tag(value=e)\n";
        for context in contexts() {
            if !context.effective_ignore_dev_dependencies() {
                assert!(
                    in_context(prefix, context)
                        .unwrap_err()
                        .contains("unsupported extension tag attribute")
                );
                continue;
            }
            let file = in_context(prefix, context).unwrap();
            assert!(
                file.extension_usages
                    .iter()
                    .all(|usage| usage.tags.is_empty())
            );
            let mixed = in_context(
                &format!("{prefix}p=use_extension('//:missing.bzl','ext')\np.tag(value=7)"),
                context,
            )
            .unwrap();
            assert_eq!(mixed.extension_usages[0].tags.len(), 1);
            assert_eq!(mixed.extension_usages[0].tags[0].attributes["value"], 7);
            let collision = format!(
                "{prefix}use_repo(e,'reserved')\nr=use_repo_rule('missing','rule')\nr(name='reserved')"
            );
            assert!(
                in_context(&collision, context)
                    .unwrap_err()
                    .contains("already used")
            );
            let invalid_call =
                "e=use_extension('//:missing.bzl','ext',dev_dependency=True)\ne.tag(7)";
            assert!(in_context(invalid_call, context).is_err());
        }
    }

    fn assert_direct_usage(usage: &ExtensionUsage, expected: &[(&str, bool)]) {
        assert!(usage.identity.is_repo_rule());
        assert_eq!(usage.declarations.len(), expected.len());
        assert_eq!(usage.tags.len(), expected.len());
        assert_eq!(usage.imports.len(), expected.len());
        for (i, (name, dev)) in expected.iter().enumerate() {
            assert_eq!(usage.declarations[i].dev_dependency, *dev);
            assert_eq!(usage.tags[i].name, "repo");
            assert_eq!(usage.tags[i].attributes["name"], *name);
            assert!(!usage.tags[i].attributes.contains_key("dev_dependency"));
            assert_eq!(usage.tags[i].declaration, i);
            assert_eq!(usage.tags[i].dev_dependency, *dev);
            assert_eq!(usage.imports[i].local_name, *name);
            assert_eq!(usage.imports[i].exported_name, *name);
            assert_eq!(usage.imports[i].declaration, i);
            assert_eq!(usage.imports[i].dev_dependency, *dev);
        }
    }

    #[test]
    fn repo_rule_declaration_signature_and_ordering() {
        for source in [
            "use_repo_rule()",
            "use_repo_rule('a')",
            "use_repo_rule('a','b','c')",
            "use_repo_rule(bzl_file='a',rule_name='b')",
            "use_repo_rule('a',rule_name='b')",
            "use_repo_rule('a','b',extra=None)",
        ] {
            fails(source, "two positional-only string arguments");
        }
        for source in ["use_repo_rule(1,'b')", "use_repo_rule('a',None)"] {
            fails(source, "must be a string");
        }
        assert!(
            run("r=use_repo_rule('invalid label','bad symbol')")
                .extension_usages
                .is_empty()
        );
        fails(
            "r=use_repo_rule('','')\nmodule()",
            "before other MODULE directives",
        );
        let f = run("module()\nr=use_repo_rule('','')\nr(name='repo')");
        assert_eq!(f.extension_usages[0].identity.extension_name, " ");
        assert_direct_usage(&f.extension_usages[0], &[("repo", false)]);
    }

    #[test]
    fn repo_rule_raw_concatenation_merges_only_when_invoked() {
        let f = run(r#"
unused = use_repo_rule("//:never_loaded.bzl", "never")
a = use_repo_rule("invalid label", "bad symbol")
b = use_repo_rule("invalid", "label bad symbol")
c = use_repo_rule("invalid label", "bad symbol")
normal = use_extension("//:real.bzl", "real")
b(name="first", dev_dependency=True)
a(name="second")
c(name="third")
other = use_repo_rule("other", "rule")
other(name="fourth")
"#);
        assert_eq!(f.extension_usages.len(), 3);
        assert!(!f.extension_usages[0].identity.is_repo_rule());
        let u = &f.extension_usages[1];
        assert_eq!(
            u.identity,
            ExtensionIdentity {
                label: "//:MODULE.bazel".to_owned(),
                extension_name: "invalid label bad symbol".to_owned(),
                isolation: None,
            }
        );
        assert_direct_usage(u, &[("first", true), ("second", false), ("third", false)]);
        assert_direct_usage(&f.extension_usages[2], &[("fourth", false)]);
        let mut identity = u.identity.clone();
        identity.label = "//:other.bazel".to_owned();
        assert!(!identity.is_repo_rule());
        identity.label = "//:MODULE.bazel".to_owned();
        identity.isolation = Some(0);
        assert!(!identity.is_repo_rule());
        identity.isolation = None;
        identity.extension_name = "identifier".to_owned();
        assert!(!identity.is_repo_rule());
    }

    #[test]
    fn repo_rule_context_filters_before_snapshot_or_name_reservation() {
        for context in contexts() {
            let ignored = context.effective_ignore_dev_dependencies();
            let f = in_context(
                "r=use_repo_rule('raw','rule')\nr(name='prod')\nr(name='dev',dev_dependency=True)",
                context,
            )
            .unwrap();
            let expected = if ignored {
                vec![("prod", false)]
            } else {
                vec![("prod", false), ("dev", true)]
            };
            assert_direct_usage(&f.extension_usages[0], &expected);
            let only_dev = in_context(
                "r=use_repo_rule('raw','rule')\nr(name='dev',dev_dependency=True)",
                context,
            )
            .unwrap();
            assert_eq!(only_dev.extension_usages.is_empty(), ignored);
            let unsupported = "r=use_repo_rule('raw','rule')\nresult=r(name='repo',dev_dependency=True,foo=r)\nr(name='repo',result=result)";
            if ignored {
                let f = in_context(unsupported, context).unwrap();
                assert_direct_usage(&f.extension_usages[0], &[("repo", false)]);
                assert_eq!(
                    f.extension_usages[0].tags[0].attributes["result"],
                    Json::Null
                );
            } else {
                // Namespace errors occur during evaluation, before deferred
                // conversion can reject the first call's proxy-valued attribute.
                assert!(
                    in_context(unsupported, context)
                        .unwrap_err()
                        .contains("already used")
                );
            }
            // Overrides share exactly the effective ignore-dev predicate.
            let f = in_context("local_path_override(module_name='a',path='x')", context).unwrap();
            assert_eq!(f.overrides.is_empty(), ignored);
        }
    }

    #[test]
    fn repo_rule_duplicate_dev_matrix() {
        for context in contexts() {
            for (first, second) in [(true, true), (true, false), (false, true), (false, false)] {
                let boolean = |b| if b { "True" } else { "False" };
                let source = format!(
                    "r=use_repo_rule('raw','rule')\nr(name='same',dev_dependency={})\nr(name='same',dev_dependency={})",
                    boolean(first),
                    boolean(second)
                );
                let active = [first, second]
                    .into_iter()
                    .filter(|dev| !*dev || !context.effective_ignore_dev_dependencies())
                    .collect::<Vec<_>>();
                let result = in_context(&source, context);
                if active.len() == 2 {
                    assert!(result.unwrap_err().contains("already used"));
                } else {
                    let f = result.unwrap();
                    if active.is_empty() {
                        assert!(f.extension_usages.is_empty());
                    } else {
                        assert_direct_usage(&f.extension_usages[0], &[("same", active[0])]);
                    }
                }
            }
        }
    }

    #[test]
    fn repo_rule_call_signature_validated_even_when_dev_is_ignored() {
        for context in contexts() {
            for (call, expected) in [
                ("r(dev_dependency=True)", "missing required argument"),
                (
                    "r(name='_x',dev_dependency=True)",
                    "invalid repository name",
                ),
                ("r(name='',dev_dependency=True)", "invalid repository name"),
                ("r(name=None,dev_dependency=True)", "must be a string"),
                ("r(name=1,dev_dependency=True)", "must be a string"),
                ("r(name='repo',dev_dependency=1)", "must be a bool"),
                ("r('repo',dev_dependency=True)", "positional"),
            ] {
                let error = in_context(&format!("r=use_repo_rule('raw','rule')\n{call}"), context)
                    .unwrap_err();
                assert!(error.contains(expected), "{call}: {error}");
            }
            for call in [
                "r(name='repo',dev_dependency=True,**{'name':'other'})",
                "r(name='repo',dev_dependency=True,**{'dev_dependency':True})",
            ] {
                assert!(
                    in_context(&format!("r=use_repo_rule('raw','rule')\n{call}"), context).is_err()
                );
            }
            let error = in_context(
                "r=use_repo_rule('raw','rule')\nr(name='repo',dev_dependency=True)\nmodule()",
                context,
            )
            .unwrap_err();
            assert!(error.contains("before other MODULE directives"));
        }
    }

    #[test]
    fn repo_rule_namespace_collisions_include_ignored_deps_and_imports() {
        for context in contexts() {
            for prefix in [
                "module(name='taken')",
                "module(name='root',repo_name='taken')",
                "bazel_dep(name='dep',repo_name='taken')",
                "bazel_dep(name='dep',repo_name='taken',dev_dependency=True)",
                "x=use_extension('//:e.bzl','e')\nuse_repo(x,'taken')",
                "x=use_extension('//:e.bzl','e',dev_dependency=True)\nuse_repo(x,'taken')",
                "other=use_repo_rule('other','rule')\nother(name='taken')",
            ] {
                let error = in_context(
                    &format!("{prefix}\nr=use_repo_rule('raw','rule')\nr(name='taken')"),
                    context,
                )
                .unwrap_err();
                assert!(error.contains("already used"), "{prefix}: {error}");
                if context.effective_ignore_dev_dependencies() {
                    in_context(&format!("{prefix}\nr=use_repo_rule('raw','rule')\nr(name='taken',dev_dependency=True)"), context).unwrap();
                }
            }
            for suffix in [
                "bazel_dep(name='dep',repo_name='taken',dev_dependency=True)",
                "x=use_extension('//:e.bzl','e',dev_dependency=True)\nuse_repo(x,'taken')",
            ] {
                assert!(
                    in_context(
                        &format!("r=use_repo_rule('raw','rule')\nr(name='taken')\n{suffix}"),
                        context
                    )
                    .unwrap_err()
                    .contains("already used")
                );
            }
        }
        let f = run(
            "module(name='root',repo_name='renamed')\nr=use_repo_rule('raw','rule')\nr(name='root')",
        );
        assert_direct_usage(&f.extension_usages[0], &[("root", false)]);
    }

    #[test]
    fn repo_rule_arbitrary_attributes_are_owned_bounded_final_values() {
        let f = run(r#"
r=use_repo_rule("not a label", "not an identifier")
attrs={"nested":[True,None,42,1.5,{"x":"before"}]}
result=r(name="repo",dev_dependency=True,repo_mapping={"@a":"@b"},tags=["manual"],visibility=None,**{"dash-key":attrs,"":False})
attrs["nested"][4]["x"]="after"
attrs["nested"].append("later")
r(name="second",result=result)
"#);
        assert_direct_usage(&f.extension_usages[0], &[("repo", true), ("second", false)]);
        assert_eq!(
            f.extension_usages[0].tags[0].attributes,
            BTreeMap::from([
                ("name".to_owned(), serde_json::json!("repo")),
                ("repo_mapping".to_owned(), serde_json::json!({"@a":"@b"})),
                ("tags".to_owned(), serde_json::json!(["manual"])),
                ("visibility".to_owned(), Json::Null),
                (
                    "dash-key".to_owned(),
                    serde_json::json!({"nested":[true,null,42,1.5,{"x":"after"},"later"]})
                ),
                ("".to_owned(), Json::Bool(false)),
            ])
        );
        assert_eq!(
            f.extension_usages[0].tags[1].attributes["result"],
            Json::Null
        );
        for (setup, value, expected) in [
            ("", "r", "unsupported extension tag attribute"),
            ("a=[]\na.append(a)", "a", "cyclic or exceed nesting depth"),
        ] {
            fails(
                &format!("r=use_repo_rule('raw','rule')\n{setup}\nr(name='repo',value={value})"),
                expected,
            );
        }
    }

    fn deferred_attributes_reference_matrix(direct: bool) {
        let declaration = if direct {
            "r=use_repo_rule('raw','rule')\ncall=r"
        } else {
            "r=use_repo_rule('raw','rule')\nx=use_extension('//:e.bzl','e')\ncall=x.tag"
        };
        let f = run(&format!(
            r#"
{declaration}
xs=["before"]
original=xs
nested={{"items":xs}}
kwargs={{"name":"first","values":xs,"nested":nested,"proxy_type":type(r)}}
call(**{{"name":kwargs["name"],"values":kwargs["values"],"nested":kwargs["nested"],"proxy_type":kwargs["proxy_type"]}})
kwargs["values"]=["outer replacement"]
kwargs["nested"]={{"items":["outer replacement"]}}
kwargs["name"]="not the recorded name"
original.append("after")
nested["added"]={{"list":[1]}}
call(name="second",values=original)
original.append("last")
nested["added"]["list"].append(2)
"#
        ));
        let usage = &f.extension_usages[0];
        assert_eq!(usage.tags.len(), 2);
        let final_list = serde_json::json!(["before", "after", "last"]);
        assert_eq!(usage.tags[0].attributes["values"], final_list);
        assert_eq!(usage.tags[1].attributes["values"], final_list);
        assert_eq!(usage.tags[0].attributes["name"], "first");
        assert_eq!(usage.tags[1].attributes["name"], "second");
        assert_eq!(
            usage.tags[0].attributes["nested"],
            serde_json::json!({
                "items":["before","after","last"], "added":{"list":[1,2]}
            })
        );
        assert_eq!(usage.tags[0].attributes["proxy_type"], "repo_rule_proxy");
    }

    #[test]
    fn deferred_attributes_direct_retain_references_until_module_end() {
        deferred_attributes_reference_matrix(true);
    }

    #[test]
    fn deferred_attributes_ordinary_retain_references_until_module_end() {
        deferred_attributes_reference_matrix(false);
    }

    #[test]
    fn deferred_attributes_final_cycles_and_depth_are_rejected() {
        for declaration in [
            "call=use_repo_rule('raw','rule')",
            "x=use_extension('//:e.bzl','e')\ncall=x.tag",
        ] {
            fails(
                &format!("{declaration}\nxs=[]\ncall(name='repo',values=xs)\nxs.append(xs)"),
                "cyclic or exceed nesting depth",
            );
            fails(
                &format!(
                    "{declaration}\nnodes=[[] for i in range(66)]\ncall(name='repo',values=nodes[0])\n[nodes[i].append(nodes[i+1]) for i in range(65)]"
                ),
                "cyclic or exceed nesting depth",
            );
        }
    }

    #[test]
    fn deferred_attributes_only_final_value_types_are_serialized() {
        for declaration in [
            "call=use_repo_rule('raw','rule')",
            "x=use_extension('//:e.bzl','e')\ncall=x.tag",
        ] {
            let f = run(&format!(
                "{declaration}\nattrs={{'value':call}}\ncall(name='repo',attrs=attrs)\nattrs['value']='repaired'"
            ));
            assert_eq!(
                f.extension_usages[0].tags[0].attributes["attrs"],
                serde_json::json!({"value":"repaired"})
            );
            fails(
                &format!(
                    "{declaration}\nattrs={{'value':'before'}}\ncall(name='repo',attrs=attrs)\nattrs['value']=call"
                ),
                "unsupported extension tag attribute",
            );
        }
    }

    #[test]
    fn empty_and_anonymous_root_modules_are_supported() {
        assert_eq!(run(""), ModuleFile::default());
        assert_eq!(
            run("module()\nprint('recording only')").module,
            Some(ModuleInfo::default())
        );
    }

    #[test]
    fn dict_literal_expansion_and_keyword_extension_arguments() {
        let f = run(r#"
opts = {"name": "a", "version": "1.2.3"}
bazel_dep(**{"name":opts["name"],"version":opts["version"]})
x = use_extension(extension_bzl_file="//:e.bzl", extension_name="e")
x.tag(**{"dash-key": [1, 2]})
use_repo(x, "repo", **{"repo.alias": "original"})
register_toolchains("//:tc", **{"dev_dependency": True})
"#);
        assert_eq!(f.dependencies[0].name, "a");
        assert_eq!(
            f.extension_usages[0].tags[0].attributes["dash-key"],
            serde_json::json!([1, 2])
        );
        assert_eq!(f.extension_usages[0].imports[1].local_name, "repo.alias");
        fails(
            "use_extension('//:e.bzl', 'e', extension_name='f')",
            "supplied twice",
        );
    }

    #[test]
    fn side_effects_require_the_module_collector() {
        let mut globals = GlobalsBuilder::standard();
        globals.set("module", Builtin("module"));
        let globals = globals.build();
        Module::with_temp_heap(|module| {
            let ast =
                AstModule::parse("other.star", "module()".to_owned(), &Dialect::Standard).unwrap();
            let error = Evaluator::new(&module)
                .eval_module(ast, &globals)
                .unwrap_err()
                .to_string();
            assert!(error.contains("MODULE.bazel evaluation context"), "{error}");
        });
    }

    #[test]
    fn ignored_overrides_skip_duplicates_but_still_validate_arguments() {
        let duplicate = "local_path_override(module_name='a',path='first')\nlocal_path_override(module_name='a',path='second')";
        assert!(
            evaluate_source_for_resolution("MODULE.bazel", duplicate.to_owned(), true)
                .unwrap()
                .overrides
                .is_empty()
        );
        fails(duplicate, "duplicate override");
        for (source, expected) in [
            (
                "local_path_override(module_name='a',path=1)",
                "must be a string",
            ),
            (
                "local_path_override(module_name='a',path='x',typo=True)",
                "unknown or unsupported argument",
            ),
            (
                "single_version_override(module_name='a',version='1..2')",
                "invalid module version",
            ),
            (
                "multiple_version_override(module_name='a',versions=['1'])",
                "at least 2 versions",
            ),
        ] {
            let error = evaluate_source_for_resolution("MODULE.bazel", source.to_owned(), true)
                .unwrap_err();
            assert!(error.contains(expected), "{source}: {error}");
        }
    }
}
