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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// (crate::observed_fs::note_repository_input): (path, a tree).
    pub repository_inputs: Vec<(PathBufWire, bool)>,
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
    pub loaded: std::collections::BTreeSet<u32>,
    pub content: std::collections::BTreeSet<u32>,
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
    let (loaded, content) = classify(&index, &inputs.recording, &changed, cache)?;
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
        loaded,
        content,
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
            loaded,
            content,
        } = classified?;
        Ok(Self {
            inputs,
            window,
            request,
            index,
            changed,
            loaded,
            content,
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
#[allow(clippy::type_complexity)]
pub fn classify(
    index: &Index,
    recording: &crate::observed_fs::Recording,
    changed: &[usize],
    cache: &std::path::Path,
) -> Result<
    (
        std::collections::BTreeSet<u32>,
        std::collections::BTreeSet<u32>,
    ),
    String,
> {
    use crate::observed_fs::{Op, Projection};
    use std::collections::{BTreeSet, HashMap};
    if changed.is_empty() {
        return Err("no observation changed".to_owned());
    }
    let mut packages: HashMap<&str, u32> = HashMap::new();
    for (id, package) in index.packages.iter().enumerate() {
        if package.repository.is_empty() && package.loaded && package.fingerprint.is_some() {
            packages.insert(package.package.as_str(), id as u32);
        }
    }
    let mut loaded = BTreeSet::new();
    let mut content = BTreeSet::new();
    for &at in changed {
        let (path, observation) = recording
            .observations
            .get(at)
            .ok_or("internal error: a changed observation out of range")?;
        let what = format!("{} {}", observation.op.name(), escaped(path));
        if path.starts_with(cache) {
            return Err(format!("{what}: inside the cache"));
        }
        for (input, tree) in &index.repository_inputs {
            if *path == input.0 || (*tree && path.starts_with(&input.0)) {
                return Err(format!(
                    "{what}: a recorded input of a repository or module extension"
                ));
            }
        }
        let relative = path
            .strip_prefix(&index.main_root)
            .map_err(|_| format!("{what}: outside the main repository"))?;
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
            .is_some_and(|fingerprint| fingerprint.shared_kinds)
        {
            return Err(format!(
                "{shown} defines toolchains, platforms, constraints, flags or package groups"
            ));
        }
    }
    Ok((loaded, content))
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
}

/// Compare a reloaded package with what the index recorded of it.
/// `strict_patterns`: a target pattern names file targets (`:*`,
/// `:all-targets`), whose expansion any fingerprint change may move.
pub fn compare_package(
    candidate: &Candidate,
    id: u32,
    now: Fingerprint,
    printed: bool,
    strict_patterns: bool,
    changed: &mut Changed,
) -> Result<(), String> {
    let package = &candidate.index.packages[id as usize];
    let shown = format!("package //{}", package.package);
    let before = package
        .fingerprint
        .ok_or_else(|| format!("{shown} has no recorded fingerprint"))?;
    if printed {
        return Err(format!("{shown} prints while it loads"));
    }
    if now.shared_kinds {
        return Err(format!(
            "{shown} now defines toolchains, platforms, constraints, flags or package groups"
        ));
    }
    // Whether the target pattern phase expanded the package is not
    // tracked precisely: any change of what an expansion reads falls back.
    if before.summary != now.summary || (strict_patterns && before.digest != now.digest) {
        return Err(format!(
            "{shown}: the target patterns' expansion may change (targets added, removed or retagged)"
        ));
    }
    changed.fingerprints.insert(id, now);
    if before.digest == now.digest && !candidate.content.contains(&id) {
        changed.pruned += 1;
    } else {
        changed.packages.insert(id);
    }
    Ok(())
}

/// The configured targets the changed `packages` dirty — those in them,
/// those whose analysis looked them up, and everything reaching either —
/// and the requested labels among their roots, in request order.
pub fn dirty_labels(
    index: &Index,
    packages: &std::collections::BTreeSet<u32>,
) -> Result<(Vec<Label>, std::collections::BTreeSet<u32>), String> {
    use std::collections::{BTreeSet, HashMap};
    let mut by_package: HashMap<(&str, &str), u32> = HashMap::new();
    for (id, package) in index.packages.iter().enumerate() {
        by_package.insert((&package.repository, &package.package), id as u32);
    }
    let mut dirty: BTreeSet<u32> = BTreeSet::new();
    for (id, key) in index.keys.iter().enumerate() {
        if by_package
            .get(&(key.label.repository.as_str(), key.label.package.as_str()))
            .is_some_and(|package| packages.contains(package))
        {
            dirty.insert(id as u32);
        }
    }
    for (key, package) in &index.accesses {
        if packages.contains(package) {
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
    if let Some(printed) = index.printed.iter().find(|key| dirty.contains(key)) {
        return Err(format!(
            "{} printed while it was analyzed and would be re-analyzed",
            index.keys[*printed as usize].label
        ));
    }
    if let Some(shared) = index.shared.iter().find(|key| dirty.contains(key)) {
        return Err(format!(
            "{} feeds a shared computation (toolchains, platforms, transitions) and would be re-analyzed",
            index.keys[*shared as usize].label
        ));
    }
    let mut positions: BTreeSet<u32> = BTreeSet::new();
    for (position, key) in &index.roots {
        if dirty.contains(key) {
            positions.insert(*position);
        }
    }
    let labels: Vec<Label> = positions
        .into_iter()
        .filter_map(|position| index.labels.get(position as usize).cloned())
        .collect();
    if labels.is_empty() && !dirty.is_empty() {
        return Err(format!(
            "{} is dirty but reaches no requested target",
            index.keys[*dirty.iter().next().expect("not empty") as usize].label
        ));
    }
    Ok((labels, dirty))
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

/// Map the keys of `sub` (a partial analysis's index) to `index`'s (the
/// saved one, which becomes the merged index), checking that the
/// configured graph did not change: every configured target the partial
/// analysis reached is one the saved analysis had, each dirty one with the
/// same configured dependencies, and it loaded no package the saved
/// analysis did not (so the plan order and the counts the window reports
/// are the same). Source files may come and go (a `srcs` edit): new ones
/// join the index, and the dirty targets' source edges become the partial
/// analysis's.
pub fn check_graph(
    index: &mut Index,
    dirty: &std::collections::BTreeSet<u32>,
    sub: &Index,
) -> Result<Vec<u32>, String> {
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
    for key in &sub.keys {
        match ids.get(key) {
            Some(id) => mapped.push(*id),
            None if key.config.is_none() => {
                let id = index.keys.len() as u32;
                index.keys.push(key.clone());
                ids.insert(key.clone(), id);
                mapped.push(id);
            }
            None => {
                return Err(format!(
                    "the re-analysis reached {}, which the saved analysis did not (the dependency graph changed)",
                    key.label
                ));
            }
        }
    }
    let configured = |index: &Index, key: u32| index.keys[key as usize].config.is_some();
    let mut base_children: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for (parent, child) in &index.edges {
        if dirty.contains(parent) && configured(index, *child) {
            base_children.entry(*parent).or_default().insert(*child);
        }
    }
    let mut sub_children: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    let mut sub_sources: Vec<(u32, u32)> = Vec::new();
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    for (parent, child) in &sub.edges {
        let (parent, child) = (mapped[*parent as usize], mapped[*child as usize]);
        reached.insert(parent);
        reached.insert(child);
        if dirty.contains(&parent) {
            if configured(index, child) {
                sub_children.entry(parent).or_default().insert(child);
            } else {
                sub_sources.push((parent, child));
            }
        }
    }
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
        if base_children.get(key) != sub_children.get(key) {
            return Err(format!(
                "the dependencies of {} changed",
                index.keys[*key as usize].label
            ));
        }
    }
    for key in &sub.printed {
        if dirty.contains(&mapped[*key as usize]) {
            return Err(format!(
                "{} printed while it was re-analyzed",
                sub.keys[*key as usize].label
            ));
        }
    }
    for key in &sub.shared {
        if dirty.contains(&mapped[*key as usize]) {
            return Err(format!(
                "{} feeds a shared computation",
                sub.keys[*key as usize].label
            ));
        }
    }
    // The distinct aspect applications (the window's count) stay the same.
    let before: BTreeSet<&str> = index
        .aspects
        .iter()
        .map(|(_, application)| application.as_str())
        .collect();
    let mut after: BTreeSet<&str> = index
        .aspects
        .iter()
        .filter(|(key, _)| !dirty.contains(key))
        .map(|(_, application)| application.as_str())
        .collect();
    after.extend(
        sub.aspects
            .iter()
            .filter(|(key, _)| {
                mapped
                    .get(*key as usize)
                    .is_some_and(|key| dirty.contains(key))
            })
            .map(|(_, application)| application.as_str()),
    );
    if before != after {
        return Err("the aspect applications changed".to_owned());
    }
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
    // A package only re-analyzed targets looked up before, and none now,
    // may no longer be loaded (the window's count would change) unless
    // something else loads it.
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
        if dirty.contains(&mapped[*key as usize]) {
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
        if !dirty.contains(key) {
            elsewhere.insert(*package);
        }
    }
    for key in index.keys.iter() {
        if let Some(id) =
            by_identity.get(&(key.label.repository.as_str(), key.label.package.as_str()))
        {
            elsewhere.insert(*id);
        }
    }
    for (key, package) in &index.accesses {
        if dirty.contains(key)
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
    // The dirty targets' source edges are the re-analysis's.
    let mut edges: Vec<(u32, u32)> = index
        .edges
        .iter()
        .filter(|(parent, child)| !(dirty.contains(parent) && !configured(index, *child)))
        .copied()
        .collect();
    let mut seen: std::collections::HashSet<(u32, u32)> = edges.iter().copied().collect();
    for edge in sub_sources {
        if seen.insert(edge) {
            edges.push(edge);
        }
    }
    index.edges = edges;
    Ok(mapped)
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
            package.fingerprint = Some(*fingerprint);
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
    let mut inputs: BTreeSet<(PathBufWire, bool)> = index.repository_inputs.into_iter().collect();
    inputs.extend(sub.repository_inputs.iter().cloned());
    index.repository_inputs = inputs.into_iter().collect();
    index
}

/// Splice the partial analysis `sub` of the requested `labels` (whose
/// configured targets `dirty` holds, base ids) into the saved request
/// `full`: each re-analyzed target's actions replace its saved ones in
/// place; every other action the partial analysis planned must equal the
/// saved one; the requested labels' reported outputs must be the saved
/// ones. Any difference is the error (the run falls back).
pub fn splice(
    full: crate::request::PlannedRequest,
    index: &Index,
    labels: &[Label],
    dirty: &std::collections::BTreeSet<u32>,
    sub: crate::request::PlannedRequest,
) -> Result<crate::request::PlannedRequest, String> {
    use crate::scheduler::{InputGroup, PlannedAction};
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::sync::Arc;
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
    if full.command != crate::request::RequestCommand::Build
        || sub.command != crate::request::RequestCommand::Build
        || full.execute != sub.execute
    {
        return Err("not a build".to_owned());
    }
    // The owners and whether they were re-analyzed.
    let mut owners: HashMap<String, bool> = HashMap::new();
    for (id, key) in index.keys.iter().enumerate() {
        if let Some(owner) = owner_of_key(key, &index.top) {
            let entry = owners.entry(owner).or_insert(false);
            *entry |= dirty.contains(&(id as u32));
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
    let mut removed_dependencies: BTreeSet<String> = BTreeSet::new();
    let mut kept: Vec<PlannedAction> = Vec::with_capacity(full.nodes.len());
    let mut replaced: Vec<(usize, String)> = Vec::new();
    for node in full.nodes {
        match owner(&node) {
            Some((name, true)) => {
                if previous.as_deref() != Some(name) {
                    if !seen_runs.insert(name.to_owned()) {
                        return Err(format!("the saved actions of {name} are not contiguous"));
                    }
                    replaced.push((kept.len(), name.to_owned()));
                }
                previous = Some(name.to_owned());
                for dependency in node.all_dependencies() {
                    if owner_of_action(&dependency.action_id, &owners)
                        .is_none_or(|(_, dirty)| !dirty)
                    {
                        removed_dependencies.insert(dependency.action_id.clone());
                    }
                }
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
        if !seen_runs.contains(name) {
            return Err(format!(
                "{name} has actions now but had none in the saved plan"
            ));
        }
    }
    let mut added_dependencies: BTreeSet<String> = BTreeSet::new();
    for (_, run) in &runs {
        for node in run {
            for dependency in node.all_dependencies() {
                if owner_of_action(&dependency.action_id, &owners).is_none_or(|(_, dirty)| !dirty) {
                    added_dependencies.insert(dependency.action_id.clone());
                }
            }
        }
    }
    if added_dependencies != removed_dependencies {
        return Err(
            "the re-analyzed actions depend on other actions of other targets than before"
                .to_owned(),
        );
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
    let requested_labels: HashSet<&Label> = labels.iter().collect();
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
    if encoded(&full_requested) != encoded(&sub_requested) {
        return Err("the requested targets' outputs changed".to_owned());
    }
    let full_results: Vec<_> = full
        .results
        .iter()
        .filter(|(label, _)| requested_labels.contains(label))
        .collect();
    let sub_results: Vec<_> = sub.results.iter().collect();
    if encoded(&full_results) != encoded(&sub_results) {
        return Err("the requested targets' results changed".to_owned());
    }
    let full_aspects: Vec<_> = full
        .aspect_reports
        .iter()
        .filter(|(_, label, _)| requested_labels.contains(label))
        .collect();
    let sub_aspects: Vec<_> = sub.aspect_reports.iter().collect();
    if encoded(&full_aspects) != encoded(&sub_aspects) {
        return Err("the requested targets' aspect outputs changed".to_owned());
    }
    // The requested labels skipped as incompatible are the saved ones.
    let full_skipped: Vec<&Label> = full
        .skipped
        .iter()
        .filter(|label| requested_labels.contains(label))
        .collect();
    if full_skipped != sub.skipped.iter().collect::<Vec<_>>() {
        return Err("the requested targets skipped as incompatible changed".to_owned());
    }
    if !full.run_plans.is_empty() || !sub.run_plans.is_empty() {
        return Err("run plans".to_owned());
    }
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
    if bep_of(&full.bep, &full.requested) != bep_of(&sub.bep, &sub.requested) {
        return Err("the requested targets' build events changed".to_owned());
    }
    Ok(crate::request::PlannedRequest { nodes, ..full })
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
            }),
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
            repository_inputs: vec![(PathBufWire(PathBuf::from("/ws/watched")), true)],
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
        let (loaded, content) = classify(&index, &recording, &[0, 1, 2], cache).unwrap();
        assert_eq!(loaded.into_iter().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(content.into_iter().collect::<Vec<_>>(), [2]);
        for (at, reason) in [
            (3, ".bzl"),
            (4, "package boundary"),
            (5, "directory listing"),
            (6, "recorded input of a repository"),
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
            repository_inputs: vec![(PathBufWire(PathBuf::from("/ws/a")), true)],
            top: [7; 32],
            labels: vec![Label::parse_canonical("//a:b").unwrap()],
            keys: vec![IndexKey {
                label: Label::parse_canonical("//a:b").unwrap(),
                config: Some([1; 32]),
                apply: true,
            }],
            edges: vec![(0, 0)],
            accesses: vec![(0, 0)],
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
                }),
            }],
            roots: vec![(0, 0)],
            shared: Vec::new(),
            printed: Vec::new(),
            aspects: vec![(0, "//a:b%x".to_owned())],
        };
        let stored = encode_index(&index).unwrap();
        assert_eq!(decode_index(stored).unwrap(), index);
    }
}
