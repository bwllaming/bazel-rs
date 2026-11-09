//! Phase-1 package prefetch (W6; docs/PARALLEL_ANALYSIS.md "Phase-1
//! prefetch"): while the coordinator analyzes, a small pool of background
//! workers loads the packages analysis is about to demand — the registered
//! toolchain packages as soon as their patterns are known, and the
//! packages of the targets every analyzed or prefetched rule names, several
//! hops ahead.
//!
//! Workers run exactly like the P2 pattern-loading workers: no-demand +
//! reuse-only repository mode (a repository that needs fetching, a
//! module extension or any other coordinator-only work aborts the unit
//! with the [`crate::repository_demand::NEEDS_REPOSITORY`] sentinel), their
//! own Starlark heaps, buffered print events. Nothing a worker produces is
//! reported: a finished [`LoadUnit`] waits in the prefetcher until the
//! coordinator demands the package (`Engine::load_package`), at exactly the
//! point where the sequential engine would have loaded it, and commits it
//! there — prints, errors and commit order are the sequential ones, and a
//! sentinel unit is re-run inline at that point as in P2. A unit nobody
//! demands is dropped unreported.
//!
//! The expansion is target-directed, like a loading-phase transitive
//! closure: a queued package remembers which of its targets were named
//! (or all of them, for a `:all` toolchain pattern), and a finished unit
//! queues the packages of those rules' labels (skipping test rules when
//! all are wanted). Units aborted by the sentinel remember the demand they
//! were blocked on; once the coordinator has satisfied it (prepared the
//! extension group, materialized the root) the package is queued again
//! ([`Prefetcher::retry`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::label::{Label, PackageId};
use crate::package::{LoadUnit, Package, Rule};
use crate::repository::RepositorySet;

/// How many label hops beyond what the coordinator analyzes (depth 1) the
/// workers follow.
const MAX_DEPTH: u32 = 12;

/// At most this many background workers (each owns a Starlark heap while
/// it loads).
const MAX_WORKERS: usize = 16;

/// The worker stack size (as P2's pattern-loading workers).
const WORKER_STACK: usize = 16 << 20;

pub(crate) struct Prefetcher {
    shared: Arc<Shared>,
}

struct Shared {
    repositories: Arc<RepositorySet>,
    state: Mutex<State>,
    /// Signalled when the queue gains work, and on shutdown.
    work: Condvar,
    /// Signalled whenever a unit finishes.
    done: Condvar,
    jitter_millis: u64,
    /// The worker bound; workers start as work is queued (a build that
    /// queues nothing starts none).
    max_workers: usize,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// The targets of a package whose rules' labels the expansion follows.
#[derive(Default)]
struct Wanted {
    /// Every non-test rule (a `:all` toolchain pattern).
    all: bool,
    names: BTreeSet<String>,
    /// The names the build certainly analyzes (see [`Reach`]): their
    /// package's repository is fetched ahead of its sequential demand
    /// (crate::repository_speculation).
    certain: BTreeSet<String>,
}

impl Wanted {
    fn of(name: Option<String>, certain: bool) -> Self {
        let mut wanted = Self::default();
        wanted.add(name, certain);
        wanted
    }

    /// Add `name` (`None`: all, never certain); whether it was new or
    /// newly certain.
    fn add(&mut self, name: Option<String>, certain: bool) -> bool {
        match name {
            None if self.all => false,
            None => {
                self.all = true;
                true
            }
            Some(name) => {
                let newly_certain = certain && !self.certain.contains(&name);
                if newly_certain {
                    self.certain.insert(name.clone());
                }
                let new = !self.all && self.names.insert(name);
                new || newly_certain
            }
        }
    }

    /// Whether some wanted target is certainly analyzed.
    fn any_certain(&self) -> bool {
        !self.certain.is_empty()
    }
}

#[derive(Default)]
struct State {
    /// The queue: lowest depth first, submission order within a depth.
    queue: BTreeMap<(u32, u64), PackageId>,
    entries: HashMap<PackageId, Entry>,
    next: u64,
    shutdown: bool,
    /// Units being loaded.
    running: usize,
    counts: Counts,
    /// Whether each select() condition matched wherever the coordinator
    /// analyzed it (`None`: not in every configuration alike, or in error).
    conditions: Conditions,
    /// The certainly analyzed rules (package, name, depth of their labels)
    /// with a select() whose branch waits for the coordinator to analyze
    /// this condition: expanded again once it did.
    undecided: HashMap<Label, Vec<(PackageId, String, u32)>>,
}

/// What the coordinator's analysis decided of select() conditions
/// ([`State::conditions`]), and whether every select() branch is demanded
/// anyway.
#[derive(Default)]
struct Conditions {
    decided: HashMap<Label, Option<bool>>,
    /// `query` visits every select() branch (an unconfigured graph):
    /// every label a certainly visited rule names is certainly demanded,
    /// so its repository is fetched ahead too
    /// ([`Prefetcher::visit_all_branches`]).
    all_branches: bool,
}

impl std::ops::Deref for Conditions {
    type Target = HashMap<Label, Option<bool>>;
    fn deref(&self) -> &Self::Target {
        &self.decided
    }
}

impl std::ops::DerefMut for Conditions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.decided
    }
}

/// Select() conditions an expansion could not decide yet, with the rule
/// (package, name) and the depth of its labels ([`State::undecided`]).
type Undecided = Vec<(Label, PackageId, String, u32)>;

/// `PERF-PREFETCH` telemetry (BAZEL_RS_TIMING=1).
#[derive(Default)]
struct Counts {
    /// Units the workers finished, and how many of them the sentinel
    /// aborted.
    loaded: u64,
    aborted: u64,
    /// Units the coordinator committed (sentinel units included), units it
    /// had to wait for, and aborted units queued again.
    taken: u64,
    waited: u64,
    /// The coordinator's total wait for running units.
    wait_micros: u128,
    retried: u64,
    /// Demanded packages the workers had not started (loaded inline).
    missed: u64,
}

enum Entry {
    Queued {
        key: (u32, u64),
        wanted: Wanted,
    },
    Running {
        wanted: Wanted,
    },
    Done {
        depth: u32,
        unit: Box<LoadUnit>,
        /// The sentinel error of a unit aborted by the no-demand guard.
        blocked: Option<String>,
        /// What the expansion followed (a successful unit), or follows
        /// once the unit is retried (an aborted one).
        wanted: Wanted,
    },
    /// Claimed by the coordinator for its commit (`take`): the targets
    /// wanted of it are followed at the commit unless a worker's unit
    /// already did.
    Taken {
        wanted: Wanted,
        expanded: bool,
    },
    /// Committed by the coordinator (or abandoned by a panicking worker):
    /// never queued again. A committed package keeps (shares) its rules so
    /// targets reached later are still followed.
    Settled {
        rules: Option<Rules>,
        followed: Wanted,
    },
}

/// A loaded package's rules, shared with the engine's copy.
pub(crate) type Rules = BTreeMap<Label, Arc<Rule>>;

impl Entry {
    fn settled() -> Self {
        Entry::Settled {
            rules: None,
            followed: Wanted::default(),
        }
    }
}

/// One target the expansion reached: its package, its name (`None`: every
/// rule), its depth, and whether the build certainly analyzes it — the
/// coordinator analyzes it, toolchain resolution selected it, or a target
/// certainly analyzed names it outside select() or in a select() branch the
/// coordinator's analysis of the conditions decides (a guess for another
/// configuration costs a wasted fetch, never output). Only certain targets'
/// repositories are fetched ahead of demand: a package load guessed wrong
/// is cheap, a download is not (select() branches for other platforms,
/// registered toolchains of other platforms).
type Reach = (PackageId, Option<String>, u32, bool);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    /// Add the reached targets (and, transitively, what already finished
    /// units of their packages name); whether the queue changed. The
    /// sentinels of finished units blocked on a repository that a target
    /// newly certain needs are added to `speculate`.
    fn reach(&mut self, mut work: Vec<Reach>, speculate: &mut Vec<String>) -> bool {
        let mut changed = false;
        let mut undecided = Undecided::new();
        while let Some((id, name, depth, certain)) = work.pop() {
            match self.entries.get_mut(&id) {
                None => {
                    let key = (depth, self.next);
                    self.next += 1;
                    self.queue.insert(key, id.clone());
                    self.entries.insert(
                        id,
                        Entry::Queued {
                            key,
                            wanted: Wanted::of(name, certain),
                        },
                    );
                    changed = true;
                }
                Some(Entry::Queued { key, wanted }) => {
                    wanted.add(name, certain);
                    if key.0 > depth {
                        let old = *key;
                        let new = (depth, self.next);
                        self.next += 1;
                        *key = new;
                        if let Some(id) = self.queue.remove(&old) {
                            self.queue.insert(new, id);
                        }
                        changed = true;
                    }
                }
                Some(Entry::Running { wanted }) => {
                    wanted.add(name, certain);
                }
                Some(Entry::Done {
                    unit,
                    blocked,
                    wanted,
                    ..
                }) => {
                    let was_certain = wanted.any_certain();
                    // An aborted unit follows its wanted targets once a
                    // retry finishes; a finished one follows a new target
                    // now.
                    if wanted.add(name.clone(), certain)
                        && blocked.is_none()
                        && depth < MAX_DEPTH
                        && let Ok(package) = &unit.result
                    {
                        work.extend(expansion(
                            &package.rules,
                            &Wanted::of(name, certain),
                            depth + 1,
                            &self.conditions,
                            &mut undecided,
                        ));
                    }
                    if !was_certain
                        && wanted.any_certain()
                        && let Some(blocked) = blocked
                    {
                        speculate.push(blocked.clone());
                    }
                }
                Some(Entry::Taken { wanted, .. }) => {
                    wanted.add(name, certain);
                }
                Some(Entry::Settled {
                    rules: Some(rules),
                    followed,
                }) => {
                    if followed.add(name.clone(), certain) && depth < MAX_DEPTH {
                        work.extend(expansion(
                            rules,
                            &Wanted::of(name, certain),
                            depth + 1,
                            &self.conditions,
                            &mut undecided,
                        ));
                    }
                }
                Some(Entry::Settled { rules: None, .. }) => {}
            }
        }
        self.note_undecided(undecided);
        changed
    }

    /// Remember [`Undecided`] conditions ([`Self::undecided`]).
    fn note_undecided(&mut self, undecided: Undecided) {
        for (condition, id, name, depth) in undecided {
            let waiting = self.undecided.entry(condition).or_default();
            if !waiting
                .iter()
                .any(|(other, rule, _)| *other == id && *rule == name)
            {
                waiting.push((id, name, depth));
            }
        }
    }

    /// The coordinator decided `condition`: the certainly analyzed rules
    /// waiting for it are expanded again ([`Self::undecided`]).
    fn decided(&mut self, condition: &Label, speculate: &mut Vec<String>) -> bool {
        let Some(waiting) = self.undecided.remove(condition) else {
            return false;
        };
        let mut work = Vec::new();
        let mut undecided = Undecided::new();
        for (id, name, depth) in waiting {
            let rules = match self.entries.get(&id) {
                Some(Entry::Done { unit, .. }) => unit.result.as_ref().ok().map(|p| &p.rules),
                Some(Entry::Settled {
                    rules: Some(rules), ..
                }) => Some(rules),
                _ => None,
            };
            if let Some(rules) = rules {
                work.extend(
                    expansion(
                        rules,
                        &Wanted::of(Some(name), true),
                        depth,
                        &self.conditions,
                        &mut undecided,
                    )
                    .into_iter()
                    .filter(|reach| reach.3),
                );
            }
        }
        self.note_undecided(undecided);
        work.reverse();
        self.reach(work, speculate)
    }
}

/// The targets the `wanted` rules of a package (its `rules`) name, at
/// `depth`; a target certainly analyzed certainly analyzes the labels
/// [`certain_labels`] keeps (`conditions`: what the coordinator's analysis
/// decided of select() conditions).
fn expansion(
    package: &Rules,
    wanted: &Wanted,
    depth: u32,
    conditions: &Conditions,
    undecided: &mut Undecided,
) -> Vec<Reach> {
    let mut reached = Vec::new();
    let mut follow = |rule: &Rule, certain: bool| {
        let certain = if certain {
            let mut waiting = Vec::new();
            let labels = certain_labels(rule, conditions, &mut waiting);
            undecided.extend(waiting.into_iter().map(|condition| {
                (
                    condition,
                    rule.label.package_id(),
                    rule.label.name.clone(),
                    depth,
                )
            }));
            labels
        } else {
            HashSet::new()
        };
        for label in rule_dependency_labels(rule) {
            let id = label.package_id();
            let certain = certain.contains(&label);
            reached.push((id, Some(label.name), depth, certain));
        }
    };
    if wanted.all {
        for rule in package.values().filter(|rule| !is_test(rule)) {
            let certain = wanted.certain.contains(&rule.label.name);
            follow(rule, certain);
        }
        return reached;
    }
    let Some(any) = package.values().next() else {
        return reached;
    };
    for name in &wanted.names {
        let certain = wanted.certain.contains(name);
        let label = Label {
            repository: any.label.repository.clone(),
            package: any.label.package.clone(),
            name: name.clone(),
        };
        match package.get(&label) {
            Some(rule) => follow(rule, certain),
            // An output file: its generating rule.
            None => {
                if let Some(rule) = package
                    .values()
                    .find(|rule| rule.outs.iter().any(|out| out == name))
                {
                    follow(rule, certain);
                }
            }
        }
    }
    reached
}

/// The labels of [`rule_dependency_labels`] that analyzing `rule` certainly
/// analyzes: every label outside select() branches, and the labels of the
/// branch a select() takes when the coordinator's analysis decided all of
/// its conditions the same way everywhere (`conditions`): exactly one
/// matches, or none does and the default branch is taken. A select() with
/// an undecided condition, or several matching ones (specialization), keeps
/// none of its branches' labels — unless another attribute names them; the
/// conditions the coordinator has not analyzed yet are added to `undecided`.
fn certain_labels(
    rule: &Rule,
    conditions: &Conditions,
    undecided: &mut Vec<Label>,
) -> HashSet<Label> {
    use crate::custom_rule::ConfigurablePart;
    if conditions.all_branches {
        return rule_dependency_labels(rule).into_iter().collect();
    }
    let mut branches: HashSet<Label> = HashSet::new();
    let mut taken: Vec<Label> = Vec::new();
    let configurable =
        rule.native
            .iter()
            .map(|native| &native.configurable)
            .chain(rule.custom.iter().flat_map(|custom| {
                [
                    &custom.declaration.configurable,
                    &custom.declaration.builtin.target.configurable,
                ]
            }));
    for attributes in configurable {
        for parts in attributes.values() {
            for part in parts {
                let ConfigurablePart::Select { branches: arms, .. } = part else {
                    continue;
                };
                let mut labels = Vec::new();
                for (_, value) in arms {
                    if let Some(value) = value {
                        crate::native_rules::collect_labels(value, &mut labels);
                    }
                }
                branches.extend(labels);
                if let Some(value) = taken_branch(arms, conditions, undecided) {
                    crate::native_rules::collect_labels(value, &mut taken);
                }
            }
        }
    }
    let taken: HashSet<Label> = taken.into_iter().collect();
    rule_dependency_labels(rule)
        .into_iter()
        .filter(|label| !branches.contains(label) || taken.contains(label))
        .collect()
}

/// The value of the branch a select() takes, if `conditions` decide it
/// ([`certain_labels`]).
fn taken_branch<'a>(
    arms: &'a [(String, Option<crate::custom_rule::AttrValue>)],
    conditions: &HashMap<Label, Option<bool>>,
    undecided: &mut Vec<Label>,
) -> Option<&'a crate::custom_rule::AttrValue> {
    let mut matched = None;
    let mut default = None;
    let mut decided = true;
    let mut ambiguous = false;
    for (key, value) in arms {
        if key == crate::native_rules::DEFAULT_CONDITION {
            default = Some(value);
            continue;
        }
        let Ok(label) = Label::parse_canonical(key) else {
            return None;
        };
        match conditions.get(&label) {
            None => {
                undecided.push(label);
                decided = false;
            }
            Some(None) => ambiguous = true,
            Some(Some(true)) if matched.is_some() => ambiguous = true,
            Some(Some(true)) => matched = Some(value),
            Some(Some(false)) => {}
        }
    }
    if !decided || ambiguous {
        return None;
    }
    matched.or(default)?.as_ref()
}

impl Prefetcher {
    /// Start `workers` background loaders (at most [`MAX_WORKERS`]) over
    /// `repositories`. `known` packages (loaded already) are never queued.
    pub(crate) fn start(
        repositories: Arc<RepositorySet>,
        workers: usize,
        known: impl IntoIterator<Item = (PackageId, Rules)>,
        jitter_millis: u64,
    ) -> Option<Self> {
        let mut state = State::default();
        for (id, rules) in known {
            state.entries.insert(
                id,
                Entry::Settled {
                    rules: Some(rules),
                    followed: Wanted::default(),
                },
            );
        }
        if workers <= 1 {
            return None;
        }
        let shared = Arc::new(Shared {
            repositories,
            state: Mutex::new(state),
            work: Condvar::new(),
            done: Condvar::new(),
            jitter_millis,
            max_workers: workers.min(MAX_WORKERS),
            workers: Mutex::new(Vec::new()),
        });
        // Parallel fetching (crate::repository_speculation): the
        // repositories units are blocked on execute ahead of their
        // sequential demand; a published root lets the blocked units load.
        let listener = Arc::downgrade(&shared);
        shared.repositories.start_speculation(
            workers,
            Arc::new(move || {
                if let Some(shared) = listener.upgrade() {
                    retry(&shared);
                }
            }),
            jitter_millis,
        );
        Some(Self { shared })
    }

    fn reach(&self, work: Vec<Reach>) {
        let mut speculate = Vec::new();
        let changed = {
            let mut state = lock(&self.shared.state);
            if state.shutdown {
                return;
            }
            state.reach(work, &mut speculate)
        };
        if changed {
            grow(&self.shared);
            self.shared.work.notify_all();
        }
        for blocked in speculate {
            self.shared.repositories.speculate(&blocked);
        }
    }

    /// Queue targets (a package and a target name, `None` for every
    /// non-test rule) at `depth`, in order; `certain`: the build certainly
    /// analyzes them ([`Reach`]).
    pub(crate) fn submit_targets(
        &self,
        targets: impl IntoIterator<Item = (PackageId, Option<String>)>,
        depth: u32,
        certain: bool,
    ) {
        let mut work: Vec<Reach> = targets
            .into_iter()
            .map(|(id, name)| (id, name, depth, certain))
            .collect();
        // `reach` pops from the end: keep the submission order.
        work.reverse();
        self.reach(work);
    }

    /// The coordinator visits every select() branch (`query`'s
    /// unconfigured graph under the default dependency filter): the
    /// labels of every branch are certain ([`Reach`]), so their
    /// repositories are fetched ahead of the sequential demand instead of
    /// one after another at it. Hints only, like every certainty: output
    /// stays the sequential one.
    pub(crate) fn visit_all_branches(&self) {
        lock(&self.shared.state).conditions.all_branches = true;
    }

    /// Queue the targets `rule` (one the coordinator analyzes) names, at
    /// depth 1.
    pub(crate) fn submit_rule(&self, rule: &Rule) {
        let certain = {
            let mut state = lock(&self.shared.state);
            let mut waiting = Vec::new();
            let certain = certain_labels(rule, &state.conditions, &mut waiting);
            let undecided = waiting
                .into_iter()
                .map(|condition| {
                    (
                        condition,
                        rule.label.package_id(),
                        rule.label.name.clone(),
                        1,
                    )
                })
                .collect();
            state.note_undecided(undecided);
            certain
        };
        let mut work: Vec<Reach> = rule_dependency_labels(rule)
            .into_iter()
            .map(|label| {
                let certain = certain.contains(&label);
                (label.package_id(), Some(label.name), 1, certain)
            })
            .collect();
        work.reverse();
        self.reach(work);
    }

    /// The coordinator analyzed the select() condition `label`: whether it
    /// matched (`None`: in error) — a hint for [`certain_labels`].
    pub(crate) fn note_condition(&self, label: &Label, matched: Option<bool>) {
        let mut speculate = Vec::new();
        let changed = {
            let mut state = lock(&self.shared.state);
            match state.conditions.get_mut(label) {
                None => {
                    state.conditions.insert(label.clone(), matched);
                    !state.shutdown && state.decided(label, &mut speculate)
                }
                Some(known) => {
                    if *known != matched {
                        *known = None;
                    }
                    false
                }
            }
        };
        if changed {
            grow(&self.shared);
            self.shared.work.notify_all();
        }
        for blocked in speculate {
            self.shared.repositories.speculate(&blocked);
        }
    }

    /// The finished unit of `id` for the coordinator's commit, waiting for
    /// a worker that is loading it; `None` when the coordinator should
    /// load it itself (never queued, not started yet, or abandoned). The
    /// package is settled either way: it is never queued again.
    pub(crate) fn take(&self, id: &PackageId) -> Option<LoadUnit> {
        let mut state = lock(&self.shared.state);
        let mut waited = None;
        loop {
            match state.entries.get(id) {
                Some(Entry::Running { .. }) => {
                    if waited.is_none() {
                        waited = Some(std::time::Instant::now());
                        state.counts.waited += 1;
                    }
                    state = self
                        .shared
                        .done
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                Some(Entry::Queued { key, .. }) => {
                    let key = *key;
                    state.queue.remove(&key);
                    let Some(Entry::Queued { wanted, .. }) = state.entries.remove(id) else {
                        unreachable!("matched above");
                    };
                    state.entries.insert(
                        id.clone(),
                        Entry::Taken {
                            wanted,
                            expanded: false,
                        },
                    );
                    state.counts.missed += 1;
                    return None;
                }
                Some(Entry::Done { .. }) => {
                    state.counts.taken += 1;
                    if let Some(started) = waited {
                        state.counts.wait_micros += started.elapsed().as_micros();
                    }
                    let Some(Entry::Done {
                        unit,
                        blocked,
                        wanted,
                        ..
                    }) = state.entries.remove(id)
                    else {
                        unreachable!("matched above");
                    };
                    state.entries.insert(
                        id.clone(),
                        Entry::Taken {
                            wanted,
                            expanded: blocked.is_none(),
                        },
                    );
                    return Some(*unit);
                }
                Some(Entry::Taken { .. }) | Some(Entry::Settled { .. }) => return None,
                None => {
                    state.entries.insert(
                        id.clone(),
                        Entry::Taken {
                            wanted: Wanted::default(),
                            expanded: false,
                        },
                    );
                    state.counts.missed += 1;
                    return None;
                }
            }
        }
    }

    /// `package` (`id`) was committed, by any path: it is never queued
    /// again, and the targets wanted of it that no worker followed yet
    /// (its unit was aborted, or the coordinator loaded it itself) are
    /// followed now; targets reached later are followed from its rules.
    pub(crate) fn committed(&self, id: &PackageId, package: &Package) {
        let mut speculate = Vec::new();
        let changed = {
            let mut state = lock(&self.shared.state);
            if state.shutdown {
                return;
            }
            let previous = state.entries.remove(id);
            let (followed, expanded) = match previous {
                // A worker is loading it (a P2 pattern load committed it
                // meanwhile): its unit is dropped when it finishes.
                Some(entry @ Entry::Running { .. }) => {
                    state.entries.insert(id.clone(), entry);
                    return;
                }
                Some(Entry::Queued { key, wanted }) => {
                    state.queue.remove(&key);
                    (wanted, false)
                }
                Some(Entry::Taken { wanted, expanded }) => (wanted, expanded),
                Some(Entry::Done {
                    wanted, blocked, ..
                }) => {
                    let expanded = blocked.is_none();
                    (wanted, expanded)
                }
                Some(Entry::Settled { followed, .. }) => (followed, true),
                None => (Wanted::default(), true),
            };
            let reached = if expanded {
                Vec::new()
            } else {
                let mut undecided = Undecided::new();
                let reached = expansion(
                    &package.rules,
                    &followed,
                    1,
                    &state.conditions,
                    &mut undecided,
                );
                state.note_undecided(undecided);
                reached
            };
            state.entries.insert(
                id.clone(),
                Entry::Settled {
                    rules: Some(package.rules.clone()),
                    followed,
                },
            );
            state.reach(reached, &mut speculate)
        };
        if changed {
            grow(&self.shared);
            self.shared.work.notify_all();
        }
        for blocked in speculate {
            self.shared.repositories.speculate(&blocked);
        }
    }

    /// Queue again every unit aborted by the no-demand sentinel whose
    /// blocking demand `ready` reports satisfied (the coordinator prepared
    /// the group or materialized the root since).
    pub(crate) fn retry(&self, ready: impl Fn(&str) -> bool) {
        retry_ready(&self.shared, ready);
        self.shared.repositories.retry_speculations();
    }

    /// Stop the workers (each finishes the unit it is loading) and drop
    /// every unit nobody demanded.
    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shared.repositories.stop_speculation();
        {
            let mut state = lock(&self.shared.state);
            if state.shutdown {
                return;
            }
            state.shutdown = true;
            state.queue.clear();
        }
        self.shared.work.notify_all();
        let handles = std::mem::take(&mut *lock(&self.shared.workers));
        let workers = handles.len();
        for handle in handles {
            let _ = handle.join();
        }
        let mut state = lock(&self.shared.state);
        if crate::plan_timings::enabled() {
            let unused = state
                .entries
                .values()
                .filter(|entry| matches!(entry, Entry::Done { blocked: None, .. }))
                .count();
            let counts = &state.counts;
            // The diagnostics stream: never part of what an analysis
            // snapshot captures of planning's stderr (W6R: prefetch runs
            // while a snapshot records).
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-PREFETCH workers={workers} loaded={} aborted={} retried={} taken={} waited={} wait={:.3}s missed={} unused={unused}",
                counts.loaded,
                counts.aborted,
                counts.retried,
                counts.taken,
                counts.waited,
                counts.wait_micros as f64 / 1e6,
                counts.missed
            ));
        }
        state.entries.clear();
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A speculation published a root (crate::repository_speculation): the
/// units blocked on a demand satisfied now, and the speculations, are
/// queued again.
fn retry(shared: &Arc<Shared>) {
    let repositories = shared.repositories.clone();
    retry_ready(shared, |blocked| repositories.sentinel_ready(blocked));
    repositories.retry_speculations();
}

/// [`Prefetcher::retry`] of the units.
fn retry_ready(shared: &Arc<Shared>, ready: impl Fn(&str) -> bool) {
    let mut changed = false;
    {
        let mut state = lock(&shared.state);
        if state.shutdown {
            return;
        }
        let ready_ids: Vec<PackageId> = state
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry {
                Entry::Done {
                    blocked: Some(blocked),
                    ..
                } if ready(blocked) => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in ready_ids {
            let Some(Entry::Done { depth, wanted, .. }) = state.entries.remove(&id) else {
                continue;
            };
            state.counts.retried += 1;
            let key = (depth, state.next);
            state.next += 1;
            state.queue.insert(key, id.clone());
            state.entries.insert(id, Entry::Queued { key, wanted });
            changed = true;
        }
    }
    if changed {
        grow(shared);
        shared.work.notify_all();
    }
}

/// Start workers until every queued or running unit has one (at most
/// `max_workers`). A worker that cannot be spawned leaves its work to the
/// coordinator, which loads what no worker took.
fn grow(shared: &Arc<Shared>) {
    // Lock order: the worker list, then the state (`stop` sets `shutdown`
    // before it takes the list, so no worker starts after it).
    let mut workers = lock(&shared.workers);
    let wanted = {
        let state = lock(&shared.state);
        if state.shutdown {
            return;
        }
        (state.queue.len() + state.running).min(shared.max_workers)
    };
    while workers.len() < wanted {
        let index = workers.len();
        let worker_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("bazel-rs-prefetch".to_owned())
            .stack_size(WORKER_STACK)
            .spawn(move || worker(&worker_shared, index));
        match spawned {
            Ok(handle) => workers.push(handle),
            Err(_) => break,
        }
    }
}

fn worker(shared: &Arc<Shared>, index: usize) {
    loop {
        let (id, depth, sequence) = {
            let mut state = lock(&shared.state);
            loop {
                if state.shutdown {
                    return;
                }
                if let Some((&key, _)) = state.queue.iter().next() {
                    let id = state.queue.remove(&key).expect("queued key");
                    let wanted = match state.entries.remove(&id) {
                        Some(Entry::Queued { wanted, .. }) => wanted,
                        _ => Wanted::default(),
                    };
                    state.entries.insert(id.clone(), Entry::Running { wanted });
                    state.running += 1;
                    break (id, key.0, key.1);
                }
                state = shared
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        if shared.jitter_millis > 0 {
            // BAZEL_RS_LOADING_JITTER (hidden test hook): reproducible
            // per-unit delays reorder worker completion; results must not
            // change.
            let roll = crate::engine::loading_jitter_roll(sequence, index as u64);
            std::thread::sleep(std::time::Duration::from_millis(
                roll % (shared.jitter_millis + 1),
            ));
        }
        // A panicking load is repeated by the coordinator (below).
        let loaded = crate::internal_error::recoverable_asserted(|| {
            worker_load(shared.repositories.clone(), &id)
        });
        let mut state = lock(&shared.state);
        state.running -= 1;
        let mut queued = false;
        let mut speculate = None;
        let mut speculated = Vec::new();
        match loaded {
            Ok(unit) => {
                let blocked = match &unit.result {
                    Err(error) if crate::repository_demand::is_needs_repository(error) => {
                        Some(error.clone())
                    }
                    _ => None,
                };
                state.counts.loaded += 1;
                if blocked.is_some() {
                    state.counts.aborted += 1;
                }
                if let Some(Entry::Running { .. }) = state.entries.get(&id) {
                    let Some(Entry::Running { wanted }) = state.entries.remove(&id) else {
                        unreachable!("checked above");
                    };
                    let reached = match &unit.result {
                        Ok(package)
                            if blocked.is_none() && depth < MAX_DEPTH && !state.shutdown =>
                        {
                            let mut undecided = Undecided::new();
                            let reached = expansion(
                                &package.rules,
                                &wanted,
                                depth + 1,
                                &state.conditions,
                                &mut undecided,
                            );
                            state.note_undecided(undecided);
                            reached
                        }
                        _ => Vec::new(),
                    };
                    if wanted.any_certain() {
                        speculate.clone_from(&blocked);
                    }
                    state.entries.insert(
                        id,
                        Entry::Done {
                            depth,
                            unit: Box::new(unit),
                            blocked,
                            wanted,
                        },
                    );
                    queued = state.reach(reached, &mut speculated);
                }
            }
            // A panicking load is the coordinator's to repeat (inline, at
            // its demand), where it reports like the sequential engine.
            Err(_) => {
                state.entries.insert(id, Entry::settled());
            }
        }
        drop(state);
        shared.done.notify_all();
        // The repository the unit was blocked on executes ahead of its
        // sequential demand (crate::repository_speculation).
        for blocked in speculate.iter().chain(&speculated) {
            shared.repositories.speculate(blocked);
        }
        if queued {
            grow(shared);
            shared.work.notify_all();
        }
    }
}

/// One worker's load of `id` under P2's worker guards: it never fetches,
/// and a warm publication whose recorded inputs validate may be served
/// (reuse-only).
fn worker_load(repositories: Arc<RepositorySet>, id: &PackageId) -> LoadUnit {
    let guard = crate::repository_demand::no_demand();
    let reuse = crate::repository_demand::reuse_only();
    let unit = crate::package::evaluate_load_unit(repositories, id);
    drop(reuse);
    drop(guard);
    unit
}

/// Whether `rule` is a test (or a test-only helper) by its declaration.
fn is_test(rule: &Rule) -> bool {
    if let Some(custom) = &rule.custom {
        let builtin = &custom.declaration.builtin;
        return builtin.kind.test || builtin.testonly;
    }
    rule.native.as_ref().is_some_and(|native| {
        native.rule_class.ends_with("_test")
            || native.rule_class == "test_suite"
            || matches!(
                native.attrs.get("testonly"),
                Some(crate::custom_rule::AttrValue::Bool(true))
            )
    })
}

/// The labels one rule names: native label attributes (and select() keys
/// and branches), a Starlark rule's label attributes, built-in attributes
/// and select() labels, and the `package_metadata` defaults — distinct, in
/// that order (NODEP attributes such as a `toolchain`'s implementation are
/// not followed: the engine queues an implementation once toolchain
/// resolution selects it). Hints only: a wrong guess costs a wasted load,
/// never output.
pub(crate) fn rule_dependency_labels(rule: &Rule) -> Vec<Label> {
    let mut parsed: Vec<Label> = Vec::new();
    if rule.custom.is_some() {
        parsed.extend(
            rule.srcs
                .iter()
                .filter_map(|text| Label::parse_canonical(text).ok()),
        );
    }
    parsed.extend(
        rule.package_metadata
            .iter()
            .filter_map(|text| Label::parse_canonical(text).ok()),
    );
    let native = rule
        .native
        .iter()
        .flat_map(|native| native.input_labels.iter());
    let custom = rule.custom.iter().flat_map(|custom| {
        custom
            .declaration
            .builtin
            .target
            .input_labels
            .iter()
            .chain(&custom.declaration.configurable_labels)
    });
    let mut seen: HashSet<&Label> = HashSet::new();
    let mut labels = Vec::new();
    for label in native.chain(custom).chain(parsed.iter()) {
        if label.name.is_empty() || !seen.insert(label) {
            continue;
        }
        if crate::label::unresolvable_package_message(&label.package_id()).is_none() {
            labels.push(label.clone());
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant (b) of the prefetch overlap (docs/PARALLEL_ANALYSIS.md),
    /// end to end through package loading: a worker's unit whose glob or
    /// `.bzl` load follows a symlink into another live repository root (a
    /// local_path_override module) aborts with the order-dependent
    /// sentinel — the worker never decides the link — and the
    /// coordinator's inline load of the same package follows it.
    #[cfg(unix)]
    #[test]
    fn worker_links_into_another_live_root_abort_as_order_dependent() {
        let dir = tempfile::Builder::new()
            .prefix("bazel-rs-prefetch-links-")
            .tempdir()
            .unwrap();
        let root = dir.path();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "main/MODULE.bazel",
            &format!(
                "module(name = 'main')\nbazel_dep(name = 'lib', version = '1')\nlocal_path_override(module_name = 'lib', path = '{}')\n",
                root.join("lib").display()
            ),
        );
        write("lib/MODULE.bazel", "module(name = 'lib')\n");
        write("lib/BUILD.bazel", "");
        write("lib/data/x.txt", "x\n");
        write("lib/defs.bzl", "value = 'from_lib'\n");
        write(
            "main/globbed/BUILD.bazel",
            "filegroup(name = 'g', srcs = glob(['link/**']))\n",
        );
        std::os::unix::fs::symlink(root.join("lib/data"), root.join("main/globbed/link")).unwrap();
        write(
            "main/loaded/BUILD.bazel",
            "load(':linked.bzl', 'value')\nfilegroup(name = value)\n",
        );
        std::os::unix::fs::symlink(
            root.join("lib/defs.bzl"),
            root.join("main/loaded/linked.bzl"),
        )
        .unwrap();
        let repositories = Arc::new(RepositorySet::new(root.join("main")));
        repositories.resolve_graph().unwrap();
        for (package, target, decision) in [
            ("globbed", "g", "glob symlink "),
            (
                "loaded",
                "from_lib",
                "load //loaded:linked.bzl through a symlink ",
            ),
        ] {
            let id = PackageId {
                repository: String::new(),
                package: package.to_owned(),
            };
            let worker = {
                let repositories = repositories.clone();
                let id = id.clone();
                std::thread::Builder::new()
                    .stack_size(WORKER_STACK)
                    .spawn(move || worker_load(repositories, &id).result.err())
                    .unwrap()
                    .join()
                    .unwrap()
            };
            let error = worker.unwrap_or_else(|| panic!("{package}: the worker's load succeeded"));
            assert!(
                crate::repository_demand::is_needs_repository(&error)
                    && error.contains(decision)
                    && error.contains("depends on the repositories materialized so far"),
                "{package}: {error}"
            );
            // The coordinator takes the decision at its demand point.
            let inline = crate::package::evaluate_load_unit(repositories.clone(), &id);
            let rules = inline
                .result
                .unwrap_or_else(|error| panic!("{package}: inline: {error}"))
                .rules;
            let label = Label {
                repository: String::new(),
                package: package.to_owned(),
                name: target.to_owned(),
            };
            let rule = rules
                .get(&label)
                .unwrap_or_else(|| panic!("{package}: {:?}", rules.keys()));
            if package == "globbed" {
                assert_eq!(rule.srcs, ["//globbed:link/x.txt"], "{package}");
            }
        }
    }
}
