//! Incremental analysis on a snapshot miss (docs/INCREMENTAL_ANALYSIS.md,
//! tier A).
//!
//! A full analysis that the snapshot saves also records a dependency index
//! (the INDEX section, [`Index`]; crate::engine's tracker): the configured
//! targets with their dependency edges, the packages each one looked up,
//! the packages and targets shared computations reached, and a fingerprint
//! of every main-repository package. When a later lookup misses only
//! because inputs inside some main-repository packages changed (a BUILD
//! file, a glob listing, a source file's existence), the run re-loads just
//! those packages: an unchanged fingerprint (a comment edit) changes
//! nothing; a changed one dirties the package's configured targets and
//! everything that reaches them, and only the requested targets among them
//! are analyzed again — their forward closure, in a fresh engine — and
//! spliced into the saved plan. Anything the splice cannot attribute falls
//! back to the full analysis: before any analysis ran, in this process;
//! after, by executing this binary again (`BAZEL_RS_INCREMENTAL_REEXEC`),
//! so nothing the partial analysis did — printed, cached, counted — leaks
//! into the full one.

use crate::label::Label;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The environment variable a fallback after a partial analysis executes
/// this binary again with (read and removed at startup: never seen by
/// repository rules or actions): `fallback` runs the full analysis without
/// a lookup; `verify` (verify mode, after a successful splice was saved)
/// runs the normal verify-mode lookup, whose hit — the spliced snapshot —
/// is checked against a full analysis.
pub const REEXEC_VAR: &str = "BAZEL_RS_INCREMENTAL_REEXEC";
/// [`REEXEC_VAR`]'s value of a fallback.
pub const REEXEC_FALLBACK: &str = "fallback";
/// [`REEXEC_VAR`]'s value of verify mode after a successful splice.
pub const REEXEC_VERIFY: &str = "verify";

/// `BAZEL_RS_SNAPSHOT_INCREMENTAL=0` turns incremental analysis off (every
/// miss runs the full analysis, as before).
pub const SWITCH_VAR: &str = "BAZEL_RS_SNAPSHOT_INCREMENTAL";

/// How this process was started (see [`REEXEC_VAR`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reexec {
    /// A normal start.
    None,
    /// A fallback: no lookup, the full analysis.
    Fallback,
    /// Verify mode after a successful splice: look up (and verify) the
    /// spliced snapshot.
    Verify,
}

static REEXEC: std::sync::OnceLock<Reexec> = std::sync::OnceLock::new();

/// Read and remove [`REEXEC_VAR`] (call once, at startup, before any
/// thread starts).
pub fn take_reexec_marker() {
    let value = std::env::var_os(REEXEC_VAR);
    if value.is_some() {
        // SAFETY: called at the start of main, before any thread is
        // spawned (no concurrent environment access).
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var(REEXEC_VAR);
        }
    }
    let reexec = match value.as_deref().and_then(|value| value.to_str()) {
        Some("fallback") => Reexec::Fallback,
        Some("verify") => Reexec::Verify,
        _ => Reexec::None,
    };
    let _ = REEXEC.set(reexec);
}

/// The original stderr of a re-executed process while it is silenced
/// ([`silence_until_check_point`]).
static SILENCED: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// A re-executed process ([`REEXEC_VAR`]) repeats what the first one did
/// up to the snapshot check point — and what it printed there, which is on
/// stderr already (the memory limit note, option warnings): its stderr goes
/// to `/dev/null` until [`restore_after_check_point`].
pub fn silence_until_check_point() {
    use std::os::fd::AsFd;
    if reexec() == Reexec::None {
        return;
    }
    let Ok(saved) = std::io::stderr().as_fd().try_clone_to_owned() else {
        return;
    };
    let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
        return;
    };
    if rustix::stdio::dup2_stderr(&null).is_ok() {
        *SILENCED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::fs::File::from(saved));
    }
}

/// The check point: a silenced process's stderr is its own again.
pub fn restore_after_check_point() {
    let saved = SILENCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(saved) = saved {
        let _ = rustix::stdio::dup2_stderr(&saved);
    }
}

/// How this process was started.
pub fn reexec() -> Reexec {
    REEXEC.get().copied().unwrap_or(Reexec::None)
}

/// Whether incremental analysis is switched on ([`SWITCH_VAR`]): full
/// analyses record the dependency index.
pub fn switched_on() -> bool {
    !std::env::var_os(SWITCH_VAR)
        .is_some_and(|value| value == "0" || value == "off" || value == "false")
}

/// Whether incremental analysis may run in this process.
pub fn enabled() -> bool {
    reexec() == Reexec::None && switched_on()
}

/// `text` on one line (a diagnostics line: newlines escaped).
pub fn one_line(text: &str) -> String {
    text.replace('\n', "\\n")
}

/// A configured target (or, without a configuration, a source file) of the
/// index.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexKey {
    pub label: Label,
    pub config: Option<[u8; 32]>,
    pub apply: bool,
}

/// What a package's load produced, as far as analysis and the target
/// pattern phase can read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Every rule with its load-time attributes, the exports, the package
    /// arguments and loads.
    pub digest: [u8; 32],
    /// What the target pattern phase reads: the rules' names, classes,
    /// tags and test attributes.
    pub summary: [u8; 32],
    /// The package holds a target of a kind shared computations read
    /// (package groups, toolchains, platforms, constraints, flags).
    pub shared_kinds: bool,
    /// What the package's own arguments, exports, defaults and loads
    /// digest to (`digest` but the rules).
    pub package: [u8; 32],
    /// Every rule's name and output names with the rule's digest (its
    /// part of `digest`; 16 bytes), sorted: a change of some rules only
    /// dirties what looked those targets up.
    pub targets: Vec<(String, [u8; 16])>,
}

/// One package the analysis touched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPackage {
    pub repository: String,
    pub package: String,
    /// Looked up by a shared computation or outside every target.
    pub global: bool,
    /// Why it is global (diagnostics).
    pub why: Option<String>,
    /// Loaded or looked up by the target pattern phase.
    pub pattern: bool,
    /// Its BUILD file printed when it loaded.
    pub printed: bool,
    /// Loaded (committed) by the analysis.
    pub loaded: bool,
    /// Main-repository packages only.
    pub fingerprint: Option<Fingerprint>,
    /// Its BUILD file's direct loads among [`Index::modules`].
    pub loads: Vec<u32>,
}

/// A `.bzl` module of the index: a main-repository module, or one loading
/// one (transitively). Edits of main-repository modules dirty the packages
/// loading them and the targets whose rules they define.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexModule {
    pub label: Label,
    /// Its direct loads among [`Index::modules`].
    pub loads: Vec<u32>,
    /// Entered by a definition load (a repository rule, a module extension,
    /// an aspect's identity): its edits are not attributable.
    pub definition: bool,
    /// The [`code_digest`] of what the analysis loaded (main-repository
    /// modules read once, or with the same code): an edit leaving it is
    /// one of comments only.
    pub code: Option<[u8; 32]>,
}

/// The dependency index of one full analysis (the INDEX section).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// The main repository's root, as the analysis named it.
    #[serde(with = "path_bytes")]
    pub main_root: PathBuf,
    /// The top-level configuration.
    pub top: [u8; 32],
    /// The requested labels, in order.
    pub labels: Vec<Label>,
    pub keys: Vec<IndexKey>,
    /// (parent, child) key ids, in first-seen order.
    pub edges: Vec<(u32, u32)>,
    /// (key, package) ids: the packages each key's analysis looked up.
    pub accesses: Vec<(u32, u32)>,
    /// (key, package, target name): the main-repository targets each key's
    /// analysis looked up (the same lookups as `accesses`).
    pub label_accesses: Vec<(u32, u32, String)>,
    pub packages: Vec<IndexPackage>,
    /// (requested label position, key): the key each requested label's
    /// analysis began with.
    pub roots: Vec<(u32, u32)>,
    /// Keys reached by shared computations (sorted).
    pub shared: Vec<u32>,
    /// Keys whose analysis printed (sorted): the window's lines are not
    /// attributed further, so such a key is never re-analyzed.
    pub printed: Vec<u32>,
    /// (key, aspect application): the aspect applications each key's
    /// planning requested (their distinct number is the window's count).
    pub aspects: Vec<(u32, String)>,
    /// The recorded inputs of repositories and extensions
    /// (crate::observed_fs::note_repository_input): (path, a tree, the
    /// repository whose execution or warm check named it).
    pub repository_inputs: Vec<(PathBufWire, bool, Option<String>)>,
    /// The roots of the repositories `repository_inputs` names.
    pub repository_roots: Vec<(String, PathBufWire)>,
    /// The `.bzl` modules edits of which incremental analysis attributes
    /// ([`IndexModule`]).
    pub modules: Vec<IndexModule>,
    /// (key, module): the modules defining a configured target's rule
    /// (the `.bzl` calling `rule()` and the one being initialized).
    pub rule_modules: Vec<(u32, u32)>,
    /// (key, entry, position): the targets phase 1 planned, with the number
    /// of targets planned when their analysis began and their position in
    /// the plan (its post-order).
    pub planned: Vec<(u32, u32, u32)>,
    /// The plan length when each requested label's analysis began (by
    /// position), then at the end of phase 1.
    pub root_entries: Vec<u32>,
    /// (requested label position, or `u32::MAX` for the workspace status;
    /// action id): the producers of the hidden top-level artifacts (with
    /// the reported outputs, the roots of the plan's reachability).
    pub demand: Vec<(u32, String)>,
    /// The plain `build` target pattern phase (no tag filters, tests or
    /// test suites): the (pattern position, package) its wildcards
    /// expanded; `None`: another one, the requested labels never change.
    pub patterns: Option<Vec<(u32, u32)>>,
    /// The packages the pattern phase's test suite expansions read: their
    /// requested labels never change incrementally.
    pub suite_packages: Vec<u32>,
}

/// The digest of a `.bzl` file's code: its bytes with every comment's text
/// removed (a `#` outside string literals up to the end of its line), all
/// other bytes — whitespace and newlines included — in place, but the
/// whitespace ending the file. Two files with equal code digests lex to the
/// same tokens at the same positions: their evaluation is the same (an
/// edit of comments only, or of blank lines after the code).
pub fn code_digest(bytes: &[u8]) -> [u8; 32] {
    let mut code: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    let mut start = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'#' => {
                code.extend_from_slice(&bytes[start..at]);
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                start = at;
            }
            quote @ (b'\'' | b'"') => {
                let triple = bytes.get(at + 1) == Some(&quote) && bytes.get(at + 2) == Some(&quote);
                at += if triple { 3 } else { 1 };
                loop {
                    match bytes.get(at) {
                        None => break,
                        Some(b'\\') => at += 2,
                        Some(b'\n') if !triple => break,
                        Some(byte) if *byte == quote => {
                            if !triple {
                                at += 1;
                                break;
                            }
                            if bytes.get(at + 1) == Some(&quote)
                                && bytes.get(at + 2) == Some(&quote)
                            {
                                at += 3;
                                break;
                            }
                            at += 1;
                        }
                        Some(_) => at += 1,
                    }
                }
                continue;
            }
            _ => at += 1,
        }
    }
    code.extend_from_slice(&bytes[start.min(bytes.len())..]);
    // Blank lines after the code move nothing.
    while code.last().is_some_and(u8::is_ascii_whitespace) {
        code.pop();
    }
    *blake3::hash(&code).as_bytes()
}

/// The code digests of the main-repository modules this process read
/// ([`code_digest`]); `None`: read twice with different code.
static MODULE_CODE: std::sync::Mutex<Option<std::collections::HashMap<Label, Option<[u8; 32]>>>> =
    std::sync::Mutex::new(None);

/// The loader read the main-repository module `label`.
pub fn note_module_code(label: &Label, bytes: &[u8]) {
    let digest = code_digest(bytes);
    let mut code = MODULE_CODE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = code
        .get_or_insert_with(Default::default)
        .entry(label.clone())
        .or_insert(Some(digest));
    if *entry != Some(digest) {
        *entry = None;
    }
}

/// The code digests of the modules this process read.
pub fn module_code() -> std::collections::HashMap<Label, Option<[u8; 32]>> {
    MODULE_CODE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// The stand-in module of a rule whose defining modules are unknown: an
/// index naming it attributes no `.bzl` edit.
pub fn unattributed_module() -> Label {
    Label {
        repository: String::new(),
        package: String::new(),
        name: "<unattributed>".to_owned(),
    }
}

/// The module graph as labels (crate::engine builds it from the loaded
/// packages' load graphs; [`set_modules`] stores it).
#[derive(Clone, Debug, Default)]
pub struct ModuleGraph {
    /// Every module's direct loads.
    pub loads: std::collections::HashMap<Label, Vec<Label>>,
    /// Modules a definition load entered.
    pub definitions: std::collections::HashSet<Label>,
    /// The code digests of the main-repository modules read
    /// ([`module_code`]).
    pub code: std::collections::HashMap<Label, Option<[u8; 32]>>,
}

/// Store `graph` in `index`: the main-repository modules and those loading
/// them (transitively), `package_loads` each package's direct loads (by
/// its position in `index.packages`), `rules` each key's rule modules.
pub fn set_modules(
    index: &mut Index,
    graph: &ModuleGraph,
    package_loads: &[(u32, Vec<Label>)],
    rules: &[(u32, Vec<Label>)],
) {
    use std::collections::{BTreeSet, HashMap};
    // Relevant: main-repository modules, and modules reaching one.
    let mut parents: HashMap<&Label, Vec<&Label>> = HashMap::new();
    let mut all: BTreeSet<&Label> = BTreeSet::new();
    for (module, loads) in &graph.loads {
        all.insert(module);
        for load in loads {
            all.insert(load);
            parents.entry(load).or_default().push(module);
        }
    }
    all.extend(graph.definitions.iter());
    for (_, loads) in package_loads.iter().chain(rules) {
        all.extend(loads.iter());
    }
    let mut relevant: BTreeSet<&Label> = all
        .iter()
        .copied()
        .filter(|label| label.repository.is_empty())
        .collect();
    let mut todo: Vec<&Label> = relevant.iter().copied().collect();
    while let Some(module) = todo.pop() {
        for parent in parents.get(module).into_iter().flatten() {
            if relevant.insert(parent) {
                todo.push(parent);
            }
        }
    }
    let ids: HashMap<&Label, u32> = relevant
        .iter()
        .enumerate()
        .map(|(id, label)| (*label, id as u32))
        .collect();
    let id_list = |loads: &[Label]| -> Vec<u32> {
        let mut seen = BTreeSet::new();
        loads
            .iter()
            .filter_map(|load| ids.get(load).copied())
            .filter(|id| seen.insert(*id))
            .collect()
    };
    index.modules = relevant
        .iter()
        .map(|label| IndexModule {
            label: (*label).clone(),
            loads: graph
                .loads
                .get(*label)
                .map(|loads| id_list(loads))
                .unwrap_or_default(),
            definition: graph.definitions.contains(*label),
            code: graph.code.get(*label).copied().flatten(),
        })
        .collect();
    for package in &mut index.packages {
        package.loads.clear();
    }
    for (package, loads) in package_loads {
        if let Some(package) = index.packages.get_mut(*package as usize) {
            package.loads = id_list(loads);
        }
    }
    let mut rule_modules: BTreeSet<(u32, u32)> = BTreeSet::new();
    for (key, modules) in rules {
        for module in id_list(modules) {
            rule_modules.insert((*key, module));
        }
    }
    index.rule_modules = rule_modules.into_iter().collect();
}

/// Refresh the modules of `index` (an incremental save's, after
/// [`merge_index`]) with what this run loaded: `now`'s modules' loads and
/// definitions, the direct loads of the packages it loaded, the rule
/// modules of the keys it re-analyzed.
pub fn refresh_modules(
    index: &mut Index,
    now: &ModuleGraph,
    package_loads: &[(crate::label::PackageId, Vec<Label>)],
    rules: &[(u32, Vec<Label>)],
) {
    use std::collections::{BTreeMap, HashMap};
    let mut graph = module_graph(index);
    for (module, loads) in &now.loads {
        graph.loads.insert(module.clone(), loads.clone());
    }
    graph.definitions.extend(now.definitions.iter().cloned());
    for (module, code) in &now.code {
        graph.code.insert(module.clone(), *code);
    }
    let label = |id: &u32| index.modules[*id as usize].label.clone();
    let mut loads: BTreeMap<u32, Vec<Label>> = index
        .packages
        .iter()
        .enumerate()
        .map(|(id, package)| (id as u32, package.loads.iter().map(label).collect()))
        .collect();
    let positions: HashMap<(&str, &str), u32> = index
        .packages
        .iter()
        .enumerate()
        .map(|(id, package)| {
            (
                (package.repository.as_str(), package.package.as_str()),
                id as u32,
            )
        })
        .collect();
    for (package, direct) in package_loads {
        if let Some(id) = positions.get(&(package.repository.as_str(), package.package.as_str())) {
            loads.insert(*id, direct.clone());
        }
    }
    let mut rule_modules: BTreeMap<u32, Vec<Label>> = BTreeMap::new();
    for (key, module) in &index.rule_modules {
        rule_modules.entry(*key).or_default().push(label(module));
    }
    for (key, modules) in rules {
        rule_modules.insert(*key, modules.clone());
    }
    let loads: Vec<(u32, Vec<Label>)> = loads.into_iter().collect();
    let rules: Vec<(u32, Vec<Label>)> = rule_modules.into_iter().collect();
    set_modules(index, &graph, &loads, &rules);
}

/// The module graph `index` stores, as labels.
pub fn module_graph(index: &Index) -> ModuleGraph {
    let label = |id: &u32| index.modules[*id as usize].label.clone();
    ModuleGraph {
        loads: index
            .modules
            .iter()
            .map(|module| {
                (
                    module.label.clone(),
                    module.loads.iter().map(label).collect(),
                )
            })
            .collect(),
        definitions: index
            .modules
            .iter()
            .filter(|module| module.definition)
            .map(|module| module.label.clone())
            .collect(),
        code: index
            .modules
            .iter()
            .map(|module| (module.label.clone(), module.code))
            .collect(),
    }
}

/// A path as raw bytes on the wire (non-UTF-8 survives).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PathBufWire(#[serde(with = "path_bytes")] pub PathBuf);

mod path_bytes {
    use serde::{Deserializer, Serializer};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    #[allow(clippy::ptr_arg)] // serde's `with` passes the field's own type
    pub fn serialize<S: Serializer>(path: &PathBuf, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(path.as_os_str().as_bytes())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let bytes: Vec<u8> = serde_bytes_vec(deserializer)?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }

    fn serde_bytes_vec<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Vec<u8>, E> {
                Ok(value.to_vec())
            }
            fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(value)
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

/// The INDEX section's stored bytes: postcard, compressed like the plan.
pub fn encode_index(index: &Index) -> Result<Vec<u8>, String> {
    let bytes = postcard::to_stdvec(index).map_err(|error| error.to_string())?;
    Ok(crate::snapshot::format::assemble_compressed(vec![
        crate::snapshot::format::compress_part(bytes),
    ]))
}

/// [`encode_index`]'s inverse (the section's checksum already passed).
pub fn decode_index(stored: Vec<u8>) -> Result<Index, String> {
    let bytes = crate::snapshot::format::decompress_plan(stored)?;
    postcard::from_bytes(&bytes).map_err(|error| format!("malformed INDEX section: {error}"))
}

/// The index the last planned build recorded, for the save
/// (crate::snapshot::save takes it).
static PLANNED_INDEX: std::sync::Mutex<Option<Index>> = std::sync::Mutex::new(None);

/// Keep `index` for the save of this command.
pub fn set_planned_index(index: Option<Index>) {
    *PLANNED_INDEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = index;
}

/// The index the save writes (taken once).
pub fn take_planned_index() -> Option<Index> {
    PLANNED_INDEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

// ---- merging the saved INPUTS (crate::snapshot::save) ----------------------

/// The recording an incremental save writes: everything `new` (the partial
/// analysis, selected by the policy) observed, plus every observation,
/// environment digest and tree of `old` (the snapshot it started from,
/// validated by the lookup: what did not change still holds) that `new`
/// does not have. Sorted like a recorder's.
pub fn merge_recordings(
    new: crate::observed_fs::Recording,
    old: crate::observed_fs::Recording,
) -> crate::observed_fs::Recording {
    use std::collections::{BTreeMap, HashSet};
    use std::os::unix::ffi::OsStrExt;
    let mut recording = new;
    let seen: HashSet<(PathBuf, crate::observed_fs::Op)> = recording
        .observations
        .iter()
        .map(|(path, observation)| (path.clone(), observation.op.clone()))
        .collect();
    for (path, observation) in old.observations {
        if !seen.contains(&(path.clone(), observation.op.clone())) {
            recording.observations.push((path, observation));
        }
    }
    recording.observations.sort_by(|(a, x), (b, y)| {
        a.as_os_str()
            .as_bytes()
            .cmp(b.as_os_str().as_bytes())
            .then_with(|| x.op.cmp(&y.op))
    });
    let mut env: BTreeMap<std::ffi::OsString, crate::observed_fs::EnvObservation> =
        old.env.into_iter().collect();
    env.extend(recording.env);
    recording.env = env.into_iter().collect();
    let roots: HashSet<PathBuf> = recording
        .repositories
        .iter()
        .map(|repository| repository.root.clone())
        .collect();
    recording.repositories.extend(
        old.repositories
            .into_iter()
            .filter(|repository| !roots.contains(&repository.root)),
    );
    recording.repositories.sort_by(|a, b| a.root.cmp(&b.root));
    // Receipts travel with the same already-validated repository inputs; they
    // do not grant CARRIED authority or reclassify a pointer change as package-only.
    let mut selections: BTreeMap<_, _> = old
        .selections
        .into_iter()
        .map(|receipt| (receipt.canonical.clone(), receipt))
        .collect();
    selections.extend(
        recording
            .selections
            .into_iter()
            .map(|receipt| (receipt.canonical.clone(), receipt)),
    );
    recording.selections = selections.into_values().collect();
    let mut crossed: std::collections::BTreeSet<PathBuf> = old.crossed.into_iter().collect();
    crossed.extend(recording.crossed);
    // A crossing's record stands only while no owner claimed the tree.
    let owned: HashSet<&PathBuf> = recording
        .repositories
        .iter()
        .map(|repository| &repository.root)
        .collect();
    let _ = owned;
    recording.crossed = crossed.into_iter().collect();
    recording
}

/// The replay records of an incremental save: the partial analysis's, then
/// the previous snapshot's for the local repositories it did not execute
/// (their replay matched during the lookup), in the previous order.
pub fn merge_replay(
    new: Vec<crate::snapshot::replay::ReplayRecord>,
    old: Vec<crate::snapshot::replay::ReplayRecord>,
) -> Vec<crate::snapshot::replay::ReplayRecord> {
    let mut new_by_name: std::collections::BTreeMap<String, crate::snapshot::replay::ReplayRecord> =
        new.into_iter()
            .map(|record| (record.canonical.clone(), record))
            .collect();
    let mut merged = Vec::with_capacity(old.len() + new_by_name.len());
    for record in old {
        match new_by_name.remove(&record.canonical) {
            Some(newer) => merged.push(newer),
            None => merged.push(record),
        }
    }
    merged.extend(new_by_name.into_values());
    merged
}

/// The pinned tables of an incremental save: the previous snapshot's,
/// overridden by what the partial analysis pinned.
pub fn merge_tables(
    new: Option<crate::snapshot::replay::PinnedTables>,
    old: Option<crate::snapshot::replay::PinnedTables>,
) -> Option<crate::snapshot::replay::PinnedTables> {
    match (new, old) {
        (Some(mut new), Some(old)) => {
            for (name, root) in old.roots {
                new.roots.entry(name).or_insert(root);
            }
            for (name, mapping) in old.mappings {
                new.mappings.entry(name).or_insert(mapping);
            }
            Some(new)
        }
        (new, old) => new.or(old),
    }
}

// ---- the miss: what changed, and where ------------------------------------

/// What the lookup of a missed snapshot keeps for incremental analysis:
/// its decoded parts and the observations that changed (every other check
/// passed: environment, trees, local repositories' replays).
pub struct Base {
    pub inputs: crate::snapshot::format::Inputs,
    pub window: Vec<u8>,
    pub request: crate::request::PlannedRequest,
    /// What changed, and where ([`classify_attempt`]); the error says why
    /// incremental analysis cannot handle this miss (the base's other
    /// parts are then empty).
    pub classified: Result<Classified, String>,
}

impl Base {
    /// A miss incremental analysis cannot handle, for `reason`.
    pub fn refused(reason: String) -> Self {
        Self {
            inputs: crate::snapshot::format::Inputs::default(),
            window: Vec::new(),
            request: crate::request::PlannedRequest::empty(),
            classified: Err(reason),
        }
    }
}

/// The changed observations of a miss attributed to packages.
pub struct Classified {
    pub index: Index,
    /// Indices into the INPUTS' observations.
    pub changed: Vec<usize>,
    pub attribution: Attribution,
}

/// Where the changed observations of a miss lie ([`classify`]).
#[derive(Debug, Default)]
pub struct Attribution {
    /// Packages whose BUILD file or glob listings changed.
    pub loaded: std::collections::BTreeSet<u32>,
    /// Packages whose other observations changed.
    pub content: std::collections::BTreeSet<u32>,
    /// The edited `.bzl` modules and every module loading one
    /// (transitively), index module ids.
    pub modules: std::collections::BTreeSet<u32>,
    /// Packages loading one of `modules`: loaded again, their fingerprint
    /// decides.
    pub bzl: std::collections::BTreeSet<u32>,
    /// The edited modules themselves.
    pub edited: std::collections::BTreeSet<u32>,
    /// The repositories one of whose recorded inputs changed (canonical
    /// names): they run again, their trees must keep their contents.
    pub repositories: std::collections::BTreeSet<String>,
}

/// While a lookup misses (crate::snapshot::hit): every changed observation
/// of `inputs`, the snapshot's INDEX, and their attribution to packages
/// ([`classify`]); the error names why incremental analysis cannot handle
/// the miss.
pub fn classify_attempt(
    inputs: &crate::snapshot::format::Inputs,
    reader: &crate::snapshot::store::Reader,
    cache: &std::path::Path,
    environment: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<Classified, String> {
    let started = std::time::Instant::now();
    let changed = crate::snapshot::validate::changed_observations(
        &inputs.recording,
        &crate::snapshot::validate::Environment::new(environment),
    )?;
    let validated = started.elapsed();
    let index = reader
        .section(crate::snapshot::format::Section::Index)
        .and_then(decode_index)
        .map_err(|error| format!("no dependency index: {error}"))?;
    let decoded = started.elapsed();
    let attribution = classify(&index, &inputs.recording, &changed, cache)?;
    if crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-SNAPSHOT incremental classify validate_ms={:.3} index_ms={:.3} classify_ms={:.3} changed={}",
            validated.as_secs_f64() * 1000.0,
            (decoded - validated).as_secs_f64() * 1000.0,
            (started.elapsed() - decoded).as_secs_f64() * 1000.0,
            changed.len()
        ));
    }
    Ok(Classified {
        index,
        changed,
        attribution,
    })
}

/// A miss incremental analysis may handle: the packages whose recorded
/// inputs changed.
pub struct Candidate {
    pub inputs: crate::snapshot::format::Inputs,
    pub window: Vec<u8>,
    pub request: crate::request::PlannedRequest,
    pub index: Index,
    /// Indices into `inputs.recording.observations`.
    pub changed: Vec<usize>,
    /// Packages (index ids) whose BUILD file or glob listings changed:
    /// their fingerprint decides whether their targets changed.
    pub loaded: std::collections::BTreeSet<u32>,
    /// Packages whose other observations changed (a source file's
    /// existence, type or link): their targets are re-analyzed.
    pub content: std::collections::BTreeSet<u32>,
    /// The edited `.bzl` modules and those loading them ([`Attribution`]).
    pub modules: std::collections::BTreeSet<u32>,
    /// Packages loading one of `modules`: loaded again.
    pub bzl: std::collections::BTreeSet<u32>,
    /// The edited modules themselves.
    pub edited: std::collections::BTreeSet<u32>,
    /// The repositories whose recorded inputs changed ([`Attribution`]).
    pub repositories: std::collections::BTreeSet<String>,
    /// The snapshot's short key (diagnostics).
    pub key: String,
    pub started: std::time::Instant,
}

impl Candidate {
    /// The candidate of a classified miss (`Err`: why there is none).
    pub fn of(base: Base, key: String) -> Result<Self, String> {
        let Base {
            inputs,
            window,
            request,
            classified,
        } = base;
        let Classified {
            index,
            changed,
            attribution:
                Attribution {
                    loaded,
                    content,
                    modules,
                    bzl,
                    edited,
                    repositories,
                },
        } = classified?;
        Ok(Self {
            inputs,
            window,
            request,
            index,
            changed,
            loaded,
            content,
            modules,
            bzl,
            edited,
            repositories,
            key,
            started: std::time::Instant::now(),
        })
    }
}

fn escaped(path: &std::path::Path) -> String {
    crate::observed_fs::escape(path.as_os_str())
}

/// Attribute every changed observation of `base` to a main-repository
/// package the analysis loaded, or name the first change that cannot be
/// (in the cache, a repository's or extension's recorded input, a `.bzl`
/// file, a listing of the target patterns, a package boundary, a package
/// shared computations read or whose BUILD file printed).
pub fn classify(
    index: &Index,
    recording: &crate::observed_fs::Recording,
    changed: &[usize],
    cache: &std::path::Path,
) -> Result<Attribution, String> {
    use crate::observed_fs::{Op, Projection};
    use std::collections::{BTreeSet, HashMap};
    if changed.is_empty() {
        return Err("no observation changed".to_owned());
    }
    let module_paths: HashMap<PathBuf, u32> = index
        .modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.label.repository.is_empty())
        .map(|(id, module)| {
            (
                index
                    .main_root
                    .join(&module.label.package)
                    .join(&module.label.name),
                id as u32,
            )
        })
        .collect();
    let mut edited: BTreeSet<u32> = BTreeSet::new();
    let mut packages: HashMap<&str, u32> = HashMap::new();
    for (id, package) in index.packages.iter().enumerate() {
        if package.repository.is_empty() && package.loaded && package.fingerprint.is_some() {
            packages.insert(package.package.as_str(), id as u32);
        }
    }
    let mut loaded = BTreeSet::new();
    let mut content = BTreeSet::new();
    let mut repositories: BTreeSet<String> = BTreeSet::new();
    for &at in changed {
        let (path, observation) = recording
            .observations
            .get(at)
            .ok_or("internal error: a changed observation out of range")?;
        let what = format!("{} {}", observation.op.name(), escaped(path));
        if matches!(
            observation.op,
            Op::SelectionRead(_) | Op::SelectionAbsent(_)
        ) {
            return Err(format!("{what}: repository publication selection"));
        }
        if path.starts_with(cache) {
            return Err(format!("{what}: inside the cache"));
        }
        // A recorded input of repositories (not of a module extension):
        // they run again, and must publish the same tree (below, and
        // crate::main's incremental analysis); the observation is
        // attributed like any other.
        for (input, tree, owner) in &index.repository_inputs {
            if *path == input.0 || (*tree && path.starts_with(&input.0)) {
                match owner {
                    Some(owner) => {
                        repositories.insert(owner.clone());
                    }
                    None => {
                        return Err(format!("{what}: a recorded input of a module extension"));
                    }
                }
            }
        }
        let relative = path
            .strip_prefix(&index.main_root)
            .map_err(|_| format!("{what}: outside the main repository"))?;
        // A main-repository `.bzl` module of the index: the packages
        // loading it and the targets whose rules it defines (below).
        if let Some(module) = module_paths.get(path) {
            edited.insert(*module);
            if observation.op == Op::Read {
                continue;
            }
        }
        let name = relative.file_name().and_then(|name| name.to_str());
        let build_file = matches!(name, Some("BUILD" | "BUILD.bazel"));
        let directory = match observation.op {
            Op::ReadDir(_) => Some(relative),
            _ => relative.parent(),
        };
        let owner = directory.and_then(|directory| {
            directory.ancestors().find_map(|ancestor| {
                ancestor
                    .to_str()
                    .and_then(|text| packages.get(text).copied())
                    .map(|id| (id, ancestor))
            })
        });
        let Some((owner, owner_directory)) = owner else {
            return Err(format!("{what}: not inside a package the analysis loaded"));
        };
        match &observation.op {
            Op::Read if build_file && relative.parent() == Some(owner_directory) => {
                loaded.insert(owner);
            }
            Op::Read if name.is_some_and(|name| name.ends_with(".bzl")) => {
                return Err(format!("{what}: a .bzl file (its edits fall back)"));
            }
            Op::Read => return Err(format!("{what}: a file planning read")),
            Op::ReadDir(Projection::Glob { .. }) => {
                loaded.insert(owner);
            }
            Op::ReadDir(_) => {
                return Err(format!(
                    "{what}: a directory listing of the target patterns or a repository"
                ));
            }
            _ if build_file => return Err(format!("{what}: a package boundary")),
            _ => {
                content.insert(owner);
            }
        }
    }
    let (modules, bzl) = bzl_attribution(index, &edited)?;
    // A repository that runs again publishes a new tree (its identity
    // names its recorded inputs): nothing the analysis analyzed or loaded
    // may lie in it, and its old root must be known.
    for repository in &repositories {
        if !index
            .repository_roots
            .iter()
            .any(|(name, _)| name == repository)
        {
            return Err(format!(
                "repository @@{repository}, whose recorded input changed, has no recorded root"
            ));
        }
        if let Some(package) = index
            .packages
            .iter()
            .find(|package| package.repository == *repository)
        {
            return Err(format!(
                "repository @@{repository}, whose recorded input changed, holds package //{} of the analysis",
                package.package
            ));
        }
        if let Some(key) = index
            .keys
            .iter()
            .find(|key| key.label.repository == *repository)
        {
            return Err(format!(
                "repository @@{repository}, whose recorded input changed, holds {} of the analysis",
                key.label
            ));
        }
    }
    for &id in loaded.iter().chain(content.iter()) {
        let package = &index.packages[id as usize];
        let shown = format!("package //{}", package.package);
        if package.global {
            return Err(format!(
                "{shown} is read by a shared computation (toolchains, platforms, transitions, visibility or the command line): {}",
                package.why.as_deref().unwrap_or("-")
            ));
        }
        if package.printed {
            return Err(format!("{shown} printed while it loaded"));
        }
        if package
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.shared_kinds)
        {
            return Err(format!(
                "{shown} defines toolchains, platforms, constraints, flags or package groups"
            ));
        }
    }
    Ok(Attribution {
        loaded,
        content,
        modules,
        bzl,
        edited,
        repositories,
    })
}

/// The modules an edit of `edited` affects (they and every module loading
/// one) and the packages loading them; the error names why the edit is not
/// attributable (a definition load entered an affected module, a package
/// of another repository or one not loaded loads one, a rule's defining
/// modules are unknown).
#[allow(clippy::type_complexity)]
pub fn bzl_attribution(
    index: &Index,
    edited: &std::collections::BTreeSet<u32>,
) -> Result<
    (
        std::collections::BTreeSet<u32>,
        std::collections::BTreeSet<u32>,
    ),
    String,
> {
    use std::collections::{BTreeSet, HashMap};
    if edited.is_empty() {
        return Ok(Default::default());
    }
    let unattributed = unattributed_module();
    if let Some((key, _)) = index
        .rule_modules
        .iter()
        .find(|(_, module)| index.modules[*module as usize].label == unattributed)
    {
        return Err(format!(
            "the rule of {} has no known defining module",
            index.keys[*key as usize].label
        ));
    }
    let mut parents: HashMap<u32, Vec<u32>> = HashMap::new();
    for (id, module) in index.modules.iter().enumerate() {
        for load in &module.loads {
            parents.entry(*load).or_default().push(id as u32);
        }
    }
    let mut affected: BTreeSet<u32> = edited.clone();
    let mut todo: Vec<u32> = edited.iter().copied().collect();
    while let Some(module) = todo.pop() {
        for parent in parents.get(&module).into_iter().flatten() {
            if affected.insert(*parent) {
                todo.push(*parent);
            }
        }
    }
    for id in &affected {
        let module = &index.modules[*id as usize];
        if module.definition {
            return Err(format!(
                "{} is loaded by a repository rule, a module extension or an aspect's definition",
                module.label
            ));
        }
    }
    let mut packages = BTreeSet::new();
    for (id, package) in index.packages.iter().enumerate() {
        if !package.loads.iter().any(|load| affected.contains(load)) {
            continue;
        }
        if !package.repository.is_empty() || !package.loaded || package.fingerprint.is_none() {
            return Err(format!(
                "package @@{}//{} loads an edited .bzl file",
                package.repository, package.package
            ));
        }
        if package.printed {
            return Err(format!(
                "package //{} loads an edited .bzl file and printed while it loaded",
                package.package
            ));
        }
        packages.insert(id as u32);
    }
    Ok((affected, packages))
}

/// The packages whose targets an incremental analysis re-analyzes, after
/// their new fingerprints were taken.
pub struct Changed {
    pub packages: std::collections::BTreeSet<u32>,
    /// The new fingerprints (and printed flags) of every package loaded
    /// again.
    pub fingerprints: std::collections::BTreeMap<u32, Fingerprint>,
    /// Packages loaded again whose fingerprint did not change.
    pub pruned: usize,
    /// Changed packages whose pattern summary changed too (targets added,
    /// removed, renamed or retagged): the requested labels they contribute
    /// are computed again ([`relabel`]).
    pub relabeled: std::collections::BTreeSet<u32>,
    /// Of each changed package, the target names whose rules changed, were
    /// added or removed (`None`: the package itself changed — its
    /// arguments, defaults, loads, or a file inside it — and every target
    /// of it is dirty).
    pub names: std::collections::BTreeMap<u32, Option<std::collections::BTreeSet<String>>>,
}

/// The target names whose digests differ between two fingerprints of a
/// package (added and removed ones included).
pub fn changed_targets(
    before: &Fingerprint,
    now: &Fingerprint,
) -> std::collections::BTreeSet<String> {
    let before: std::collections::BTreeMap<&String, &[u8; 16]> = before
        .targets
        .iter()
        .map(|(name, digest)| (name, digest))
        .collect();
    let now_map: std::collections::BTreeMap<&String, &[u8; 16]> = now
        .targets
        .iter()
        .map(|(name, digest)| (name, digest))
        .collect();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (name, digest) in &before {
        if now_map.get(name) != Some(digest) {
            names.insert((*name).clone());
        }
    }
    for name in now_map.keys() {
        if !before.contains_key(name) {
            names.insert((*name).clone());
        }
    }
    names
}

/// Compare a reloaded package with what the index recorded of it.
/// `strict_patterns`: a target pattern names file targets (`:*`,
/// `:all-targets`), whose expansion any fingerprint change may move.
/// `relabel`: the requested labels may be computed again (a plain `build`
/// pattern phase, [`Index::patterns`]): a changed pattern summary is one
/// of [`Changed::relabeled`].
pub fn compare_package(
    candidate: &Candidate,
    id: u32,
    now: Fingerprint,
    printed: bool,
    strict_patterns: bool,
    relabel: &Result<(), String>,
    changed: &mut Changed,
) -> Result<(), String> {
    let package = &candidate.index.packages[id as usize];
    let shown = format!("package //{}", package.package);
    let before = package
        .fingerprint
        .as_ref()
        .ok_or_else(|| format!("{shown} has no recorded fingerprint"))?;
    if printed {
        return Err(format!("{shown} prints while it loads"));
    }
    let unchanged = before.digest == now.digest && !candidate.content.contains(&id);
    // A package reloaded for a `.bzl` edit may be read by shared
    // computations or define shared kinds while it does not change.
    if now.shared_kinds && !unchanged {
        return Err(format!(
            "{shown} defines toolchains, platforms, constraints, flags or package groups"
        ));
    }
    if package.global && !unchanged {
        return Err(format!(
            "{shown} is read by a shared computation (toolchains, platforms, transitions, visibility or the command line): {}",
            package.why.as_deref().unwrap_or("-")
        ));
    }
    // Whether the target pattern phase expanded the package is not
    // tracked precisely: any change of what an expansion reads falls back.
    if strict_patterns && before.digest != now.digest {
        return Err(format!(
            "{shown}: the target patterns' expansion may change (targets added, removed or retagged)"
        ));
    }
    if before.summary != now.summary {
        if let Err(why) = relabel {
            return Err(format!(
                "{shown}: the target patterns' expansion may change (targets added, removed or retagged), and {why}"
            ));
        }
        changed.relabeled.insert(id);
    }
    if unchanged {
        changed.pruned += 1;
    } else {
        changed.packages.insert(id);
        // The rules alone changed: what looked them up is dirty.
        let names = (before.package == now.package
            && !candidate.content.contains(&id)
            && before.package != [0; 32])
            .then(|| changed_targets(before, &now));
        changed.names.insert(id, names);
    }
    changed.fingerprints.insert(id, now);
    Ok(())
}

/// The requested labels after a change of the packages `relabeled`
/// ([`Changed::relabeled`]) and what changed of them.
#[derive(Debug, Default)]
pub struct Relabel {
    /// The requested labels now, sorted like the target pattern phase's.
    pub labels: Vec<Label>,
    /// The positions (in [`Index::labels`]) of the labels no longer
    /// requested.
    pub removed: std::collections::BTreeSet<u32>,
    /// The labels requested now and not before.
    pub added: std::collections::BTreeSet<Label>,
}

impl Relabel {
    /// Whether the requested labels changed.
    pub fn changes(&self) -> bool {
        !self.removed.is_empty() || !self.added.is_empty()
    }
}

/// The requested labels of `index` with each package of `selections`
/// (index package ids) contributing what the pattern phase selects of it
/// now (crate::engine::Engine::package_selection).
pub fn relabel(
    index: &Index,
    selections: &std::collections::BTreeMap<u32, std::collections::BTreeSet<Label>>,
) -> Result<Relabel, String> {
    use std::collections::BTreeSet;
    if !index.labels.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err("the requested labels are not the sorted pattern selection".to_owned());
    }
    let packages: BTreeSet<(&str, &str)> = selections
        .keys()
        .map(|id| {
            let package = &index.packages[*id as usize];
            (package.repository.as_str(), package.package.as_str())
        })
        .collect();
    let in_changed =
        |label: &Label| packages.contains(&(label.repository.as_str(), label.package.as_str()));
    let old: BTreeSet<&Label> = index.labels.iter().collect();
    let mut labels: BTreeSet<Label> = index
        .labels
        .iter()
        .filter(|label| !in_changed(label))
        .cloned()
        .collect();
    for selection in selections.values() {
        labels.extend(selection.iter().cloned());
    }
    let removed = index
        .labels
        .iter()
        .enumerate()
        .filter(|(_, label)| !labels.contains(*label))
        .map(|(position, _)| position as u32)
        .collect();
    let added = labels
        .iter()
        .filter(|label| !old.contains(label))
        .cloned()
        .collect();
    Ok(Relabel {
        labels: labels.into_iter().collect(),
        removed,
        added,
    })
}

/// What a change dirties ([`dirty_labels`]).
#[derive(Debug, Default)]
pub struct Dirty {
    /// The requested labels the partial analysis analyzes, in request
    /// order: those whose root is dirty and the added ones.
    pub labels: Vec<Label>,
    /// The saved analysis's configured targets (and source files) that
    /// are re-analyzed.
    pub dirty: std::collections::BTreeSet<u32>,
    /// The keys only removed labels' analyses planned (or reached): they
    /// and their actions are gone.
    pub removed: std::collections::BTreeSet<u32>,
    /// The share of the saved analysis's configured targets the partial
    /// analysis re-analyzes (the dirty roots' forward closure), percent.
    pub cost: usize,
}

/// The configured targets the changed `packages` dirty — those in them,
/// those whose analysis looked them up, and everything reaching either —
/// the keys of the labels `relabel` removes, and the requested labels to
/// analyze again (with the added ones), in request order.
pub fn dirty_labels(
    index: &Index,
    packages: &std::collections::BTreeSet<u32>,
    names: &std::collections::BTreeMap<u32, Option<std::collections::BTreeSet<String>>>,
    modules: &std::collections::BTreeSet<u32>,
    relabel: Option<&Relabel>,
) -> Result<Dirty, String> {
    use std::collections::{BTreeSet, HashMap};
    let mut by_package: HashMap<(&str, &str), u32> = HashMap::new();
    for (id, package) in index.packages.iter().enumerate() {
        by_package.insert((&package.repository, &package.package), id as u32);
    }
    // A package whose rules alone changed dirties the targets named so
    // and what looked them up; any other one all of its targets and what
    // looked it up.
    let whole = |package: &u32| {
        packages.contains(package) && names.get(package).is_none_or(Option::is_none)
    };
    let named = |package: &u32, name: &str| {
        names
            .get(package)
            .and_then(Option::as_ref)
            .is_some_and(|names| names.contains(name))
    };
    let mut dirty: BTreeSet<u32> = BTreeSet::new();
    for (id, key) in index.keys.iter().enumerate() {
        if let Some(package) =
            by_package.get(&(key.label.repository.as_str(), key.label.package.as_str()))
            && (whole(package) || named(package, &key.label.name))
        {
            dirty.insert(id as u32);
        }
    }
    for (key, package) in &index.accesses {
        if whole(package) {
            dirty.insert(*key);
        }
    }
    for (key, package, name) in &index.label_accesses {
        if named(package, name) {
            dirty.insert(*key);
        }
    }
    // The targets whose rules an edited module defines.
    for (key, module) in &index.rule_modules {
        if modules.contains(module) {
            dirty.insert(*key);
        }
    }
    let mut parents: HashMap<u32, Vec<u32>> = HashMap::new();
    for (parent, child) in &index.edges {
        parents.entry(*child).or_default().push(*parent);
    }
    let mut todo: Vec<u32> = dirty.iter().copied().collect();
    while let Some(key) = todo.pop() {
        for parent in parents.get(&key).into_iter().flatten() {
            if dirty.insert(*parent) {
                todo.push(*parent);
            }
        }
    }
    // The removed labels' keys: what their analyses planned (the plan
    // between their roots' entries), and what only those reach.
    let empty = Relabel::default();
    let relabel = relabel.unwrap_or(&empty);
    let mut removed: BTreeSet<u32> = BTreeSet::new();
    if !relabel.removed.is_empty() {
        if index.root_entries.len() != index.labels.len() + 1 {
            return Err("the saved index has no root entries".to_owned());
        }
        let positions: HashMap<u32, u32> = index
            .planned
            .iter()
            .map(|(key, _, position)| (*position, *key))
            .collect();
        for &at in &relabel.removed {
            let (from, to) = (
                index.root_entries[at as usize],
                index.root_entries[at as usize + 1],
            );
            for position in from..to {
                let key = positions.get(&position).ok_or_else(|| {
                    format!("the saved plan has no target at position {position}")
                })?;
                removed.insert(*key);
            }
        }
        let planned: std::collections::HashSet<u32> =
            index.planned.iter().map(|(key, _, _)| *key).collect();
        // Keys planned nowhere (source files, aliases) reached only from
        // removed ones go with them.
        loop {
            let mut grew = false;
            for (parent, child) in &index.edges {
                if removed.contains(parent)
                    && !removed.contains(child)
                    && !planned.contains(child)
                    && parents
                        .get(child)
                        .is_some_and(|all| all.iter().all(|parent| removed.contains(parent)))
                {
                    removed.insert(*child);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        for (parent, child) in &index.edges {
            if removed.contains(child) && !removed.contains(parent) {
                return Err(format!(
                    "{}, planned by a removed target's analysis, is reached by {} too",
                    index.keys[*child as usize].label, index.keys[*parent as usize].label
                ));
            }
        }
        for (position, key) in &index.roots {
            if !relabel.removed.contains(position) && removed.contains(key) {
                return Err(format!(
                    "the requested {} was planned by a removed target's analysis",
                    index.keys[*key as usize].label
                ));
            }
        }
    }
    if let Some(printed) = index
        .printed
        .iter()
        .find(|key| dirty.contains(key) || removed.contains(key))
    {
        return Err(format!(
            "{} printed while it was analyzed and would be re-analyzed or removed",
            index.keys[*printed as usize].label
        ));
    }
    if let Some(shared) = index
        .shared
        .iter()
        .find(|key| dirty.contains(key) || removed.contains(key))
    {
        return Err(format!(
            "{} feeds a shared computation (toolchains, platforms, transitions) and would be re-analyzed or removed",
            index.keys[*shared as usize].label
        ));
    }
    dirty.retain(|key| !removed.contains(key));
    let mut positions: BTreeSet<u32> = BTreeSet::new();
    for (position, key) in &index.roots {
        if dirty.contains(key) && !relabel.removed.contains(position) {
            positions.insert(*position);
        }
    }
    let mut labels: BTreeSet<Label> = positions
        .iter()
        .filter_map(|position| index.labels.get(*position as usize).cloned())
        .collect();
    labels.extend(relabel.added.iter().cloned());
    let labels: Vec<Label> = labels.into_iter().collect();
    if labels.is_empty() && !dirty.is_empty() {
        return Err(format!(
            "{} is dirty but reaches no requested target",
            index.keys[*dirty.iter().next().expect("not empty") as usize].label
        ));
    }
    // The cost: the partial analysis re-analyzes the forward closure of
    // the dirty roots; near the whole analysis it is no faster (and a
    // fallback after it pays both).
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (parent, child) in &index.edges {
        children.entry(*parent).or_default().push(*child);
    }
    let mut closure: BTreeSet<u32> = BTreeSet::new();
    let mut todo: Vec<u32> = index
        .roots
        .iter()
        .filter(|(position, _)| positions.contains(position))
        .map(|(_, key)| *key)
        .collect();
    while let Some(key) = todo.pop() {
        if closure.insert(key) {
            todo.extend(children.get(&key).into_iter().flatten().copied());
        }
    }
    let configured = index.keys.iter().filter(|key| key.config.is_some()).count();
    let reanalyzed = closure
        .iter()
        .filter(|key| index.keys[**key as usize].config.is_some())
        .count();
    let cost = (reanalyzed * 100).checked_div(configured).unwrap_or(0);
    let (limit, minimum) = max_cost_percent();
    if configured >= minimum && reanalyzed * 100 > configured * limit {
        return Err(format!(
            "the re-analysis would analyze {reanalyzed} of {configured} configured targets (more than {limit}%)"
        ));
    }
    Ok(Dirty {
        labels,
        dirty,
        removed,
        cost,
    })
}

/// `BAZEL_RS_INCREMENTAL_MAX_COST_PERCENT`: when the partial analysis
/// would re-analyze more than this share of the saved analysis's
/// configured targets (the dirty roots' forward closure), the full
/// analysis runs at once (default 60, for analyses of 500 configured
/// targets or more; set, for any).
pub const MAX_COST_VAR: &str = "BAZEL_RS_INCREMENTAL_MAX_COST_PERCENT";

/// The cost limit (percent) and the analysis size it applies from.
fn max_cost_percent() -> (usize, usize) {
    match std::env::var(MAX_COST_VAR)
        .ok()
        .and_then(|value| value.parse().ok())
    {
        Some(limit) => (limit, 0),
        None => (60, 500),
    }
}

/// The saved window `window` with the notes `old` (the saved analysis's
/// `INFO: Analyzed …` and `INFO: Found …` lines, crate::progress::
/// analysis_lines) replaced by `new`; the window must end with `old`'s
/// notes, in order, where the analysis printed them (else the counts are
/// not the saved analysis's).
pub fn replace_window_lines(
    window: &[u8],
    old: &[String],
    new: &[String],
) -> Result<Vec<u8>, String> {
    let old_bytes: Vec<u8> = old
        .iter()
        .flat_map(|line| crate::ui::marked(line))
        .collect();
    let new_bytes: Vec<u8> = new
        .iter()
        .flat_map(|line| crate::ui::marked(line))
        .collect();
    let at = window
        .windows(old_bytes.len().max(1))
        .rposition(|part| part == old_bytes.as_slice())
        .filter(|_| !old_bytes.is_empty())
        .ok_or_else(|| {
            format!(
                "the saved window does not print what the saved counts print ({})",
                one_line(&old.join(" "))
            )
        })?;
    let mut replaced = window[..at].to_vec();
    replaced.extend_from_slice(&new_bytes);
    replaced.extend_from_slice(&window[at + old_bytes.len()..]);
    Ok(replaced)
}

/// Every line the partial analysis printed — but its own `INFO: Analyzed`
/// and `INFO: Found` lines — is one the full analysis printed too (the
/// window printed in its place is the full analysis's).
pub fn window_covers(full: &[u8], partial: &[u8]) -> Result<(), String> {
    let (full, partial) = (crate::ui::unmarked(full), crate::ui::unmarked(partial));
    let lines: std::collections::HashSet<&[u8]> = full.split(|byte| *byte == b'\n').collect();
    for line in partial.split(|byte| *byte == b'\n') {
        if line.is_empty()
            || line.starts_with(b"INFO: Analyzed ")
            || line.starts_with(b"INFO: Found ")
        {
            continue;
        }
        if !lines.contains(line) {
            return Err(format!(
                "the partial analysis printed a line the full analysis did not: {}",
                String::from_utf8_lossy(line)
            ));
        }
    }
    Ok(())
}

// ---- the splice ------------------------------------------------------------

/// The action owner of an index key (crate::config_registry::
/// ConfiguredTargetKey::action_owner).
fn owner_of_key(key: &IndexKey, top: &[u8; 32]) -> Option<String> {
    let config = key.config?;
    Some(if config == *top {
        key.label.to_string()
    } else {
        let hex: String = config[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}[{hex}]", key.label)
    })
}

/// The owner of action `id` among `owners`: the id itself (a native
/// rule's action, `//pkg:name`), else its longest owner prefix followed by
/// `#` or `%` (`//pkg:name#0`, `//pkg:name[cfg]#1`, an aspect's
/// `//pkg:name%//:a.bzl%asp#0`).
fn owner_of_action<'a>(
    id: &str,
    owners: &'a std::collections::HashMap<String, bool>,
) -> Option<(&'a str, bool)> {
    if let Some((owner, dirty)) = owners.get_key_value(id) {
        return Some((owner.as_str(), *dirty));
    }
    let mut found = None;
    for (at, byte) in id.bytes().enumerate() {
        if (byte == b'#' || byte == b'%')
            && let Some((owner, dirty)) = owners.get_key_value(&id[..at])
        {
            found = Some((owner.as_str(), *dirty));
        }
    }
    found
}

fn node_bytes(node: &crate::scheduler::PlannedAction) -> Result<Vec<u8>, String> {
    postcard::to_stdvec(node).map_err(|error| format!("action '{}': {error}", node.action.id))
}

/// The new targets an added requested label's analysis planned, inserted
/// into the saved plan before position `boundary` ([`check_graph`]).
#[derive(Clone, Debug, Default)]
pub struct Insert {
    /// The saved plan position (the plan length when the next saved
    /// requested label's analysis began; the end of phase 1).
    pub boundary: u32,
    /// The new keys (merged index ids) in plan order, each with the number
    /// of this group's keys planned when its analysis began.
    pub keys: Vec<(u32, u32)>,
}

/// What [`check_graph`] found of the partial analysis's graph.
#[derive(Debug, Default)]
pub struct Graph {
    /// The partial analysis's key ids as merged index ids.
    pub mapped: Vec<u32>,
    /// The configured targets the saved analysis did not have (added
    /// labels' new targets), appended to the index's keys.
    pub new_keys: std::collections::BTreeSet<u32>,
    /// Per added label (in request order), its new planned targets.
    pub inserts: Vec<Insert>,
    /// The distinct aspect applications before and after.
    pub aspects: (usize, usize),
}

/// Map the keys of `sub` (a partial analysis's index) to `index`'s (the
/// saved one, which becomes the merged index), checking that the
/// configured graph did not change but where the requested labels did:
/// every configured target the partial analysis reached is one the saved
/// analysis had — but the new ones only added labels' analyses reach —
/// each dirty one with the same configured dependencies, and it loaded no
/// package the saved analysis did not (so the plan order and the counts
/// the window reports follow). An added label (`added`: its position among
/// the partial analysis's labels, and the saved plan position its new
/// targets go before) reaches only targets the saved plan had before that
/// position. Source files may come and go (a `srcs` edit): new ones join
/// the index, and the dirty targets' source edges become the partial
/// analysis's. `removed` (the keys of removed labels) are not looked at.
pub fn check_graph(
    index: &mut Index,
    dirty: &std::collections::BTreeSet<u32>,
    removed: &std::collections::BTreeSet<u32>,
    sub: &Index,
    added: &[(u32, u32)],
) -> Result<Graph, String> {
    use std::collections::{BTreeSet, HashMap};
    if sub.top != index.top {
        return Err("the top-level configuration changed".to_owned());
    }
    let mut ids: HashMap<IndexKey, u32> = index
        .keys
        .iter()
        .enumerate()
        .map(|(id, key)| (key.clone(), id as u32))
        .collect();
    let mut mapped = Vec::with_capacity(sub.keys.len());
    let mut new_keys: BTreeSet<u32> = BTreeSet::new();
    for key in &sub.keys {
        match ids.get(key) {
            Some(id) if removed.contains(id) => {
                return Err(format!(
                    "the re-analysis reached {}, a removed target's",
                    key.label
                ));
            }
            Some(id) => mapped.push(*id),
            None => {
                if key.config.is_some() && added.is_empty() {
                    return Err(format!(
                        "the re-analysis reached {}, which the saved analysis did not (the dependency graph changed)",
                        key.label
                    ));
                }
                let id = index.keys.len() as u32;
                index.keys.push(key.clone());
                ids.insert(key.clone(), id);
                if key.config.is_some() {
                    new_keys.insert(id);
                }
                mapped.push(id);
            }
        }
    }
    let configured = |index: &Index, key: u32| index.keys[key as usize].config.is_some();
    let analyzed = |key: &u32| dirty.contains(key) || new_keys.contains(key);
    // The configured dependencies of each dirty target, in the order its
    // analysis reached them (phase 1's depth-first order).
    let mut base_children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (parent, child) in &index.edges {
        if dirty.contains(parent) && configured(index, *child) {
            base_children.entry(*parent).or_default().push(*child);
        }
    }
    let mut sub_children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut all_children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut sub_edges: Vec<(u32, u32)> = Vec::new();
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    for (parent, child) in &sub.edges {
        let (parent, child) = (mapped[*parent as usize], mapped[*child as usize]);
        reached.insert(parent);
        reached.insert(child);
        all_children.entry(parent).or_default().push(child);
        if new_keys.contains(&child) && !new_keys.contains(&parent) {
            return Err(format!(
                "{} depends on {}, which the saved analysis did not have",
                index.keys[parent as usize].label, index.keys[child as usize].label
            ));
        }
        if analyzed(&parent) {
            sub_edges.push((parent, child));
            if configured(index, child) {
                let children = sub_children.entry(parent).or_default();
                if !children.contains(&child) {
                    children.push(child);
                }
            }
        }
    }
    // Phase 1's plan: when each target's analysis began and its position.
    let planned: HashMap<u32, (u32, u32)> = index
        .planned
        .iter()
        .map(|(key, entry, position)| (*key, (*entry, *position)))
        .collect();
    for (_, root) in &sub.roots {
        reached.insert(mapped[*root as usize]);
    }
    for key in dirty {
        if !configured(index, *key) {
            continue;
        }
        if !reached.contains(key) {
            return Err(format!(
                "{} was not re-analyzed",
                index.keys[*key as usize].label
            ));
        }
        let empty = Vec::new();
        let (before, after) = (
            base_children.get(key).unwrap_or(&empty),
            sub_children.get(key).unwrap_or(&empty),
        );
        if before != after {
            // Dependencies planned before this target's analysis began are
            // memo hits wherever they are reached: adding or removing one
            // changes neither the plan's order nor its targets; any other
            // dependency must be reached in the same order.
            let changed = || {
                format!(
                    "the dependencies of {} changed",
                    index.keys[*key as usize].label
                )
            };
            let (entry, _) = planned.get(key).ok_or_else(changed)?;
            let late = |child: &&u32| {
                planned
                    .get(*child)
                    .is_none_or(|(_, position)| position >= entry)
            };
            if !before.iter().filter(late).eq(after.iter().filter(late)) {
                return Err(changed());
            }
        }
    }
    // Added labels: what their analyses reach of the saved analysis was
    // planned before their place in the plan; their new targets go there.
    let mut inserts: Vec<Insert> = Vec::new();
    if !added.is_empty() {
        if sub.root_entries.len() != sub.labels.len() + 1 {
            return Err("the partial analysis recorded no root entries".to_owned());
        }
        let sub_planned: HashMap<u32, (u32, u32)> = sub
            .planned
            .iter()
            .map(|(key, entry, position)| (mapped[*key as usize], (*entry, *position)))
            .collect();
        let mut owner_of_new: HashMap<u32, usize> = HashMap::new();
        for (group, (at, boundary)) in added.iter().enumerate() {
            let root = sub
                .roots
                .iter()
                .find(|(position, _)| position == at)
                .map(|(_, key)| mapped[*key as usize])
                .ok_or("an added label has no root in the partial analysis")?;
            let mut seen: BTreeSet<u32> = BTreeSet::new();
            let mut todo = vec![root];
            while let Some(key) = todo.pop() {
                if !seen.insert(key) {
                    continue;
                }
                if !new_keys.contains(&key)
                    && configured(index, key)
                    && let Some((_, position)) = planned.get(&key)
                    && position >= boundary
                {
                    return Err(format!(
                        "the added {} reaches {}, which the saved analysis planned after it",
                        sub.labels[*at as usize], index.keys[key as usize].label
                    ));
                }
                todo.extend(all_children.get(&key).into_iter().flatten().copied());
            }
            let (from, to) = (
                sub.root_entries[*at as usize],
                sub.root_entries[*at as usize + 1],
            );
            let mut keys: Vec<(u32, u32, u32)> = new_keys
                .iter()
                .filter_map(|key| {
                    let (entry, position) = sub_planned.get(key)?;
                    (*position >= from && *position < to).then_some((*position, *entry, *key))
                })
                .collect();
            keys.sort_unstable();
            for (_, _, key) in &keys {
                owner_of_new.insert(*key, group);
            }
            let positions: Vec<u32> = keys.iter().map(|(position, _, _)| *position).collect();
            inserts.push(Insert {
                boundary: *boundary,
                keys: keys
                    .iter()
                    .map(|(_, entry, key)| {
                        (
                            *key,
                            positions
                                .iter()
                                .filter(|position| *position < entry)
                                .count() as u32,
                        )
                    })
                    .collect(),
            });
        }
        if let Some(key) = new_keys
            .iter()
            .find(|key| sub_planned.contains_key(key) && !owner_of_new.contains_key(key))
        {
            return Err(format!(
                "{} is new and planned outside an added label's analysis",
                index.keys[*key as usize].label
            ));
        }
    }
    for key in &sub.printed {
        if analyzed(&mapped[*key as usize]) {
            return Err(format!(
                "{} printed while it was re-analyzed",
                sub.keys[*key as usize].label
            ));
        }
    }
    for key in &sub.shared {
        if analyzed(&mapped[*key as usize]) {
            return Err(format!(
                "{} feeds a shared computation",
                sub.keys[*key as usize].label
            ));
        }
    }
    // The distinct aspect applications (the window's count).
    let before: BTreeSet<&str> = index
        .aspects
        .iter()
        .map(|(_, application)| application.as_str())
        .collect();
    let mut after: BTreeSet<&str> = index
        .aspects
        .iter()
        .filter(|(key, _)| !dirty.contains(key) && !removed.contains(key))
        .map(|(_, application)| application.as_str())
        .collect();
    after.extend(
        sub.aspects
            .iter()
            .filter(|(key, _)| mapped.get(*key as usize).is_some_and(analyzed))
            .map(|(_, application)| application.as_str()),
    );
    if before != after && added.is_empty() && removed.is_empty() {
        return Err("the aspect applications changed".to_owned());
    }
    let aspects = (before.len(), after.len());
    let loaded: BTreeSet<(&str, &str)> = index
        .packages
        .iter()
        .filter(|package| package.loaded)
        .map(|package| (package.repository.as_str(), package.package.as_str()))
        .collect();
    if let Some(package) = sub.packages.iter().find(|package| {
        package.loaded && !loaded.contains(&(package.repository.as_str(), package.package.as_str()))
    }) {
        return Err(format!(
            "the re-analysis loaded package @@{}//{}, which the saved analysis did not",
            package.repository, package.package
        ));
    }
    // A package only re-analyzed or removed targets looked up before, and
    // none now, may no longer be loaded (the window's count would change)
    // unless something else loads it.
    let by_identity: std::collections::HashMap<(&str, &str), u32> = index
        .packages
        .iter()
        .enumerate()
        .map(|(id, package)| {
            (
                (package.repository.as_str(), package.package.as_str()),
                id as u32,
            )
        })
        .collect();
    let mut still: BTreeSet<u32> = BTreeSet::new();
    for (key, package) in &sub.accesses {
        if analyzed(&mapped[*key as usize]) {
            let package = &sub.packages[*package as usize];
            if let Some(id) =
                by_identity.get(&(package.repository.as_str(), package.package.as_str()))
            {
                still.insert(*id);
            }
        }
    }
    let mut elsewhere: BTreeSet<u32> = BTreeSet::new();
    for (key, package) in &index.accesses {
        if !dirty.contains(key) && !removed.contains(key) {
            elsewhere.insert(*package);
        }
    }
    for (id, key) in index.keys.iter().enumerate() {
        if removed.contains(&(id as u32)) {
            continue;
        }
        if let Some(package) =
            by_identity.get(&(key.label.repository.as_str(), key.label.package.as_str()))
        {
            elsewhere.insert(*package);
        }
    }
    for (key, package) in &index.accesses {
        if (dirty.contains(key) || removed.contains(key))
            && !still.contains(package)
            && !elsewhere.contains(package)
            && !index.packages[*package as usize].pattern
            && !index.packages[*package as usize].global
        {
            let package = &index.packages[*package as usize];
            return Err(format!(
                "package @@{}//{} may no longer be loaded",
                package.repository, package.package
            ));
        }
    }
    // The re-analyzed targets' edges are the re-analysis's (each target's
    // in the order it reached them); the removed ones' are gone.
    let mut edges: Vec<(u32, u32)> = index
        .edges
        .iter()
        .filter(|(parent, _)| !dirty.contains(parent) && !removed.contains(parent))
        .copied()
        .collect();
    let mut seen: std::collections::HashSet<(u32, u32)> = edges.iter().copied().collect();
    for edge in sub_edges {
        if seen.insert(edge) {
            edges.push(edge);
        }
    }
    index.edges = edges;
    Ok(Graph {
        mapped,
        new_keys,
        inserts,
        aspects,
    })
}

/// The index an incremental save writes: `index` (the saved one, after
/// [`check_graph`]) with the re-loaded packages' fingerprints, the
/// re-analyzed keys' lookups, and every flag the partial analysis set.
pub fn merge_index(
    mut index: Index,
    dirty: &std::collections::BTreeSet<u32>,
    sub: Option<(&Index, &[u32])>,
    fingerprints: &std::collections::BTreeMap<u32, Fingerprint>,
) -> Index {
    use std::collections::{BTreeSet, HashMap};
    for (id, fingerprint) in fingerprints {
        if let Some(package) = index.packages.get_mut(*id as usize) {
            package.fingerprint = Some(fingerprint.clone());
        }
    }
    let Some((sub, mapped)) = sub else {
        return index;
    };
    let mut package_ids: HashMap<(String, String), u32> = index
        .packages
        .iter()
        .enumerate()
        .map(|(id, package)| {
            (
                (package.repository.clone(), package.package.clone()),
                id as u32,
            )
        })
        .collect();
    let mut package_map = Vec::with_capacity(sub.packages.len());
    for package in &sub.packages {
        let identity = (package.repository.clone(), package.package.clone());
        let id = match package_ids.get(&identity) {
            Some(id) => *id,
            None => {
                let id = index.packages.len() as u32;
                index.packages.push(IndexPackage {
                    fingerprint: None,
                    ..package.clone()
                });
                package_ids.insert(identity, id);
                id
            }
        };
        let merged = &mut index.packages[id as usize];
        if package.global && !merged.global {
            merged.global = true;
            merged.why.clone_from(&package.why);
        }
        merged.pattern |= package.pattern;
        merged.printed |= package.printed;
        merged.loaded |= package.loaded;
        package_map.push(id);
    }
    let mut accesses: BTreeSet<(u32, u32)> = index
        .accesses
        .iter()
        .filter(|(key, _)| !dirty.contains(key))
        .copied()
        .collect();
    for (key, package) in &sub.accesses {
        let key = mapped[*key as usize];
        if dirty.contains(&key) {
            accesses.insert((key, package_map[*package as usize]));
        }
    }
    index.accesses = accesses.into_iter().collect();
    let mut label_accesses: BTreeSet<(u32, u32, String)> = index
        .label_accesses
        .iter()
        .filter(|(key, _, _)| !dirty.contains(key))
        .cloned()
        .collect();
    for (key, package, name) in &sub.label_accesses {
        let key = mapped[*key as usize];
        if dirty.contains(&key) {
            label_accesses.insert((key, package_map[*package as usize], name.clone()));
        }
    }
    index.label_accesses = label_accesses.into_iter().collect();
    // The re-analyzed keys' aspect applications are the re-analysis's.
    let mut aspects: BTreeSet<(u32, String)> = index
        .aspects
        .iter()
        .filter(|(key, _)| !dirty.contains(key))
        .cloned()
        .collect();
    for (key, application) in &sub.aspects {
        let key = mapped.get(*key as usize).copied().unwrap_or(u32::MAX);
        if dirty.contains(&key) {
            aspects.insert((key, application.clone()));
        }
    }
    index.aspects = aspects.into_iter().collect();
    let mut shared: BTreeSet<u32> = index.shared.iter().copied().collect();
    shared.extend(sub.shared.iter().map(|key| mapped[*key as usize]));
    index.shared = shared.into_iter().collect();
    let mut inputs: BTreeSet<(PathBufWire, bool, Option<String>)> =
        index.repository_inputs.into_iter().collect();
    inputs.extend(sub.repository_inputs.iter().cloned());
    index.repository_inputs = inputs.into_iter().collect();
    let mut roots: std::collections::BTreeMap<String, PathBufWire> =
        index.repository_roots.into_iter().collect();
    roots.extend(sub.repository_roots.iter().cloned());
    index.repository_roots = roots.into_iter().collect();
    index
}

/// The merged index's request (after [`merge_index`]): the requested
/// labels now, their roots, the plan positions with the removed labels'
/// keys dropped and the added labels' new ones inserted, the hidden
/// artifacts' producers; the removed keys compacted away. `analyzed`: the
/// partial analysis's labels, `sub` its index and key mapping.
pub fn reshape_index(
    mut index: Index,
    labels: &[Label],
    analyzed: &[Label],
    sub: Option<(&Index, &[u32])>,
    removed: &std::collections::BTreeSet<u32>,
    inserts: &[Insert],
) -> Index {
    use std::collections::{BTreeSet, HashMap, HashSet};
    let now: HashMap<&Label, u32> = labels
        .iter()
        .enumerate()
        .map(|(position, label)| (label, position as u32))
        .collect();
    let analyzed: HashSet<&Label> = analyzed.iter().collect();
    // The hidden artifacts' producers and the roots.
    let mut demand: BTreeSet<(u32, String)> = BTreeSet::new();
    let mut roots: BTreeSet<(u32, u32)> = BTreeSet::new();
    let kept_position = |position: u32| -> Option<u32> {
        let label = index.labels.get(position as usize)?;
        if analyzed.contains(label) {
            return None;
        }
        now.get(label).copied()
    };
    for (position, id) in &index.demand {
        if *position == u32::MAX {
            demand.insert((*position, id.clone()));
        } else if let Some(position) = kept_position(*position) {
            demand.insert((position, id.clone()));
        }
    }
    for (position, key) in &index.roots {
        if let Some(position) = kept_position(*position) {
            roots.insert((position, *key));
        }
    }
    if let Some((sub, mapped)) = sub {
        let position = |at: u32| sub.labels.get(at as usize).and_then(|label| now.get(label));
        for (at, id) in &sub.demand {
            if *at == u32::MAX {
                demand.insert((*at, id.clone()));
            } else if let Some(position) = position(*at) {
                demand.insert((*position, id.clone()));
            }
        }
        for (at, key) in &sub.roots {
            if let Some(position) = position(*at) {
                roots.insert((*position, mapped[*key as usize]));
            }
        }
    }
    // The plan positions.
    let mut kept: Vec<u32> = index
        .planned
        .iter()
        .filter(|(key, _, _)| !removed.contains(key))
        .map(|(_, _, position)| *position)
        .collect();
    kept.sort_unstable();
    let kept_before = |position: u32| kept.partition_point(|at| *at < position) as u32;
    let new_up_to = |boundary: u32, inclusive: bool| -> u32 {
        inserts
            .iter()
            .filter(|insert| {
                if inclusive {
                    insert.boundary <= boundary
                } else {
                    insert.boundary < boundary
                }
            })
            .map(|insert| insert.keys.len() as u32)
            .sum()
    };
    let map_after = |position: u32| kept_before(position) + new_up_to(position, true);
    let mut planned: Vec<(u32, u32, u32)> = index
        .planned
        .iter()
        .filter(|(key, _, _)| !removed.contains(key))
        .map(|(key, entry, position)| (*key, map_after(*entry), map_after(*position)))
        .collect();
    let mut bases: Vec<u32> = Vec::with_capacity(inserts.len());
    for (group, insert) in inserts.iter().enumerate() {
        let offset: u32 = inserts[..group]
            .iter()
            .filter(|earlier| earlier.boundary == insert.boundary)
            .map(|earlier| earlier.keys.len() as u32)
            .sum();
        let base = kept_before(insert.boundary) + new_up_to(insert.boundary, false) + offset;
        bases.push(base);
        for (at, (key, entry)) in insert.keys.iter().enumerate() {
            planned.push((*key, base + entry, base + at as u32));
        }
    }
    planned.sort_unstable();
    let total = kept.len() as u32
        + inserts
            .iter()
            .map(|insert| insert.keys.len() as u32)
            .sum::<u32>();
    let mut root_entries: Vec<u32> = Vec::new();
    if index.root_entries.len() == index.labels.len() + 1 {
        let old: HashMap<&Label, u32> = index
            .labels
            .iter()
            .enumerate()
            .map(|(position, label)| (label, position as u32))
            .collect();
        let mut added = bases.iter();
        for label in labels {
            match old.get(label) {
                Some(position) => {
                    root_entries.push(map_after(index.root_entries[*position as usize]));
                }
                None => match added.next() {
                    Some(base) => root_entries.push(*base),
                    None => {
                        root_entries.clear();
                        break;
                    }
                },
            }
        }
        if root_entries.len() == labels.len() {
            root_entries.push(total);
        } else {
            root_entries.clear();
        }
    }
    index.labels = labels.to_vec();
    index.demand = demand.into_iter().collect();
    index.roots = roots.into_iter().collect();
    index.planned = planned;
    index.root_entries = root_entries;
    if removed.is_empty() {
        return index;
    }
    // Compact the removed keys away.
    let mut ids: Vec<u32> = Vec::with_capacity(index.keys.len());
    let mut next = 0u32;
    for id in 0..index.keys.len() as u32 {
        if removed.contains(&id) {
            ids.push(u32::MAX);
        } else {
            ids.push(next);
            next += 1;
        }
    }
    let map = |id: u32| ids.get(id as usize).copied().unwrap_or(u32::MAX);
    let alive = |id: &u32| map(*id) != u32::MAX;
    index.keys = std::mem::take(&mut index.keys)
        .into_iter()
        .enumerate()
        .filter(|(id, _)| !removed.contains(&(*id as u32)))
        .map(|(_, key)| key)
        .collect();
    index.edges = index
        .edges
        .iter()
        .filter(|(parent, child)| alive(parent) && alive(child))
        .map(|(parent, child)| (map(*parent), map(*child)))
        .collect();
    index.accesses = index
        .accesses
        .iter()
        .filter(|(key, _)| alive(key))
        .map(|(key, package)| (map(*key), *package))
        .collect();
    index.label_accesses = index
        .label_accesses
        .iter()
        .filter(|(key, _, _)| alive(key))
        .map(|(key, package, name)| (map(*key), *package, name.clone()))
        .collect();
    index.roots = index
        .roots
        .iter()
        .filter(|(_, key)| alive(key))
        .map(|(position, key)| (*position, map(*key)))
        .collect();
    index.shared = index
        .shared
        .iter()
        .filter(|key| alive(key))
        .map(|key| map(*key))
        .collect();
    index.printed = index
        .printed
        .iter()
        .filter(|key| alive(key))
        .map(|key| map(*key))
        .collect();
    index.aspects = index
        .aspects
        .iter()
        .filter(|(key, _)| *key == u32::MAX || alive(key))
        .map(|(key, application)| {
            (
                if *key == u32::MAX { *key } else { map(*key) },
                application.clone(),
            )
        })
        .collect();
    index.rule_modules = index
        .rule_modules
        .iter()
        .filter(|(key, _)| alive(key))
        .map(|(key, module)| (map(*key), *module))
        .collect();
    index.planned = index
        .planned
        .iter()
        .filter(|(key, _, _)| alive(key))
        .map(|(key, entry, position)| (map(*key), *entry, *position))
        .collect();
    index
}

/// How the requested labels changed for [`splice`].
pub struct Shape<'a> {
    /// The requested labels now ([`Index::labels`] when they did not
    /// change).
    pub labels: &'a [Label],
    /// The removed labels' keys ([`Dirty::removed`]).
    pub removed: &'a std::collections::BTreeSet<u32>,
    /// The added labels' new keys ([`Graph::new_keys`]).
    pub new_keys: &'a std::collections::BTreeSet<u32>,
    /// Where the new keys' actions go ([`Graph::inserts`]).
    pub inserts: &'a [Insert],
    /// The partial analysis's index (its labels' hidden top-level
    /// artifacts).
    pub sub: &'a Index,
}

impl Shape<'_> {
    fn relabels(&self, index: &Index) -> bool {
        self.labels != index.labels.as_slice()
            || !self.removed.is_empty()
            || !self.new_keys.is_empty()
    }
}

/// Splice the partial analysis `sub` of the requested `labels` (whose
/// configured targets `dirty` holds, base ids, the new ones included) into
/// the saved request `full`: each re-analyzed target's actions replace its
/// saved ones in place, a new one's go where an added label's analysis
/// planned it, a removed one's are dropped; every other action the partial
/// analysis planned must equal the saved one; the actions no longer
/// reachable from the requested outputs are pruned like the full
/// analysis's prune; the requested labels' reported outputs must be the
/// saved ones (or, when the requested labels changed, are put in their
/// place). Any difference is the error (the run falls back).
pub fn splice(
    full: crate::request::PlannedRequest,
    index: &Index,
    labels: &[Label],
    dirty: &std::collections::BTreeSet<u32>,
    sub: crate::request::PlannedRequest,
    shape: &Shape<'_>,
) -> Result<crate::request::PlannedRequest, String> {
    use crate::scheduler::{InputGroup, PlannedAction};
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::sync::Arc;
    let relabels = shape.relabels(index);
    if relabels && !matches!(full.command, crate::request::RequestCommand::Build) {
        return Err("the requested labels of a test or run changed".to_owned());
    }
    // The plan reaches what the saved roots say: checked on the saved plan
    // (a root it does not know would prune a needed action).
    reachable(&full, &saved_demand(index, |_| true), "the saved plan")?;
    if !full.warnings.is_empty() || !sub.warnings.is_empty() {
        return Err("the analysis reported warnings".to_owned());
    }
    if !full.errors.is_empty() || !sub.errors.is_empty() {
        return Err("the analysis reported errors".to_owned());
    }
    if !full.build_info.is_empty() || !sub.build_info.is_empty() {
        return Err("build-info callbacks".to_owned());
    }
    if full.keep_going.is_some() || sub.keep_going.is_some() {
        return Err("--keep_going".to_owned());
    }
    if full.execute != sub.execute {
        return Err("--nobuild differs".to_owned());
    }
    let requested_labels: HashSet<&Label> = labels.iter().collect();
    // The command's part: `test`'s tests and `run`'s test properties are
    // the re-analysis's for the re-analyzed labels.
    let command = {
        use crate::request::RequestCommand;
        match (&full.command, &sub.command) {
            (RequestCommand::Build, RequestCommand::Build) => RequestCommand::Build,
            (
                RequestCommand::Test { build, tests },
                RequestCommand::Test {
                    tests: sub_tests, ..
                },
            ) => {
                let mut newer: HashMap<&Label, &crate::request::PlannedTest> =
                    sub_tests.iter().map(|test| (&test.label, test)).collect();
                let mut merged = Vec::with_capacity(tests.len());
                for test in tests {
                    if requested_labels.contains(&test.label) {
                        merged.push(
                            (*newer.remove(&test.label).ok_or_else(|| {
                                format!("{} is no longer a test of the request", test.label)
                            })?)
                            .clone(),
                        );
                    } else {
                        merged.push(test.clone());
                    }
                }
                if !newer.is_empty() {
                    return Err("the re-analysis tests other targets".to_owned());
                }
                RequestCommand::Test {
                    build: build.clone(),
                    tests: merged,
                }
            }
            (
                RequestCommand::Run { label, test },
                RequestCommand::Run {
                    label: sub_label,
                    test: sub_test,
                },
            ) if label == sub_label => RequestCommand::Run {
                label: label.clone(),
                test: if requested_labels.contains(label) {
                    sub_test.clone()
                } else {
                    test.clone()
                },
            },
            _ => return Err("the re-analysis is another command's".to_owned()),
        }
    };
    // The requested labels' parts (the reachability roots below).
    let parts = if relabels {
        Some(relabeled_parts(
            &full,
            &sub,
            index,
            shape,
            &requested_labels,
        )?)
    } else {
        None
    };
    // The owners and whether they were re-analyzed; the removed and new
    // ones.
    let mut owners: HashMap<String, bool> = HashMap::new();
    let mut removed_owners: HashSet<String> = HashSet::new();
    let mut new_owners: HashSet<String> = HashSet::new();
    let mut owner_positions: HashMap<String, u32> = HashMap::new();
    let positions: HashMap<u32, u32> = index
        .planned
        .iter()
        .map(|(key, _, position)| (*key, *position))
        .collect();
    for (id, key) in index.keys.iter().enumerate() {
        let id = id as u32;
        if let Some(owner) = owner_of_key(key, &index.top) {
            if let Some(position) = positions.get(&id) {
                owner_positions.insert(owner.clone(), *position);
            }
            if shape.removed.contains(&id) {
                removed_owners.insert(owner.clone());
            }
            if shape.new_keys.contains(&id) {
                new_owners.insert(owner.clone());
            }
            let entry = owners.entry(owner).or_insert(false);
            *entry |= dirty.contains(&id);
        }
    }
    for (id, key) in index.keys.iter().enumerate() {
        if let Some(owner) = owner_of_key(key, &index.top)
            && removed_owners.contains(&owner)
            && !shape.removed.contains(&(id as u32))
        {
            return Err(format!("{owner} is removed and kept"));
        }
    }
    let owner = |node: &PlannedAction| owner_of_action(&node.action.id, &owners);
    // The partial analysis's actions: re-analyzed owners' runs, the others
    // checked against the saved ones.
    let full_at: HashMap<&str, usize> = full
        .nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.action.id.as_str(), at))
        .collect();
    let mut groups: HashMap<*const InputGroup, Arc<InputGroup>> = HashMap::new();
    let mut map_groups = |from: &PlannedAction, to: &PlannedAction| -> Result<(), String> {
        if from.groups.len() != to.groups.len() {
            return Ok(());
        }
        for (from, to) in from.groups.iter().zip(&to.groups) {
            if !crate::snapshot::codec::same_group(from, to) {
                return Ok(());
            }
            match groups.get(&Arc::as_ptr(from)) {
                Some(mapped) if !Arc::ptr_eq(mapped, to) => {
                    return Err(format!(
                        "action '{}': an input group shared differently",
                        from.root.display()
                    ));
                }
                Some(_) => {}
                None => {
                    groups.insert(Arc::as_ptr(from), to.clone());
                }
            }
        }
        Ok(())
    };
    let mut runs: Vec<(String, Vec<PlannedAction>)> = Vec::new();
    let mut run_of: HashMap<String, usize> = HashMap::new();
    let mut last_owner: Option<String> = None;
    for node in sub.nodes {
        match owner(&node) {
            Some((name, true)) => {
                let name = name.to_owned();
                match run_of.get(&name) {
                    Some(run) if last_owner.as_deref() == Some(name.as_str()) => {
                        runs[*run].1.push(node);
                    }
                    Some(_) => {
                        return Err(format!(
                            "the re-analyzed actions of {name} are not contiguous"
                        ));
                    }
                    None => {
                        run_of.insert(name.clone(), runs.len());
                        runs.push((name.clone(), vec![node]));
                    }
                }
                last_owner = Some(name);
            }
            _ => {
                last_owner = None;
                let at = full_at.get(node.action.id.as_str()).ok_or_else(|| {
                    format!(
                        "the re-analysis planned action '{}', which the saved plan does not have",
                        node.action.id
                    )
                })?;
                let saved = &full.nodes[*at];
                if node_bytes(saved)? != node_bytes(&node)?
                    || saved.groups.len() != node.groups.len()
                    || saved
                        .groups
                        .iter()
                        .zip(&node.groups)
                        .any(|(a, b)| !crate::snapshot::codec::same_group(a, b))
                {
                    return Err(format!(
                        "action '{}' of a target that was not re-analyzed changed",
                        node.action.id
                    ));
                }
                map_groups(&node, saved)?;
            }
        }
    }
    // The saved plan's runs of re-analyzed owners: contiguous, replaced in
    // place.
    let mut seen_runs: HashSet<String> = HashSet::new();
    let mut previous: Option<String> = None;
    let mut kept: Vec<PlannedAction> = Vec::with_capacity(full.nodes.len());
    let mut replaced: Vec<(usize, String)> = Vec::new();
    for node in full.nodes {
        if owner(&node).is_some_and(|(name, _)| removed_owners.contains(name)) {
            previous = None;
            continue;
        }
        match owner(&node) {
            Some((name, true)) => {
                if previous.as_deref() != Some(name) {
                    if !seen_runs.insert(name.to_owned()) {
                        return Err(format!("the saved actions of {name} are not contiguous"));
                    }
                    replaced.push((kept.len(), name.to_owned()));
                }
                previous = Some(name.to_owned());
                // The saved run's groups map to the re-analysis's by position.
                if let Some(run) = run_of.get(name)
                    && let Some(newer) = runs[*run]
                        .1
                        .iter()
                        .find(|newer| newer.action.id == node.action.id)
                {
                    map_groups(newer, &node)?;
                }
            }
            _ => {
                previous = None;
                kept.push(node);
            }
        }
    }
    for (name, _) in &runs {
        if !seen_runs.contains(name) && !new_owners.contains(name) {
            return Err(format!(
                "{name} has actions now but had none in the saved plan"
            ));
        }
    }
    // Assemble: each re-analyzed run where the saved one was.
    let mut by_name: HashMap<String, Vec<PlannedAction>> = runs.into_iter().collect();
    let mut nodes = Vec::with_capacity(kept.len() + by_name.values().map(Vec::len).sum::<usize>());
    let mut kept = kept.into_iter();
    let mut taken = 0;
    for (at, name) in replaced {
        while taken < at {
            nodes.push(kept.next().expect("kept nodes"));
            taken += 1;
        }
        for mut node in by_name.remove(&name).unwrap_or_default() {
            for group in &mut node.groups {
                if let Some(saved) = groups.get(&Arc::as_ptr(group)) {
                    *group = saved.clone();
                }
            }
            nodes.push(node);
        }
    }
    nodes.extend(kept);
    // The new targets' runs where their added labels' analyses planned
    // them: after every action of a target planned before, before every
    // one of a target planned after (an action of no planned target in
    // between is ambiguous).
    if !shape.inserts.is_empty() {
        let key_owner = |key: u32| owner_of_key(&index.keys[key as usize], &index.top);
        let at: Vec<Option<u32>> = nodes
            .iter()
            .map(|node| owner(node).and_then(|(name, _)| owner_positions.get(name).copied()))
            .collect();
        let mut placed: Vec<(usize, usize, Vec<PlannedAction>)> = Vec::new();
        for (group, insert) in shape.inserts.iter().enumerate() {
            let mut run: Vec<PlannedAction> = Vec::new();
            for (key, _) in &insert.keys {
                if let Some(name) = key_owner(*key) {
                    run.extend(by_name.remove(&name).unwrap_or_default());
                }
            }
            if run.is_empty() {
                continue;
            }
            let before = at
                .iter()
                .rposition(|position| position.is_some_and(|position| position < insert.boundary));
            let after = at
                .iter()
                .position(|position| position.is_some_and(|position| position >= insert.boundary));
            let from = before.map_or(0, |at| at + 1);
            let to = after.unwrap_or(nodes.len());
            // A workspace status node after the last earlier target's
            // actions was registered by a later target, or at the end of
            // phase 2: after the new targets either way.
            if to < from
                || nodes[from..to]
                    .iter()
                    .any(|node| !node.action.id.starts_with(crate::request::STATUS_ID_PREFIX))
            {
                return Err(format!(
                    "the new actions of {} have no unambiguous place in the plan (between {} and {})",
                    index.keys[insert.keys[0].0 as usize].label,
                    before.map_or("the start", |at| nodes[at].action.id.as_str()),
                    after.map_or("the end", |at| nodes[at].action.id.as_str()),
                ));
            }
            placed.push((from, group, run));
        }
        placed.sort_by_key(|(from, group, _)| (*from, *group));
        let mut merged =
            Vec::with_capacity(nodes.len() + placed.iter().map(|p| p.2.len()).sum::<usize>());
        let mut placed = placed.into_iter().peekable();
        for (at, node) in nodes.into_iter().enumerate() {
            while placed.peek().is_some_and(|(from, _, _)| *from == at) {
                merged.extend(placed.next().expect("peeked").2);
            }
            merged.push(node);
        }
        for (_, _, run) in placed {
            merged.extend(run);
        }
        nodes = merged;
        for node in &mut nodes {
            for group in &mut node.groups {
                if let Some(saved) = groups.get(&Arc::as_ptr(group)) {
                    *group = saved.clone();
                }
            }
        }
    }
    if let Some(name) = by_name.keys().next() {
        return Err(format!(
            "the re-analyzed actions of {name} have no place in the plan"
        ));
    }
    // What the plan reaches now: the actions no longer needed (a removed
    // dependency, a removed label's) are pruned like the full analysis
    // prunes them.
    {
        let (requested, aspect_reports) = match &parts {
            Some(parts) => (&parts.requested, &parts.aspect_reports),
            None => (&full.requested, &full.aspect_reports),
        };
        let mut roots = merged_demand(index, shape, labels);
        roots.extend(
            requested
                .iter()
                .map(|(_, produced)| produced)
                .chain(aspect_reports.iter().flat_map(|(_, _, produced)| produced))
                .filter_map(|produced| produced.producer.as_ref().map(|(id, _)| id.clone())),
        );
        let required: HashSet<String> = reach(&nodes, roots)?
            .into_iter()
            .map(str::to_owned)
            .collect();
        nodes.retain(|node| required.contains(node.action.id.as_str()));
    }
    // Every dependency is planned, and no action of a target that was not
    // re-analyzed consumes one that was (its target would be dirty).
    let planned: HashSet<&str> = nodes.iter().map(|node| node.action.id.as_str()).collect();
    for node in &nodes {
        let consumer_dirty = owner(node).is_some_and(|(_, dirty)| dirty);
        for dependency in node.all_dependencies() {
            if !planned.contains(dependency.action_id.as_str()) {
                return Err(format!(
                    "action '{}' depends on '{}', which the spliced plan does not have",
                    node.action.id, dependency.action_id
                ));
            }
            if !consumer_dirty
                && owner_of_action(&dependency.action_id, &owners).is_some_and(|(_, dirty)| dirty)
            {
                return Err(format!(
                    "action '{}' of a target that was not re-analyzed consumes '{}'",
                    node.action.id, dependency.action_id
                ));
            }
        }
    }
    // No output of a re-analyzed action is another action's, or lies
    // below or above one (the full analysis reports such a conflict; the
    // partial one cannot see the actions it did not plan).
    let mut outputs: HashMap<&std::path::Path, &str> = HashMap::new();
    let mut directories: HashSet<&std::path::Path> = HashSet::new();
    for node in &nodes {
        for output in &node.action.outputs {
            if let Some(other) = outputs.insert(output.as_path(), node.action.id.as_str())
                && other != node.action.id
                && (owner(node).is_some_and(|(_, dirty)| dirty)
                    || owner_of_action(other, &owners).is_some_and(|(_, dirty)| dirty))
            {
                return Err(format!(
                    "actions '{other}' and '{}' both write {}",
                    node.action.id,
                    output.display()
                ));
            }
            directories.extend(output.ancestors().skip(1));
        }
    }
    for node in nodes
        .iter()
        .filter(|node| owner(node).is_some_and(|(_, dirty)| dirty))
    {
        for output in &node.action.outputs {
            if directories.contains(output.as_path())
                || output
                    .ancestors()
                    .skip(1)
                    .any(|ancestor| outputs.contains_key(ancestor))
            {
                return Err(format!(
                    "an output of action '{}' is a prefix of another output, or has one: {}",
                    node.action.id,
                    output.display()
                ));
            }
        }
    }
    // The requested labels' reported outputs are the saved ones, and no
    // other label reports one a re-analyzed action produces.
    let produced_by_dirty = |produced: &crate::request::Produced| {
        produced
            .producer
            .as_ref()
            .is_some_and(|(id, _)| owner_of_action(id, &owners).is_some_and(|(_, dirty)| dirty))
    };
    let other_reports = full
        .requested
        .iter()
        .filter(|(label, _)| !requested_labels.contains(label))
        .map(|(_, produced)| produced)
        .chain(
            full.results
                .iter()
                .filter(|(label, _)| !requested_labels.contains(label))
                .flat_map(|(_, produced)| produced),
        )
        .chain(
            full.aspect_reports
                .iter()
                .filter(|(_, label, _)| !requested_labels.contains(label))
                .flat_map(|(_, _, produced)| produced),
        );
    for produced in other_reports {
        if produced_by_dirty(produced) {
            return Err(format!(
                "a target that was not re-analyzed reports {}, which a re-analyzed one produces",
                produced.real.display()
            ));
        }
    }
    let encoded = |value: &dyn erased::Encode| value.bytes();
    let full_requested: Vec<_> = full
        .requested
        .iter()
        .filter(|(label, _)| requested_labels.contains(label))
        .collect();
    let sub_requested: Vec<_> = sub.requested.iter().collect();
    if parts.is_none() && encoded(&full_requested) != encoded(&sub_requested) {
        return Err("the requested targets' outputs changed".to_owned());
    }
    let full_results: Vec<_> = full
        .results
        .iter()
        .filter(|(label, _)| requested_labels.contains(label))
        .collect();
    let sub_results: Vec<_> = sub.results.iter().collect();
    if parts.is_none() && encoded(&full_results) != encoded(&sub_results) {
        return Err("the requested targets' results changed".to_owned());
    }
    let full_aspects: Vec<_> = full
        .aspect_reports
        .iter()
        .filter(|(_, label, _)| requested_labels.contains(label))
        .collect();
    let sub_aspects: Vec<_> = sub.aspect_reports.iter().collect();
    if parts.is_none() && encoded(&full_aspects) != encoded(&sub_aspects) {
        return Err("the requested targets' aspect outputs changed".to_owned());
    }
    // The requested labels skipped as incompatible are the saved ones.
    let full_skipped: Vec<&Label> = full
        .skipped
        .iter()
        .filter(|label| requested_labels.contains(label))
        .collect();
    if parts.is_none() && full_skipped != sub.skipped.iter().collect::<Vec<_>>() {
        return Err("the requested targets skipped as incompatible changed".to_owned());
    }
    // The run plans (`test`'s tests, `run`'s target): the re-analyzed
    // labels' are the re-analysis's, in the saved order and configuration;
    // the actions of other targets they need are the ones the saved plans
    // needed (the plan keeps exactly the actions they reach), and no other
    // plan needs a re-analyzed action.
    let dirty_owner = |id: &str| owner_of_action(id, &owners).is_some_and(|(_, dirty)| dirty);
    let mut newer_runs: HashMap<&Label, &crate::request::PlannedRun> = sub
        .run_plans
        .iter()
        .map(|(label, plan)| (label, plan))
        .collect();
    let mut run_plans = Vec::with_capacity(full.run_plans.len());
    let mut needed_before: BTreeSet<&str> = BTreeSet::new();
    let mut needed_after: BTreeSet<&str> = BTreeSet::new();
    for (label, plan) in &full.run_plans {
        if plan.coverage.is_some() {
            return Err("a coverage run plan".to_owned());
        }
        if !requested_labels.contains(label) {
            if let Some(id) = run_plan_producers(plan).find(|id| dirty_owner(id)) {
                return Err(format!(
                    "the run plan of {label}, which was not re-analyzed, needs '{id}'"
                ));
            }
            run_plans.push((label.clone(), plan.clone()));
            continue;
        }
        let newer = newer_runs
            .remove(label)
            .ok_or_else(|| format!("{label} has no run plan now"))?;
        if newer.coverage.is_some() {
            return Err("a coverage run plan".to_owned());
        }
        let configuration = |configurations: &[crate::request::ConfigurationSummary], at: usize| {
            configurations
                .get(at)
                .and_then(|summary| postcard::to_stdvec(summary).ok())
        };
        if configuration(&full.configurations, plan.configuration).is_none()
            || configuration(&full.configurations, plan.configuration)
                != configuration(&sub.configurations, newer.configuration)
        {
            return Err(format!("the configuration of {label}'s run plan changed"));
        }
        needed_before.extend(run_plan_producers(plan).filter(|id| !dirty_owner(id)));
        needed_after.extend(run_plan_producers(newer).filter(|id| !dirty_owner(id)));
        run_plans.push((
            label.clone(),
            crate::request::PlannedRun {
                configuration: plan.configuration,
                ..newer.clone()
            },
        ));
    }
    if !newer_runs.is_empty() {
        return Err("the re-analysis planned other run targets".to_owned());
    }
    if let Some(dropped) = needed_before.difference(&needed_after).next() {
        return Err(format!(
            "the re-analyzed run plans no longer need '{dropped}' of a target that was not re-analyzed"
        ));
    }
    let planned_ids: HashSet<&str> = nodes.iter().map(|node| node.action.id.as_str()).collect();
    for (label, plan) in &run_plans {
        if let Some(id) = run_plan_producers(plan).find(|id| {
            !id.starts_with(crate::request::STATUS_ID_PREFIX) && !planned_ids.contains(id)
        }) {
            return Err(format!(
                "the run plan of {label} needs '{id}', which the spliced plan does not have"
            ));
        }
    }
    drop(planned_ids);
    let printed: HashSet<String> = labels.iter().map(crate::test_runner::label_print).collect();
    let bep_of = |plan: &crate::bep::plan::BepPlan,
                  requested: &[(Label, crate::request::Produced)]|
     -> Vec<u8> {
        let targets: Vec<_> = plan
            .targets
            .iter()
            .filter(|target| printed.contains(&target.label))
            .map(|target| {
                let groups: Vec<_> = target
                    .groups
                    .iter()
                    .map(|(name, files)| {
                        (
                            name,
                            files
                                .iter()
                                .map(|at| requested.get(*at))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                (
                    &target.label,
                    &target.kind,
                    &target.tags,
                    &target.completion_tags,
                    &target.test,
                    plan.configurations.get(target.configuration),
                    groups,
                )
            })
            .collect();
        postcard::to_stdvec(&targets).unwrap_or_default()
    };
    if parts.is_none() && bep_of(&full.bep, &full.requested) != bep_of(&sub.bep, &sub.requested) {
        return Err("the requested targets' build events changed".to_owned());
    }
    if let Some(parts) = parts {
        return Ok(crate::request::PlannedRequest {
            nodes,
            command,
            run_plans,
            requested: parts.requested,
            results: parts.results,
            aspect_reports: parts.aspect_reports,
            bep: parts.bep,
            ..full
        });
    }
    Ok(crate::request::PlannedRequest {
        nodes,
        command,
        run_plans,
        ..full
    })
}

/// The saved demand ([`Index::demand`]) of the requested labels `keep`
/// accepts (by position), and the workspace status's.
fn saved_demand(index: &Index, keep: impl Fn(u32) -> bool) -> Vec<String> {
    index
        .demand
        .iter()
        .filter(|(position, _)| *position == u32::MAX || keep(*position))
        .map(|(_, id)| id.clone())
        .collect()
}

/// The hidden top-level artifacts' producers after the splice: the saved
/// ones of the requested labels that were not re-analyzed (nor removed),
/// the partial analysis's of those it analyzed.
fn merged_demand(index: &Index, shape: &Shape<'_>, labels: &[Label]) -> Vec<String> {
    let analyzed: std::collections::HashSet<&Label> = labels.iter().collect();
    let requested: std::collections::HashSet<&Label> = shape.labels.iter().collect();
    let mut demand = saved_demand(index, |position| {
        index
            .labels
            .get(position as usize)
            .is_some_and(|label| requested.contains(label) && !analyzed.contains(label))
    });
    demand.extend(shape.sub.demand.iter().map(|(_, id)| id.clone()));
    demand
}

/// What the actions `roots` name reach in `plan` (every one must be
/// planned).
fn reach(
    nodes: &[crate::scheduler::PlannedAction],
    roots: Vec<String>,
) -> Result<std::collections::HashSet<&str>, String> {
    let by_id: std::collections::HashMap<&str, &crate::scheduler::PlannedAction> = nodes
        .iter()
        .map(|node| (node.action.id.as_str(), node))
        .collect();
    let mut required: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut todo: Vec<String> = roots;
    while let Some(id) = todo.pop() {
        let Some((id, node)) = by_id.get_key_value(id.as_str()) else {
            return Err(format!("the plan does not have the needed action '{id}'"));
        };
        if required.insert(id) {
            todo.extend(
                node.all_dependencies()
                    .map(|dependency| dependency.action_id.clone()),
            );
        }
    }
    Ok(required)
}

/// Every action of `plan` is reached from its reported outputs and the
/// hidden top-level artifacts `demand` names (else the roots are not the
/// full analysis's).
fn reachable(
    plan: &crate::request::PlannedRequest,
    demand: &[String],
    what: &str,
) -> Result<(), String> {
    let mut roots = demand.to_vec();
    roots.extend(
        plan.requested
            .iter()
            .map(|(_, produced)| produced)
            .chain(
                plan.aspect_reports
                    .iter()
                    .flat_map(|(_, _, produced)| produced),
            )
            .filter_map(|produced| produced.producer.as_ref().map(|(id, _)| id.clone())),
    );
    let required = reach(&plan.nodes, roots)?;
    if let Some(node) = plan
        .nodes
        .iter()
        .find(|node| !required.contains(node.action.id.as_str()))
    {
        return Err(format!(
            "{what} has action '{}', which its roots do not reach",
            node.action.id
        ));
    }
    Ok(())
}

/// The per-label parts of a request whose requested labels changed.
struct Parts {
    requested: Vec<(Label, crate::request::Produced)>,
    results: Vec<(Label, Vec<crate::request::Produced>)>,
    aspect_reports: Vec<(String, Label, Vec<crate::request::Produced>)>,
    bep: crate::bep::plan::BepPlan,
}

/// The per-label parts in the new label order: a label analyzed again or
/// added takes the partial analysis's, any other the saved request's; the
/// build events' output groups index the new `requested`.
fn relabeled_parts(
    full: &crate::request::PlannedRequest,
    sub: &crate::request::PlannedRequest,
    index: &Index,
    shape: &Shape<'_>,
    analyzed: &std::collections::HashSet<&Label>,
) -> Result<Parts, String> {
    use std::collections::HashMap;
    if !full.skipped.is_empty() || !sub.skipped.is_empty() {
        return Err("targets skipped as incompatible".to_owned());
    }
    let compatible = |plan: &crate::bep::plan::BepPlan| {
        plan.targets
            .iter()
            .all(|target| target.compatibility == crate::bep::plan::Compatibility::Compatible)
    };
    if !compatible(&full.bep) || !compatible(&sub.bep) {
        return Err("incompatible requested targets".to_owned());
    }
    if full.bep.targets.len() != index.labels.len() {
        return Err("the saved build events do not name each requested label once".to_owned());
    }
    // Contiguous per label, in label order (crate::engine's plan_build).
    fn by_label<T>(
        items: &[T],
        label: impl Fn(&T) -> &Label,
    ) -> Result<HashMap<&Label, (usize, usize)>, String> {
        let mut ranges: HashMap<&Label, (usize, usize)> = HashMap::new();
        let mut last: Option<&Label> = None;
        for (at, item) in items.iter().enumerate() {
            let current = label(item);
            if last != Some(current) {
                if ranges.contains_key(current) {
                    return Err(format!("the parts of {current} are not contiguous"));
                }
                ranges.insert(current, (at, at + 1));
            } else if let Some(range) = ranges.get_mut(current) {
                range.1 = at + 1;
            }
            last = Some(current);
        }
        Ok(ranges)
    }
    let full_requested = by_label(&full.requested, |(label, _)| label)?;
    let sub_requested = by_label(&sub.requested, |(label, _)| label)?;
    let full_aspects = by_label(&full.aspect_reports, |(_, label, _)| label)?;
    let sub_aspects = by_label(&sub.aspect_reports, |(_, label, _)| label)?;
    let full_results: HashMap<&Label, &(Label, Vec<crate::request::Produced>)> = full
        .results
        .iter()
        .map(|result| (&result.0, result))
        .collect();
    let sub_results: HashMap<&Label, &(Label, Vec<crate::request::Produced>)> = sub
        .results
        .iter()
        .map(|result| (&result.0, result))
        .collect();
    let printed = |plan: &crate::bep::plan::BepPlan| -> HashMap<String, usize> {
        plan.targets
            .iter()
            .enumerate()
            .map(|(at, target)| (target.label.clone(), at))
            .collect()
    };
    let (full_bep, sub_bep) = (printed(&full.bep), printed(&sub.bep));
    let mut parts = Parts {
        requested: Vec::new(),
        results: Vec::new(),
        aspect_reports: Vec::new(),
        bep: crate::bep::plan::BepPlan {
            configurations: Vec::new(),
            targets: Vec::new(),
        },
    };
    for label in shape.labels {
        let from_sub = analyzed.contains(label);
        let (plan, requested, aspects, results, bep) = if from_sub {
            (sub, &sub_requested, &sub_aspects, &sub_results, &sub_bep)
        } else {
            (
                full,
                &full_requested,
                &full_aspects,
                &full_results,
                &full_bep,
            )
        };
        let base = parts.requested.len();
        let (from, to) = requested.get(label).copied().unwrap_or((0, 0));
        parts
            .requested
            .extend(plan.requested[from..to].iter().cloned());
        let (from_aspects, to_aspects) = aspects.get(label).copied().unwrap_or((0, 0));
        parts.aspect_reports.extend(
            plan.aspect_reports[from_aspects..to_aspects]
                .iter()
                .cloned(),
        );
        if let Some(result) = results.get(label) {
            parts.results.push((*result).clone());
        }
        let target = bep
            .get(&crate::test_runner::label_print(label))
            .map(|at| &plan.bep.targets[*at])
            .ok_or_else(|| format!("{label} has no build events"))?;
        let mut target = target.clone();
        for (_, files) in &mut target.groups {
            for file in files.iter_mut() {
                if *file < from || *file >= to {
                    return Err(format!(
                        "an output group of {label} names another label's file"
                    ));
                }
                *file = *file - from + base;
            }
        }
        parts.bep.targets.push(target);
    }
    // The configurations: the untrimmed one, then the trimmed one when a
    // target is not a test (crate::engine's bep_plan).
    let trimmed = parts.bep.targets.iter().any(|target| target.test.is_none());
    let source = [&full.bep, &sub.bep]
        .into_iter()
        .find(|plan| plan.configurations.len() == if trimmed { 2 } else { 1 })
        .or_else(|| {
            [&full.bep, &sub.bep]
                .into_iter()
                .find(|plan| plan.configurations.len() == 2 && !trimmed)
        })
        .ok_or("the build events' configurations are unknown")?;
    let mut configurations = source.configurations.clone();
    configurations.truncate(if trimmed { 2 } else { 1 });
    if [&full.bep, &sub.bep].into_iter().any(|plan| {
        plan.configurations
            .iter()
            .zip(&configurations)
            .any(|(a, b)| a != b)
    }) {
        return Err("the build events' configurations differ".to_owned());
    }
    parts.bep.configurations = configurations;
    Ok(parts)
}

/// The ids of the actions producing what a run plan reads (its executable,
/// runfiles tree and entries, coverage inputs, completion).
fn run_plan_producers(plan: &crate::request::PlannedRun) -> impl Iterator<Item = &str> {
    let produced = std::iter::once(&plan.executable)
        .chain(plan.tree.iter().map(|(tree, _)| tree))
        .chain(
            plan.entries
                .iter()
                .flatten()
                .filter_map(|(_, produced)| produced.as_ref()),
        )
        .chain(
            plan.coverage
                .iter()
                .flat_map(|coverage| coverage.inputs.iter().chain(&coverage.baselines))
                .map(|(_, produced)| produced),
        );
    produced
        .filter_map(|produced| produced.producer.as_ref().map(|(id, _)| id.as_str()))
        .chain(plan.completion.iter().map(String::as_str))
}

/// Encoding helper for the splice's comparisons.
mod erased {
    pub trait Encode {
        fn bytes(&self) -> Vec<u8>;
    }
    impl<T: serde::Serialize> Encode for T {
        fn bytes(&self) -> Vec<u8> {
            postcard::to_stdvec(self).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> IndexPackage {
        IndexPackage {
            repository: String::new(),
            package: name.to_owned(),
            global: false,
            why: None,
            pattern: true,
            printed: false,
            loaded: true,
            fingerprint: Some(Fingerprint {
                digest: [0; 32],
                summary: [0; 32],
                shared_kinds: false,
                package: [0; 32],
                targets: Vec::new(),
            }),
            loads: Vec::new(),
        }
    }

    fn observation(
        path: &str,
        op: crate::observed_fs::Op,
    ) -> (PathBuf, crate::observed_fs::Observation) {
        (
            PathBuf::from(path),
            crate::observed_fs::Observation {
                op,
                outcome: crate::observed_fs::Outcome::Error(2),
            },
        )
    }

    #[test]
    fn changes_are_attributed_to_the_innermost_loaded_package_or_refused() {
        use crate::observed_fs::{Op, Projection};
        let mut index = Index {
            main_root: PathBuf::from("/ws"),
            packages: vec![package(""), package("a"), package("a/b")],
            repository_inputs: vec![(PathBufWire(PathBuf::from("/ws/watched")), true, None)],
            ..Index::default()
        };
        let recording = crate::observed_fs::Recording {
            observations: vec![
                observation("/ws/a/b/BUILD.bazel", Op::Read),
                observation(
                    "/ws/a/srcs",
                    Op::ReadDir(Projection::Glob {
                        pattern: "srcs/*".to_owned(),
                        relative: "srcs".to_owned(),
                    }),
                ),
                observation("/ws/a/b/c/x.go", Op::Lstat),
                observation("/ws/a/defs.bzl", Op::Read),
                observation("/ws/a/c/BUILD", Op::Stat),
                observation("/ws/a", Op::ReadDir(Projection::Directories)),
                observation("/ws/watched/x/BUILD", Op::Read),
                observation("/cache/x", Op::Read),
                observation("/elsewhere/x", Op::Stat),
            ],
            ..Default::default()
        };
        let cache = std::path::Path::new("/cache");
        let attribution = classify(&index, &recording, &[0, 1, 2], cache).unwrap();
        assert_eq!(attribution.loaded.into_iter().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(attribution.content.into_iter().collect::<Vec<_>>(), [2]);
        for (at, reason) in [
            (3, ".bzl"),
            (4, "package boundary"),
            (5, "directory listing"),
            (6, "recorded input of a module extension"),
            (7, "inside the cache"),
            (8, "outside the main repository"),
        ] {
            let error = classify(&index, &recording, &[at], cache).unwrap_err();
            assert!(error.contains(reason), "{at}: {error}");
        }
        index.packages[2].global = true;
        assert!(
            classify(&index, &recording, &[0], cache)
                .unwrap_err()
                .contains("shared computation")
        );
        index.packages[2].global = false;
        index.packages[2].printed = true;
        assert!(
            classify(&index, &recording, &[0], cache)
                .unwrap_err()
                .contains("printed")
        );
    }

    #[test]
    fn bzl_edits_reach_the_packages_and_rules_loading_them_or_are_refused() {
        use crate::observed_fs::Op;
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        let mut index = Index {
            main_root: PathBuf::from("/ws"),
            packages: vec![package("a"), package("b"), package("c")],
            keys: vec![
                IndexKey {
                    label: label("//a:x"),
                    config: Some([1; 32]),
                    apply: true,
                },
                IndexKey {
                    label: label("//c:y"),
                    config: Some([1; 32]),
                    apply: true,
                },
            ],
            labels: vec![label("//a:x"), label("//c:y")],
            roots: vec![(0, 0), (1, 1)],
            ..Index::default()
        };
        let graph = ModuleGraph {
            loads: [
                (label("//a:defs.bzl"), vec![label("//lib:util.bzl")]),
                (label("//lib:util.bzl"), Vec::new()),
                (label("//c:other.bzl"), vec![label("@@ext+//:x.bzl")]),
                (label("@@ext+//:x.bzl"), Vec::new()),
            ]
            .into_iter()
            .collect(),
            definitions: Default::default(),
            code: Default::default(),
        };
        set_modules(
            &mut index,
            &graph,
            &[
                (0, vec![label("//a:defs.bzl")]),
                (2, vec![label("//c:other.bzl")]),
            ],
            &[
                (0, vec![label("//a:defs.bzl")]),
                (1, vec![label("//c:other.bzl")]),
            ],
        );
        // The external module loads no main-repository module: not kept.
        assert!(
            index
                .modules
                .iter()
                .all(|module| module.label.repository.is_empty())
        );
        let recording = crate::observed_fs::Recording {
            observations: vec![
                observation("/ws/lib/util.bzl", Op::Read),
                observation("/ws/c/other.bzl", Op::Read),
            ],
            ..Default::default()
        };
        let cache = std::path::Path::new("/cache");
        let attribution = classify(&index, &recording, &[0], cache).unwrap();
        assert_eq!(attribution.bzl.iter().copied().collect::<Vec<_>>(), [0]);
        assert!(attribution.loaded.is_empty() && attribution.content.is_empty());
        let dirty = dirty_labels(
            &index,
            &Default::default(),
            &Default::default(),
            &attribution.modules,
            None,
        )
        .unwrap();
        assert_eq!(dirty.dirty.into_iter().collect::<Vec<_>>(), [0]);
        assert_eq!(dirty.labels, [label("//a:x")]);
        // A definition load entered it: refused.
        let mut defined = graph.clone();
        defined.definitions.insert(label("//lib:util.bzl"));
        set_modules(
            &mut index,
            &defined,
            &[(0, vec![label("//a:defs.bzl")])],
            &[(0, vec![label("//a:defs.bzl")])],
        );
        assert!(
            classify(&index, &recording, &[0], cache)
                .unwrap_err()
                .contains("repository rule")
        );
        // An external package loading an edited module: refused.
        index.packages.push(IndexPackage {
            repository: "ext+".to_owned(),
            ..package("")
        });
        set_modules(
            &mut index,
            &graph,
            &[(3, vec![label("//c:other.bzl")])],
            &[],
        );
        assert!(
            classify(&index, &recording, &[1], cache)
                .unwrap_err()
                .contains("@@ext+//")
        );
    }

    #[test]
    fn code_digests_ignore_comments_only() {
        let base = code_digest(
            b"x = 1  # one\ny = '#a' # b\nz = \"\"\"q\n# not a comment \\\" \"\"\" # c\n",
        );
        assert_eq!(
            base,
            code_digest(
                b"x = 1  # uno\ny = '#a' #\nz = \"\"\"q\n# not a comment \\\" \"\"\" # comment\n"
            )
        );
        for changed in [
            &b"x = 1  # one\ny = '#b' # b\nz = \"\"\"q\n# not a comment \\\" \"\"\" # c\n"[..],
            b"x = 1  # one\ny = '#a' # b\nz = \"\"\"q\n# not a kommentar \\\" \"\"\" # c\n",
            b"x = 1   # one\ny = '#a' # b\nz = \"\"\"q\n# not a comment \\\" \"\"\" # c\n",
            b"x = 1  # one\n\ny = '#a' # b\nz = \"\"\"q\n# not a comment \\\" \"\"\" # c\n",
            b"x = 2  # one\ny = '#a' # b\nz = \"\"\"q\n# not a comment \\\" \"\"\" # c\n",
        ] {
            assert_ne!(
                base,
                code_digest(changed),
                "{}",
                String::from_utf8_lossy(changed)
            );
        }
        // An appended comment line.
        // Comment lines appended after the code; one inserted above it moves
        // the code.
        assert_eq!(code_digest(b"a = 1\n"), code_digest(b"a = 1\n# edit\n\n"));
        assert_ne!(code_digest(b"a = 1\n"), code_digest(b"# edit\na = 1\n"));
    }

    #[test]
    fn actions_belong_to_the_longest_owner() {
        let owners: std::collections::HashMap<String, bool> = [
            ("//a:b".to_owned(), true),
            ("//a:b#c".to_owned(), false),
            ("//a:b[0011aabb]".to_owned(), false),
        ]
        .into_iter()
        .collect();
        assert_eq!(owner_of_action("//a:b", &owners), Some(("//a:b", true)));
        assert_eq!(owner_of_action("//a:b#0", &owners), Some(("//a:b", true)));
        assert_eq!(
            owner_of_action("//a:b#c#1", &owners),
            Some(("//a:b#c", false))
        );
        assert_eq!(
            owner_of_action("//a:b%//x:a.bzl%asp#0", &owners),
            Some(("//a:b", true))
        );
        assert_eq!(
            owner_of_action("//a:b[0011aabb]#2", &owners),
            Some(("//a:b[0011aabb]", false))
        );
        assert_eq!(owner_of_action("status#stable", &owners), None);
    }

    #[test]
    fn the_partial_window_must_print_nothing_new() {
        let full = b"DEBUG: x\nINFO: Analyzed 9 targets (3 packages loaded, 9 targets configured).\nINFO: Found 9 targets...\n";
        let partial = b"DEBUG: x\nINFO: Analyzed 2 targets (1 packages loaded, 2 targets configured).\nINFO: Found 2 targets...\n";
        assert!(window_covers(full, partial).is_ok());
        assert!(window_covers(full, b"WARNING: new\n").is_err());
    }

    #[test]
    fn the_index_round_trips() {
        let index = Index {
            main_root: PathBuf::from("/ws"),
            repository_inputs: vec![(
                PathBufWire(PathBuf::from("/ws/a")),
                true,
                Some("r".to_owned()),
            )],
            repository_roots: vec![("r".to_owned(), PathBufWire(PathBuf::from("/c/r")))],
            root_entries: vec![0, 1],
            demand: vec![(0, "//a:b#0".to_owned())],
            patterns: Some(vec![(0, 0)]),
            suite_packages: vec![0],
            top: [7; 32],
            labels: vec![Label::parse_canonical("//a:b").unwrap()],
            keys: vec![IndexKey {
                label: Label::parse_canonical("//a:b").unwrap(),
                config: Some([1; 32]),
                apply: true,
            }],
            edges: vec![(0, 0)],
            accesses: vec![(0, 0)],
            label_accesses: vec![(0, 0, "b".to_owned())],
            packages: vec![IndexPackage {
                repository: String::new(),
                package: "a".to_owned(),
                global: false,
                why: None,
                pattern: true,
                printed: false,
                loaded: true,
                fingerprint: Some(Fingerprint {
                    digest: [2; 32],
                    summary: [3; 32],
                    shared_kinds: false,
                    package: [5; 32],
                    targets: vec![("b".to_owned(), [6; 16])],
                }),
                loads: vec![0],
            }],
            roots: vec![(0, 0)],
            shared: Vec::new(),
            printed: Vec::new(),
            aspects: vec![(0, "//a:b%x".to_owned())],
            modules: vec![IndexModule {
                label: Label::parse_canonical("//a:defs.bzl").unwrap(),
                loads: Vec::new(),
                definition: false,
                code: Some([4; 32]),
            }],
            rule_modules: vec![(0, 0)],
            planned: vec![(0, 0, 0)],
        };
        let stored = encode_index(&index).unwrap();
        assert_eq!(decode_index(stored).unwrap(), index);
    }
}
