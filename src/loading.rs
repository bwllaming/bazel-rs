//! Repository-aware `.bzl` loading. Package BUILD files establish boundaries;
//! they are never evaluated as a side effect of loading an extension.

use crate::bzl_visibility::{BzlVisibility, InitGuard, check_load_visibilities};
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::workspace::package_build_file;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::syntax::ast::{AstLiteral, Expr, Stmt};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{FrozenHeapName, OwnedFrozenValue};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_LOAD_DEPTH: usize = 256;

// Owned extension phase context, deliberately distinct from BUILD declarations.
#[allow(unsafe_code)]
mod extension_context {
    #[derive(starlark::any::ProvidesStaticType)]
    pub struct ExtensionContext {
        pub label: crate::label::Label,
        pub repositories: std::sync::Arc<crate::repository::RepositorySet>,
        /// Repository/extension execution profile: Bazel's internal Latin-1
        /// byte strings apply (e.g. json.decode `\u` escapes become UTF-8
        /// bytes). False for BUILD/analysis loading, which keeps Unicode text.
        pub byte_strings: bool,
    }
}
pub use extension_context::ExtensionContext;

pub(crate) fn definition_label(eval: &Evaluator<'_, '_, '_>) -> Option<Label> {
    eval.call_stack_top_location()
        .and_then(|location| {
            location
                .filename()
                .starts_with("//")
                .then_some(())
                .or_else(|| location.filename().starts_with("@@").then_some(()))
                .and_then(|()| Label::parse_canonical(location.filename()).ok())
        })
        .or_else(|| {
            eval.extra
                .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
                .map(|context| context.label.clone())
        })
}

/// Phase accessors only downcast their own context, avoiding recursive dispatch.
pub(crate) fn repositories(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
        .map(|context| context.repositories.clone())
        .or_else(|| crate::package::repository_context(eval))
        .or_else(|| crate::analysis::repository_context(eval))
        .or_else(|| crate::repository_ctx::repository_context(eval))
        .or_else(|| crate::extension_ctx::repository_context(eval))
        .or_else(|| crate::starlark_transitions::repository_context(eval))
}

pub fn resolve_label(eval: &Evaluator<'_, '_, '_>, text: &str) -> anyhow::Result<Label> {
    resolve_label_inner(eval, text, None)
}

/// Reserve lexical/mapped expansion before parsing copies the resolved parts.
pub(crate) fn resolve_label_bounded(
    eval: &Evaluator<'_, '_, '_>,
    text: &str,
    budget: &mut crate::repository_budget::HostBudget,
) -> anyhow::Result<Label> {
    resolve_label_inner(eval, text, Some(budget))
}

fn resolve_label_inner(
    eval: &Evaluator<'_, '_, '_>,
    text: &str,
    mut budget: Option<&mut crate::repository_budget::HostBudget>,
) -> anyhow::Result<Label> {
    if let Some(repositories) = repositories(eval) {
        let mut resolve = |repository: &str, package: &str| -> anyhow::Result<Label> {
            if let Some(budget) = budget.as_deref_mut() {
                let bound = repositories
                    .resolution_size_bound(text, repository, package)
                    .map_err(anyhow::Error::msg)?;
                budget.bytes(bound).map_err(anyhow::Error::msg)?;
            }
            repositories
                .parse(text, repository, package)
                .map_err(anyhow::Error::msg)
        };
        if let Some(label) = definition_label(eval) {
            return resolve(&label.repository, &label.package);
        }
        if let Ok((_, package)) = crate::package::custom_label_context(eval) {
            return resolve(&package.repository, &package.package);
        }
        if let Some(package) = crate::repository_ctx::definition_package(eval) {
            return resolve(&package.repository, &package.package);
        }
        if let Some(package) = crate::extension_ctx::definition_package(eval) {
            return resolve(&package.repository, &package.package);
        }
    }
    if let Some(budget) = budget {
        budget
            .bytes(Label::resolution_size_bound(text, "", "", &BTreeMap::new()))
            .map_err(anyhow::Error::msg)?;
    }
    Label::parse(text, "").map_err(anyhow::Error::msg)
}

/// Resolve the raw load IDs in `ast` to frozen modules, evaluating each canonical
/// label once per invocation. `globals` must be the extension-file globals, not
/// BUILD globals. Extension evaluation supplies its definition label, not a
/// BUILD declaration context; native rules must be invoked from BUILD or macros.
pub fn load_modules(
    root: &Path,
    current_package: &str,
    ast: &AstModule,
    globals: &Globals,
) -> Result<HashMap<String, FrozenModule>, String> {
    load_modules_with_functions(root, current_package, ast, globals).map(|loaded| loaded.modules)
}

/// Frozen imports and verified top-level function identities from the complete
/// transitive load graph. Ownership keeps even private helper functions alive.
#[derive(Debug, Default)]
pub struct LoadedModules {
    pub modules: HashMap<String, FrozenModule>,
    pub functions: Vec<OwnedFrozenValue>,
}

/// Exact input bytes and the owning repository's mapping for one evaluated file.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SourceProvenance {
    pub bytes: Vec<u8>,
    pub mapping: BTreeMap<String, String>,
}

/// An owned frozen export, transitive function identities, and canonical inputs.
/// No evaluator or repository runtime is retained by this result.
#[derive(Debug)]
pub struct LoadedDefinition {
    pub value: OwnedFrozenValue,
    pub functions: Vec<OwnedFrozenValue>,
    pub sources: BTreeMap<String, SourceProvenance>,
}

/// Load a public export directly from a canonical `.bzl` label, without a BUILD
/// shim. Unlike ordinary package loading, this demanded definition snapshots
/// repository mappings and therefore may resolve the module graph.
pub fn load_definition_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
) -> Result<LoadedDefinition, String> {
    load_frozen_export_in(repositories, label, symbol, globals, true)
}

/// Module-extension identity is the requested label and global, not the first
/// export of the value. Pinned Bazel permits private and imported globals here.
pub fn load_module_extension_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
) -> Result<LoadedDefinition, String> {
    load_frozen_export_in(repositories, label, symbol, globals, false)
}

fn load_frozen_export_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
    public_only: bool,
) -> Result<LoadedDefinition, String> {
    if public_only && symbol.starts_with('_') {
        return Err(format!(
            "cannot load private symbol {symbol:?} from {label}"
        ));
    }
    if symbol.is_empty()
        || !symbol
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    {
        return Err(format!("invalid export identifier {symbol:?} from {label}"));
    }
    if !label.name.ends_with(".bzl") {
        return Err(format!("load {label} requires a .bzl suffix"));
    }
    let mut loader = Loader::new(repositories, globals, true);
    loader.load(label.clone())?;
    let module = loader
        .cache
        .get(label)
        .ok_or_else(|| format!("internal loader error: module {label} was not evaluated"))?;
    let value = if public_only {
        module.get(symbol)
    } else {
        module.get_any_visibility(symbol).map(|(value, _)| value)
    }
    .map_err(|e| format!("cannot load {symbol:?} from {label}: {e}"))?;
    Ok(LoadedDefinition {
        value,
        functions: loader.functions,
        sources: loader.sources,
    })
}

pub fn load_modules_with_functions(
    root: &Path,
    current_package: &str,
    ast: &AstModule,
    globals: &Globals,
) -> Result<LoadedModules, String> {
    load_modules_with_functions_in(
        Arc::new(RepositorySet::new(root.to_path_buf())),
        &PackageId {
            repository: String::new(),
            package: current_package.to_owned(),
        },
        ast,
        globals,
    )
}

pub fn load_modules_with_functions_in(
    repositories: Arc<RepositorySet>,
    package: &PackageId,
    ast: &AstModule,
    globals: &Globals,
) -> Result<LoadedModules, String> {
    let labels = dependencies(&repositories, package, ast)?;
    if labels.is_empty() {
        return Ok(LoadedModules::default());
    }
    let mut loader = Loader::new(repositories, globals, false);
    for label in labels {
        loader.load(label)?;
    }
    // PackageFunction.java:1134-1154: BUILD loads are checked against the
    // package with the description "file <canonical BUILD label>".
    let file_span = ast.file_span(ast.statement().span);
    let build_name = Path::new(file_span.filename())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("BUILD");
    let build = Label {
        repository: package.repository.clone(),
        package: package.package.clone(),
        name: build_name.to_owned(),
    };
    loader.check_load_visibilities(package, &format!("file {build}"), file_span.filename(), ast)?;
    Ok(LoadedModules {
        modules: loader.imports(package, ast)?,
        functions: loader.functions,
    })
}

fn dependencies(
    repositories: &RepositorySet,
    package: &PackageId,
    ast: &AstModule,
) -> Result<Vec<Label>, String> {
    ast.loads()
        .into_iter()
        .map(|load| {
            let label = repositories
                .parse(load.module_id, &package.repository, &package.package)
                .map_err(|e| {
                    let context = if load.module_id.starts_with('@') {
                        "external repository load: "
                    } else {
                        ""
                    };
                    format!("{}: {context}{e}", load.span)
                })?;
            if !label.name.ends_with(".bzl") {
                return Err(format!(
                    "{}: load {label} requires a .bzl suffix",
                    load.span
                ));
            }
            for (_, symbol) in &load.symbols {
                if symbol.starts_with('_') {
                    return Err(format!(
                        "{}: cannot load private symbol {symbol:?} from {label}",
                        load.span
                    ));
                }
            }
            Ok(label)
        })
        .collect()
}

struct Loader<'a> {
    repositories: Arc<RepositorySet>,
    globals: &'a Globals,
    cache: HashMap<Label, FrozenModule>,
    /// Stored with each frozen module; checks are per requesting package.
    visibilities: HashMap<Label, BzlVisibility>,
    functions: Vec<OwnedFrozenValue>,
    provenance_capture: bool,
    sources: BTreeMap<String, SourceProvenance>,
}

enum Work {
    Enter(Label),
    Finish(Label, PathBuf, Box<AstModule>),
}

impl<'a> Loader<'a> {
    fn new(
        repositories: Arc<RepositorySet>,
        globals: &'a Globals,
        provenance_capture: bool,
    ) -> Self {
        Self {
            repositories,
            globals,
            cache: HashMap::new(),
            visibilities: HashMap::new(),
            functions: Vec::new(),
            provenance_capture,
            sources: BTreeMap::new(),
        }
    }

    fn load(&mut self, label: Label) -> Result<(), String> {
        // Explicit DFS frames avoid exhausting the Rust stack on deep graphs.
        let mut work = vec![Work::Enter(label)];
        let mut active = Vec::new();
        while let Some(next) = work.pop() {
            match next {
                Work::Enter(label) => {
                    if self.cache.contains_key(&label) {
                        continue;
                    }
                    if active.contains(&label) {
                        return Err(format!("load cycle: {}", chain(&active, &label)));
                    }
                    if active.len() >= MAX_LOAD_DEPTH {
                        return Err(format!(
                            "load depth limit {MAX_LOAD_DEPTH} exceeded: {}",
                            chain(&active, &label)
                        ));
                    }
                    let path = self.source_path(&label)?;
                    let bytes = fs::read(&path)
                        .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?;
                    let source = std::str::from_utf8(&bytes)
                        .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?
                        .to_owned();
                    if self.provenance_capture {
                        self.sources.insert(
                            label.to_string(),
                            SourceProvenance {
                                bytes,
                                mapping: self.repositories.mapping(&label.repository)?.clone(),
                            },
                        );
                    }
                    // Bazel extension files allow definitions but not top-level
                    // if/for statements. Do not enable the Extended dialect.
                    let dialect = Dialect {
                        enable_keyword_only_arguments: true,
                        enable_load_reexport: false,
                        ..Dialect::Standard
                    };
                    let ast = AstModule::parse(&label.to_string(), source, &dialect)
                        .map_err(|e| format!("cannot parse {label}: {e}"))?;
                    // Bazel's lexer rejects invalid escapes in every .bzl
                    // (Lexer.java:389-403), whether or not the code runs.
                    crate::repository_text::validate_source(&ast)?;
                    if self.provenance_capture {
                        crate::repository_text::validate_execution_source(&ast)?;
                    }
                    validate_bzl_load_order(&ast)?;
                    crate::guarded_globals::check(&label, &ast)?;
                    crate::guarded_globals::check_universe(&ast)?;
                    let dependencies = dependencies(&self.repositories, &label.package_id(), &ast)?;
                    active.push(label.clone());
                    work.push(Work::Finish(label, path, Box::new(ast)));
                    work.extend(dependencies.into_iter().rev().map(Work::Enter));
                }
                Work::Finish(label, path, ast) => {
                    let function_names = top_level_function_names(&ast);
                    // BzlLoadFunction.java:845-856: after the direct loads are
                    // computed, before this file executes.
                    self.check_load_visibilities(
                        &label.package_id(),
                        &format!("module {label}"),
                        &path.to_string_lossy(),
                        &ast,
                    )?;
                    let imports = self.imports(&label.package_id(), &ast)?;
                    let modules = imports.iter().map(|(id, m)| (id.as_str(), m)).collect();
                    let file_loader = ReturnFileLoader { modules: &modules };
                    let (frozen, visibility) = Module::with_temp_heap(|module| {
                        let visibility = {
                            let context = ExtensionContext {
                                label: label.clone(),
                                repositories: self.repositories.clone(),
                                byte_strings: self.provenance_capture,
                            };
                            let initialization = InitGuard::begin(&context);
                            let mut eval = Evaluator::new(&module);
                            eval.extra = Some(&context);
                            eval.set_max_callstack_size(128)
                                .map_err(|e| e.to_string())?;
                            if self.provenance_capture {
                                // Definition initialization is execution too: limiting
                                // only the later callback leaves top-level/helper work
                                // in any transitive .bzl outside the repository budget.
                                eval.set_max_tick_count(1_000_000)
                                    .map_err(|e| e.to_string())?;
                                eval.set_max_heap_size(128 * 1024 * 1024)
                                    .map_err(|e| e.to_string())?;
                            }
                            eval.set_loader(&file_loader);
                            // Repository/extension definitions evaluate in the
                            // Latin-1 char model; BUILD/analysis loads do not.
                            let _model = starlark::values::string::enter_latin1_char_model(
                                self.provenance_capture,
                            );
                            eval.eval_module(*ast, self.globals)
                                .map_err(|e| format!("cannot evaluate {label}: {e}"))?;
                            drop(eval);
                            initialization.finish()
                        };
                        module
                            .freeze_named(FrozenHeapName::User(Box::new(label.to_string())))
                            .map_err(|e| format!("cannot freeze {label}: {e:?}"))
                            .map(|frozen| (frozen, visibility))
                    })?;
                    for name in function_names {
                        let (function, _) = frozen.get_any_visibility(&name).map_err(|e| {
                            format!("cannot retain top-level function {name:?} from {label}: {e}")
                        })?;
                        self.functions.push(function);
                    }
                    self.cache.insert(label.clone(), frozen);
                    self.visibilities.insert(label.clone(), visibility);
                    let finished = active.pop();
                    debug_assert_eq!(finished.as_ref(), Some(&label));
                }
            }
        }
        Ok(())
    }

    fn check_load_visibilities(
        &self,
        package: &PackageId,
        description: &str,
        path: &str,
        ast: &AstModule,
    ) -> Result<(), String> {
        let mut loads = Vec::new();
        for load in ast.loads() {
            let label =
                self.repositories
                    .parse(load.module_id, &package.repository, &package.package)?;
            let visibility = self.visibilities.get(&label).ok_or_else(|| {
                format!("internal loader error: module {label} was not evaluated")
            })?;
            loads.push((label, visibility));
        }
        check_load_visibilities(package, description, path, ast, loads)
    }

    fn imports(
        &self,
        package: &PackageId,
        ast: &AstModule,
    ) -> Result<HashMap<String, FrozenModule>, String> {
        let mut imports = HashMap::new();
        for load in ast.loads() {
            let label =
                self.repositories
                    .parse(load.module_id, &package.repository, &package.package)?;
            let module = self.cache.get(&label).ok_or_else(|| {
                format!("internal loader error: module {label} was not evaluated")
            })?;
            // Check even BUILD imports now, including repeated raw IDs whose
            // later load statements may request different symbols.
            for (_, symbol) in &load.symbols {
                module.get(symbol).map_err(|e| {
                    format!("{}: cannot load {symbol:?} from {label}: {e}", load.span)
                })?;
            }
            imports.insert(load.module_id.to_owned(), module.clone());
        }
        Ok(imports)
    }

    fn source_path(&self, label: &Label) -> Result<PathBuf, String> {
        // This checks every package-directory component (including symlinks and
        // nested workspaces), without reading/evaluating the BUILD contents.
        let root = self.repositories.root(&label.repository)?;
        // BzlLoadFunction's ContainingPackageLookup: ignored directories are
        // deleted packages, so the lookup continues with the parent.
        let ignored = self
            .repositories
            .ignored_subdirectories(&label.repository)?;
        if ignored.matching_entry(&label.package).is_some() {
            return Err(ignored_package_load_error(root, label, &ignored)?);
        }
        package_build_file(root, &label.package)?;
        let mut path = root.join(&label.package);
        let mut relative = label.package.clone();
        let mut components = label.name.split('/').peekable();
        // Bazel reads a .bzl through symlinks (its FileValue follows them);
        // package boundaries stay checked on the logical label path, as for
        // source artifacts (engine.rs). Links resolve inside the repository,
        // into a repository already materialized in this invocation, or (for
        // repository-rule-generated repositories) into a host path
        // (source_links::LinkPolicy); the label keeps its logical identity
        // and only the resolved file's bytes are read.
        let mut linked: Option<PathBuf> = None;
        // BzlCompileFunction.java:109-136.
        let no_such_file = || format!("cannot load '{label}': no such file");
        while let Some(component) = components.next() {
            path.push(component);
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(component);
            let inspect = |e: std::io::Error| {
                format!("cannot inspect load {label} ({}): {e}", path.display())
            };
            let mut metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    return Err(no_such_file());
                }
                Err(e) => return Err(inspect(e)),
            };
            if metadata.file_type().is_symlink() || linked.is_some() {
                let policy = self.repositories.link_policy(&label.repository);
                match crate::source_links::resolve_with_policy(root, Path::new(&relative), &policy)
                {
                    Err(error) if error.starts_with("circular symlinks detected") => {
                        return Err(error);
                    }
                    Err(error) => return Err(format!("load {label}: {error}")),
                    Ok(crate::source_links::Resolution::Missing(_)) => {
                        return Err(no_such_file());
                    }
                    Ok(crate::source_links::Resolution::Resolved(resolved)) => {
                        metadata = fs::metadata(&resolved).map_err(inspect)?;
                        linked = Some(resolved);
                    }
                }
            }
            if components.peek().is_none() {
                if metadata.is_dir() {
                    return Err(format!("cannot load '{label}': is a directory"));
                }
                if !metadata.is_file() {
                    return Err(format!(
                        "cannot load '{label}': not a regular file (dangling link?)"
                    ));
                }
            } else {
                if !metadata.is_dir() {
                    return Err(format!(
                        "load {label} component is not a directory: {}",
                        path.display()
                    ));
                }
                // Bazel 9.3 has no nested repositories: MODULE.bazel,
                // REPO.bazel and WORKSPACE files are no boundaries
                // (LocalRepositoryLookupFunction.java:27-35).
                // An ignored directory with a BUILD file is no subpackage.
                let names: &[&str] = if ignored.matching_entry(&relative).is_some() {
                    &[]
                } else {
                    &["BUILD.bazel", "BUILD"]
                };
                for name in names {
                    let build = path.join(name);
                    match fs::metadata(&build) {
                        Ok(metadata) if metadata.is_file() => {
                            return Err(format!(
                                "load {label} crosses a subpackage boundary at {}; use that package's label",
                                path.display()
                            ));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(format!("cannot inspect {}: {e}", build.display())),
                    }
                }
            }
        }
        Ok(linked.unwrap_or(path))
    }
}

/// The BzlLoadFunction error for a `.bzl` label whose package lies in an
/// ignored directory: ContainingPackageLookupFunction walks up past deleted
/// packages; the nearest existing ancestor package makes the label cross a
/// package boundary (`ContainingPackageLookupValue
/// .getErrorMessageForLabelCrossingPackageBoundary`), none means
/// `BzlLoadFunction.noBuildFile`.
fn ignored_package_load_error(
    root: &Path,
    label: &Label,
    ignored: &crate::ignored::IgnoredSubdirectories,
) -> Result<String, String> {
    let mut current = label.package.clone();
    let containing = loop {
        let Some(parent) = (if current.is_empty() {
            None
        } else {
            Some(
                current
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    .to_owned(),
            )
        }) else {
            break None;
        };
        current = parent;
        if ignored.matching_entry(&current).is_none()
            && crate::workspace::is_package(root, &current)?
        {
            break Some(current);
        }
    };
    let Some(containing) = containing else {
        return Ok(format!(
            "Every .bzl file must have a corresponding package, but '{label}' does not have one. Please create a BUILD file in the same or any parent directory. Note that this BUILD file does not need to do anything except exist."
        ));
    };
    let full = format!("{}/{}", label.package, label.name);
    let relative = if containing.is_empty() {
        full.as_str()
    } else {
        &full[containing.len() + 1..]
    };
    let containing = if label.repository.is_empty() {
        format!("//{containing}")
    } else {
        format!("@@{}//{containing}", label.repository)
    };
    Ok(format!(
        "Label '{label}' is invalid because '{}' is not a package; perhaps you meant to put the colon here: '{containing}:{relative}'?",
        label.package
    ))
}

/// Only immediate `def` statements qualify. A frozen function obtained from a
/// factory, lambda, alias, or nested definition is not evidence of top-level
/// declaration. Exclude every multiply-bound name so reassignment cannot turn
/// a qualifying def's final global value into an incorrectly trusted closure.
fn top_level_function_names(ast: &AstModule) -> BTreeSet<String> {
    let mut statements = vec![ast.statement()];
    let mut candidates = BTreeSet::new();
    let mut bindings = HashMap::<String, usize>::new();
    while let Some(statement) = statements.pop() {
        match &statement.node {
            Stmt::Statements(children) => statements.extend(children.iter().rev()),
            Stmt::Def(def) => {
                let name = &def.name.node.ident;
                candidates.insert(name.clone());
                *bindings.entry(name.clone()).or_default() += 1;
                // Deliberately do not traverse the function body.
            }
            Stmt::Assign(assign) => assign.lhs.visit_lvalue(|name| {
                *bindings.entry(name.node.ident.clone()).or_default() += 1;
            }),
            Stmt::AssignModify(target, _, _) => target.visit_lvalue(|name| {
                *bindings.entry(name.node.ident.clone()).or_default() += 1;
            }),
            Stmt::Load(load) => {
                for argument in &load.args {
                    *bindings
                        .entry(argument.local.node.ident.clone())
                        .or_default() += 1;
                }
            }
            // The extension dialect rejects module-scope if/for statements;
            // expressions and comprehension locals cannot rebind globals.
            _ => {}
        }
    }
    candidates.retain(|name| bindings.get(name) == Some(&1));
    candidates
}

fn validate_bzl_load_order(ast: &AstModule) -> Result<(), String> {
    let mut statements = vec![ast.statement()];
    let mut seen_nonload = false;
    while let Some(statement) = statements.pop() {
        match &statement.node {
            Stmt::Statements(children) => statements.extend(children.iter().rev()),
            Stmt::Load(_) if seen_nonload => {
                return Err(format!(
                    "{}: .bzl load statements must precede other statements",
                    ast.file_span(statement.span)
                ));
            }
            Stmt::Load(_) => {}
            Stmt::Expression(expression)
                if matches!(&expression.node, Expr::Literal(AstLiteral::String(_))) => {}
            _ => seen_nonload = true,
        }
    }
    Ok(())
}

fn chain(active: &[Label], next: &Label) -> String {
    active
        .iter()
        .chain(std::iter::once(next))
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::GlobalsBuilder;
    use starlark::starlark_module;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    static EVALUATIONS: AtomicUsize = AtomicUsize::new(0);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = PathBuf::from("/tmp/opencode").join(format!(
                "bazel-loading-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let temp = Self(root);
            temp.file("BUILD", "fail('BUILD must not be evaluated')\n");
            temp
        }

        fn file(&self, name: &str, text: &str) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn load(&self, source: &str) -> Result<HashMap<String, FrozenModule>, String> {
            load_modules(&self.0, "", &parse(source), &Globals::standard())
        }

        fn definition(&self, label: &str, symbol: &str) -> Result<LoadedDefinition, String> {
            load_definition_in(
                Arc::new(RepositorySet::new(self.0.clone())),
                &Label::parse_canonical(label).unwrap(),
                symbol,
                &crate::package::bzl_globals(),
            )
        }

        fn error(&self, source: &str, expected: &str) {
            let error = self.load(source).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?} in {error}");
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn parse(source: &str) -> AstModule {
        AstModule::parse("BUILD", source.to_owned(), &Dialect::Standard).unwrap()
    }

    fn evaluate(source: &str, imports: &HashMap<String, FrozenModule>, globals: &Globals) -> i32 {
        let modules = imports.iter().map(|(id, m)| (id.as_str(), m)).collect();
        let loader = ReturnFileLoader { modules: &modules };
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&loader);
            eval.eval_module(parse(source), globals)
                .unwrap()
                .unpack_i32()
                .unwrap()
        })
    }

    #[starlark_module]
    fn test_globals(builder: &mut GlobalsBuilder) {
        fn rewrite_source(eval: &mut Evaluator) -> anyhow::Result<i32> {
            let context = eval
                .extra
                .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
                .unwrap();
            fs::write(
                context
                    .repositories
                    .root(&context.label.repository)
                    .map_err(anyhow::Error::msg)?
                    .join(&context.label.package)
                    .join(&context.label.name),
                "value = 99\n",
            )?;
            Ok(42)
        }

        fn count_evaluation() -> anyhow::Result<i32> {
            EVALUATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(21)
        }

        fn rule(eval: &mut Evaluator) -> anyhow::Result<i32> {
            anyhow::ensure!(
                eval.extra
                    .and_then(|context| context.downcast_ref::<i32>())
                    .is_some(),
                "rule requires BUILD context"
            );
            Ok(42)
        }
    }

    #[test]
    fn empty_loads_need_no_filesystem() {
        assert!(
            load_modules(
                Path::new("/does-not-exist"),
                "",
                &parse("x = 1"),
                &Globals::standard()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn definition_is_owned_debug_send_sync_and_keeps_private_transitive_functions() {
        fn assert_traits<T: std::fmt::Debug + Send + Sync>() {}
        assert_traits::<LoadedDefinition>();
        assert_traits::<SourceProvenance>();
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file(
            "helper.bzl",
            "def _private(x):\n    return x + 1\ndef public(x):\n    return _private(x)\n",
        );
        temp.file(
            "left.bzl",
            "load(':helper.bzl', _public='public')\nleft = _public\n",
        );
        temp.file(
            "right.bzl",
            "load('@//:helper.bzl', _public='public')\nright = _public\n",
        );
        temp.file("defs.bzl", "load(':left.bzl', 'left')\nload(':right.bzl', 'right')\nidentity = left == right\nalias = left\n");
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        let weak = Arc::downgrade(&repositories);
        let loaded = load_definition_in(
            repositories,
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "alias",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        assert!(
            weak.upgrade().is_none(),
            "frozen result retained repositories"
        );
        assert_eq!(loaded.sources.len(), 4);
        assert_eq!(loaded.functions.len(), 2);
        assert_eq!(
            loaded
                .functions
                .iter()
                .filter(|f| f.value().ptr_eq(loaded.value.value()))
                .count(),
            1
        );
        assert_eq!(
            temp.definition("//:defs.bzl", "identity")
                .unwrap()
                .value
                .value()
                .unpack_bool(),
            Some(true)
        );
        drop(temp);
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            for owned in std::iter::once(&loaded.value).chain(&loaded.functions) {
                let function = module.heap().access_owned_frozen_value(owned);
                let result = eval
                    .eval_function(function, &[module.heap().alloc(41)], &[])
                    .unwrap();
                assert_eq!(result.unpack_i32(), Some(42));
            }
        });
    }

    #[test]
    fn definition_accepts_provider_custom_rule_struct_and_scalar_exports() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file(
            "defs.bzl",
            r#"
P = provider(fields=["answer"])
def _impl(ctx):
    return [P(answer=42)]
r = rule(implementation=_impl, attrs={"dep": attr.label(default=Label(":input"))})
bundle = struct(p=P(answer=42), r=r, label=Label(":input"), native_type=type(native))
scalar = 42
"#,
        );
        for (symbol, expected) in [
            ("P", "Provider"),
            ("r", "rule"),
            ("bundle", "struct"),
            ("scalar", "int"),
        ] {
            let loaded = temp.definition("//:defs.bzl", symbol).unwrap();
            assert_eq!(loaded.value.value().get_type(), expected);
        }
        let bundle = temp.definition("//:defs.bzl", "bundle").unwrap().value;
        drop(temp);
        Module::with_temp_heap(|module| {
            module.set("bundle", module.heap().access_owned_frozen_value(&bundle));
            let value = Evaluator::new(&module).eval_module(
                parse("bundle.p.answer == 42 and str(bundle.label) == '@@//:input' and bundle.native_type == 'struct'"),
                &crate::package::bzl_globals(),
            ).unwrap();
            assert_eq!(value.unpack_bool(), Some(true));
        });
    }

    #[test]
    fn definition_rejects_private_invalid_missing_and_implicit_reexports() {
        let temp = Temp::new();
        // Invalid symbols must fail before filesystem access or evaluation.
        for symbol in ["_private", "", "1name", "bad-name", "name space", "é"] {
            let error = temp.definition("//:missing.bzl", symbol).unwrap_err();
            assert!(
                error.contains(if symbol.starts_with('_') {
                    "private symbol"
                } else {
                    "invalid export identifier"
                }),
                "{error}"
            );
        }
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file("helper.bzl", "public = 42\n");
        temp.file("defs.bzl", "load(':helper.bzl', 'public')\n_private = 1\n");
        for symbol in ["missing", "public"] {
            let error = temp.definition("//:defs.bzl", symbol).unwrap_err();
            assert!(
                error.contains("cannot load") && error.contains(symbol),
                "{error}"
            );
        }
        assert!(
            temp.definition("//:defs.star", "public")
                .unwrap_err()
                .contains(".bzl suffix")
        );
    }

    #[test]
    fn definition_evaluates_transitive_failures_and_reports_full_cycles() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file("defs.bzl", "load(':middle.bzl', 'middle')\nvalue = 42\n");
        temp.file("middle.bzl", "load(':leaf.bzl', 'leaf')\nmiddle = leaf\n");
        temp.file("leaf.bzl", "fail('TRANSITIVE_SENTINEL')\nleaf = 1\n");
        let error = temp.definition("//:defs.bzl", "value").unwrap_err();
        assert!(
            error.contains("TRANSITIVE_SENTINEL") && error.contains("//:leaf.bzl"),
            "{error}"
        );
        temp.file("leaf.bzl", "load('@@//:defs.bzl', 'value')\nleaf = value\n");
        let error = temp.definition("//:defs.bzl", "value").unwrap_err();
        assert!(
            error.contains("//:defs.bzl -> //:middle.bzl -> //:leaf.bzl -> //:defs.bzl"),
            "{error}"
        );
    }

    #[test]
    fn definition_provenance_is_exact_read_bytes_not_post_evaluation_file() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        let source = "# café\r\nvalue = rewrite_source()\r\n";
        temp.file("defs.bzl", source);
        let globals = GlobalsBuilder::standard().with(test_globals).build();
        let label = Label::parse_canonical("//:defs.bzl").unwrap();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        let first = load_definition_in(repositories.clone(), &label, "value", &globals).unwrap();
        assert_eq!(first.value.value().unpack_i32(), Some(42));
        assert_eq!(first.sources["//:defs.bzl"].bytes, source.as_bytes());
        assert_eq!(fs::read(temp.0.join("defs.bzl")).unwrap(), b"value = 99\n");
        let second = load_definition_in(repositories, &label, "value", &globals).unwrap();
        assert_eq!(second.value.value().unpack_i32(), Some(99));
        assert_eq!(second.sources["//:defs.bzl"].bytes, b"value = 99\n");
        assert!(serde_json::to_value(&first.sources).unwrap().is_object());
        fs::write(temp.0.join("defs.bzl"), [0xff]).unwrap();
        let error = temp.definition("//:defs.bzl", "value").unwrap_err();
        assert!(
            error.contains("cannot read //:defs.bzl") && error.contains("utf-8"),
            "{error}"
        );
    }

    #[test]
    fn definition_profile_checks_transitive_sources_without_changing_ordinary_loading() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file(
            "defs.bzl",
            "load(':helper.bzl', 'compute')\nvalue = compute()\n",
        );
        temp.file("helper.bzl", "def compute():\n    return str(len('é'))\n");
        let error = temp.definition("//:defs.bzl", "value").unwrap_err();
        assert!(
            error.contains("helper.bzl") && error.contains("direct repository execution"),
            "{error}"
        );
        // The ordinary loader keeps its established behavior; the
        // conservative source profile is attached only to demanded definitions.
        assert!(temp.load("load(':defs.bzl', 'value')\n").is_ok());
        // Bazel's universe has no `chr`: a free reference is a resolver error
        // in every .bzl (Module.java:250-256), demanded or not.
        temp.file(
            "helper.bzl",
            "alias = chr\ndef compute():\n    return str(len(alias(233)))\n",
        );
        for error in [
            temp.definition("//:defs.bzl", "value").unwrap_err(),
            temp.load("load(':defs.bzl', 'value')\n").unwrap_err(),
        ] {
            assert!(
                error.contains("helper.bzl:1:9") && error.contains("name 'chr' is not defined"),
                "{error}"
            );
        }
    }

    #[test]
    fn module_extension_lookup_allows_private_and_imported_globals_without_changing_direct_exports()
    {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name='main')\n");
        temp.file(
            "original.bzl",
            "def _impl(ctx):\n    pass\n_private = module_extension(_impl)\next = _private\n",
        );
        temp.file("wrapper.bzl", "load(':original.bzl', 'ext')\n");
        let repos = Arc::new(RepositorySet::new(temp.0.clone()));
        let globals = crate::package::bzl_globals();
        for (file, symbol) in [("//:original.bzl", "_private"), ("//:wrapper.bzl", "ext")] {
            let label = Label::parse_canonical(file).unwrap();
            let loaded = load_module_extension_in(repos.clone(), &label, symbol, &globals).unwrap();
            assert!(crate::module_extension::unpack(&loaded.value).is_ok());
            assert!(load_definition_in(repos.clone(), &label, symbol, &globals).is_err());
        }
    }

    #[test]
    fn definition_records_owner_mappings_using_existing_repository_configuration() {
        let main = Temp::new();
        let external = Temp::new();
        external_fixture(&main, &external);
        let module_path = main.0.join("MODULE.bazel");
        let mut module = fs::read_to_string(&module_path).unwrap();
        module.push_str("bazel_dep(name='unavailable',version='1',dev_dependency=True)\n");
        main.file("MODULE.bazel", &module);
        // Ignoring dev dependencies also ignores root overrides. A configured
        // local registry therefore supplies the selected dependency instead.
        main.file(
            "registry/modules/dep/1/MODULE.bazel",
            "module(name='dep',version='1',repo_name='self_dep')\n",
        );
        main.file(
            "registry/modules/dep/1/source.json",
            &serde_json::json!({"type": "local_path", "path": external.0}).to_string(),
        );
        main.file("defs.bzl", "load('@alias//:defs.bzl', 'dep')\nvalue = struct(main=Label('@alias//:input'), dep=dep)\n");
        external.file(
            "defs.bzl",
            "load('@self_dep//:helper.bzl', 'helper')\ndep = helper\n",
        );
        external.file("helper.bzl", "helper = Label('@self_dep//:input')\n");
        let cache = main.0.join("custom-cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("sentinel"), "untouched").unwrap();
        let repositories = Arc::new(
            RepositorySet::with_options(
                main.0.clone(),
                crate::module_graph::ResolverOptions {
                    registries: vec![main.0.join("registry").into()],
                    ignore_dev_dependencies: true,
                    ..Default::default()
                },
            )
            .with_source_cache(cache.clone()),
        );
        let main_mapping = repositories.mapping("").unwrap().clone();
        let dep_mapping = repositories.mapping("dep+").unwrap().clone();
        // The loader must reuse this resolved graph, not construct another set.
        main.file("MODULE.bazel", "invalid MODULE !!!\n");
        let loaded = load_definition_in(
            repositories,
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "value",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        assert_eq!(loaded.sources.len(), 3);
        assert_eq!(loaded.sources["//:defs.bzl"].mapping, main_mapping);
        for label in ["@@dep+//:defs.bzl", "@@dep+//:helper.bzl"] {
            assert_eq!(loaded.sources[label].mapping, dep_mapping);
        }
        assert_eq!(main_mapping["alias"], "dep+");
        assert_eq!(dep_mapping["self_dep"], "dep+");
        Module::with_temp_heap(|module| {
            module.set(
                "value",
                module.heap().access_owned_frozen_value(&loaded.value),
            );
            assert_eq!(
                Evaluator::new(&module)
                    .eval_module(
                        parse("str(value.main) == '@@dep+//:input' and value.main == value.dep"),
                        &crate::package::bzl_globals()
                    )
                    .unwrap()
                    .unpack_bool(),
                Some(true)
            );
        });
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
        assert_eq!(fs::read(cache.join("sentinel")).unwrap(), b"untouched");
        assert!(!main.0.join("bazel-rs-out").exists());
    }

    fn external_fixture(main: &Temp, external: &Temp) {
        main.file("MODULE.bazel", &format!("module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path={:?})\n", external.0.to_string_lossy()));
        external.file("MODULE.bazel", "module(name='dep',repo_name='self_dep')\n");
    }

    #[test]
    fn external_load_cache_and_provider_identity_use_canonical_repository() {
        let main = Temp::new();
        let external = Temp::new();
        external_fixture(&main, &external);
        let source = "load('//:defs.bzl', main='P')\nload('@alias//:defs.bzl', dep='P')\nload('@@dep+//:defs.bzl', canonical='P')\nmain != dep and dep == canonical";
        main.file("defs.bzl", "P = provider()\n");
        external.file("defs.bzl", "P = provider()\n");
        let globals = GlobalsBuilder::standard()
            .with(crate::providers::globals)
            .build();
        let first = load_modules(&main.0, "", &parse(source), &globals).unwrap();
        let second = load_modules(&main.0, "", &parse(source), &globals).unwrap();
        let one = first["@alias//:defs.bzl"].get("P").unwrap();
        let two = second["@@dep+//:defs.bzl"].get("P").unwrap();
        drop(first);
        drop(second);
        Module::with_temp_heap(|module| {
            module.set("one", module.heap().access_owned_frozen_value(&one));
            module.set("two", module.heap().access_owned_frozen_value(&two));
            let mut eval = Evaluator::new(&module);
            assert_eq!(
                eval.eval_module(parse("one == two"), &globals)
                    .unwrap()
                    .unpack_bool(),
                Some(true)
            );
        });
        let loaded = load_modules(&main.0, "", &parse(source), &globals).unwrap();
        let modules = loaded.iter().map(|(id, m)| (id.as_str(), m)).collect();
        let loader = ReturnFileLoader { modules: &modules };
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&loader);
            assert_eq!(
                eval.eval_module(parse(source), &globals)
                    .unwrap()
                    .unpack_bool(),
                Some(true)
            );
        });
    }

    #[test]
    fn external_loads_preserve_private_and_package_boundaries() {
        let main = Temp::new();
        let external = Temp::new();
        external_fixture(&main, &external);
        external.file("defs.bzl", "_private=1\npublic=2\n");
        main.error("load('@alias//:defs.bzl', '_private')", "private symbol");
        external.file("nested/BUILD", "");
        external.file("nested/defs.bzl", "public=2\n");
        main.error(
            "load('@alias//:nested/defs.bzl', 'public')",
            "subpackage boundary",
        );
        external.file("defs.bzl", "load('@alias//:defs.bzl', 'public')\n");
        main.error(
            "load('@alias//:defs.bzl', 'public')",
            "unknown apparent repository",
        );
    }

    #[test]
    fn nested_functions_resolve_in_declaring_package_and_keep_imports_alive() {
        let temp = Temp::new();
        temp.file("defs/BUILD", "this is not valid Starlark !!!");
        temp.file("defs/base.bzl", "def base(x):\n    return x + 1\n");
        temp.file("defs/sub/macros.bzl", "load(':base.bzl', 'base')\ndef macro(x):\n    def inner(y):\n        return base(y)\n    return inner(x)\n");
        let source = "load('//defs:sub/macros.bzl', first = 'macro', second = 'macro')\nfirst(19) + second(21)\n";
        let imports = temp.load(source).unwrap();
        assert_eq!(evaluate(source, &imports, &Globals::standard()), 42);
    }

    #[test]
    fn canonical_aliases_and_diamond_dependencies_evaluate_only_once() {
        let temp = Temp::new();
        temp.file("shared.bzl", "value = count_evaluation()\n");
        temp.file("left.bzl", "load(':shared.bzl', 'value')\nleft = value\n");
        temp.file(
            "right.bzl",
            "load('@//:shared.bzl', 'value')\nright = value\n",
        );
        let source = "load(':left.bzl', 'left')\nload(':right.bzl', 'right')\nload('shared.bzl', 'value')\nload('@@//:shared.bzl', alias = 'value')\nleft + right\n";
        let globals = GlobalsBuilder::standard().with(test_globals).build();
        EVALUATIONS.store(0, Ordering::Relaxed);
        let imports = load_modules(&temp.0, "", &parse(source), &globals).unwrap();
        assert_eq!(EVALUATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(imports.len(), 4);
        assert_eq!(evaluate(source, &imports, &globals), 42);
        // The cache belongs to the invocation, not the process.
        load_modules(&temp.0, "", &parse(source), &globals).unwrap();
        assert_eq!(EVALUATIONS.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn top_level_function_registry_retains_private_transitive_defs_not_frozen_closures() {
        let temp = Temp::new();
        temp.file("helpers/BUILD", "");
        temp.file(
            "helpers/mapper.bzl",
            r#"
def _mapper(value):
    return value + 1
def factory():
    captured = 10
    def closure(value):
        return value + captured
    return closure
alias = _mapper
frozen_closure = factory()
lambda_mapper = lambda value: value + 2
"#,
        );
        temp.file("left.bzl", "load('//helpers:mapper.bzl', 'alias', 'frozen_closure', 'lambda_mapper')\nleft = alias\nclosure = frozen_closure\nvia_lambda = lambda_mapper\n");
        temp.file(
            "right.bzl",
            "load('@//helpers:mapper.bzl', 'alias')\nright = alias\n",
        );
        temp.file(
            "root.bzl",
            r#"
load(":left.bzl", "left", "closure", "via_lambda")
load(":right.bzl", "right")
mapper = left
other_alias = right
stored_closure = closure
stored_lambda = via_lambda
def invoke(value):
    return mapper(value)
"#,
        );
        let loaded = load_modules_with_functions(
            &temp.0,
            "",
            &parse("load(':root.bzl', 'invoke')"),
            &Globals::standard(),
        )
        .unwrap();
        let root = &loaded.modules[":root.bzl"];
        let mapper = root.get("mapper").unwrap();
        let alias = root.get("other_alias").unwrap();
        let closure = root.get("stored_closure").unwrap();
        let lambda = root.get("stored_lambda").unwrap();
        assert!(mapper.value().ptr_eq(alias.value()));
        assert_eq!(loaded.functions.len(), 3); // _mapper, factory, invoke; diamond once.
        assert_eq!(
            loaded
                .functions
                .iter()
                .filter(|f| f.value().ptr_eq(mapper.value()))
                .count(),
            1
        );
        assert!(
            !loaded
                .functions
                .iter()
                .any(|f| f.value().ptr_eq(closure.value()))
        );
        assert!(
            !loaded
                .functions
                .iter()
                .any(|f| f.value().ptr_eq(lambda.value()))
        );
        let retained = loaded
            .functions
            .into_iter()
            .find(|f| f.value().ptr_eq(mapper.value()))
            .unwrap();
        drop(loaded.modules);
        drop(mapper);
        drop(alias);
        drop(closure);
        drop(lambda);
        drop(temp);
        Module::with_temp_heap(|module| {
            let function = module.heap().access_owned_frozen_value(&retained);
            let result = Evaluator::new(&module)
                .eval_function(function, &[module.heap().alloc(41)], &[])
                .unwrap();
            assert_eq!(result.unpack_i32(), Some(42));
        });
    }

    #[test]
    fn top_level_function_registry_excludes_reassigned_and_duplicate_names() {
        let ast = parse(
            r#"
load(":dependency.bzl", "loaded")
def untouched(value):
    def nested(value):
        return value
    return nested(value)
def assigned(value):
    return value
assigned = factory()
def unpacked(value):
    return value
[unpacked, other] = [factory(), 1]
def augmented(value):
    return value
augmented += factory()
def duplicate(value):
    return value
def duplicate(value):
    return value
def loaded(value):
    return value
alias = untouched
lambda_mapper = lambda value: value
"#,
        );
        assert_eq!(
            top_level_function_names(&ast),
            BTreeSet::from(["untouched".to_owned()])
        );
    }

    #[test]
    fn empty_function_registry_needs_no_filesystem() {
        let loaded = load_modules_with_functions(
            Path::new("/does-not-exist"),
            "",
            &parse("x = 1"),
            &Globals::standard(),
        )
        .unwrap();
        assert!(loaded.modules.is_empty());
        assert!(loaded.functions.is_empty());
    }

    #[test]
    fn top_level_rules_have_no_context_but_loaded_macros_use_caller_context() {
        let temp = Temp::new();
        let globals = GlobalsBuilder::standard().with(test_globals).build();
        temp.file("bad.bzl", "x = rule()\n");
        let error =
            load_modules(&temp.0, "", &parse("load(':bad.bzl', 'x')"), &globals).unwrap_err();
        assert!(error.contains("rule requires BUILD context"), "{error}");
        temp.file("macro.bzl", "def macro():\n    return rule()\n");
        let source = "load(':macro.bzl', 'macro')\nmacro()\n";
        let imports = load_modules(&temp.0, "", &parse(source), &globals).unwrap();
        let modules = imports.iter().map(|(id, m)| (id.as_str(), m)).collect();
        let loader = ReturnFileLoader { modules: &modules };
        Module::with_temp_heap(|module| {
            let context = 0_i32;
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.set_loader(&loader);
            // BUILD globals intentionally do not include the native callback.
            assert_eq!(
                eval.eval_module(parse(source), &Globals::standard())
                    .unwrap()
                    .unpack_i32(),
                Some(42)
            );
        });
    }

    #[test]
    fn cycles_use_canonical_labels_and_show_the_chain() {
        let temp = Temp::new();
        temp.file("a.bzl", "load('@//:a.bzl', 'x')\nx = 1\n");
        temp.error("load(':a.bzl', 'x')", "//:a.bzl -> //:a.bzl");
        temp.file("a.bzl", "load(':b.bzl', 'x')\n");
        temp.file("b.bzl", "load('@@//:a.bzl', 'x')\n");
        temp.error("load(':a.bzl', 'x')", "//:a.bzl -> //:b.bzl -> //:a.bzl");
    }

    #[test]
    fn depth_limit_is_exact_and_does_not_exhaust_the_rust_stack() {
        let temp = Temp::new();
        for i in 0..MAX_LOAD_DEPTH {
            temp.file(
                &format!("m{i}.bzl"),
                &format!("load(':m{}.bzl', _x='x')\nx=_x\n", i + 1),
            );
        }
        temp.file(&format!("m{MAX_LOAD_DEPTH}.bzl"), "x = 1\n");
        temp.error("load(':m0.bzl', 'x')", "load depth limit 256 exceeded");
        assert!(temp.load("load(':m1.bzl', 'x')").is_ok());
    }

    #[test]
    fn missing_private_external_and_non_bzl_imports_fail() {
        let temp = Temp::new();
        temp.error(
            "load(':missing.bzl', 'x')",
            "cannot load '//:missing.bzl': no such file",
        );
        temp.error("load('@other//:defs.bzl', 'x')", "external repository");
        temp.error("load(':defs.star', 'x')", ".bzl suffix");
        temp.file("defs.bzl", "_private = 1\npublic = 2\n");
        temp.error("load(':defs.bzl', alias = '_private')", "private symbol");
        temp.error("load(':defs.bzl', 'missing')", "cannot load");
        temp.error(
            "load(':defs.bzl', 'public')\nload(':defs.bzl', 'missing')",
            "cannot load",
        );
        let source = "load(':defs.bzl', _local = 'public')\n_local\n";
        assert_eq!(
            evaluate(source, &temp.load(source).unwrap(), &Globals::standard()),
            2
        );
    }

    #[test]
    fn imports_are_file_local_unless_explicitly_reexported() {
        let temp = Temp::new();
        temp.file("a.bzl", "value=42\n");
        temp.file("b.bzl", "load(':a.bzl', 'value')\n");
        temp.error("load(':b.bzl', 'value')", "cannot load");
        temp.file("b.bzl", "load(':a.bzl', _value='value')\nvalue=_value\n");
        assert!(temp.load("load(':b.bzl', 'value')").is_ok());
    }

    #[test]
    fn bzl_loads_must_be_first_except_docstrings() {
        let temp = Temp::new();
        temp.file("a.bzl", "value=42\n");
        temp.file("b.bzl", "before=1\nload(':a.bzl', 'value')\nx=value\n");
        temp.error("load(':b.bzl', 'x')", "must precede");
        temp.file(
            "b.bzl",
            "'''Module docstring.'''\nload(':a.bzl', 'value')\nx=value\n",
        );
        assert!(temp.load("before=1\nload(':b.bzl', 'x')").is_ok());
    }

    #[test]
    fn requires_package_without_evaluating_its_build_and_checks_boundaries() {
        let temp = Temp::new();
        temp.file("pkg/defs.bzl", "x = 1\n");
        temp.error("load('//pkg:defs.bzl', 'x')", "has no regular BUILD");
        temp.file("pkg/BUILD", "invalid BUILD !!!");
        assert!(temp.load("load('//pkg:defs.bzl', 'x')").is_ok());
        temp.error("load(':pkg/defs.bzl', 'x')", "subpackage boundary");
        temp.file("other/BUILD.bazel", "");
        temp.file("other/defs.bzl", "x = 1\n");
        temp.error("load(':other/defs.bzl', 'x')", "subpackage boundary");
        // Bazel 9.3 has no nested repositories: markers are no boundaries.
        for marker in ["MODULE.bazel", "REPO.bazel", "WORKSPACE", "WORKSPACE.bazel"] {
            temp.file(&format!("nested/{marker}"), "");
            temp.file("nested/defs.bzl", "x = 1\n");
            assert!(temp.load("load(':nested/defs.bzl', 'x')").is_ok());
            temp.file("nested/BUILD", "");
            assert!(temp.load("load('//nested:defs.bzl', 'x')").is_ok());
            fs::remove_file(temp.0.join("nested/BUILD")).unwrap();
            fs::remove_file(temp.0.join("nested").join(marker)).unwrap();
        }
    }

    #[test]
    fn loads_from_ignored_directories_follow_containing_package_lookup() {
        let temp = Temp::new();
        temp.file("REPO.bazel", "ignore_directories(['**/node_modules'])\n");
        temp.file(".bazelignore", "gen\n");
        for dir in ["node_modules/pkg", "gen", "a/node_modules"] {
            temp.file(&format!("{dir}/BUILD"), "");
            temp.file(&format!("{dir}/defs.bzl"), "x = 1\n");
        }
        // The nearest existing ancestor package makes the label cross a
        // package boundary (ContainingPackageLookupValue).
        for (load, expected) in [
            (
                "load('//node_modules/pkg:defs.bzl', 'x')",
                "Label '//node_modules/pkg:defs.bzl' is invalid because 'node_modules/pkg' is not a package; perhaps you meant to put the colon here: '//:node_modules/pkg/defs.bzl'?",
            ),
            (
                "load('//gen:defs.bzl', 'x')",
                "Label '//gen:defs.bzl' is invalid because 'gen' is not a package; perhaps you meant to put the colon here: '//:gen/defs.bzl'?",
            ),
        ] {
            assert_eq!(temp.load(load).unwrap_err(), expected);
        }
        temp.file("a/BUILD", "");
        assert_eq!(
            temp.load("load('//a/node_modules:defs.bzl', 'x')")
                .unwrap_err(),
            "Label '//a/node_modules:defs.bzl' is invalid because 'a/node_modules' is not a package; perhaps you meant to put the colon here: '//a:node_modules/defs.bzl'?"
        );
        // An ignored directory with a BUILD file is no subpackage, so the
        // file belongs to the containing package.
        assert!(temp.load("load('//:gen/defs.bzl', 'x')").is_ok());
        assert!(temp.load("load('//a:node_modules/defs.bzl', 'x')").is_ok());
        // Without any containing package: BzlLoadFunction.noBuildFile.
        fs::remove_file(temp.0.join("BUILD")).unwrap();
        assert_eq!(
            temp.load("load('//gen:defs.bzl', 'x')").unwrap_err(),
            "Every .bzl file must have a corresponding package, but '//gen:defs.bzl' does not have one. Please create a BUILD file in the same or any parent directory. Note that this BUILD file does not need to do anything except exist."
        );
    }

    #[test]
    fn parse_evaluation_and_source_type_failures_are_not_ignored() {
        let temp = Temp::new();
        for source in [
            "def broken(:\n",
            "if True:\n    x = 1\n",
            "for x in []:\n    pass\n",
        ] {
            temp.file("bad.bzl", source);
            temp.error("load(':bad.bzl', 'x')", "cannot parse //:bad.bzl");
        }
        temp.file("bad.bzl", "x = 1 // 0\n");
        temp.error("load(':bad.bzl', 'x')", "cannot evaluate //:bad.bzl");
        temp.file("bad.bzl", "x = unknown_name\n");
        temp.error("load(':bad.bzl', 'x')", "unknown_name");
        fs::create_dir(temp.0.join("directory.bzl")).unwrap();
        temp.error(
            "load(':directory.bzl', 'x')",
            "cannot load '//:directory.bzl': is a directory",
        );
        temp.file("plain", "");
        temp.error("load(':plain/defs.bzl', 'x')", "not a directory");
    }

    #[cfg(unix)]
    #[test]
    fn follows_in_root_bzl_symlinks_but_rejects_dangling_escaping_and_package_links() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let outside = Temp::new();
        temp.file("real/defs.bzl", "x = 1\n");
        outside.file("defs.bzl", "x = 2\n");
        symlink("real/defs.bzl", temp.0.join("alias.bzl")).unwrap();
        symlink(temp.0.join("real/defs.bzl"), temp.0.join("absolute.bzl")).unwrap();
        symlink("missing", temp.0.join("dangling.bzl")).unwrap();
        symlink("real", temp.0.join("alias")).unwrap();
        symlink(outside.0.join("defs.bzl"), temp.0.join("escape.bzl")).unwrap();
        // Bazel reads .bzl content through symlinks (FileValue); native
        // follows links that resolve inside the repository (source_links).
        for source in [
            "load(':alias.bzl', 'x')",
            "load(':absolute.bzl', 'x')",
            "load(':alias/defs.bzl', 'x')",
        ] {
            let modules = temp.load(source).unwrap();
            assert_eq!(
                evaluate(&format!("{source}\nx"), &modules, &Globals::standard()),
                1
            );
        }
        temp.error(
            "load(':dangling.bzl', 'x')",
            "cannot load '//:dangling.bzl': no such file",
        );
        symlink("real", temp.0.join("dirlink.bzl")).unwrap();
        temp.error(
            "load(':dirlink.bzl', 'x')",
            "cannot load '//:dirlink.bzl': is a directory",
        );
        symlink("cyc2.bzl", temp.0.join("cyc1.bzl")).unwrap();
        symlink("cyc1.bzl", temp.0.join("cyc2.bzl")).unwrap();
        temp.error(
            "load(':cyc1.bzl', 'x')",
            "circular symlinks detected\n[start of symlink cycle]\n",
        );
        temp.error("load(':escape.bzl', 'x')", "outside its repository root");
        temp.file("real/BUILD", "");
        temp.error(
            "load('//alias:defs.bzl', 'x')",
            "symlink package directories",
        );
    }
}
