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
}

impl Wanted {
    fn of(name: Option<String>) -> Self {
        let mut wanted = Self::default();
        wanted.add(name);
        wanted
    }

    /// Add `name` (`None`: all); whether it was new.
    fn add(&mut self, name: Option<String>) -> bool {
        match name {
            _ if self.all => false,
            None => {
                self.all = true;
                true
            }
            Some(name) => self.names.insert(name),
        }
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
}

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
/// rule) and its depth.
type Reach = (PackageId, Option<String>, u32);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    /// Add the reached targets (and, transitively, what already finished
    /// units of their packages name); whether the queue changed.
    fn reach(&mut self, mut work: Vec<Reach>) -> bool {
        let mut changed = false;
        while let Some((id, name, depth)) = work.pop() {
            match self.entries.get_mut(&id) {
                None => {
                    let key = (depth, self.next);
                    self.next += 1;
                    self.queue.insert(key, id.clone());
                    self.entries.insert(
                        id,
                        Entry::Queued {
                            key,
                            wanted: Wanted::of(name),
                        },
                    );
                    changed = true;
                }
                Some(Entry::Queued { key, wanted }) => {
                    wanted.add(name);
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
                    wanted.add(name);
                }
                Some(Entry::Done {
                    unit,
                    blocked,
                    wanted,
                    ..
                }) => {
                    // An aborted unit follows its wanted targets once a
                    // retry finishes; a finished one follows a new target
                    // now.
                    if wanted.add(name.clone())
                        && blocked.is_none()
                        && depth < MAX_DEPTH
                        && let Ok(package) = &unit.result
                    {
                        work.extend(expansion(&package.rules, &Wanted::of(name), depth + 1));
                    }
                }
                Some(Entry::Taken { wanted, .. }) => {
                    wanted.add(name);
                }
                Some(Entry::Settled {
                    rules: Some(rules),
                    followed,
                }) => {
                    if followed.add(name.clone()) && depth < MAX_DEPTH {
                        work.extend(expansion(rules, &Wanted::of(name), depth + 1));
                    }
                }
                Some(Entry::Settled { rules: None, .. }) => {}
            }
        }
        changed
    }
}

/// The targets the `wanted` rules of a package (its `rules`) name, at
/// `depth`.
fn expansion(package: &Rules, wanted: &Wanted, depth: u32) -> Vec<Reach> {
    let mut reached = Vec::new();
    let mut follow = |rule: &Rule| {
        for label in rule_dependency_labels(rule) {
            let id = label.package_id();
            reached.push((id, Some(label.name), depth));
        }
    };
    if wanted.all {
        for rule in package.values().filter(|rule| !is_test(rule)) {
            follow(rule);
        }
        return reached;
    }
    let Some(any) = package.values().next() else {
        return reached;
    };
    for name in &wanted.names {
        let label = Label {
            repository: any.label.repository.clone(),
            package: any.label.package.clone(),
            name: name.clone(),
        };
        match package.get(&label) {
            Some(rule) => follow(rule),
            // An output file: its generating rule.
            None => {
                if let Some(rule) = package
                    .values()
                    .find(|rule| rule.outs.iter().any(|out| out == name))
                {
                    follow(rule);
                }
            }
        }
    }
    reached
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
        Some(Self {
            shared: Arc::new(Shared {
                repositories,
                state: Mutex::new(state),
                work: Condvar::new(),
                done: Condvar::new(),
                jitter_millis,
                max_workers: workers.min(MAX_WORKERS),
                workers: Mutex::new(Vec::new()),
            }),
        })
    }

    fn reach(&self, work: Vec<Reach>) {
        let changed = {
            let mut state = lock(&self.shared.state);
            if state.shutdown {
                return;
            }
            state.reach(work)
        };
        if changed {
            grow(&self.shared);
            self.shared.work.notify_all();
        }
    }

    /// Queue targets (a package and a target name, `None` for every
    /// non-test rule) at `depth`, in order.
    pub(crate) fn submit_targets(
        &self,
        targets: impl IntoIterator<Item = (PackageId, Option<String>)>,
        depth: u32,
    ) {
        let mut work: Vec<Reach> = targets
            .into_iter()
            .map(|(id, name)| (id, name, depth))
            .collect();
        // `reach` pops from the end: keep the submission order.
        work.reverse();
        self.reach(work);
    }

    /// Queue the targets `rule` (one the coordinator analyzes) names, at
    /// depth 1.
    pub(crate) fn submit_rule(&self, rule: &Rule) {
        let mut work: Vec<Reach> = rule_dependency_labels(rule)
            .into_iter()
            .map(|label| (label.package_id(), Some(label.name), 1))
            .collect();
        work.reverse();
        self.reach(work);
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
                expansion(&package.rules, &followed, 1)
            };
            state.entries.insert(
                id.clone(),
                Entry::Settled {
                    rules: Some(package.rules.clone()),
                    followed,
                },
            );
            state.reach(reached)
        };
        if changed {
            grow(&self.shared);
            self.shared.work.notify_all();
        }
    }

    /// Queue again every unit aborted by the no-demand sentinel whose
    /// blocking demand `ready` reports satisfied (the coordinator prepared
    /// the group or materialized the root since).
    pub(crate) fn retry(&self, ready: impl Fn(&str) -> bool) {
        let mut changed = false;
        {
            let mut state = lock(&self.shared.state);
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
            grow(&self.shared);
            self.shared.work.notify_all();
        }
    }

    /// Stop the workers (each finishes the unit it is loading) and drop
    /// every unit nobody demanded.
    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
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
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_load(shared.repositories.clone(), &id)
        }));
        let mut state = lock(&shared.state);
        state.running -= 1;
        let mut queued = false;
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
                            expansion(&package.rules, &wanted, depth + 1)
                        }
                        _ => Vec::new(),
                    };
                    state.entries.insert(
                        id,
                        Entry::Done {
                            depth,
                            unit: Box::new(unit),
                            blocked,
                            wanted,
                        },
                    );
                    queued = state.reach(reached);
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
