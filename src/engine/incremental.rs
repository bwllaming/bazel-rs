//! Dependency tracking for incremental analysis (docs/INCREMENTAL_ANALYSIS.md,
//! tier A): while a full analysis that the analysis snapshot saves runs, the
//! engine records which configured targets reached which (every
//! `analyze_key` call, memo hits included, before the memo check), which
//! packages each configured target's own analysis looked up, and which
//! packages and targets were reached from computations whose result is
//! shared by many targets (toolchain resolution, platforms, transitions,
//! package groups, ...) or from outside any target (option parsing, the
//! top-level setup). The snapshot keeps the result
//! ([`crate::snapshot::incremental::Index`]); a later run whose recorded
//! inputs changed only inside some packages re-analyzes the requested
//! targets that reach those packages and splices the result into the saved
//! plan (crate::snapshot::incremental).
//!
//! The rules that keep this sound (a changed package that is not
//! attributable makes the next run fall back to the full analysis):
//!
//! * A target looked up (`Engine::target`) while a configured target's
//!   analysis or planning is the innermost frame counts as that target's
//!   access; while any shared computation runs ([`SharedScope`], a
//!   thread-local depth) the package is *global*; with no frame at all it
//!   is global too, except for the requested labels themselves (their own
//!   package: they are dirty whenever it is) and the target pattern phase
//!   ([`PatternScope`]: the package is *pattern-loaded*, its pattern summary
//!   must not change).
//! * A configured key reached inside a shared computation, or outside every
//!   frame (but the requested roots), is *shared*: if an edit dirties it, or
//!   anything it reaches, the run falls back.
//! * Edges are recorded before the memo check, so every consumer of a
//!   configured target is an edge, not only the first.

use super::*;
use crate::snapshot::incremental::{Fingerprint, Index, IndexKey, IndexPackage};
use std::cell::Cell;

thread_local! {
    static SHARED_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// The outermost shared computation (diagnostics).
    static SHARED_NAME: Cell<&'static str> = const { Cell::new("") };
    static PATTERN_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// While alive, the engine's lookups belong to a computation whose result
/// many targets share (a memo not keyed by the configured target).
pub(super) struct SharedScope(());

impl SharedScope {
    pub(super) fn enter(name: &'static str) -> Self {
        SHARED_DEPTH.with(|depth| {
            if depth.get() == 0 {
                SHARED_NAME.with(|shared| shared.set(name));
            }
            depth.set(depth.get() + 1)
        });
        Self(())
    }
}

impl Drop for SharedScope {
    fn drop(&mut self) {
        SHARED_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// While alive, the engine's lookups belong to the target pattern phase.
pub(super) struct PatternScope(());

impl PatternScope {
    pub(super) fn enter() -> Self {
        PATTERN_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for PatternScope {
    fn drop(&mut self) {
        PATTERN_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn shared() -> bool {
    SHARED_DEPTH.with(Cell::get) > 0
}

fn shared_depth() -> u32 {
    SHARED_DEPTH.with(Cell::get)
}

fn pattern() -> bool {
    PATTERN_DEPTH.with(Cell::get) > 0
}

type KeyIdentity = (Label, Option<ConfigId>, bool);

/// What one analysis records (see the module docs).
#[derive(Default)]
pub(super) struct Tracker {
    keys: Vec<KeyIdentity>,
    key_ids: HashMap<KeyIdentity, u32>,
    /// The configured targets being analyzed or planned, innermost last,
    /// with the shared-computation depth their frame began at: a lookup
    /// in a frame is the frame's own unless a shared computation began
    /// inside it (a key analyzed by a shared computation, or a pending
    /// target planned while one runs, owns its lookups).
    frames: Vec<(u32, u32)>,
    edges: Vec<(u32, u32)>,
    edge_set: HashSet<(u32, u32)>,
    accesses: HashSet<(u32, u32)>,
    packages: Vec<PackageRecord>,
    package_ids: HashMap<PackageId, u32>,
    shared_keys: BTreeSet<u32>,
    /// Keys whose analysis printed (`print()` in an implementation, an
    /// aspect or a transition while the key's frame was innermost).
    printed_keys: BTreeSet<u32>,
    /// (key, aspect application) requests.
    aspects: BTreeSet<(u32, String)>,
    roots: Vec<(u32, u32)>,
    requested: HashSet<Label>,
    /// Inside `analyze_requested`'s analysis of the requested label at
    /// this position (its first key is a root, not a shared one).
    root_analysis: Option<u32>,
    /// The root analysis reached its first key.
    root_reached: bool,
}

#[derive(Default)]
struct PackageRecord {
    id: Option<PackageId>,
    global: bool,
    /// Why it is global (the first reason; diagnostics).
    why: Option<String>,
    pattern: bool,
    printed: bool,
}

impl Tracker {
    fn key(&mut self, identity: KeyIdentity) -> u32 {
        if let Some(id) = self.key_ids.get(&identity) {
            return *id;
        }
        let id = self.keys.len() as u32;
        self.keys.push(identity.clone());
        self.key_ids.insert(identity, id);
        id
    }

    fn package(&mut self, package: &PackageId) -> u32 {
        if let Some(id) = self.package_ids.get(package) {
            return *id;
        }
        let id = self.packages.len() as u32;
        self.packages.push(PackageRecord {
            id: Some(package.clone()),
            ..PackageRecord::default()
        });
        self.package_ids.insert(package.clone(), id);
        id
    }

    fn make_global(&mut self, package: u32, why: impl FnOnce() -> String) {
        let record = &mut self.packages[package as usize];
        if !record.global {
            record.global = true;
            record.why = Some(why());
        }
    }

    fn edge(&mut self, parent: u32, child: u32) {
        if self.edge_set.insert((parent, child)) {
            self.edges.push((parent, child));
        }
    }

    /// A configured key (or, `config` `None`, a source file) is reached:
    /// returns its id. Its frame is pushed by the caller when it is
    /// analyzed.
    fn reach(&mut self, identity: KeyIdentity) -> u32 {
        let package = identity.0.package_id();
        let id = self.key(identity);
        let inner = self.frames.last().copied();
        if let Some((parent, depth)) = inner
            && depth == shared_depth()
        {
            if parent != id {
                self.edge(parent, id);
            }
        } else if shared() {
            self.shared_keys.insert(id);
            let package = self.package(&package);
            let label = &self.keys[id as usize].0;
            let why = format!("{} reached by {}", label, SHARED_NAME.with(Cell::get));
            self.make_global(package, || why);
        } else if let Some(position) = self.root_analysis
            && !self.root_reached
        {
            self.root_reached = true;
            self.roots.push((position, id));
        } else {
            self.shared_keys.insert(id);
            let package = self.package(&package);
            let why = format!("{} reached outside every target", self.keys[id as usize].0);
            self.make_global(package, || why);
        }
        id
    }

    fn access(&mut self, label: &Label) {
        let package = label.package_id();
        let id = self.package(&package);
        let inner = self.frames.last().copied();
        if pattern() {
            self.packages[id as usize].pattern = true;
        } else if let Some((frame, depth)) = inner
            && depth == shared_depth()
        {
            self.accesses.insert((frame, id));
        } else if shared() {
            self.make_global(id, || {
                format!("{label} looked up by {}", SHARED_NAME.with(Cell::get))
            });
        } else if !self.requested.contains(label) {
            self.make_global(id, || format!("{label} looked up outside every target"));
        }
    }
}

impl Engine {
    /// Record dependencies for incremental analysis (docs/INCREMENTAL_ANALYSIS.md)
    /// from now on: the CLI turns it on for the builds whose analysis the
    /// snapshot saves.
    pub fn set_incremental_tracking(&mut self, on: bool) {
        self.incremental = on.then(Box::default);
    }

    /// Whether this engine records dependencies.
    pub(super) fn tracking(&self) -> bool {
        self.incremental.is_some()
    }

    /// `analyze_key` reached `key` (before its memo check).
    pub(super) fn track_key(&mut self, key: &ConfiguredTargetKey) -> Option<u32> {
        let tracker = self.incremental.as_mut()?;
        Some(tracker.reach((
            key.label.clone(),
            Some(key.config),
            key.apply_rule_transition,
        )))
    }

    /// `analyze_key` reached the source file `label`.
    pub(super) fn track_source(&mut self, label: &Label) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.reach((label.clone(), None, true));
        }
    }

    /// The analysis or planning of a configured target begins (its frame).
    pub(super) fn track_enter(&mut self, id: Option<u32>) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id) {
            tracker.frames.push((id, shared_depth()));
        }
    }

    /// The frame depth (restored by [`Self::track_restore`]).
    pub(super) fn track_depth(&self) -> usize {
        self.incremental
            .as_ref()
            .map_or(0, |tracker| tracker.frames.len())
    }

    pub(super) fn track_restore(&mut self, depth: usize) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.frames.truncate(depth);
        }
    }

    /// Phase 2 plans `key`: its frame (no edge: phase 1 recorded them).
    pub(super) fn track_planning(&mut self, key: &ConfiguredTargetKey) {
        if let Some(tracker) = self.incremental.as_mut() {
            let id = tracker.key((
                key.label.clone(),
                Some(key.config),
                key.apply_rule_transition,
            ));
            tracker.frames.push((id, shared_depth()));
        }
    }

    /// `Engine::target` looks up `label`.
    pub(super) fn track_access(&mut self, label: &Label) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.access(label);
        }
    }

    /// Print a line an analysis printed (`print()`), noting the configured
    /// target whose frame is innermost.
    pub(super) fn note_analysis_print(&mut self, line: &str) {
        if let Some(tracker) = self.incremental.as_mut()
            && let Some((frame, _)) = tracker.frames.last().copied()
        {
            tracker.printed_keys.insert(frame);
        }
        crate::ui::note(line);
    }

    /// The innermost key's planning uses the aspect application `key`.
    pub(super) fn track_aspect_application(&mut self, key: &crate::aspects::AspectApplicationKey) {
        if let Some(tracker) = self.incremental.as_mut() {
            let frame = tracker.frames.last().map_or(u32::MAX, |(frame, _)| *frame);
            let base = &key.base;
            let identity = format!(
                "{}|{}|{}|{}|{}",
                base.label,
                base.config,
                base.apply_rule_transition,
                key.aspect,
                key.bases.join(",")
            );
            tracker.aspects.insert((frame, identity));
        }
    }

    /// A package was committed; `printed`: its BUILD file printed.
    pub(super) fn track_package_printed(&mut self, package: &PackageId, printed: bool) {
        if let Some(tracker) = self.incremental.as_mut() {
            let id = tracker.package(package);
            let record = &mut tracker.packages[id as usize];
            record.printed |= printed;
            record.pattern |= pattern();
        }
    }

    /// The requested labels of this build (accesses to them outside any
    /// frame are theirs).
    pub(super) fn track_requested(&mut self, labels: &[Label]) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.requested = labels.iter().cloned().collect();
        }
    }

    /// `analyze_requested` analyzes the requested label at `position`
    /// (`None`: done).
    pub(super) fn track_root_analysis(&mut self, position: Option<usize>) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.root_analysis = position.map(|position| position as u32);
            tracker.root_reached = false;
        }
    }

    /// The dependency index of the last planned build (`labels`: its
    /// requested labels), with the fingerprints of its main-repository
    /// packages; `None` without tracking.
    pub(super) fn incremental_index(&mut self, labels: &[Label]) -> Option<Index> {
        let tracker = self.incremental.as_mut()?;
        // Every loaded package has a record (fingerprints of the main
        // repository's).
        let loaded: Vec<PackageId> = self.packages.keys().cloned().collect();
        for package in &loaded {
            tracker.package(package);
        }
        let main: Vec<(u32, &Package)> = tracker
            .packages
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let id = record.id.as_ref()?;
                if !id.repository.is_empty() {
                    return None;
                }
                self.packages.get(id).map(|package| (index as u32, package))
            })
            .collect();
        let fingerprints = fingerprints(&main);
        let mut packages: Vec<IndexPackage> = tracker
            .packages
            .iter()
            .map(|record| {
                let id = record.id.clone().unwrap_or_else(|| PackageId {
                    repository: String::new(),
                    package: String::new(),
                });
                IndexPackage {
                    repository: id.repository,
                    package: id.package,
                    global: record.global,
                    why: record.why.clone(),
                    pattern: record.pattern,
                    printed: record.printed,
                    loaded: false,
                    fingerprint: None,
                }
            })
            .collect();
        for (index, fingerprint) in fingerprints {
            packages[index as usize].fingerprint = Some(fingerprint);
        }
        for package in &loaded {
            if let Some(id) = tracker.package_ids.get(package) {
                packages[*id as usize].loaded = true;
            }
        }
        let mut accesses: Vec<(u32, u32)> = tracker.accesses.iter().copied().collect();
        accesses.sort_unstable();
        let main_root = self
            .repositories
            .root("")
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Some(Index {
            main_root,
            repository_inputs: crate::observed_fs::repository_inputs()
                .into_iter()
                .map(|(path, tree)| (crate::snapshot::incremental::PathBufWire(path), tree))
                .collect(),
            top: self.configurations.top().bytes(),
            labels: labels.to_vec(),
            keys: tracker
                .keys
                .iter()
                .map(|(label, config, apply)| IndexKey {
                    label: label.clone(),
                    config: config.map(|config| config.bytes()),
                    apply: *apply,
                })
                .collect(),
            edges: tracker.edges.clone(),
            accesses,
            packages,
            roots: tracker.roots.clone(),
            shared: tracker.shared_keys.iter().copied().collect(),
            printed: tracker.printed_keys.iter().copied().collect(),
            aspects: tracker.aspects.iter().cloned().collect(),
        })
    }

    /// Load `package` (if it is not yet) and return its fingerprint, and
    /// whether its BUILD file printed when it was loaded by this engine.
    pub fn package_fingerprint(
        &mut self,
        package: &PackageId,
    ) -> Result<(Fingerprint, bool), String> {
        {
            let _pattern = PatternScope::enter();
            self.load_package(package)?;
        }
        let printed = self.incremental.as_ref().is_some_and(|tracker| {
            tracker
                .package_ids
                .get(package)
                .is_some_and(|id| tracker.packages[*id as usize].printed)
        });
        Ok((fingerprint(&self.packages[package]), printed))
    }
}

/// The fingerprints of `packages`, on a few threads (a large workspace
/// has thousands of main-repository packages).
fn fingerprints(packages: &[(u32, &Package)]) -> Vec<(u32, Fingerprint)> {
    if packages.len() < 64 {
        return packages
            .iter()
            .map(|(index, package)| (*index, fingerprint(package)))
            .collect();
    }
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 8);
    let chunk = packages.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = packages
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|(index, package)| (*index, fingerprint(package)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// Rule kinds whose targets feed computations many targets share
/// (visibility, toolchain and platform resolution, configurations): a
/// package holding one is never re-analyzed incrementally.
fn shared_kind(rule: &Rule) -> bool {
    if matches!(rule.kind, RuleKind::PackageGroup(_)) {
        return true;
    }
    if rule
        .custom
        .as_ref()
        .is_some_and(|custom| custom.build_setting.is_some())
    {
        return true;
    }
    matches!(
        rule.rule_class_name(),
        "toolchain"
            | "toolchain_type"
            | "platform"
            | "constraint_setting"
            | "constraint_value"
            | "label_flag"
            | "label_setting"
            | "config_feature_flag"
            | "environment"
            | "environment_group"
    )
}

/// The fingerprint of a loaded package: a digest of everything its load
/// produced that analysis can read (rules with their attributes, exports,
/// package arguments, loads), and the summaries the target pattern phase
/// depends on.
pub(crate) fn fingerprint(package: &Package) -> Fingerprint {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = write!(
        text,
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        package.exports,
        package.default_visibility,
        package.macro_names,
        package.args,
        package.build_file,
        package.direct_loads,
    );
    let mut rules_summary = String::new();
    let mut shared_kinds = false;
    for (label, rule) in package.rules.iter() {
        let custom = rule.custom.as_ref().map(|custom| {
            (
                &custom.attrs,
                &custom.build_setting,
                &custom.declaration,
                custom.transitions.keys().collect::<Vec<_>>(),
            )
        });
        let _ = write!(
            text,
            "\n{label}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            rule.kind,
            rule.srcs,
            rule.outs,
            rule.cmd,
            rule.visibility,
            custom,
            rule.native,
            rule.unanalyzed,
            rule.package_metadata,
            rule.location,
            rule.explicit_visibility,
            rule.macro_owner,
            rule.call_stack,
            rule.generator,
        );
        shared_kinds |= shared_kind(rule);
        let test_attributes = rule.native.as_ref().map(|native| {
            [
                "size",
                "timeout",
                "flaky",
                "shard_count",
                "local",
                "tests",
                "deprecation",
                "testonly",
            ]
            .iter()
            .filter_map(|name| native.attrs.get(*name).map(|value| (*name, value)))
            .collect::<Vec<_>>()
        });
        let custom_test_attributes = rule.custom.as_ref().map(|custom| {
            (
                &custom.declaration.builtin,
                custom.declaration.rule_class.as_str(),
            )
        });
        let _ = write!(
            rules_summary,
            "\n{label}|{}|{:?}|{}|{:?}|{:?}|{:?}",
            rule.rule_class_name(),
            rule.tags(),
            rule.is_manual(),
            rule.outs,
            test_attributes,
            custom_test_attributes,
        );
    }
    Fingerprint {
        digest: *blake3::hash(text.as_bytes()).as_bytes(),
        summary: *blake3::hash(rules_summary.as_bytes()).as_bytes(),
        shared_kinds,
    }
}
