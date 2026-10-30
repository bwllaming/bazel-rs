//! Lazy repository roots and strict apparent-name resolution.
//!
//! Main-repository source operations never evaluate MODULE.bazel. External
//! resolution records the graph once; generated import aliases are metadata-only.
//! Ordinary groups prepare on generated-root/mapping demand, while repository
//! rules execute only on root demand. Local source roots remain
//! live inputs; archive sources are materialized only when requested. Git sources
//! are materialized for discovery and revalidated before repository use.
use crate::label::{Label, PackageId, validate_repository_name};
use crate::module_graph::{self, ModuleSource, ResolvedGraph, ResolverOptions};
use crate::patch_inputs::LocalPatchPlan;
use crate::registry::Registry;
use crate::repository_catalog::{self, DirectCatalog};
use crate::repository_demand::DemandSlot;
use crate::{custom_rule::AttrValue, loading, repository_rule};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// [`RepositorySet::shared_mapping`]'s copy of a mapping table entry.
type SharedMapping = Arc<BTreeMap<String, String>>;

/// A canonical root list of [`RepositorySet::link_policy`] and the roots
/// (name, path) it was made of.
#[derive(Debug)]
struct LinkOthers {
    roots: Vec<(String, PathBuf)>,
    others: Arc<Vec<(String, PathBuf)>>,
}

/// Byte-equal root lists (stricter than component-wise path equality).
fn same_roots(known: &[(String, PathBuf)], roots: &[(String, PathBuf)]) -> bool {
    known.len() == roots.len()
        && known
            .iter()
            .zip(roots)
            .all(|((a, p), (b, q))| a == b && p.as_os_str() == q.as_os_str())
}

/// The canonical form of a root, the repository store or a blocked prefix
/// as the link policy and the recorded-input keys take it (W6R): not
/// recorded where taken — [`RepositorySet::record_link_anchors`] records
/// them once at the end of planning — unless the `canon` hook is off (a
/// test's kill switch).
pub(crate) fn link_canonical_form(path: &Path) -> std::io::Result<PathBuf> {
    if crate::observed_fs::hook(crate::observed_fs::Hook::Canon) {
        // snapshot-unrecorded: recorded once at the end of planning for every counting root, the store and the blocked prefixes (RepositorySet::record_link_anchors, W6R)
        std::fs::canonicalize(path)
    } else {
        crate::observed_fs::canonicalize(path)
    }
}

/// Demanding `@@bazel_tools` without the built-in profile. Its mapping entry
/// exists in every module, like Bazel; only its contents are absent.
pub(crate) const BAZEL_TOOLS_UNAVAILABLE: &str = "the built-in repository @@bazel_tools is visible to every module but has no contents in the explicit-module profile; drop --experimental-builtin-modules=false";

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The direct and extension-generated repositories whose roots the
    /// load unit evaluating on this no-demand thread (a loading worker)
    /// obtained before the sequential engine had them ([`UnitRoots`]).
    static UNIT_ROOTS: std::cell::RefCell<Option<BTreeSet<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Records, while it lives, the direct and extension-generated
/// repositories whose roots a load unit obtains on a worker before the
/// sequential engine has them — published by the worker's own warm-reuse
/// probe or by another worker's, demanded by the package or by a `.bzl`
/// module its loads reach, also at a hit of a memo whose miss demands
/// them (the shared `.bzl` cache, the ignored subdirectories:
/// [`RepositorySet::note_obtained`]; W6 review F2 and its follow-ups).
/// They count as materialized for order-dependent decisions only once
/// the coordinator commits the unit
/// ([`RepositorySet::note_committed_roots`]): the sequential engine
/// demands them at that load. A warm-reuse probe records in a nested
/// scope ([`Self::during`]): what it obtained is stored with the root it
/// publishes and counts wherever that root does.
pub(crate) struct UnitRoots {
    previous: Option<Option<BTreeSet<String>>>,
}

impl UnitRoots {
    pub(crate) fn record() -> Self {
        let previous = UNIT_ROOTS.with(|roots| roots.replace(Some(BTreeSet::new())));
        Self {
            previous: Some(previous),
        }
    }

    /// Run `probe` in its own scope: its result and what it obtained on
    /// this thread — the roots it obtained (a warm-reuse probe's
    /// recorded-input check demands the roots its inputs name), which the
    /// enclosing scope does not see, and what it observed for the
    /// analysis snapshot, in a detached capture (W6R): both are stored
    /// with the root it publishes and count where that root does
    /// ([`DirectRuntime::mark_sequential`]).
    fn during<R>(probe: impl FnOnce() -> R) -> (R, crate::repository_demand::Obtained) {
        let scope = Self::record();
        let (result, observations) = crate::observed_fs::capture_detached(probe);
        (
            result,
            crate::repository_demand::Obtained {
                keys: scope.finish(),
                observations,
            },
        )
    }

    /// The recorded repositories, in name order.
    pub(crate) fn finish(mut self) -> Vec<String> {
        let previous = self.previous.take().flatten();
        UNIT_ROOTS
            .with(|roots| roots.replace(previous))
            .map(|roots| roots.into_iter().collect())
            .unwrap_or_default()
    }

    /// Note `canonical`'s root, just obtained on this thread, unless the
    /// sequential engine already has it.
    fn note<T>(canonical: &str, slot: &DemandSlot<T>) {
        if !crate::repository_demand::no_demand_active() || slot.sequential().is_some() {
            return;
        }
        UNIT_ROOTS.with(|roots| {
            if let Some(roots) = roots.borrow_mut().as_mut()
                && !roots.contains(canonical)
            {
                roots.insert(canonical.to_owned());
            }
        });
    }
}

impl Drop for UnitRoots {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            UNIT_ROOTS.with(|roots| *roots.borrow_mut() = previous);
        }
    }
}

/// [`RepositorySet::root_extension_fixups`]: (extension, buildozer commands)
/// per fixup, and the failures.
pub type RootFixups = (Vec<(String, Vec<String>)>, Vec<String>);

/// RepoDefinitionValue (bazel/repository/RepoDefinitionValue.java): a
/// definition, or a repository overridden by a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoDefinitionValue {
    Found(RepositoryDefinition),
    Override(String),
}

/// PathFragment.create(path).toString(): `.` and `..` segments and repeated
/// separators normalized away.
fn normalize_path_fragment(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            ".." if absolute => {}
            segment => segments.push(segment),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        String::new()
    } else {
        joined
    }
}

/// A repository rule invocation (RepoDefinition, bazel/repository/RepoDefinition.java)
/// as `mod show_repo` prints it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryDefinition {
    /// The defining `.bzl` file's unambiguous canonical label.
    pub rule_bzl: String,
    pub rule_name: String,
    /// The canonical repository name.
    pub name: String,
    pub original_name: Option<String>,
    /// The explicitly specified, type-checked attributes in call order.
    pub attributes: Vec<(String, AttrValue)>,
}

#[derive(Clone, Debug)]
pub struct RepositorySet {
    state: Arc<RepositoryState>,
}

#[path = "repository_current.rs"]
mod current;
pub use current::CurrentRefresh;

/// Ephemeral, point-in-time current-intent proof. There is deliberately no
/// deserializer, public constructor, adoption method or CARRIED conversion.
/// Historical graph switching still requires guarded whole-candidate acceptance.
#[derive(Debug)]
pub struct ProvedPublication {
    receipt: crate::repository_publication::Receipt,
}

impl ProvedPublication {
    pub fn receipt(&self) -> &crate::repository_publication::Receipt {
        &self.receipt
    }
}

/// Minted only by current, isolated group preparation, never from a receipt,
/// a persisted recipe, a pre-existing root or an invocation's prepared memo.
struct CurrentSpec {
    origin: crate::repository_publication::Origin,
    key: String,
}

#[derive(Debug)]
struct RepositoryState {
    runtime_id: u64,
    root: PathBuf,
    options: ResolverOptions,
    repository_downloads_enabled: bool,
    source_cache: PathBuf,
    absolute_source_cache: OnceLock<Result<PathBuf, String>>,
    root_module: OnceLock<Result<module_graph::RootModuleSnapshot, String>>,
    resolved: OnceLock<Result<ResolvedGraph, String>>,
    registry_roots: OnceLock<BTreeMap<String, OnceLock<Result<PathBuf, String>>>>,
    direct: OnceLock<Result<DirectRuntime, String>>,
    environment: Arc<crate::repository_environment::Snapshot>,
    host: Arc<crate::host_os::HostSnapshot>,
    /// IgnoredSubdirectoriesValue per repository, computed once per set,
    /// with what its computation observed for the analysis snapshot
    /// (W6R: replayed at every hit, crate::observed_fs::replay).
    ignored: std::sync::Mutex<BTreeMap<String, IgnoredEntry>>,
    /// Canonical forms of materialized roots and blocked prefixes for the
    /// link policy, memoized per invocation (published roots never move).
    /// Keyed by the path's bytes (a path spelled two ways is canonicalized
    /// twice, to the same answer), hashed: the roots of a link policy are
    /// looked up on every change of the materialized set, thousands each.
    link_canonical: std::sync::Mutex<
        std::collections::HashMap<std::ffi::OsString, PathBuf, crate::fast_hash::FxBuildHasher>,
    >,
    /// [`RepositorySet::shared_mapping`]: one shared copy per mapping table
    /// entry, by the entry's address.
    shared_mappings: std::sync::Mutex<std::collections::HashMap<usize, SharedMapping>>,
    /// The last canonical `others` of [`RepositorySet::link_policy`] per
    /// root view (materialized, sequential) with the roots it was made of.
    link_others: std::sync::Mutex<[Option<Arc<LinkOthers>>; 2]>,
    /// BUILD-phase `.bzl` modules evaluated in this invocation, shared by
    /// every package that loads them (BzlLoadValue; crate::loading).
    build_bzl: crate::loading::BzlCache,
    /// The `.bzl` modules definition loads evaluated (repository rule and
    /// module extension definitions and their loads; crate::loading, W6).
    definition_bzl: crate::loading::DefinitionBzlCache,
    /// The module-phase `.bzl` globals (immutable), built on first use.
    module_globals: OnceLock<starlark::environment::Globals>,
    /// The BUILD-file and BUILD-phase `.bzl` globals (immutable), built on
    /// first use.
    build_globals: OnceLock<(
        starlark::environment::Globals,
        starlark::environment::Globals,
    )>,
    /// Pinned mode ([`RepositorySet::pinned`], crate::snapshot::replay):
    /// roots, mappings and the generated flag answer from recorded tables;
    /// the module graph is never resolved and anything else is refused.
    pinned: Option<Arc<crate::snapshot::replay::PinnedTables>>,
    /// Isolated current-intent preparation, never exposed to an Engine. Every
    /// external producer is resolved and verified without materialization before
    /// a definition or recorded runtime input can be loaded from its root.
    publication_proof: bool,
    proof_receipts: BTreeMap<String, crate::repository_publication::Receipt>,
    current_locals: std::sync::Mutex<BTreeMap<String, Arc<current::LocalResult>>>,
    current_loading: std::sync::atomic::AtomicBool,
    proof_locals: BTreeMap<String, Arc<current::LocalResult>>,
    /// Only phase-aware cquery acceptance after exact current-prefix agreement.
    /// Other/default proof callers still reject diagnostic-producing preparation.
    phase_diagnostics_agreed: bool,
    /// The record mode's replay capture: local executions become replay
    /// records, and `root()`/`mapping()` results inside replay scopes are
    /// pinned for them.
    replay: Option<Arc<crate::snapshot::replay::ReplayCapture>>,
    /// Published store trees a link crossed into, verified once per set
    /// (crate::source_links::StoreTrees).
    verified_trees: Arc<std::sync::Mutex<BTreeMap<PathBuf, bool>>>,
    /// Repository rules executing ahead of their sequential demand
    /// (parallel fetching, crate::repository_speculation).
    speculation: crate::repository_speculation::Speculations,
    /// Module roots warmed ahead of their demand (crate::module_root_warm).
    root_warmer: crate::module_root_warm::RootWarmer,
    /// Repositories a file's loads name whose extension group was not
    /// prepared yet, by group: speculated once it is (`warm_loads`).
    pending_speculation: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
    /// `mod tidy`: extensions are evaluated for their eval-only value
    /// (no `use_repo` fixup warning).
    eval_only: std::sync::atomic::AtomicBool,
}

type IgnoredResult = Result<Arc<crate::ignored::IgnoredSubdirectories>, String>;
/// An ignored-subdirectories memo entry: the value and what computing it
/// observed (W6R).
type IgnoredEntry = (IgnoredResult, Option<Arc<crate::observed_fs::Captured>>);

/// What a local repository rule's execution callback observed, for its
/// replay record (crate::snapshot::replay::ReplayRecord).
struct LocalObservation {
    output: Vec<u8>,
    environ_read: bool,
    facilities: Vec<&'static str>,
    elapsed: std::time::Duration,
}

impl LocalObservation {
    fn of(
        executed: &crate::repository_ctx::ExecutedRepository,
        elapsed: std::time::Duration,
    ) -> Self {
        Self {
            output: executed.output.clone(),
            environ_read: executed.environ_read,
            facilities: executed.inputs.host_facilities.iter().copied().collect(),
            elapsed,
        }
    }
}

/// A non-local repository rule printed while it executed (print(), the
/// `execute(quiet=False)` echo, the reproducibility DEBUG lines): the
/// analysis snapshot's window captured that output, which the next
/// invocation — reusing the publication — would not print, so the run is
/// not saved even when crate::repository_store's re-probe settles the
/// execution itself.
fn taint_printed_output(canonical: &str, executed: &crate::repository_ctx::ExecutedRepository) {
    if !executed.output.is_empty() {
        crate::observed_fs::taint(crate::observed_fs::Taint::RepositoryRuleExecuted, || {
            format!("{canonical} printed {} bytes", executed.output.len())
        });
    }
}

/// A speculation (crate::repository_speculation) whose execution formed a
/// recorded-input key that depends on the roots the sequential engine will
/// have obtained when it executes the rule
/// ([`RepositorySet::friendly_repository`]) never publishes it: it aborts
/// before the publication, and the coordinator executes the rule itself.
fn speculation_order_check() -> Result<(), String> {
    if crate::repository_speculation::active() && crate::repository_demand::take_order_dependent() {
        return Err(crate::repository_demand::order_dependent(
            "a recorded input's repository key",
        ));
    }
    Ok(())
}

/// The canonical repository a no-demand sentinel (crate::repository_demand::
/// NEEDS_REPOSITORY) names when it was blocked on a direct or
/// extension-generated repository's root (`runtime N:repository:NAME`).
fn sentinel_repository(error: &str) -> Option<&str> {
    let start = error.find(crate::repository_demand::NEEDS_REPOSITORY)?;
    let rest =
        error[start + crate::repository_demand::NEEDS_REPOSITORY.len()..].strip_prefix(": ")?;
    let (subject, _) = rest.split_once(" is unfetched")?;
    let (_, key) = subject.strip_prefix("runtime ")?.split_once(':')?;
    key.strip_prefix("repository:")
}

/// The error of everything a pinned repository set does not answer.
const NOT_PINNED: &str = "not pinned for the replay of local repositories";

#[derive(Debug)]
struct DirectRuntime {
    catalog: DirectCatalog,
    groups: BTreeMap<String, DemandSlot<PreparedGroup>>,
    roots: BTreeMap<String, DemandSlot<PathBuf>>,
    ordinary: BTreeMap<String, DemandSlot<PreparedOrdinary>>,
}

impl DirectRuntime {
    /// The root slot of a direct or extension-generated repository, if it
    /// exists yet (an extension-generated one only once its group is
    /// prepared). Never prepares or evaluates anything.
    fn root_slot(&self, canonical: &str) -> Option<&DemandSlot<PathBuf>> {
        if let Some(slot) = self.roots.get(canonical) {
            return Some(slot);
        }
        let (prefix, generated) = canonical.rsplit_once('+')?;
        self.catalog
            .ordinary_routes
            .get(prefix)
            .and_then(|id| self.ordinary.get(id))
            .and_then(DemandSlot::ready)
            .and_then(|prepared| prepared.repositories.get(generated))
            .map(|repository| &repository.root)
    }

    /// The sequential engine has obtained `slot`'s root (W6 review F2 and
    /// its follow-up): it counts for [`RepositorySet::sequential_roots`],
    /// and so does every root the warm-reuse probe that published it
    /// obtained ([`DemandSlot::obtained`]), transitively — the evaluation
    /// the sequential engine runs instead of that probe demands them. A
    /// root that already counted had its own marked when it began to
    /// count. Only filled slots count; nothing is evaluated.
    ///
    /// What each publishing probe observed reaches the analysis snapshot's
    /// recording here, with the root (W6R, [`DemandSlot::observations`]):
    /// the sequential engine's evaluation of the root observes the same.
    fn mark_sequential(&self, slot: &DemandSlot<PathBuf>) {
        if slot.ready().is_none() {
            return;
        }
        slot.note_sequential();
        let replay = crate::observed_fs::hook(crate::observed_fs::Hook::Probe);
        if replay {
            crate::observed_fs::replay_some(slot.observations());
        }
        let mut pending: Vec<&str> = slot.obtained().iter().map(String::as_str).collect();
        while let Some(canonical) = pending.pop() {
            let Some(obtained) = self.root_slot(canonical) else {
                continue;
            };
            if obtained.ready().is_none() || obtained.sequential().is_some() {
                continue;
            }
            obtained.note_sequential();
            if replay {
                crate::observed_fs::replay_some(obtained.observations());
            }
            pending.extend(obtained.obtained().iter().map(String::as_str));
        }
    }

    /// `slot`, the root of `canonical`, was just obtained on this thread:
    /// on a loading worker (no-demand mode) its unit records it for the
    /// commit ([`UnitRoots`]); on the coordinator it counts at once,
    /// with what its publishing probe obtained.
    fn note_obtained(&self, canonical: &str, slot: &DemandSlot<PathBuf>) {
        if crate::repository_demand::no_demand_active() {
            UnitRoots::note(canonical, slot);
        } else {
            self.mark_sequential(slot);
        }
    }
}

#[derive(Debug)]
struct PreparedGroup {
    definition: repository_rule::RuleDefinition,
    loaded: loading::LoadedDefinition,
    label: Label,
    attributes: Vec<BTreeMap<String, AttrValue>>,
    /// Serialized source-digest entries of `loaded.sources`, computed ONCE
    /// when the group is prepared (review R1): every sibling repository
    /// frames only these digests + its own attributes/mappings into its
    /// spec key. Bytes (not a `Value` tree) so per-repo framing streams.
    source_entries: std::sync::OnceLock<Result<Vec<u8>, String>>,
}

impl PreparedGroup {
    fn source_entries(&self) -> Result<&[u8], String> {
        self.source_entries
            .get_or_init(|| {
                crate::repository_store::source_digest_entries_bytes(&self.loaded.sources)
            })
            .as_deref()
            .map_err(Clone::clone)
    }
}

/// Serialize one direct provenance object with `sources` rendered from
/// `sources_json` (the full `SourceProvenance` map for the content
/// identity, or the group's cached digest entries for the spec key).
/// Field order is the historical `serde_json::json!` order
/// (preserve_order: insertion order), so both renderings stay
/// byte-identical to the forms they replace.
#[allow(clippy::too_many_arguments)]
fn write_direct_provenance<W: std::io::Write>(
    out: &mut W,
    group: &crate::repository_catalog::DirectGroup,
    prepared: &PreparedGroup,
    entry: &crate::repository_catalog::DirectRepository,
    attributes: &BTreeMap<String, AttrValue>,
    owner_mapping: &BTreeMap<String, String>,
    sources_json: &[u8],
) -> Result<(), String> {
    fn field<W: std::io::Write>(
        out: &mut W,
        first: &mut bool,
        name: &str,
        value: &impl serde::Serialize,
    ) -> Result<(), String> {
        if !*first {
            out.write_all(b",").map_err(io_err)?;
        }
        *first = false;
        out.write_all(
            serde_json::to_string(name)
                .map_err(|e| format!("cannot serialize repository provenance: {e}"))?
                .as_bytes(),
        )
        .map_err(io_err)?;
        out.write_all(b":").map_err(io_err)?;
        serde_json::to_writer(&mut *out, value)
            .map_err(|e| format!("cannot serialize repository provenance: {e}"))
    }
    fn io_err(e: std::io::Error) -> String {
        format!("cannot serialize repository provenance: {e}")
    }
    out.write_all(b"{").map_err(io_err)?;
    let mut first = true;
    field(out, &mut first, "profile", &"direct-file-template-v1")?;
    field(out, &mut first, "owner", &group.owner)?;
    field(out, &mut first, "raw_identity", &group.raw_identity)?;
    field(out, &mut first, "public_label", &prepared.label.to_string())?;
    field(
        out,
        &mut first,
        "definition_identity",
        &prepared.definition.identity,
    )?;
    // sources: pre-rendered bytes (full closure or digest entries).
    if !first {
        out.write_all(b",").map_err(io_err)?;
    }
    first = false;
    out.write_all(b"\"sources\":").map_err(io_err)?;
    out.write_all(sources_json).map_err(io_err)?;
    field(out, &mut first, "attributes", &attributes)?;
    field(out, &mut first, "schema", &prepared.definition.schema)?;
    field(out, &mut first, "local", &prepared.definition.local)?;
    field(out, &mut first, "owner_mapping", &owner_mapping)?;
    field(out, &mut first, "repository_mapping", &entry.mapping)?;
    if prepared.definition.configure {
        field(out, &mut first, "configure", &true)?;
    }
    if !prepared.definition.environ.is_empty() {
        field(out, &mut first, "environ", &prepared.definition.environ)?;
    }
    out.write_all(b"}").map_err(io_err)?;
    Ok(())
}

/// Render the full direct provenance bytes (execute path only, review
/// R1): the closure is serialized from `prepared.loaded.sources`
/// in place — no intermediate `Value` tree.
fn direct_provenance_bytes(
    prepared: &PreparedGroup,
    group: &crate::repository_catalog::DirectGroup,
    entry: &crate::repository_catalog::DirectRepository,
    attributes: &BTreeMap<String, AttrValue>,
    owner_mapping: &BTreeMap<String, String>,
) -> Result<Vec<u8>, String> {
    let sources = serde_json::to_vec(&prepared.loaded.sources)
        .map_err(|e| format!("cannot serialize repository provenance: {e}"))?;
    let mut out = Vec::with_capacity(sources.len() + 1024);
    write_direct_provenance(
        &mut out,
        group,
        prepared,
        entry,
        attributes,
        owner_mapping,
        &sources,
    )?;
    Ok(out)
}

#[derive(Debug)]
struct PreparedOrdinary {
    loaded: loading::LoadedDefinition,
    generated_mapping: BTreeMap<String, String>,
    repositories: BTreeMap<String, OrdinaryRepository>,
    /// The group's provenance bytes ([`PreparedOrdinary::provenance`]):
    /// set at preparation when `recipe` is `None`, else on first use.
    provenance: OnceLock<Vec<u8>>,
    /// What regenerates `provenance` besides the fields above, when the
    /// bytes are not retained (MEM2): with a lockfile they depend on
    /// nothing the extension's evaluation observed, and only a repository
    /// that executes reads them again, so a warm run drops them right
    /// after hashing (~134 MB of JSON on ~/code: every generated
    /// repository's spec, schema included, of go_deps and pip).
    recipe: Option<ProvenanceRecipe>,
    /// Per-canonical-name spec digest cache (review R1), with what the
    /// digest's computation observed for the analysis snapshot (W6R: the
    /// declared environment; replayed at every hit).
    spec_keys: std::sync::Mutex<BTreeMap<String, SpecKeyEntry>>,
    /// The extension's metadata (`extension_metadata`), for `mod tidy`.
    metadata: Option<crate::extension_metadata::ExtensionMetadata>,
    /// SingleExtensionFunction's import check (a root or other module
    /// imports a repository the extension did not generate): the group's
    /// evaluation stands (SingleExtensionEvalFunction, which `mod tidy`
    /// reads), every other use fails with this error.
    import_error: Option<String>,
    /// SHA-256 of `provenance`, computed ONCE per prepared group:
    /// `specification_digest_bytes` framed these bytes into every
    /// sibling's spec key, re-hashing the whole (closure-bearing) group
    /// provenance per repository — the ordinary-path twin of the direct
    /// 64 GB re-hash. The per-repo key now mixes this digest instead.
    provenance_digest: [u8; 32],
}

/// An ordinary group's provenance parts that its [`PreparedOrdinary`]
/// does not keep otherwise (lockfile case only: no evaluation-derived
/// module tags, inputs or watched environment are part of the bytes).
#[derive(Debug)]
struct ProvenanceRecipe {
    public_label: String,
    symbol: String,
    prefix: String,
    tag_classes: BTreeMap<String, BTreeMap<String, crate::custom_rule::AttrSpec>>,
    source_guards: Vec<serde_json::Value>,
    root_has_non_dev_dependency: bool,
    imports: Vec<serde_json::Value>,
    host_os: Option<String>,
    host_arch: Option<String>,
    environ: crate::repository_environment::DeclaredEnvironment,
}

/// The provenance of an ordinary extension group: hashed into every
/// generated repository's spec key and identity. Field order is the
/// serialized key order.
#[derive(serde::Serialize)]
struct OrdinaryProvenance<'a> {
    profile: &'static str,
    public_label: String,
    symbol: &'a str,
    prefix: &'a str,
    sources: &'a BTreeMap<String, loading::SourceProvenance>,
    tag_classes: &'a BTreeMap<String, BTreeMap<String, crate::custom_rule::AttrSpec>>,
    modules: &'a [crate::extension_ctx::BoundModule],
    source_guards: &'a [serde_json::Value],
    root_has_non_dev_dependency: bool,
    generated_mapping: &'a BTreeMap<String, String>,
    repositories: &'a BTreeMap<String, OrdinaryRepository>,
    imports: &'a [serde_json::Value],
    #[serde(skip_serializing_if = "crate::host_os::HostFactors::is_empty")]
    host_factors: crate::host_os::HostFactors<'a>,
    /// Recorded module_ctx inputs (watched files and directories,
    /// non-hermetic host facilities); omitted when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    extension_inputs: Option<ExtensionInputs<'a>>,
    /// module_extension(environ=...) declarations and the digest of
    /// declared/module_ctx.getenv observations; omitted when empty
    /// so existing extension identities are unchanged.
    #[serde(skip_serializing_if = "crate::repository_environment::DeclaredEnvironment::is_empty")]
    environ: &'a crate::repository_environment::DeclaredEnvironment,
    #[serde(skip_serializing_if = "Option::is_none")]
    watched_env: Option<&'a crate::repository_environment::WatchedEnvironment>,
}

#[derive(serde::Serialize)]
struct ExtensionInputs<'a> {
    version: u8,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    files: &'a [crate::repository_ctx::FileInput],
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    host_facilities: &'a BTreeSet<&'static str>,
}

impl PreparedOrdinary {
    /// The group's provenance bytes: retained ones, or regenerated once
    /// from the recipe and checked against the digest taken at
    /// preparation (so a regeneration can never silently change a key).
    fn provenance(&self) -> Result<&[u8], String> {
        if let Some(bytes) = self.provenance.get() {
            return Ok(bytes);
        }
        let recipe = self.recipe.as_ref().ok_or_else(|| {
            "internal error: an ordinary extension provenance neither retained nor regenerable"
                .to_owned()
        })?;
        let mut bytes = bounded_provenance(
            &OrdinaryProvenance {
                profile: "ordinary-file-template-v1",
                public_label: recipe.public_label.clone(),
                symbol: &recipe.symbol,
                prefix: &recipe.prefix,
                sources: &self.loaded.sources,
                tag_classes: &recipe.tag_classes,
                modules: &[],
                source_guards: &recipe.source_guards,
                root_has_non_dev_dependency: recipe.root_has_non_dev_dependency,
                generated_mapping: &self.generated_mapping,
                repositories: &self.repositories,
                imports: &recipe.imports,
                host_factors: crate::host_os::HostFactors::from_parts(
                    recipe.host_os.as_deref(),
                    recipe.host_arch.as_deref(),
                ),
                extension_inputs: None,
                environ: &recipe.environ,
                watched_env: None,
            },
            usize::MAX,
        )?;
        let digest: [u8; 32] = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&bytes).into()
        };
        if digest != self.provenance_digest {
            return Err(format!(
                "internal error: the regenerated provenance of module extension {}%{} differs from the prepared one",
                recipe.public_label, recipe.symbol
            ));
        }
        // Kept from now on: a cold run executes many of the group's
        // repositories (the sink grows by doubling, so drop the slack).
        bytes.shrink_to_fit();
        Ok(self.provenance.get_or_init(|| bytes))
    }

    /// Length-framed provenance bytes for one repository of this group:
    /// canonical name + the group's shared provenance bytes. Built ON
    /// DEMAND and returned unretained (perf follow-up 2): caching a copy
    /// per canonical name held a full ~31 MB provenance copy per executed
    /// repository forever (a cold run executing ~2,058 repositories would
    /// hold ~64 GB), and each canonical name executes at most once per
    /// process, so a cache could never be hit twice. The caller
    /// (materialize_lazy) runs this only on the execute path.
    fn framed_provenance(&self, canonical_name: &str) -> Result<Vec<u8>, String> {
        let provenance = self.provenance()?;
        let mut framed = Vec::with_capacity(16 + canonical_name.len() + provenance.len());
        framed.extend_from_slice(&(canonical_name.len() as u64).to_le_bytes());
        framed.extend_from_slice(canonical_name.as_bytes());
        framed.extend_from_slice(&(provenance.len() as u64).to_le_bytes());
        framed.extend_from_slice(provenance);
        Ok(framed)
    }

    /// Spec digest for one repository, cached per canonical name.
    /// Mixes the group's ONCE-computed provenance digest with only the
    /// per-repository frame (canonical name + host + environment), so a
    /// warm run never re-hashes the shared closure bytes per sibling.
    fn spec_key(
        &self,
        repositories: &RepositorySet,
        canonical_name: &str,
        declared: &crate::repository_environment::DeclaredEnvironment,
    ) -> Result<String, String> {
        if let Ok(cache) = self.spec_keys.lock()
            && let Some((key, observations)) = cache.get(canonical_name)
        {
            // A worker's probe may have computed it (W6R): the sequential
            // engine's computation reads the declared environment here.
            if crate::observed_fs::hook(crate::observed_fs::Hook::Spec) {
                crate::observed_fs::replay_some(observations.as_ref());
            }
            return Ok(key.clone());
        }
        let (key, observations) = crate::observed_fs::capture(|| {
            crate::repository_warm::specification_digest_mixed(
                repositories,
                canonical_name,
                &self.provenance_digest,
                declared,
            )
        });
        let key = key?;
        if let Ok(mut cache) = self.spec_keys.lock() {
            cache
                .entry(canonical_name.to_owned())
                .or_insert((key.clone(), observations));
        }
        Ok(key)
    }
}

/// A cached ordinary spec key and what computing it observed (W6R).
type SpecKeyEntry = (String, Option<Arc<crate::observed_fs::Captured>>);

#[derive(Debug)]
struct OrdinaryRepository {
    definition: repository_rule::RuleDefinition,
    attributes: BTreeMap<String, AttrValue>,
    /// The explicitly specified attributes in call order (RepoRule.instantiate
    /// keeps only those, bazel/repository/RepoRule.java:110-117), for `mod
    /// show_repo`'s RepoDefinition.
    explicit: Vec<String>,
    original_name: String,
    canonical_name: String,
    root: DemandSlot<PathBuf>,
}

impl serde::Serialize for OrdinaryRepository {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let has_environ = !self.definition.environ.is_empty();
        let configure = self.definition.configure;
        let mut value = serializer.serialize_struct(
            "RepositorySpec",
            6 + usize::from(has_environ) + usize::from(configure),
        )?;
        value.serialize_field("definition_identity", &self.definition.identity)?;
        value.serialize_field("schema", &self.definition.schema)?;
        value.serialize_field("local", &self.definition.local)?;
        // Omitted when false so pre-existing provenance bytes/keys are unchanged.
        if configure {
            value.serialize_field("configure", &true)?;
        }
        value.serialize_field("attributes", &self.attributes)?;
        value.serialize_field("canonical_name", &self.canonical_name)?;
        value.serialize_field("original_name", &self.original_name)?;
        if has_environ {
            value.serialize_field("environ", &self.definition.environ)?;
        }
        value.end()
    }
}

// Stream borrowed provenance instead of first cloning potentially large
// defaults into serde_json::Value trees. Check the limit before buffer growth.
fn bounded_provenance(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, String> {
    struct Sink {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "ordinary extension host provenance limit exceeded",
                ));
            }
            let required = self.bytes.len() + bytes.len();
            if required > self.bytes.capacity() {
                let capacity = required
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.limit);
                self.bytes
                    .try_reserve_exact(capacity - self.bytes.len())
                    .map_err(std::io::Error::other)?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut sink, value)
        .map_err(|e| format!("cannot serialize ordinary extension provenance: {e}"))?;
    Ok(sink.bytes)
}

const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RepositorySet>();
    send_sync::<PreparedGroup>();
    send_sync::<PreparedOrdinary>();
};

impl RepositorySet {
    pub fn new(root: PathBuf) -> Self {
        Self::with_options(root, ResolverOptions::default())
    }

    pub fn with_options(root: PathBuf, options: ResolverOptions) -> Self {
        Self::configured(root, options, PathBuf::from("bazel-rs-out/repositories"))
    }

    fn configured(root: PathBuf, options: ResolverOptions, source_cache: PathBuf) -> Self {
        Self::configured_environment(
            root,
            options,
            source_cache,
            Arc::default(),
            Arc::default(),
            true,
        )
    }

    fn configured_environment(
        root: PathBuf,
        options: ResolverOptions,
        source_cache: PathBuf,
        environment: Arc<crate::repository_environment::Snapshot>,
        host: Arc<crate::host_os::HostSnapshot>,
        repository_downloads_enabled: bool,
    ) -> Self {
        Self {
            state: Arc::new(RepositoryState {
                runtime_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
                source_cache,
                absolute_source_cache: OnceLock::new(),
                root,
                options,
                repository_downloads_enabled,
                root_module: OnceLock::new(),
                resolved: OnceLock::new(),
                registry_roots: OnceLock::new(),
                direct: OnceLock::new(),
                environment,
                host,
                ignored: std::sync::Mutex::default(),
                link_canonical: std::sync::Mutex::default(),
                shared_mappings: std::sync::Mutex::default(),
                link_others: std::sync::Mutex::default(),
                build_bzl: crate::loading::BzlCache::default(),
                definition_bzl: crate::loading::DefinitionBzlCache::default(),
                module_globals: OnceLock::new(),
                build_globals: OnceLock::new(),
                pinned: None,
                publication_proof: false,
                proof_receipts: BTreeMap::new(),
                current_locals: Default::default(),
                current_loading: Default::default(),
                proof_locals: BTreeMap::new(),
                phase_diagnostics_agreed: false,
                replay: crate::snapshot::replay::global(),
                verified_trees: Arc::default(),
                speculation: Default::default(),
                root_warmer: Default::default(),
                pending_speculation: Default::default(),
                eval_only: Default::default(),
            }),
        }
    }

    /// A repository set for replaying local repository rules
    /// (crate::snapshot::replay): `root`, `mapping`, label parsing, the
    /// link policy and the generated flag answer from `tables` (the main
    /// repository is `tables.main_root`, the store `tables.source_cache`);
    /// it never reads MODULE.bazel, resolves the module graph, builds the
    /// extension catalog, prepares groups or materializes anything, and any
    /// repository or mapping the tables lack is an error (a miss).
    pub(crate) fn pinned(
        tables: crate::snapshot::replay::PinnedTables,
        options: ResolverOptions,
        environment: crate::repository_environment::Snapshot,
        repository_downloads_enabled: bool,
    ) -> Self {
        let mut set = Self::configured_environment(
            tables.main_root.clone(),
            options,
            tables.source_cache.clone(),
            Arc::new(environment),
            Arc::default(),
            repository_downloads_enabled,
        );
        let state = Arc::get_mut(&mut set.state).expect("a new repository state is unshared");
        state.pinned = Some(Arc::new(tables));
        state.replay = None;
        set
    }

    /// Record local executions and pinned tables into `capture` (tests; the
    /// record mode installs a process-wide capture instead).
    #[cfg(test)]
    pub(crate) fn with_replay_capture(
        self,
        capture: Arc<crate::snapshot::replay::ReplayCapture>,
    ) -> Self {
        let mut set = Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            self.state.environment.clone(),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        );
        Arc::get_mut(&mut set.state)
            .expect("a new repository state is unshared")
            .replay = Some(capture);
        set
    }

    fn pinned_tables(&self) -> Option<&crate::snapshot::replay::PinnedTables> {
        self.state.pinned.as_deref()
    }

    /// Whether phase-1 package prefetch (crate::prefetch) may run against
    /// this set: never for a pinned (replaying) set. A set that records for
    /// replay (the analysis snapshot's capture of local repository rules)
    /// allows it (W6R): its root/mapping capture records inside replay
    /// scopes, which only the coordinator opens (group preparation and
    /// local executions are coordinator-only; [`Self::replay_scope`]), so
    /// it sees exactly the sequential demands.
    pub(crate) fn prefetch_allowed(&self) -> bool {
        self.state.pinned.is_none()
            // Workers never resolve the module graph (a lazily resolving
            // worker could fetch outside the guard).
            && self.state.resolved.get().is_some_and(Result::is_ok)
    }

    /// Whether the demand a no-demand sentinel (`error`, see
    /// crate::repository_demand::NEEDS_REPOSITORY) was blocked on has been
    /// satisfied since: its slot holds a value now (a group the
    /// coordinator prepared, a root it materialized). Reads ready slots
    /// only — never evaluates anything. `false` when the blocking demand
    /// cannot be identified (crate::prefetch then leaves the package to
    /// the sequential path).
    pub(crate) fn sentinel_ready(&self, error: &str) -> bool {
        let Some(start) = error.find(crate::repository_demand::NEEDS_REPOSITORY) else {
            return false;
        };
        let rest = &error[start + crate::repository_demand::NEEDS_REPOSITORY.len()..];
        let Some(rest) = rest.strip_prefix(": ") else {
            return false;
        };
        let Some((subject, _)) = rest.split_once(" is unfetched") else {
            return false;
        };
        if let Some(repository) = subject.strip_prefix("repository @@") {
            // A module repository's materialization slot (guarded_root_slot).
            return self
                .state
                .registry_roots
                .get()
                .and_then(|roots| roots.get(repository))
                .is_some_and(|slot| slot.get().is_some());
        }
        let Some((_, key)) = subject
            .strip_prefix("runtime ")
            .and_then(|rest| rest.split_once(':'))
        else {
            return false;
        };
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return false;
        };
        if let Some(id) = key.strip_prefix("prepare:ordinary:") {
            return runtime
                .ordinary
                .get(id)
                .is_some_and(|slot| slot.ready().is_some());
        }
        if let Some(id) = key.strip_prefix("prepare:") {
            return runtime
                .groups
                .get(id)
                .is_some_and(|slot| slot.ready().is_some());
        }
        let Some(canonical) = key.strip_prefix("repository:") else {
            return false;
        };
        if let Some(slot) = runtime.roots.get(canonical) {
            return slot.ready().is_some();
        }
        let Some((prefix, name)) = canonical.rsplit_once('+') else {
            return false;
        };
        runtime
            .catalog
            .ordinary_routes
            .get(prefix)
            .and_then(|id| runtime.ordinary.get(id))
            .and_then(|slot| slot.ready())
            .and_then(|prepared| prepared.repositories.get(name))
            .is_some_and(|repository| repository.root.ready().is_some())
    }

    /// Whether this set records local executions for replay (the record
    /// mode's capture): only then is a live execution's output collected
    /// (crate::repository_ctx).
    pub(crate) fn records_for_replay(&self) -> bool {
        self.state.replay.is_some()
    }

    /// Give the replay capture this set's configuration (the first set
    /// wins), when it records for replay; false otherwise. A local rule's
    /// execution does it; so does a portable `map_each`, whose module a hit
    /// loads against the pinned tables even without local rules
    /// (crate::snapshot::mapper).
    pub(crate) fn note_replay_context(&self) -> bool {
        let Some(capture) = &self.state.replay else {
            return false;
        };
        let Ok(source_cache) = self.source_cache_path() else {
            return false;
        };
        self.set_replay_context(capture, source_cache);
        true
    }

    fn set_replay_context(
        &self,
        capture: &crate::snapshot::replay::ReplayCapture,
        source_cache: &Path,
    ) {
        capture.set_context(|| crate::snapshot::replay::CaptureContext {
            options: self.state.options.clone(),
            environment: self.state.environment.fresh(),
            downloads_enabled: self.state.repository_downloads_enabled,
            main_root: self.state.root.clone(),
            source_cache: source_cache.to_path_buf(),
        });
    }

    /// The replay capture, while a replay scope is open on this thread.
    fn capturing(&self) -> Option<&crate::snapshot::replay::ReplayCapture> {
        self.state
            .replay
            .as_deref()
            .filter(|_| crate::snapshot::replay::in_scope())
    }

    /// Open a replay scope on this thread when this set records for replay:
    /// the `root()` and `mapping()` results until the guard drops are
    /// pinned (definition loads of repository rules, local executions).
    fn replay_scope(&self) -> Option<crate::snapshot::replay::Scope> {
        // Phase-1 prefetch runs beside the record mode's capture (W6R):
        // a loading worker never opens a scope (it cannot prepare a group
        // or execute a local rule: the no-demand sentinel).
        debug_assert!(
            !crate::repository_demand::no_demand_active(),
            "a replay scope on a no-demand thread"
        );
        self.state
            .replay
            .is_some()
            .then(crate::snapshot::replay::scope)
    }

    /// A local repository rule executed (PLAN-L2 update 1 §(a)): no taint
    /// of the analysis snapshot but a replay record; printed output still
    /// taints (v1, §(b)). `definition` (how a replay loads the rule) is
    /// only formed while a replay capture records; a definition that cannot
    /// be formed leaves the execution without a replay, which taints.
    #[allow(clippy::too_many_arguments)]
    fn record_local_execution(
        &self,
        canonical: &str,
        origin: crate::repository_publication::Origin,
        spec: &str,
        original_name: &str,
        definition: impl FnOnce() -> Result<crate::snapshot::replay::DefinitionSource, String>,
        rule: &repository_rule::RuleDefinition,
        definition_label: &Label,
        attributes: &BTreeMap<String, AttrValue>,
        root: &Path,
        execution: crate::repository_store::LocalExecution,
        observed: Option<LocalObservation>,
    ) {
        // Replayed, never a REPO (also when a link crossed into the tree
        // before this execution).
        crate::observed_fs::local_tree(root);
        let Some(observed) = observed else {
            return;
        };
        self.record_current_local(
            canonical,
            origin,
            spec,
            &rule.identity,
            root,
            &execution,
            observed.output.is_empty(),
        );
        if !observed.output.is_empty() {
            crate::observed_fs::taint(crate::observed_fs::Taint::LocalRepositoryOutput, || {
                format!("{canonical} ({} bytes)", observed.output.len())
            });
        }
        let Some(capture) = &self.state.replay else {
            return;
        };
        let Ok(source_cache) = self.source_cache_path() else {
            return;
        };
        let definition = match definition() {
            Ok(definition) => definition,
            Err(error) => {
                crate::observed_fs::taint(crate::observed_fs::Taint::ReplayMismatch, || {
                    format!("{canonical}: no replay record: {error}")
                });
                return;
            }
        };
        self.set_replay_context(capture, source_cache);
        capture.record(crate::snapshot::replay::ReplayRecord {
            canonical: canonical.to_owned(),
            original_name: original_name.to_owned(),
            definition,
            identity: rule.identity.clone(),
            definition_label: definition_label.clone(),
            attributes: attributes.clone(),
            root: root.to_path_buf(),
            execution,
            output: observed.output,
            environ_read: observed.environ_read,
            facilities: observed.facilities,
            elapsed: observed.elapsed,
        });
    }

    /// The BUILD-file globals and the BUILD-phase `.bzl` globals
    /// (crate::package), shared by every package of the invocation.
    pub(crate) fn build_phase_globals(
        &self,
    ) -> (
        starlark::environment::Globals,
        starlark::environment::Globals,
    ) {
        self.state
            .build_globals
            .get_or_init(|| {
                (
                    crate::package::build_file_globals(),
                    crate::package::bzl_globals(),
                )
            })
            .clone()
    }

    /// The invocation's cache of BUILD-phase `.bzl` modules (crate::loading).
    pub(crate) fn build_bzl_cache(&self) -> &crate::loading::BzlCache {
        &self.state.build_bzl
    }

    /// The invocation's cache of definition `.bzl` modules (crate::loading,
    /// W6).
    pub(crate) fn definition_bzl_cache(&self) -> &crate::loading::DefinitionBzlCache {
        &self.state.definition_bzl
    }

    /// Whether definition loads may share the definition `.bzl` cache: not
    /// while root and mapping results are pinned (a replaying set must
    /// fail on every unpinned lookup). While they are captured for the
    /// analysis snapshot's replay records the cache keeps each module's
    /// `root()`/`mapping()` lookups and a hit inside a replay scope repeats
    /// them (W6R; crate::snapshot::replay::lookups), so the pinned tables
    /// are the uncached loader's.
    pub(crate) fn definition_cache_allowed(&self) -> bool {
        self.state.pinned.is_none()
            // The uncached loader, for A/B checks of the cache.
            && std::env::var_os("BAZEL_RS_DEBUG_NO_DEFINITION_CACHE").is_none()
    }

    /// Repeat `lookups` of a definition module the definition `.bzl` cache
    /// serves, inside a replay scope (see [`Self::definition_cache_allowed`]):
    /// each pins what the module's first evaluation pinned. The lookups
    /// already happened in this invocation, so nothing is evaluated anew.
    pub(crate) fn repeat_lookups(&self, lookups: &[crate::snapshot::replay::Lookup]) {
        if self.capturing().is_none() {
            return;
        }
        for lookup in lookups {
            match lookup {
                crate::snapshot::replay::Lookup::Root(name) => {
                    let _ = self.root(name);
                }
                crate::snapshot::replay::Lookup::Mapping(name) => {
                    let _ = self.mapping(name);
                }
            }
        }
    }

    /// The module-phase `.bzl` globals (crate::package::module_bzl_globals),
    /// built once per repository set.
    pub(crate) fn module_globals(&self) -> &starlark::environment::Globals {
        self.state
            .module_globals
            .get_or_init(crate::package::module_bzl_globals)
    }

    /// The repository's ignored subdirectories (`IgnoredSubdirectoriesValue`),
    /// evaluated once per repository set like a Skyframe value. The lock is
    /// not held while computing: evaluation may resolve repositories.
    ///
    /// A loading worker may compute it first (W6 review F2 follow-up FU1):
    /// the miss demands the repository's root (REPO.bazel and
    /// `.bazelignore` are read there, `ignored::for_repository`), a hit
    /// demands nothing, and the coordinator's target-pattern expansion
    /// asks before it demands the root — never, when the pattern is
    /// filtered out. So a hit notes the root ([`Self::note_obtained`]):
    /// the sequential engine's lookup there is the miss that demands it,
    /// or a hit after one. A success or a cached REPO.bazel error alike
    /// (the root was obtained before it failed); a root whose demand
    /// failed has no filled slot to note.
    ///
    /// Likewise what the miss observed for the analysis snapshot is kept
    /// with the entry and replayed at every hit (W6R): into the hitting
    /// load unit, or the recording on the coordinator.
    pub fn ignored_subdirectories(&self, repository: &str) -> IgnoredResult {
        let cached = self
            .state
            .ignored
            .lock()
            .map_err(|_| "ignored-subdirectories cache poisoned".to_owned())?
            .get(repository)
            .cloned();
        if let Some((result, observations)) = cached {
            self.note_obtained(repository);
            if crate::observed_fs::hook(crate::observed_fs::Hook::Ignored) {
                crate::observed_fs::replay_some(observations.as_ref());
            }
            return result;
        }
        let (computed, observations) = crate::observed_fs::capture(|| {
            crate::ignored::for_repository(&Arc::new(self.clone()), repository).map(Arc::new)
        });
        self.state
            .ignored
            .lock()
            .map_err(|_| "ignored-subdirectories cache poisoned".to_owned())?
            .entry(repository.to_owned())
            .or_insert((computed, observations))
            .0
            .clone()
    }

    pub fn with_source_cache(self, cache: PathBuf) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            cache,
            self.state.environment.clone(),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        )
    }

    /// Configure source acquisition, not registry metadata or Git execution.
    /// Verified completed trees and download blobs remain reusable when disabled.
    /// Like other configuration builders, this starts fresh demand memoization;
    /// the policy is deliberately absent from persistent content identities.
    pub fn with_repository_downloads_enabled(self, enabled: bool) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            self.state.environment.clone(),
            self.state.host.clone(),
            enabled,
        )
    }

    /// Use this fixed repository environment (the client environment with
    /// `--repo_env` applied, crate::repo_env) instead of reading the process
    /// environment. Like other configuration builders, this starts fresh
    /// demand memoization.
    pub fn with_repository_environment(
        self,
        environment: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            Arc::new(crate::repository_environment::Snapshot::with_reader(
                Arc::new(crate::repository_environment::FixedReader::new(environment)),
            )),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        )
    }

    pub fn repository_downloads_enabled(&self) -> bool {
        self.state.repository_downloads_enabled
    }

    /// Whether the genuine built-in profile (`--experimental-builtin-modules`)
    /// provides `@bazel_tools` and the builtin module closure.
    pub(crate) fn has_builtin_modules(&self) -> bool {
        self.state.options.builtin_profile == module_graph::BuiltinProfile::Bazel93Builtins
    }

    /// Make the repository directory `external/bazel_tools` the embedded
    /// `@bazel_tools` tree before a repository rule runs, as genuine's
    /// `<output_base>/external/bazel_tools` (a link to the install base's
    /// `embedded_tools`): a rule's relative `../bazel_tools/...` — `~/code`'s
    /// `--repo_env=JAVA_HOME=../bazel_tools/jdk`, which rules_jvm_external's
    /// coursier resolves as `rctx.path(JAVA_HOME + "/bin/java")` — finds
    /// the bundled JDK (docs/REPOSITORY_SLOTS.md, "The bazel_tools slot").
    /// A slot already linking the current bundle's tree is left as is
    /// without demanding the repository (a speculation worker never
    /// demands then); otherwise `@bazel_tools` is demanded (the no-demand
    /// sentinel is returned) and the slot bound, best effort: any other
    /// failure leaves the slot as it was.
    pub(crate) fn bind_builtin_tools_slot(&self) -> Result<(), String> {
        if !self.has_builtin_modules() {
            return Ok(());
        }
        let (Ok(identity), Ok(store)) = (
            crate::builtin_tools::bundle_identity(),
            self.source_cache_path(),
        ) else {
            return Ok(());
        };
        let slot = crate::repository_slots::slot_path(store, "bazel_tools");
        let relative = crate::repository_slots::builtin_link_target(&identity);
        if std::fs::read_link(&slot).is_ok_and(|target| target == relative)
            && std::fs::metadata(&slot).is_ok_and(|metadata| metadata.is_dir())
        {
            return Ok(());
        }
        let root = match self.root("bazel_tools") {
            Ok(root) => root.to_path_buf(),
            Err(error) if crate::repository_demand::is_needs_repository(&error) => {
                return Err(error);
            }
            Err(_) => return Ok(()),
        };
        // The builtin cache is `<store>/builtins` (Self::external_root):
        // the link is relative like an ordinary slot's, absolute otherwise.
        let in_store = std::fs::canonicalize(store.join("builtins/trees"))
            .is_ok_and(|trees| root.parent() == Some(trees.as_path()))
            && root.file_name() == Some(std::ffi::OsStr::new(&identity));
        let target = if in_store { relative } else { root };
        let _ = crate::repository_slots::bind_to(store, "bazel_tools", &target);
        Ok(())
    }

    /// `--incompatible_disable_starlark_host_transitions` (ResolverOptions).
    pub(crate) fn disable_starlark_host_transitions(&self) -> bool {
        self.state.options.disable_starlark_host_transitions
    }

    /// The Java Starlark semantics options (ResolverOptions::java_semantics).
    pub(crate) fn java_semantics(&self) -> crate::module_graph::JavaSemantics {
        self.state.options.java_semantics
    }

    pub(crate) fn environment(&self) -> &crate::repository_environment::Snapshot {
        &self.state.environment
    }

    pub(crate) fn host(&self) -> Result<Arc<crate::host_os::HostMetadata>, String> {
        self.state.host.get()
    }

    #[cfg(test)]
    fn with_host(self, host: Result<Arc<crate::host_os::HostMetadata>, String>) -> Self {
        // Never rewrite prepared handles or root/environment memoized values.
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            self.state.environment.clone(),
            Arc::new(crate::host_os::HostSnapshot::fixture(host)),
            self.state.repository_downloads_enabled,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_environment_reader(
        self,
        reader: Arc<dyn crate::repository_environment::EnvironmentReader>,
    ) -> Self {
        Self::configured_environment(
            self.state.root.clone(),
            self.state.options.clone(),
            self.state.source_cache.clone(),
            Arc::new(crate::repository_environment::Snapshot::with_reader(reader)),
            self.state.host.clone(),
            self.state.repository_downloads_enabled,
        )
    }

    /// Library-relative cache paths are workspace-relative. The CLI resolves
    /// its cwd-relative option before constructing this set. Both discovery
    /// and later repository access must reuse this exact absolute path.
    pub(crate) fn source_cache_path(&self) -> Result<&Path, String> {
        self.state
            .absolute_source_cache
            .get_or_init(|| {
                if self.state.source_cache.is_absolute() {
                    Ok(self.state.source_cache.clone())
                } else {
                    crate::observed_fs::canonicalize(&self.state.root)
                        .map(|root| root.join(&self.state.source_cache))
                        .map_err(|e| {
                            format!(
                                "cannot resolve repository cache workspace {}: {e}",
                                self.state.root.display()
                            )
                        })
                }
            })
            .as_ref()
            .map(PathBuf::as_path)
            .map_err(Clone::clone)
    }

    pub fn main_root(&self) -> &Path {
        &self.state.root
    }

    fn root_module(&self) -> Result<&module_graph::RootModuleSnapshot, String> {
        if self.pinned_tables().is_some() {
            return Err(format!("the root MODULE.bazel is {NOT_PINNED}"));
        }
        self.state
            .root_module
            .get_or_init(|| module_graph::read_root_module(&self.state.root, &self.state.options))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Record an optional root MODULE without resolving dependencies or loading
    /// repository/extension definitions. Later graph demand reuses this record,
    /// including cached absence or evaluation errors. Source-profile restrictions
    /// remain deferred to the repository demand paths that enforce them.
    pub fn validate_root_module(&self) -> Result<(), String> {
        self.state.options.validate_registries()?;
        self.root_module().map(|_| ()).map_err(|error| {
            // A root-module failure without an event reaches query/build as
            // RepositoryMappingResolutionException
            // (skyframe/RepositoryMappingValue.java:158-171).
            if error.starts_with(crate::module_graph::UNUSED_ISOLATED_USAGE) {
                format!("Error computing the main repository mapping: {error}")
            } else {
                error
            }
        })
    }

    /// Read-only proof of a batch of non-local publication hints. Groups share
    /// one fresh preparation cache, which (including all Starlark heaps) is
    /// released before return. Nothing is bound, published, refreshed or
    /// recorded into the calling analysis. A failure is a conservative miss.
    ///
    /// Current module metadata is read locally or from current-lock-checksummed
    /// CAS only. Source roots must be current live local roots or verified existing
    /// registry/Git/builtin publications. Generated producers require their own
    /// receipt and recursive current proof before loading. Absolute runtime paths,
    /// local rules and diagnostic-producing preparations remain unsupported.
    pub fn prove_publications(
        &self,
        receipts: &[crate::repository_publication::Receipt],
    ) -> Option<Vec<ProvedPublication>> {
        crate::observed_fs::record_private(|| self.prove_publications_current(receipts)).0
    }

    /// Only used inside a private recording by candidate acceptance. The
    /// public inspection API discards these observations instead.
    pub(crate) fn prove_publications_current(
        &self,
        receipts: &[crate::repository_publication::Receipt],
    ) -> Option<Vec<ProvedPublication>> {
        (|| {
            let notes = crate::ui::PrivateNotes::new();
            let mut options = self.state.options.clone();
            options.registry_session = options
                .registry_session
                .for_publication_proof(self.main_root(), self.source_cache_path().ok()?)
                .ok()?;
            let mut proof = Self::configured_environment(
                self.state.root.clone(),
                options,
                self.state.source_cache.clone(),
                Arc::new(self.state.environment.fresh()),
                self.state.host.clone(),
                false,
            );
            let state = Arc::get_mut(&mut proof.state)?;
            state.publication_proof = true;
            state.proof_receipts = receipts
                .iter()
                .map(|receipt| (receipt.canonical.clone(), receipt.clone()))
                .collect();
            state.proof_locals = self.state.proof_locals.clone();
            state.phase_diagnostics_agreed = self.state.phase_diagnostics_agreed;
            state.replay = None;
            proof.validate_root_module().ok()?;
            proof.root_module().ok()?.module_file()?;
            let workspace = crate::observed_fs::canonicalize(proof.main_root()).ok()?;
            let cache = proof.source_cache_path().ok()?;
            let mut proved = Vec::with_capacity(receipts.len());
            for receipt in receipts {
                let current = proof.prepare_publication_spec(&receipt.canonical).ok()?;
                if current.origin != receipt.origin || current.key != receipt.spec {
                    return None;
                }
                let candidate = crate::repository_store::probe_candidate(
                    cache,
                    receipt,
                    &workspace,
                    |tree, recorded| {
                        crate::repository_warm::unchanged_for_proof(
                            &proof, &workspace, tree, recorded,
                        )
                    },
                )?;
                proved.push(ProvedPublication {
                    receipt: candidate.receipt().clone(),
                });
            }
            (self.state.phase_diagnostics_agreed || !notes.emitted()).then_some(proved)
        })()
    }

    /// Recursively prove a derived definition/runtime root before loading any
    /// file from it. DemandSlot's existing depth/cycle checks bound recursion.
    /// A graph must supply its producer receipt; mere root existence is never
    /// enough. No normal materializer, speculative worker or binder is called.
    fn proof_generated_root(&self, canonical: &str) -> Result<&Path, String> {
        if let Some(local) = self.state.proof_locals.get(canonical) {
            local.validate(self.source_cache_path()?)?;
            let spec = self.prepare_spec(canonical, true)?;
            if spec.origin != local.origin || spec.key != local.spec {
                return Err(format!(
                    "current local producer @@{canonical} changed recipe"
                ));
            }
            crate::observed_fs::proof_repository_tree(
                "direct",
                &local.spec,
                &local.root,
                &self
                    .source_cache_path()?
                    .join("direct/manifests")
                    .join(format!("{}.json", local.execution.identity)),
            );
            return Ok(&local.root);
        }
        let runtime = self.direct_runtime()?;
        let slot = if let Some(slot) = runtime.roots.get(canonical) {
            slot
        } else {
            &self.ordinary_repository(canonical)?.1.root
        };
        slot.get_or_try_init(
            self.state.runtime_id,
            &format!("proof:repository:{canonical}"),
            || {
                let receipt = self.state.proof_receipts.get(canonical).ok_or_else(|| {
                    format!("no current-proof receipt for generated producer @@{canonical}")
                })?;
                let current = self.prepare_publication_spec(canonical)?;
                if current.origin != receipt.origin || current.key != receipt.spec {
                    return Err(format!(
                        "generated producer @@{canonical} changed specification"
                    ));
                }
                let workspace = crate::observed_fs::canonicalize(self.main_root())
                    .map_err(|e| e.to_string())?;
                let cache = self.source_cache_path()?;
                crate::repository_store::probe_candidate(
                    cache,
                    receipt,
                    &workspace,
                    |tree, recorded| {
                        crate::repository_warm::unchanged_for_proof(
                            self, &workspace, tree, recorded,
                        )
                    },
                )
                .ok_or_else(|| {
                    format!("generated producer @@{canonical} has no current publication proof")
                })?;
                let root = receipt.root(cache);
                let manifest = cache
                    .join("direct/manifests")
                    .join(format!("{}.json", receipt.publication));
                crate::observed_fs::proof_repository_tree(
                    "direct",
                    &receipt.spec,
                    &root,
                    &manifest,
                );
                Ok(root)
            },
        )
        .map(PathBuf::as_path)
    }

    fn prepare_publication_spec(&self, canonical: &str) -> Result<CurrentSpec, String> {
        self.prepare_spec(canonical, false)
    }

    fn prepare_spec(&self, canonical: &str, local: bool) -> Result<CurrentSpec, String> {
        use crate::repository_publication::Origin;
        if !self.state.publication_proof {
            return Err("current publication specs require isolated proof preparation".into());
        }
        validate_repository_name(canonical)?;
        let runtime = self.direct_runtime()?;
        if let Some(entry) = runtime.catalog.repositories.get(canonical) {
            let group = &runtime.catalog.groups[&entry.group_id];
            let prepared = self.prepare_direct_group(&entry.group_id)?;
            if prepared.definition.local != local {
                return Err("local repository rules cannot prove non-local selection".into());
            }
            return Ok(CurrentSpec {
                origin: Origin::DirectV2,
                key: self.direct_spec_key(
                    prepared,
                    group,
                    entry,
                    &prepared.attributes[entry.declaration_index],
                )?,
            });
        }
        let (prepared, repository) = self.ordinary_repository(canonical)?;
        if repository.definition.local != local {
            return Err("local repository rules cannot prove non-local selection".into());
        }
        Ok(CurrentSpec {
            origin: Origin::ExtensionV3,
            key: prepared.spec_key(self, canonical, &repository.definition.environ)?,
        })
    }

    /// The Bzlmod module associated with a repository, for native
    /// `module_name()`/`module_version()` (packages/StarlarkNativeModule.java:669-683,
    /// skyframe/RepositoryMappingFunction.java:100-159). Module-extension repos
    /// report their hosting module in Bazel; that association is not modeled.
    /// ModuleKey.toDisplayString (bazel/bzlmod/ModuleKey.java:70-76) of the
    /// module owning canonical repository `repository`: `root module` or
    /// `module 'NAME@VERSION'` (`_` for an empty key version).
    pub(crate) fn module_display(&self, repository: &str) -> String {
        if repository.is_empty() {
            return "root module".to_owned();
        }
        match self
            .resolve_graph()
            .ok()
            .and_then(|graph| graph.modules.get(repository))
        {
            Some(module) => format!(
                "module '{}@{}'",
                module.name,
                if module.version.is_empty() {
                    "_"
                } else {
                    module.version.as_str()
                }
            ),
            None => format!("module '@@{repository}'"),
        }
    }

    pub(crate) fn associated_module(&self, repository: &str) -> Result<(String, String), String> {
        if repository.is_empty() {
            return self.root_module()?.identity();
        }
        let graph = self.resolve_graph()?;
        if let Some(module) = graph.modules.get(repository) {
            // Module.getVersion is the module() declaration (InterimModule
            // keeps it for non-registry overrides, whose key version is
            // empty; ModuleFileFunction.java:274-279 forces equality otherwise).
            let version = match &module.module.module {
                Some(info) => crate::module_graph::normalize_declared_version(&info.version)?,
                None => module.version.clone(),
            };
            return Ok((module.name.clone(), version));
        }
        Err(format!(
            "module_name()/module_version() in repository @@{repository}: the associated module of a non-module (extension or direct) repository is not implemented by the native profile"
        ))
    }

    /// `register_toolchains` patterns with the repository they are parsed
    /// in, in RegisteredToolchainsFunction order
    /// (skyframe/toolchains/RegisteredToolchainsFunction.java:96-104, 149-172):
    /// the modules of the dependency graph in breadth-first order (root
    /// first). Non-root dev registrations are dropped, and the root's under
    /// --ignore_dev_dependency (bazel/bzlmod/ModuleFileGlobals.java:390-411).
    /// The module graph is resolved when the root module has dependencies.
    pub(crate) fn toolchain_registrations(&self) -> Result<Vec<(String, Vec<String>)>, String> {
        let root = self.root_module()?;
        self.registrations(root, crate::bzlmod::RegistrationKind::Toolchains)
    }

    /// `register_execution_platforms` patterns with the repository they are
    /// parsed in, in RegisteredExecutionPlatformsFunction.getBzlmodExecution-
    /// Platforms order (skyframe/toolchains/RegisteredExecutionPlatforms-
    /// Function.java:165-189: the dependency graph's modules, root first, each
    /// with its own repository mapping); dev registrations as for toolchains.
    /// Every rule resolving its execution platform asks: a workspace without
    /// a readable root module (whose failure the build reports where it
    /// needs the module) registers none. The other modules' registrations
    /// need the module graph: resolved when `resolve` (a rule that resolves
    /// toolchains, which resolves it anyway), else only read when it has
    /// already been resolved (the native lazy profile: a main-repository-only
    /// build never resolves it). The flag: whether the list is complete.
    pub(crate) fn execution_platform_registrations(
        &self,
        resolve: bool,
    ) -> Result<(Registrations, bool), String> {
        let Ok(root) = self.root_module() else {
            return Ok((Vec::new(), true));
        };
        let complete = resolve
            || root
                .module_file()
                .is_none_or(|module| module.effective_dependencies().next().is_none())
            || matches!(self.state.resolved.get(), Some(Ok(_)));
        if !complete {
            let mut found = self.registrations_of(
                root,
                crate::bzlmod::RegistrationKind::ExecutionPlatforms,
                false,
            )?;
            found.retain(|(repository, _)| repository.is_empty());
            return Ok((found, false));
        }
        Ok((
            self.registrations(root, crate::bzlmod::RegistrationKind::ExecutionPlatforms)?,
            true,
        ))
    }

    fn registrations(
        &self,
        root: &module_graph::RootModuleSnapshot,
        kind: crate::bzlmod::RegistrationKind,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        self.registrations_of(root, kind, true)
    }

    /// The root module's registrations of `kind`, then (with `graph`) the
    /// other modules' in breadth-first order.
    fn registrations_of(
        &self,
        root: &module_graph::RootModuleSnapshot,
        kind: crate::bzlmod::RegistrationKind,
        graph: bool,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        let ignore_dev = self.state.options.ignore_dev_dependencies;
        let mut found = Vec::new();
        let Some(module) = root.module_file() else {
            return Ok(found);
        };
        for registration in &module.registrations {
            if registration.kind == kind && !(ignore_dev && registration.dev_dependency) {
                found.push((String::new(), registration.labels.clone()));
            }
        }
        if !graph || module.effective_dependencies().next().is_none() {
            return Ok(found);
        }
        for module in self.resolve_graph()?.modules_in_bfs_order() {
            for registration in &module.module.registrations {
                if registration.kind == kind && !registration.dev_dependency {
                    found.push((module.canonical_name.clone(), registration.labels.clone()));
                }
            }
        }
        Ok(found)
    }

    /// Whether module resolution already ran and succeeded in this
    /// invocation (without starting it).
    pub fn graph_resolved(&self) -> bool {
        self.state.resolved.get().is_some_and(Result::is_ok)
    }

    pub fn resolve_graph(&self) -> Result<&ResolvedGraph, String> {
        if self.pinned_tables().is_some() {
            return Err(format!("the module graph is {NOT_PINNED}"));
        }
        self.state
            .resolved
            .get_or_init(|| {
                self.state.options.validate_registries()?;
                module_graph::resolve_from_root_snapshot(
                    self.root_module()?,
                    &self.state.options,
                    self.source_cache_path()?,
                )
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// `fetch --repo` (RepositoryFetcher.getRepositoryName): the canonical
    /// name `text` (`@apparent` through the main repository mapping,
    /// `@@canonical` as is) names. `Err`: the invalid-name detail (Bazel's
    /// `Invalid repo name: ...`); `Ok(Err(..))`: the repository's failure
    /// (RepositoryFetchFunction's `No repository visible as '@X' from main
    /// repository`, `Repository '@@X' is not defined`).
    pub fn fetch_repository_name(&self, text: &str) -> Result<Result<String, String>, String> {
        let canonical = if let Some(canonical) = text.strip_prefix("@@") {
            crate::label::validate_repository_name(canonical)?;
            canonical.to_owned()
        } else if let Some(apparent) = text.strip_prefix('@') {
            // RepositoryName.VALID_USER_PROVIDED_NAME.
            if !apparent
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
                || !apparent
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(format!(
                    "invalid user-provided repo name '{apparent}': valid names may contain only A-Z, a-z, 0-9, '-', '_', '.', and must start with a letter or a number"
                ));
            }
            match self.mapping("")?.get(apparent) {
                Some(canonical) => canonical.clone(),
                None => {
                    return Ok(Err(format!(
                        "No repository visible as '@{apparent}' from main repository"
                    )));
                }
            }
        } else {
            return Err(
                "The repo value has to be either apparent '@repo' or canonical '@@repo' repo name"
                    .to_owned(),
            );
        };
        if !self.repository_defined(&canonical)? {
            return Ok(Err(format!("Repository '@@{canonical}' is not defined")));
        }
        Ok(Ok(canonical))
    }

    /// Whether the canonical repository `canonical` exists: the main
    /// repository, a selected module, a `use_repo_rule` repository or a
    /// repository generated by its module extension (which this evaluates,
    /// like RepositoryFetchFunction's repository definition lookup).
    pub fn repository_defined(&self, canonical: &str) -> Result<bool, String> {
        if canonical.is_empty() || self.resolve_graph()?.modules.contains_key(canonical) {
            return Ok(true);
        }
        let runtime = self.direct_runtime()?;
        if runtime.catalog.repositories.contains_key(canonical) {
            return Ok(true);
        }
        let Some((prefix, name)) = canonical.rsplit_once('+') else {
            return Ok(false);
        };
        let Some(id) = runtime.catalog.ordinary_routes.get(prefix) else {
            return Ok(false);
        };
        Ok(self
            .prepare_ordinary_group(id)?
            .repositories
            .contains_key(name))
    }

    /// `fetch --all` / `fetch --configure` (BazelFetchAllFunction): every
    /// repository of the resolved module graph except the main one, then
    /// every repository of every module extension (each extension is
    /// evaluated or reused from the lockfile); with `configure` only the
    /// repositories of repository rules declared `configure = True`.
    pub fn all_repository_names(&self, configure: bool) -> Result<Vec<String>, String> {
        let graph = self.resolve_graph()?;
        let mut names: Vec<String> = Vec::new();
        if !configure {
            names.extend(
                graph
                    .modules
                    .keys()
                    .filter(|name| !name.is_empty())
                    .cloned(),
            );
        }
        let runtime = self.direct_runtime()?;
        for (canonical, entry) in &runtime.catalog.repositories {
            if !configure
                || self
                    .prepare_direct_group(&entry.group_id)?
                    .definition
                    .configure
            {
                names.push(canonical.clone());
            }
        }
        for id in runtime.catalog.ordinary_groups.keys() {
            let prepared = self.prepare_ordinary_group(id)?;
            for repository in prepared.repositories.values() {
                if !configure || repository.definition.configure {
                    names.push(repository.canonical_name.clone());
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        names.retain(|name| seen.insert(name.clone()));
        Ok(names)
    }

    /// `mod` (ModCommand, BazelModuleInspectorFunction.computExtensionRepos):
    /// the internal names of the repositories the ordinary module extension
    /// `label%symbol` generates (SingleExtensionValue.generatedRepoSpecs, in
    /// name order), evaluating it or reusing its lockfile entry; `None` when
    /// no module uses it.
    pub fn extension_repository_names(
        &self,
        label: &Label,
        symbol: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let runtime = self.direct_runtime()?;
        let Some((id, _)) = runtime
            .catalog
            .ordinary_groups
            .iter()
            .find(|(_, group)| group.label == *label && group.symbol == symbol)
        else {
            return Ok(None);
        };
        Ok(Some(
            self.prepare_ordinary_group(id)?
                .repositories
                .keys()
                .cloned()
                .collect(),
        ))
    }

    /// `mod show_repo` (RepoDefinitionFunction steps 4 and the innate
    /// extensions): the definition of a repository a module extension or a
    /// `use_repo_rule` declaration generates; `None` for any other name.
    pub fn generated_repository_definition(
        &self,
        canonical: &str,
    ) -> Result<Option<RepositoryDefinition>, String> {
        let rule = |identity: &str| -> Result<(String, String), String> {
            let id = crate::lockfile_extensions::repo_rule_id(identity)?;
            let (label, name) = id
                .rsplit_once('%')
                .ok_or_else(|| format!("invalid repository rule identity {identity:?}"))?;
            Ok((label.to_owned(), name.to_owned()))
        };
        let runtime = self.direct_runtime()?;
        if let Some(entry) = runtime.catalog.repositories.get(canonical) {
            let group = &runtime.catalog.groups[&entry.group_id];
            let declaration = &group.declarations[entry.declaration_index];
            let prepared = self.prepare_direct_group(&entry.group_id)?;
            let (rule_bzl, rule_name) = rule(&prepared.definition.identity)?;
            let values = &prepared.attributes[entry.declaration_index];
            // The declaration's keyword arguments in call order.
            let order =
                self.direct_attribute_order(&group.owner, &group.raw_identity, &declaration.name);
            let attributes = order
                .iter()
                .filter(|name| {
                    !crate::lockfile_extensions::LEGACY_BUILTIN_ATTRIBUTES.contains(&name.as_str())
                        && !declaration
                            .attributes
                            .get(name.as_str())
                            .is_none_or(serde_json::Value::is_null)
                })
                .filter_map(|name| values.get(name).map(|value| (name.clone(), value.clone())))
                .collect();
            return Ok(Some(RepositoryDefinition {
                rule_bzl,
                rule_name,
                name: canonical.to_owned(),
                original_name: Some(declaration.name.clone()),
                attributes,
            }));
        }
        let Some((prefix, internal)) = canonical.rsplit_once('+') else {
            return Ok(None);
        };
        let Some(id) = runtime.catalog.ordinary_routes.get(prefix) else {
            return Ok(None);
        };
        let prepared = self.prepare_ordinary_group(id)?;
        let Some(repository) = prepared.repositories.get(internal) else {
            return Ok(None);
        };
        let (rule_bzl, rule_name) = rule(&repository.definition.identity)?;
        Ok(Some(RepositoryDefinition {
            rule_bzl,
            rule_name,
            name: canonical.to_owned(),
            original_name: Some(repository.original_name.clone()),
            attributes: repository
                .explicit
                .iter()
                .filter_map(|name| {
                    repository
                        .attributes
                        .get(name)
                        .map(|value| (name.clone(), value.clone()))
                })
                .collect(),
        }))
    }

    /// `mod show_repo` (RepoDefinitionFunction steps 0-3): the definition of
    /// the repository of the selected module `canonical` — `@bazel_tools` is
    /// the embedded built-in (a RepoOverride at its directory), a
    /// non-registry override its RepoSpec, a registry module the RepoSpec
    /// IndexRegistry.getRepoSpec builds from its source.json. `None` for any
    /// other name.
    pub fn module_repository_definition(
        &self,
        canonical: &str,
    ) -> Result<Option<RepoDefinitionValue>, String> {
        use crate::custom_rule::AttrValue as V;
        let graph = self.resolve_graph()?;
        let Some(module) = graph.modules.get(canonical) else {
            return Ok(None);
        };
        let found = |rule_bzl: &str, rule_name: &str, attributes: Vec<(&str, V)>| {
            Ok(Some(RepoDefinitionValue::Found(RepositoryDefinition {
                rule_bzl: rule_bzl.to_owned(),
                rule_name: rule_name.to_owned(),
                name: canonical.to_owned(),
                original_name: None,
                attributes: attributes
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            })))
        };
        const LOCAL: &str = "@@bazel_tools//tools/build_defs/repo:local.bzl";
        const GIT: &str = "@@bazel_tools//tools/build_defs/repo:git.bzl";
        const HTTP: &str = "@@bazel_tools//tools/build_defs/repo:http.bzl";
        let overrides = if self.state.options.ignore_dev_dependencies {
            None
        } else {
            graph.root.overrides.get(&module.name)
        };
        match (&module.source, overrides) {
            (ModuleSource::Embedded(_), _) => {
                let root = self.root(canonical)?;
                Ok(Some(RepoDefinitionValue::Override(
                    root.display().to_string(),
                )))
            }
            (_, Some(crate::bzlmod::ModuleOverride::LocalPath { path })) => found(
                LOCAL,
                "local_repository",
                vec![("path", V::String(path.clone()))],
            ),
            (
                _,
                Some(crate::bzlmod::ModuleOverride::Git {
                    remote,
                    commit,
                    tag,
                    branch,
                    init_submodules,
                    strip_prefix,
                    ..
                }),
            ) => {
                let mut attributes = vec![("remote", V::String(remote.clone()))];
                for (name, value) in [
                    ("commit", commit),
                    ("tag", tag),
                    ("branch", branch),
                    ("strip_prefix", strip_prefix),
                ] {
                    if !value.is_empty() {
                        attributes.push((name, V::String(value.clone())));
                    }
                }
                if *init_submodules {
                    attributes.push(("init_submodules", V::Bool(true)));
                }
                found(GIT, "git_repository", attributes)
            }
            (
                ModuleSource::Registry {
                    registry,
                    name,
                    version,
                    module_file,
                    ..
                },
                _,
            ) => {
                let access = &self.state.options.registry_session;
                let relative = format!("modules/{name}/{version}/source.json");
                let source = access.read_object(registry, &relative)?.ok_or_else(|| {
                    format!(
                        "Module {name}@{version}'s source.json not found in registry {}",
                        access.registry_display(registry)
                    )
                })?;
                let text = |key: &str| {
                    source
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                let string_map = |key: &str| -> Vec<(String, String)> {
                    source
                        .get(key)
                        .and_then(serde_json::Value::as_object)
                        .map(|map| {
                            map.iter()
                                .map(|(k, v)| {
                                    (k.clone(), v.as_str().unwrap_or_default().to_owned())
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let base = access.registry_display(registry);
                let construct = |segments: &[&str]| {
                    let mut url = base.clone();
                    for segment in segments {
                        if !url.ends_with('/') && !segment.starts_with('/') {
                            url.push('/');
                        }
                        url.push_str(segment);
                    }
                    url
                };
                let registry_config = access.read_object(registry, "bazel_registry.json")?;
                match text("type").as_deref().unwrap_or("archive") {
                    "local_path" => {
                        let path = text("path").unwrap_or_default();
                        let path = if path.starts_with('/') {
                            path
                        } else {
                            let module_base = registry_config
                                .as_ref()
                                .and_then(|config| config.get("module_base_path"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let joined = format!("{module_base}/{path}");
                            if module_base.starts_with('/') {
                                joined
                            } else {
                                match registry.local_path() {
                                    Some(local) => format!("{}/{joined}", local.display()),
                                    None => {
                                        return Err(format!(
                                            "Provided non local registry for module {name}@{version}"
                                        ));
                                    }
                                }
                            }
                        };
                        found(
                            LOCAL,
                            "local_repository",
                            vec![("path", V::String(normalize_path_fragment(&path)))],
                        )
                    }
                    kind @ ("archive" | "git_repository") => {
                        let module_file_url =
                            construct(&["modules", name, version, "MODULE.bazel"]);
                        let module_file_integrity = {
                            use base64::Engine as _;
                            use sha2::Digest as _;
                            format!(
                                "sha256-{}",
                                base64::engine::general_purpose::STANDARD
                                    .encode(sha2::Sha256::digest(module_file))
                            )
                        };
                        let patches: Vec<(String, String)> = string_map("patches")
                            .into_iter()
                            .map(|(file, integrity)| {
                                (
                                    construct(&["modules", name, version, "patches", &file]),
                                    integrity,
                                )
                            })
                            .collect();
                        let patch_strip = source
                            .get("patch_strip")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0) as i32;
                        if kind == "git_repository" {
                            let mut attributes = Vec::new();
                            for key in ["remote", "commit", "shallow_since", "tag"] {
                                if let Some(value) = text(key).filter(|value| !value.is_empty()) {
                                    attributes.push((key, V::String(value)));
                                }
                            }
                            let submodules = source
                                .get("init_submodules")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false);
                            attributes.push(("init_submodules", V::Bool(submodules)));
                            attributes.push(("recursive_init_submodules", V::Bool(submodules)));
                            attributes.push((
                                "verbose",
                                V::Bool(
                                    source
                                        .get("verbose")
                                        .and_then(serde_json::Value::as_bool)
                                        .unwrap_or(false),
                                ),
                            ));
                            if let Some(value) =
                                text("strip_prefix").filter(|value| !value.is_empty())
                            {
                                attributes.push(("strip_prefix", V::String(value)));
                            }
                            attributes.push((
                                "remote_module_file_urls",
                                V::Strings(vec![module_file_url]),
                            ));
                            attributes.push((
                                "remote_module_file_integrity",
                                V::String(module_file_integrity),
                            ));
                            attributes.push(("remote_patches", V::StringDict(patches)));
                            attributes.push(("remote_patch_strip", V::Int(patch_strip)));
                            return found(GIT, "git_repository", attributes);
                        }
                        let url = text("url").ok_or_else(|| {
                            format!("Missing source URL for module {name}@{version}")
                        })?;
                        let integrity = text("integrity").ok_or_else(|| {
                            format!("Missing integrity for module {name}@{version}")
                        })?;
                        let mut urls = Vec::new();
                        let mirrors = registry_config
                            .as_ref()
                            .and_then(|config| config.get("mirrors"))
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let (authority, path_and_query) = url
                            .split_once("://")
                            .map(|(_, rest)| {
                                rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p))
                            })
                            .unwrap_or(("", ""));
                        for mirror in mirrors.iter().filter_map(serde_json::Value::as_str) {
                            let mut mirrored = mirror.to_owned();
                            for segment in [authority, &format!("/{path_and_query}")] {
                                if !mirrored.ends_with('/') && !segment.starts_with('/') {
                                    mirrored.push('/');
                                }
                                mirrored.push_str(segment);
                            }
                            urls.push(mirrored);
                        }
                        urls.push(url.clone());
                        if let Some(mirror_urls) = source
                            .get("mirror_urls")
                            .and_then(serde_json::Value::as_array)
                        {
                            urls.extend(
                                mirror_urls
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_owned),
                            );
                        }
                        let overlay = string_map("overlay");
                        let mut attributes = vec![
                            ("urls", V::Strings(urls)),
                            ("integrity", V::String(integrity)),
                            (
                                "strip_prefix",
                                V::String(text("strip_prefix").unwrap_or_default()),
                            ),
                            ("remote_patches", V::StringDict(patches)),
                            (
                                "remote_file_urls",
                                V::StringListDict(
                                    overlay
                                        .iter()
                                        .map(|(file, _)| {
                                            (
                                                file.clone(),
                                                vec![construct(&[
                                                    "modules", name, version, "overlay", file,
                                                ])],
                                            )
                                        })
                                        .collect(),
                                ),
                            ),
                            ("remote_file_integrity", V::StringDict(overlay)),
                            ("remote_module_file_urls", V::Strings(vec![module_file_url])),
                            (
                                "remote_module_file_integrity",
                                V::String(module_file_integrity),
                            ),
                            ("remote_patch_strip", V::Int(patch_strip)),
                        ];
                        if let Some(archive_type) =
                            text("archive_type").filter(|value| !value.is_empty())
                        {
                            attributes.push(("type", V::String(archive_type)));
                        }
                        found(HTTP, "http_archive", attributes)
                    }
                    other => Err(format!(
                        "Invalid source type \"{other}\" for module {name}@{version}"
                    )),
                }
            }
            (ModuleSource::LocalPath(path), _) => found(
                LOCAL,
                "local_repository",
                vec![("path", V::String(path.display().to_string()))],
            ),
            (ModuleSource::Git { spec, .. }, _) => found(
                GIT,
                "git_repository",
                vec![
                    ("remote", V::String(spec.remote.clone())),
                    ("commit", V::String(spec.commit.clone())),
                ],
            ),
        }
    }

    /// `mod tidy` (BazelModTidyFunction): the `use_repo` fixups of the
    /// root module's ordinary extensions — the extension's identity as the
    /// root spells it (`extensionBzlFile%name`) and buildozer commands of the
    /// root MODULE.bazel — and the failures of the extensions that failed
    /// (each evaluated without the import check, like its eval-only key).
    /// `proxy_name` names a `use_extension` proxy by its location.
    pub fn root_extension_fixups(
        &self,
        proxy_name: &dyn Fn(&str) -> String,
    ) -> Result<RootFixups, String> {
        self.state.eval_only.store(true, Ordering::Relaxed);
        let runtime = self.direct_runtime()?;
        let mut fixups = Vec::new();
        let mut errors = Vec::new();
        for (id, group) in &runtime.catalog.ordinary_groups {
            let Some(root) = &group.root_usage else {
                continue;
            };
            match self.evaluate_ordinary_group(id) {
                Err(error) => errors.push(error),
                Ok(prepared) => {
                    let Some(metadata) = &prepared.metadata else {
                        continue;
                    };
                    let all: Vec<String> = prepared.repositories.keys().cloned().collect();
                    match crate::extension_metadata::fixup_commands(
                        metadata, root, &all, proxy_name,
                    ) {
                        Ok(Some(commands)) => {
                            let file = root
                                .proxies
                                .first()
                                .map_or(String::new(), |proxy| proxy.extension_bzl_file.clone());
                            fixups.push((format!("{file}%{}", root.extension_name), commands));
                        }
                        Ok(None) => {}
                        Err(error) => errors.push(error),
                    }
                }
            }
        }
        Ok((fixups, errors))
    }

    /// The keyword argument order of the `use_repo_rule` declaration `name`
    /// of `owner`'s innate extension `raw_identity`.
    fn direct_attribute_order(&self, owner: &str, raw_identity: &str, name: &str) -> Vec<String> {
        let Ok(graph) = self.resolve_graph() else {
            return Vec::new();
        };
        let module = if owner.is_empty() {
            &graph.root
        } else {
            match graph.modules.get(owner) {
                Some(module) => &module.module,
                None => return Vec::new(),
            }
        };
        module
            .extension_usages
            .iter()
            .filter(|usage| {
                usage.identity.is_repo_rule() && usage.identity.extension_name == raw_identity
            })
            .flat_map(|usage| &usage.tags)
            .find(|tag| {
                tag.attributes
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    == Some(name)
            })
            .map(|tag| tag.attribute_order.clone())
            .unwrap_or_default()
    }

    /// How many module extensions this invocation resolved — evaluated or
    /// reused from an up-to-date MODULE.bazel.lock entry (prepared ordinary
    /// groups with a ready result either way). BazelLockFileModule.
    /// afterCommand collects the SingleExtensionValues "evaluated in the
    /// current build" (:105-111) — demand-driven, like
    /// SingleExtensionEvalFunction. Count-only input for the write-path
    /// diagnostic: the catalog is metadata-only, and a resolution error
    /// maps to 0, which merely omits the diagnostic.
    pub fn resolved_module_extensions(&self) -> usize {
        self.direct_runtime()
            .map(|runtime| {
                runtime
                    .ordinary
                    .values()
                    .filter(|slot| slot.ready().is_some())
                    .count()
            })
            .unwrap_or(0)
    }

    /// The main repository mapping's apparent names in Bazel's entry order
    /// (BazelDepGraphValue.getRepositoryMapping, bzlmod/BazelDepGraphValue.java:126-160,
    /// with RepositoryMapping.withAdditionalMappings putting extension imports
    /// first, :108-117; Module.getRepoMappingWithBazelDepsOnly, Module.java:49-68:
    /// `""`, the root's repo name, then bazel_deps in declaration order).
    /// RepositoryMapping.getInverse returns the FIRST of these for a canonical
    /// name (cmdline/RepositoryMapping.java:142-150). The native mapping
    /// itself is name-ordered; names missing here fall back to it.
    pub fn main_mapping_order(&self) -> Vec<String> {
        self.mapping_order("")
    }

    /// [`Self::main_mapping_order`] of the module repository `canonical`
    /// (only the root module sees itself as `""`).
    pub fn mapping_order(&self, canonical: &str) -> Vec<String> {
        let Ok(graph) = self.resolve_graph() else {
            return Vec::new();
        };
        let root = if canonical.is_empty() {
            &graph.root
        } else {
            match graph.modules.get(canonical) {
                Some(module) => &module.module,
                None => return Vec::new(),
            }
        };
        let mut names = Vec::new();
        // ModuleExtensionUsages of one extension merge in first-use order.
        let mut usages: Vec<&crate::bzlmod::ExtensionUsage> = Vec::new();
        for usage in &root.extension_usages {
            if !usages.iter().any(|known| known.identity == usage.identity) {
                usages.push(usage);
            }
        }
        for first in usages {
            for usage in root
                .extension_usages
                .iter()
                .filter(|usage| usage.identity == first.identity)
            {
                names.extend(usage.imports.iter().map(|import| import.local_name.clone()));
            }
        }
        if canonical.is_empty() {
            names.push(String::new());
        }
        if let Some(module) = &root.module {
            names.push(module.effective_repo_name().to_owned());
        }
        names.extend(
            root.effective_dependencies()
                .filter_map(|dependency| dependency.effective_repo_name())
                .map(str::to_owned),
        );
        names
    }

    /// Canonical repository roots already materialized by this repository set
    /// (the current invocation): main, local_path modules of a resolved graph,
    /// published registry/Git/built-in trees and generated repositories whose
    /// demand completed. Never resolves, fetches or executes anything; stale
    /// trees and private stages are never listed. Includes the roots a
    /// loading worker's warm-reuse probe published (published store trees,
    /// which anchor links in any order anyway; [`Self::link_policy`]).
    pub fn materialized_roots(&self) -> Vec<(String, PathBuf)> {
        self.roots_materialized(false)
    }

    /// [`Self::materialized_roots`] as the sequential engine sees them (W6
    /// review F2 and its follow-ups). The invariant: at every point of the
    /// coordinator, a direct or extension-generated repository's root
    /// counts exactly when the sequential engine (one loading thread) has
    /// obtained it by that point, which is when
    /// - the coordinator demanded it (outside no-demand mode; a filled slot
    ///   included), or
    /// - the coordinator committed a load unit, or ran a load inline, that
    ///   obtained it: the package's root, and the root of every `.bzl`
    ///   module the load reached — recorded on a worker by [`UnitRoots`]
    ///   and noted at the commit, noted at once on the coordinator; or
    /// - the coordinator, or a load it committed or ran inline, hit a
    ///   worker-fillable, invocation-wide memo whose miss path demands it:
    ///   the root that such a memo demands on its miss path counts at a hit
    ///   ([`Self::note_obtained`]: at once on the coordinator, in a
    ///   worker's unit from its commit), whoever filled the memo — the
    ///   sequential engine's lookup there demands it, or an earlier one, a
    ///   miss, did. There are two such memos: the shared `.bzl` cache (a
    ///   module's root, `Loader::source_path`; a worker may have evaluated
    ///   the module for a unit the build never commits, or one that
    ///   aborted) and the ignored subdirectories
    ///   ([`Self::ignored_subdirectories`]: the repository's root;
    ///   target-pattern expansion asks for them before the root, which a
    ///   filtered-out pattern never demands); or
    /// - transitively, the warm-reuse probe that published a root that
    ///   counts obtained it while checking that root's recorded inputs
    ///   ([`DemandSlot::obtained`]): the coordinator's evaluation of the
    ///   root runs the same check and demands the same roots.
    ///
    /// No other memo a worker can fill demands a root on its miss path
    /// only (the audit in docs/PARALLEL_ANALYSIS.md); the definition `.bzl`
    /// cache's hits skip a module's root too, but only coordinator-only
    /// definition loads fill it. Registry, Git and built-in roots are only
    /// ever materialized outside no-demand mode. So a key formed from these
    /// roots ([`Self::friendly_repository`]) is the sequential engine's
    /// whatever phase-1 prefetch or pattern-loading workers published,
    /// provided the files a probe checks do not change during the
    /// invocation.
    fn sequential_roots(&self) -> Vec<(String, PathBuf)> {
        self.roots_materialized(true)
    }

    /// The roots of [`Self::materialized_roots`] (`sequential` false) or of
    /// [`Self::sequential_roots`].
    fn roots_materialized(&self, sequential: bool) -> Vec<(String, PathBuf)> {
        let ready = |slot: &DemandSlot<PathBuf>| {
            if sequential {
                slot.sequential().cloned()
            } else {
                slot.ready().cloned()
            }
        };
        let mut roots = vec![(String::new(), self.state.root.clone())];
        if let Some(pinned) = self.pinned_tables() {
            // Pinned: every recorded root (published trees anchor links in
            // any order anyway, crate::source_links::StoreTrees).
            roots.extend(
                pinned
                    .roots
                    .iter()
                    .map(|(name, root)| (name.clone(), root.path.clone())),
            );
            return roots;
        }
        if let Some(Ok(graph)) = self.state.resolved.get() {
            for (name, module) in &graph.modules {
                if let ModuleSource::LocalPath(root) = &module.source {
                    roots.push((name.clone(), root.clone()));
                }
            }
        }
        if let Some(registry_roots) = self.state.registry_roots.get() {
            for (name, slot) in registry_roots {
                if let Some(Ok(root)) = slot.get() {
                    roots.push((name.clone(), root.clone()));
                }
            }
        }
        if let Some(Ok(runtime)) = self.state.direct.get() {
            for (name, slot) in &runtime.roots {
                if let Some(root) = ready(slot) {
                    roots.push((name.clone(), root));
                }
            }
            // Groups are prepared outside no-demand mode only.
            for slot in runtime.ordinary.values() {
                if let Some(prepared) = slot.ready() {
                    for repository in prepared.repositories.values() {
                        if let Some(root) = ready(&repository.root) {
                            roots.push((repository.canonical_name.clone(), root));
                        }
                    }
                }
            }
        }
        roots
    }

    /// The coordinator committed a load unit that obtained these roots on
    /// a worker ([`UnitRoots`]): the sequential engine demands them at that
    /// load, so from now on they count for [`Self::sequential_roots`], with
    /// the roots their publishing probes obtained.
    pub(crate) fn note_committed_roots(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return;
        };
        for name in names {
            if let Some(slot) = runtime.root_slot(name) {
                runtime.mark_sequential(slot);
            }
        }
    }

    /// A worker-fillable, invocation-wide memo served a value whose miss
    /// path demands `repository`'s root (W6 review F2 follow-ups): a
    /// module of `repository` from the shared `.bzl` cache — the
    /// sequential engine's load of it reads it from that root
    /// (`Loader::source_path`), the first load evaluates it there — or
    /// `repository`'s ignored subdirectories
    /// ([`Self::ignored_subdirectories`]). Whoever filled the memo may have
    /// been a worker, possibly for a unit the build never commits or one
    /// that aborted. Noted like a demand of the root that finds it filled:
    /// on a loading worker in its unit, on the coordinator at once (an
    /// inline re-run's load runs the module extensions it demands before
    /// its commit). Never evaluates anything; only a filled direct or
    /// extension-generated root slot is noted. The main repository and
    /// local_path modules always count, and only the coordinator
    /// materializes registry, Git and built-in trees: they need no noting.
    pub(crate) fn note_obtained(&self, repository: &str) {
        if repository.is_empty() {
            return;
        }
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return;
        };
        let Some(slot) = runtime.root_slot(repository) else {
            return;
        };
        // A root that counts already counts with what its probe obtained.
        if slot.ready().is_none() || slot.sequential().is_some() {
            return;
        }
        runtime.note_obtained(repository, slot);
    }

    /// Start parallel fetching for this set (crate::repository_speculation,
    /// with the phase-1 prefetcher): at most `width` repository rules
    /// execute ahead of their sequential demand; `listener` runs after each
    /// root a speculation published. Off for a pinned set, while a replay
    /// capture records local rules only the coordinator executes anyway,
    /// and with `BAZEL_RS_DEBUG_NO_SPECULATION`.
    pub(crate) fn start_speculation(
        &self,
        width: usize,
        listener: crate::repository_speculation::Listener,
        jitter_millis: u64,
    ) {
        if width <= 1
            || self.state.publication_proof
            || self.state.pinned.is_some()
            || std::env::var_os("BAZEL_RS_DEBUG_NO_SPECULATION").is_some()
        {
            return;
        }
        let this = self.clone();
        self.state.speculation.start(
            width,
            Arc::new(move |canonical: &str| this.speculation_job(canonical)),
            listener,
            jitter_millis,
        );
    }

    /// Warm the registry module roots ahead of their demand with at most
    /// `width` workers (crate::module_root_warm; `width` <= 1: off). Never
    /// for a pinned set.
    pub(crate) fn enable_root_warming(&self, width: usize) {
        let width = if self.state.pinned.is_some() || self.state.publication_proof {
            0
        } else {
            width
        };
        self.state.root_warmer.enable(width);
    }

    /// Own root-warming workers for one invocation, independently of the
    /// CLI's intentionally leaked Engine/RepositorySet analysis graph.
    pub fn root_warming_scope(&self) -> RootWarmingScope {
        self.state.root_warmer.scope()
    }

    /// The files `labels` name are about to be loaded (the `load()`
    /// statements of a file the sequential engine entered): warm their
    /// module roots and, transitively, those of their loads
    /// (crate::module_root_warm). Never demands or records anything.
    pub(crate) fn warm_loads(&self, labels: &[Label]) {
        if labels.is_empty() || self.state.publication_proof {
            return;
        }
        // Direct and extension-generated repositories (of a prepared
        // group) execute on the speculation pool when one runs.
        if let Some(Ok(runtime)) = self.state.direct.get() {
            let mut requested = BTreeSet::new();
            for label in labels {
                if label.repository.is_empty() || !requested.insert(label.repository.as_str()) {
                    continue;
                }
                match runtime.root_slot(&label.repository) {
                    Some(slot) if slot.ready().is_none() => {
                        self.state.speculation.request(&label.repository);
                    }
                    Some(_) => {}
                    None => {
                        // A repository of a group not prepared yet: the
                        // batch's first load prepares it.
                        let group = label
                            .repository
                            .rsplit_once('+')
                            .and_then(|(prefix, _)| runtime.catalog.ordinary_routes.get(prefix))
                            .filter(|id| {
                                runtime
                                    .ordinary
                                    .get(*id)
                                    .is_some_and(|slot| slot.ready().is_none())
                            });
                        if let Some(id) = group
                            && self.state.speculation.accepting()
                        {
                            lock_ignoring_poison(&self.state.pending_speculation)
                                .entry(id.clone())
                                .or_default()
                                .push(label.repository.clone());
                        }
                    }
                }
            }
        }
        if self.state.root_warmer.enabled() {
            self.state.root_warmer.request(self, labels.iter().cloned());
        }
    }

    /// The registered toolchain (or execution platform) patterns are
    /// about to be expanded together (RegisteredToolchainsFunction):
    /// warm the module roots their packages and the module extensions
    /// generating their repositories load from.
    pub(crate) fn warm_registrations(&self, patterns: &[(String, String)]) {
        if !self.state.root_warmer.enabled() {
            return;
        }
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return;
        };
        let files = crate::observed_fs::discard(|| {
            let _guard = crate::repository_demand::no_demand();
            let mut files = Vec::new();
            for (repository, pattern) in patterns {
                let text = pattern.trim_start_matches('-');
                let base = text
                    .rsplit_once(':')
                    .map_or(text, |(base, _)| base)
                    .trim_end_matches("/...")
                    .trim_end_matches("...");
                let Ok(label) = self.parse(&format!("{base}:BUILD"), repository, "") else {
                    continue;
                };
                if let Some((prefix, _)) = label.repository.rsplit_once('+')
                    && let Some(group) = runtime
                        .catalog
                        .ordinary_routes
                        .get(prefix)
                        .and_then(|id| runtime.catalog.ordinary_groups.get(id))
                {
                    files.push(group.label.clone());
                    continue;
                }
                for name in ["BUILD.bazel", "BUILD"] {
                    files.push(Label {
                        name: name.to_owned(),
                        ..label.clone()
                    });
                }
            }
            files
        });
        self.state.root_warmer.request(self, files);
    }

    /// Whether [`Self::warm_root`] can know `repository`'s root: the main
    /// repository, a local_path module, or a registry module (not the
    /// built-in `@bazel_tools`).
    pub(crate) fn warmable_root(&self, repository: &str) -> bool {
        if repository.is_empty() {
            return true;
        }
        let Some(Ok(graph)) = self.state.resolved.get() else {
            return false;
        };
        repository != "bazel_tools"
            && graph.modules.get(repository).is_some_and(|module| {
                matches!(
                    module.source,
                    ModuleSource::LocalPath(_) | ModuleSource::Registry { .. }
                )
            })
    }

    /// `repository`'s root for a warm-up worker: the main repository's or
    /// a local_path module's, a materialized registry module's, or a
    /// registry module's tree published into the store now (never filling
    /// its demand slot, never recording; crate::module_root_warm).
    pub(crate) fn warm_root(&self, repository: &str) -> Option<PathBuf> {
        if repository.is_empty() {
            return Some(self.state.root.clone());
        }
        let graph = self.state.resolved.get()?.as_ref().ok()?;
        let module = graph.modules.get(repository)?;
        if let Some(Some(Ok(root))) = self
            .state
            .registry_roots
            .get()
            .and_then(|roots| roots.get(repository))
            .map(OnceLock::get)
        {
            return Some(root.clone());
        }
        match &module.source {
            ModuleSource::LocalPath(root) => Some(root.clone()),
            ModuleSource::Registry {
                registry,
                name,
                version,
                module_file,
                local_patches,
                ..
            } if repository != "bazel_tools" => {
                let cache = self.state.absolute_source_cache.get()?.as_ref().ok()?;
                crate::observed_fs::discard(|| {
                    warm_registry_source_root(
                        &self.state.options.registry_session,
                        registry,
                        name,
                        version,
                        module_file,
                        cache,
                        local_patches,
                        self.repository_downloads_enabled(),
                    )
                })
            }
            _ => None,
        }
    }

    /// The labels the file `package`/`name` of `repository` (at `root`)
    /// loads, resolved with the repository's base mapping
    /// (crate::module_root_warm).
    pub(crate) fn crawl_loads(
        &self,
        repository: &str,
        root: &Path,
        package: &str,
        name: &str,
    ) -> Vec<Label> {
        let Some(Ok(graph)) = self.state.resolved.get() else {
            return Vec::new();
        };
        let mapping = if repository.is_empty() {
            &graph.root_mapping
        } else {
            match graph.modules.get(repository) {
                Some(module) => &module.mapping,
                None => return Vec::new(),
            }
        };
        let path = root.join(package).join(name);
        crate::module_root_warm::file_loads(&path, repository, package, mapping)
    }

    /// Stop parallel fetching: speculations in flight are cancelled (and
    /// awaited), queued ones dropped; finished outcomes stay for the
    /// coordinator's demand.
    pub(crate) fn stop_speculation(&self) {
        self.state.speculation.stop();
    }

    /// A loading worker's unit was blocked on `sentinel` (the no-demand
    /// sentinel): when it names a direct or extension-generated repository,
    /// execute that repository's rule ahead of its sequential demand.
    pub(crate) fn speculate(&self, sentinel: &str) {
        if let Some(canonical) = sentinel_repository(sentinel) {
            self.state.speculation.request(canonical);
        }
    }

    /// A load unit the sequential engine commits was blocked on `sentinel`
    /// (the no-demand sentinel): execute the direct or extension-generated
    /// repository it names ahead of its demand ([`Self::speculate`]), or
    /// warm the registry module root it names (crate::module_root_warm).
    pub(crate) fn fetch_blocked_ahead(&self, sentinel: &str) {
        if !crate::repository_demand::is_needs_repository(sentinel) {
            return;
        }
        self.speculate(sentinel);
        let Some(start) = sentinel.find(crate::repository_demand::NEEDS_REPOSITORY) else {
            return;
        };
        let repository = sentinel[start + crate::repository_demand::NEEDS_REPOSITORY.len()..]
            .strip_prefix(": repository @@")
            .and_then(|rest| rest.split_once(" is unfetched"))
            .map(|(name, _)| name);
        if let Some(repository) = repository
            && self.state.root_warmer.enabled()
        {
            self.state.root_warmer.request(
                self,
                ["BUILD.bazel", "BUILD"].map(|name| Label {
                    repository: repository.to_owned(),
                    package: String::new(),
                    name: name.to_owned(),
                }),
            );
        }
    }

    /// Queue again the speculations blocked on a demand that is satisfied
    /// now ([`Self::sentinel_ready`]).
    pub(crate) fn retry_speculations(&self) {
        self.state
            .speculation
            .retry(&|blocked| self.sentinel_ready(blocked));
    }

    /// One speculation of `canonical` on a fetch worker
    /// (crate::repository_speculation): the materialization the coordinator
    /// runs at its demand, in no-demand + reuse-only mode, recording the
    /// roots it obtains and capturing what it observes.
    fn speculation_job(&self, canonical: &str) -> crate::repository_speculation::Finished {
        use crate::repository_speculation::{Finished, Outcome};
        let _guard = crate::repository_demand::no_demand();
        let _reuse = crate::repository_demand::reuse_only();
        let _ = crate::repository_demand::take_order_dependent();
        let slot = match self.speculation_slot(canonical) {
            Ok(slot) if slot.ready().is_none() => slot,
            Ok(_) => return Finished::Failed,
            Err(error) if crate::repository_demand::is_needs_repository(&error) => {
                return Finished::Blocked(error);
            }
            Err(_) => return Finished::Failed,
        };
        let (result, obtained) = UnitRoots::during(|| self.speculative_materialize(canonical));
        let order_dependent = crate::repository_demand::take_order_dependent();
        let root = match result {
            Err(error) if crate::repository_demand::is_needs_repository(&error) => {
                return if order_dependent {
                    Finished::Failed
                } else {
                    Finished::Blocked(error)
                };
            }
            Err(_) => return Finished::Failed,
            Ok(root) => root,
        };
        let output = crate::repository_speculation::take_output();
        let (root, obtained) = if output.is_empty() {
            match slot.publish_speculated(root, obtained) {
                Ok(()) => return Finished::Published,
                // The coordinator evaluates the slot: it takes the outcome.
                Err(kept) => kept,
            }
        } else {
            (root, obtained)
        };
        let withdraw = self.speculation_withdrawal(canonical, &root);
        Finished::Done(Box::new(Outcome {
            root,
            output,
            obtained,
            withdraw,
        }))
    }

    /// How to withdraw the publication at `root` a speculation of
    /// `canonical` made (crate::repository_speculation::withdraw_pointer), when
    /// the coordinator never commits it.
    fn speculation_withdrawal(
        &self,
        canonical: &str,
        root: &Path,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        let identity = root.file_name()?.to_str()?.to_owned();
        let cache = self.source_cache_path().ok()?.to_path_buf();
        // Memoized already; what computing them observes counts nowhere.
        let (spec_key, workspace) = crate::observed_fs::discard(|| {
            let runtime = self.direct_runtime().ok()?;
            let spec_key = if let Some(entry) = runtime.catalog.repositories.get(canonical) {
                let prepared = runtime.groups.get(&entry.group_id)?.ready()?;
                let group = &runtime.catalog.groups[&entry.group_id];
                let attributes = &prepared.attributes[entry.declaration_index];
                self.direct_spec_key(prepared, group, entry, attributes)
                    .ok()?
            } else {
                let (prepared, repository) = self.ordinary_repository(canonical).ok()?;
                prepared
                    .spec_key(
                        self,
                        &repository.canonical_name,
                        &repository.definition.environ,
                    )
                    .ok()?
            };
            let workspace = crate::observed_fs::canonicalize(self.main_root()).ok()?;
            Some((spec_key, crate::repository_warm::workspace_text(&workspace)))
        })?;
        let canonical = canonical.to_owned();
        Some(Box::new(move || {
            crate::repository_speculation::withdraw_pointer(
                &cache,
                &canonical,
                &spec_key,
                Some(&workspace),
                &identity,
            );
        }))
    }

    /// The root slot of a direct or extension-generated repository that may
    /// be speculated: a direct repository, or a repository of an extension
    /// group the coordinator prepared (an unprepared group blocks the
    /// speculation instead, [`Self::speculative_materialize`]).
    fn speculation_slot(&self, canonical: &str) -> Result<&DemandSlot<PathBuf>, String> {
        let runtime = self.direct_runtime()?;
        if let Some(slot) = runtime.roots.get(canonical) {
            return Ok(slot);
        }
        let (_, repository) = self.ordinary_repository(canonical)?;
        Ok(&repository.root)
    }

    /// [`Self::speculation_job`]'s materialization: exactly the
    /// coordinator's, for non-local rules only.
    fn speculative_materialize(&self, canonical: &str) -> Result<PathBuf, String> {
        let runtime = self.direct_runtime()?;
        if let Some(entry) = runtime.catalog.repositories.get(canonical) {
            let prepared = self.prepare_direct_group(&entry.group_id)?;
            if prepared.definition.local {
                return Err("a local repository rule is never speculated".to_owned());
            }
            return self.materialize_direct(canonical, entry, prepared);
        }
        let (prepared, repository) = self.ordinary_repository(canonical)?;
        if repository.definition.local {
            return Err("a local repository rule is never speculated".to_owned());
        }
        self.materialize_ordinary(prepared, repository)
    }

    /// The coordinator demands a repository a speculation executed: what
    /// the execution observed reaches the analysis snapshot's recording,
    /// the roots it obtained count, and what it printed is written — where
    /// the sequential engine executes the rule.
    fn commit_speculation(&self, outcome: crate::repository_speculation::Outcome) -> PathBuf {
        crate::observed_fs::replay_some(outcome.obtained.observations.as_ref());
        self.note_committed_roots(&outcome.obtained.keys);
        // Its notes are marked (crate::ui).
        crate::ui::emit(&outcome.output);
        outcome.root
    }

    /// The repository of a host path for RepoCacheFriendlyPath
    /// (RepoRecordedInput.java:325-405, formed by toRepoCacheFriendlyPath,
    /// StarlarkBaseExternalContext.java:1656-1704): the repository whose
    /// materialized root is the longest one containing `path` (the main
    /// repository is `""`) and `path` relative to that root. Bazel's
    /// external repositories all live under `<output_base>/external`,
    /// outside the workspace; native roots are the repository store under
    /// the cache directory — by default inside the workspace — and
    /// local_path_override directories, so the most specific root names
    /// the repository. Roots match as materialized or canonicalized
    /// (memoized like `link_policy`); equally long roots (repositories
    /// sharing one content-addressed tree) resolve to the smallest name.
    ///
    /// A path in the repository store ([`Self::in_repository_store`]) is
    /// never keyed to the main repository (review i20): one that no
    /// materialized root contains (a store directory, a tree no repository
    /// of this invocation names) is None — spelled absolute — with the
    /// cache inside the workspace exactly as with the cache outside it, and
    /// never an `@@//<cache>/...` key, which the lockfile check treats as
    /// outdated (`LockInputProbe`).
    pub(crate) fn friendly_repository(&self, path: &Path) -> Option<(String, PathBuf)> {
        // A path in a repository's directory (its slot, genuine's
        // `<output_base>/external/<name>/rel`, toRepoCacheFriendlyPath
        // :1656-1704) is `@@<name>//rel`, whatever the slot links now.
        if let Some(found) = self.in_repository_directory(path) {
            return Some(found);
        }
        // The sequential engine's roots (W6 review F2): a root a phase-1
        // prefetch worker happened to probe first must not change a key.
        let roots = self.sequential_roots();
        if crate::repository_speculation::active() {
            return self.speculative_friendly_repository(roots, path);
        }
        self.friendly_in(roots, path)
    }

    /// [`Self::friendly_repository`] on a speculation's thread
    /// (crate::repository_speculation): the sequential engine forms the key
    /// where it executes the rule, with the roots that count then — those
    /// that count now, those the rule obtains itself (and, transitively, the
    /// roots their publishing probes obtained), and possibly roots that only
    /// begin to count later. A root that begins to count later could change
    /// the key only of a path in the repository store (every other root
    /// counts from the start), and only if it is materialized by then. So
    /// the key is the one formed with the roots that count now plus the
    /// rule's own, and when every materialized root would form another key,
    /// or the path is in the store but in no such root, the speculation
    /// notes an order-dependent decision and never publishes
    /// (`speculation_order_check`). Residual: two repositories publishing
    /// the very same tree, the one with the smaller name materialized later.
    fn speculative_friendly_repository(
        &self,
        mut roots: Vec<(String, PathBuf)>,
        path: &Path,
    ) -> Option<(String, PathBuf)> {
        if let Some(Ok(runtime)) = self.state.direct.get() {
            let mut pending: Vec<String> = UNIT_ROOTS
                .with(|obtained| obtained.borrow().clone())
                .unwrap_or_default()
                .into_iter()
                .collect();
            let mut seen = BTreeSet::new();
            while let Some(name) = pending.pop() {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if let Some(slot) = runtime.root_slot(&name)
                    && let Some(root) = slot.ready()
                {
                    roots.push((name, root.clone()));
                    pending.extend(slot.obtained().iter().cloned());
                }
            }
        }
        let speculative = self.friendly_in(roots, path);
        let materialized = self.friendly_in(self.materialized_roots(), path);
        if speculative != materialized || (speculative.is_none() && self.in_repository_store(path))
        {
            crate::repository_demand::note_order_dependent();
        }
        speculative
    }

    /// [`Self::friendly_repository`] over `roots`.
    fn friendly_in(&self, roots: Vec<(String, PathBuf)>, path: &Path) -> Option<(String, PathBuf)> {
        let mut canonical = self
            .link_canonical_all(roots.iter().map(|(_, root)| root.as_path()))
            .into_iter();
        // longest_root asks once per root, in order.
        let found = longest_root(roots, |_| canonical.next().flatten(), path);
        found.filter(|(name, _)| !(name.is_empty() && self.in_repository_store(path)))
    }

    /// The repository whose directory (`<store>/external/<name>`,
    /// crate::repository_slots) contains `path`, and `path` relative to it —
    /// spelled through the store path, its canonical form or, for a
    /// workspace-relative store, the workspace root as given (like
    /// [`Self::in_repository_store`]). Lexical: the path itself is not
    /// resolved. A `#long-` directory names no repository (its name is a
    /// digest).
    fn in_repository_directory(&self, path: &Path) -> Option<(String, PathBuf)> {
        let store = self.source_cache_path().ok()?;
        let given = self
            .state
            .source_cache
            .is_relative()
            .then(|| self.state.root.join(&self.state.source_cache));
        let canonical = {
            let mut memo = self
                .state
                .link_canonical
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match memo.get(store.as_os_str()) {
                Some(canonical) => Some(canonical.clone()),
                None => crate::observed_fs::canonicalize(store)
                    .ok()
                    .inspect(|canonical| {
                        memo.insert(store.as_os_str().to_owned(), canonical.clone());
                    }),
            }
        };
        let rest = [Some(store.to_path_buf()), canonical, given]
            .into_iter()
            .flatten()
            .find_map(|store| {
                path.strip_prefix(store.join(crate::repository_slots::SLOTS))
                    .ok()
                    .map(Path::to_path_buf)
            })?;
        let mut components = rest.components();
        let std::path::Component::Normal(name) = components.next()? else {
            return None;
        };
        let name = name.to_str()?;
        (!name.starts_with('#') && !name.starts_with('.'))
            .then(|| (name.to_owned(), components.as_path().to_path_buf()))
    }

    /// Whether `path` lies in the native repository store
    /// ([`Self::source_cache_path`]: content-addressed trees, their
    /// manifests and stages, module extension working directories) —
    /// spelled through the store path, its canonical form or, for a
    /// workspace-relative store, the workspace root as given. Lexical: the
    /// path itself is not resolved.
    pub(crate) fn in_repository_store(&self, path: &Path) -> bool {
        let Ok(store) = self.source_cache_path() else {
            return false;
        };
        if path.starts_with(store)
            || (self.state.source_cache.is_relative()
                && path.starts_with(self.state.root.join(&self.state.source_cache)))
        {
            return true;
        }
        let canonical = {
            let mut memo = self
                .state
                .link_canonical
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match memo.get(store.as_os_str()) {
                Some(canonical) => Some(canonical.clone()),
                None => link_canonical_form(store).ok().inspect(|canonical| {
                    memo.insert(store.as_os_str().to_owned(), canonical.clone());
                }),
            }
        };
        canonical.is_some_and(|canonical| path.starts_with(canonical))
    }

    /// Host prefixes no shorter materialized root may claim for links: the
    /// repository store (stale trees, private stages and trash) and, with the
    /// CLI layout `--cache-dir/repositories`, the whole `--cache-dir` root
    /// (action outputs and downloads too).
    pub fn link_blocked_prefixes(&self) -> Vec<PathBuf> {
        let Ok(store) = self.source_cache_path() else {
            return Vec::new();
        };
        let mut blocked = vec![store.to_path_buf()];
        if store.file_name().is_some_and(|name| name == "repositories")
            && let Some(root) = store.parent()
        {
            blocked.push(root.to_path_buf());
        }
        blocked
    }

    /// Record, once, the canonical forms the link policy and the
    /// recorded-input keys use (W6R): of every root that counts for the
    /// sequential engine now ([`Self::sequential_roots`]), of the
    /// repository store and of the blocked prefixes. [`Self::link_policy`],
    /// [`Self::friendly_repository`], [`Self::in_repository_store`] and
    /// crate::source_links take them unrecorded: which roots they see
    /// when depends on the loading thread count (a worker's unit asks while
    /// it runs), while every root that counted at some point of planning
    /// still counts at its end — call this at the end of planning, inside
    /// the recording window. The same set at every thread count.
    pub fn record_link_anchors(&self) {
        if !crate::observed_fs::recording()
            || !crate::observed_fs::hook(crate::observed_fs::Hook::Canon)
        {
            return;
        }
        let mut paths: Vec<PathBuf> = self
            .sequential_roots()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        if let Ok(store) = self.source_cache_path() {
            paths.push(store.to_path_buf());
        }
        paths.extend(self.link_blocked_prefixes());
        for path in paths {
            let _ = crate::observed_fs::canonicalize(&path);
        }
    }

    /// Whether `repository` is a repository generated by a repository rule
    /// (a `use_repo_rule` declaration or a module extension's repository
    /// call), executed through `repository_ctx`. Never resolves, fetches or
    /// executes anything.
    pub fn is_repository_rule_generated(&self, repository: &str) -> bool {
        if repository.is_empty() {
            return false;
        }
        if let Some(pinned) = self.pinned_tables() {
            return pinned
                .roots
                .get(repository)
                .is_some_and(|root| root.generated);
        }
        let Some(Ok(runtime)) = self.state.direct.get() else {
            return false;
        };
        if runtime.catalog.repositories.contains_key(repository) {
            return true;
        }
        runtime.ordinary.values().any(|slot| {
            slot.ready().is_some_and(|prepared| {
                prepared
                    .repositories
                    .values()
                    .any(|generated| generated.canonical_name == repository)
            })
        })
    }

    /// The memoized [`link_canonical_form`] of each of `paths`, in order.
    /// Only existing paths are memoized: a prefix created later in the
    /// invocation is canonicalized again. The memo is locked once to look
    /// the paths up and once to insert what was missing, never while the
    /// filesystem is asked (the coordinator and the loading workers ask
    /// for link policies all the time; two threads canonicalizing the same
    /// new root get the same answer).
    fn link_canonical_all<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Vec<Option<PathBuf>> {
        let memo = || {
            self.state
                .link_canonical
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let mut missing = Vec::new();
        let mut canonical: Vec<Option<PathBuf>> = {
            let memo = memo();
            paths
                .into_iter()
                .enumerate()
                .map(|(index, path)| {
                    let known = memo.get(path.as_os_str()).cloned();
                    if known.is_none() {
                        missing.push((index, path));
                    }
                    known
                })
                .collect()
        };
        if missing.is_empty() {
            return canonical;
        }
        let found: Vec<(usize, &Path, PathBuf)> = missing
            .into_iter()
            .filter_map(|(index, path)| {
                link_canonical_form(path)
                    .ok()
                    .map(|canonical| (index, path, canonical))
            })
            .collect();
        let mut memo = memo();
        for (index, path, path_canonical) in found {
            memo.insert(path.as_os_str().to_owned(), path_canonical.clone());
            canonical[index] = Some(path_canonical);
        }
        canonical
    }

    /// The shared native link policy for reads of `repository`'s sources,
    /// `.bzl` files and globs (`source_links::LinkPolicy`): materialized roots
    /// and blocked store prefixes, plus host-path reads for repositories
    /// generated by repository rules.
    ///
    /// While the analysis snapshot records (W6R), the roots are those that
    /// count for the sequential engine ([`Self::sequential_roots`]): a root
    /// a worker's probe published but the sequential engine has not
    /// obtained yet is a published store tree, which anchors a link in any
    /// order anyway (crate::source_links::StoreTrees), so no link decision
    /// changes, but what the policy records does not depend on the
    /// workers' progress — a crossing's REPO record is the threads-1 one.
    /// The canonical forms of the roots, the store and the blocked
    /// prefixes are taken unrecorded (which roots a policy holds depends
    /// on when a worker's unit asks); [`Self::record_link_anchors`]
    /// records them once.
    pub fn link_policy(&self, repository: &str) -> crate::source_links::LinkPolicy {
        if self.state.publication_proof {
            // Retained store files reached through a symlink cannot prove
            // their producer's current intent, even if their bytes verify.
            return crate::source_links::LinkPolicy {
                blocked: self
                    .link_blocked_prefixes()
                    .into_iter()
                    .map(|path| crate::observed_fs::canonicalize(&path).unwrap_or(path))
                    .collect(),
                ..Default::default()
            };
        }
        let sequential = crate::observed_fs::recording()
            && crate::observed_fs::hook(crate::observed_fs::Hook::Link);
        let roots = if sequential {
            self.sequential_roots()
        } else {
            self.materialized_roots()
        };
        // The same roots as last time share the canonical list (a root
        // that does not exist yet is retried: such a list is not kept).
        // Neither lock is held while a path is canonicalized: the
        // coordinator and the loading workers ask for policies all the
        // time, and two threads canonicalizing the same new root get the
        // same answer.
        let slot = usize::from(sequential);
        let last = self
            .state
            .link_others
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[slot]
            .clone();
        let known = last
            .filter(|known| same_roots(&known.roots, &roots))
            .map(|known| Arc::clone(&known.others));
        let others = match known {
            Some(others) => others,
            None => {
                let count = roots.len();
                let others: Vec<(String, PathBuf)> = roots
                    .iter()
                    .zip(self.link_canonical_all(roots.iter().map(|(_, path)| path.as_path())))
                    .filter_map(|((name, _), canonical)| canonical.map(|path| (name.clone(), path)))
                    .collect();
                let others = Arc::new(others);
                // Roots only become ready: of two threads that computed
                // lists concurrently, the one that saw more roots keeps
                // its list (no ping-pong between an older and a newer set).
                if others.len() == count {
                    let mut last = self
                        .state
                        .link_others
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if last[slot]
                        .as_ref()
                        .is_none_or(|known| known.roots.len() <= count)
                    {
                        last[slot] = Some(Arc::new(LinkOthers {
                            roots,
                            others: Arc::clone(&others),
                        }));
                    }
                }
                others
            }
        };
        let prefixes = self.link_blocked_prefixes();
        let store = self.source_cache_path().ok();
        let mut canonical =
            self.link_canonical_all(prefixes.iter().map(PathBuf::as_path).chain(store));
        let store_canonical = store.and_then(|_| canonical.pop().flatten());
        let blocked = prefixes
            .into_iter()
            .zip(canonical)
            .map(|(path, canonical)| canonical.unwrap_or(path))
            .collect();
        // Published store trees anchor links whatever was materialized
        // before (PLAN-L2 update 1 §(c)).
        let store = store_canonical.map(|store| {
            crate::source_links::StoreTrees::new(store, self.state.verified_trees.clone())
        });
        crate::source_links::LinkPolicy {
            others,
            blocked,
            host_reads: self.is_repository_rule_generated(repository),
            store,
        }
    }

    pub fn root(&self, repository: &str) -> Result<&Path, String> {
        validate_repository_name(repository)?;
        if repository.is_empty() {
            return Ok(&self.state.root);
        }
        if let Some(pinned) = self.pinned_tables() {
            return pinned
                .roots
                .get(repository)
                .map(|root| root.path.as_path())
                .ok_or_else(|| format!("repository @@{repository} is {NOT_PINNED}"));
        }
        let root = self.external_root(repository)?;
        if self.state.replay.is_some() {
            crate::snapshot::replay::note_lookup(|| {
                crate::snapshot::replay::Lookup::Root(repository.to_owned())
            });
        }
        if let Some(capture) = self.capturing() {
            capture.note_root(repository, root, || {
                self.is_repository_rule_generated(repository)
            });
        }
        Ok(root)
    }

    /// [`Self::root`] of a non-main repository of an unpinned set.
    fn external_root(&self, repository: &str) -> Result<&Path, String> {
        let graph = self.resolve_graph()?;
        if repository == "bazel_tools"
            && self.state.options.builtin_profile == module_graph::BuiltinProfile::ExplicitModules
            && !graph.modules.contains_key(repository)
        {
            // Every module's mapping names @@bazel_tools (module_graph::mapping),
            // but this profile has no built-in contents to materialize.
            return Err(BAZEL_TOOLS_UNAVAILABLE.to_owned());
        }
        let Some(module) = graph.modules.get(repository) else {
            if self.state.publication_proof {
                return self.proof_generated_root(repository);
            }
            return if self
                .direct_runtime()?
                .catalog
                .repositories
                .contains_key(repository)
            {
                self.direct_root(repository)
            } else {
                self.ordinary_root(repository)
            };
        };
        // MODULE overrides affect discovery metadata, not the upstream special
        // repository filesystem precedence for the canonical built-in name.
        if repository == "bazel_tools"
            && self.state.options.builtin_profile == module_graph::BuiltinProfile::Bazel93Builtins
        {
            let roots = self.state.registry_roots.get_or_init(|| {
                graph
                    .modules
                    .keys()
                    .map(|name| (name.clone(), OnceLock::new()))
                    .collect()
            });
            return guarded_root_slot(&roots[repository], repository, || {
                let cache = self.source_cache_path()?.join("builtins");
                if self.state.publication_proof {
                    crate::builtin_tools::probe_existing(&cache)
                } else {
                    crate::builtin_tools::materialize(&cache)
                }
            });
        }
        match &module.source {
            ModuleSource::Embedded(_) => Err("embedded module source requires the genuine built-in profile and canonical bazel_tools repository".to_owned()),
            ModuleSource::LocalPath(root) => Ok(root),
            ModuleSource::Git {
                spec,
                local_patches,
                tree,
            } => {
                if self.state.publication_proof {
                    // Discovery already proved this exact current Git key and
                    // recorded its integrity before evaluating its MODULE.
                    return Ok(&tree.root);
                }
                let roots = self.state.registry_roots.get_or_init(|| {
                    graph
                        .modules
                        .keys()
                        .map(|name| (name.clone(), OnceLock::new()))
                        .collect()
                });
                guarded_root_slot(&roots[repository], repository, || {
                    spec.validate(self.state.options.allow_remote)?;
                    if !tree.commit.eq_ignore_ascii_case(&spec.commit) {
                        return Err(format!("Git commit mismatch for repository @@{repository}"));
                    }
                    let (verified, ()) = crate::git_source::materialize(
                        spec,
                        local_patches,
                        self.source_cache_path()?,
                        self.state.options.allow_remote,
                        |_, bytes| {
                            if bytes != tree.module_file {
                                return Err(format!("Git MODULE differs from resolved graph for repository @@{repository}"));
                            }
                            Ok(())
                        },
                    )?;
                    Ok(verified.root)
                })
            }
            ModuleSource::Registry {
                registry,
                name,
                version,
                module_file,
                local_patches,
                ..
            } => {
                let roots = self.state.registry_roots.get_or_init(|| {
                    graph
                        .modules
                        .keys()
                        .map(|name| (name.clone(), OnceLock::new()))
                        .collect()
                });
                guarded_root_slot(&roots[repository], repository, || {
                    registry_source_root(
                        &self.state.options.registry_session,
                        registry,
                        name,
                        version,
                        module_file,
                        self.source_cache_path()?,
                        local_patches,
                        self.repository_downloads_enabled(),
                    )
                    .map_err(|error| {
                        format!("repository @@{repository} is not materialized: {error}")
                    })
                })
            }
        }
    }

    /// Return the source repository root, not its package subdirectory.
    pub fn package_root(&self, package: &PackageId) -> Result<&Path, String> {
        self.root(&package.repository)
    }

    pub fn mapping(&self, repository: &str) -> Result<&BTreeMap<String, String>, String> {
        validate_repository_name(repository)?;
        if let Some(pinned) = self.pinned_tables() {
            return pinned
                .mappings
                .get(repository)
                .map(|mapping| &mapping.mapping)
                .ok_or_else(|| {
                    format!("the repository mapping of @@{repository} is {NOT_PINNED}")
                });
        }
        let mapping = self.unpinned_mapping(repository)?;
        if self.state.replay.is_some() {
            crate::snapshot::replay::note_lookup(|| {
                crate::snapshot::replay::Lookup::Mapping(repository.to_owned())
            });
        }
        if let Some(capture) = self.capturing() {
            capture.note_mapping(repository, mapping, || self.isolated_imports(repository));
        }
        Ok(mapping)
    }

    /// [`Self::mapping`] as one shared copy per mapping table entry (a rule
    /// context keeps its repository's mapping; an extension's, which all
    /// its generated repositories share, can hold thousands of apparent
    /// names). The lookup itself (and what it notes for the snapshot) runs
    /// every time. The entries live in this set's write-once state (catalog,
    /// prepared groups, pinned tables) and are never freed or changed while
    /// it lives, so an entry's address identifies its content.
    pub fn shared_mapping(
        &self,
        repository: &str,
    ) -> Result<Arc<BTreeMap<String, String>>, String> {
        let mapping = self.mapping(repository)?;
        let address = std::ptr::from_ref(mapping) as usize;
        let mut shared = self
            .state
            .shared_mappings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(Arc::clone(
            shared
                .entry(address)
                .or_insert_with(|| Arc::new(mapping.clone())),
        ))
    }

    fn unpinned_mapping(&self, repository: &str) -> Result<&BTreeMap<String, String>, String> {
        let catalog = &self.direct_runtime()?.catalog;
        if let Some(mapping) = catalog.owner_mappings.get(repository) {
            return Ok(mapping);
        }
        if let Some(entry) = catalog.repositories.get(repository) {
            return Ok(&entry.mapping);
        }
        let (prepared, _) = self.ordinary_repository(repository)?;
        Ok(&prepared.generated_mapping)
    }

    /// The apparent names `mapping_for_label` rejects in `repository`'s
    /// module: not in its base mapping, and the first extension usage that
    /// imports them is isolated (pinned with its mapping, so a pinned set
    /// parses exactly like this one; empty for non-module repositories).
    fn isolated_imports(&self, repository: &str) -> BTreeSet<String> {
        let mut isolated = BTreeSet::new();
        let Some(Ok(graph)) = self.state.resolved.get() else {
            return isolated;
        };
        let owner = if repository.is_empty() {
            Some((&graph.root, &graph.root_mapping))
        } else {
            graph
                .modules
                .get(repository)
                .map(|module| (&module.module, &module.mapping))
        };
        let Some((module, base)) = owner else {
            return isolated;
        };
        let mut seen = BTreeSet::new();
        for usage in &module.extension_usages {
            for import in &usage.imports {
                if (import.dev_dependency
                    && (!repository.is_empty() || self.state.options.ignore_dev_dependencies))
                    || base.contains_key(&import.local_name)
                    || !seen.insert(import.local_name.as_str())
                {
                    continue;
                }
                if usage.identity.isolation.is_some() {
                    isolated.insert(import.local_name.clone());
                }
            }
        }
        isolated
    }

    /// Catalog construction is pure metadata work. In particular, its OnceLock
    /// initializer cannot recursively demand a source root or load a definition.
    fn direct_runtime(&self) -> Result<&DirectRuntime, String> {
        if self.pinned_tables().is_some() {
            return Err(format!("the repository catalog is {NOT_PINNED}"));
        }
        self.state
            .direct
            .get_or_init(|| {
                let catalog = repository_catalog::build(
                    self.resolve_graph()?,
                    self.state.options.ignore_dev_dependencies,
                )?;
                let groups = catalog
                    .groups
                    .keys()
                    .map(|id| (id.clone(), DemandSlot::new()))
                    .collect();
                let roots = catalog
                    .repositories
                    .keys()
                    .map(|name| (name.clone(), DemandSlot::new()))
                    .collect();
                let ordinary = catalog
                    .ordinary_groups
                    .keys()
                    .map(|id| (id.clone(), DemandSlot::new()))
                    .collect();
                Ok(DirectRuntime {
                    catalog,
                    groups,
                    roots,
                    ordinary,
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn prepare_direct_group(&self, id: &str) -> Result<&PreparedGroup, String> {
        let runtime = self.direct_runtime()?;
        let group = runtime
            .catalog
            .groups
            .get(id)
            .ok_or_else(|| format!("unknown direct repository group {id:?}"))?;
        let slot = runtime
            .groups
            .get(id)
            .ok_or_else(|| format!("missing direct repository preparation slot {id:?}"))?;
        slot.get_or_try_init(self.state.runtime_id, &format!("prepare:{id}"), || {
            if let Some(error) = &group.owner_profile_error {
                return Err(error.clone());
            }
            let (text, symbol) = group.definition_request()?;
            // A local rule's replay loads this definition again from pinned
            // roots and mappings (crate::snapshot::replay).
            let pinning = self.replay_scope();
            let label = self.parse(text, &group.owner, "")?;
            let loaded = loading::load_shared_definition_module_in(
                Arc::new(self.clone()),
                &label,
                symbol,
                true,
            )?;
            drop(pinning);
            let definition = repository_rule::unpack(&loaded.value)
                .map_err(|error| format!("repository rule {label}%{symbol}: {error}"))?;
            // An innate usage validates every declaration before executing even
            // its first callback, including declarations not otherwise demanded.
            // Reserve the whole group's copied defaults and supplied payloads
            // before binding any declaration. A budget per invocation would
            // miss amplification by many name-only uses of one large default.
            let mut budget = crate::repository_budget::HostBudget::default();
            for declaration in &group.declarations {
                budget.schema(&definition.schema)?;
                definition.environ.charge(&mut budget)?;
                budget.string(&declaration.canonical_name)?;
                budget.node()?;
                for (name, value) in &declaration.attributes {
                    budget.string(name)?;
                    budget.json(value, 0)?;
                }
            }
            let attributes = group
                .declarations
                .iter()
                .map(|declaration| {
                    repository_rule::bind_attributes_where(
                        &definition,
                        &declaration.attributes,
                        &declaration.canonical_name,
                        |text| self.parse(text, &group.owner, ""),
                        &format!("to the {}", self.module_display(&group.owner)),
                    )
                    .map_err(|error| {
                        format!("repository @@{}: {error}", declaration.canonical_name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(PreparedGroup {
                definition,
                loaded,
                label,
                attributes,
                source_entries: std::sync::OnceLock::new(),
            })
        })
    }

    /// Step C worker probe for a direct repository: compute the digest-form
    /// specification WITHOUT evaluating the defining `.bzl` (the shared
    /// BzlCache can only serve already-evaluated modules — checked through
    /// `ReuseSources` hashes, pure stats/reads of files whose roots are
    /// already materialized), then `try_reuse`. Any doubt → `Ok(None)`
    /// (sentinel; coordinator fetches inline, exactly as before Step C).
    fn try_reuse_direct(
        &self,
        canonical: &str,
        entry: &crate::repository_catalog::DirectRepository,
    ) -> Result<Option<PathBuf>, String> {
        // Before and after: a speculation that starts while this probe runs
        // may publish the tree it reads (crate::repository_speculation).
        if self.state.speculation.hides(canonical) {
            return Ok(None);
        }
        let reused = self.try_reuse_direct_unhidden(canonical, entry)?;
        Ok(reused.filter(|_| !self.state.speculation.hides(canonical)))
    }

    fn try_reuse_direct_unhidden(
        &self,
        canonical: &str,
        entry: &crate::repository_catalog::DirectRepository,
    ) -> Result<Option<PathBuf>, String> {
        let runtime = self.direct_runtime()?;
        let group = &runtime.catalog.groups[&entry.group_id];
        if group.owner_profile_error.is_some() {
            return Ok(None);
        }
        let Some(prepared) = runtime.groups.get(&entry.group_id).and_then(|s| s.ready()) else {
            return Ok(None);
        };
        let attributes = &prepared.attributes[entry.declaration_index];
        if prepared.definition.local {
            return Ok(None); // local rules never reuse across invocations
        }
        // Per-repository frame only: the group's cached source-digest
        // entries + this repo's attributes/mappings (review R1). No deep
        // copy of the closure, no re-hash of source bytes.
        let spec_key = self.direct_spec_key(prepared, group, entry, attributes)?;
        let workspace = crate::observed_fs::canonicalize(self.main_root())
            .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
        let workspace_text = crate::repository_warm::workspace_text(&workspace);
        let validating = self.clone();
        let reused = crate::repository_store::try_reuse_in(
            self.source_cache_path()?,
            canonical,
            &spec_key,
            Some(&workspace_text),
            move |tree, recorded| {
                crate::repository_warm::unchanged(&validating, &workspace, tree, recorded)
            },
        )?;
        if let Some(root) = &reused {
            self.record_publication(
                canonical,
                crate::repository_publication::Origin::DirectV2,
                &spec_key,
                root,
            );
        }
        Ok(reused)
    }

    /// The direct spec key, streamed: per-repo fields + the group's
    /// once-computed digest entries. No closure `Value` is built.
    fn direct_spec_key(
        &self,
        prepared: &PreparedGroup,
        group: &crate::repository_catalog::DirectGroup,
        entry: &crate::repository_catalog::DirectRepository,
        attributes: &BTreeMap<String, AttrValue>,
    ) -> Result<String, String> {
        let runtime = self.direct_runtime()?;
        let owner_mapping = &runtime.catalog.owner_mappings[&group.owner];
        let entries = prepared.source_entries()?;
        crate::repository_warm::specification_digest_streaming(
            self,
            |out: &mut crate::repository_store::CountWriter| {
                write_direct_provenance(
                    out,
                    group,
                    prepared,
                    entry,
                    attributes,
                    owner_mapping,
                    entries,
                )
            },
            |out: &mut crate::repository_store::SeededDigestWriter| {
                write_direct_provenance(
                    out,
                    group,
                    prepared,
                    entry,
                    attributes,
                    owner_mapping,
                    entries,
                )
            },
            &prepared.definition.environ,
        )
    }

    fn direct_root(&self, canonical: &str) -> Result<&Path, String> {
        let runtime = self.direct_runtime()?;
        let entry = runtime.catalog.repositories.get(canonical).ok_or_else(|| {
            format!("unknown canonical repository @@{canonical}; extension repository not materialized or module not selected")
        })?;
        let slot = runtime
            .roots
            .get(canonical)
            .ok_or_else(|| format!("missing direct repository root slot @@{canonical}"))?;
        // Claim before preparation: definitions can themselves be generated,
        // and a recursive definition demand must encounter our cycle guard.
        // Step C: on a worker, the probe arm may serve a warm publication
        // (reads plus at most an idempotent manifest refresh, via
        // `try_reuse_direct`) without preparing the group. The roots its
        // recorded-input check obtains are stored with the publication
        // (W6 review F2 follow-up; `DirectRuntime::mark_sequential`).
        slot.get_or_probe_obtaining(
            self.state.runtime_id,
            &format!("repository:{canonical}"),
            || UnitRoots::during(|| self.try_reuse_direct(canonical, entry)),
            || {
                let prepared = {
                    let _m = crate::repository_store::timing::mark(
                        &crate::repository_store::timing::PHASES.prepare_group,
                    );
                    self.prepare_direct_group(&entry.group_id)?
                };
                // Executed already by a speculation (parallel fetching,
                // crate::repository_speculation): committed here, where the
                // sequential engine executes the rule.
                if !prepared.definition.local
                    && let Some(outcome) = self.state.speculation.take(canonical)
                {
                    return Ok(self.commit_speculation(*outcome));
                }
                self.materialize_direct(canonical, entry, prepared)
            },
        )
        .inspect(|_| runtime.note_obtained(canonical, slot))
        .map(PathBuf::as_path)
    }

    /// Execute (or reuse) the direct repository `canonical` (`entry`) of the
    /// prepared group `prepared`: the evaluation of its root slot, on the
    /// coordinator or — non-local rules only — on a speculation worker
    /// ([`Self::speculation_job`]).
    fn materialize_direct(
        &self,
        canonical: &str,
        entry: &crate::repository_catalog::DirectRepository,
        prepared: &PreparedGroup,
    ) -> Result<PathBuf, String> {
        let runtime = self.direct_runtime()?;
        let group = &runtime.catalog.groups[&entry.group_id];
        let declaration = &group.declarations[entry.declaration_index];
        let attributes = &prepared.attributes[entry.declaration_index];
        // Digest-form specification (Step A + review R1): the spec
        // key streams the group's ONCE-computed source-digest
        // entries + this repo's attributes/mappings. The full
        // provenance bytes are serialized LAZILY, only on the
        // execute path inside materialize (closure runs there).
        let owner_mapping = runtime.catalog.owner_mappings[&group.owner].clone();
        let spec_key = {
            let _m = crate::repository_store::timing::mark(
                &crate::repository_store::timing::PHASES.spec_serialize,
            );
            self.direct_spec_key(prepared, group, entry, attributes)?
        };
        // Hidden diagnostic (N62 cold-variance hunt,
        // BAZEL_RS_DEBUG_PROVENANCE=1): provenance bytes per
        // repository, so two cold runs can be diffed field by field.
        // Only computed when the diagnostic is requested (lazy
        // provenance otherwise stays unserialized on reuse paths).
        if std::env::var_os("BAZEL_RS_DEBUG_PROVENANCE").is_some()
            && !crate::repository_speculation::active()
        {
            let provenance =
                direct_provenance_bytes(prepared, group, entry, attributes, &owner_mapping)?;
            eprintln!(
                "PROVENANCE {canonical} {}",
                String::from_utf8_lossy(&provenance)
            );
        }
        crate::repository_store::timing::count_spec(spec_key.len() as u64, 0);
        let workspace = crate::observed_fs::canonicalize(self.main_root())
            .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
        let workspace_text = crate::repository_warm::workspace_text(&workspace);
        let this = Arc::new(self.clone());
        let validating = Arc::new(self.clone());
        // A local rule never reuses a publication across invocations:
        // Bazel's RepositoryFetchFunction.shouldUseCachedRepoContents
        // keeps local repositories refetching while fetching is
        // enabled, and every native invocation is a fresh server —
        // their unrecorded dependencies (which/execute/executed-file
        // reads) make stale reuse dangerous.
        let local = prepared.definition.local;
        let provenance =
            move || direct_provenance_bytes(prepared, group, entry, attributes, &owner_mapping);
        // A local execution's observations for its replay record.
        let observed = std::cell::RefCell::new(None);
        let observed_by = &observed;
        let execute = move |stage: &Path| {
            let started = std::time::Instant::now();
            let pinning = if local { this.replay_scope() } else { None };
            let executed = crate::repository_ctx::execute(
                this.clone(),
                &prepared.definition,
                &prepared.loaded.functions,
                attributes,
                canonical,
                &declaration.name,
                &prepared.label,
                stage,
            )?;
            drop(pinning);
            speculation_order_check()?;
            if local {
                *observed_by.borrow_mut() =
                    Some(LocalObservation::of(&executed, started.elapsed()));
            } else {
                taint_printed_output(canonical, &executed);
            }
            let inputs =
                crate::host_os::repository_inputs(this.host()?.as_ref(), &executed.inputs)?;
            let recorded = crate::repository_warm::record(&executed.inputs, &executed.watch_names)?;
            Ok(crate::repository_store::Executed { inputs, recorded })
        };
        if local {
            let (root, execution) = crate::repository_store::materialize_local(
                self.source_cache_path()?,
                canonical,
                &spec_key,
                provenance,
                execute,
            )?;
            // A local rule executes on every run (PERF-REPO).
            crate::repository_store::timing::record_local_execution(canonical);
            self.record_local_execution(
                canonical,
                crate::repository_publication::Origin::DirectV2,
                &spec_key,
                &declaration.name,
                || {
                    let (_, symbol) = group.definition_request()?;
                    Ok(crate::snapshot::replay::DefinitionSource::Direct {
                        label: prepared.label.clone(),
                        symbol: symbol.to_owned(),
                    })
                },
                &prepared.definition,
                &prepared.label,
                attributes,
                &root,
                execution,
                observed.into_inner(),
            );
            return Ok(root);
        }
        crate::repository_store::materialize_lazy_in(
            self.source_cache_path()?,
            canonical,
            &spec_key,
            Some(&workspace_text),
            provenance,
            move |tree, recorded| {
                crate::repository_warm::unchanged(&validating, &workspace, tree, recorded)
            },
            execute,
        )
        .map(|(root, _)| {
            self.record_publication(
                canonical,
                crate::repository_publication::Origin::DirectV2,
                &spec_key,
                &root,
            );
            root
        })
    }

    fn prepare_ordinary_group(&self, id: &str) -> Result<&PreparedOrdinary, String> {
        let prepared = self.evaluate_ordinary_group(id)?;
        match &prepared.import_error {
            Some(error) => Err(error.clone()),
            None => Ok(prepared),
        }
    }

    /// [`Self::prepare_ordinary_group`] without the import check
    /// (SingleExtensionValue.evalKey).
    fn evaluate_ordinary_group(&self, id: &str) -> Result<&PreparedOrdinary, String> {
        let runtime = self.direct_runtime()?;
        let group = runtime
            .catalog
            .ordinary_groups
            .get(id)
            .ok_or_else(|| format!("unknown ordinary extension group {id:?}"))?;
        let slot = runtime
            .ordinary
            .get(id)
            .ok_or_else(|| format!("missing ordinary extension preparation slot {id:?}"))?;
        let prepared = slot.get_or_try_init(self.state.runtime_id, &format!("prepare:ordinary:{id}"), || {
            let _span = crate::timeline::span("extension", || id.to_owned());
            for participant in &group.participants {
                if let Some(error) = &participant.source_profile_error {
                    return Err(error.clone());
                }
            }
            let pinning = self.replay_scope();
            let loaded = loading::load_shared_definition_module_in(
                Arc::new(self.clone()), &group.label, &group.symbol, false,
            )?;
            drop(pinning);
            let mut preparation_budget = crate::repository_budget::HostBudget::unbounded();
            for (label, source) in &loaded.sources {
                preparation_budget.string(label)?;
                preparation_budget.bytes(source.bytes.len())?;
                for (apparent, canonical) in &source.mapping {
                    preparation_budget.string(apparent)?;
                    preparation_budget.string(canonical)?;
                }
            }
            let definition = crate::module_extension::unpack(&loaded.value)
                .map_err(|error| format!("module extension {}%{}: {error}", group.label, group.symbol))?;
            let host = self.host()?;
            let host_factors = host.factors(definition.os_dependent, definition.arch_dependent, &mut preparation_budget)?;
            // SingleExtensionEvalFunction.compute (Bazel 9.3:144-318): a valid
            // lockfile entry is used instead of running the extension.
            let mut loaded = loaded;
            let lock = self.extension_lock(group, &definition, &loaded, host_factors.lockfile_key())?;
            let reused = match &lock {
                Some(lock) => self.reusable_lock_entry(lock)?,
                None => None,
            };
            let (recorded_repositories, metadata, execution) = match &reused {
                Some(entry) => {
                    let (recorded, functions) = self.repositories_from_lock_entry(entry)?;
                    loaded.functions.extend(functions);
                    let metadata = crate::lockfile_extensions::member(entry, "moduleExtensionMetadata")
                        .map(crate::lockfile_extensions::metadata_from_json)
                        .transpose()?;
                    (recorded, metadata, None)
                }
                None => {
                    if self.state.publication_proof {
                        return Err("publication proof requires a currently valid extension lock entry".into());
                    }
                    crate::observed_fs::taint(crate::observed_fs::Taint::ExtensionEvaluated, || {
                        format!("{}%{}", group.label, group.symbol)
                    });
                    let facts = lock.as_ref().map(|lock| lock.facts.clone()).unwrap_or_default();
                    let tainted = crate::observed_fs::tainted_section();
                    let mut execution = crate::extension_ctx::execute(
                        Arc::new(self.clone()), &definition, &loaded.functions, &group.label,
                        &group.symbol, &group.prefix, &group.participants,
                        group.root_has_non_dev_dependency, facts,
                    )?;
                    drop(tainted);
                    let recorded = std::mem::take(&mut execution.repositories);
                    (recorded, execution.metadata.clone(), Some(execution))
                }
            };
            // RepoRule.instantiate stores only explicitly specified, non-None
            // attributes, in call order (bazel/repository/RepoRule.java:110-117).
            let explicit_attributes: BTreeMap<String, Vec<String>> = recorded_repositories
                .iter()
                .map(|recorded| {
                    let names = recorded
                        .order
                        .iter()
                        .filter(|name| {
                            !crate::lockfile_extensions::LEGACY_BUILTIN_ATTRIBUTES.contains(&name.as_str())
                                && !matches!(
                                    recorded.attributes.get(name.as_str()),
                                    None | Some(crate::extension_calls::RecordedValue::None)
                                )
                        })
                        .cloned()
                        .collect();
                    (recorded.name.clone(), names)
                })
                .collect();

            // The requested extension host owns this mapping, not its first
            // importing MODULE. Install all siblings before binding any spec.
            let host_mapping = self.mapping(&group.label.repository)?;
            for (apparent, canonical) in host_mapping {
                preparation_budget.string(apparent)?;
                preparation_budget.string(canonical)?;
            }
            let mut generated_mapping = host_mapping.clone();
            let mut names = BTreeSet::new();
            // generatedRepoSpecs.keySet(): creation order, for metadata fixups.
            let mut generated_order = Vec::new();
            for recorded in &recorded_repositories {
                preparation_budget.string(&recorded.name)?;
                preparation_budget.bytes(group.prefix.len())?;
                crate::bzlmod::validate_repo_name(&recorded.name).map_err(|e| e.to_string())?;
                if !names.insert(recorded.name.clone()) {
                    return Err(format!("duplicate generated repository {:?}", recorded.name));
                }
                let canonical = format!("{}+{}", group.prefix, recorded.name);
                validate_repository_name(&canonical)?;
                let (prefix, _) = canonical.rsplit_once('+')
                    .ok_or_else(|| format!("invalid generated repository @@{canonical}"))?;
                if runtime.catalog.ordinary_routes.get(prefix).map(String::as_str) != Some(id) {
                    return Err(format!("ambiguous ordinary extension route for @@{canonical}"));
                }
                if self.resolve_graph()?.modules.contains_key(&canonical)
                    || runtime.catalog.repositories.contains_key(&canonical) {
                    return Err(format!("generated repository canonical name collision: @@{canonical}"));
                }
                generated_mapping.insert(recorded.name.clone(), canonical);
                generated_order.push(recorded.name.clone());
            }
            let mut repositories = BTreeMap::new();
            let resolution_budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::unbounded());
            for recorded in recorded_repositories {
                // Charge omitted defaults and supplied values before the binder
                // expands either into another owned attribute map.
                preparation_budget.schema(&recorded.definition.schema)?;
                recorded.definition.environ.charge(&mut preparation_budget)?;
                for (name, value) in &recorded.attributes {
                    preparation_budget.string(name)?;
                    preparation_budget.recorded(value, 0)?;
                }
                let canonical_name = generated_mapping[&recorded.name].clone();
                let attributes = repository_rule::bind_recorded_attributes_where(
                    &recorded.definition, &recorded.attributes, &canonical_name,
                    |text| {
                        resolution_budget.borrow_mut().bytes(Label::resolution_size_bound(text, &group.label.repository, &group.label.package, &generated_mapping))?;
                        Label::parse_in(text, &group.label.repository, &group.label.package, &generated_mapping)
                    },
                    &format!("in the extension '{}%{}'", group.label.canonical_starlark(), group.symbol),
                ).map_err(|error| format!("repository @@{canonical_name}: {error}"))?;
                let explicit = explicit_attributes.get(&recorded.name).cloned().unwrap_or_default();
                repositories.insert(recorded.name.clone(), OrdinaryRepository {
                    definition: recorded.definition,
                    attributes,
                    explicit,
                    original_name: recorded.name,
                    canonical_name,
                    root: DemandSlot::new(),
                });
            }
            if let Some(lock) = &lock {
                let specs = generated_order
                    .iter()
                    .map(|name| {
                        let repository = &repositories[name];
                        let mut attributes = Vec::new();
                        for attribute in &explicit_attributes[name] {
                            if let Some(value) = repository.attributes.get(attribute) {
                                attributes.push((
                                    attribute.clone(),
                                    crate::lockfile_extensions::attribute_json(value)?,
                                ));
                            }
                        }
                        Ok((
                            name.clone(),
                            crate::lockfile::Json::Object(vec![
                                (
                                    "repoRuleId".into(),
                                    crate::lockfile::Json::String(
                                        crate::lockfile_extensions::repo_rule_id(
                                            &repository.definition.identity,
                                        )?,
                                    ),
                                ),
                                ("attributes".into(), crate::lockfile::Json::Object(attributes)),
                            ]),
                        ))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                self.record_extension_lock(
                    lock,
                    reused.as_ref(),
                    execution.as_ref(),
                    &loaded,
                    metadata.as_ref(),
                    specs,
                )?;
            }
            // SingleExtensionEvalFunction.createSingleExtensionValue (Bazel
            // 9.3:455-487) then SingleExtensionFunction.java:53: metadata errors
            // fail the evaluation; the fixup warning precedes the import check.
            if let (Some(metadata), Some(root_usage)) = (&metadata, &group.root_usage) {
                match crate::extension_metadata::generate_fixup(metadata, root_usage, &generated_order) {
                    // SingleExtensionFunction reports the fixup; `mod tidy`
                    // reads the eval-only value, which does not.
                    Ok(Some(fixup)) if !self.state.eval_only.load(Ordering::Relaxed) => {
                        crate::ui::note(&fixup.warning())
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {}
                    Err(message) => return Err(format!(
                        "{message}\nerror evaluating module extension {} in {}",
                        group.symbol, group.label,
                    )),
                }
            }
            // Missing imports invalidate the complete usage even if that alias
            // was never requested. Nothing from this group is published yet.
            let mut imports = Vec::new();
            let mut source_guards = Vec::new();
            let mut import_error = None;
            for participant in &group.participants {
                preparation_budget.string(&participant.owner)?;
                source_guards.push(serde_json::json!({
                    "owner": participant.owner,
                    "source_profile_error": participant.source_profile_error,
                }));
                for import in &participant.imports {
                    preparation_budget.string(&import.local_name)?;
                    preparation_budget.string(&import.exported_name)?;
                    if !repositories.contains_key(&import.exported_name) {
                        import_error.get_or_insert_with(|| format!(
                            "module extension {}%{} did not declare imported repository {:?} (alias @{} in MODULE owner {:?})",
                            group.label, group.symbol, import.exported_name, import.local_name, participant.owner,
                        ));
                        continue;
                    }
                    imports.push(serde_json::json!({
                        "owner": participant.owner,
                        "local_name": import.local_name,
                        "exported_name": import.exported_name,
                        "dev_dependency": import.dev_dependency,
                    }));
                }
            }
            // With a lockfile, an extension result may be evaluated or taken
            // from MODULE.bazel.lock; generated repositories then depend only
            // on what the result determines (specs, mapping, sources), so both
            // paths give the same identity. Without one, the evaluation's
            // bound tags and recorded inputs are kept (previous identities).
            let no_inputs = crate::repository_ctx::ExecutionInputs::default();
            let evaluated = execution.as_ref().filter(|_| lock.is_none());
            let inputs = evaluated.map_or(&no_inputs, |execution| &execution.inputs);
            let modules = evaluated.map_or(&[][..], |execution| &execution.modules[..]);
            let provenance = bounded_provenance(&OrdinaryProvenance {
                profile: "ordinary-file-template-v1",
                public_label: group.label.to_string(),
                symbol: &group.symbol, prefix: &group.prefix,
                sources: &loaded.sources, tag_classes: &definition.tag_classes,
                modules, source_guards: &source_guards,
                root_has_non_dev_dependency: group.root_has_non_dev_dependency,
                generated_mapping: &generated_mapping, repositories: &repositories,
                imports: &imports,
                host_factors,
                extension_inputs: (!inputs.files.is_empty() || !inputs.host_facilities.is_empty())
                    .then_some(ExtensionInputs {
                        version: 2,
                        files: &inputs.files,
                        host_facilities: &inputs.host_facilities,
                    }),
                environ: &definition.environ,
                watched_env: inputs.watched_env.as_ref(),
            }, usize::MAX)?; // No size cap (Bazel has none); allocation failure stays explicit.
            let provenance_digest = {
                use sha2::Digest as _;
                sha2::Sha256::digest(&provenance).into()
            };
            // Only an executing repository reads the bytes again (MEM2):
            // with a lockfile they are regenerated on demand from the
            // recipe (checked against the digest); without one they may
            // hold the evaluation's observations, so they are kept.
            let (provenance, recipe) = if lock.is_some() {
                let (host_os, host_arch) = host_factors.parts();
                let recipe = ProvenanceRecipe {
                    public_label: group.label.to_string(),
                    symbol: group.symbol.clone(),
                    prefix: group.prefix.clone(),
                    tag_classes: definition.tag_classes.clone(),
                    source_guards,
                    root_has_non_dev_dependency: group.root_has_non_dev_dependency,
                    imports,
                    host_os: host_os.map(str::to_owned),
                    host_arch: host_arch.map(str::to_owned),
                    environ: definition.environ.clone(),
                };
                (OnceLock::new(), Some(recipe))
            } else {
                let mut provenance = provenance;
                provenance.shrink_to_fit(); // the sink grows by doubling
                (OnceLock::from(provenance), None)
            };
            Ok(PreparedOrdinary {
                loaded,
                generated_mapping,
                repositories,
                provenance,
                recipe,
                spec_keys: std::sync::Mutex::new(BTreeMap::new()),
                metadata,
                import_error,
                provenance_digest,
            })
        });
        if prepared.is_ok() {
            // The loads a file entered before this group was prepared
            // (`warm_loads`) can execute ahead now.
            let pending = lock_ignoring_poison(&self.state.pending_speculation).remove(id);
            for canonical in pending.unwrap_or_default() {
                self.state.speculation.request(&canonical);
            }
        }
        prepared
    }

    fn ordinary_repository(
        &self,
        canonical: &str,
    ) -> Result<(&PreparedOrdinary, &OrdinaryRepository), String> {
        let unknown = || {
            format!(
                "unknown canonical repository @@{canonical}; extension repository not materialized or module not selected"
            )
        };
        // Exact last-separator routing avoids executing an unrelated extension
        // for a merely similar canonical name. User names cannot contain '+'.
        let (prefix, name) = canonical.rsplit_once('+').ok_or_else(unknown)?;
        let runtime = self.direct_runtime()?;
        let id = runtime
            .catalog
            .ordinary_routes
            .get(prefix)
            .ok_or_else(unknown)?;
        crate::bzlmod::validate_repo_name(name).map_err(|error| error.to_string())?;
        let prepared = self.prepare_ordinary_group(id)?;
        let repository = prepared.repositories.get(name).ok_or_else(|| {
            format!("generated repository @@{canonical} was not declared by its module extension")
        })?;
        Ok((prepared, repository))
    }

    /// Step C worker probe for an ordinary (extension-generated)
    /// repository: only when the group is already prepared (its slot is
    /// ready — extension evaluation itself is coordinator-only) and the
    /// rule is not local.
    fn try_reuse_ordinary(&self, canonical: &str) -> Result<Option<PathBuf>, String> {
        // Before and after: a speculation that starts while this probe runs
        // may publish the tree it reads (crate::repository_speculation).
        if self.state.speculation.hides(canonical) {
            return Ok(None);
        }
        let reused = self.try_reuse_ordinary_unhidden(canonical)?;
        Ok(reused.filter(|_| !self.state.speculation.hides(canonical)))
    }

    fn try_reuse_ordinary_unhidden(&self, canonical: &str) -> Result<Option<PathBuf>, String> {
        let Some((prefix, _name)) = canonical.rsplit_once('+') else {
            return Ok(None);
        };
        let runtime = self.direct_runtime()?;
        let Some(id) = runtime.catalog.ordinary_routes.get(prefix) else {
            return Ok(None);
        };
        let Some(prepared) = runtime.ordinary.get(id).and_then(|s| s.ready()) else {
            return Ok(None);
        };
        let (_, repository) = self.ordinary_repository(canonical)?;
        if repository.definition.local {
            return Ok(None);
        }
        // Only the per-repo spec digest is cached (small strings,
        // review R1); framed provenance bytes are built on demand on
        // the execute path, never retained (perf follow-up 2).
        let spec_key = prepared.spec_key(
            self,
            &repository.canonical_name,
            &repository.definition.environ,
        )?;
        let workspace = crate::observed_fs::canonicalize(self.main_root())
            .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
        let workspace_text = crate::repository_warm::workspace_text(&workspace);
        let validating = self.clone();
        let reused = crate::repository_store::try_reuse_in(
            self.source_cache_path()?,
            &repository.canonical_name,
            &spec_key,
            Some(&workspace_text),
            move |tree, recorded| {
                crate::repository_warm::unchanged(&validating, &workspace, tree, recorded)
            },
        )?;
        if let Some(root) = &reused {
            self.record_publication(
                canonical,
                crate::repository_publication::Origin::ExtensionV3,
                &spec_key,
                root,
            );
        }
        Ok(reused)
    }

    fn ordinary_root(&self, canonical: &str) -> Result<&Path, String> {
        let (prepared, repository) = self.ordinary_repository(canonical)?;
        let runtime = self.direct_runtime()?;
        repository
            .root
            .get_or_probe_obtaining(
                self.state.runtime_id,
                &format!("repository:{canonical}"),
                || UnitRoots::during(|| self.try_reuse_ordinary(canonical)),
                || {
                    if std::env::var_os("BAZEL_RS_DEBUG_PROVENANCE").is_some() {
                        eprintln!(
                            "PROVENANCE-ORDINARY {} {}",
                            repository.canonical_name,
                            String::from_utf8_lossy(prepared.provenance()?)
                        );
                    }
                    // Executed already by a speculation (parallel fetching,
                    // crate::repository_speculation): committed here, where
                    // the sequential engine executes the rule.
                    if !repository.definition.local
                        && let Some(outcome) = self.state.speculation.take(canonical)
                    {
                        return Ok(self.commit_speculation(*outcome));
                    }
                    self.materialize_ordinary(prepared, repository)
                },
            )
            .inspect(|_| runtime.note_obtained(canonical, &repository.root))
            .map(PathBuf::as_path)
    }

    /// Execute (or reuse) the extension-generated repository `repository`
    /// of the prepared group `prepared`: the evaluation of its root slot,
    /// on the coordinator or — non-local rules only — on a speculation
    /// worker ([`Self::speculation_job`]).
    fn materialize_ordinary(
        &self,
        prepared: &PreparedOrdinary,
        repository: &OrdinaryRepository,
    ) -> Result<PathBuf, String> {
        let (label, _) = repository
            .definition
            .identity
            .split_once('%')
            .ok_or_else(|| {
                format!(
                    "invalid repository rule identity {:?}",
                    repository.definition.identity
                )
            })?;
        let definition_label = Label::parse_canonical(label)?;
        // The spec digest is cached on the prepared group
        // (review R1); framed provenance bytes are built on
        // demand, unretained, on the execute path only
        // (materialize_lazy; perf follow-up 2 — no per-repo
        // ~31 MB cache).
        let canonical_name = repository.canonical_name.clone();
        let spec_key = {
            let _m = crate::repository_store::timing::mark(
                &crate::repository_store::timing::PHASES.spec_serialize,
            );
            prepared.spec_key(
                self,
                &repository.canonical_name,
                &repository.definition.environ,
            )?
        };
        crate::repository_store::timing::count_spec(spec_key.len() as u64, 0);
        let workspace = crate::observed_fs::canonicalize(self.main_root())
            .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
        let workspace_text = crate::repository_warm::workspace_text(&workspace);
        let this = Arc::new(self.clone());
        let validating = Arc::new(self.clone());
        // Local rules never reuse across invocations here either
        // (shouldUseCachedRepoContents; see the direct path).
        let local = repository.definition.local;
        let provenance = move || prepared.framed_provenance(&canonical_name);
        // A local execution's observations for its replay record.
        let observed = std::cell::RefCell::new(None);
        let observed_by = &observed;
        let definition_label_ref = &definition_label;
        let execute = move |stage: &Path| {
            let started = std::time::Instant::now();
            let pinning = if local { this.replay_scope() } else { None };
            let executed = crate::repository_ctx::execute(
                this.clone(),
                &repository.definition,
                &prepared.loaded.functions,
                &repository.attributes,
                &repository.canonical_name,
                &repository.original_name,
                definition_label_ref,
                stage,
            )?;
            drop(pinning);
            speculation_order_check()?;
            if local {
                *observed_by.borrow_mut() =
                    Some(LocalObservation::of(&executed, started.elapsed()));
            } else {
                taint_printed_output(&repository.canonical_name, &executed);
            }
            let inputs =
                crate::host_os::repository_inputs(this.host()?.as_ref(), &executed.inputs)?;
            let recorded = crate::repository_warm::record(&executed.inputs, &executed.watch_names)?;
            Ok(crate::repository_store::Executed { inputs, recorded })
        };
        if local {
            let (root, execution) = crate::repository_store::materialize_local(
                self.source_cache_path()?,
                &repository.canonical_name,
                &spec_key,
                provenance,
                execute,
            )?;
            // A local rule executes on every run (PERF-REPO).
            crate::repository_store::timing::record_local_execution(&repository.canonical_name);
            self.record_local_execution(
                &repository.canonical_name,
                crate::repository_publication::Origin::ExtensionV3,
                &spec_key,
                &repository.original_name,
                || {
                    Ok(crate::snapshot::replay::DefinitionSource::Ordinary {
                        rule_id: crate::lockfile_extensions::repo_rule_id(
                            &repository.definition.identity,
                        )?,
                    })
                },
                &repository.definition,
                &definition_label,
                &repository.attributes,
                &root,
                execution,
                observed.into_inner(),
            );
            return Ok(root);
        }
        crate::repository_store::materialize_lazy_in(
            self.source_cache_path()?,
            &repository.canonical_name,
            &spec_key,
            Some(&workspace_text),
            provenance,
            move |tree, recorded| {
                crate::repository_warm::unchanged(&validating, &workspace, tree, recorded)
            },
            execute,
        )
        .map(|(root, _)| {
            self.record_publication(
                &repository.canonical_name,
                crate::repository_publication::Origin::ExtensionV3,
                &spec_key,
                &root,
            );
            root
        })
    }

    fn record_publication(
        &self,
        canonical: &str,
        origin: crate::repository_publication::Origin,
        spec: &str,
        root: &Path,
    ) {
        if crate::observed_fs::recording()
            && let Ok(cache) = self.source_cache_path()
            && let Some(receipt) =
                crate::repository_store::selection_receipt(cache, canonical, origin, spec, root)
        {
            crate::observed_fs::publication_selection(receipt);
        }
    }

    /// Select exactly the mapping parsing needs, without demanding an extension
    /// catalog for base aliases or a module graph for canonical/local labels.
    /// `None` is the local fast path (including the main repository's @// alias).
    fn mapping_for_label(
        &self,
        text: &str,
        repository: &str,
    ) -> Result<Option<&BTreeMap<String, String>>, String> {
        // Apparent aliases require the owning module's mapping. Only the main
        // repository has the empty alias: external @// is NOT self or main.
        let main_empty_alias = repository.is_empty() && text.starts_with("@//");
        if let Some(pinned) = self.pinned_tables() {
            if !text.starts_with('@') || text.starts_with("@@") || main_empty_alias {
                return Ok(None);
            }
            // The complete mapping resolves every visible apparent name to
            // what the base/import selection below resolves it to (it is
            // the base mapping plus the active non-isolated imports), and
            // fails like it for the others; isolated imports are pinned.
            let entry = pinned.mappings.get(repository).ok_or_else(|| {
                format!("the repository mapping of @@{repository} is {NOT_PINNED}")
            })?;
            let apparent = text[1..]
                .split_once("//")
                .map_or(&text[1..], |(name, _)| name);
            if entry.isolated.contains(apparent) {
                return Err("isolated module extensions unsupported".to_owned());
            }
            return Ok(Some(&entry.mapping));
        }
        if text.starts_with('@') && !text.starts_with("@@") && !main_empty_alias {
            let graph = self.resolve_graph()?;
            let owner = if repository.is_empty() {
                Some((&graph.root, &graph.root_mapping))
            } else {
                graph
                    .modules
                    .get(repository)
                    .map(|module| (&module.module, &module.mapping))
            };
            if let Some((module, base_mapping)) = owner {
                let apparent = text[1..]
                    .split_once("//")
                    .map_or(&text[1..], |(name, _)| name);
                if base_mapping.contains_key(apparent) {
                    self.pin_complete_mapping(repository);
                    return Ok(Some(base_mapping));
                }
                let imported_usage = module.extension_usages.iter().find(|usage| {
                    usage.imports.iter().any(|import| {
                        import.local_name == apparent
                            && !(import.dev_dependency
                                && (!repository.is_empty()
                                    || self.state.options.ignore_dev_dependencies))
                    })
                });
                // Selected-module base aliases remain independent of extension
                // catalog allocation. Only an active generated import needs it.
                if imported_usage.is_some_and(|usage| usage.identity.isolation.is_some()) {
                    return Err("isolated module extensions unsupported".to_owned());
                }
                let supported_generated_import = imported_usage.is_some();
                if !supported_generated_import {
                    self.pin_complete_mapping(repository);
                    return Ok(Some(base_mapping));
                }
            }
            self.mapping(repository).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Inside a replay scope, pin `repository`'s complete mapping when this
    /// parse used its module's base mapping (a pinned set parses through the
    /// complete one), if the catalog exists already (never built for this).
    fn pin_complete_mapping(&self, repository: &str) {
        if self.capturing().is_some() && matches!(self.state.direct.get(), Some(Ok(_))) {
            let _ = self.mapping(repository);
        }
    }

    pub(crate) fn resolution_size_bound(
        &self,
        text: &str,
        repository: &str,
        package: &str,
    ) -> Result<usize, String> {
        let empty = BTreeMap::new();
        let mapping = self.mapping_for_label(text, repository)?.unwrap_or(&empty);
        Ok(Label::resolution_size_bound(
            text, repository, package, mapping,
        ))
    }

    pub fn parse(&self, text: &str, repository: &str, package: &str) -> Result<Label, String> {
        if let Some(mapping) = self.mapping_for_label(text, repository)? {
            Label::parse_in(text, repository, package, mapping)
        } else {
            let mapping = if repository.is_empty() && text.starts_with("@//") {
                BTreeMap::from([(String::new(), String::new())])
            } else {
                BTreeMap::new()
            };
            Label::parse_in(text, repository, package, &mapping)
        }
    }

    /// Label.parseWithPackageContext (cmdline/Label.java:177-238): an
    /// apparent repository the context repository does not see is NOT an
    /// error but an unresolvable repository name
    /// (RepositoryMapping.get, cmdline/RepositoryMapping.java:133-140) with
    /// SpellChecker.didYouMean over the context repository's mapping keys;
    /// only a later fetch of that repository fails
    /// (crate::label::unresolvable_package_message). Every other error is
    /// returned unchanged.
    pub fn parse_deferred(
        &self,
        text: &str,
        repository: &str,
        package: &str,
    ) -> Result<Label, String> {
        match self.parse(text, repository, package) {
            Err(error) if error.starts_with("unknown apparent repository") => {
                let name = crate::bazel_label::Parts::parse(text)
                    .ok()
                    .and_then(|parts| parts.repo.filter(|_| !parts.repo_is_canonical))
                    .unwrap_or_default();
                let suggestion = self.unknown_repository_suggestion(name, repository);
                crate::label::deferred_label(text, repository, &suggestion).ok_or(error)
            }
            other => other,
        }
    }

    /// SpellChecker.didYouMean(name, mapping.entries().keySet()) for the
    /// context repository's complete mapping. Bazel iterates the mapping in
    /// insertion order and keeps the first of equally close candidates;
    /// native mappings are sorted maps, so a tie between equally distant
    /// names picks the lexicographically first (docs/EXTENSION_REPO_MAPPING.md).
    pub fn unknown_repository_suggestion(&self, name: &str, repository: &str) -> String {
        let keys = match self.mapping(repository) {
            Ok(mapping) => mapping.keys().map(String::as_str).collect::<Vec<_>>(),
            Err(_) => return String::new(),
        };
        crate::native_rules::did_you_mean(name, &keys)
    }
}

pub use crate::module_root_warm::Scope as RootWarmingScope;

/// [`registry_source_root`] ahead of the demand (crate::module_root_warm):
/// archive sources only, the registry files peeked (never recorded).
#[allow(clippy::too_many_arguments)]
fn warm_registry_source_root(
    access: &crate::registry_session::SessionHandle,
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    cache: &Path,
    local_patches: &LocalPatchPlan,
    downloads_enabled: bool,
) -> Option<PathBuf> {
    let source = access.peek_object(registry, &format!("modules/{name}/{version}/source.json"))?;
    match source.get("type") {
        None => {}
        Some(serde_json::Value::String(kind)) if kind == "archive" => {}
        Some(_) => return None,
    }
    let config = access.peek_object(registry, "bazel_registry.json");
    crate::materializer::warm_with_registry_config(
        registry,
        name,
        version,
        module_file,
        &source,
        cache,
        local_patches,
        downloads_enabled,
        config,
    )
    .ok()
}

#[allow(clippy::too_many_arguments)]
fn registry_source_root(
    access: &crate::registry_session::SessionHandle,
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    cache: &Path,
    local_patches: &LocalPatchPlan,
    downloads_enabled: bool,
) -> Result<PathBuf, String> {
    let relative = format!("modules/{name}/{version}/source.json");
    let descriptor = registry.location(&relative);
    let source = access
        .read_object(registry, &relative)?
        .ok_or_else(|| format!("missing registry source descriptor {descriptor}"))?;
    if access.publication_proof()
        && source
            .get("type")
            .is_none_or(|kind| kind.as_str() == Some("archive"))
    {
        return crate::materializer::probe_with_registry_config(
            registry,
            name,
            version,
            module_file,
            &source,
            cache,
            local_patches,
            access.read_object(registry, "bazel_registry.json")?,
        );
    }
    match source.get("type") {
        Some(serde_json::Value::String(kind)) if kind == "local_path" => {}
        None => {
            return crate::materializer::materialize_with_registry_config(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
                downloads_enabled,
                access.read_object(registry, "bazel_registry.json")?,
            );
        }
        Some(serde_json::Value::String(kind)) if kind == "archive" => {
            return crate::materializer::materialize_with_registry_config(
                registry,
                name,
                version,
                module_file,
                &source,
                cache,
                local_patches,
                downloads_enabled,
                access.read_object(registry, "bazel_registry.json")?,
            );
        }
        Some(kind) => return Err(format!("unsupported source type {kind} in {descriptor}")),
    }
    if local_patches.has_transformations() {
        return Err(format!(
            "single_version_override patches and patch_cmds are unsupported for local_path source in {descriptor}; live source checkout is not modified"
        ));
    }
    for key in source.keys() {
        if !matches!(key.as_str(), "type" | "path") {
            return Err(format!(
                "unsupported local_path source field {key:?} in {descriptor}; patches and other source transformations are not implemented"
            ));
        }
    }
    let path = source
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!("local_path source requires a nonempty string path in {descriptor}")
        })?;
    if path.is_empty() || path.contains('\0') {
        return Err(format!(
            "invalid local_path path in {descriptor}: must be nonempty and contain no NUL"
        ));
    }
    let path = PathBuf::from(path);
    let chosen = if path.is_absolute() {
        path
    } else {
        let config_path = registry.location("bazel_registry.json");
        let config = access.read_object(registry, "bazel_registry.json").map_err(|error| {
            format!("relative local_path source requires registry module_base_path: {error}")
        })?.ok_or_else(|| format!("relative local_path source requires registry module_base_path: missing {config_path}"))?;
        let base = config
            .get("module_base_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "relative local_path source requires string module_base_path in {config_path}"
                )
            })?;
        if base.contains('\0') {
            return Err(format!(
                "invalid module_base_path in {config_path}: NUL is not allowed"
            ));
        }
        // IndexRegistry.getLocalPathRepoSpec resolves a relative base against
        // the selected registry, not the workspace or source descriptor folder.
        let base = PathBuf::from(base);
        if base.is_absolute() {
            base.join(path)
        } else if let Some(registry) = registry.local_path() {
            registry.join(base).join(path)
        } else {
            return Err(format!(
                "relative module_base_path is unsupported for remote registry {registry}; local_path sources require an absolute path or absolute module_base_path"
            ));
        }
    };
    let root = crate::observed_fs::canonicalize(&chosen).map_err(|error| {
        format!(
            "cannot resolve registry local_path source {}: {error}",
            chosen.display()
        )
    })?;
    if access.publication_proof() {
        let cache = crate::observed_fs::canonicalize(cache).unwrap_or_else(|_| cache.to_path_buf());
        if root.starts_with(&cache) {
            return Err("registry local_path requires an unproved derived root".into());
        }
    }
    if !crate::observed_fs::is_dir(&root) {
        return Err(format!(
            "registry local_path source is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

/// The lockfile view of one module extension evaluation
/// (SingleExtensionEvalFunction.java:144-213).
struct ExtensionLock {
    mode: crate::lockfile::LockfileMode,
    /// GsonTypeAdapterUtil's ModuleExtensionId spelling (`//:ext.bzl%ext`).
    id: String,
    /// ModuleExtensionId.toString (`@@//:ext.bzl%ext`), used in messages.
    display_id: String,
    factors: String,
    bzl_transitive_digest: String,
    usages_digest: String,
    /// The facts passed to the extension: the workspace lockfile's when
    /// present (and of the current facts_version), else the hidden lockfile's.
    facts: Vec<(String, crate::lockfile::Json)>,
    /// The workspace lockfile's facts, for error mode (:271-282).
    workspace_facts: Vec<(String, crate::lockfile::Json)>,
    /// The hidden lockfile's facts of the current facts_version, if any.
    hidden_facts: Option<Vec<(String, crate::lockfile::Json)>>,
    facts_version: i64,
    /// The entry for these factors: the workspace lockfile's, else the hidden one's.
    locked: Option<crate::lockfile::Json>,
    /// RepoRecordedInput.EnvVar.wrap of module_extension(environ=...),
    /// sorted by name (ModuleExtensionContext.java:91-93).
    static_env: Vec<crate::lockfile_extensions::RecordedInput>,
}

impl RepositorySet {
    /// The lockfile state of `group` in this invocation; None without a
    /// lockfile (`--lockfile_mode=off` or no registry session).
    fn extension_lock(
        &self,
        group: &repository_catalog::OrdinaryGroup,
        definition: &crate::module_extension::ExtensionDefinition,
        loaded: &loading::LoadedDefinition,
        factors: String,
    ) -> Result<Option<ExtensionLock>, String> {
        use crate::lockfile::Json;
        use crate::lockfile_extensions as lx;
        let Some(session) = self.state.options.registry_session.session() else {
            return Ok(None);
        };
        let mode = session.mode();
        if mode == crate::lockfile::LockfileMode::Off {
            return Ok(None);
        }
        let id = format!("{}%{}", group.label, group.symbol);
        let display_id = format!("{}%{}", group.label.canonical_starlark(), group.symbol);
        let context = |error: String| format!("module extension {display_id}: {error}");
        let facts_version = i64::from(definition.facts_version);
        let workspace = session.lockfile();
        let hidden = session.hidden_lockfile();
        let facts_of = |lockfile: &crate::lockfile::Lockfile| {
            lockfile
                .facts
                .iter()
                .find(|(key, _)| key == &id)
                .map(|(_, value)| match value {
                    Json::Object(members) => members.clone(),
                    _ => Vec::new(),
                })
        };
        let hidden_facts = facts_of(hidden)
            .filter(|_| lx::facts_version_of(&hidden.facts_versions, &id) == facts_version);
        let mut workspace_facts = Vec::new();
        let mut facts = Vec::new();
        if let Some(found) = facts_of(workspace) {
            if lx::facts_version_of(&workspace.facts_versions, &id) == facts_version {
                workspace_facts = found.clone();
                facts = found;
            }
        } else if let Some(found) = &hidden_facts {
            facts = found.clone();
        }
        let entry_of = |lockfile: &crate::lockfile::Lockfile| {
            lockfile
                .module_extensions
                .iter()
                .find(|(key, _)| key == &id)
                .and_then(|(_, per_factors)| lx::member(per_factors, &factors))
                .cloned()
        };
        let locked = entry_of(workspace).or_else(|| entry_of(hidden));
        let mut names: Vec<&String> = definition.environ.names().iter().collect();
        names.sort_by(|a, b| crate::lockfile::java_string_cmp(a, b));
        names.dedup();
        let mut static_env = Vec::new();
        for name in names {
            let value = self
                .environment()
                .lookup(name)
                .map_err(|error| context(error.to_string()))?;
            static_env.push(lx::RecordedInput::env(name, value));
        }
        let usages = lx::usages_value(group).map_err(context)?;
        Ok(Some(ExtensionLock {
            mode,
            bzl_transitive_digest: lx::bzl_transitive_digest(
                &group.label,
                &loaded.load_graph,
                &loaded.sources,
            )
            .map_err(context)?,
            usages_digest: lx::usages_digest(&usages),
            id,
            display_id,
            factors,
            facts,
            workspace_facts,
            hidden_facts,
            facts_version,
            locked,
            static_env,
        }))
    }

    /// SingleExtensionEvalFunction.tryGettingValueFromLockFile (:331-407):
    /// the locked entry if it is up to date. In error mode an outdated
    /// non-reproducible entry is Bazel's error with every detected reason.
    fn reusable_lock_entry(
        &self,
        lock: &ExtensionLock,
    ) -> Result<Option<crate::lockfile::Json>, String> {
        use crate::lockfile::Json;
        use crate::lockfile_extensions as lx;
        let Some(entry) = &lock.locked else {
            return Ok(None);
        };
        let all = lock.mode == crate::lockfile::LockfileMode::Error;
        let id = &lock.display_id;
        let mut diffs = Vec::new();
        let string = |name: &str| match lx::member(entry, name) {
            Some(Json::String(text)) => Some(text.as_str()),
            _ => None,
        };
        if string("bzlTransitiveDigest") != Some(lock.bzl_transitive_digest.as_str()) {
            diffs.push(format!(
                "the implementation of the extension '{id}' or one of its transitive .bzl files has changed"
            ));
        }
        if (all || diffs.is_empty()) && string("usagesDigest") != Some(lock.usages_digest.as_str())
        {
            diffs.push(format!("the usages of the extension '{id}' have changed"));
        }
        if all || diffs.is_empty() {
            let inputs = match lx::member(entry, "recordedInputs") {
                Some(Json::Array(inputs)) => inputs.clone(),
                _ => Vec::new(),
            };
            if let Some(reason) = lx::outdated_input(&inputs, &LockInputProbe(self)) {
                diffs.push(format!(
                    "an input to the extension '{id}' changed: {reason}"
                ));
            }
        }
        let reproducible = lx::entry_is_reproducible(entry);
        if (all || diffs.is_empty())
            && reproducible
            && !lock.facts.is_empty()
            && Some(&lock.facts) != lock.hidden_facts.as_ref()
        {
            diffs.push(format!(
                "the facts recorded in the lockfile for the extension '{id}' do not match the result of its most recent evaluation"
            ));
        }
        if diffs.is_empty() {
            return Ok(Some(entry.clone()));
        }
        if all && !reproducible {
            return Err(lx::outdated_message(&diffs.join(",")));
        }
        Ok(None)
    }

    /// The repository calls of a locked entry's generatedRepoSpecs: each
    /// RepoRuleId's rule loaded from its .bzl file (BzlmodRepoRuleCreator),
    /// attributes deserialized like AttributeValuesAdapter.read.
    fn repositories_from_lock_entry(
        &self,
        entry: &crate::lockfile::Json,
    ) -> Result<
        (
            Vec<crate::extension_calls::RecordedRepository>,
            Vec<starlark::values::OwnedFrozenValue>,
        ),
        String,
    > {
        use crate::extension_calls::{RecordedRepository, RecordedValue};
        use crate::lockfile::Json;
        use crate::lockfile_extensions as lx;
        let Some(Json::Object(specs)) = lx::member(entry, "generatedRepoSpecs") else {
            return Err("MODULE.bazel.lock entry without generatedRepoSpecs".to_owned());
        };
        let mut definitions: BTreeMap<String, repository_rule::RuleDefinition> = BTreeMap::new();
        let mut functions = Vec::new();
        let mut repositories = Vec::new();
        for (name, spec) in specs {
            let Some(Json::String(rule_id)) = lx::member(spec, "repoRuleId") else {
                return Err(format!(
                    "MODULE.bazel.lock repo spec {name:?} without a repoRuleId"
                ));
            };
            if !definitions.contains_key(rule_id) {
                let (label, rule) = lx::parse_repo_rule_id(rule_id)?;
                // A local rule's replay loads it the same way from pinned
                // roots and mappings (crate::snapshot::replay).
                let pinning = self.replay_scope();
                let loaded = loading::load_shared_definition_module_in(
                    Arc::new(self.clone()),
                    &label,
                    &rule,
                    false,
                )?;
                drop(pinning);
                let definition = repository_rule::unpack(&loaded.value)
                    .map_err(|error| format!("repoRuleId {rule_id}: {error}"))?;
                functions.extend(loaded.functions);
                definitions.insert(rule_id.clone(), definition);
            }
            let mut attributes = BTreeMap::new();
            let mut order = vec!["name".to_owned()];
            attributes.insert("name".to_owned(), RecordedValue::String(name.clone()));
            if let Some(Json::Object(members)) = lx::member(spec, "attributes") {
                for (attribute, value) in members {
                    attributes.insert(attribute.clone(), lx::recorded_value(value)?);
                    order.push(attribute.clone());
                }
            }
            repositories.push(RecordedRepository {
                definition: definitions[rule_id].clone(),
                attributes,
                name: name.clone(),
                order,
            });
        }
        Ok((repositories, functions))
    }

    /// After a SingleExtensionValue exists: Bazel's error-mode checks for an
    /// evaluated extension (:237-282) and the result BazelLockFileModule
    /// records (:117-136, LockFileModuleExtension.WithFactors).
    fn record_extension_lock(
        &self,
        lock: &ExtensionLock,
        reused: Option<&crate::lockfile::Json>,
        execution: Option<&crate::extension_ctx::ExtensionExecution>,
        loaded: &loading::LoadedDefinition,
        metadata: Option<&crate::extension_metadata::ExtensionMetadata>,
        specs: Vec<(String, crate::lockfile::Json)>,
    ) -> Result<(), String> {
        if self.state.publication_proof {
            return if reused.is_some() && execution.is_none() {
                Ok(())
            } else {
                Err("publication proof cannot record an evaluated extension".into())
            };
        }
        use crate::lockfile::{Json, LockfileMode};
        use crate::lockfile_extensions as lx;
        let session = self
            .state
            .options
            .registry_session
            .session()
            .ok_or("internal error: lockfile without a registry session")?;
        if let Some(entry) = reused {
            if lock.mode.writes() {
                session.record_extension_result(lx::ExtensionResult {
                    id: lock.id.clone(),
                    factors: lock.factors.clone(),
                    entry: entry.clone(),
                    facts: Json::Object(lock.facts.clone()),
                    facts_version: lock.facts_version,
                });
            }
            return Ok(());
        }
        let execution = execution.ok_or("internal error: neither reused nor evaluated")?;
        // The ModuleExtensionContext's recorded inputs: static environment,
        // static repository mappings (rows by repository name, columns by
        // apparent name; ImmutableTable in Label.SimpleRepoMappingRecorder,
        // cmdline/Label.java:266-281), then observations in order.
        let mut table: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
        for entry in loaded
            .repo_mappings
            .iter()
            .chain(&execution.tag_repo_mappings)
        {
            table.insert(
                (entry.from.clone(), entry.apparent.clone()),
                entry.canonical.clone(),
            );
        }
        let mut static_mappings: Vec<_> = table.into_iter().collect();
        static_mappings.sort_by(|((a_from, a_name), _), ((b_from, b_name), _)| {
            crate::lockfile::java_string_cmp(a_from, b_from)
                .then_with(|| crate::lockfile::java_string_cmp(a_name, b_name))
        });
        let mut recorded = Vec::new();
        for input in &lock.static_env {
            lx::push_recorded(&mut recorded, input.clone());
        }
        for ((from, apparent), canonical) in static_mappings {
            lx::push_recorded(
                &mut recorded,
                lx::RecordedInput::repo_mapping(&from, &apparent, canonical),
            );
        }
        for input in &execution.inputs.bazel {
            lx::push_recorded(&mut recorded, input.clone());
        }
        // SingleExtensionEvalFunction.java:237-257.
        let invisible: Vec<String> = recorded
            .iter()
            .filter(|input| input.key.starts_with("REPO_MAPPING:") && input.value.is_none())
            .filter_map(|input| {
                lx::unescape(&input.key["REPO_MAPPING:".len()..])
                    .and_then(|key| key.split_once(',').map(|(_, name)| format!("@{name}")))
            })
            .collect();
        if !invisible.is_empty() {
            crate::note!(
                "WARNING: The module extension {} produced an invalid lockfile entry because it referenced {}. Please report this issue to its maintainers.",
                lock.display_id,
                invisible.join(", ")
            );
        }
        let reproducible = metadata.is_some_and(|metadata| metadata.reproducible);
        if lock.mode == LockfileMode::Error && !reproducible {
            return Err(format!(
                "The module extension '{}'{} does not exist in the lockfile",
                lock.display_id,
                if lock.factors == "general" {
                    String::new()
                } else {
                    format!(" for platform {}", lock.factors)
                }
            ));
        }
        let new_facts = match metadata.and_then(|metadata| metadata.facts.as_deref()) {
            Some(text) => match crate::lockfile::parse_json(text) {
                Ok(Json::Object(members)) => members,
                _ => return Err(format!("internal error: invalid facts encoding {text}")),
            },
            None => Vec::new(),
        };
        if lock.mode == LockfileMode::Error
            && Json::Object(new_facts.clone()) != Json::Object(lock.workspace_facts.clone())
        {
            return Err(lx::outdated_message(&format!(
                "the extension '{}' has changed its facts: {} != {}",
                lock.display_id,
                facts_repr(&new_facts)?,
                facts_repr(&lock.workspace_facts)?
            )));
        }
        if !lock.mode.writes() {
            return Ok(());
        }
        if let Some(reason) = &execution.inputs.bazel_unsupported {
            return Err(format!(
                "module extension {}: cannot write its MODULE.bazel.lock entry: {reason} (use --lockfile_mode=off)",
                lock.display_id
            ));
        }
        let entry = lx::entry_json(
            &lock.bzl_transitive_digest,
            &lock.usages_digest,
            &recorded,
            specs,
            lx::metadata_json(metadata),
        );
        session.record_extension_result(lx::ExtensionResult {
            id: lock.id.clone(),
            factors: lock.factors.clone(),
            entry,
            facts: Json::Object(new_facts),
            facts_version: lock.facts_version,
        });
        Ok(())
    }
}

/// Starlark.repr of a Facts value (SingleExtensionEvalFunction.java:275-280).
fn facts_repr(members: &[(String, crate::lockfile::Json)]) -> Result<String, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        crate::extension_ctx::alloc_json(
            module.heap(),
            &crate::lockfile::Json::Object(members.to_vec()),
        )
        .map(|value| value.to_repr())
        .map_err(|error| error.to_string())
    })
}

/// The core of [`RepositorySet::friendly_repository`] over explicit roots:
/// the name of the longest root containing `path` — each root tried as
/// given and as `canonical` resolves it — and `path` relative to that root.
/// Equally long roots resolve to the smallest name.
fn longest_root(
    roots: impl IntoIterator<Item = (String, PathBuf)>,
    mut canonical: impl FnMut(&Path) -> Option<PathBuf>,
    path: &Path,
) -> Option<(String, PathBuf)> {
    let mut best: Option<(usize, String, PathBuf)> = None;
    for (name, root) in roots {
        let resolved = canonical(&root);
        for candidate in std::iter::once(root).chain(resolved) {
            let Ok(relative) = path.strip_prefix(&candidate) else {
                continue;
            };
            let length = candidate.as_os_str().len();
            if best.as_ref().is_none_or(|(longest, smallest, _)| {
                length > *longest || (length == *longest && name < *smallest)
            }) {
                best = Some((length, name.clone(), relative.to_path_buf()));
            }
        }
    }
    best.map(|(_, name, relative)| (name, relative))
}

/// Current values of recorded inputs (RepoRecordedInput.getValue).
struct LockInputProbe<'a>(&'a RepositorySet);

impl LockInputProbe<'_> {
    fn host_path(
        &self,
        path: &crate::lockfile_extensions::FriendlyPath,
    ) -> Result<PathBuf, String> {
        if self.0.state.publication_proof {
            let repository = path
                .repository
                .as_deref()
                .ok_or("absolute extension inputs are unsupported in publication proof")?;
            let root = crate::observed_fs::canonicalize(self.0.root(repository)?)
                .map_err(|e| e.to_string())?;
            if !crate::repository_warm::proof_path(
                &root,
                &path.path,
                repository.is_empty().then_some(self.0.source_cache_path()?),
            ) {
                return Err("publication proof requires an unsupported extension input".into());
            }
        }
        Ok(match path.repository.as_deref() {
            None => PathBuf::from(&path.path),
            Some("") => self.0.main_root().join(&path.path),
            Some(repository) => self.0.root(repository)?.join(&path.path),
        })
    }

    /// Why a workspace-relative (or absolute) key naming the repository
    /// store is outdated (review i20). Before keys were
    /// RepoCacheFriendlyPaths, with the cache inside the workspace (the
    /// default layout) a module extension's read of another repository's
    /// file was keyed as a path into that repository's content-addressed
    /// store tree through the workspace, `@@//bazel-rs-out/repositories/...`.
    /// Store trees are never rewritten, so such a key kept validating after
    /// the repository was fetched anew and the extension stayed stale.
    /// Re-evaluating once in update mode records `@@NAME//path` keys instead
    /// (error mode writes no lockfile: docs/REPOSITORY_RULES.md, upgrade
    /// note), and since `friendly_repository` never keys a store path to the
    /// workspace, a re-evaluation never records this kind again. With the
    /// cache outside the workspace the keys were already `@@NAME//path`, and
    /// no bazel-rs has written an absolute key into a lockfile (a path in no
    /// repository fails the write: `Context::record_bazel_file`), so the
    /// absolute branch is defensive, for keys written by hand or by another
    /// tool. Keys of a named repository resolve through its current root and
    /// are never affected.
    fn store_key(&self, path: &crate::lockfile_extensions::FriendlyPath) -> Option<String> {
        let host = match path.repository.as_deref() {
            None => PathBuf::from(&path.path),
            Some("") => self.0.main_root().join(&path.path),
            Some(_) => return None,
        };
        self.0.in_repository_store(&host).then(|| {
            format!(
                "{path} is a path into the native repository store (recorded by an older bazel-rs)"
            )
        })
    }
}

impl crate::lockfile_extensions::InputProbe for LockInputProbe<'_> {
    fn file(&self, path: &crate::lockfile_extensions::FriendlyPath) -> Result<String, String> {
        if let Some(reason) = self.store_key(path) {
            return Err(reason);
        }
        let host = self
            .host_path(path)
            .map_err(|error| format!("failed to stat {path}: {error}"))?;
        match crate::observed_fs::metadata(&host) {
            Ok(metadata) if metadata.is_dir() => Ok("DIR".to_owned()),
            Ok(metadata) if metadata.is_file() => {
                let bytes = crate::observed_fs::read(&host)
                    .map_err(|error| format!("failed to stat {path}: {error}"))?;
                Ok(
                    sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&bytes))
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                )
            }
            Ok(_) => Err(format!(
                "failed to stat {path}: special files are not supported by the native profile"
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok("ENOENT".to_owned())
            }
            Err(error) => Err(format!("failed to stat {path}: {error}")),
        }
    }

    fn dirents(&self, path: &crate::lockfile_extensions::FriendlyPath) -> Result<String, String> {
        use std::os::unix::ffi::OsStrExt as _;
        if let Some(reason) = self.store_key(path) {
            return Err(reason);
        }
        let host = self.host_path(path)?;
        let names =
            crate::observed_fs::read_dir_projected(&host, &crate::observed_fs::Projection::All)
                .and_then(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| entry.map(|entry| entry.file_name().as_bytes().to_vec()))
                        .collect::<Result<Vec<_>, _>>()
                })
                .map_err(|error| format!("failed to list {path}: {error}"))?;
        Ok(crate::lockfile_extensions::dirents_marker_value(&names))
    }

    fn env(&self, name: &str) -> Result<Option<String>, String> {
        self.0
            .environment()
            .lookup(name)
            .map_err(|error| error.to_string())
    }

    fn mapping(&self, from: &str, apparent: &str) -> Result<Option<String>, String> {
        Ok(self.0.mapping(from).ok().map(|mapping| {
            mapping
                .get(apparent)
                .cloned()
                .unwrap_or_else(|| apparent.to_owned())
        }))
    }
}

/// A module repository's materialization slot, honoring the no-demand
/// guard (crate::repository_demand): a thread that must not fetch sees the
/// [`crate::repository_demand::NEEDS_REPOSITORY`] sentinel instead of
/// running the initializer; a materialized repository is returned as
/// usual. The coordinator re-runs the aborted unit inline at its commit
/// slot, where fetching is allowed (review §1.2).
fn guarded_root_slot<'a>(
    slot: &'a std::sync::OnceLock<Result<PathBuf, String>>,
    repository: &str,
    init: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<&'a Path, String> {
    if let Some(result) = slot.get() {
        return result.as_ref().map(PathBuf::as_path).map_err(Clone::clone);
    }
    if crate::repository_demand::no_demand_active() {
        return Err(format!(
            "{}: repository @@{repository} is unfetched and this thread does not fetch",
            crate::repository_demand::NEEDS_REPOSITORY
        ));
    }
    slot.get_or_init(|| {
        let _span = crate::timeline::span("module-root", || repository.to_owned());
        init()
    })
    .as_ref()
    .map(PathBuf::as_path)
    .map_err(Clone::clone)
}

/// Registration patterns with the repository each is parsed in (module
/// order, `""` for the main repository).
pub(crate) type Registrations = Vec<(String, Vec<String>)>;

fn lock_ignoring_poison<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_budget_counts_serialized_bytes_and_rejects_before_growth() {
        let input = BTreeMap::from([("nul", "\0\0\0"), ("text", "plain")]);
        let expected = serde_json::to_vec(&input).unwrap();
        assert_eq!(
            bounded_provenance(&input, expected.len()).unwrap(),
            expected
        );
        assert!(
            bounded_provenance(&input, expected.len() - 1)
                .unwrap_err()
                .contains("host provenance limit")
        );
        assert!(
            bounded_provenance(&vec![255_u8; 4096], 4096)
                .unwrap_err()
                .contains("host provenance limit")
        );
        assert!(bounded_provenance(&(), 0).is_err());
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = crate::test_temp::root().join(format!(
                "bazel-rs-repository-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn registry(&self) -> RepositorySet {
            self.write(
                "main/MODULE.bazel",
                "module(name='main')\nbazel_dep(name='dep',version='1')\n",
            );
            self.write(
                "registry/modules/dep/1/MODULE.bazel",
                "module(name='dep',version='1')\n",
            );
            RepositorySet::with_options(
                self.0.join("main"),
                ResolverOptions {
                    registries: vec![self.0.join("registry").into()],
                    ..ResolverOptions::default()
                },
            )
        }

        fn source(&self, value: serde_json::Value) {
            self.write("registry/modules/dep/1/source.json", &value.to_string());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn assert_root_module_demand_is_lazy(repos: &RepositorySet) {
        assert!(repos.state.resolved.get().is_none());
        assert!(repos.state.direct.get().is_none());
        assert!(repos.state.registry_roots.get().is_none());
        assert!(repos.state.absolute_source_cache.get().is_none());
    }

    #[test]
    fn root_module_preflight_records_unused_declarations_without_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nbazel_dep(name='not_downloaded',version='1')\nr=use_repo_rule('//:poison.bzl','r')\nr(name='direct')\ne=use_extension('//:poison.bzl','extension')\ne.tag(value='recorded')\nuse_repo(e,'ordinary')\n");
        f.write("poison.bzl", "fail('definition must not load')\n");
        f.write("BUILD", "fail('BUILD must not load')\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                registries: vec![f.0.join("absent-registry").into()],
                ..Default::default()
            },
        );
        repos.validate_root_module().unwrap();
        assert!(repos.state.root_module.get().unwrap().is_ok());
        assert_root_module_demand_is_lazy(&repos);
        assert!(!f.0.join("bazel-rs-out").exists());
        assert!(!f.0.join("absent-registry").exists());
    }

    #[test]
    fn root_module_success_shared_by_clones_preserves_graph_metadata_after_edit() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='before',version='1')\nbazel_dep(name='dep',version='1',repo_name='original')\n");
        repos.validate_root_module().unwrap();
        let cloned = repos.clone();
        f.write("main/MODULE.bazel", "module(name='after',version='2')\n");
        cloned.validate_root_module().unwrap();
        assert!(std::ptr::eq(
            repos.root_module().unwrap(),
            cloned.root_module().unwrap()
        ));
        assert_root_module_demand_is_lazy(&cloned);
        let graph = cloned.resolve_graph().unwrap();
        let info = graph.root.module.as_ref().unwrap();
        assert_eq!((&*info.name, &*info.version), ("before", "1"));
        assert_eq!(graph.root.dependencies.len(), 1);
        assert_eq!(graph.root_mapping["original"], "dep+");
        let ModuleSource::Registry {
            registry,
            module_file,
            ..
        } = &graph.modules["dep+"].source
        else {
            panic!("expected the original selected registry metadata");
        };
        assert_eq!(registry.local_path().unwrap(), f.0.join("registry"));
        assert_eq!(module_file, b"module(name='dep',version='1')\n");
        assert!(std::ptr::eq(graph, repos.resolve_graph().unwrap()));
        let fresh = RepositorySet::with_options(f.0.join("main"), repos.state.options.clone());
        fresh.validate_root_module().unwrap();
        let graph = fresh.resolve_graph().unwrap();
        assert_eq!(graph.root.module.as_ref().unwrap().name, "after");
        assert_eq!(graph.root.module.as_ref().unwrap().version, "2");
        assert!(graph.modules.is_empty());
    }

    #[test]
    fn root_module_language_errors_are_cached_across_clones_not_fresh_sets() {
        let f = Fixture::new();
        for source in [
            "x=RunEnvironmentInfo()",
            "x=UnknownRootSymbol",
            "module(",
            "x=1//0",
        ] {
            f.write("MODULE.bazel", source);
            let expected = crate::bzlmod::evaluate(&f.0.join("MODULE.bazel")).unwrap_err();
            let repos = RepositorySet::new(f.0.clone());
            let error = repos.validate_root_module().unwrap_err();
            assert_eq!(error, expected);
            assert!(error.contains("MODULE.bazel"), "{error}");
            assert_root_module_demand_is_lazy(&repos);
            let cloned = repos.clone();
            f.write("MODULE.bazel", "module(name='fixed')\n");
            assert_eq!(cloned.validate_root_module().unwrap_err(), error);
            assert_eq!(cloned.resolve_graph().unwrap_err(), error);
            let fresh = RepositorySet::new(f.0.clone());
            fresh.validate_root_module().unwrap();
            assert_eq!(
                fresh
                    .resolve_graph()
                    .unwrap()
                    .root
                    .module
                    .as_ref()
                    .unwrap()
                    .name,
                "fixed"
            );
        }
    }

    #[test]
    fn root_module_absence_is_valid_but_graph_demand_keeps_original_diagnostic() {
        let f = Fixture::new();
        let expected = crate::bzlmod::evaluate(&f.0.join("MODULE.bazel")).unwrap_err();
        let repos = RepositorySet::new(f.0.clone());
        repos.validate_root_module().unwrap();
        assert_root_module_demand_is_lazy(&repos);
        let cloned = repos.clone();
        f.write("MODULE.bazel", "module(name='created')\n");
        cloned.validate_root_module().unwrap();
        assert!(std::ptr::eq(
            repos.root_module().unwrap(),
            cloned.root_module().unwrap()
        ));
        assert_eq!(cloned.resolve_graph().unwrap_err(), expected);
        assert_eq!(repos.resolve_graph().unwrap_err(), expected);
        let fresh = RepositorySet::new(f.0.clone());
        fresh.validate_root_module().unwrap();
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "created"
        );
    }

    #[test]
    fn root_module_source_cache_configuration_starts_a_fresh_runtime() {
        let f = Fixture::new();
        let repos = f.registry().with_source_cache("first".into());
        f.source(serde_json::json!({"type":"local_path", "path":f.0}));
        repos.validate_root_module().unwrap();
        repos.mapping("").unwrap();
        repos.root("dep+").unwrap();
        assert!(repos.state.root_module.get().is_some());
        assert!(repos.state.resolved.get().is_some());
        assert!(repos.state.direct.get().is_some());
        assert!(repos.state.registry_roots.get().is_some());
        assert!(repos.state.absolute_source_cache.get().is_some());
        f.write("main/MODULE.bazel", "module(name='fresh')\n");
        let fresh = repos.clone().with_source_cache("second".into());
        assert_ne!(fresh.state.runtime_id, repos.state.runtime_id);
        assert_eq!(fresh.state.options, repos.state.options);
        assert!(fresh.state.root_module.get().is_none());
        assert_root_module_demand_is_lazy(&fresh);
        fresh.validate_root_module().unwrap();
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "fresh"
        );
        assert_eq!(
            repos
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "main"
        );
        assert_eq!(fresh.source_cache_path().unwrap(), f.0.join("main/second"));
        assert_eq!(repos.source_cache_path().unwrap(), f.0.join("main/first"));
    }

    #[test]
    fn root_module_ignore_dev_context_is_recorded_and_configuration_is_isolated() {
        let f = Fixture::new();
        for source in [
            "r=use_repo_rule('//:absent.bzl','r')\nr(name='same')\nr(name='same',dev_dependency=True)\n",
            "local_path_override(module_name='unused',path='one')\nlocal_path_override(module_name='unused',path='two')\n",
        ] {
            f.write("MODULE.bazel", source);
            let strict = RepositorySet::new(f.0.clone());
            assert!(strict.validate_root_module().is_err());
            let ignored = RepositorySet::with_options(
                f.0.clone(),
                ResolverOptions {
                    ignore_dev_dependencies: true,
                    ..Default::default()
                },
            );
            ignored.validate_root_module().unwrap();
            assert_root_module_demand_is_lazy(&ignored);
            let expected = crate::bzlmod::evaluate_with_context(
                &f.0.join("MODULE.bazel"),
                crate::bzlmod::EvaluationContext {
                    role: crate::bzlmod::ModuleRole::Root,
                    ignore_dev_dependencies: true,
                    isolated_extension_usages: false,
                    print_is_noop: false,
                },
            )
            .unwrap();
            assert_eq!(ignored.resolve_graph().unwrap().root, expected);
            assert!(strict.clone().validate_root_module().is_err());
        }
    }

    #[test]
    fn root_module_builtin_profile_is_isolated_and_retained_by_source_cache_configuration() {
        // Both profiles reserve the built-in name (ModuleThreadContext.java:
        // 383-401); only the built-in profile records the implicit edge.
        let implicit = |repos: &RepositorySet| match repos.state.root_module.get() {
            Some(Ok(snapshot)) => snapshot.module_file().unwrap().implicit_dependencies.len(),
            other => panic!("{other:?}"),
        };
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='root')\n");
        let explicit = RepositorySet::new(f.0.clone());
        explicit.validate_root_module().unwrap();
        assert_eq!(implicit(&explicit), 0);
        let builtin = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                builtin_profile: module_graph::BuiltinProfile::Bazel93Builtins,
                ..Default::default()
            },
        );
        builtin.validate_root_module().unwrap();
        assert_eq!(implicit(&builtin), 1);
        f.write("MODULE.bazel", "module(name='bazel_tools')\n");
        builtin.clone().validate_root_module().unwrap();
        let fresh = builtin.with_source_cache("new-cache".into());
        assert_eq!(
            fresh.state.options.builtin_profile,
            module_graph::BuiltinProfile::Bazel93Builtins
        );
        assert!(fresh.state.root_module.get().is_none());
        let error = fresh.validate_root_module().unwrap_err();
        assert!(
            error.contains("already defined as the current module name"),
            "{error}"
        );
        assert_root_module_demand_is_lazy(&fresh);
        assert_eq!(
            RepositorySet::new(f.0.clone())
                .validate_root_module()
                .unwrap_err(),
            error
        );
        assert_eq!(implicit(&explicit), 0);
    }

    #[test]
    fn root_module_source_profile_error_remains_deferred_to_repository_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nr=use_repo_rule('//:poison.bzl','r')\nr(name='made',text=str(len('é')))\n");
        f.write("poison.bzl", "fail('definition must not load')\n");
        let repos = RepositorySet::new(f.0.clone());
        repos.validate_root_module().unwrap();
        assert_root_module_demand_is_lazy(&repos);
        let error = repos
            .resolve_graph()
            .unwrap()
            .root
            .source_profile_error
            .as_ref()
            .unwrap()
            .clone();
        assert!(error.contains("non-ASCII"), "{error}");
        assert_eq!(
            repos.parse("@made//:file", "", "").unwrap().repository,
            "+r+made"
        );
        assert_eq!(repos.root("+r+made").unwrap_err(), error);
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn root_module_invalid_registry_options_precede_recording() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "UnknownRootSymbol\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                registries: vec![Registry::parse("https://example.invalid").unwrap()],
                ..Default::default()
            },
        );
        let expected = repos.state.options.validate_registries().unwrap_err();
        assert_eq!(repos.validate_root_module().unwrap_err(), expected);
        assert!(repos.state.root_module.get().is_none());
        assert_root_module_demand_is_lazy(&repos);
        assert_eq!(repos.resolve_graph().unwrap_err(), expected);
        assert!(repos.state.root_module.get().is_none());
    }

    #[test]
    fn ordinary_repository_local_metadata_participates_in_provenance_not_attributes() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'implicit','explicit','enabled')\n",
        );
        f.write("BUILD", "fail('BUILD must not run')\n");
        f.write(
            "rules.bzl",
            "def implementation(ctx):\n    fail('repository callback must not run during preparation')\nImplementationAlias = implementation\nimplicit = repository_rule(ImplementationAlias)\nexplicit = repository_rule(ImplementationAlias, local=False)\nenabled = repository_rule(ImplementationAlias, local=True)\nAlias = enabled\n",
        );
        f.write(
            "reexport.bzl",
            "load(':rules.bzl', 'implicit', 'explicit', 'Alias')\nImplicit = implicit\nExplicit = explicit\nEnabled = Alias\n",
        );
        f.write(
            "extension.bzl",
            "load(':reexport.bzl', 'Implicit', 'Explicit', 'Enabled')\ndef implementation(ctx):\n    Implicit(name='implicit')\n    Explicit(name='explicit')\n    Enabled(name='enabled')\nextension = module_extension(implementation)\n",
        );
        let repos = RepositorySet::new(f.0.clone());
        let (prepared, enabled) = repos.ordinary_repository("+extension+enabled").unwrap();
        let provenance: serde_json::Value =
            serde_json::from_slice(prepared.provenance().unwrap()).unwrap();
        for (name, local) in [("implicit", false), ("explicit", false), ("enabled", true)] {
            let repository = &prepared.repositories[name];
            assert_eq!(repository.definition.local, local);
            assert_eq!(
                repository.definition.identity,
                format!("//:rules.bzl%{name}")
            );
            assert!(!repository.attributes.contains_key("local"));
            assert!(repository.definition.schema.is_empty());
            assert_eq!(provenance["repositories"][name]["local"], local);
            // configure=False (default) is omitted, keeping legacy bytes/keys.
            assert!(provenance["repositories"][name].get("configure").is_none());
        }
        let mut configured = OrdinaryRepository {
            definition: enabled.definition.clone(),
            attributes: enabled.attributes.clone(),
            explicit: enabled.explicit.clone(),
            original_name: enabled.original_name.clone(),
            canonical_name: enabled.canonical_name.clone(),
            root: DemandSlot::new(),
        };
        let unconfigured = serde_json::to_string(&configured).unwrap();
        configured.definition.configure = true;
        let with_configure: serde_json::Value = serde_json::to_value(&configured).unwrap();
        assert_eq!(with_configure["configure"], serde_json::Value::Bool(true));
        assert_ne!(serde_json::to_string(&configured).unwrap(), unconfigured);
        // Hold all source, identity, schema and call metadata constant: only the
        // retained local flag changes serialized provenance, including false.
        let mut toggled = OrdinaryRepository {
            definition: enabled.definition.clone(),
            attributes: enabled.attributes.clone(),
            explicit: enabled.explicit.clone(),
            original_name: enabled.original_name.clone(),
            canonical_name: enabled.canonical_name.clone(),
            root: DemandSlot::new(),
        };
        let before = bounded_provenance(&toggled, 4096).unwrap();
        toggled.definition.local = false;
        let after = bounded_provenance(&toggled, 4096).unwrap();
        assert_ne!(before, after);
        let mut before: serde_json::Value = serde_json::from_slice(&before).unwrap();
        let mut after: serde_json::Value = serde_json::from_slice(&after).unwrap();
        assert_eq!(
            before.as_object_mut().unwrap().remove("local"),
            Some(true.into())
        );
        assert_eq!(
            after.as_object_mut().unwrap().remove("local"),
            Some(false.into())
        );
        assert_eq!(before, after);
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn download_policy_is_immutable_and_preserved_by_every_rebuilding_builder() {
        use crate::repository_environment::tests::Reader;
        let f = Fixture::new();
        let original = f.registry();
        assert!(original.repository_downloads_enabled());
        original.validate_root_module().unwrap();
        original.mapping("").unwrap();
        let disabled = original.clone().with_repository_downloads_enabled(false);
        assert!(original.repository_downloads_enabled());
        assert!(!disabled.clone().repository_downloads_enabled());
        assert!(Arc::ptr_eq(&disabled.state, &disabled.clone().state));
        let reader = Reader::new(Ok(None));
        for rebuilt in [
            disabled.clone(),
            disabled.clone().with_source_cache("other-cache".into()),
            disabled.clone().with_environment_reader(reader.clone()),
            disabled.clone().with_host(Err("test host".into())),
        ] {
            assert!(!rebuilt.repository_downloads_enabled());
            assert_ne!(original.state.runtime_id, rebuilt.state.runtime_id);
            assert_eq!(original.state.options, rebuilt.state.options);
            assert!(rebuilt.state.root_module.get().is_none());
            assert!(rebuilt.state.resolved.get().is_none());
            assert!(rebuilt.state.registry_roots.get().is_none());
            assert!(rebuilt.state.direct.get().is_none());
            assert!(rebuilt.state.absolute_source_cache.get().is_none());
        }
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &disabled.state.environment
        ));
        assert!(Arc::ptr_eq(&original.state.host, &disabled.state.host));
        let enabled = disabled.with_repository_downloads_enabled(true);
        assert!(enabled.repository_downloads_enabled());
        assert_ne!(original.state.runtime_id, enabled.state.runtime_id);
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changing_download_policy_does_not_reuse_demand_success_or_failure() {
        for failure in [false, true] {
            let f = environment_fixture(
                false,
                "[]",
                if failure {
                    "fail('old demand failure')"
                } else {
                    "ctx.file('value', 'old')"
                },
            );
            let original = RepositorySet::new(f.0.clone());
            let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
            let old = original.root(&canonical).map(Path::to_owned);
            assert_eq!(old.is_err(), failure);
            let replacement = "def implementation(ctx):\n    ctx.file('value', 'new')\nR = repository_rule(implementation, environ=[])\n";
            f.write("rules.bzl", replacement);
            let fresh = original.clone().with_repository_downloads_enabled(false);
            assert_ne!(fresh.state.runtime_id, original.state.runtime_id);
            assert!(fresh.state.direct.get().is_none());
            let root = fresh.root(&canonical).unwrap();
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"new");
            assert_eq!(original.clone().root(&canonical).map(Path::to_owned), old);
            if let Ok(old) = old {
                assert_eq!(std::fs::read(old.join("value")).unwrap(), b"old");
            }
        }
    }

    #[test]
    fn download_policy_leaves_no_source_io_provenance_and_persistent_keys_unchanged() {
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "[]", "ctx.file('value', 'constant')");
            let original = RepositorySet::new(f.0.clone());
            let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
            let disabled = original.clone().with_repository_downloads_enabled(false);
            if ordinary {
                let before = original
                    .ordinary_repository(&canonical)
                    .unwrap()
                    .0
                    .provenance()
                    .unwrap();
                let after = disabled
                    .ordinary_repository(&canonical)
                    .unwrap()
                    .0
                    .provenance()
                    .unwrap();
                assert_eq!(before, after);
            }
            let root = original.root(&canonical).unwrap();
            assert_eq!(root, disabled.root(&canonical).unwrap());
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
        }
    }

    #[test]
    fn extension_io_zero_operations_omit_entire_field_and_preserve_legacy_key_order() {
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        let mut baseline = None;
        for enabled in [true, false] {
            let repos = RepositorySet::new(f.0.clone()).with_repository_downloads_enabled(enabled);
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(prepared.provenance().unwrap()).unwrap();
            let keys: Vec<_> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "profile",
                    "public_label",
                    "symbol",
                    "prefix",
                    "sources",
                    "tag_classes",
                    "modules",
                    "source_guards",
                    "root_has_non_dev_dependency",
                    "generated_mapping",
                    "repositories",
                    "imports"
                ]
            );
            assert_eq!(value["profile"], "ordinary-file-template-v1");
            assert!(
                !String::from_utf8_lossy(prepared.provenance().unwrap()).contains("extension_io")
            );
            let root = repos.root(&canonical).unwrap().to_owned();
            let snapshot = (prepared.provenance().unwrap().to_vec(), root);
            if let Some(baseline) = &baseline {
                assert_eq!(&snapshot, baseline);
            } else {
                baseline = Some(snapshot);
            }
        }
    }

    #[test]
    fn module_downloads_are_host_facilities_not_content_inputs_and_rerun_per_runtime() {
        use crate::extension_io::tests::{Server, response};
        let a = vec![b'a'; 33000];
        let b = vec![b'b'; 33000];
        let server = Server::new(
            [&a, &a, &b, &b, &a, &a]
                .into_iter()
                .map(|bytes| response(200, bytes, ""))
                .collect(),
        );
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        f.write("extension.bzl", &format!("load(':rules.bzl', 'R')\ndef implementation(ctx):\n    ctx.download({url:?}, 'same', {sha:?}, allow_fail=True)\n    ctx.read('same', watch='no')\n    ctx.download({url:?}, 'same', {sha:?}, allow_fail=True)\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation)\n", url=server.url, sha="00".repeat(32)));
        let mut roots = Vec::new();
        for index in 0..3 {
            let repos = RepositorySet::new(f.0.clone());
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            let root = repos.root(&canonical).unwrap().to_owned();
            assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
            // Every fresh runtime re-evaluates the extension (two downloads),
            // memoized within the runtime.
            assert_eq!(server.count(), (index + 1) * 2);
            assert_eq!(repos.root(&canonical).unwrap(), root);
            let other = repos.parse("@other//:value", "", "").unwrap().repository;
            repos.root(&other).unwrap();
            assert_eq!(server.count(), (index + 1) * 2);
            let prepared = &repos.ordinary_repository(&canonical).unwrap().0;
            let value: serde_json::Value =
                serde_json::from_slice(prepared.provenance().unwrap()).unwrap();
            // Bazel records no input for a download or a read of the working
            // directory (StarlarkBaseExternalContext.java:1656-1676); the
            // non-hermetic facility is noted without URLs or content.
            assert_eq!(
                value["extension_inputs"],
                serde_json::json!({"version": 2, "host_facilities": ["download"]})
            );
            let receipt = serde_json::to_string(&value).unwrap();
            assert!(!receipt.contains(&server.url));
            assert!(!receipt.contains("modextwd"));
            roots.push(root);
        }
        // Identical generated repositories publish identically.
        assert_eq!(roots[0], roots[1]);
        assert_eq!(roots[0], roots[2]);
    }

    #[test]
    fn environment_snapshot_lifetime_follows_owner_not_source_cache_configuration() {
        use crate::repository_environment::{Watch, tests::Reader};
        let f = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let original = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        let clone = original.clone();
        let configured = clone.clone().with_source_cache("another-cache".into());
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &clone.state.environment
        ));
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &configured.state.environment
        ));
        assert_ne!(original.state.runtime_id, configured.state.runtime_id);
        for owner in [&original, &clone, &configured] {
            Watch::default()
                .observe(owner.environment(), "SYNTHETIC")
                .unwrap();
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        let fresh = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        Watch::default()
            .observe(fresh.environment(), "SYNTHETIC")
            .unwrap();
        assert_eq!(reader.calls.load(Ordering::SeqCst), 2);
        assert!(!Arc::ptr_eq(
            &original.state.environment,
            &RepositorySet::new(f.0.clone()).state.environment
        ));
        assert!(!Arc::ptr_eq(
            &original.state.environment,
            &RepositorySet::with_options(f.0.clone(), ResolverOptions::default())
                .state
                .environment
        ));
        assert!(!format!("{original:?}").contains("SyntheticSecret"));
    }

    #[test]
    fn host_snapshot_is_shared_but_test_injection_starts_fresh_demand_state() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        let f = environment_fixture(true, "[]", "ctx.file('value', 'constant')");
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let original = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        let canonical = original.parse("@repo//:value", "", "").unwrap().repository;
        let (prepared, _) = original.ordinary_repository(&canonical).unwrap();
        let bytes = prepared.provenance().unwrap().to_vec();
        let configured = original.clone().with_source_cache("another-cache".into());
        let replacement_reader = original.clone().with_environment_reader(reader.clone());
        for owner in [original.clone(), configured, replacement_reader] {
            assert!(Arc::ptr_eq(&original.state.host, &owner.state.host));
            assert!(Arc::ptr_eq(
                &original.host().unwrap(),
                &owner.host().unwrap()
            ));
        }
        let injected = original.clone().with_host(Ok(HostMetadata::fixture(
            "native-only",
            "test-arch",
            "test-os",
        )));
        assert!(!Arc::ptr_eq(&original.state.host, &injected.state.host));
        assert!(Arc::ptr_eq(
            &original.state.environment,
            &injected.state.environment
        ));
        assert!(injected.state.direct.get().is_none());
        assert!(injected.state.root_module.get().is_none());
        assert_ne!(original.state.runtime_id, injected.state.runtime_id);
        assert_eq!(
            original
                .ordinary_repository(&canonical)
                .unwrap()
                .0
                .provenance()
                .unwrap(),
            bytes
        );
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_only_host_factor_matrix_is_checked_before_materialization() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        for os in [false, true] {
            for arch in [false, true] {
                let f = environment_fixture(true, "[]", "fail('repository callback must not run')");
                f.write("extension.bzl", &format!("load(':rules.bzl', 'R')\ndef implementation(ctx):\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation, os_dependent={}, arch_dependent={})\n", if os { "True" } else { "False" }, if arch { "True" } else { "False" }));
                let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
                let run = |name, host_arch, factor| {
                    let repos = RepositorySet::new(f.0.clone())
                        .with_environment_reader(reader.clone())
                        .with_host(Ok(HostMetadata::fixture(name, host_arch, factor)));
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
                    assert!(!f.0.join("bazel-rs-out").exists());
                    prepared.provenance().unwrap().to_vec()
                };
                let base = run("getter", "arch-a", "os-a");
                assert_eq!(base != run("getter", "arch-a", "os-b"), os);
                assert_eq!(base != run("getter", "arch-b", "os-a"), arch);
                assert_eq!(base != run("getter", "arch-b", "os-b"), os || arch);
                assert_eq!(base, run("another getter", "arch-a", "os-a"));
                let json: serde_json::Value = serde_json::from_slice(&base).unwrap();
                if !os && !arch {
                    assert_eq!(json.as_object().unwrap().len(), 12);
                    assert!(json.get("host_factors").is_none());
                } else {
                    let factors = json["host_factors"].as_object().unwrap();
                    assert_eq!(factors.len(), usize::from(os) + usize::from(arch));
                    assert_eq!(
                        factors.get("os").and_then(|v| v.as_str()),
                        os.then_some("os-a")
                    );
                    assert_eq!(
                        factors.get("arch").and_then(|v| v.as_str()),
                        arch.then_some("arch-a")
                    );
                }
                assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn native_only_unread_host_rekeys_direct_and_generated_repositories_and_restores() {
        use crate::host_os::HostMetadata;
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "[]", "ctx.file('value', 'constant')");
            let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
            let run = |name, arch, factor| {
                let repos = RepositorySet::new(f.0.clone())
                    .with_environment_reader(reader.clone())
                    .with_host(Ok(HostMetadata::fixture(name, arch, factor)));
                let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                let root = repos.root(&canonical).unwrap().to_owned();
                assert_eq!(std::fs::read(root.join("value")).unwrap(), b"constant");
                assert_eq!(repos.clone().root(&canonical).unwrap(), root);
                root
            };
            let base = run("getter-a", "arch-a", "os-a");
            assert_ne!(base, run("getter-b", "arch-a", "os-a"));
            assert_ne!(base, run("getter-a", "arch-b", "os-a"));
            // Canonical factor is not an unconditional repository host input.
            assert_eq!(base, run("getter-a", "arch-a", "os-b"));
            assert_eq!(base, run("getter-a", "arch-a", "os-a"));
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn host_getter_reads_never_enable_flags_but_selected_attributes_remain_inputs() {
        use crate::host_os::HostMetadata;
        for select in [false, true] {
            let f = Fixture::new();
            f.write("BUILD", "");
            f.write(
                "MODULE.bazel",
                "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo')\n",
            );
            f.write("rules.bzl", "def implementation(ctx):\n    fail('must not materialize')\nR=repository_rule(implementation, attrs={'selected':attr.string()})\n");
            f.write("extension.bzl", &format!("load(':rules.bzl','R')\ndef implementation(ctx):\n    host = ctx.os\n    name = host.name\n    arch = host.arch\n    R(name='repo', selected={})\nextension=module_extension(implementation)\n", if select { "name + '/' + arch" } else { "'constant'" }));
            let run =
                |name, arch| {
                    let repos = RepositorySet::new(f.0.clone())
                        .with_host(Ok(HostMetadata::fixture(name, arch, "canonical")));
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    let (prepared, repository) = repos.ordinary_repository(&canonical).unwrap();
                    let json: serde_json::Value =
                        serde_json::from_slice(prepared.provenance().unwrap()).unwrap();
                    assert!(json.get("host_factors").is_none());
                    assert!(!repository.attributes.contains_key("host_factors"));
                    assert!(!f.0.join("bazel-rs-out").exists());
                    prepared.provenance().unwrap().to_vec()
                };
            let base = run("getter-a", "arch-a");
            assert_eq!(base != run("getter-b", "arch-a"), select);
            assert_eq!(base != run("getter-a", "arch-b"), select);
        }
    }

    #[test]
    fn unsupported_host_stays_lazy_then_fails_before_callbacks_and_publication() {
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "['SYNTHETIC']", "fail('callback ran')");
            let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
            let repos = RepositorySet::new(f.0.clone())
                .with_environment_reader(reader.clone())
                .with_host(Err("synthetic unsupported host".into()));
            repos.validate_root_module().unwrap();
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            for _ in 0..2 {
                let error = repos.root(&canonical).unwrap_err();
                assert!(error.contains("synthetic unsupported host"), "{error}");
                assert!(!error.contains("callback ran"));
            }
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            let trees = f.0.join("bazel-rs-out/repositories/direct/trees");
            assert!(!trees.exists() || std::fs::read_dir(trees).unwrap().next().is_none());
        }
    }

    /// [`longest_root`] over literal roots, `canonical` mapping some of them.
    fn longest(
        roots: &[(&str, &str)],
        canonical: &[(&str, &str)],
        path: &str,
    ) -> Option<(String, String)> {
        let canonical: BTreeMap<PathBuf, PathBuf> = canonical
            .iter()
            .map(|(root, resolved)| (PathBuf::from(root), PathBuf::from(resolved)))
            .collect();
        longest_root(
            roots
                .iter()
                .map(|(name, root)| ((*name).to_owned(), PathBuf::from(root))),
            |root| canonical.get(root).cloned(),
            Path::new(path),
        )
        .map(|(name, relative)| (name, relative.to_str().unwrap().to_owned()))
    }

    /// Review i20: RepoCacheFriendlyPath's repository is the longest root
    /// containing the path. Nested roots name the innermost repository;
    /// equally long roots (repositories sharing one content-addressed tree)
    /// the smallest name in any order; a root matches as given or resolved,
    /// the longer candidate winning; prefixes match whole components.
    #[test]
    fn longest_root_takes_nested_roots_ties_and_canonical_roots() {
        let some = |name: &str, relative: &str| Some((name.to_owned(), relative.to_owned()));
        let nested = [
            ("", "/ws"),
            ("outer+", "/ws/vendor/outer"),
            ("inner+", "/ws/vendor/outer/inner"),
        ];
        let mut reversed = nested;
        reversed.reverse();
        for roots in [nested, reversed] {
            let at = |path| longest(&roots, &[], path);
            assert_eq!(
                at("/ws/vendor/outer/inner/x/y.txt"),
                some("inner+", "x/y.txt")
            );
            assert_eq!(at("/ws/vendor/outer/z.txt"), some("outer+", "z.txt"));
            assert_eq!(at("/ws/vendor/outer"), some("outer+", ""));
            assert_eq!(
                at("/ws/vendor/outer-sibling/z.txt"),
                some("", "vendor/outer-sibling/z.txt")
            );
            assert_eq!(at("/elsewhere/x"), None);
        }
        // Ties: one tree, three repositories.
        let tied = [
            ("b", "/store/tree"),
            ("a", "/store/tree"),
            ("c", "/store/tree"),
        ];
        let mut reversed = tied;
        reversed.reverse();
        for roots in [tied, reversed] {
            assert_eq!(longest(&roots, &[], "/store/tree/f"), some("a", "f"));
        }
        // Canonical: a root given through a symlink matches both spellings.
        let linked = [("", "/ws"), ("linked+", "/ws/link")];
        let resolved = [("/ws/link", "/real/dir")];
        assert_eq!(
            longest(&linked, &resolved, "/real/dir/f"),
            some("linked+", "f")
        );
        assert_eq!(
            longest(&linked, &resolved, "/ws/link/f"),
            some("linked+", "f")
        );
        assert_eq!(longest(&linked, &[], "/real/dir/f"), None);
        // The longer candidate wins: a short root resolving inside a longer
        // one, and a tie between one root's canonical form and another root.
        let roots = [("short", "/l"), ("long", "/very/long")];
        let resolved = [("/l", "/very/long/real/root")];
        assert_eq!(
            longest(&roots, &resolved, "/very/long/real/root/f"),
            some("short", "f")
        );
        assert_eq!(
            longest(&roots, &resolved, "/very/long/f"),
            some("long", "f")
        );
        let roots = [("b", "/x/y"), ("a", "/link")];
        let resolved = [("/link", "/x/y")];
        assert_eq!(longest(&roots, &resolved, "/x/y/f"), some("a", "f"));
    }

    /// friendly_repository over a resolved graph: local_path_override roots
    /// nested inside each other and inside the workspace name the innermost
    /// repository (genuine checks the workspace first: the documented
    /// divergence in docs/REPOSITORY_RULES.md), with the workspace given as
    /// is or through a symlink, paths spelled raw or canonically.
    #[cfg(unix)]
    #[test]
    fn friendly_repository_names_the_innermost_materialized_root() {
        let f = Fixture::new();
        f.write(
            "main/MODULE.bazel",
            "module(name='main')\nbazel_dep(name='outer',version='1')\nlocal_path_override(module_name='outer',path='vendor/outer')\nbazel_dep(name='inner',version='1')\nlocal_path_override(module_name='inner',path='vendor/outer/inner')\n",
        );
        f.write("main/vendor/outer/MODULE.bazel", "module(name='outer')\n");
        f.write(
            "main/vendor/outer/inner/MODULE.bazel",
            "module(name='inner')\n",
        );
        std::os::unix::fs::symlink(f.0.join("main"), f.0.join("link")).unwrap();
        let main = std::fs::canonicalize(f.0.join("main")).unwrap();
        for root in [f.0.join("main"), f.0.join("link")] {
            let repos = RepositorySet::new(root.clone());
            repos.resolve_graph().unwrap();
            let at = |path: PathBuf| {
                repos
                    .friendly_repository(&path)
                    .map(|(name, relative)| (name, relative.to_str().unwrap().to_owned()))
            };
            let some = |name: &str, relative: &str| Some((name.to_owned(), relative.to_owned()));
            assert_eq!(
                at(main.join("vendor/outer/inner/x.txt")),
                some("inner+", "x.txt")
            );
            assert_eq!(at(main.join("vendor/outer/y.txt")), some("outer+", "y.txt"));
            assert_eq!(
                at(main.join("vendor/outer-sibling/z")),
                some("", "vendor/outer-sibling/z")
            );
            assert_eq!(at(root.join("src/a")), some("", "src/a"));
            assert_eq!(at(main.join("src/a")), some("", "src/a"));
            assert_eq!(at(f.0.join("elsewhere/a")), None);
        }
    }

    /// Review i20: a path in the repository store is never keyed to the
    /// main repository — the cache inside the workspace (the default
    /// layout) behaves like a cache outside it — and a lockfile key naming
    /// the store through the workspace or absolutely is outdated, whether
    /// the workspace is spelled as given (here through a symlink) or
    /// canonically. Other workspace paths, the cache directory outside the
    /// store included, keep their `@@//` keys and current values.
    #[cfg(unix)]
    #[test]
    fn store_paths_never_key_to_the_workspace_and_store_lock_keys_are_outdated() {
        use crate::lockfile_extensions::{FriendlyPath, InputProbe as _};
        let f = Fixture::new();
        let workspace = f.0.join("ws");
        let store = workspace.join("bazel-rs-out/repositories");
        f.write(
            "ws/bazel-rs-out/repositories/direct/trees/abc/file.txt",
            "old",
        );
        f.write("ws/src/file.txt", "source");
        std::os::unix::fs::symlink(&workspace, f.0.join("link")).unwrap();
        let key = |repository: Option<&str>, path: &str| FriendlyPath {
            repository: repository.map(str::to_owned),
            path: path.to_owned(),
        };
        for root in [workspace.clone(), f.0.join("link")] {
            let repos = RepositorySet::new(root.clone());
            for spelled in [
                root.join("bazel-rs-out/repositories/direct/trees/abc/file.txt"),
                store.join("direct/trees/abc/file.txt"),
                store.join("direct/trees"),
            ] {
                assert!(repos.in_repository_store(&spelled), "{spelled:?}");
                assert_eq!(repos.friendly_repository(&spelled), None, "{spelled:?}");
            }
            // A repository's directory (its slot) names that repository,
            // genuine's `<output_base>/external/<name>/rel`; a digest-named
            // or scratch entry there names none.
            for spelled in [
                root.join("bazel-rs-out/repositories/external/+r+sib/pkg/f.txt"),
                store.join("external/+r+sib/pkg/f.txt"),
            ] {
                assert_eq!(
                    repos.friendly_repository(&spelled),
                    Some(("+r+sib".to_owned(), PathBuf::from("pkg/f.txt"))),
                    "{spelled:?}"
                );
            }
            for spelled in [
                store.join("external/#long-0123/f"),
                store.join("external/.bind-1-2"),
                store.join("external"),
            ] {
                assert_eq!(repos.friendly_repository(&spelled), None, "{spelled:?}");
            }
            for (spelled, relative) in [
                (root.join("src/file.txt"), "src/file.txt"),
                (workspace.join("src/file.txt"), "src/file.txt"),
                (
                    root.join("bazel-rs-out/actions/x"),
                    "bazel-rs-out/actions/x",
                ),
            ] {
                assert!(!repos.in_repository_store(&spelled), "{spelled:?}");
                assert_eq!(
                    repos.friendly_repository(&spelled),
                    Some((String::new(), PathBuf::from(relative)))
                );
            }
            let probe = LockInputProbe(&repos);
            for stale in [
                key(
                    Some(""),
                    "bazel-rs-out/repositories/direct/trees/abc/file.txt",
                ),
                key(
                    None,
                    store.join("direct/trees/abc/file.txt").to_str().unwrap(),
                ),
                key(Some(""), "bazel-rs-out/repositories/direct/trees/abc"),
            ] {
                let reason = probe.file(&stale).unwrap_err();
                assert!(reason.contains("native repository store"), "{reason}");
                assert_eq!(probe.dirents(&stale).unwrap_err(), reason);
            }
            assert_eq!(
                probe.file(&key(Some(""), "src/file.txt")).unwrap(),
                format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"source"))
            );
            assert_eq!(
                probe.file(&key(Some(""), "src/missing.txt")).unwrap(),
                "ENOENT"
            );
        }
    }

    #[test]
    fn versioned_host_envelope_intentionally_migrates_old_constant_repository_identity() {
        let cache = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let inputs = crate::repository_ctx::ExecutionInputs::default();
        let legacy = serde_json::to_vec(&inputs).unwrap();
        assert_eq!(legacy, br#"{"templates":[]}"#);
        let host = crate::host_os::HostMetadata::fixture("linux", "amd64", "linux");
        let current = crate::host_os::repository_inputs(&host, &inputs).unwrap();
        let run = |bytes: &[u8], warm_world_unchanged: bool| {
            crate::repository_store::materialize(
                cache.path(),
                "same-key",
                b"same-spec",
                b"same-provenance",
                move |_, _| Ok(warm_world_unchanged),
                |stage| {
                    std::fs::write(stage.join("constant"), b"same-content").unwrap();
                    Ok(crate::repository_store::Executed {
                        inputs: bytes.to_vec(),
                        recorded: Vec::new(),
                    })
                },
            )
            .unwrap()
            .0
        };
        let old_root = run(&legacy, true);
        // The host envelope change is a runtime-input change: the recorded
        // world differs, so the publication refetches under a new identity.
        let new_root = run(&current, false);
        assert_ne!(old_root, new_root);
        assert_eq!(new_root, run(&current, true));
        assert_eq!(old_root, run(&legacy, false));
    }

    fn environment_fixture(ordinary: bool, declaration: &str, body: &str) -> Fixture {
        let f = Fixture::new();
        f.write("BUILD", "");
        f.write("rules.bzl", &format!("def implementation(ctx):\n    {body}\nR = repository_rule(implementation, environ={declaration})\n"));
        if ordinary {
            f.write(
                "MODULE.bazel",
                "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo','other')\n",
            );
            f.write("extension.bzl", "load(':rules.bzl', 'R')\ndef implementation(ctx):\n    R(name='repo')\n    R(name='other')\nextension=module_extension(implementation)\n");
        } else {
            f.write(
                "MODULE.bazel",
                "r=use_repo_rule('//:rules.bzl','R')\nr(name='repo')\nr(name='other')\n",
            );
        }
        f
    }

    #[test]
    fn warm_reuse_validates_label_recorded_inputs() {
        // Label reads record main-repository labels in their bare `//pkg:name`
        // form; warm revalidation must resolve them (and refetch on change).
        let f = Fixture::new();
        f.write("BUILD", "");
        f.write("data.txt", "stable");
        f.write(
            "rules.bzl",
            "def implementation(ctx):\n    ctx.read(Label('//:data.txt'))\n    ctx.file('value', 'constant')\nR = repository_rule(implementation)\n",
        );
        f.write(
            "MODULE.bazel",
            "r=use_repo_rule('//:rules.bzl','R')\nr(name='repo')\n",
        );
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
        let first = repos.root(&canonical).unwrap().to_path_buf();
        assert_eq!(std::fs::read(first.join("value")).unwrap(), b"constant");
        // A fresh repository set (a new process) reuses the publication: the
        // recorded Label input still resolves to unchanged bytes.
        let warm = RepositorySet::new(f.0.clone());
        assert_eq!(warm.root(&canonical).unwrap(), first);
        let trees = f.0.join("bazel-rs-out/repositories/direct/trees");
        let count = std::fs::read_dir(&trees)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(count, 1);
        // Changed label input bytes refetch under a new identity.
        std::fs::write(f.0.join("data.txt"), "changed").unwrap();
        let changed = RepositorySet::new(f.0.clone());
        let second = changed.root(&canonical).unwrap().to_path_buf();
        assert_ne!(second, first);
    }

    /// W6 review F2 follow-ups: `@a` records a read of `@b//:file.txt` and
    /// `@b` one of `@c//:file.txt`, so the sequential engine's (warm)
    /// evaluation of `@a` demands `@b`, whose evaluation demands `@c`. A
    /// loading worker's probe of `@a` obtains `@b` in its own scope — and
    /// `@b`'s nested probe `@c` in its own — and stores it with `@a`'s
    /// publication (`@c` with `@b`'s); the worker's unit records only
    /// `@a`, also when it computes `@a`'s ignored subdirectories. None of
    /// them counts for keys until the sequential engine obtains `@a` — the
    /// coordinator's demand of the filled slot, the commit of a unit that
    /// obtained it, a shared-cache hit on one of its `.bzl` modules, or a
    /// hit of its ignored subdirectories (review FU1 of 6474766) — and
    /// then all three do: with `@a` a direct repository, and with `@a`
    /// generated by a module extension (an ordinary root's probe).
    #[test]
    fn a_probed_root_counts_with_the_roots_its_probe_obtained() {
        use crate::repository_demand::{no_demand, reuse_only};
        for generated in [false, true] {
            let f = Fixture::new();
            f.write("BUILD", "");
            f.write(
                "rules.bzl",
                "def implementation(ctx):\n    if ctx.attr.read_from:\n        ctx.read(ctx.attr.read_from)\n    ctx.file('BUILD.bazel', '')\n    ctx.file('file.txt', 'data')\nR = repository_rule(implementation, attrs = {'read_from': attr.label()})\n",
            );
            let chain = "r=use_repo_rule('//:rules.bzl','R')\nr(name='early')\nr(name='c')\nr(name='b', read_from='@c//:file.txt')\n";
            if generated {
                f.write(
                    "extension.bzl",
                    "load(':rules.bzl', 'R')\ndef implementation(ctx):\n    R(name='a', read_from='@b//:file.txt')\nextension=module_extension(implementation)\n",
                );
                f.write(
                    "MODULE.bazel",
                    &format!(
                        "{chain}e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'a')\n"
                    ),
                );
            } else {
                f.write(
                    "MODULE.bazel",
                    &format!("{chain}r(name='a', read_from='@b//:file.txt')\n"),
                );
            }
            let cold = RepositorySet::new(f.0.clone());
            let canonical = |name: &str| {
                cold.parse(&format!("@{name}//:file.txt"), "", "")
                    .unwrap()
                    .repository
            };
            let (early, a, b, c) = (
                canonical("early"),
                canonical("a"),
                canonical("b"),
                canonical("c"),
            );
            // `@a` is an ordinary (extension-generated) repository exactly
            // when `generated`.
            assert_eq!(
                cold.direct_runtime()
                    .unwrap()
                    .catalog
                    .repositories
                    .contains_key(&a),
                !generated
            );
            cold.root(&a).unwrap();
            let file_of = |root: &Path| std::fs::canonicalize(root.join("file.txt")).unwrap();
            let files = [&a, &b, &c].map(|name| (name, file_of(cold.root(name).unwrap())));
            let key = |name: &str| Some((name.to_owned(), PathBuf::from("file.txt")));
            for obtained_by in ["demand", "commit", "cache hit", "ignored hit"] {
                let context = format!("generated={generated} {obtained_by}");
                // A new invocation: the coordinator prepares the groups.
                let warm = RepositorySet::new(f.0.clone());
                warm.root(&early).unwrap();
                if generated {
                    warm.mapping(&a).unwrap();
                }
                let unit = {
                    let _guard = no_demand();
                    let _reuse = reuse_only();
                    let unit = UnitRoots::record();
                    warm.root(&a).unwrap();
                    // A second obtain (a cache hit) records nothing new;
                    // nor does filling the ignored-subdirectories memo.
                    warm.note_obtained(&a);
                    warm.ignored_subdirectories(&a).unwrap();
                    unit.finish()
                };
                assert_eq!(unit, std::slice::from_ref(&a), "{context}");
                let runtime = warm.direct_runtime().unwrap();
                let slot = |name: &str| runtime.root_slot(name).unwrap();
                assert_eq!(slot(&a).obtained(), std::slice::from_ref(&b), "{context}");
                assert_eq!(slot(&b).obtained(), std::slice::from_ref(&c), "{context}");
                assert!(slot(&c).ready().is_some(), "{context}");
                for (_, file) in &files {
                    assert_eq!(warm.friendly_repository(file), None, "{context}");
                }
                match obtained_by {
                    "demand" => {
                        warm.root(&a).unwrap();
                    }
                    "commit" => warm.note_committed_roots(&unit),
                    "cache hit" => warm.note_obtained(&a),
                    _ => {
                        warm.ignored_subdirectories(&a).unwrap();
                    }
                }
                for (name, file) in &files {
                    assert_eq!(warm.friendly_repository(file), key(name), "{context}");
                }
            }
        }
    }

    /// W6R: an extension-generated repository's spec key reads its
    /// declared environment once per invocation (the key is cached per
    /// name). A worker's warm-reuse probe may compute it first and miss
    /// (here: nothing is published yet), its observations dropped with the
    /// probe; the sequential engine's own computation, a cache hit, still
    /// records the environment value: the entry's captured set replays.
    #[test]
    fn a_spec_key_hit_after_a_probe_miss_records_the_declared_environment() {
        use crate::observed_fs::tests_support::record_this_thread;
        use crate::repository_demand::{no_demand, reuse_only};
        let f = environment_fixture(true, "['SYNTHETIC']", "ctx.file('value', 'constant')");
        let reader = crate::repository_environment::tests::Reader::new(Ok(Some("v".into())));
        let repos = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
        let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
        // The coordinator prepares the extension group (outside the window).
        repos.mapping(&canonical).unwrap();
        let window = record_this_thread();
        {
            let _guard = no_demand();
            let _reuse = reuse_only();
            let unit = crate::observed_fs::unit_scope();
            let missed = repos.root(&canonical).unwrap_err();
            assert!(
                crate::repository_demand::is_needs_repository(&missed),
                "{missed}"
            );
            // The build never commits this unit.
            drop(unit);
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        assert!(window.recorder.snapshot().env.is_empty());
        let (prepared, repository) = repos.ordinary_repository(&canonical).unwrap();
        prepared
            .spec_key(
                &repos,
                &repository.canonical_name,
                &repository.definition.environ,
            )
            .unwrap();
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        let env = window.recorder.snapshot().env;
        assert_eq!(
            env.iter()
                .map(|(name, _)| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["SYNTHETIC"]
        );
    }

    /// W6R: while the analysis snapshot records, the link policy holds the
    /// roots as the sequential engine sees them — a root a worker's probe
    /// published is not among them until the sequential engine obtains it
    /// (outside the window it is: published trees anchor links in any
    /// order) — and records no canonical form; the end of planning records
    /// every counting root's, the store's and the blocked prefixes' once.
    #[test]
    fn the_link_policy_holds_the_sequential_roots_while_recording() {
        use crate::observed_fs::tests_support::record_this_thread;
        use crate::repository_demand::{no_demand, reuse_only};
        let f = Fixture::new();
        f.write("BUILD", "");
        f.write(
            "rules.bzl",
            "def implementation(ctx):\n    ctx.file('BUILD.bazel', '')\n    ctx.file('file.txt', 'data')\nR = repository_rule(implementation)\n",
        );
        f.write(
            "MODULE.bazel",
            "r=use_repo_rule('//:rules.bzl','R')\nr(name='early')\nr(name='data')\n",
        );
        let cold = RepositorySet::new(f.0.clone());
        let canonical = |name: &str| {
            cold.parse(&format!("@{name}//:file.txt"), "", "")
                .unwrap()
                .repository
        };
        let (early, data) = (canonical("early"), canonical("data"));
        cold.root(&data).unwrap();
        let warm = RepositorySet::new(f.0.clone());
        warm.root(&early).unwrap();
        let window = record_this_thread();
        {
            let _guard = no_demand();
            let _reuse = reuse_only();
            let unit = crate::observed_fs::unit_scope();
            warm.root(&data).unwrap();
            drop(unit);
        }
        let holds = |policy: &crate::source_links::LinkPolicy, name: &str| {
            policy.others.iter().any(|(other, _)| other == name)
        };
        let canons = |recording: &crate::observed_fs::Recording| -> Vec<PathBuf> {
            recording
                .observations
                .iter()
                .filter(|(_, observation)| observation.op == crate::observed_fs::Op::Canon)
                .map(|(path, _)| path.clone())
                .collect()
        };
        let policy = warm.link_policy("");
        assert!(holds(&policy, &early) && !holds(&policy, &data));
        assert!(canons(&window.recorder.snapshot()).is_empty());
        drop(window);
        assert!(holds(&warm.link_policy(""), &data));
        let window = record_this_thread();
        // The demand replays the probe's observations (its CANON of the
        // workspace root among them); the policy adds none.
        warm.root(&data).unwrap();
        let probed = canons(&window.recorder.snapshot());
        assert!(holds(&warm.link_policy(""), &data));
        assert_eq!(canons(&window.recorder.snapshot()), probed);
        let window = record_this_thread();
        warm.record_link_anchors();
        let recorded = canons(&window.recorder.snapshot());
        let mut expected: Vec<PathBuf> = warm
            .sequential_roots()
            .into_iter()
            .map(|(_, root)| root)
            .chain(warm.link_blocked_prefixes())
            .collect();
        expected.push(warm.source_cache_path().unwrap().to_path_buf());
        expected.sort();
        expected.dedup();
        let mut recorded_sorted = recorded.clone();
        recorded_sorted.sort();
        assert_eq!(recorded_sorted, expected);
        for root in [warm.root(&early).unwrap(), warm.root(&data).unwrap()] {
            assert!(recorded.iter().any(|path| path == root), "{root:?}");
        }
    }

    /// W6R, the observations twin of
    /// [`a_probed_root_counts_with_the_roots_its_probe_obtained`]: the chain
    /// `@a` → `@b` → `@c` of recorded reads, and a load unit of `@a`'s
    /// root package (`load(':defs.bzl')`) on a worker. What the probes
    /// observed (the three trees' REPO records, the recorded inputs' reads)
    /// travels with the publications, what the unit observed with the unit,
    /// what `defs.bzl` and the ignored subdirectories observed with their
    /// memo entries. None of it reaches the recording before the sequential
    /// engine obtains `@a`, and then exactly what that obtain observes at
    /// threads 1: a demand (the probes'), the unit's commit (everything), a
    /// load of `@a//sub` that hits the `.bzl` cache (the probes', the
    /// module's, the ignored lookup's), an ignored-subdirectories hit (the
    /// probes', the ignored lookup's). One kill switch per hook breaks it.
    #[test]
    fn a_probed_roots_observations_reach_the_recording_where_it_counts() {
        use crate::observed_fs::tests_support::{hook_off, record_this_thread};
        use crate::observed_fs::{Hook, Op};
        use crate::repository_demand::{no_demand, reuse_only};
        let f = Fixture::new();
        f.write("BUILD", "");
        f.write(
            "rules.bzl",
            "def implementation(ctx):\n    if ctx.attr.read_from:\n        ctx.read(ctx.attr.read_from)\n    ctx.file('BUILD.bazel', \"load(':defs.bzl', 'X')\\nexports_files(['file.txt'])\\n\")\n    ctx.file('sub/BUILD.bazel', \"load('//:defs.bzl', 'X')\\n\")\n    ctx.file('defs.bzl', 'X = 1\\n')\n    ctx.file('file.txt', 'data')\nR = repository_rule(implementation, attrs = {'read_from': attr.label()})\n",
        );
        f.write(
            "MODULE.bazel",
            "r=use_repo_rule('//:rules.bzl','R')\nr(name='early')\nr(name='c')\nr(name='b', read_from='@c//:file.txt')\nr(name='a', read_from='@b//:file.txt')\n",
        );
        let cold = RepositorySet::new(f.0.clone());
        let canonical = |name: &str| {
            cold.parse(&format!("@{name}//:file.txt"), "", "")
                .unwrap()
                .repository
        };
        let (early, a, b, c) = (
            canonical("early"),
            canonical("a"),
            canonical("b"),
            canonical("c"),
        );
        cold.root(&a).unwrap();
        let roots: Vec<PathBuf> = [&a, &b, &c]
            .map(|name| cold.root(name).unwrap().to_path_buf())
            .to_vec();
        // What each obtain must bring: the probes' (`P`), the module's
        // (`D`), the ignored lookup's (`I`), the unit's own (`U`).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Part {
            Probes,
            Module,
            Ignored,
            Unit,
        }
        let parts = |recording: &crate::observed_fs::Recording| -> Vec<Part> {
            let observed = |path: PathBuf, op: Op| {
                recording
                    .observations
                    .iter()
                    .any(|(seen, observation)| *seen == path && observation.op == op)
            };
            let probes = roots
                .iter()
                .all(|root| recording.repositories.iter().any(|tree| tree.root == *root))
                && observed(roots[1].join("file.txt"), Op::Read)
                && observed(roots[2].join("file.txt"), Op::Read);
            let mut found = Vec::new();
            if probes {
                found.push(Part::Probes);
            }
            if observed(roots[0].join("defs.bzl"), Op::Read) {
                found.push(Part::Module);
            }
            if observed(roots[0].join(".bazelignore"), Op::Stat) {
                found.push(Part::Ignored);
            }
            if observed(roots[0].join("BUILD.bazel"), Op::Read) {
                found.push(Part::Unit);
            }
            found
        };
        type Obtain = fn(&Arc<RepositorySet>, &str, crate::package::LoadUnit);
        let obtains: [(&str, Obtain, &[Part]); 4] = [
            (
                "demand",
                |warm, a, _| {
                    warm.root(a).unwrap();
                },
                &[Part::Probes],
            ),
            (
                "commit",
                |warm, _, unit| {
                    warm.note_committed_roots(&unit.roots);
                    crate::observed_fs::commit(unit.observations);
                },
                &[Part::Probes, Part::Module, Part::Ignored, Part::Unit],
            ),
            (
                "cache hit",
                |warm, a, _| {
                    let sub = PackageId {
                        repository: a.to_owned(),
                        package: "sub".to_owned(),
                    };
                    crate::package::evaluate_in(warm.clone(), &sub).unwrap();
                },
                &[Part::Probes, Part::Module, Part::Ignored],
            ),
            (
                "ignored hit",
                |warm, a, _| {
                    warm.ignored_subdirectories(a).unwrap();
                },
                &[Part::Probes, Part::Ignored],
            ),
        ];
        // Every hook on: exactly the parts; one hook off: the parts it
        // carries are missing at the obtain (the unit's frame: everything
        // the unit observed is there before any obtain).
        let switches: [Option<Hook>; 5] = [
            None,
            Some(Hook::Probe),
            Some(Hook::Bzl),
            Some(Hook::Ignored),
            Some(Hook::Unit),
        ];
        for (name, obtain, expected) in obtains {
            for off in switches {
                let context = format!("{name}, hook off: {off:?}");
                let warm = Arc::new(RepositorySet::new(f.0.clone()));
                warm.root(&early).unwrap();
                let window = record_this_thread();
                let _off = off.map(hook_off);
                let unit = {
                    let _guard = no_demand();
                    let _reuse = reuse_only();
                    let root_package = PackageId {
                        repository: a.clone(),
                        package: String::new(),
                    };
                    let unit = crate::package::evaluate_load_unit(warm.clone(), &root_package);
                    assert!(unit.result.is_ok(), "{context}: {:?}", unit.result.err());
                    unit
                };
                assert_eq!(unit.roots, std::slice::from_ref(&a), "{context}");
                let before = parts(&window.recorder.snapshot());
                if off == Some(Hook::Unit) {
                    assert!(
                        before.contains(&Part::Unit),
                        "{context}: the unit recorded at once"
                    );
                    continue;
                }
                assert!(before.is_empty(), "{context}: before the obtain {before:?}");
                obtain(&warm, &a, unit);
                let after = parts(&window.recorder.snapshot());
                let wanted: Vec<Part> = expected
                    .iter()
                    .copied()
                    .filter(|part| match (off, part) {
                        (Some(Hook::Probe), Part::Probes) => false,
                        (Some(Hook::Bzl), Part::Module) => name != "cache hit",
                        (Some(Hook::Ignored), Part::Ignored) => {
                            !matches!(name, "cache hit" | "ignored hit")
                        }
                        _ => true,
                    })
                    .collect();
                assert_eq!(after, wanted, "{context}");
            }
        }
    }

    #[test]
    fn direct_and_ordinary_environment_content_identity_restore_and_demand_memoization() {
        use crate::repository_environment::{Watch, tests::Reader};
        for ordinary in [false, true] {
            for (declaration, body, output_changes) in [
                ("['SYNTHETIC']", "ctx.file('value', 'constant')", false),
                (
                    "[]",
                    "ctx.getenv('SYNTHETIC')\n    ctx.file('value', 'constant')",
                    false,
                ),
                (
                    "('SYNTHETIC',)",
                    "ctx.file('value', ctx.getenv('SYNTHETIC'))",
                    true,
                ),
            ] {
                let f = environment_fixture(ordinary, declaration, body);
                let run = |value: &str, unrelated: bool| {
                    let reader = Reader::new(Ok(Some(value.into())));
                    let repos =
                        RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
                    let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                    // Nothing is read before the repository is demanded.
                    assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
                    if unrelated {
                        Watch::default()
                            .observe(repos.environment(), "UNRELATED")
                            .unwrap();
                    }
                    let root = repos.root(&canonical).unwrap().to_path_buf();
                    assert_eq!(
                        std::fs::read(root.join("value")).unwrap(),
                        if output_changes {
                            value.as_bytes()
                        } else {
                            b"constant"
                        }
                    );
                    // In-set demand memoization: a second demand never reads.
                    let calls = reader.calls.load(Ordering::SeqCst);
                    assert!(calls >= 1);
                    assert_eq!(repos.clone().root(&canonical).unwrap(), root);
                    assert_eq!(reader.calls.load(Ordering::SeqCst), calls);
                    // Only inventory, digests and watch-probe NAMES persist
                    // (a native policy: Bazel's own .marker stores ENV:
                    // values in plain text; the native manifest never
                    // persists environment VALUES).
                    for entry in
                        std::fs::read_dir(f.0.join("bazel-rs-out/repositories/direct/manifests"))
                            .unwrap()
                    {
                        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
                        let text = String::from_utf8(bytes).unwrap();
                        assert!(!text.contains("SyntheticSecret"));
                        let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(manifest.as_object().unwrap().len(), 6);
                        assert_eq!(manifest["profile"], "direct-file-template-v1");
                    }
                    root
                };
                // A fresh repository set (a new process) with the same cache
                // and environment reuses the publication without executing.
                let first = run("SyntheticSecretFirst", false);
                assert_eq!(run("SyntheticSecretFirst", true), first);
                // A changed environment value is a changed repository input:
                // declared values re-key the specification before execution,
                // observed values fail warm revalidation; both refetch.
                assert_ne!(run("SyntheticSecretChanged", false), first);
                // Restoring the value restores the publication (the trees are
                // content-addressed, so no re-publication happens either).
                assert_eq!(run("SyntheticSecretFirst", false), first);
            }
        }
    }

    #[test]
    fn warm_repository_reuse_never_reexecutes_the_rule() {
        // Across fresh repository sets (processes) with unchanged inputs the
        // repository rule must not run again: the implementation appends to a
        // counter file outside the repository (execute is a host facility,
        // recorded only as a name), so one line after a warm demand proves
        // pure publication reuse.
        for ordinary in [false, true] {
            let counter = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            std::fs::write(counter.path().join("executions"), b"").unwrap();
            let script = format!(
                "ctx.execute(['sh', '-c', 'echo run >> {}'])\n    ctx.file('value', 'constant')",
                counter.path().join("executions").display()
            );
            let f = environment_fixture(ordinary, "[]", &script);
            let run = || {
                let repos = RepositorySet::new(f.0.clone());
                let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                repos.root(&canonical).unwrap().to_path_buf()
            };
            let first = run();
            let warm = run();
            assert_eq!(warm, first);
            assert_eq!(std::fs::read(warm.join("value")).unwrap(), b"constant");
            assert_eq!(
                std::fs::read_to_string(counter.path().join("executions")).unwrap(),
                "run\n"
            );
            let trees = f.0.join("bazel-rs-out/repositories/direct/trees");
            let count = std::fs::read_dir(&trees)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
                .count();
            assert_eq!(count, 1, "warm demand published a new tree");
        }
    }

    #[test]
    fn unused_and_prepared_declarations_do_not_capture_environment() {
        use crate::repository_environment::tests::Reader;
        for ordinary in [false, true] {
            let f = environment_fixture(ordinary, "['SYNTHETIC']", "fail('must not run')");
            let reader = Reader::new(Ok(None));
            let repos = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
            let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            repos.mapping(&canonical).unwrap();
            if ordinary {
                let (prepared, _) = repos.ordinary_repository(&canonical).unwrap();
                assert!(
                    String::from_utf8_lossy(prepared.provenance().unwrap()).contains("environ")
                );
            } else {
                let runtime = repos.direct_runtime().unwrap();
                repos
                    .prepare_direct_group(&runtime.catalog.repositories[&canonical].group_id)
                    .unwrap();
            }
            assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
            assert!(!f.0.join("bazel-rs-out").exists());
        }
    }

    #[test]
    fn environment_capture_and_callback_errors_never_publish_and_demand_errors_are_cached() {
        use crate::repository_environment::{ReadError, tests::Reader};
        for ordinary in [false, true] {
            for reader_failure in [false, true] {
                let f = environment_fixture(
                    ordinary,
                    "['SYNTHETIC']",
                    "ctx.file('partial', 'safe')\n    ctx.getenv('SYNTHETIC')\n    fail('deliberate failure')",
                );
                let reader = Reader::new(if reader_failure {
                    Err(ReadError)
                } else {
                    Ok(Some("SyntheticSecret".into()))
                });
                let repos = RepositorySet::new(f.0.clone()).with_environment_reader(reader.clone());
                let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
                let error = repos.root(&canonical).unwrap_err();
                assert!(!error.contains("SyntheticSecret"));
                assert!(
                    error.contains(if reader_failure {
                        "environment read failed"
                    } else {
                        "deliberate failure"
                    }),
                    "{error}"
                );
                assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
                assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
                for subdir in ["trees", "manifests"] {
                    assert_eq!(
                        std::fs::read_dir(
                            f.0.join("bazel-rs-out/repositories/direct").join(subdir)
                        )
                        .unwrap()
                        .count(),
                        0
                    );
                }
                // The slot's lock files persist; nothing else does (no
                // repository directory either).
                assert!(
                    std::fs::read_dir(f.0.join("bazel-rs-out/repositories/direct/locks"))
                        .unwrap()
                        .all(|entry| entry
                            .unwrap()
                            .file_name()
                            .to_str()
                            .unwrap()
                            .starts_with("slot-"))
                );
                assert!(
                    std::fs::read_dir(f.0.join("bazel-rs-out/repositories/external"))
                        .map_or(true, |mut entries| entries.next().is_none())
                );
            }
        }
    }

    #[test]
    fn environment_provenance_omission_keeps_legacy_field_order_and_profile() {
        let f = environment_fixture(true, "[]", "return None");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
        let (prepared, repository) = repos.ordinary_repository(&canonical).unwrap();
        let before = serde_json::to_string(repository).unwrap();
        assert!(!before.contains("environ"));
        let legacy = serde_json::json!({
            "definition_identity": repository.definition.identity,
            "schema": repository.definition.schema,
            "local": repository.definition.local,
            "attributes": repository.attributes,
            "canonical_name": repository.canonical_name,
            "original_name": repository.original_name,
        });
        assert_eq!(before, serde_json::to_string(&legacy).unwrap());
        let mut changed = OrdinaryRepository {
            definition: repository.definition.clone(),
            attributes: repository.attributes.clone(),
            explicit: repository.explicit.clone(),
            canonical_name: repository.canonical_name.clone(),
            original_name: repository.original_name.clone(),
            root: DemandSlot::new(),
        };
        changed.definition.environ =
            crate::repository_environment::DeclaredEnvironment::from_names(
                [Ok("SYNTHETIC")],
                &mut crate::repository_budget::HostBudget::default(),
            )
            .unwrap();
        let mut with_environ = serde_json::to_value(&changed).unwrap();
        assert_eq!(
            with_environ
                .as_object_mut()
                .unwrap()
                .remove("environ")
                .unwrap(),
            serde_json::json!(["SYNTHETIC"])
        );
        assert_eq!(serde_json::to_string(&with_environ).unwrap(), before);
        let provenance: serde_json::Value =
            serde_json::from_slice(prepared.provenance().unwrap()).unwrap();
        assert_eq!(provenance["profile"], "ordinary-file-template-v1");
    }

    #[test]
    fn cloned_repository_handles_share_graph_and_fresh_configuration_does_not_mutate_them() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<RepositorySet>();
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='before')\n");
        let original = RepositorySet::new(f.0.clone()).with_source_cache("first".into());
        let cloned = original.clone();
        let graph = original.resolve_graph().unwrap();
        assert!(std::ptr::eq(graph, cloned.resolve_graph().unwrap()));
        f.write("MODULE.bazel", "module(name='after')\n");
        let fresh = cloned.with_source_cache("second".into());
        assert_ne!(original.state.runtime_id, fresh.state.runtime_id);
        assert!(fresh.state.direct.get().is_none());
        assert_eq!(
            original
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "before"
        );
        assert_eq!(
            fresh
                .resolve_graph()
                .unwrap()
                .root
                .module
                .as_ref()
                .unwrap()
                .name,
            "after"
        );
        assert_eq!(original.source_cache_path().unwrap(), f.0.join("first"));
        assert_eq!(fresh.source_cache_path().unwrap(), f.0.join("second"));
    }

    #[test]
    fn main_and_canonical_parsing_are_lazy_even_with_broken_module() {
        let f = Fixture::new();
        for module in [None, Some("not valid MODULE syntax !!!")] {
            if let Some(module) = module {
                f.write("MODULE.bazel", module);
            }
            let repos = RepositorySet::new(f.0.clone());
            assert_eq!(repos.root("").unwrap(), f.0);
            for input in ["//p:x", ":x", "@//p:x", "@@//p:x", "@@dep+//p:x"] {
                let bound = repos.resolution_size_bound(input, "", "p").unwrap();
                assert!(bound >= repos.parse(input, "", "p").unwrap().to_string().len());
            }
            assert!(repos.resolution_size_bound("//p:x", "dep+", "").is_ok());
            assert_eq!(repos.parse("//p:x", "dep+", "").unwrap().repository, "dep+");
            assert!(repos.state.resolved.get().is_none());
            assert!(repos.parse("@//p:x", "dep+", "").is_err());
            assert!(repos.parse("@dep//p:x", "", "").is_err());
            assert!(repos.state.resolved.get().is_some());
        }
    }

    #[test]
    fn local_overrides_have_strict_per_repository_aliases() {
        let f = Fixture::new();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='../dep')\n");
        f.write(
            "dep/MODULE.bazel",
            "module(name='dep',repo_name='self_dep')\n",
        );
        let repos = RepositorySet::new(f.0.join("main"));
        let label = repos.parse("@alias//same:x", "", "").unwrap();
        assert_eq!(label.repository, "dep+");
        assert_eq!(
            repos.root(&label.repository).unwrap(),
            std::fs::canonicalize(f.0.join("dep")).unwrap()
        );
        assert_eq!(repos.parse("@self_dep//same:x", "dep+", "").unwrap(), label);
        assert!(
            repos
                .parse("@//same:x", "dep+", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert_eq!(
            repos.parse("@@//same:x", "dep+", "").unwrap().repository,
            ""
        );
        assert_eq!(repos.parse("//same:x", "dep+", "").unwrap(), label);
        assert!(repos.parse("@alias//same:x", "dep+", "").is_err());
        assert!(repos.parse("@dep+//same:x", "", "").is_err());
        assert_eq!(repos.parse("@@dep+//same:x", "", "").unwrap(), label);
        assert_ne!(
            repos.parse("//same:x", "", "").unwrap().package_id(),
            label.package_id()
        );
        let visibility = crate::visibility::Visibility::parse_in(
            &["@alias//same:__pkg__".into()],
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &repos,
        )
        .unwrap();
        assert!(visibility.allows_in(
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &label.package_id()
        ));
        assert!(!visibility.allows_in(
            &Label::parse("//owner:x", "").unwrap().package_id(),
            &Label::parse("//same:x", "").unwrap().package_id()
        ));
        assert!(
            repos
                .root("unselected+")
                .unwrap_err()
                .contains("not materialized")
        );
    }

    #[test]
    fn graph_failure_is_cached_without_fallback() {
        let f = Fixture::new();
        let repos = RepositorySet::new(f.0.clone());
        let error = repos.resolve_graph().unwrap_err();
        f.write("MODULE.bazel", "module(name='main')\n");
        assert_eq!(repos.resolve_graph().unwrap_err(), error);
        assert!(repos.root("").is_ok());
    }

    #[test]
    fn direct_repository_aliases_resolve_without_definition_io_and_are_owner_scoped() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='root_generated')\n");
        f.write("registry/modules/dep/1/MODULE.bazel", "module(name='dep',version='1')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='dep_generated')\nr(name='ignored',dev_dependency=True)\n");
        assert!(repos.root("").is_ok());
        assert!(repos.state.resolved.get().is_none());
        assert_eq!(
            repos.parse("@dep//:file", "", "").unwrap().repository,
            "dep+"
        );
        for (name, owner, canonical) in [
            ("root_generated", "", "+r+root_generated"),
            ("dep_generated", "dep+", "dep++r+dep_generated"),
        ] {
            let label = repos.parse(&format!("@{name}//:file"), owner, "").unwrap();
            assert_eq!(label.repository, canonical);
            assert_eq!(repos.mapping(owner).unwrap()[name], canonical);
            assert_eq!(
                repos
                    .parse(&format!("@{name}//:file"), canonical, "")
                    .unwrap(),
                label
            );
            assert!(
                repos
                    .parse("@unknown//:file", canonical, "")
                    .unwrap_err()
                    .contains("unknown apparent repository")
            );
        }
        for (name, owner) in [
            ("dep_generated", ""),
            ("root_generated", "dep+"),
            ("ignored", "dep+"),
        ] {
            assert!(
                repos
                    .parse(&format!("@{name}//:file"), owner, "")
                    .unwrap_err()
                    .contains("unknown apparent repository")
            );
        }
        assert!(repos.state.registry_roots.get().is_none());
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn direct_definition_failure_is_demanded_and_cached_only_in_this_runtime() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "module(name='main')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='generated')\n",
        );
        f.write("BUILD", "");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@generated//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+r+generated");
        assert!(!f.0.join("bazel-rs-out").exists());
        let error = repos.root(&canonical).unwrap_err();
        assert!(error.contains("missing.bzl"), "{error}");
        f.write("missing.bzl", "r = 1\n");
        assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
        let fresh = repos.with_source_cache("another-cache".into());
        assert!(
            fresh
                .root(&canonical)
                .unwrap_err()
                .contains("expected a frozen repository_rule")
        );
        assert!(!f.0.join("another-cache").exists());
    }

    #[test]
    fn ordinary_import_aliases_are_metadata_only_and_owner_scoped() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\ne=use_extension('@dep//:absent.bzl','extension')\nuse_repo(e, local='generated')\nr=use_repo_rule('//:also_absent.bzl','r')\nr(name='direct')\n");
        f.write("registry/modules/dep/1/MODULE.bazel", "module(name='dep',version='1')\ne=use_extension('//:absent.bzl','extension')\nuse_repo(e, own='generated')\n");
        let label = repos.parse("@local//:file", "", "").unwrap();
        assert_eq!(label.repository, "dep++extension+generated");
        assert_eq!(repos.parse("@own//:file", "dep+", "").unwrap(), label);
        assert_eq!(repos.mapping("").unwrap()["local"], label.repository);
        assert_eq!(
            repos.mapping("+r+direct").unwrap()["local"],
            label.repository
        );
        assert!(
            repos
                .parse("@local//:file", "dep+", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(
            repos
                .parse("@own//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(repos.state.registry_roots.get().is_none());
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn ordinary_definition_failure_is_demanded_and_shared_by_root_and_mapping() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:absent.bzl','extension')\nuse_repo(e,'generated')\n",
        );
        f.write("BUILD", "");
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@generated//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+extension+generated");
        assert!(!f.0.join("bazel-rs-out").exists());
        let error = repos.root(&canonical).unwrap_err();
        assert!(error.contains("absent.bzl"), "{error}");
        f.write("absent.bzl", "extension = 1\n");
        assert_eq!(repos.mapping(&canonical).unwrap_err(), error);
        assert_eq!(repos.clone().root(&canonical).unwrap_err(), error);
        let fresh = repos.with_source_cache("fresh-cache".into());
        assert!(
            fresh
                .root(&canonical)
                .unwrap_err()
                .contains("expected a frozen module_extension")
        );
        assert!(!f.0.join("fresh-cache").exists());
    }

    #[test]
    fn unknown_ordinary_prefix_never_loads_or_executes_a_similar_extension() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'generated')\n",
        );
        f.write("BUILD", "");
        f.write("extension.bzl", "def implementation(ctx):\n    fail('callback must not run')\nextension = module_extension(implementation=implementation)\n");
        let repos = RepositorySet::new(f.0.clone());
        for canonical in [
            "+extension2+generated",
            "+extension+generated+suffix",
            "+extension+",
            "+other+generated",
        ] {
            assert!(repos.root(canonical).is_err());
            assert!(repos.mapping(canonical).is_err());
        }
        // Replacing the definition after unknown demands demonstrates that no
        // preparation result was cached for the real exact-prefix route.
        f.write("extension.bzl", "extension = 1\n");
        let error = repos.root("+extension+generated").unwrap_err();
        assert!(
            error.contains("expected a frozen module_extension"),
            "{error}"
        );
        assert!(!f.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn base_aliases_stay_lazy_and_isolated_imports_fail_only_on_demand() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\ne=use_extension('//:absent.bzl','extension',isolate=True)\nuse_repo(e,'generated')\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                isolated_extension_usages: true,
                ..Default::default()
            },
        );
        assert!(repos.resolution_size_bound("@main//:file", "", "").is_ok());
        assert_eq!(repos.parse("@main//:file", "", "").unwrap().repository, "");
        assert!(repos.state.direct.get().is_none());
        assert!(
            repos
                .resolution_size_bound("@unrelated//:file", "", "")
                .is_ok()
        );
        assert!(
            repos
                .parse("@unrelated//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(repos.state.direct.get().is_none());
        assert!(
            repos
                .resolution_size_bound("@generated//:file", "", "")
                .unwrap_err()
                .contains("isolated module extensions unsupported")
        );
        assert!(
            repos
                .parse("@generated//:file", "", "")
                .unwrap_err()
                .contains("isolated module extensions unsupported")
        );
        assert!(repos.state.direct.get().is_none());
        let ignored = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                ignore_dev_dependencies: true,
                isolated_extension_usages: true,
                ..Default::default()
            },
        );
        f.write("MODULE.bazel", "e=use_extension('//:absent.bzl','extension',isolate=True,dev_dependency=True)\nuse_repo(e,'generated')\n");
        assert!(
            ignored
                .parse("@generated//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(ignored.state.direct.get().is_none());
    }

    #[test]
    fn unused_invalid_definition_label_stays_lazy_until_root_demand() {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            "r=use_repo_rule('//bad/../:defs.bzl','maker')\nr(name='unused')\n",
        );
        let repos = RepositorySet::new(f.0.clone());
        let canonical = repos.parse("@unused//:file", "", "").unwrap().repository;
        assert_eq!(canonical, "+maker+unused");
        assert!(!f.0.join("bazel-rs-out").exists());
        assert!(repos.root(&canonical).is_err());
    }

    #[test]
    fn ignored_root_direct_repositories_do_not_enter_namespace() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "module(name='main')\nr=use_repo_rule('//:missing.bzl','r')\nr(name='ignored',dev_dependency=True)\n");
        let repos = RepositorySet::with_options(
            f.0.clone(),
            ResolverOptions {
                ignore_dev_dependencies: true,
                ..Default::default()
            },
        );
        assert!(
            repos
                .parse("@ignored//:file", "", "")
                .unwrap_err()
                .contains("unknown apparent repository")
        );
        assert!(
            repos
                .resolve_graph()
                .unwrap()
                .root
                .extension_usages
                .is_empty()
        );
    }

    #[test]
    fn registry_metadata_is_never_a_repository_root() {
        let f = Fixture::new();
        f.write(
            "main/MODULE.bazel",
            "module(name='main')\nbazel_dep(name='dep',version='1')\n",
        );
        f.write(
            "registry/modules/dep/1/MODULE.bazel",
            "module(name='dep',version='1')\n",
        );
        let repos = RepositorySet::with_options(
            f.0.join("main"),
            ResolverOptions {
                registries: vec![f.0.join("registry").into()],
                ..ResolverOptions::default()
            },
        );
        let label = repos.parse("@dep//p:x", "", "").unwrap();
        assert!(
            repos
                .root(&label.repository)
                .unwrap_err()
                .contains("not materialized")
        );
    }

    #[test]
    fn registry_absolute_local_path_is_live_and_registry_module_is_authoritative() {
        let f = Fixture::new();
        let repos = f.registry();
        // Outside the main workspace, and no valid source MODULE is required.
        f.write(
            "live/MODULE.bazel",
            "this source MODULE must never be evaluated !!!",
        );
        f.write("live/BUILD", "exports_files(['data'])");
        f.write("live/data", "before");
        f.source(serde_json::json!({"type":"local_path", "path": f.0.join("live")}));
        let root = repos.root("dep+").unwrap();
        assert_eq!(root, std::fs::canonicalize(f.0.join("live")).unwrap());
        assert_eq!(repos.resolve_graph().unwrap().modules["dep+"].name, "dep");
        assert_eq!(repos.resolve_graph().unwrap().modules["dep+"].version, "1");
        f.write("live/data", "after");
        assert_eq!(std::fs::read_to_string(root.join("data")).unwrap(), "after");
        // Both success and failure are cached, without reevaluating descriptors.
        f.source(serde_json::json!({"type":"archive"}));
        assert_eq!(repos.root("dep+").unwrap(), root);
    }

    #[test]
    fn local_path_sources_reject_override_transformations_without_mutating_live_tree() {
        for options in [
            "patches=[':fix.patch'],patch_strip=1",
            "patch_cmds=['touch command-ran']",
            "patches=['@@external+//:missing.patch']",
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("main/MODULE.bazel", &format!("bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',{options})\n"));
            f.write("main/BUILD", "");
            f.write(
                "main/fix.patch",
                "--- a/data\n+++ b/data\n@@ -1 +1 @@\n-before\n+after\n",
            );
            f.write("live/data", "before\n");
            f.write("live/MODULE.bazel", "invalid live MODULE !!!");
            f.source(serde_json::json!({"type":"local_path", "path":f.0.join("live")}));
            assert_eq!(repos.resolve_graph().unwrap().modules.len(), 1);
            assert!(repos.state.registry_roots.get().is_none());
            let error = repos.root("dep+").unwrap_err();
            assert!(
                error.contains("unsupported for local_path source"),
                "{error}"
            );
            assert_eq!(std::fs::read(f.0.join("live/data")).unwrap(), b"before\n");
            assert_eq!(
                std::fs::read(f.0.join("live/MODULE.bazel")).unwrap(),
                b"invalid live MODULE !!!"
            );
            assert!(!f.0.join("main/command-ran").exists());
            assert!(!f.0.join("main/bazel-rs-out").exists());
        }
    }

    #[test]
    fn strip_only_override_keeps_local_path_sources_live() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',patch_strip=3)\n");
        f.write("live/data", "before");
        f.source(serde_json::json!({"type":"local_path", "path":f.0.join("live")}));
        let root = repos.root("dep+").unwrap();
        f.write("live/data", "after");
        assert_eq!(std::fs::read(root.join("data")).unwrap(), b"after");
        assert!(!f.0.join("main/bazel-rs-out").exists());
    }

    #[test]
    fn archive_commands_and_external_patches_fail_before_fetch_or_cache_creation() {
        for (options, diagnostic) in [
            ("patch_cmds=['touch command-ran']", "patch_cmds"),
            (
                "patches=['@@external+//missing:fix.patch']",
                "canonical external patch",
            ),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("main/MODULE.bazel", &format!("bazel_dep(name='dep',version='1')\nsingle_version_override(module_name='dep',{options})\n"));
            // Deliberately invalid descriptor: unsupported local operations must
            // be diagnosed before archive parsing, fetching or cache creation.
            f.source(serde_json::json!({"type":"archive"}));
            assert_eq!(repos.resolve_graph().unwrap().modules.len(), 1);
            let error = repos.root("dep+").unwrap_err();
            assert!(error.contains(diagnostic), "{error}");
            assert!(!f.0.join("main/command-ran").exists());
            assert!(!f.0.join("main/bazel-rs-out").exists());
        }
    }

    #[test]
    fn registry_relative_paths_use_absolute_or_registry_relative_base() {
        for absolute in [false, true] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("sources/dep/BUILD", "");
            f.source(serde_json::json!({"type":"local_path", "path":"dep"}));
            let base = if absolute {
                f.0.join("sources")
            } else {
                PathBuf::from("../sources")
            };
            f.write(
                "registry/bazel_registry.json",
                &serde_json::json!({
                    "module_base_path": base, "mirrors": ["https://unused.invalid/"]
                })
                .to_string(),
            );
            assert_eq!(
                repos.root("dep+").unwrap(),
                std::fs::canonicalize(f.0.join("sources/dep")).unwrap()
            );
        }
    }

    #[test]
    fn unused_registry_sources_remain_unmaterialized() {
        let f = Fixture::new();
        let repos = f.registry();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nbazel_dep(name='local',version='1')\nlocal_path_override(module_name='local',path='../local')\n");
        f.write("local/MODULE.bazel", "module(name='local')\n");
        // Invalid before any download is attempted, even with archive support.
        f.source(serde_json::json!({"type":"archive", "url":17}));
        assert!(repos.root("").is_ok());
        assert!(repos.state.resolved.get().is_none());
        assert!(repos.root("local+").is_ok());
        assert!(repos.state.registry_roots.get().is_none());
        assert!(repos.root("dep+").unwrap_err().contains("url"));
    }

    #[test]
    fn source_descriptors_reject_unsupported_or_malformed_values() {
        for descriptor in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"type":null}),
            serde_json::json!({"type":"archive"}),
            serde_json::json!({"type":"git"}),
            serde_json::json!({"type":"unknown"}),
            serde_json::json!({"type":"local_path"}),
            serde_json::json!({"type":"local_path","path":null}),
            serde_json::json!({"type":"local_path","path":7}),
            serde_json::json!({"type":"local_path","path":""}),
            serde_json::json!({"type":"local_path","path":"bad\u{0000}path"}),
            serde_json::json!({"type":"local_path","path":".","patches":{}}),
            serde_json::json!({"type":"local_path","path":".","unknown":true}),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.source(descriptor.clone());
            assert!(repos.root("dep+").is_err(), "accepted {descriptor}");
        }
        let f = Fixture::new();
        let repos = f.registry();
        f.write("registry/modules/dep/1/source.json", "{invalid JSON");
        assert!(
            repos
                .root("dep+")
                .unwrap_err()
                .contains("invalid registry JSON")
        );
    }

    #[test]
    fn relative_source_requires_valid_registry_base_configuration() {
        for config in [
            None,
            Some("null"),
            Some("[]"),
            Some("{"),
            Some("{}"),
            Some("{\"module_base_path\":null}"),
            Some("{\"module_base_path\":5}"),
            Some("{\"module_base_path\":\"bad\\u0000path\"}"),
        ] {
            let f = Fixture::new();
            let repos = f.registry();
            f.source(serde_json::json!({"type":"local_path","path":"dep"}));
            if let Some(config) = config {
                f.write("registry/bazel_registry.json", config);
            }
            assert!(repos.root("dep+").is_err(), "accepted {config:?}");
        }
    }

    #[test]
    fn local_source_requires_existing_directory_and_caches_errors() {
        for target in ["missing", "file"] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("file", "not a directory");
            f.source(serde_json::json!({"type":"local_path", "path":f.0.join(target)}));
            let error = repos.root("dep+").unwrap_err();
            f.source(serde_json::json!({"type":"local_path", "path":f.0}));
            assert_eq!(repos.root("dep+").unwrap_err(), error);
        }
    }

    #[test]
    fn selected_registry_source_does_not_fall_back_to_another_registry() {
        let f = Fixture::new();
        let _ = f.registry();
        f.write(
            "other/modules/dep/1/MODULE.bazel",
            "module(name='dep',version='1')\n",
        );
        f.write(
            "other/modules/dep/1/source.json",
            &serde_json::json!({"type":"local_path","path":f.0}).to_string(),
        );
        let repos = RepositorySet::with_options(
            f.0.join("main"),
            ResolverOptions {
                registries: vec![f.0.join("registry").into(), f.0.join("other").into()],
                ..ResolverOptions::default()
            },
        );
        let error = repos.root("dep+").unwrap_err();
        assert!(
            error.contains("registry/modules/dep/1/source.json"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn registry_metadata_symlinks_are_rejected_but_declared_source_symlink_is_allowed() {
        use std::os::unix::fs::symlink;
        for config in [false, true] {
            let f = Fixture::new();
            let repos = f.registry();
            f.write("elsewhere.json", "{}");
            let path = if config {
                f.source(serde_json::json!({"type":"local_path","path":"dep"}));
                "registry/bazel_registry.json"
            } else {
                "registry/modules/dep/1/source.json"
            };
            symlink(f.0.join("elsewhere.json"), f.0.join(path)).unwrap();
            assert!(
                repos
                    .root("dep+")
                    .unwrap_err()
                    .contains("symlink registry paths")
            );
        }
        let f = Fixture::new();
        let repos = f.registry();
        f.write("live/BUILD", "");
        symlink(f.0.join("live"), f.0.join("live-link")).unwrap();
        f.source(serde_json::json!({"type":"local_path","path":f.0.join("live-link")}));
        assert_eq!(
            repos.root("dep+").unwrap(),
            std::fs::canonicalize(f.0.join("live")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_descriptor_checks_ancestor_components_after_graph_resolution() {
        let f = Fixture::new();
        let repos = f.registry();
        repos.resolve_graph().unwrap();
        std::fs::rename(f.0.join("registry/modules/dep/1"), f.0.join("relocated")).unwrap();
        std::os::unix::fs::symlink(f.0.join("relocated"), f.0.join("registry/modules/dep/1"))
            .unwrap();
        assert!(
            repos
                .root("dep+")
                .unwrap_err()
                .contains("symlink registry paths")
        );
    }

    #[test]
    fn link_blocked_prefixes_cover_the_store_and_the_cli_cache_root() {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cli =
            RepositorySet::new(root.clone()).with_source_cache(root.join("cache/repositories"));
        assert_eq!(
            cli.link_blocked_prefixes(),
            [root.join("cache/repositories"), root.join("cache")]
        );
        let other = RepositorySet::new(root.clone()).with_source_cache(root.join("store"));
        assert_eq!(other.link_blocked_prefixes(), [root.join("store")]);
    }

    /// The no-demand guard on a module repository's materialization slot
    /// (review §1.2): a materialized root is served under the guard; an
    /// unfetched one fails with the sentinel instead of fetching, and a
    /// later unguarded demand materializes it.
    #[test]
    fn guarded_root_slot_honors_the_no_demand_guard() {
        use std::sync::OnceLock;
        let materialized = OnceLock::new();
        materialized.set(Ok(PathBuf::from("/ready"))).unwrap();
        assert_eq!(
            guarded_root_slot(&materialized, "ready", || Ok(PathBuf::from("/never"))),
            Ok(Path::new("/ready"))
        );
        {
            let _guard = crate::repository_demand::no_demand();
            let error = guarded_root_slot(&OnceLock::new(), "fresh", || Ok(PathBuf::from("/x")))
                .unwrap_err();
            assert!(
                crate::repository_demand::is_needs_repository(&error),
                "{error}"
            );
            // A ready value still serves under the guard.
            assert_eq!(
                guarded_root_slot(&materialized, "ready", || Ok(PathBuf::from("/never"))),
                Ok(Path::new("/ready"))
            );
        }
        assert_eq!(
            guarded_root_slot(&OnceLock::new(), "fresh", || Ok(PathBuf::from("/x"))).unwrap(),
            Path::new("/x")
        );
    }

    /// Pinned mode (crate::snapshot::replay, PLAN-L2 update 1 §(a)): roots,
    /// mappings, label parsing, link policy and definition loads answer from
    /// the tables; MODULE.bazel is never read, the graph never resolved,
    /// the catalog never built; anything unpinned is an error.
    #[test]
    fn pinned_sets_answer_from_tables_never_resolve_and_refuse_unpinned_repositories() {
        use crate::snapshot::replay::{PinnedMapping, PinnedRoot, PinnedTables};
        let f = Fixture::new();
        // Evaluating this MODULE.bazel would fail: a pinned set never reads it.
        f.write(
            "main/MODULE.bazel",
            "fail('a pinned set reads no MODULE.bazel')\n",
        );
        f.write("main/BUILD", "");
        f.write("dep/BUILD", "");
        f.write(
            "dep/defs.bzl",
            "load('@dep//:more.bzl', 'VALUE')\ndef _impl(ctx):\n    ctx.file('value', VALUE)\nR = repository_rule(implementation = _impl, local = True)\n",
        );
        f.write("dep/more.bzl", "VALUE = 'from dep'\n");
        let mapping = |entries: &[(&str, &str)]| PinnedMapping {
            mapping: entries
                .iter()
                .map(|(apparent, canonical)| ((*apparent).to_owned(), (*canonical).to_owned()))
                .collect(),
            isolated: BTreeSet::new(),
        };
        let tables = PinnedTables {
            main_root: f.0.join("main"),
            source_cache: f.0.join("cache"),
            roots: BTreeMap::from([(
                "dep+".to_owned(),
                PinnedRoot {
                    path: f.0.join("dep"),
                    generated: false,
                },
            )]),
            mappings: BTreeMap::from([
                (String::new(), mapping(&[("dep", "dep+")])),
                ("dep+".to_owned(), mapping(&[("dep", "dep+")])),
            ]),
        };
        let set = Arc::new(RepositorySet::pinned(
            tables,
            ResolverOptions::default(),
            crate::repository_environment::Snapshot::default(),
            true,
        ));
        assert_eq!(set.root("").unwrap(), f.0.join("main"));
        assert_eq!(set.root("dep+").unwrap(), f.0.join("dep"));
        assert_eq!(
            set.parse("@dep//:defs.bzl", "", "").unwrap().repository,
            "dep+"
        );
        assert_eq!(set.parse("//:x", "dep+", "").unwrap().repository, "dep+");
        assert!(
            set.parse("@absent//:x", "", "")
                .unwrap_err()
                .starts_with("unknown apparent repository")
        );
        for error in [
            set.root("other+").unwrap_err(),
            set.mapping("other+").unwrap_err(),
            set.parse("@dep//:x", "other+", "").unwrap_err(),
            set.resolve_graph().map(|_| ()).unwrap_err(),
            set.validate_root_module().unwrap_err(),
        ] {
            assert!(error.contains("not pinned"), "{error}");
        }
        assert!(!set.is_repository_rule_generated("dep+"));
        assert_eq!(
            set.materialized_roots(),
            vec![
                (String::new(), f.0.join("main")),
                ("dep+".to_owned(), f.0.join("dep"))
            ]
        );
        // A definition loads through pinned roots and mappings only.
        let loaded = loading::load_module_extension_in(
            set.clone(),
            &Label::parse_canonical("@@dep+//:defs.bzl").unwrap(),
            "R",
            &crate::package::module_bzl_globals(),
        )
        .unwrap();
        assert!(repository_rule::unpack(&loaded.value).unwrap().local);
        assert_eq!(loaded.sources.len(), 2);
        assert!(set.state.root_module.get().is_none());
        assert!(set.state.resolved.get().is_none());
        assert!(set.state.direct.get().is_none());
        assert!(set.state.registry_roots.get().is_none());
        assert!(!f.0.join("cache").exists());
    }
}
