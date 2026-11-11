//! The loading nodes (S2, docs/EVALUATOR.md "Loading nodes"): `Pkg`,
//! `Tgt`, `Src` and `Group` over [`EvalShared`] instead of the engine's
//! coordinator services, plus the loading the phase-1 bodies do without a
//! node of its own (registration patterns, aspect definitions, module
//! execution platforms).
//!
//! - **Pkg**: a package the evaluation loads (one not loaded before
//!   phase 1, [`Loading::preloaded`]) is loaded by its node; what the
//!   sequential engine does at the commit is recorded in the node's
//!   transcript in its order — [`Event::LoadPrints`] (the unit's buffered
//!   print events, deduplicated against the emitted `.bzl` modules where
//!   the tape writes them) and [`Event::PkgCommit`] (the tape commits the
//!   package into the engine: `packages`, `generated`, the loaded
//!   functions, progress) — and replayed by pass 1 at the node's first
//!   linearized use ([`Event::MemoUse`] of the Pkg node by every lookup).
//!   The unit's roots and observations are committed when it settles.
//!   On the pool, the load runs under the no-demand guard; a unit that
//!   needs a repository (the inline-rerun sentinel) has it fetched ahead
//!   (repository speculation) and parks for the sequential pass, which
//!   loads it where demand may fetch (the walk's inline load).
//! - **Tgt**: a lookup (rule, output file, declared source file) over its
//!   package's value; a source file's missing-input and source-directory
//!   records go to the store (order-free maps phase 2 reads).
//! - **Src**: a source file's visibility check.
//! - **Group**: a resolved package group; its includes are Group nodes (an
//!   include cycle is a back edge).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::fast_hash::FxHashMap;
use crate::label::{Label, PackageId};
use crate::loading::LoadPrintEvent;
use crate::package::{Package, Rule};
use crate::repository_budget::HostBudget;

use super::super::super::{
    LoadStep, SourceDirProbe, SourceProbes, SourceSide, Target, check_macro_namespace,
    load_package_attempt_in, no_such_target_in, package_input_labels_in, settle_load_unit,
    source_target_in, validate_package_outputs,
};
use super::super::cx::Missing;
use super::super::key::NodeKey;
use super::super::shared::EvalShared;
use super::super::transcript::Event;
use super::super::value::NodeValue;
use super::bodies::Phase1Cx;

/// The evaluation's loading state (in [`EvalShared`]).
#[derive(Default)]
pub(crate) struct Loading {
    /// The packages loaded before phase 1 (committed already; their nodes
    /// record nothing).
    pub(crate) preloaded: FxHashMap<PackageId, Arc<Package>>,
    /// `Engine::source_dir_probes`, moved in for the evaluation.
    pub(crate) probes: Mutex<std::collections::HashMap<PathBuf, SourceDirProbe>>,
    /// `Engine::group_budget`, moved in for the evaluation.
    pub(crate) group_budget: Mutex<HostBudget>,
    /// Package loads run on pool threads (no repository demand: a unit
    /// that needs one asks the coordinator's lane).
    pub(crate) pooled: std::sync::atomic::AtomicBool,
    /// The package groups resolved so far (`Engine::group_nodes`).
    pub(crate) groups: Mutex<FxHashMap<Label, Arc<crate::package_specification::GroupNode>>>,
    /// The module repository root each Pkg node waits for on the lane.
    pub(crate) pending_blockers: Mutex<FxHashMap<PackageId, Vec<String>>>,
    /// What the lane wrote materializing a root, by blocker.
    #[allow(clippy::type_complexity)]
    pub(crate) lane_writes:
        Mutex<FxHashMap<String, (Vec<crate::ui::Deferred>, crate::observed_fs::Observations)>>,
    /// The module events of a Pkg node's attempts aborted on the pool.
    pub(crate) aborted_prints: Mutex<FxHashMap<PackageId, Vec<LoadPrintEvent>>>,
    /// The print events of the aspect definitions the evaluation loaded
    /// (every use writes them where the walk's load would, deduplicated).
    pub(crate) aspect_prints: Mutex<FxHashMap<String, Arc<[LoadPrintEvent]>>>,
    /// The module execution platforms (`Engine::module_execution_platforms`).
    #[allow(clippy::type_complexity)]
    pub(crate) module_execution_platforms: Mutex<Option<Result<Vec<(String, String)>, String>>>,
    /// The source files resolved to a path the engine memoizes
    /// (`Engine::source_targets`; S3: published for phase 2's prepares
    /// off the coordinator).
    pub(crate) stable_sources: Mutex<Vec<(Label, PathBuf)>>,
}

thread_local! {
    /// This thread runs the sequential pass while the pool serves
    /// (docs/EVALUATOR.md "Phase 1, the concurrent sequential pass"): its
    /// attempts are the sequential pass's, whatever [`Loading::pooled`]
    /// says.
    static SEQUENTIAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark this thread as the sequential pass's (or not) while the pool
/// serves; the previous mark.
pub(crate) fn set_sequential(sequential: bool) -> bool {
    SEQUENTIAL.with(|cell| cell.replace(sequential))
}

impl Loading {
    /// Whether the pool serves beside this thread's sequential pass.
    pub(crate) fn serving(&self) -> bool {
        self.pooled.load(std::sync::atomic::Ordering::Acquire)
            && SEQUENTIAL.with(std::cell::Cell::get)
    }

    /// Whether attempts on this thread run as the pool's now: the pool
    /// runs, and this thread is not the sequential pass's.
    pub(crate) fn pooled_here(&self) -> bool {
        self.pooled.load(std::sync::atomic::Ordering::Acquire)
            && !SEQUENTIAL.with(std::cell::Cell::get)
    }
}

/// A loaded package's value: the package and its lookup indexes.
pub(crate) struct PkgValue {
    pub(crate) package: Arc<Package>,
    /// The output files of its rules (`Engine::generated`).
    outputs: FxHashMap<Label, (Arc<Rule>, String)>,
    /// The labels its rules name as inputs (`Engine::declared_inputs`).
    declared: OnceLock<HashSet<Label>>,
    /// The extension groups the pool load that produced this value used
    /// speculatively (`EVAL_GROUP_KEY` keys; empty for a sequential load):
    /// the only ones a lookup in it may use ([`target_body`]).
    pub(crate) adopted_groups: Vec<String>,
}

impl PkgValue {
    pub(crate) fn new(package: Arc<Package>) -> Self {
        let mut outputs = FxHashMap::default();
        for rule in package.values() {
            for out in &rule.outs {
                if let Ok(output) = rule.label.with_name(out) {
                    outputs.insert(output, (Arc::clone(rule), out.clone()));
                }
            }
        }
        Self {
            package,
            outputs,
            declared: OnceLock::new(),
            adopted_groups: Vec::new(),
        }
    }

    /// The rule `label` names in this package (a rule, or an output
    /// file's generating rule).
    pub(crate) fn rule_of(&self, label: &Label) -> Option<Arc<Rule>> {
        self.package
            .get(label)
            .or_else(|| self.outputs.get(label).map(|(rule, _)| rule))
            .cloned()
    }

    /// `Engine::declared_source`.
    fn declared_source(
        &self,
        repositories: &crate::repository::RepositorySet,
        label: &Label,
    ) -> bool {
        if self.package.exports.contains_key(label) {
            return true;
        }
        if self.package.macro_names.contains(&label.name) {
            return false;
        }
        self.declared
            .get_or_init(|| package_input_labels_in(repositories, &self.package))
            .contains(label)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The demand of an inline-rerun sentinel when it is a module
/// repository's root (`repository @@NAME is unfetched`): what the
/// coordinator's lane materializes.
pub(crate) fn module_root_blocker(error: &str) -> Option<&str> {
    let start = error.find(crate::repository_demand::NEEDS_REPOSITORY)?;
    let rest =
        error[start + crate::repository_demand::NEEDS_REPOSITORY.len()..].strip_prefix(": ")?;
    let (subject, _) = rest.split_once(" is unfetched")?;
    subject.strip_prefix("repository @@")?;
    Some(
        &error[start..start + crate::repository_demand::NEEDS_REPOSITORY.len() + 2 + subject.len()],
    )
}

/// The lane's blocker of a pool load waiting for an extension group's
/// speculative preparation (crate::repository's `EvalAdoption`).
const EXTENSION_BLOCKER: &str = "\u{2}extension:";

/// The lane's blocker of a pool load waiting for a local repository rule's
/// speculative execution (crate::repository's `eval_local_blocker`).
const LOCAL_BLOCKER: &str = "\u{2}local:";

/// How long the lane waits for one blocker's speculative preparation (10
/// s; `BAZEL_RS_EVAL_TEST_LANE_WAIT_SECS`, a hidden test hook: a loaded
/// host's speculation may take longer, and a test counting what the lane
/// executed ahead must not depend on it).
fn lane_wait_secs() -> u64 {
    static SECS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a hidden test hook bounding a wait; the output is the same either way.
        std::env::var("BAZEL_RS_EVAL_TEST_LANE_WAIT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10)
    });
    *SECS
}

/// The coordinator's lane: materialize the module repository root a
/// pool unit was blocked on (where demand may fetch), its console writes
/// kept for the unit's re-run.
pub(crate) fn lane_fetch(shared: &EvalShared, blocker: &str) {
    // At most this long per blocker (the unit parks again past it).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(lane_wait_secs());
    if let Some(group) = blocker.strip_prefix(EXTENSION_BLOCKER) {
        // Blocked on a registry module's root: materialized here (what it
        // writes and observes is the waiting unit's), then once more.
        for _ in 0..4 {
            let Some(error) = shared.repositories.wait_eval_group(group, deadline) else {
                break;
            };
            let Some(root) = module_root_blocker(&error) else {
                break;
            };
            lane_root(shared, root, blocker);
            shared.repositories.retry_speculations();
        }
        return;
    }
    if let Some(canonical) = blocker.strip_prefix(LOCAL_BLOCKER) {
        // Blocked on a registry module's root: materialized here (what it
        // writes and observes is the waiting unit's), then once more.
        for _ in 0..4 {
            let Some(error) = shared.repositories.wait_eval_local(canonical, deadline) else {
                break;
            };
            let Some(root) = module_root_blocker(&error) else {
                break;
            };
            lane_root(shared, root, blocker);
            shared.repositories.retry_speculations();
        }
        return;
    }
    let Some(name) = blocker.split_once("repository @@").map(|(_, name)| name) else {
        return;
    };
    // What it writes and observes is the re-run unit's (where the walk's
    // inline load materializes the root).
    let console = crate::ui::DeferredConsole::start();
    let scope = crate::observed_fs::unit_scope();
    let _ = shared.repositories.root(name);
    let observations = scope.finish();
    let writes = console.finish();
    if !writes.is_empty() || !observations.is_empty() {
        lock(&shared.loading.lane_writes).insert(blocker.to_owned(), (writes, observations));
    }
}

/// The lane materializes the registry module root `root` (its
/// [`module_root_blocker`]) a speculation the pool waits for needs: what it
/// writes and observes is the waiting unit's (`blocker`'s), as
/// [`lane_fetch`]'s.
fn lane_root(shared: &EvalShared, root: &str, blocker: &str) {
    let root = root.to_owned();
    lane_fetch(shared, &root);
    if let Some((writes, observations)) = lock(&shared.loading.lane_writes).remove(&root) {
        let mut lane = lock(&shared.loading.lane_writes);
        let kept = lane.entry(blocker.to_owned()).or_default();
        kept.0.extend(writes);
        kept.1.append(observations);
    }
}

/// BAZEL_RS_EVAL_STATS: the pool's loads left to the sequential pass.
pub(crate) static POOL_INLINE_LOADS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The `Pkg` body.
pub(crate) fn package_body(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    id: &PackageId,
) -> Result<NodeValue, Missing> {
    // A label whose apparent repository the main repository does not see
    // names no loadable package (Engine::load_package).
    if let Some(message) = crate::label::unresolvable_package_message(id) {
        return Ok(NodeValue::Pkg(Err(message)));
    }
    if let Some(package) = shared.loading.preloaded.get(id) {
        return Ok(NodeValue::Pkg(Ok(Arc::new(PkgValue::new(Arc::clone(
            package,
        ))))));
    }
    let repositories = &shared.repositories;
    let pooled = shared.loading.pooled_here();
    let unit = match () {
        () if pooled => {
            let guard = crate::repository_demand::no_demand();
            let reuse = crate::repository_demand::reuse_only();
            let committed = crate::repository_demand::committed_unit();
            // The finished speculative preparations of extension groups
            // may serve it (committed where the tape replays the node).
            let adoption = crate::repository::EvalAdoption::start();
            let unit = crate::package::evaluate_load_unit(Arc::clone(repositories), id);
            // Its outputs' checks too (their repository's root and ignored
            // subdirectories), under the same guards.
            let validated = unit
                .result
                .as_ref()
                .ok()
                .map(|package| validate_package_outputs(repositories, id, package));
            let groups = adoption.finish();
            drop(committed);
            drop(reuse);
            drop(guard);
            let sentinel = unit
                .result
                .as_ref()
                .err()
                .or_else(|| {
                    validated
                        .as_ref()
                        .and_then(|validated| validated.as_ref().err())
                })
                .filter(|error| crate::repository_demand::is_inline_rerun_sentinel(error));
            if sentinel.is_some() {
                // What blocked it is fetched ahead. The aborted attempt's
                // module events print first (kept for the re-run).
                if let Some(error) = sentinel {
                    repositories.fetch_blocked_ahead(error);
                }
                POOL_INLINE_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
                if std::env::var_os("BAZEL_RS_EVAL_PARKED_DEBUG").is_some() {
                    eprintln!("bazel-rs: pool load of {id:?} left: {:?}", sentinel);
                }

                let module_events = unit
                    .prints
                    .into_iter()
                    .filter(|event| !matches!(event, LoadPrintEvent::Build(_)));
                shared
                    .loading
                    .aborted_prints
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(id.clone())
                    .or_default()
                    .extend(module_events);
                // A module repository's root: the coordinator's lane
                // materializes it and the unit loads on the pool again.
                // An extension group being prepared speculatively: the
                // lane waits for it and the unit loads on the pool again.
                if let Some(group) =
                    sentinel.and_then(|error| repositories.eval_group_blocker(error))
                {
                    let blocker = format!("{EXTENSION_BLOCKER}{group}");
                    lock(&shared.loading.pending_blockers)
                        .entry(id.clone())
                        .or_default()
                        .push(blocker.clone());
                    return Err(Missing::Repo(blocker));
                }
                // A local repository rule: the lane has it executed ahead.
                if let Some(canonical) =
                    sentinel.and_then(|error| repositories.eval_local_blocker(error))
                {
                    let blocker = format!("{LOCAL_BLOCKER}{canonical}");
                    lock(&shared.loading.pending_blockers)
                        .entry(id.clone())
                        .or_default()
                        .push(blocker.clone());
                    return Err(Missing::Repo(blocker));
                }
                if let Some(blocker) = sentinel.and_then(|error| module_root_blocker(error)) {
                    lock(&shared.loading.pending_blockers)
                        .entry(id.clone())
                        .or_default()
                        .push(blocker.to_owned());
                    return Err(Missing::Repo(blocker.to_owned()));
                }
                super::super::timeline::note_inline(cx.current().0, || {
                    let error = sentinel.map(String::as_str).unwrap_or_default();
                    format!(
                        "a package load's sentinel ({})",
                        error.chars().take(160).collect::<String>()
                    )
                });
                return Err(Missing::Repo(super::bodies::INLINE.to_owned()));
            }
            // The roots it obtained before the sequential engine had them
            // count from its commit on (W6 review F2): where the tape
            // replays the node.
            let mut unit = unit;
            // The groups it used first: their commit precedes their
            // repositories' (`RepositorySet::note_committed_roots`).
            let adopted = groups.clone();
            let mut roots = groups;
            roots.append(&mut unit.roots);
            if !roots.is_empty() {
                let current = cx.current();
                cx.defer(Box::new(move |coord| {
                    coord.store.roots.insert(current, roots);
                }));
            }
            (unit, validated, adopted)
        }
        () => {
            let unit = load_package_attempt_in(repositories, id);
            // An inline load may have prepared what the speculations were
            // blocked on (Engine::load_package's retry).
            repositories.retry_speculations();
            (unit, None, Vec::new())
        }
    };
    let (unit, validated, adopted) = unit;
    // The module events of attempts aborted on the pool print first (as
    // an aborted unit's before its inline re-run).
    let aborted = shared
        .loading
        .aborted_prints
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(id)
        .unwrap_or_default();
    if !aborted.is_empty() {
        cx.emit(Event::LoadPrints(aborted.into()));
    }
    // What the lane's materialization of a root it was blocked on wrote,
    // where the walk's inline load would have written it.
    let blockers = lock(&shared.loading.pending_blockers)
        .remove(id)
        .unwrap_or_default();
    for (writes, observations) in blockers
        .iter()
        .filter_map(|blocker| lock(&shared.loading.lane_writes).remove(blocker))
    {
        cx.coord()
            .store
            .defer_package_observations(id, observations);
        for write in writes {
            let index = cx.coord().store.console(write);
            cx.emit(Event::Console(index));
        }
    }
    let (steps, result) = settle_load_unit(repositories, id, unit, || {
        load_package_attempt_in(repositories, id)
    });
    for step in steps {
        match step {
            LoadStep::Prints(prints) => {
                if !prints.is_empty() {
                    cx.emit(Event::LoadPrints(prints.into()));
                }
            }
            LoadStep::Committed(printed) => cx.emit(Event::PkgCommit(id.clone(), printed)),
        }
    }
    let value = result.and_then(|package| {
        match validated {
            Some(validated) => validated?,
            None => validate_package_outputs(repositories, id, &package)?,
        }
        let mut value = PkgValue::new(Arc::new(package));
        value.adopted_groups = adopted;
        Ok(Arc::new(value))
    });
    Ok(NodeValue::Pkg(value))
}

/// The value of a needed `Pkg` node.
pub(crate) fn pkg_of(value: &NodeValue) -> Result<Arc<PkgValue>, String> {
    match value {
        NodeValue::Pkg(result) => result.clone(),
        _ => Err("internal error: a Pkg key with another value".to_owned()),
    }
}

/// The shared probe cache, locked per access (no lock across a probe).
struct SharedProbes<'a>(&'a Mutex<std::collections::HashMap<PathBuf, SourceDirProbe>>);

impl SourceProbes for SharedProbes<'_> {
    fn get(&self, path: &Path) -> Option<SourceDirProbe> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
    }

    fn put(&mut self, path: PathBuf, probe: SourceDirProbe) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(path)
            .or_insert(probe);
    }
}

/// The `Tgt` body (`Engine::target` after the package is loaded).
pub(crate) fn target_body(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    label: &Label,
) -> Result<NodeValue, Missing> {
    let package_id = label.package_id();
    let needed = cx.need_one(NodeKey::Pkg(package_id.clone()))?;
    let pkg = match pkg_of(needed.as_ref()) {
        Ok(pkg) => pkg,
        Err(error) => return Ok(NodeValue::Tgt(Err(error))),
    };
    if let Some(rule) = pkg.package.get(label) {
        return Ok(NodeValue::Tgt(Ok(Target::Rule(Arc::clone(rule)))));
    }
    if let Some((rule, output)) = pkg.outputs.get(label) {
        return Ok(NodeValue::Tgt(Ok(Target::Output(
            Arc::clone(rule),
            output.clone(),
        ))));
    }
    // A lookup on the pool in a package whose load used extension groups
    // speculatively may use those (where the walk's lookup finds them
    // prepared by that load), not others.
    let adoption = (shared.loading.pooled_here() && !pkg.adopted_groups.is_empty())
        .then(|| crate::repository::EvalAdoption::start_limited(&pkg.adopted_groups));
    let result = source_lookup(shared, cx, label, &pkg);
    if let Some(adoption) = adoption {
        let used = adoption.finish();
        debug_assert!(
            used.iter().all(|group| pkg.adopted_groups.contains(group)),
            "internal error: a lookup in {package_id:?} used {used:?}, its load {:?}",
            pkg.adopted_groups
        );
        // No commit of its own: a lookup has no replay point (its events
        // are its user's; `store.roots` of a `Tgt` node would never be
        // committed). The groups are committed and checked by the load that
        // produced `pkg` (they are among its roots), whose `MemoUse` the
        // user replays before it reads the target.
        drop(used);
    }
    result
}

/// [`target_body`] past the package's own targets: a source file.
fn source_lookup(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    label: &Label,
    pkg: &PkgValue,
) -> Result<NodeValue, Missing> {
    let repositories = &shared.repositories;
    if !pkg.declared_source(repositories, label) {
        let error = match no_such_target_in(repositories, Some(&pkg.package), label) {
            Ok(message) | Err(message) => message,
        };
        return Ok(NodeValue::Tgt(Err(error)));
    }
    let mut side = SourceSide::default();
    let found = source_target_in(
        repositories,
        label,
        &mut SharedProbes(&shared.loading.probes),
        &mut side,
    );
    if !side.missing.is_empty() || !side.directories.is_empty() {
        cx.defer(Box::new(move |coord| coord.store.sources_side.push(side)));
    }
    if let Ok((path, true)) = &found {
        shared
            .loading
            .stable_sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((label.clone(), path.clone()));
    }
    Ok(NodeValue::Tgt(found.map(|(path, _)| Target::Source(path))))
}

/// A source file's visibility (`Engine::source_visibility`) in its loaded
/// package.
pub(crate) fn source_visibility(pkg: &PkgValue, label: &Label) -> crate::visibility::Visibility {
    pkg.package
        .exports
        .get(label)
        .unwrap_or(&pkg.package.default_visibility)
        .clone()
}

/// The `Group` body (`Engine::resolve_group`): the include graph is
/// resolved in the body as the walk resolves it (its active path decides
/// a cycle and the depth limit), resolved groups shared through
/// [`Loading::groups`].
pub(crate) fn group_body(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    label: &Label,
) -> Result<NodeValue, Missing> {
    Ok(NodeValue::Group(resolve_group(
        shared,
        cx,
        label,
        &mut Vec::new(),
    )?))
}

type GroupResult = Result<Arc<crate::package_specification::GroupNode>, String>;

fn resolve_group(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    label: &Label,
    active: &mut Vec<Label>,
) -> Result<GroupResult, Missing> {
    use crate::package::RuleKind;
    cx.emit(Event::ScopeEnter("resolve_group"));
    let result = (|| {
        let package = label.package_id();
        let pkg = match pkg_lookup(shared, cx, &package)? {
            Ok(pkg) => pkg,
            Err(error) => return Ok(Err(error)),
        };
        let Some(rule) = pkg.package.get(label) else {
            // Preserve existing missing/source/output diagnostics, but never
            // analyze a callback or fabricate a source from an include edge.
            if let Err(error) = target_lookup(shared, cx, label)? {
                return Ok(Err(error));
            }
            return Ok(Err(format!("{label} does not refer to a package group")));
        };
        let RuleKind::PackageGroup(definition) = &rule.kind else {
            return Ok(Err(format!("{label} does not refer to a package group")));
        };
        if let Err(error) = check_macro_namespace(rule, label) {
            return Ok(Err(error));
        }
        if let Some(node) = shared
            .loading
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
        {
            return Ok(Ok(Arc::clone(node)));
        }
        if active.contains(label) {
            return Ok(Err(format!("package group include cycle at {label}")));
        }
        if active.len() >= 256 {
            return Ok(Err(
                "package group DAG exceeds depth limit 256 path nodes".into()
            ));
        }
        let charged = (|| {
            let mut budget = shared
                .loading
                .group_budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            budget.node()?;
            definition.own_specs.charge(&mut budget)?;
            for part in [&label.repository, &label.package, &label.name] {
                budget.string(part)?;
            }
            for edge in &definition.includes {
                budget.node()?;
                for part in [&edge.repository, &edge.package, &edge.name] {
                    budget.string(part)?;
                }
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = charged {
            return Ok(Err(error));
        }
        let definition = Arc::clone(definition);
        active.push(label.clone());
        let resolved = (|| {
            let mut includes = Vec::with_capacity(definition.includes.len());
            for edge in &definition.includes {
                match resolve_group(shared, cx, edge, active)? {
                    Ok(node) => includes.push(node),
                    Err(error) => return Ok(Err(error)),
                }
            }
            let node = crate::package_specification::GroupNode::new(
                Arc::new(definition.own_specs.clone()),
                includes,
            )
            .map(Arc::new);
            if let Ok(node) = &node {
                shared
                    .loading
                    .groups
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(label.clone())
                    .or_insert_with(|| Arc::clone(node));
            }
            Ok(node)
        })();
        active.pop();
        resolved
    })();
    cx.emit(Event::ScopeExit);
    result
}

/// A package lookup from a body: `MemoUse` of its Pkg node (its commit's
/// effects at the first linearized use), then its value.
pub(crate) fn pkg_lookup(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    package: &PackageId,
) -> Result<Result<Arc<PkgValue>, String>, Missing> {
    let key = NodeKey::Pkg(package.clone());
    if !shared.loading.preloaded.contains_key(package) {
        let id = cx.id_of(&key);
        cx.emit(Event::MemoUse(id));
    }
    let needed = cx.need_one(key)?;
    Ok(pkg_of(needed.as_ref()))
}

/// A target lookup from a body (`Engine::target`): its `Access` (while
/// tracking), its package's `MemoUse`, then the `Tgt` value.
pub(crate) fn target_lookup(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    label: &Label,
) -> Result<Result<Target, String>, Missing> {
    if cx.flags().tracking {
        cx.emit(Event::Access(label.clone()));
    }
    let package = label.package_id();
    if !shared.loading.preloaded.contains_key(&package) {
        let id = cx.id_of(&NodeKey::Pkg(package));
        cx.emit(Event::MemoUse(id));
    }
    let value = cx.need_one(NodeKey::Tgt(label.clone()))?;
    Ok(match value.as_ref() {
        NodeValue::Tgt(result) => result.clone(),
        _ => Err("internal error: a Tgt key with another value".to_owned()),
    })
}

/// `Engine::module_execution_platforms` over the shared view: computed
/// once per build (an incomplete list is not kept).
pub(crate) fn module_execution_platforms(
    shared: &EvalShared,
    resolve: bool,
) -> Result<Vec<(String, String)>, String> {
    let cache = &shared.loading.module_execution_platforms;
    if let Some(found) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        return found.clone();
    }
    let flatten = |registrations: Vec<(String, Vec<String>)>| -> Vec<(String, String)> {
        registrations
            .into_iter()
            .flat_map(|(repository, patterns)| {
                patterns
                    .into_iter()
                    .map(move |pattern| (repository.clone(), pattern))
            })
            .collect()
    };
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match shared
        .repositories
        .execution_platform_registrations(resolve)
    {
        Ok((registrations, complete)) => {
            let found = flatten(registrations);
            if complete {
                *cache = Some(Ok(found.clone()));
            }
            Ok(found)
        }
        Err(error) => {
            *cache = Some(Err(error.clone()));
            Err(error)
        }
    }
}

/// `Engine::aspect_definition` over the shared registry: the definition
/// and the print events of its load, which every use writes where the
/// walk's load would (`None`: a definition loaded before phase 1).
#[allow(clippy::type_complexity)]
pub(crate) fn aspect_definition(
    shared: &EvalShared,
    identity: &str,
    allow_private: bool,
) -> (
    Result<Option<Arc<crate::aspects::AspectDefinition>>, String>,
    Option<Arc<[LoadPrintEvent]>>,
) {
    let prints_of = |identity: &str| {
        shared
            .loading
            .aspect_prints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(identity)
            .cloned()
    };
    if let Some(definition) = shared
        .aspects
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(identity)
    {
        return (Ok(Some(definition)), prints_of(identity));
    }
    let (loaded, prints) =
        super::super::super::load_aspect_definition(&shared.repositories, identity, allow_private);
    let prints: Arc<[LoadPrintEvent]> = prints.into();
    let printed = (!prints.is_empty()).then(|| Arc::clone(&prints));
    match loaded {
        Ok(Some(definition)) => {
            let mut registry = shared
                .aspects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry.get(identity).is_none() {
                registry.insert(definition);
                if let Some(prints) = &printed {
                    shared
                        .loading
                        .aspect_prints
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(identity.to_owned(), Arc::clone(prints));
                }
            }
            (Ok(registry.get(identity)), printed)
        }
        other => (other.map(|_| None), printed),
    }
}
