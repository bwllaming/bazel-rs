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
    static SUITE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// While alive, the packages the engine looks up are read by a test
/// suite's expansion in the target pattern phase.
pub(super) struct SuiteScope(());

impl SuiteScope {
    pub(super) fn enter() -> Self {
        SUITE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for SuiteScope {
    fn drop(&mut self) {
        SUITE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
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
    /// (key, package, target name): the main-repository targets each
    /// key's analysis looked up.
    label_accesses: HashSet<(u32, u32, String)>,
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
    /// (key, plan length when its analysis began, its plan position).
    planned: HashMap<u32, (u32, u32)>,
    /// Inside `analyze_requested`'s analysis of the requested label at
    /// this position (its first key is a root, not a shared one).
    root_analysis: Option<u32>,
    /// The root analysis reached its first key.
    root_reached: bool,
    /// The plan length when each requested label's analysis began (by
    /// position), and at the end of phase 1 (last).
    root_entries: Vec<u32>,
    /// (requested label position, `u32::MAX` for the workspace status;
    /// action id): the producers of the requested labels' hidden top-level
    /// artifacts (the plan's reachability roots besides the reported ones).
    demand: Vec<(u32, String)>,
    /// The target pattern phase's pattern being expanded (its position).
    pattern_position: Option<u32>,
    /// (pattern position, package) the wildcard patterns expanded.
    covered: BTreeSet<(u32, u32)>,
    /// The pattern phase was the plain `build` one (no tag filters, no
    /// tests): `Some(simple)` once it ran.
    patterns_simple: Option<bool>,
    /// The packages test suite expansions read.
    suite_packages: BTreeSet<u32>,
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
        if SUITE_DEPTH.with(Cell::get) > 0 {
            self.suite_packages.insert(id);
        }
        if pattern() {
            self.packages[id as usize].pattern = true;
        } else if let Some((frame, depth)) = inner
            && depth == shared_depth()
        {
            self.accesses.insert((frame, id));
            if label.repository.is_empty() {
                self.label_accesses.insert((frame, id, label.name.clone()));
            }
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

    /// The analysis of `id` begins with `plan_len` targets planned.
    pub(super) fn track_entry(&mut self, id: Option<u32>, plan_len: usize) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id) {
            tracker
                .planned
                .entry(id)
                .or_insert((plan_len as u32, u32::MAX));
        }
    }

    /// `id` is planned at `position` (phase 1's post-order).
    pub(super) fn track_position(&mut self, id: Option<u32>, position: usize) {
        if let (Some(tracker), Some(id)) = (self.incremental.as_mut(), id)
            && let Some(planned) = tracker.planned.get_mut(&id)
            && planned.1 == u32::MAX
        {
            planned.1 = position as u32;
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

    /// The analysis of requested label `position` begins with `plan_len`
    /// targets planned (`position` = the labels' count: phase 1 ended).
    pub(super) fn track_root_entry(&mut self, position: usize, plan_len: usize) {
        if let Some(tracker) = self.incremental.as_mut() {
            if position == 0 {
                tracker.root_entries.clear();
            }
            if tracker.root_entries.len() == position {
                tracker.root_entries.push(plan_len as u32);
            }
        }
    }

    /// The hidden top-level artifacts' producers of the request (the
    /// requested label at each position; `None`: the workspace status).
    pub(super) fn track_demand(&mut self, demand: Vec<(Option<usize>, String)>) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.demand = demand
                .into_iter()
                .map(|(position, id)| (position.map_or(u32::MAX, |position| position as u32), id))
                .collect();
            tracker.demand.sort();
            tracker.demand.dedup();
        }
    }

    /// The target pattern phase expands the pattern at `position` (`None`:
    /// done).
    pub(super) fn track_pattern(&mut self, position: Option<usize>) {
        if let Some(tracker) = self.incremental.as_mut() {
            // Only the target pattern phase's (crate::engine's `select*`
            // serve other commands too).
            tracker.pattern_position = position
                .filter(|_| pattern())
                .map(|position| position as u32);
        }
    }

    /// The pattern being expanded covers `packages`.
    pub(super) fn track_pattern_packages(&mut self, packages: &[PackageId]) {
        if let Some(tracker) = self.incremental.as_mut()
            && let Some(position) = tracker.pattern_position
        {
            for package in packages {
                let id = tracker.package(package);
                tracker.covered.insert((position, id));
            }
        }
    }

    /// A test suite's expansion read `label`'s package.
    pub(super) fn track_suite_package(&mut self, label: &Label) {
        if let Some(tracker) = self.incremental.as_mut() {
            let id = tracker.package(&label.package_id());
            tracker.suite_packages.insert(id);
        }
    }

    /// The target pattern phase ended; `simple`: a plain `build` one.
    pub(super) fn track_patterns_simple(&mut self, simple: bool) {
        if let Some(tracker) = self.incremental.as_mut() {
            tracker.patterns_simple = Some(simple && tracker.patterns_simple != Some(false));
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
                    loads: Vec::new(),
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
        let mut label_accesses: Vec<(u32, u32, String)> =
            tracker.label_accesses.iter().cloned().collect();
        label_accesses.sort_unstable();
        let main_root = self
            .repositories
            .root("")
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let package_loads: Vec<(u32, Vec<Label>)> = tracker
            .packages
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let package = self.packages.get(record.id.as_ref()?)?;
                Some((index as u32, package.direct_loads.clone()))
            })
            .collect();
        let keys: Vec<(u32, Label)> = tracker
            .keys
            .iter()
            .enumerate()
            .filter(|(_, (_, config, _))| config.is_some())
            .map(|(index, (label, _, _))| (index as u32, label.clone()))
            .collect();
        let graph = module_graph(&self.packages);
        let rules = rule_modules(&self.packages, &keys);
        let repository_inputs: Vec<_> = crate::observed_fs::repository_inputs()
            .into_iter()
            .map(|(path, tree, owner)| {
                (crate::snapshot::incremental::PathBufWire(path), tree, owner)
            })
            .collect();
        let mut index = Index {
            main_root,
            repository_inputs: repository_inputs.clone(),
            repository_roots: {
                let owners: BTreeSet<&str> = repository_inputs
                    .iter()
                    .filter_map(|(_, _, owner)| owner.as_deref())
                    .collect();
                self.repositories
                    .materialized_roots()
                    .into_iter()
                    .filter(|(name, _)| owners.contains(name.as_str()))
                    .map(|(name, root)| (name, crate::snapshot::incremental::PathBufWire(root)))
                    .collect()
            },
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
            label_accesses,
            packages,
            roots: tracker.roots.clone(),
            shared: tracker.shared_keys.iter().copied().collect(),
            printed: tracker.printed_keys.iter().copied().collect(),
            aspects: tracker.aspects.iter().cloned().collect(),
            modules: Vec::new(),
            rule_modules: Vec::new(),
            planned: {
                let mut planned: Vec<(u32, u32, u32)> = tracker
                    .planned
                    .iter()
                    .filter(|(_, (_, position))| *position != u32::MAX)
                    .map(|(key, (entry, position))| (*key, *entry, *position))
                    .collect();
                planned.sort_unstable();
                planned
            },
            root_entries: if tracker.root_entries.len() == labels.len() + 1 {
                tracker.root_entries.clone()
            } else {
                Vec::new()
            },
            demand: tracker.demand.clone(),
            patterns: (tracker.patterns_simple == Some(true))
                .then(|| tracker.covered.iter().copied().collect()),
            suite_packages: tracker.suite_packages.iter().copied().collect(),
        };
        crate::snapshot::incremental::set_modules(&mut index, &graph, &package_loads, &rules);
        Some(index)
    }

    /// The `.bzl` load graph of every loaded package, and the modules
    /// definition loads entered (crate::snapshot::incremental::ModuleGraph).
    pub fn module_graph(&self) -> crate::snapshot::incremental::ModuleGraph {
        module_graph(&self.packages)
    }

    /// The modules defining the rules of `keys` (see [`rule_modules`]).
    pub fn rule_modules(&self, keys: &[(u32, Label)]) -> Vec<(u32, Vec<Label>)> {
        rule_modules(&self.packages, keys)
    }

    /// The direct loads of every loaded package.
    pub fn package_loads(&self) -> Vec<(PackageId, Vec<Label>)> {
        self.packages
            .iter()
            .map(|(id, package)| (id.clone(), package.direct_loads.clone()))
            .collect()
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

    /// What the plain `build` target pattern phase (crate::engine's
    /// `select_filtered` with FILTER_MANUAL, no tag filters, tests or test
    /// suites) selects of the loaded main-repository package `package` now:
    /// `covered` names the patterns (positions) whose wildcards expanded it
    /// (the saved analysis's: the directory listings did not change). The
    /// error says why it cannot be computed this way (a test suite, an
    /// ambiguous `:all`, a label that does not exist).
    pub fn package_selection(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        covered: &BTreeSet<u32>,
        package: &PackageId,
        expand_test_suites: bool,
    ) -> Result<BTreeSet<Label>, String> {
        use crate::target_pattern::Kind;
        // The lookups are the target pattern phase's.
        let _pattern = PatternScope::enter();
        let mut selected: BTreeSet<Label> = BTreeSet::new();
        for (position, text) in patterns.iter().enumerate() {
            let pattern = crate::target_pattern::parse(text, relative_directory)?;
            let filter = !pattern.negative;
            let covers = covered.contains(&(position as u32));
            let targets: BTreeSet<Label> = match &pattern.kind {
                Kind::TargetsBelowDirectory { rules_only } => {
                    if covers {
                        self.package_targets(package, *rules_only, filter)?
                            .into_iter()
                            .collect()
                    } else {
                        BTreeSet::new()
                    }
                }
                Kind::TargetsInPackage {
                    suffix,
                    absolute,
                    rules_only,
                } => {
                    if !covers {
                        BTreeSet::new()
                    } else if *absolute
                        && self
                            .existing_target_kind(package, suffix)
                            .ok()
                            .flatten()
                            .is_some()
                    {
                        return Err(format!("the pattern '{text}' names an existing target"));
                    } else {
                        self.package_targets(package, *rules_only, filter)?
                            .into_iter()
                            .collect()
                    }
                }
                Kind::SingleTarget { .. } | Kind::PathAsTarget { .. } => {
                    let label = match &pattern.kind {
                        Kind::SingleTarget { target } => {
                            self.repositories
                                .parse(&pattern.label_text(target), "", "")?
                        }
                        Kind::PathAsTarget { path } => self
                            .path_as_target(path)
                            .map_err(PatternError::into_message)?,
                        _ => unreachable!("a single target"),
                    };
                    if label.package_id() == *package {
                        self.target(&label)?;
                        BTreeSet::from([label])
                    } else {
                        BTreeSet::new()
                    }
                }
            };
            if pattern.negative {
                selected.retain(|label| !targets.contains(label));
            } else {
                selected.extend(targets);
            }
        }
        if expand_test_suites
            && let Some(suite) = selected.iter().find(|label| self.loaded_test_suite(label))
        {
            return Err(format!("{suite} is a test suite"));
        }
        Ok(selected)
    }

    /// The root of repository `name`, materialized now (its warm check, or
    /// its execution: incremental analysis re-runs a repository whose
    /// recorded inputs changed).
    pub fn repository_root_now(&self, name: &str) -> Result<PathBuf, String> {
        self.repositories.root(name).map(Path::to_path_buf)
    }

    /// [`Self::package_fingerprint`] of every package of `packages` (in
    /// order), their loads on the loading-phase-threads pool (commits in
    /// order, like the target pattern phase's).
    pub fn package_fingerprints(
        &mut self,
        packages: &[PackageId],
    ) -> Result<Vec<(Fingerprint, bool)>, (PackageId, String)> {
        {
            let _pattern = PatternScope::enter();
            if let Err(error) = self.load_packages_parallel(packages, |_, _| Ok(())) {
                // The first one that does not load (named by its own load).
                for package in packages {
                    if let Err(error) = self.load_package(package) {
                        return Err((package.clone(), error));
                    }
                }
                return Err((
                    packages.first().cloned().unwrap_or(PackageId {
                        repository: String::new(),
                        package: String::new(),
                    }),
                    error,
                ));
            }
        }
        packages
            .iter()
            .map(|package| {
                self.package_fingerprint(package)
                    .map_err(|error| (package.clone(), error))
            })
            .collect()
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
    let package_digest = *blake3::hash(text.as_bytes()).as_bytes();
    let mut rules_summary = String::new();
    let mut shared_kinds = false;
    let mut targets: Vec<(String, [u8; 16])> = Vec::new();
    for (label, rule) in package.rules.iter() {
        let start = text.len();
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
        // The rule's own digest, for its name and each of its outputs.
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&blake3::hash(&text.as_bytes()[start..]).as_bytes()[..16]);
        targets.push((rule.label.name.clone(), digest));
        for out in &rule.outs {
            targets.push((out.clone(), digest));
        }
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
        // The built-in attributes but the generator's location (it moves
        // with every line inserted above the macro call; the digest has it).
        let custom_test_attributes = rule.custom.as_ref().map(|custom| {
            let builtin = &custom.declaration.builtin;
            (
                (
                    &builtin.kind,
                    &builtin.target,
                    &builtin.visibility,
                    builtin.testonly,
                    &builtin.deprecation,
                    builtin
                        .generator
                        .as_ref()
                        .map(|generator| (&generator.name, &generator.function)),
                ),
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
    targets.sort();
    Fingerprint {
        digest: *blake3::hash(text.as_bytes()).as_bytes(),
        summary: *blake3::hash(rules_summary.as_bytes()).as_bytes(),
        shared_kinds,
        package: package_digest,
        targets,
    }
}

/// The `.bzl` load graph of every loaded package, and the modules
/// definition loads entered (crate::snapshot::incremental::ModuleGraph).
fn module_graph(
    packages: &BTreeMap<PackageId, Package>,
) -> crate::snapshot::incremental::ModuleGraph {
    let mut graph = crate::snapshot::incremental::ModuleGraph::default();
    let mut seen: HashSet<*const crate::loading::LoadGraph> = HashSet::new();
    for package in packages.values() {
        if !seen.insert(Arc::as_ptr(&package.load_graph)) {
            continue;
        }
        for (module, loads) in package.load_graph.iter() {
            graph
                .loads
                .entry(module.clone())
                .or_insert_with(|| loads.clone());
        }
    }
    graph.definitions = crate::loading::definition_modules().into_iter().collect();
    graph.code = crate::snapshot::incremental::module_code();
    graph
}

/// The modules defining the rules of `keys` (loaded packages only): the
/// `.bzl` that called `rule()` and the one being initialized then.
fn rule_modules(
    packages: &BTreeMap<PackageId, Package>,
    keys: &[(u32, Label)],
) -> Vec<(u32, Vec<Label>)> {
    keys.iter()
        .filter_map(|(key, label)| {
            let rule = packages.get(&label.package_id())?.rules.get(label)?;
            let custom = rule.custom.as_ref()?;
            let modules = [
                &custom.declaration.definition_label,
                &custom.declaration.definition_environment_label,
            ]
            .into_iter()
            .map(|text| {
                // Unparsable: no module edit is attributable (the index
                // says so, crate::snapshot::incremental::UNATTRIBUTED).
                Label::parse_canonical(text)
                    .unwrap_or_else(|_| crate::snapshot::incremental::unattributed_module())
            })
            .collect();
            Some((*key, modules))
        })
        .collect()
}
