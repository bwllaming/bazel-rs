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
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_LOAD_DEPTH: usize = 256;

/// Per-label `.bzl` evaluation self-time and BUILD-file time buckets
/// (`BAZEL_RS_TIMING_MODULES=1`, hidden measurement aid for N62's
/// breakdown: where a package's load wall time goes — shared-graph
/// evaluation by label vs BUILD/macro evaluation). Printed once at
/// process exit through [`report`]; never on unless the variable is set,
/// so user-visible output (and the A/B suites) are unaffected.
pub mod module_timings {
    use std::sync::{Mutex, OnceLock};

    #[derive(Default)]
    struct State {
        /// (label, self seconds, evaluated here vs taken from an in-flight
        /// waiter's recheck)
        labels: Vec<(String, f64, bool)>,
        /// (label, enter seconds) — reading, statically checking and
        /// resolving the source path of a cache miss.
        enters: Vec<(String, f64)>,
        /// (package, [parse seconds, .bzl graph seconds, BUILD
        /// evaluation+freeze seconds]) — target creation and macros run in
        /// the evaluation bucket.
        builds: Vec<(String, [f64; 3])>,
        evaluated: usize,
        waits: usize,
        hits: usize,
        /// Aggregated enter-phase buckets: path resolution, read, parse and
        /// static checks.
        enter_parts: [f64; 3],
        /// Aggregated source_path buckets: repository root (demand,
        /// materialization, warm validation) vs the component walk.
        source_parts: [f64; 2],
        /// (repository, seconds) of the slow root resolutions.
        roots: Vec<(String, f64)>,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);
    static ENABLED: OnceLock<bool> = OnceLock::new();

    pub fn enabled() -> bool {
        *ENABLED.get_or_init(|| std::env::var_os("BAZEL_RS_TIMING_MODULES").is_some())
    }

    fn with<T>(record: impl FnOnce(&mut State) -> T) -> Option<T> {
        if !enabled() {
            return None;
        }
        let mut guard = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        Some(record(guard.get_or_insert_with(State::default)))
    }

    /// One `.bzl` label's evaluation self-time (the `Work::Finish` frame:
    /// visibility checks, evaluation, freeze, publish — waits excluded).
    pub fn record_label(label: &str, seconds: f64, evaluated_here: bool) {
        let _ = with(|state| {
            if evaluated_here {
                state.evaluated += 1;
                state.labels.push((label.to_owned(), seconds, true));
            } else {
                state.waits += 1;
            }
        });
    }

    /// One shared-cache hit at traversal time (no evaluation).
    pub fn record_hit() {
        let _ = with(|state| state.hits += 1);
    }

    /// One cache miss's enter phase (path resolution + read + parse +
    /// static checks), excluding its loads' own work.
    pub fn record_enter(label: &str, seconds: f64) {
        let _ = with(|state| state.enters.push((label.to_owned(), seconds)));
    }

    /// Sub-buckets of the enter phase, aggregated over all labels.
    pub fn record_enter_parts(path: f64, read: f64, parse: f64) {
        let _ = with(|state| {
            state.enter_parts[0] += path;
            state.enter_parts[1] += read;
            state.enter_parts[2] += parse;
        });
    }

    /// One `repositories.root()` resolution from the loader (first demand
    /// materializes/warm-validates the repository; later calls are cached
    /// slot reads). Only meaningful entries are kept for the report.
    pub fn record_root(repository: &str, seconds: f64) {
        let _ = with(|state| {
            if seconds >= 0.01 {
                state.roots.push((repository.to_owned(), seconds));
            }
        });
    }

    /// Sub-buckets of `source_path`: resolving the repository root
    /// (materializing/warm-validating it on first demand) and the rest
    /// (component walk, symlink policy resolution).
    pub fn record_source_parts(root: f64, rest: f64) {
        let _ = with(|state| {
            state.source_parts[0] += root;
            state.source_parts[1] += rest;
        });
    }

    /// One package's BUILD-file buckets.
    pub fn record_build(package: &str, parse: f64, bzl: f64, evaluation: f64) {
        let _ = with(|state| {
            state
                .builds
                .push((package.to_owned(), [parse, bzl, evaluation]))
        });
    }

    /// The one-shot report (stderr, `PERF`-prefixed like the phase marks).
    pub fn report() {
        static REPORTED: OnceLock<()> = OnceLock::new();
        if REPORTED.set(()).is_err() || !enabled() {
            return;
        }
        let Some(state) = with(std::mem::take) else {
            return;
        };
        let mut labels = state.labels;
        labels.sort_by(|a, b| b.1.total_cmp(&a.1));
        let self_total: f64 = labels.iter().map(|(_, seconds, _)| *seconds).sum();
        let mut enters = state.enters;
        enters.sort_by(|a, b| b.1.total_cmp(&a.1));
        let enter_total: f64 = enters.iter().map(|(_, seconds)| *seconds).sum();
        eprintln!(
            "PERF modules: evaluated={} waits={} hits={} self_total={:.3}s enter_total={:.3}s",
            state.evaluated, state.waits, state.hits, self_total, enter_total
        );
        eprintln!(
            "PERF enter-parts: path={:.3}s read={:.3}s parse={:.3}s",
            state.enter_parts[0], state.enter_parts[1], state.enter_parts[2]
        );
        eprintln!(
            "PERF source-parts: root={:.3}s walk={:.3}s",
            state.source_parts[0], state.source_parts[1]
        );
        let mut roots = state.roots;
        roots.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (repository, seconds) in roots.iter().take(40) {
            eprintln!("PERF repo-root {seconds:9.3}s @@{repository}");
        }
        for (label, seconds, _) in labels.iter().take(40) {
            eprintln!("PERF module-self {seconds:9.3}s {label}");
        }
        for (label, seconds) in enters.iter().take(40) {
            eprintln!("PERF module-enter {seconds:9.3}s {label}");
        }
        let mut builds = state.builds;
        builds.sort_by(|a, b| (b.1[1] + b.1[2]).total_cmp(&(a.1[1] + a.1[2])));
        for (package, [parse, bzl, evaluation]) in builds.iter().take(20) {
            eprintln!(
                "PERF build parse={parse:9.3}s bzl={bzl:9.3}s eval={evaluation:9.3}s {package}"
            );
        }
    }
}

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

/// Whether `eval` runs in the repository/extension Latin-1 char model:
/// a callback, or a definition loaded for one (`ExtensionContext.byte_strings`).
pub(crate) fn latin1_profile(eval: &Evaluator<'_, '_, '_>) -> bool {
    crate::extension_ctx::evaluating_callback(eval)
        || crate::repository_ctx::evaluating_callback(eval)
        || eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
            .is_some_and(|context| context.byte_strings)
}

/// [`resolve_label`] with Label.parseWithPackageContext's treatment of an
/// apparent repository the context repository does not see: an invalid
/// Label (RepositorySet::parse_deferred) instead of an error.
pub fn resolve_label_deferred(eval: &Evaluator<'_, '_, '_>, text: &str) -> anyhow::Result<Label> {
    resolve_label_with(eval, text, None, true)
}

fn resolve_label_inner(
    eval: &Evaluator<'_, '_, '_>,
    text: &str,
    budget: Option<&mut crate::repository_budget::HostBudget>,
) -> anyhow::Result<Label> {
    resolve_label_with(eval, text, budget, false)
}

fn resolve_label_with(
    eval: &Evaluator<'_, '_, '_>,
    text: &str,
    mut budget: Option<&mut crate::repository_budget::HostBudget>,
    deferred: bool,
) -> anyhow::Result<Label> {
    // In the Latin-1 char model a non-ASCII label string holds the Latin-1
    // decoding of its UTF-8 bytes; native labels and paths are UTF-8, so it
    // would name a different file. Bazel converts it back to the original
    // bytes. Explicit, like non-ASCII load() labels and repository attribute
    // labels in that profile (docs/LABEL_SYNTAX.md).
    if !text.is_ascii() && latin1_profile(eval) {
        anyhow::bail!(
            "non-ASCII label {text:?} is unsupported in direct repository execution (repository and extension .bzl strings are Latin-1 chars of their UTF-8 bytes)"
        );
    }
    if let Some(repositories) = repositories(eval) {
        let mut resolve = |repository: &str, package: &str| -> anyhow::Result<Label> {
            if let Some(budget) = budget.as_deref_mut() {
                let bound = repositories
                    .resolution_size_bound(text, repository, package)
                    .map_err(anyhow::Error::msg)?;
                budget.bytes(bound).map_err(anyhow::Error::msg)?;
            }
            if deferred {
                repositories.parse_deferred(text, repository, package)
            } else {
                repositories.parse(text, repository, package)
            }
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
    /// The BUILD file's direct loads in statement order, then each loaded
    /// `.bzl` file's direct loads: Package.Declarations.visitLoadGraph's
    /// graph (packages/Package.java), read by `query buildfiles/loadfiles`.
    pub direct_loads: Vec<Label>,
    pub load_graph: HashMap<Label, Vec<Label>>,
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
    /// Direct loads of every evaluated `.bzl` file, in load-statement order
    /// (duplicates kept): BzlLoadFunction's programLoads (:808-870).
    pub load_graph: HashMap<Label, Vec<Label>>,
    /// Repository mapping lookups of the transitive load closure: load
    /// statements with an apparent repository and top-level `Label()` calls
    /// (BzlLoadValue.getRecordedRepoMappings, BzlLoadFunction.java:808-931).
    pub repo_mappings: Vec<RecordedRepoMapping>,
}

/// One RepoRecordedInput.RecordedRepoMapping observation: the repository whose
/// mapping was consulted, the apparent name, and the canonical result (None
/// when no visible repository has that name).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordedRepoMapping {
    pub from: String,
    pub apparent: String,
    pub canonical: Option<String>,
}

thread_local! {
    /// Recorders of the `.bzl` files whose top level is being evaluated by a
    /// provenance-capturing loader (innermost last); see [`record_label_repo_mapping`].
    static LOAD_RECORDERS: std::cell::RefCell<Vec<Vec<RecordedRepoMapping>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The apparent repository of a label string, if it names one (`@name//...`
/// or `@name`, not `@@canonical`): Label.Parts.repo() when !repoIsCanonical.
pub(crate) fn apparent_repository(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('@')?;
    if rest.starts_with('@') {
        return None;
    }
    Some(rest.split_once("//").map_or(rest, |(name, _)| name))
}

/// The canonical name a resolved label's repository records, None when the
/// apparent name was not visible (RepositoryName.isVisible).
fn recorded_canonical(label: &Label) -> Option<String> {
    (!label.repository.starts_with('[')).then(|| label.repository.clone())
}

/// `Label()` (StarlarkRuleClassFunctions.label, :2124-2143) records the
/// mapping lookup of an apparent repository in the thread's
/// RepoMappingRecorder: a module extension's context
/// (StarlarkBaseExternalContext.java:209-216), or the `.bzl` file being
/// loaded (BzlLoadFunction.java:1396). Other phases record nothing.
pub(crate) fn record_label_repo_mapping(eval: &Evaluator<'_, '_, '_>, text: &str, label: &Label) {
    let Some(apparent) = apparent_repository(text) else {
        return;
    };
    let from = if let Some(label) = definition_label(eval) {
        label.repository
    } else if let Some(package) = crate::extension_ctx::definition_package(eval) {
        package.repository
    } else {
        return;
    };
    let entry = RecordedRepoMapping {
        from,
        apparent: apparent.to_owned(),
        canonical: recorded_canonical(label),
    };
    if crate::extension_ctx::record_repo_mapping(eval, &entry) {
        return;
    }
    if eval
        .extra
        .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
        .is_some()
    {
        LOAD_RECORDERS.with(|recorders| {
            if let Some(frame) = recorders.borrow_mut().last_mut() {
                frame.push(entry);
            }
        });
    }
}

/// The `.bzl` modules a definition load entered (a repository rule's, a
/// module extension's or an aspect's defining file and its loads): their
/// edits are never attributed to packages by incremental analysis
/// (crate::snapshot::incremental). Process-wide, in first-seen order.
static DEFINITION_MODULES: std::sync::Mutex<Option<std::collections::BTreeSet<Label>>> =
    std::sync::Mutex::new(None);

fn note_definition_modules(loader: &Loader<'_>, label: &Label) {
    // A speculative extension preparation's modules count once the
    // coordinator commits it ([`speculative_definitions`]).
    let held = SPECULATIVE_DEFINITIONS.with(|held| match held.borrow_mut().as_mut() {
        Some(held) => {
            held.modules.insert(label.clone());
            held.modules.extend(loader.cache.keys().cloned());
            held.modules.extend(loader.load_graph.keys().cloned());
            for loads in loader.load_graph.values() {
                held.modules.extend(loads.iter().cloned());
            }
            true
        }
        None => false,
    });
    if held {
        return;
    }
    let mut modules = DEFINITION_MODULES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let modules = modules.get_or_insert_with(Default::default);
    modules.insert(label.clone());
    modules.extend(loader.cache.keys().cloned());
    modules.extend(loader.load_graph.keys().cloned());
    for loads in loader.load_graph.values() {
        modules.extend(loads.iter().cloned());
    }
}

/// Every module a definition load of this process entered
/// ([`note_definition_modules`]).
pub fn definition_modules() -> Vec<Label> {
    DEFINITION_MODULES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|modules| modules.iter().cloned().collect())
        .unwrap_or_default()
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

/// [`load_definition_in`] (`public_only`) or [`load_module_extension_in`]
/// with the module-phase globals, sharing the repository set's
/// [`DefinitionBzlCache`] (W6): a module another definition load of the
/// invocation evaluated is replayed — its records, functions and print
/// lines at the same DFS positions — instead of read, parsed and
/// evaluated again. The result is the one a fresh loader returns.
pub(crate) fn load_shared_definition_module_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    public_only: bool,
) -> Result<LoadedDefinition, String> {
    let globals = repositories.module_globals().clone();
    let shared = repositories.definition_cache_allowed();
    load_frozen_export_with(repositories, label, symbol, &globals, public_only, shared)
}

fn load_frozen_export_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
    public_only: bool,
) -> Result<LoadedDefinition, String> {
    load_frozen_export_with(repositories, label, symbol, globals, public_only, false)
}

fn load_frozen_export_with(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
    public_only: bool,
    shared_definitions: bool,
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
    loader.definitions = shared_definitions;
    let loaded = loader.load(label.clone());
    note_definition_modules(&loader, label);
    loaded?;
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
        load_graph: loader.load_graph,
        repo_mappings: loader.repo_mappings,
    })
}

/// [`load_shared_definition_any_in`] of a public export.
#[cfg(test)]
pub(crate) fn load_shared_definition_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
    prints: &LoadPrints,
) -> Result<LoadedDefinition, String> {
    load_shared_definition_any_in(repositories, label, symbol, globals, true, prints)
}

/// An export of a BUILD-phase `.bzl` module (an aspect's defining file)
/// over the repository set's shared `.bzl` cache: a module some BUILD file
/// already loaded is not evaluated again, and a module loaded here first
/// is published for later BUILD files (one BzlLoadValue per label, like
/// Bazel's Skyframe). The load is a BUILD-phase load like a package's (the
/// [`BzlCache`] invariant): BUILD-phase globals, the analysis char model,
/// and its print events — the lines of the modules it evaluates, a hit
/// marker for every module the cache serves — into `prints`, which the
/// caller replays like a load unit's (`Engine::emit_load_prints`). No
/// source provenance or repository mapping records are captured.
///
/// `public_only` false is for identities whose private (underscore)
/// top-level binding is legal at the reference site: an `attr(aspects=...)`
/// list may name a private aspect of the same `.bzl` file
/// (`_go_cc_aspect` of rules_go's binary.bzl), which Bazel resolves from
/// the aspect value the rule class captured — never a load() — so the
/// identity re-load here reads the binding at any visibility (N58 on N57).
pub(crate) fn load_shared_definition_any_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    symbol: &str,
    globals: &Globals,
    public_only: bool,
    prints: &LoadPrints,
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
    let mut loader = Loader::shared_build_phase(repositories, globals, prints);
    let loaded = loader.load(label.clone());
    note_definition_modules(&loader, label);
    loaded.map_err(|error| {
        // A parallel wait-cycle abort escaping a definition load has no
        // commit unit above it to re-run inline: it is a genuine
        // re-entrant demand (an extension loading the very module being
        // evaluated on this thread), so it reports as the load cycle it
        // is — the raw sentinel never reaches the user (N62 follow-up).
        if crate::repository_demand::is_parallel_bzl_cycle(&error) {
            format!("load cycle: {label} (demanded while already being evaluated)")
        } else {
            error
        }
    })?;
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
        load_graph: loader.load_graph,
        repo_mappings: loader.repo_mappings,
    })
}

/// A `.bzl` module loaded for `starlark_doc_extract`
/// (StarlarkDocExtract.loadModule: BzlLoadValue.keyForBuild), with what the
/// extractor reads besides the module: its source text and file path, and
/// the load graph (BazelModuleContext.loads).
pub(crate) struct DocModule {
    pub module: FrozenModule,
    pub source: String,
    pub path: PathBuf,
    pub load_graph: HashMap<Label, Vec<Label>>,
}

/// [`load_shared_definition_any_in`] of a whole module: the same
/// BUILD-phase load over the shared `.bzl` cache, then the source read
/// again for the extractor's syntax scan.
pub(crate) fn load_doc_module_in(
    repositories: Arc<RepositorySet>,
    label: &Label,
    globals: &Globals,
    prints: &LoadPrints,
) -> Result<DocModule, String> {
    if !label.name.ends_with(".bzl") && !label.name.ends_with(".scl") {
        return Err(format!("load {label} requires a .bzl or .scl suffix"));
    }
    let mut loader = Loader::shared_build_phase(repositories, globals, prints);
    let loaded = loader.load(label.clone());
    note_definition_modules(&loader, label);
    loaded.map_err(|error| {
        if crate::repository_demand::is_parallel_bzl_cycle(&error) {
            format!("load cycle: {label} (demanded while already being evaluated)")
        } else {
            error
        }
    })?;
    let module = loader
        .cache
        .get(label)
        .cloned()
        .ok_or_else(|| format!("internal loader error: module {label} was not evaluated"))?;
    let path = loader.source_path(label)?;
    let bytes = crate::observed_fs::read(&path)
        .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?;
    let source = String::from_utf8(bytes)
        .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?;
    Ok(DocModule {
        module,
        source,
        path,
        load_graph: loader.load_graph,
    })
}

/// A BUILD-phase `.bzl` module evaluated anew, outside the invocation's
/// shared cache (crate::snapshot::mapper: the defining module of a portable
/// `map_each` function, on a snapshot hit against a pinned repository set
/// and at the save's checks): the BUILD-phase globals and char model, its
/// loads evaluated by the same private loader, every `print()` line
/// dropped (a hit prints only the recorded window; the analysis that saved
/// the snapshot printed them already).
pub(crate) fn load_build_module_private(
    repositories: Arc<RepositorySet>,
    label: &Label,
) -> Result<FrozenModule, String> {
    if !label.name.ends_with(".bzl") {
        return Err(format!("load {label} requires a .bzl suffix"));
    }
    let globals = repositories.build_phase_globals().1;
    let prints = LoadPrints::default();
    let mut loader = Loader::shared_build_phase(repositories, &globals, &prints);
    loader.shared = false;
    loader.load(label.clone())?;
    loader
        .cache
        .get(label)
        .cloned()
        .ok_or_else(|| format!("internal loader error: module {label} was not evaluated"))
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
    // A private (non-shared) load: prints emitted immediately on
    // completion, preserving the historical direct-stderr behavior.
    let prints = LoadPrints::default();
    let result = load_build_file_modules(repositories, package, ast, globals, false, &prints);
    for event in prints.take() {
        if let LoadPrintEvent::Build(line) | LoadPrintEvent::Module { line, .. } = event {
            crate::ui::note(&line);
        }
    }
    result
}

/// [`load_modules_with_functions_in`] for a BUILD file evaluated with the
/// BUILD-phase `.bzl` globals (crate::package::bzl_globals): each `.bzl` is
/// evaluated once per repository set (the invocation) and its frozen module
/// is shared by every package that loads it, as Bazel's BzlLoadValue is
/// computed once per label (so a top-level `print` runs once, too — its
/// lines are buffered in `prints` and emitted by the coordinator at the
/// load unit's commit, review §1.4). Loads are still traversed per package
/// in the same order, with the same cycle, depth and per-requester
/// visibility checks; failures are not cached.
pub(crate) fn load_build_modules_in(
    repositories: Arc<RepositorySet>,
    package: &PackageId,
    ast: &AstModule,
    globals: &Globals,
    prints: &LoadPrints,
) -> Result<LoadedModules, String> {
    load_build_file_modules(repositories, package, ast, globals, true, prints)
}

fn load_build_file_modules(
    repositories: Arc<RepositorySet>,
    package: &PackageId,
    ast: &AstModule,
    globals: &Globals,
    shared: bool,
    prints: &LoadPrints,
) -> Result<LoadedModules, String> {
    let labels = dependencies(&repositories, package, ast)?;
    if labels.is_empty() {
        return Ok(LoadedModules::default());
    }
    warm_load_roots(&repositories, &labels);
    let mut loader = Loader::shared_build_phase(repositories, globals, prints);
    loader.shared = shared;
    for label in labels.iter().cloned() {
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
        direct_loads: labels,
        load_graph: loader.load_graph,
    })
}

/// Bazel requests the loads of a file as one Skyframe batch, fetching
/// their repositories in parallel: warm the module roots of `labels`
/// before the loader demands them one at a time (crate::module_root_warm;
/// only where the sequential engine loads, or for a unit the build
/// commits: a thread that must not fetch may otherwise be loading a
/// package the build never commits).
fn warm_load_roots(repositories: &RepositorySet, labels: &[Label]) {
    if !crate::repository_demand::no_demand_active()
        || crate::repository_demand::committed_unit_active()
    {
        repositories.warm_loads(labels);
    }
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

/// One successfully evaluated BUILD-phase `.bzl` module: what a loader
/// needs to reuse it without reading, parsing or evaluating it again.
struct CachedBzl {
    module: FrozenModule,
    visibility: BzlVisibility,
    /// The module's own retained top-level functions, in name order.
    functions: Vec<OwnedFrozenValue>,
    /// Its direct loads in load-statement order (`dependencies`).
    loads: Vec<Label>,
    /// The module's buffered top-level `print()` lines, in evaluation
    /// order: the coordinator emits them once, at the first commit or
    /// aspect load that reaches the module (review §1.4, W6 review F1).
    prints: Vec<String>,
    /// What reading, parsing and evaluating the module observed for the
    /// analysis snapshot (W6R): every hit replays it — the sequential
    /// engine's load of the module observed it there, or a miss before.
    observations: Option<Arc<crate::observed_fs::Captured>>,
}

/// The failure of loading one `.bzl` label: its error, the top-level
/// `print()` lines its partial evaluation produced (emitted once, with
/// the module's first load, while the error itself replays for every
/// loading package), and — for evaluation failures, which happen after
/// the module's own loads ran — those loads. A later loading package's
/// failure hit replays the dependencies' prints (via their hit markers)
/// before the failing module's own, exactly like the sequential DFS that
/// evaluated them (P2 follow-up 2).
struct CachedFailure {
    error: String,
    prints: Vec<String>,
    loads: Vec<Label>,
    /// What the failed load observed ([`CachedBzl::observations`]).
    observations: Option<Arc<crate::observed_fs::Captured>>,
}

/// A BUILD-phase `.bzl` label's value for the invocation: the evaluated
/// module, or the error of its own reading, parsing, load-visibility check,
/// evaluation or freezing (Bazel's stored BzlLoadValue error). Its print
/// events were emitted once, when it was evaluated.
type CachedResult = Result<Arc<CachedBzl>, Arc<CachedFailure>>;

/// The `.bzl` modules a repository set (one invocation) has evaluated for
/// BUILD files, by canonical label. Lookups and inserts take the lock
/// briefly; it is never held while evaluating. Lookups only: never iterated.
///
/// Invariant (W6 review F1): whichever thread evaluates a label first — a
/// prefetch or pattern-loading worker, or the coordinator — the entry is
/// the same, and so is what the coordinator prints. Every reader and
/// publisher is a BUILD-phase loader ([`Loader::shared_build_phase`]):
/// BUILD-phase globals, the analysis char model (never the Latin-1 model
/// of repository and extension definitions), and print events buffered
/// into a sink — the lines of the modules it evaluated and a hit marker
/// for every module the cache served — that the coordinator replays at
/// its sequential point (a load unit's commit, an aspect definition
/// load), emitting a module's lines once, at the first point that reaches
/// it. No reader prints a module's lines itself or skips a hit's.
pub(crate) struct BzlCache {
    /// Process-unique instance id: the flight registry keys claims by
    /// (cache instance, label), so independent caches never couple.
    id: u64,
    modules: std::sync::Mutex<HashMap<Label, CachedResult>>,
    /// Packages' `.bzl` load graphs by the BUILD file's direct loads
    /// ([`BzlCache::shared_load_graph`], MEM2).
    load_graphs: std::sync::Mutex<HashMap<Vec<Label>, Arc<LoadGraph>>>,
}

/// A package's `.bzl` load graph: each `.bzl` file reachable from the
/// BUILD file's direct loads with its own direct loads
/// (Package.Declarations.visitLoadGraph's graph).
pub type LoadGraph = HashMap<Label, Vec<Label>>;

static NEXT_BZL_CACHE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Default for BzlCache {
    fn default() -> Self {
        Self {
            id: NEXT_BZL_CACHE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            modules: std::sync::Mutex::new(HashMap::new()),
            load_graphs: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for BzlCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.modules.lock().map_or(0, |modules| modules.len());
        f.debug_struct("BzlCache").field("modules", &count).finish()
    }
}

impl BzlCache {
    /// This cache instance's flight-registry key component.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn get(&self, label: &Label) -> Result<Option<CachedResult>, String> {
        Ok(self
            .modules
            .lock()
            .map_err(|_| "bzl module cache poisoned".to_owned())?
            .get(label)
            .cloned())
    }

    /// Keep the first evaluation of a label (a later concurrent one is not
    /// published), like a Skyframe value.
    fn insert(&self, label: Label, result: Result<CachedBzl, CachedFailure>) -> Result<(), String> {
        self.modules
            .lock()
            .map_err(|_| "bzl module cache poisoned".to_owned())?
            .entry(label)
            .or_insert_with(|| result.map(Arc::new).map_err(Arc::new));
        Ok(())
    }

    /// The frozen module of `label` the invocation evaluated, if it did and
    /// its evaluation succeeded (crate::snapshot::mapper: the module a
    /// planned `map_each` function was created by).
    pub(crate) fn module(&self, label: &Label) -> Option<FrozenModule> {
        match self.get(label).ok()?? {
            Ok(cached) => Some(cached.module.clone()),
            Err(_) => None,
        }
    }

    /// The buffered top-level `print()` lines stored for a cached label
    /// (both successes and failures), for the coordinator's load-DFS
    /// replay (review §1.4).
    pub(crate) fn prints(&self, label: &Label) -> Vec<String> {
        self.modules
            .lock()
            .map_or(None, |modules| {
                modules.get(label).map(|result| {
                    result
                        .as_ref()
                        .map_or_else(|failure| failure.prints.clone(), |bzl| bzl.prints.clone())
                })
            })
            .unwrap_or_default()
    }

    /// One shared copy of equal package load graphs (MEM2): every package
    /// kept its own copy of its whole transitive `.bzl` load graph, and
    /// thousands of packages load the same files (all gazelle-generated
    /// Go repositories load the same rules_go macros): ~135 MB of copies
    /// on ~/code's `bworker/wcb/...`. A package's graph is the DFS from
    /// its direct loads over this cache's modules, so packages with the
    /// same direct loads get equal graphs; the memo still compares by
    /// value and keeps a differing graph unshared, so the result is
    /// always exactly the package's own graph.
    pub(crate) fn shared_load_graph(
        &self,
        direct_loads: &[Label],
        graph: LoadGraph,
    ) -> Arc<LoadGraph> {
        let known = self
            .load_graphs
            .lock()
            .ok()
            .and_then(|graphs| graphs.get(direct_loads).cloned());
        match known {
            Some(shared) if *shared == graph => shared,
            Some(_) => Arc::new(graph),
            None => {
                let shared = Arc::new(graph);
                if let Ok(mut graphs) = self.load_graphs.lock() {
                    graphs
                        .entry(direct_loads.to_vec())
                        .or_insert_with(|| shared.clone());
                }
                shared
            }
        }
    }

    /// The frozen heaps of the successfully evaluated modules, for the
    /// W0-A retained-memory breakdown (engine::alloc_report).
    pub(crate) fn frozen_heaps(&self) -> Vec<starlark::values::FrozenHeapRef> {
        self.modules.lock().map_or_else(
            |_| Vec::new(),
            |modules| {
                modules
                    .values()
                    .filter_map(|result| result.as_ref().ok())
                    .map(|bzl| bzl.module.frozen_heap().clone())
                    .collect()
            },
        )
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.modules.lock().unwrap().len()
    }
}

/// One `.bzl` module a definition load (a repository rule's or a module
/// extension's defining file and its loads: module globals, the Latin-1
/// char model, provenance capture) evaluated: exactly what a fresh
/// definition loader records for it, replayed at the same DFS positions
/// when a later definition load of the invocation reaches the module (W6).
struct CachedDefinitionBzl {
    module: FrozenModule,
    visibility: BzlVisibility,
    /// Its own retained top-level functions, in name order.
    functions: Vec<OwnedFrozenValue>,
    /// Its direct loads in load-statement order.
    loads: Vec<Label>,
    source: SourceProvenance,
    /// The repository mapping records of its load statements (taken when
    /// it is entered) and of its top-level evaluation (when it finishes).
    enter_mappings: Vec<RecordedRepoMapping>,
    eval_mappings: Vec<RecordedRepoMapping>,
    /// Its top-level `print()` lines: printed again at every reuse, as a
    /// fresh definition loader prints them on every load.
    prints: Vec<String>,
    /// The `root()`/`mapping()` lookups entering and evaluating it made of
    /// a repository set that records for replay (W6R): a reuse inside a
    /// replay scope repeats them, pinning what the evaluation pinned.
    lookups: Vec<crate::snapshot::replay::Lookup>,
}

/// The definition `.bzl` modules a repository set (one invocation)
/// evaluated, by label (W6): every module extension group preparation and
/// repository rule definition load used to evaluate its whole `.bzl`
/// graph afresh (`@bazel_tools//tools/build_defs/repo:utils.bzl` 52 times
/// on ~/code's oauth2token build). Only successes are kept; a failing
/// module is evaluated again by the next load, as before. Lookups only:
/// never iterated.
#[derive(Default)]
pub(crate) struct DefinitionBzlCache {
    modules: std::sync::Mutex<HashMap<Label, Arc<CachedDefinitionBzl>>>,
}

impl std::fmt::Debug for DefinitionBzlCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.modules.lock().map_or(0, |modules| modules.len());
        f.debug_struct("DefinitionBzlCache")
            .field("modules", &count)
            .finish()
    }
}

impl DefinitionBzlCache {
    fn get(&self, label: &Label) -> Option<Arc<CachedDefinitionBzl>> {
        let shared = self
            .modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
            .cloned();
        shared.or_else(|| {
            SPECULATIVE_DEFINITIONS.with(|held| {
                held.borrow().as_ref().and_then(|held| {
                    held.entries
                        .iter()
                        .find(|(held, _)| held == label)
                        .map(|(_, module)| module.clone())
                })
            })
        })
    }

    fn insert(&self, label: Label, module: CachedDefinitionBzl) {
        let module = Arc::new(module);
        // A speculative extension preparation's entries are this thread's
        // until the coordinator commits it ([`speculative_definitions`]):
        // a hit replays no observations, so an entry must not be shared
        // before the evaluation's observations count.
        let held = SPECULATIVE_DEFINITIONS.with(|held| match held.borrow_mut().as_mut() {
            Some(held) => {
                held.entries.push((label.clone(), module.clone()));
                true
            }
            None => false,
        });
        if !held {
            self.share(label, module);
        }
    }

    fn share(&self, label: Label, module: Arc<CachedDefinitionBzl>) {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(label)
            .or_insert(module);
    }
}

/// crate::snapshot::incremental::note_module_code, held while a
/// speculative extension preparation loads ([`speculative_definitions`]).
fn note_module_code(label: &Label, bytes: &[u8]) {
    let held = SPECULATIVE_DEFINITIONS.with(|held| match held.borrow_mut().as_mut() {
        Some(held) => {
            held.code.push((label.clone(), bytes.to_vec()));
            true
        }
        None => false,
    });
    if !held {
        crate::snapshot::incremental::note_module_code(label, bytes);
    }
}

/// What the definition loads of a speculative extension preparation
/// (crate::extension_speculation) added to the definition `.bzl` cache and
/// to the definition modules, held until the coordinator commits it.
#[derive(Default)]
pub(crate) struct SpeculativeDefinitions {
    entries: Vec<(Label, Arc<CachedDefinitionBzl>)>,
    modules: std::collections::BTreeSet<Label>,
    /// The main-repository modules it read, with their code.
    code: Vec<(Label, Vec<u8>)>,
}

impl std::fmt::Debug for SpeculativeDefinitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeculativeDefinitions")
            .field("entries", &self.entries.len())
            .field("modules", &self.modules.len())
            .finish()
    }
}

thread_local! {
    static SPECULATIVE_DEFINITIONS: RefCell<Option<SpeculativeDefinitions>> =
        const { RefCell::new(None) };
}

/// Run `f` holding what its definition loads add to the definition `.bzl`
/// cache and the definition modules ([`SpeculativeDefinitions`]); its own
/// later loads hit what it added.
pub(crate) fn speculative_definitions<R>(f: impl FnOnce() -> R) -> (R, SpeculativeDefinitions) {
    struct Restore(Option<SpeculativeDefinitions>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            if !self.1 {
                let previous = self.0.take();
                let _ = SPECULATIVE_DEFINITIONS.try_with(|held| {
                    if let Ok(mut held) = held.try_borrow_mut() {
                        *held = previous;
                    }
                });
            }
        }
    }
    let previous = SPECULATIVE_DEFINITIONS.with(|held| held.replace(Some(Default::default())));
    let mut restore = Restore(previous, false);
    let result = f();
    let held = SPECULATIVE_DEFINITIONS
        .with(|held| held.replace(restore.0.take()))
        .unwrap_or_default();
    restore.1 = true;
    (result, held)
}

/// Commit what a speculative preparation held ([`speculative_definitions`])
/// where the sequential engine prepares the group: its entries join the
/// definition `.bzl` cache (unless one exists already) and its modules the
/// definition modules.
///
/// Its definition loads ran inside replay scopes on the coordinator
/// (crate::snapshot::replay): what they pinned — the `root()`/`mapping()`
/// lookups of every module they entered, which the cache entries keep —
/// is pinned here, in a scope, as a load hitting those entries pins it.
pub(crate) fn commit_speculative_definitions(
    repositories: &RepositorySet,
    held: SpeculativeDefinitions,
) {
    let cache = repositories.definition_bzl_cache();
    for (label, module) in held.entries {
        cache.share(label, module);
    }
    for (label, code) in held.code {
        crate::snapshot::incremental::note_module_code(&label, &code);
    }
    if held.modules.is_empty() {
        return;
    }
    if repositories.records_for_replay() {
        let _scope = crate::snapshot::replay::scope();
        for label in &held.modules {
            if let Some(module) = cache.get(label) {
                repositories.repeat_lookups(&module.lookups);
            }
        }
    }
    let mut modules = DEFINITION_MODULES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    modules
        .get_or_insert_with(Default::default)
        .extend(held.modules);
}

/// Buffered stderr output of one package load unit (review §1.4 and the
/// P2 follow-up): BUILD-file `print()` lines, `.bzl` top-level lines of
/// the modules this unit evaluates, and — in DFS position — a marker for
/// EVERY module served from the shared cache (its success or its
/// failure), each meaning "replay that module's stored prints once".
/// The markers keep a FAILED load's replay identical to the sequential
/// loader's output: the first loading package emits a cache-hit
/// module's prints even when the package itself fails later in its
/// BUILD evaluation.
#[derive(Default)]
pub(crate) struct LoadPrints(RefCell<Vec<LoadPrintEvent>>);

/// One buffered output event of a [`LoadPrints`] sink.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LoadPrintEvent {
    /// A BUILD-file `print()` line.
    Build(String),
    /// A top-level `print()` line of a module this unit evaluated.
    Module { label: Label, line: String },
    /// A module served from the shared cache (success or failure):
    /// replay its stored prints if not yet emitted.
    Cached(Label),
}

impl LoadPrints {
    pub(crate) fn push_build(&self, line: String) {
        self.0.borrow_mut().push(LoadPrintEvent::Build(line));
    }

    fn push_module(&self, label: &Label, line: String) {
        self.0.borrow_mut().push(LoadPrintEvent::Module {
            label: label.clone(),
            line,
        });
    }

    fn push_cached(&self, label: &Label) {
        self.0
            .borrow_mut()
            .push(LoadPrintEvent::Cached(label.clone()));
    }

    pub(crate) fn take(&self) -> Vec<LoadPrintEvent> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

impl starlark::PrintHandler for LoadPrints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        self.push_build(crate::debug_print::line(text));
        Ok(())
    }
}

/// A `.bzl` module's own top-level print lines collected during its
/// evaluation (the evaluator's print handler for BUILD-phase loads);
/// appended to the load unit's sink once the module completes, success or
/// failure, preserving evaluation order.
struct ModulePrints {
    lines: RefCell<Vec<String>>,
    /// Also print each line immediately (a definition load records the
    /// lines its module cache replays, and prints them as it always did).
    echo: bool,
}

impl starlark::PrintHandler for ModulePrints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        let text = crate::debug_print::line(text);
        if self.echo {
            crate::ui::note(&text);
        }
        self.lines.borrow_mut().push(text);
        Ok(())
    }
}

impl ModulePrints {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.borrow_mut())
    }
}

/// Per-label single-flight `.bzl` evaluation with in-flight WAITING
/// (review §1.3 as corrected for N62: the global lock serialized
/// evaluations of *independent* labels). One evaluation per label per
/// shared cache: the first traversal to reach a cache miss claims the
/// label and evaluates; a concurrent traversal of the SAME label waits
/// for the claim and then rechecks the shared cache — exactly the
/// Finish-time recheck the global lock performed — so one identity and
/// one print record exist (BzlLoadFunction's stored value). Disjoint
/// labels evaluate in parallel up to the loading workers' width.
///
/// The registry is keyed by (shared-cache instance, label): two
/// repository sets in one process (tests, embedded callers) never
/// couple through it, and a claim only ever guards the cache it will
/// publish to. Winning a claim does NOT by itself license evaluation:
/// a publisher may have inserted (and released) between this thread's
/// cache lookup and its claim, so the winner rechecks the cache and
/// serves the published entry instead of evaluating a second instance
/// whose identity its package would keep (N62 follow-up).
///
/// Claims are never nested: a module's loads are all evaluated before its
/// own `Work::Finish` frame (the DFS front-loads them), so a claim owner
/// never waits while holding a claim and the wait graph cannot cycle in
/// the current architecture. As a safety net for any future nesting, a
/// wait that would close a cycle across threads (or a same-thread
/// re-claim) aborts the unit with the [`crate::repository_demand::
/// PARALLEL_BZL_CYCLE`] sentinel instead of deadlocking; the coordinator
/// re-runs the unit inline, where a genuine load cycle reports with the
/// sequential traversal's message and any spurious abort merely re-loads
/// from the cache.
struct BzlFlight {
    state: std::sync::Mutex<BzlFlightState>,
    completed: std::sync::Condvar,
}

/// One flight-registry key: a shared-cache instance and a label in it.
type BzlFlightKey = (u64, Label);

#[derive(Default)]
struct BzlFlightState {
    /// The label each thread is currently evaluating (its claim).
    inflight: HashMap<BzlFlightKey, std::thread::ThreadId>,
    /// The label each thread is currently waiting for (no claim held).
    waiting_for: HashMap<std::thread::ThreadId, BzlFlightKey>,
}

static BZL_FLIGHT: std::sync::OnceLock<BzlFlight> = std::sync::OnceLock::new();

fn bzl_flight() -> &'static BzlFlight {
    BZL_FLIGHT.get_or_init(|| BzlFlight {
        state: std::sync::Mutex::new(BzlFlightState::default()),
        completed: std::sync::Condvar::new(),
    })
}

/// The outcome of asking to evaluate `label`.
#[derive(Debug)]
enum BzlClaim {
    /// This thread evaluates; release the claim when published.
    Owned,
    /// Another thread's evaluation finished (published or not): recheck
    /// the shared cache before deciding to evaluate.
    Waited,
}

/// The held per-label claim; releases and wakes waiters on drop (including
/// panics and sentinel aborts, so a claim never outlives its frame).
struct BzlClaimGuard {
    key: BzlFlightKey,
}

impl Drop for BzlClaimGuard {
    fn drop(&mut self) {
        let flight = bzl_flight();
        let mut state = flight
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.inflight.remove(&self.key);
        flight.completed.notify_all();
    }
}

/// Test hook (N62 follow-up): invoked between a Finish frame's cache
/// lookup and its claim, so a unit test can force the interleaving in
/// which another traversal publishes the label in that window. The
/// winner must then serve the published entry, never re-evaluate.
#[cfg(test)]
type PreClaimHook = std::sync::Arc<dyn Fn(&Label) + Send + Sync>;

#[cfg(test)]
static PRE_CLAIM_HOOK: std::sync::Mutex<Option<PreClaimHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn invoke_pre_claim_hook(label: &Label) {
    let hook = PRE_CLAIM_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(label);
    }
}

#[cfg(test)]
fn set_pre_claim_hook(hook: Option<PreClaimHook>) {
    *PRE_CLAIM_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = hook;
}

/// Claim `label` in `cache` for evaluation, or wait for the current owner
/// to finish. A wait that would close a cycle across threads aborts with
/// the [`crate::repository_demand::PARALLEL_BZL_CYCLE`] sentinel.
fn bzl_claim(cache: u64, label: &Label) -> Result<BzlClaim, String> {
    let current = std::thread::current().id();
    let key: BzlFlightKey = (cache, label.clone());
    let flight = bzl_flight();
    let mut state = flight
        .state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    loop {
        match state.inflight.get(&key).copied() {
            None => {
                state.inflight.insert(key, current);
                return Ok(BzlClaim::Owned);
            }
            Some(owner) if owner == current => {
                // Defensive: a same-thread re-claim means nested Finish
                // frames (a load cycle through the claim itself).
                return Err(crate::repository_demand::PARALLEL_BZL_CYCLE.to_owned());
            }
            Some(owner) => {
                // Walk the wait graph: claims are never nested today, so
                // this terminates immediately in practice; a future nested
                // claim cycle must abort instead of deadlocking.
                let mut walker = owner;
                let mut cyclic = false;
                loop {
                    if walker == current {
                        cyclic = true;
                        break;
                    }
                    match state
                        .waiting_for
                        .get(&walker)
                        .and_then(|next| state.inflight.get(next).copied())
                    {
                        Some(next_owner) => walker = next_owner,
                        None => break,
                    }
                }
                if cyclic {
                    return Err(format!(
                        "{}: waiting for {} would close a load cycle across threads",
                        crate::repository_demand::PARALLEL_BZL_CYCLE,
                        label
                    ));
                }
                state.waiting_for.insert(current, key.clone());
                let (unlocked, _resume) = flight
                    .completed
                    .wait_timeout(state, std::time::Duration::from_secs(3600))
                    .unwrap_or_else(|poison| poison.into_inner());
                state = unlocked;
                state.waiting_for.remove(&current);
                if !state.inflight.contains_key(&key) {
                    return Ok(BzlClaim::Waited);
                }
                // Someone else claimed it before we woke; wait again.
            }
        }
    }
}

struct Loader<'a> {
    repositories: Arc<RepositorySet>,
    /// Reuse and publish modules in the repository set's [`BzlCache`].
    shared: bool,
    globals: &'a Globals,
    cache: HashMap<Label, FrozenModule>,
    /// Stored with each frozen module; checks are per requesting package.
    visibilities: HashMap<Label, BzlVisibility>,
    functions: Vec<OwnedFrozenValue>,
    provenance_capture: bool,
    sources: BTreeMap<String, SourceProvenance>,
    /// Direct loads of every evaluated `.bzl` file.
    load_graph: HashMap<Label, Vec<Label>>,
    /// Provenance capture only: repository mapping lookups of load
    /// statements and top-level `Label()` calls ([`LoadedDefinition`]).
    repo_mappings: Vec<RecordedRepoMapping>,
    /// The load unit's buffered print output (BUILD-phase loads): module
    /// top-level `print()` lines land here instead of stderr, replayed by
    /// the coordinator at the unit's commit (review §1.4). `None` for
    /// definition loads outside the BUILD phase, which print immediately.
    prints: Option<&'a LoadPrints>,
    /// Reuse and publish modules in the repository set's
    /// [`DefinitionBzlCache`] (definition loads of the repository and
    /// module-extension machinery, W6).
    definitions: bool,
    /// The load-statement mapping records of entered, not yet finished
    /// modules (for the definition cache).
    entered_mappings: HashMap<Label, Vec<RecordedRepoMapping>>,
    /// The replay-capture lookups entering them made (for the definition
    /// cache, W6R).
    entered_lookups: HashMap<Label, Vec<crate::snapshot::replay::Lookup>>,
}

enum Work {
    Enter(Label),
    /// A module to evaluate once its loads are, with what entering it
    /// observed (W6R: kept with its cache entry).
    Finish(
        Label,
        PathBuf,
        Box<AstModule>,
        Vec<Label>,
        Option<Arc<crate::observed_fs::Captured>>,
    ),
    /// A module of the invocation's cache, after its loads were visited.
    Reuse(Label, Arc<CachedBzl>),
    /// A cached failure, after its stored loads were visited: emit the
    /// module's hit marker, then fail with the stored error.
    FailCached(Label, Arc<CachedFailure>),
    /// A module of the definition cache, after its loads were visited.
    ReuseDefinition(Label, Arc<CachedDefinitionBzl>),
}

impl<'a> Loader<'a> {
    fn new(
        repositories: Arc<RepositorySet>,
        globals: &'a Globals,
        provenance_capture: bool,
    ) -> Self {
        Self {
            repositories,
            shared: false,
            globals,
            cache: HashMap::new(),
            visibilities: HashMap::new(),
            functions: Vec::new(),
            provenance_capture,
            sources: BTreeMap::new(),
            load_graph: HashMap::new(),
            repo_mappings: Vec::new(),
            prints: None,
            definitions: false,
            entered_mappings: HashMap::new(),
            entered_lookups: HashMap::new(),
        }
    }

    /// A BUILD-phase loader over the repository set's shared [`BzlCache`]
    /// (its invariant): `globals` are the BUILD-phase `.bzl` globals,
    /// modules evaluate in the analysis char model without provenance
    /// capture, and every print event goes to `prints`. Callers that must
    /// not share the cache (a private load) clear `shared` afterwards.
    fn shared_build_phase(
        repositories: Arc<RepositorySet>,
        globals: &'a Globals,
        prints: &'a LoadPrints,
    ) -> Self {
        let mut loader = Self::new(repositories, globals, false);
        loader.shared = true;
        loader.prints = Some(prints);
        loader
    }

    /// A shared-cache hit for `label`: schedule its reuse (loads visited
    /// exactly as if evaluated here). False on a miss or when this loader
    /// does not share the cache.
    fn shared_cache_hit(
        &mut self,
        label: &Label,
        work: &mut Vec<Work>,
        active: &mut Vec<Label>,
    ) -> Result<bool, String> {
        if !self.shared {
            return Ok(false);
        }
        let Some(cached) = self.repositories.build_bzl_cache().get(label)? else {
            return Ok(false);
        };
        crate::loading::module_timings::record_hit();
        // The sequential engine's load of this module obtained its root
        // here, at the module's entry (`source_path`, before its loads):
        // whoever evaluated it may have been a worker (W6 review F2
        // follow-up). A success or a failure alike. Likewise what its
        // load observed for the analysis snapshot (W6R).
        self.repositories.note_obtained(&label.repository);
        if crate::observed_fs::hook(crate::observed_fs::Hook::Bzl) {
            crate::observed_fs::replay_some(match &cached {
                Ok(cached) => cached.observations.as_ref(),
                Err(failure) => failure.observations.as_ref(),
            });
        }
        let cached = match cached {
            Ok(cached) => cached,
            // A cached failure replays for every loading package (P2
            // follow-up 2): the module's stored loads are visited first —
            // their hit markers replay the dependencies' prints — then the
            // deferred fail item pushes the module's own marker and fails,
            // exactly the postorder the sequential DFS produced.
            Err(failure) => {
                let loads = failure.loads.clone();
                active.push(label.clone());
                self.load_graph.insert(label.clone(), loads.clone());
                work.push(Work::FailCached(label.clone(), failure));
                work.extend(loads.into_iter().rev().map(Work::Enter));
                return Ok(true);
            }
        };
        active.push(label.clone());
        let loads = cached.loads.clone();
        self.load_graph.insert(label.clone(), loads.clone());
        work.push(Work::Reuse(label.clone(), cached));
        work.extend(loads.into_iter().rev().map(Work::Enter));
        Ok(true)
    }

    fn load(&mut self, label: Label) -> Result<(), String> {
        // The shared cache's invariant: one char model, one print sink.
        debug_assert!(
            !self.shared || (!self.provenance_capture && self.prints.is_some()),
            "a shared .bzl cache loader must be a BUILD-phase loader"
        );
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
                    // A cached module's loads are visited exactly as if it
                    // were evaluated here (same order, cycle and depth checks).
                    // A cached failure is replayed for every loading package.
                    // A miss only reads and parses here: parsing creates no
                    // identity, so it needs no single-flight guard (review
                    // §1.3); the evaluation in Work::Finish takes it.
                    if self.definitions
                        && let Some(cached) = self.repositories.definition_bzl_cache().get(&label)
                    {
                        // Exactly what entering it records (its source and
                        // its load statements' mapping records), then its
                        // loads, then its evaluation's records at reuse.
                        crate::loading::module_timings::record_hit();
                        self.repositories.repeat_lookups(&cached.lookups);
                        self.sources
                            .insert(label.to_string(), cached.source.clone());
                        self.repo_mappings
                            .extend(cached.enter_mappings.iter().cloned());
                        self.load_graph.insert(label.clone(), cached.loads.clone());
                        active.push(label.clone());
                        let loads = cached.loads.clone();
                        work.push(Work::ReuseDefinition(label, cached));
                        work.extend(loads.into_iter().rev().map(Work::Enter));
                        continue;
                    }
                    if !self.shared_cache_hit(&label, &mut work, &mut active)? {
                        let enter_started = std::time::Instant::now();
                        let recorded = self.repo_mappings.len();
                        // What a cache entry's hits replay (W6R).
                        let ((entered, observations), entered_lookups) =
                            self.definition_lookups(|loader| {
                                loader.capture_shared(|loader| loader.enter_module(&label))
                            });
                        let (path, ast, dependencies) = entered.or_else(|error| {
                            self.failed(&label, error, Vec::new(), Vec::new(), observations.clone())
                        })?;
                        if self.definitions {
                            self.entered_mappings
                                .insert(label.clone(), self.repo_mappings[recorded..].to_vec());
                            self.entered_lookups.insert(label.clone(), entered_lookups);
                        }
                        crate::loading::module_timings::record_enter(
                            &label.to_string(),
                            enter_started.elapsed().as_secs_f64(),
                        );
                        self.load_graph.insert(label.clone(), dependencies.clone());
                        active.push(label.clone());
                        work.extend(
                            std::iter::once(Work::Finish(
                                label,
                                path,
                                Box::new(ast),
                                dependencies.clone(),
                                observations,
                            ))
                            .chain(dependencies.into_iter().rev().map(Work::Enter)),
                        );
                    }
                }
                Work::FailCached(label, failure) => {
                    if let Some(sink) = self.prints {
                        sink.push_cached(&label);
                    }
                    let finished = active.pop();
                    debug_assert_eq!(finished.as_ref(), Some(&label));
                    return Err(failure.error.clone());
                }
                Work::Reuse(label, cached) => {
                    self.functions.extend(cached.functions.iter().cloned());
                    self.cache.insert(label.clone(), cached.module.clone());
                    self.visibilities
                        .insert(label.clone(), cached.visibility.clone());
                    // The hit marker, in DFS position: the coordinator
                    // replays this module's stored prints once (P2
                    // follow-up — also when this unit's load later fails).
                    if let Some(sink) = self.prints {
                        sink.push_cached(&label);
                    }
                    let finished = active.pop();
                    debug_assert_eq!(finished.as_ref(), Some(&label));
                }
                Work::ReuseDefinition(label, cached) => {
                    // A fresh definition load prints the module's lines
                    // when it evaluates it, here.
                    for line in &cached.prints {
                        crate::ui::note(line);
                    }
                    self.repo_mappings
                        .extend(cached.eval_mappings.iter().cloned());
                    self.functions.extend(cached.functions.iter().cloned());
                    self.cache.insert(label.clone(), cached.module.clone());
                    self.visibilities
                        .insert(label.clone(), cached.visibility.clone());
                    let finished = active.pop();
                    debug_assert_eq!(finished.as_ref(), Some(&label));
                }
                Work::Finish(label, path, ast, loads, entered) => {
                    let own_functions = self.functions.len();
                    let own_mappings = self.repo_mappings.len();
                    // Per-label single-flight with in-flight waiting
                    // (review §1.3, N62): claim the label and evaluate, or
                    // wait for the owner and recheck — a concurrent
                    // traversal may have published this label — or its
                    // failure — and exactly one evaluation must create its
                    // identity and prints. Independent labels evaluate in
                    // parallel on other workers. A wait that would close a
                    // cycle across threads aborts the unit (the coordinator
                    // re-runs it inline, reproducing the sequential cycle
                    // message) instead of deadlocking.
                    // The claim loop: serve from the cache whenever an
                    // entry exists — before claiming, after waiting, and
                    // (N62 follow-up) after WINNING a claim, because a
                    // publisher may have inserted and released between
                    // this thread's lookup and its claim; evaluating then
                    // would create a second instance whose identity this
                    // package keeps even though the cache keeps the
                    // first. The claim is held across the evaluation and
                    // publication below and released at the frame's end.
                    let mut served_from_cache = false;
                    let _claim: Option<BzlClaimGuard> = {
                        let cache_id = self.repositories.build_bzl_cache().id();
                        let mut guard: Option<BzlClaimGuard> = None;
                        loop {
                            if self.shared {
                                let served = self.repositories.build_bzl_cache().get(&label)?;
                                if let Some(served) = &served {
                                    // Its root was demanded at the entry
                                    // already; noted like every hit, and
                                    // its observations replayed (W6R).
                                    self.repositories.note_obtained(&label.repository);
                                    if crate::observed_fs::hook(crate::observed_fs::Hook::Bzl) {
                                        crate::observed_fs::replay_some(match served {
                                            Ok(cached) => cached.observations.as_ref(),
                                            Err(failure) => failure.observations.as_ref(),
                                        });
                                    }
                                }
                                match served {
                                    Some(Ok(cached)) => {
                                        self.functions.extend(cached.functions.iter().cloned());
                                        self.cache.insert(label.clone(), cached.module.clone());
                                        self.visibilities
                                            .insert(label.clone(), cached.visibility.clone());
                                        if let Some(sink) = self.prints {
                                            sink.push_cached(&label);
                                        }
                                        let finished = active.pop();
                                        debug_assert_eq!(finished.as_ref(), Some(&label));
                                        served_from_cache = true;
                                        break;
                                    }
                                    Some(Err(failure)) => {
                                        // This traversal's DFS already visited the
                                        // module's loads (their markers are in the
                                        // sink); only the module's own marker is
                                        // missing before the stored error.
                                        if let Some(sink) = self.prints {
                                            sink.push_cached(&label);
                                        }
                                        return Err(failure.error.clone());
                                    }
                                    None => {}
                                }
                            }
                            if guard.is_some() {
                                // We hold the claim and the cache is still
                                // empty on the post-claim recheck: evaluate.
                                break;
                            }
                            if !self.shared {
                                break;
                            }
                            #[cfg(test)]
                            invoke_pre_claim_hook(&label);
                            match bzl_claim(cache_id, &label)? {
                                BzlClaim::Owned => {
                                    guard = Some(BzlClaimGuard {
                                        key: (cache_id, label.clone()),
                                    });
                                    // Loop: recheck the cache before
                                    // evaluating (a publisher may have
                                    // won the window before our claim).
                                    continue;
                                }
                                // The owner published (or aborted without
                                // publishing): recheck before evaluating.
                                BzlClaim::Waited => continue,
                            }
                        }
                        guard
                    };
                    if served_from_cache {
                        continue;
                    }
                    let self_time = std::time::Instant::now();
                    let ((finished, evaluated), evaluated_lookups) =
                        self.definition_lookups(|loader| {
                            loader
                                .capture_shared(|loader| loader.finish_module(&label, &path, *ast))
                        });
                    let observations = crate::observed_fs::Captured::join(entered, evaluated);
                    let (frozen, visibility, prints) = match finished {
                        Ok(finished) => finished,
                        Err((error, prints)) => {
                            self.failed(&label, error, prints, loads.clone(), observations.clone())?
                        }
                    };
                    if self.definitions
                        && let Some(source) = self.sources.get(&label.to_string())
                    {
                        let enter_mappings =
                            self.entered_mappings.remove(&label).unwrap_or_default();
                        let mut lookups = self.entered_lookups.remove(&label).unwrap_or_default();
                        for lookup in evaluated_lookups {
                            if !lookups.contains(&lookup) {
                                lookups.push(lookup);
                            }
                        }
                        self.repositories.definition_bzl_cache().insert(
                            label.clone(),
                            CachedDefinitionBzl {
                                module: frozen.clone(),
                                visibility: visibility.clone(),
                                functions: self.functions[own_functions..].to_vec(),
                                loads: loads.clone(),
                                source: source.clone(),
                                enter_mappings,
                                eval_mappings: self.repo_mappings[own_mappings..].to_vec(),
                                prints: prints.clone(),
                                lookups,
                            },
                        );
                    }
                    if self.shared {
                        self.repositories.build_bzl_cache().insert(
                            label.clone(),
                            Ok(CachedBzl {
                                module: frozen.clone(),
                                visibility: visibility.clone(),
                                functions: self.functions[own_functions..].to_vec(),
                                loads,
                                prints,
                                observations,
                            }),
                        )?;
                    }
                    crate::loading::module_timings::record_label(
                        &label.to_string(),
                        self_time.elapsed().as_secs_f64(),
                        true,
                    );
                    self.cache.insert(label.clone(), frozen);
                    self.visibilities.insert(label.clone(), visibility);
                    let finished = active.pop();
                    debug_assert_eq!(finished.as_ref(), Some(&label));
                }
            }
        }
        Ok(())
    }

    /// Publish the failure of `label`'s own loading in the invocation's
    /// cache. Cycle and depth errors depend on the traversal and are never
    /// published; a module whose load fails is not evaluated and so caches
    /// nothing itself (the failing load is replayed when it is visited).
    /// The evaluation's buffered print lines (empty unless it evaluated)
    /// are stored with the failure so the coordinator can emit them once.
    fn failed<T>(
        &self,
        label: &Label,
        error: String,
        prints: Vec<String>,
        loads: Vec<Label>,
        observations: Option<Arc<crate::observed_fs::Captured>>,
    ) -> Result<T, String> {
        // The inline-rerun sentinels (no-demand, parallel wait-cycle) are
        // never published (P2: a worker's abort is environmental, not a
        // stored BzlLoadValue error; the coordinator's inline retry must
        // evaluate the module for real).
        if self.shared && !crate::repository_demand::is_inline_rerun_sentinel(&error) {
            self.repositories.build_bzl_cache().insert(
                label.clone(),
                Err(CachedFailure {
                    error: error.clone(),
                    prints,
                    loads,
                    observations,
                }),
            )?;
        }
        Err(error)
    }

    /// Run `f` on this loader capturing what it observes for the analysis
    /// snapshot when this loader publishes to the shared cache (W6R: a
    /// worker may evaluate a module for a unit the build never commits, or
    /// one that aborts; the sequential engine's load, a later cache hit,
    /// replays the entry's set). Other loaders observe in place.
    fn capture_shared<R>(
        &mut self,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (R, Option<Arc<crate::observed_fs::Captured>>) {
        if self.shared {
            crate::observed_fs::capture(|| f(self))
        } else {
            (f(self), None)
        }
    }

    /// Run `f` on this loader noting the replay capture's lookups it makes
    /// when this loader publishes to the definition `.bzl` cache under a
    /// repository set that records for replay (W6R,
    /// [`CachedDefinitionBzl::lookups`]); none otherwise.
    fn definition_lookups<R>(
        &mut self,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (R, Vec<crate::snapshot::replay::Lookup>) {
        if self.definitions && self.repositories.records_for_replay() {
            crate::snapshot::replay::lookups(|| f(self))
        } else {
            (f(self), Vec::new())
        }
    }

    /// Read, parse and statically check a module; its direct loads.
    fn enter_module(&mut self, label: &Label) -> Result<(PathBuf, AstModule, Vec<Label>), String> {
        let timing_path = std::time::Instant::now();
        let path = self.source_path(label)?;
        let timing_read = std::time::Instant::now();
        let bytes = crate::observed_fs::read(&path)
            .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?;
        if label.repository.is_empty() {
            note_module_code(label, &bytes);
        }
        let timing_parse = std::time::Instant::now();
        let source = std::str::from_utf8(&bytes)
            .map_err(|e| format!("cannot read {label} ({}): {e}", path.display()))?;
        // Bazel lexes a file as the Latin-1 chars of its bytes
        // (StarlarkUtil.createParserInput, skyframe/StarlarkUtil
        // .java:43-67: ParserInput.fromLatin1). Repository and
        // extension execution model those chars as U+0000..U+00FF
        // (crate::repository_text), so the source is decoded the
        // same way: a raw non-ASCII character is one char per
        // UTF-8 byte, exactly as in Bazel. Analysis keeps UTF-8.
        let source = if self.provenance_capture {
            crate::host_os::latin1_text(&bytes)
        } else {
            source.to_owned()
        };
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
        crate::debug_print::record_source(&label.to_string(), &path);
        // Bazel's lexer rejects invalid escapes in every .bzl
        // (Lexer.java:389-403), whether or not the code runs.
        crate::repository_text::validate_source(&ast)?;
        if self.provenance_capture {
            crate::repository_text::validate_execution_load_labels(&ast)?;
        }
        validate_bzl_load_order(&ast)?;
        crate::guarded_globals::check(label, &ast)?;
        crate::guarded_globals::check_universe(&ast)?;
        let dependencies = dependencies(&self.repositories, &label.package_id(), &ast)?;
        warm_load_roots(&self.repositories, &dependencies);
        if self.provenance_capture {
            // Label.parseWithPackageContext with the load's recorder
            // (BzlLoadFunction.getLoadLabels, :808-820).
            for (load, resolved) in ast.loads().iter().zip(&dependencies) {
                if let Some(apparent) = apparent_repository(load.module_id) {
                    self.repo_mappings.push(RecordedRepoMapping {
                        from: label.repository.clone(),
                        apparent: apparent.to_owned(),
                        canonical: recorded_canonical(resolved),
                    });
                }
            }
        }
        self.enter_module_timings(timing_path, timing_read, timing_parse);
        Ok((path, ast, dependencies))
    }

    /// The enter-phase buckets of one [`Self::enter_module`] call.
    fn enter_module_timings(
        &self,
        timing_path: std::time::Instant,
        timing_read: std::time::Instant,
        timing_parse: std::time::Instant,
    ) {
        crate::loading::module_timings::record_enter_parts(
            timing_read.duration_since(timing_path).as_secs_f64(),
            timing_parse.duration_since(timing_read).as_secs_f64(),
            timing_parse.elapsed().as_secs_f64(),
        );
    }

    /// Check a module's load visibilities, evaluate and freeze it, and
    /// retain its top-level functions (appended to `self.functions`).
    /// Returns the module's buffered top-level `print()` lines with the
    /// result; on failure they are returned with the error (review §1.4),
    /// already appended to the unit's sink. Definition loads outside the
    /// BUILD phase keep the immediate default handler and buffer nothing.
    fn finish_module(
        &mut self,
        label: &Label,
        path: &Path,
        ast: AstModule,
    ) -> Result<FinishedModule, FailedModule> {
        let function_names = top_level_function_names(&ast);
        // BzlLoadFunction.java:845-856: after the direct loads are
        // computed, before this file executes.
        self.check_load_visibilities(
            &label.package_id(),
            &format!("module {label}"),
            &path.to_string_lossy(),
            &ast,
        )
        .map_err(|error| (error, Vec::new()))?;
        let imports = self
            .imports(&label.package_id(), &ast)
            .map_err(|error| (error, Vec::new()))?;
        let modules = imports.iter().map(|(id, m)| (id.as_str(), m)).collect();
        let file_loader = ReturnFileLoader { modules: &modules };
        let buffered = ModulePrints {
            lines: RefCell::new(Vec::new()),
            echo: self.prints.is_none(),
        };
        let evaluated = Module::with_temp_heap(|module| {
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
                eval.set_loader(&file_loader);
                if self.prints.is_some() || self.definitions {
                    eval.set_print_handler(&buffered);
                }
                // Repository/extension definitions evaluate in the
                // Latin-1 char model; BUILD/analysis loads do not.
                let _model =
                    starlark::values::string::enter_latin1_char_model(self.provenance_capture);
                if self.provenance_capture {
                    LOAD_RECORDERS.with(|recorders| recorders.borrow_mut().push(Vec::new()));
                }
                let evaluated = eval.eval_module(ast, self.globals);
                if self.provenance_capture {
                    let recorded = LOAD_RECORDERS
                        .with(|recorders| recorders.borrow_mut().pop())
                        .unwrap_or_default();
                    self.repo_mappings.extend(recorded);
                }
                evaluated.map_err(|e| format!("cannot evaluate {label}: {e}"))?;
                drop(eval);
                initialization.finish()
            };
            module
                .freeze_named(FrozenHeapName::User(Box::new(label.to_string())))
                .map_err(|e| format!("cannot freeze {label}: {e:?}"))
                .map(|frozen| (frozen, visibility))
        });
        // The module's lines join the unit's buffered output whether it
        // succeeded or failed, in evaluation order — EXCEPT a no-demand
        // sentinel abort (P2 follow-up 2): those partial lines are
        // dropped, or the early emit would mark the module emitted and
        // suppress the inline re-run's full lines (a module printing "a",
        // demanding a repository, then printing "b" would lose "b").
        let prints = buffered.take();
        let mut evaluated_prints = Vec::new();
        let evaluated = match evaluated {
            Ok(finished) => {
                if let Some(sink) = self.prints {
                    for line in &prints {
                        sink.push_module(label, line.clone());
                    }
                }
                evaluated_prints = prints.clone();
                Ok(finished)
            }
            Err(error) => {
                if let Some(sink) = self.prints
                    && !crate::repository_demand::is_needs_repository(&error)
                {
                    for line in &prints {
                        sink.push_module(label, line.clone());
                    }
                }
                Err((error, prints))
            }
        };
        let (frozen, visibility) = evaluated?;
        for name in function_names {
            let (function, _) = frozen.get_any_visibility(&name).map_err(|e| {
                (
                    format!("cannot retain top-level function {name:?} from {label}: {e}"),
                    Vec::new(),
                )
            })?;
            self.functions.push(function);
        }
        Ok((frozen, visibility, evaluated_prints))
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
        let timing_root = std::time::Instant::now();
        // This checks every package-directory component (including symlinks and
        // nested workspaces), without reading/evaluating the BUILD contents.
        let root = self.repositories.root(&label.repository).map_err(|error| {
            // Name the demanded file: the mapping entry resolved, only the
            // built-in contents are absent (docs/BUILTIN_MODULES.md).
            if error == crate::repository::BAZEL_TOOLS_UNAVAILABLE {
                format!("cannot load '{label}': {error}")
            } else {
                error
            }
        })?;
        let timing_rest = std::time::Instant::now();
        let root_seconds = timing_rest.duration_since(timing_root).as_secs_f64();
        crate::loading::module_timings::record_source_parts(root_seconds, 0.0);
        crate::loading::module_timings::record_root(&label.repository, root_seconds);
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
            let mut metadata = match crate::observed_fs::symlink_metadata(&path) {
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
                crate::repository_demand::take_order_dependent();
                let resolution =
                    crate::source_links::resolve_with_policy(root, Path::new(&relative), &policy);
                // A link decision that depends on the repositories
                // materialized so far is the sequential engine's to take
                // (a worker aborts; crate::repository_demand::order_dependent).
                if crate::repository_demand::take_order_dependent() {
                    return Err(crate::repository_demand::order_dependent(&format!(
                        "load {label} through a symlink"
                    )));
                }
                match resolution {
                    Err(error) if error.starts_with("circular symlinks detected") => {
                        return Err(error);
                    }
                    Err(error) => return Err(format!("load {label}: {error}")),
                    Ok(crate::source_links::Resolution::Missing(_)) => {
                        return Err(no_such_file());
                    }
                    Ok(crate::source_links::Resolution::Resolved(resolved)) => {
                        metadata = crate::observed_fs::metadata(&resolved).map_err(inspect)?;
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
                let names: &[&str] = if ignored.matching_entry(&relative).is_some()
                    || crate::ignored::is_deleted_directory(&path)
                {
                    &[]
                } else {
                    &["BUILD.bazel", "BUILD"]
                };
                for name in names {
                    let build = path.join(name);
                    match crate::observed_fs::metadata(&build) {
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
        crate::loading::module_timings::record_source_parts(
            0.0,
            timing_rest.elapsed().as_secs_f64(),
        );
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

/// A module's successful finish_module outcome with its buffered lines.
type FinishedModule = (FrozenModule, BzlVisibility, Vec<String>);

/// A module's failed finish_module outcome: the error and the buffered
/// print lines its partial evaluation produced.
type FailedModule = (String, Vec<String>);

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
    use starlark::environment::{GlobalsBuilder, LibraryExtension};
    use starlark::starlark_module;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    static EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
    static SHARED_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
    static SINGLE_FLIGHT_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
    static CLAIM_RECHECK_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
    static FAILING_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
    /// Rendezvous for the parallel-evaluation test: two evaluations must
    /// both sit in `wait_for_pair` at once (a timeout marks a violation).
    static PAIR_RENDEZVOUS: std::sync::Mutex<Option<Arc<PairGate>>> = std::sync::Mutex::new(None);
    static CONCURRENT_PAIR_VIOLATION: AtomicBool = AtomicBool::new(false);

    /// A two-party gate: `enter` returns once both parties arrived (or
    /// after `deadline`, reporting the timeout).
    struct PairGate {
        arrived: AtomicUsize,
        generation: AtomicU64,
    }

    impl PairGate {
        fn enter(&self) -> bool {
            let generation = self.generation.load(Ordering::Acquire);
            let count = self.arrived.fetch_add(1, Ordering::AcqRel) + 1;
            if count == 2 {
                self.arrived.store(0, Ordering::Release);
                self.generation.store(generation + 1, Ordering::Release);
                return true;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while self.generation.load(Ordering::Acquire) == generation {
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::yield_now();
            }
            true
        }
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = crate::test_temp::root().join(format!(
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

        fn count_failing_evaluation() -> anyhow::Result<i32> {
            FAILING_EVALUATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(21)
        }

        fn count_single_flight_evaluation() -> anyhow::Result<i32> {
            SINGLE_FLIGHT_EVALUATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(21)
        }

        fn wait_for_pair() -> anyhow::Result<i32> {
            let gate = PAIR_RENDEZVOUS.lock().unwrap().clone();
            if let Some(gate) = gate.as_ref()
                && !gate.enter()
            {
                CONCURRENT_PAIR_VIOLATION.store(true, Ordering::Relaxed);
            }
            Ok(21)
        }

        fn count_shared_evaluation() -> anyhow::Result<i32> {
            SHARED_EVALUATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(21)
        }

        fn count_claim_recheck_evaluation() -> anyhow::Result<i32> {
            CLAIM_RECHECK_EVALUATIONS.fetch_add(1, Ordering::Relaxed);
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
        // The definition profile decodes every transitive source as Latin-1
        // (StarlarkUtil.java:43-67): 'é' is two chars, as in Bazel.
        let definition = temp.definition("//:defs.bzl", "value").unwrap();
        assert_eq!(definition.value.value().unpack_str(), Some("2"));
        assert_eq!(
            definition.sources["//:helper.bzl"].bytes,
            "def compute():\n    return str(len('é'))\n".as_bytes()
        );
        // The ordinary loader keeps its UTF-8 model.
        assert!(temp.load("load(':defs.bzl', 'value')\n").is_ok());
        // A non-ASCII load() label would name its Latin-1 decoding natively.
        temp.file(
            "defs.bzl",
            "load(':helper\u{e9}.bzl', 'compute')\nvalue = compute()\n",
        );
        let error = temp.definition("//:defs.bzl", "value").unwrap_err();
        assert!(
            error
                .contains("non-ASCII load() labels are unsupported in direct repository execution"),
            "{error}"
        );
        temp.file(
            "defs.bzl",
            "load(':helper.bzl', 'compute')\nvalue = compute()\n",
        );
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

    fn root_package() -> PackageId {
        PackageId {
            repository: String::new(),
            package: String::new(),
        }
    }

    fn function_reprs(loaded: &LoadedModules) -> Vec<String> {
        loaded
            .functions
            .iter()
            .map(|function| function.value().to_repr())
            .collect()
    }

    #[test]
    fn build_loads_share_each_module_per_repository_set_with_unchanged_results() {
        // The cache is shareable by future parallel package loading.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BzlCache>();
        assert_send_sync::<RepositorySet>();
        let temp = Temp::new();
        temp.file("helpers/BUILD", "");
        temp.file(
            "helpers/shared.bzl",
            "def _own(x):\n    return x\ndef helper(x):\n    return _own(x)\nvalue = count_shared_evaluation()\n",
        );
        temp.file(
            "left.bzl",
            "load('//helpers:shared.bzl', 'helper', 'value')\ndef left_fn():\n    return helper(value)\nleft = value\n",
        );
        temp.file(
            "right.bzl",
            "load('@//helpers:shared.bzl', 'value')\ndef right_fn():\n    return value\nright = value\n",
        );
        let globals = GlobalsBuilder::standard().with(test_globals).build();
        let first_source = "load(':left.bzl', 'left')\nload(':right.bzl', 'right')\nleft + right\n";
        let second_source =
            "load(':right.bzl', 'right')\nload(':left.bzl', 'left')\nright + left\n";
        let package = root_package();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        let fresh = |source: &str| {
            load_modules_with_functions_in(
                Arc::new(RepositorySet::new(temp.0.clone())),
                &package,
                &parse(source),
                &globals,
            )
            .unwrap()
        };
        SHARED_EVALUATIONS.store(0, Ordering::Relaxed);
        let first_prints = LoadPrints::default();
        let first = load_build_modules_in(
            repositories.clone(),
            &package,
            &parse(first_source),
            &globals,
            &first_prints,
        )
        .unwrap();
        let second_prints = LoadPrints::default();
        let second = load_build_modules_in(
            repositories.clone(),
            &package,
            &parse(second_source),
            &globals,
            &second_prints,
        )
        .unwrap();
        // One evaluation per label and repository set (BzlLoadValue).
        assert_eq!(SHARED_EVALUATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(repositories.build_bzl_cache().len(), 3);
        // Both packages see the same frozen values ...
        let left = |loaded: &LoadedModules| loaded.modules[":left.bzl"].get("left_fn").unwrap();
        assert!(left(&first).value().ptr_eq(left(&second).value()));
        assert_eq!(evaluate(second_source, &second.modules, &globals), 42);
        // ... and exactly the transitive function registry, in the order an
        // uncached per-package load visits the graph.
        assert_eq!(function_reprs(&first), function_reprs(&fresh(first_source)));
        assert_eq!(
            function_reprs(&second),
            function_reprs(&fresh(second_source))
        );
        assert_eq!(function_reprs(&second).len(), 4);
        // A new repository set (invocation) evaluates again.
        assert_eq!(SHARED_EVALUATIONS.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn shared_build_loads_keep_cycle_depth_and_failure_semantics() {
        let temp = Temp::new();
        for i in 0..MAX_LOAD_DEPTH {
            temp.file(
                &format!("m{i}.bzl"),
                &format!("load(':m{}.bzl', _x='x')\nx=_x\n", i + 1),
            );
        }
        temp.file(&format!("m{MAX_LOAD_DEPTH}.bzl"), "x = 1\n");
        temp.file(
            "bad.bzl",
            "load(':m255.bzl', 'x')\ny = count_failing_evaluation()\nprint('bad evaluates')\nfail('boom')\n",
        );
        temp.file("uses_bad.bzl", "load(':bad.bzl', 'x')\n");
        temp.file("unparsable.bzl", "x = (\n");
        temp.file("a.bzl", "load(':b.bzl', 'x')\n");
        temp.file("b.bzl", "load(':a.bzl', 'x')\n");
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
            .with(test_globals)
            .build();
        let package = root_package();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        let shared = |source: &str| {
            let prints = LoadPrints::default();
            let result = load_build_modules_in(
                repositories.clone(),
                &package,
                &parse(source),
                &globals,
                &prints,
            );
            (result, prints.take())
        };
        let fresh = |source: &str| {
            load_modules_with_functions_in(
                Arc::new(RepositorySet::new(temp.0.clone())),
                &package,
                &parse(source),
                &globals,
            )
            .unwrap_err()
        };
        // The cached chain below m1 is traversed again from m0: the depth
        // limit applies as when every module is evaluated per package.
        assert!(shared("load(':m1.bzl', 'x')").0.is_ok());
        let deep = shared("load(':m0.bzl', 'x')").0.unwrap_err();
        assert_eq!(deep, fresh("load(':m0.bzl', 'x')"));
        assert!(deep.contains("load depth limit 256 exceeded"));
        // Cycle errors name the traversal's chain: never cached.
        for root in ["a", "b", "a"] {
            let source = format!("load(':{root}.bzl', 'x')");
            assert_eq!(shared(&source).0.unwrap_err(), fresh(&source));
        }
        // A failing module is evaluated (its prints buffered with its
        // cached failure, review §1.4) once; its stored error is replayed
        // for every loading package and every dependent.
        FAILING_EVALUATIONS.store(0, Ordering::Relaxed);
        let (failed, failed_prints) = shared("load(':bad.bzl', 'x')");
        let failed = failed.unwrap_err();
        assert!(failed.contains("boom"), "{failed}");
        for source in ["load(':bad.bzl', 'x')", "load(':uses_bad.bzl', 'x')"] {
            assert_eq!(shared(source).0.unwrap_err(), failed);
        }
        assert_eq!(FAILING_EVALUATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(fresh("load(':uses_bad.bzl', 'x')"), failed);
        assert_eq!(FAILING_EVALUATIONS.load(Ordering::Relaxed), 2);
        let unparsable = shared("load(':unparsable.bzl', 'x')").0.unwrap_err();
        assert!(unparsable.contains("cannot parse"), "{unparsable}");
        assert_eq!(
            shared("load(':unparsable.bzl', 'x')").0.unwrap_err(),
            unparsable
        );
        let cache = repositories.build_bzl_cache().modules.lock().unwrap();
        let failure = |name: &str| cache[&Label::parse_canonical(name).unwrap()].clone().err();
        let failure_error = |name: &str| failure(name).map(|failure| failure.error.clone());
        assert_eq!(failure_error("//:bad.bzl"), Some(failed));
        assert_eq!(failure_error("//:unparsable.bzl"), Some(unparsable));
        // The buffered print events of the failing evaluation: the
        // earlier-loaded chain (m255, m256) leaves cache-hit markers in
        // DFS position and the failing module's own line last — the
        // coordinator replays exactly this, also on failure (P2
        // follow-up). The line is stored with the cached failure too.
        let failing_label = Label::parse_canonical("//:bad.bzl").unwrap();
        assert_eq!(
            failed_prints,
            vec![
                LoadPrintEvent::Cached(Label::parse_canonical("//:m256.bzl").unwrap()),
                LoadPrintEvent::Cached(Label::parse_canonical("//:m255.bzl").unwrap()),
                LoadPrintEvent::Module {
                    label: failing_label.clone(),
                    line: "bad evaluates".to_owned(),
                },
            ]
        );
        for uncached in ["//:uses_bad.bzl", "//:a.bzl", "//:b.bzl", "//:m0.bzl"] {
            assert!(!cache.contains_key(&Label::parse_canonical(uncached).unwrap()));
        }
        drop(cache);
        assert_eq!(
            repositories.build_bzl_cache().prints(&failing_label),
            vec!["bad evaluates".to_owned()]
        );
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

    /// Per-label single-flight `.bzl` evaluation (review §1.3, N62): two
    /// threads loading the same uncached label run exactly one evaluation
    /// — the loser waits for the in-flight claim and rechecks the cache —
    /// and independent labels evaluate CONCURRENTLY (no global lock).
    #[test]
    fn bzl_evaluation_is_single_flight_and_reentrant() {
        let temp = Temp::new();
        temp.file("BUILD", "");
        temp.file("shared.bzl", "value = count_single_flight_evaluation()\n");
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
            .with(test_globals)
            .build();
        let package = root_package();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        SINGLE_FLIGHT_EVALUATIONS.store(0, Ordering::Relaxed);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let loads: Vec<_> = (0..2)
            .map(|_| {
                let repositories = repositories.clone();
                let globals = globals.clone();
                let package = package.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let prints = LoadPrints::default();
                    let ast = parse("load(':shared.bzl', 'value')");
                    barrier.wait();
                    let loaded =
                        load_build_modules_in(repositories, &package, &ast, &globals, &prints)
                            .unwrap();
                    (loaded.modules[":shared.bzl"].clone(), prints.take())
                })
            })
            .collect();
        let results: Vec<_> = loads.into_iter().map(|load| load.join().unwrap()).collect();
        // Exactly one evaluation served both threads the same frozen
        // value; the loser's unit holds only a cache-hit marker for it
        // (its prints replay from the entry, never duplicated).
        assert_eq!(SINGLE_FLIGHT_EVALUATIONS.load(Ordering::Relaxed), 1);
        let value = |module: &FrozenModule| module.get("value").unwrap();
        assert!(
            value(&results[0].0)
                .value()
                .ptr_eq(value(&results[1].0).value())
        );
        let shared = Label::parse_canonical("//:shared.bzl").unwrap();
        let module_lines = results
            .iter()
            .flat_map(|(_, prints)| {
                prints
                    .iter()
                    .filter(|event| matches!(event, LoadPrintEvent::Module { .. }))
            })
            .count();
        let markers = results
            .iter()
            .flat_map(|(_, prints)| {
                prints.iter().filter(
                    |event| matches!(event, LoadPrintEvent::Cached(label) if *label == shared),
                )
            })
            .count();
        // No prints exist to buffer; the winner evaluated (no marker) and
        // the loser's unit holds exactly one cache-hit marker.
        assert_eq!((module_lines, markers), (0, 1));
    }

    /// Independent labels evaluate in parallel (N62): each module's
    /// evaluation blocks in a barrier inside Starlark — with the old
    /// global evaluation lock both waits time out (serialized); with
    /// per-label claims the evaluations rendezvous concurrently and both
    /// units load.
    #[test]
    fn independent_bzl_labels_evaluate_concurrently() {
        let temp = Temp::new();
        temp.file("BUILD", "");
        temp.file("left.bzl", "value = wait_for_pair()\n");
        temp.file("right.bzl", "value = wait_for_pair()\n");
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
            .with(test_globals)
            .build();
        let package = root_package();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        PAIR_RENDEZVOUS.lock().unwrap().replace(Arc::new(PairGate {
            arrived: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
        }));
        CONCURRENT_PAIR_VIOLATION.store(false, Ordering::Relaxed);
        let loads: Vec<_> = ["left.bzl", "right.bzl"]
            .into_iter()
            .map(|name| {
                let repositories = repositories.clone();
                let globals = globals.clone();
                let package = package.clone();
                let source = format!("load(':{name}', 'value')");
                std::thread::spawn(move || {
                    let prints = LoadPrints::default();
                    let ast = parse(&source);
                    load_build_modules_in(repositories, &package, &ast, &globals, &prints).unwrap();
                    prints.take()
                })
            })
            .collect();
        for load in loads {
            load.join().expect("concurrent evaluations complete");
        }
        assert!(
            !CONCURRENT_PAIR_VIOLATION.load(Ordering::Relaxed),
            "the two labels' evaluations must overlap"
        );
    }

    /// The wait-cycle safety net (N62): a claim never deadlocks. A
    /// same-thread re-claim and a wait that would close a cycle across
    /// threads abort with the [`crate::repository_demand::
    /// PARALLEL_BZL_CYCLE`] sentinel instead (the coordinator re-runs the
    /// unit inline).
    #[test]
    fn wait_cycle_abort_sentinel_prevents_cross_thread_deadlock() {
        // The registry is keyed by (cache instance, label), so this test
        // uses its own cache id: no other test's evaluation can contend
        // these keys (N62 follow-up — no unique-label workaround needed,
        // the ordinary names are safe here).
        let cache = BzlCache::default().id();
        let left = Label::parse_canonical("//:left.bzl").unwrap();
        let right = Label::parse_canonical("//:right.bzl").unwrap();
        let guard = match bzl_claim(cache, &left) {
            Ok(BzlClaim::Owned) => BzlClaimGuard {
                key: (cache, left.clone()),
            },
            Ok(BzlClaim::Waited) => panic!("uncontended claim must be owned"),
            Err(error) => panic!("uncontended claim must be owned: {error}"),
        };
        // A same-thread re-claim is a load cycle through the claim itself.
        let repeat = match bzl_claim(cache, &left) {
            Err(error) => error,
            Ok(_) => panic!("a same-thread re-claim aborts"),
        };
        assert!(crate::repository_demand::is_parallel_bzl_cycle(&repeat));
        // Another cache instance's claim of the same label is independent.
        let other_cache_guard = match bzl_claim(cache + 1, &left) {
            Ok(BzlClaim::Owned) => BzlClaimGuard {
                key: (cache + 1, left.clone()),
            },
            Ok(BzlClaim::Waited) => panic!("a different cache instance must not contend"),
            Err(error) => panic!("a different cache instance must not contend: {error}"),
        };
        drop(other_cache_guard);
        // Register this thread as waiting for `right` while it owns
        // `left` — the state a nested Finish frame would produce.
        {
            let mut state = bzl_flight()
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state
                .waiting_for
                .insert(std::thread::current().id(), (cache, right.clone()));
        }
        // The other thread holds `right` and asks for `left`: its
        // wait-cycle walk must observe the registration and abort.
        let registered = Arc::new(std::sync::Barrier::new(2));
        let signal = registered.clone();
        let other = std::thread::spawn(move || {
            let right_guard = match bzl_claim(cache, &right) {
                Ok(BzlClaim::Owned) => BzlClaimGuard {
                    key: (cache, right),
                },
                Ok(BzlClaim::Waited) => panic!("uncontended claim must be owned"),
                Err(error) => panic!("uncontended claim must be owned: {error}"),
            };
            signal.wait();
            let aborted = match bzl_claim(cache, &left) {
                Err(error) => error,
                Ok(_) => panic!("a wait closing a cycle across threads aborts"),
            };
            (aborted, right_guard)
        });
        registered.wait();
        let (aborted, _right_guard) = other.join().unwrap();
        assert!(crate::repository_demand::is_parallel_bzl_cycle(&aborted));
        {
            let mut state = bzl_flight()
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.waiting_for.remove(&std::thread::current().id());
        }
        drop(guard);
    }

    /// Post-claim recheck (N62 follow-up blocker): a publisher that
    /// inserts (and releases) between this thread's cache lookup and its
    /// claim must win — the claim winner rechecks and serves the cached
    /// entry instead of evaluating a second instance. The pre-claim hook
    /// forces exactly that interleaving: inside the window it loads the
    /// same label through a second loader (one real evaluation, one
    /// publication); the outer load must then observe exactly one
    /// evaluation and one cache-hit marker, never its own module lines.
    #[test]
    fn claim_winner_rechecks_cache_before_evaluating() {
        let temp = Temp::new();
        temp.file("BUILD", "");
        temp.file("counted.bzl", "value = count_claim_recheck_evaluation()\n");
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
            .with(test_globals)
            .build();
        let package = root_package();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        CLAIM_RECHECK_EVALUATIONS.store(0, Ordering::Relaxed);
        // The hook fires between the outer loader's lookup and claim and
        // publishes the label via an independent shared load, once.
        let fired = Arc::new(AtomicBool::new(false));
        let hook = {
            let repositories = repositories.clone();
            let globals = globals.clone();
            let package = package.clone();
            let fired = fired.clone();
            Arc::new(move |label: &Label| {
                if label.name != "counted.bzl" || fired.swap(true, Ordering::Relaxed) {
                    return;
                }
                let prints = LoadPrints::default();
                let ast = parse("load(':counted.bzl', 'value')");
                load_build_modules_in(repositories.clone(), &package, &ast, &globals, &prints)
                    .unwrap();
            }) as Arc<dyn Fn(&Label) + Send + Sync>
        };
        set_pre_claim_hook(Some(hook));
        let prints = LoadPrints::default();
        let ast = parse("load(':counted.bzl', 'value')");
        let result = load_build_modules_in(repositories, &package, &ast, &globals, &prints);
        set_pre_claim_hook(None);
        result.unwrap();
        assert!(fired.load(Ordering::Relaxed), "the hook must have run");
        assert_eq!(
            CLAIM_RECHECK_EVALUATIONS.load(Ordering::Relaxed),
            1,
            "exactly one evaluation across the forced interleaving"
        );
        // The outer unit holds exactly one cache-hit marker and no
        // evaluated-module lines: it served the published entry.
        let (module_lines, markers) =
            prints
                .take()
                .into_iter()
                .fold((0, 0), |(lines, markers), event| match event {
                    LoadPrintEvent::Module { .. } => (lines + 1, markers),
                    LoadPrintEvent::Cached(_) => (lines, markers + 1),
                    LoadPrintEvent::Build(_) => (lines, markers),
                });
        assert_eq!((module_lines, markers), (0, 1));
    }

    /// Demand-cycle error mapping (N62 follow-up): a shared definition
    /// load for a label this very thread is already evaluating (an
    /// extension loading the module being evaluated) aborts internally
    /// with the wait-cycle sentinel, but the sentinel never reaches the
    /// caller — it reports as the load cycle it is.
    #[test]
    fn reentrant_shared_definition_demand_reports_load_cycle_not_sentinel() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name = 'reentrant')\n");
        temp.file("BUILD", "");
        temp.file("self.bzl", "value = 1\n");
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
            .with(test_globals)
            .build();
        let repositories = Arc::new(RepositorySet::new(temp.0.clone()));
        let label = Label::parse_canonical("//:self.bzl").unwrap();
        // This thread claims the label, as its Finish frame would while
        // evaluating it; the nested demand must not deadlock or leak the
        // sentinel.
        let cache = repositories.build_bzl_cache().id();
        let _guard = match bzl_claim(cache, &label) {
            Ok(BzlClaim::Owned) => BzlClaimGuard {
                key: (cache, label.clone()),
            },
            other => panic!("uncontended claim must be owned: {other:?}"),
        };
        let prints = LoadPrints::default();
        let error = load_shared_definition_in(repositories, &label, "value", &globals, &prints)
            .expect_err("re-entrant demand must fail");
        assert!(error.contains("load cycle"), "{error}");
        assert!(error.contains("self.bzl"), "{error}");
        assert!(
            !error.contains(crate::repository_demand::PARALLEL_BZL_CYCLE),
            "the raw sentinel must not reach the user: {error:?}"
        );
    }

    /// W6 review F1: an aspect definition load is a BUILD-phase load over
    /// the shared cache whichever load reaches the module first. Evaluated
    /// by the aspect load, the module's lines are print events of its sink
    /// (never stderr directly) and its strings are in the analysis char
    /// model; served from the cache after a BUILD unit (a prefetch worker)
    /// published it, the sink holds the hit marker that replays the stored
    /// lines — and the value is the same.
    #[test]
    fn shared_definition_loads_are_build_phase_loads_whoever_evaluates_first() {
        let temp = Temp::new();
        temp.file("MODULE.bazel", "module(name = 'main')\n");
        temp.file("tools/BUILD", "");
        temp.file(
            "tools/lint.bzl",
            "NAME = '\u{e9}'\nprint('len %d' % len(NAME))\nlength = len(NAME)\n",
        );
        let globals = crate::package::bzl_globals();
        let label = Label::parse_canonical("//tools:lint.bzl").unwrap();
        let line = || LoadPrintEvent::Module {
            label: label.clone(),
            line: "DEBUG: //tools:lint.bzl:2:6: len 1".to_owned(),
        };

        // The aspect load evaluates the module.
        let first = Arc::new(RepositorySet::new(temp.0.clone()));
        let prints = LoadPrints::default();
        let loaded =
            load_shared_definition_in(first.clone(), &label, "length", &globals, &prints).unwrap();
        assert_eq!(loaded.value.value().unpack_i32(), Some(1));
        assert!(loaded.sources.is_empty() && loaded.repo_mappings.is_empty());
        assert_eq!(prints.take(), vec![line()]);
        assert_eq!(
            first.build_bzl_cache().prints(&label),
            vec!["DEBUG: //tools:lint.bzl:2:6: len 1"]
        );

        // A BUILD unit (a worker's) evaluates and publishes it first.
        let second = Arc::new(RepositorySet::new(temp.0.clone()));
        let unit = LoadPrints::default();
        load_build_modules_in(
            second.clone(),
            &root_package(),
            &parse("load('//tools:lint.bzl', 'length')\n"),
            &globals,
            &unit,
        )
        .unwrap();
        assert_eq!(unit.take(), vec![line()]);
        let prints = LoadPrints::default();
        let loaded =
            load_shared_definition_in(second.clone(), &label, "length", &globals, &prints).unwrap();
        assert_eq!(loaded.value.value().unpack_i32(), Some(1));
        assert_eq!(prints.take(), vec![LoadPrintEvent::Cached(label.clone())]);
        assert_eq!(
            second.build_bzl_cache().prints(&label),
            vec!["DEBUG: //tools:lint.bzl:2:6: len 1"]
        );
    }
}
