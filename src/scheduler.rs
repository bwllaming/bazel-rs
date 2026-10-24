//! Bounded native DAG execution over the action cache.
//!
//! Graph structure is checked before any action (or cache I/O). A failed or
//! panicking worker stops new scheduling; all in-flight workers are joined.
//! Cancellation does not kill processes: an already-running command, including
//! one blocked forever, must finish before this call can return. The action
//! executor's unsandboxed-command and cache trust limitations still apply.

use crate::action::{self, Action, ActionResult};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Dependency {
    pub action_id: String,
    /// Index in the producer's declared outputs, not sorted output order.
    pub output_index: usize,
    #[serde(with = "crate::snapshot::codec::path")]
    pub destination: PathBuf,
}

/// Tree listings of an action's generated tree inputs, by input destination:
/// the tree's regular files relative to its root, in byte order.
pub type TreeListings = BTreeMap<PathBuf, Vec<PathBuf>>;

/// Execution-time completion of an action whose command line (or written
/// Args content) expands tree artifact inputs (docs/ACTIONS_PARAMS.md). It
/// runs once all dependencies are published, before the action key is taken.
/// Plain data (L2-W4, PLAN-L2 §1d): what to expand, applied by
/// [`Deferred::apply`], so a planned request can be encoded.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Deferred(pub(crate) Arc<DeferredSpec>);

/// What a [`Deferred`] completion expands.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum DeferredSpec {
    /// A Starlark spawn's command line (StarlarkActionFactory.buildCommandLine):
    /// the arguments and parameter files, expanded with the tree listings;
    /// an expansion failure is prefixed by the action's progress message
    /// (AbstractAction.java:380-400).
    CommandLine {
        spec: crate::command_line::CommandLineSpec,
        progress: String,
        /// Parameter-file-like inputs kept after the expansion (the empty
        /// files of a tool's runfiles tree, crate::engine).
        #[serde(with = "crate::snapshot::codec::path_contents")]
        extra_param_files: Vec<(PathBuf, Vec<u8>)>,
    },
    /// A ParameterFileWriteAction's Args content (StarlarkActionFactory.write):
    /// the written bytes in the Args' parameter file format.
    WriteArgs {
        snapshot: crate::args::ArgsSnapshot,
        #[serde(with = "crate::snapshot::codec::path_contents")]
        extra_param_files: Vec<(PathBuf, Vec<u8>)>,
    },
}

impl Deferred {
    pub(crate) fn new(spec: DeferredSpec) -> Self {
        Self(Arc::new(spec))
    }

    /// This completion, also keeping `files` as parameter-file-like inputs
    /// after the expansion.
    pub(crate) fn with_extra_param_files(self, files: Vec<(PathBuf, Vec<u8>)>) -> Self {
        let mut spec = Arc::unwrap_or_clone(self.0);
        match &mut spec {
            DeferredSpec::CommandLine {
                extra_param_files, ..
            }
            | DeferredSpec::WriteArgs {
                extra_param_files, ..
            } => extra_param_files.extend(files),
        }
        Self::new(spec)
    }

    /// Whether the expansion calls a Starlark `map_each` (an in-memory
    /// value: the request cannot be encoded, PLAN-L2 §1b).
    pub fn uses_map_each(&self) -> bool {
        match &*self.0 {
            DeferredSpec::CommandLine { spec, .. } => spec.deferred_map_each(),
            DeferredSpec::WriteArgs { snapshot, .. } => {
                crate::command_line::has_deferred_map_each(&snapshot.groups)
            }
        }
    }

    /// Complete `action` with the tree listings of its generated inputs.
    pub fn apply(&self, listings: &TreeListings, action: &mut Action) -> Result<(), String> {
        let lookup = |file: &crate::starlark_types::File| tree_listing(listings, file);
        match &*self.0 {
            DeferredSpec::CommandLine {
                spec,
                progress,
                extra_param_files,
            } => {
                let (arguments, param_files) = spec
                    .expand(Some(&lookup))
                    .map_err(|e| format!("{progress} failed: {e}"))?;
                action.arguments = arguments;
                action.extra.param_files = param_files;
                action
                    .extra
                    .param_files
                    .extend(extra_param_files.iter().cloned());
            }
            DeferredSpec::WriteArgs {
                snapshot,
                extra_param_files,
            } => {
                let arguments = crate::command_line::expand_groups(
                    &snapshot.groups,
                    snapshot.flag_per_line,
                    Some(&lookup),
                )?;
                if let Some(write) = action.write.as_mut() {
                    write.content =
                        crate::command_line::param_file_bytes(&arguments, snapshot.file_type);
                }
                action
                    .extra
                    .param_files
                    .extend(extra_param_files.iter().cloned());
            }
        }
        Ok(())
    }
}

/// The listing of one expanded tree artifact (VectorArg.expandDirectories,
/// StarlarkCustomCommandLine.java:456-485): a directory that is not an
/// input of the action fails.
fn tree_listing(
    listings: &TreeListings,
    file: &crate::starlark_types::File,
) -> Result<Vec<PathBuf>, String> {
    listings.get(Path::new(&file.path)).cloned().ok_or_else(|| {
        format!(
            "Failed to expand directory {file}. Either add the directory as an input of the action or set 'expand_directories = False' in the 'add_all' or 'add_joined' call to have the path of the directory added to the command line instead of its contents."
        )
    })
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlannedAction {
    /// Inputs here are filesystem sources; generated inputs go in dependencies.
    pub action: crate::action::Action,
    pub dependencies: Vec<Dependency>,
    pub deferred: Option<Deferred>,
    /// Shared input groups (PA-1): inputs listed after the action's own
    /// ones, each group held by every action that uses it ([`InputGroup`]).
    /// Never serialized per node: serde's `rc` support would write each
    /// group once per holder (crate::snapshot::codec).
    #[serde(skip)]
    pub groups: Vec<Arc<InputGroup>>,
}

/// Inputs shared by many planned actions (PA-1): a tool's runfiles tree
/// expanded once per build, held by every action that uses the tool — the
/// way Bazel's spawns hold the one runfiles tree artifact of a tool, rather
/// than one copy of its entries per action. A node's inputs are its own
/// followed by each group's, in order: its source inputs
/// [`PlannedAction::inputs`], its generated inputs
/// [`PlannedAction::all_dependencies`], and the tree inputs,
/// constant-metadata inputs and empty files ([`PlannedAction::flattened`]
/// is the node with the groups listed in its action). The planner shares a
/// group only when no other path of the action is, contains or lies below
/// the group's root, so listing the entries changes nothing else: no entry
/// is a duplicate to drop and the namespace checks of the listed action
/// pass exactly when [`InputGroup::valid`] and the action's own pass
/// ([`validate_node`]).
#[derive(Debug)]
pub struct InputGroup {
    /// The directory every destination lies below (the runfiles tree).
    pub root: PathBuf,
    /// Source inputs (path, destination), in order.
    pub inputs: Vec<(PathBuf, PathBuf)>,
    /// Generated inputs, in order.
    pub dependencies: Vec<Dependency>,
    /// Destinations of tree artifacts (crate::action::ActionExtra::tree_inputs).
    pub tree_inputs: Vec<PathBuf>,
    /// Destinations of constant-metadata inputs
    /// (crate::action::ActionExtra::constant_metadata).
    pub constant_metadata: Vec<PathBuf>,
    /// Empty files (runfiles empty files): parameter files without content.
    pub empty_files: Vec<PathBuf>,
    /// Every source input's path, also the ones execroot planting drops,
    /// first occurrences in order (the engine's missing-input and
    /// source-directory checks).
    pub sources: Vec<PathBuf>,
    /// Every destination (also of the source inputs planting drops) is a
    /// relative normal path strictly below `root`, none is or contains
    /// another ([`group_namespace_valid`]), and every tree input is a
    /// listed input.
    pub valid: bool,
}

/// The namespace part of [`InputGroup::valid`] for a group below `root`:
/// every path of `paths` (the destinations of every input, also of the
/// source inputs planting drops, and the empty files) is a relative normal
/// path strictly below `root`, and none is or contains another.
pub fn group_namespace_valid(root: &Path, mut paths: Vec<&Path>) -> bool {
    if paths.iter().any(|path| {
        action::validate_relative(path).is_err() || *path == root || !path.starts_with(root)
    }) {
        return false;
    }
    // In component order a path sorts directly before the paths below it:
    // adjacent pairs find any path equal to or containing another.
    paths.sort();
    !paths.windows(2).any(|pair| pair[1].starts_with(pair[0]))
}

/// Whether the groups of an action whose own part is `action` can stay
/// shared: every group is valid, no two groups' roots are equal or nested,
/// and no path of the action itself (input and embedded input
/// destinations, outputs, parameter files) is, contains or lies below a
/// group's root. Only process actions share groups.
pub fn groups_apart(action: &Action, groups: &[Arc<InputGroup>]) -> bool {
    let related = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
    if action.write.is_some() || action.extra.transform.is_some() {
        return false;
    }
    for (index, group) in groups.iter().enumerate() {
        if !group.valid
            || groups[..index]
                .iter()
                .any(|other| related(&other.root, &group.root))
        {
            return false;
        }
    }
    let own = action
        .inputs
        .iter()
        .map(|(_, destination)| destination)
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination),
        )
        .chain(&action.outputs)
        .chain(action.extra.param_files.iter().map(|(path, _)| path));
    for path in own {
        if groups.iter().any(|group| related(path, &group.root)) {
            return false;
        }
    }
    true
}

impl PlannedAction {
    /// A node without shared input groups.
    pub fn new(action: Action, dependencies: Vec<Dependency>) -> Self {
        PlannedAction {
            action,
            dependencies,
            deferred: None,
            groups: Vec::new(),
        }
    }

    /// The node's source inputs: its action's, then each group's.
    pub fn inputs(&self) -> impl Iterator<Item = &(PathBuf, PathBuf)> {
        self.action
            .inputs
            .iter()
            .chain(self.groups.iter().flat_map(|group| group.inputs.iter()))
    }

    /// The node's generated inputs: its own, then each group's.
    pub fn all_dependencies(&self) -> impl Iterator<Item = &Dependency> {
        self.dependencies.iter().chain(
            self.groups
                .iter()
                .flat_map(|group| group.dependencies.iter()),
        )
    }

    /// The node with every group listed in its action and dependencies:
    /// exactly the node planning lists without sharing.
    pub fn flattened(&self) -> PlannedAction {
        let mut node = PlannedAction {
            action: self.action.clone(),
            dependencies: self.all_dependencies().cloned().collect(),
            deferred: self.deferred.clone(),
            groups: Vec::new(),
        };
        list_groups(&mut node.action, &self.groups);
        if self
            .groups
            .iter()
            .any(|group| !group.empty_files.is_empty())
            && let Some(inner) = node.deferred.take()
        {
            // A deferred command line replaces the parameter files; the
            // empty files stay (as the planner keeps them,
            // Deferred::with_extra_param_files).
            let empty_files = self
                .groups
                .iter()
                .flat_map(|group| group.empty_files.iter())
                .map(|path| (path.clone(), Vec::new()))
                .collect();
            node.deferred = Some(inner.with_extra_param_files(empty_files));
        }
        node
    }
}

/// List the groups' source inputs, tree inputs, constant-metadata inputs
/// and empty files in `action`, after its own.
fn list_groups(action: &mut Action, groups: &[Arc<InputGroup>]) {
    for group in groups {
        action.inputs.extend(group.inputs.iter().cloned());
        action
            .extra
            .tree_inputs
            .extend(group.tree_inputs.iter().cloned());
        action
            .extra
            .constant_metadata
            .extend(group.constant_metadata.iter().cloned());
    }
    list_empty_files(action, groups);
}

fn list_empty_files(action: &mut Action, groups: &[Arc<InputGroup>]) {
    for group in groups {
        action.extra.param_files.extend(
            group
                .empty_files
                .iter()
                .map(|path| (path.clone(), Vec::new())),
        );
    }
}

/// The source path of a generated input in [`validate_node`]'s synthetic
/// action (validate_structure inspects destinations only).
const GENERATED_SOURCE: &str = "__scheduler_generated_source__";

/// `action::validate_structure` of a node as it executes: its action with
/// its generated inputs, every group listed. A node whose groups are
/// apart ([`groups_apart`]) is checked without listing them: its own part
/// and each group's namespace ([`InputGroup::valid`]) decide, exactly as
/// the listed action would; any other node is listed and checked as is.
pub fn validate_node(node: &PlannedAction) -> Result<(), String> {
    let generated = |dependency: &Dependency| {
        (
            PathBuf::from(GENERATED_SOURCE),
            dependency.destination.clone(),
        )
    };
    let mut synthetic = node.action.clone();
    synthetic
        .inputs
        .extend(node.dependencies.iter().map(generated));
    if node.groups.is_empty() {
        return action::validate_structure(&synthetic);
    }
    if groups_apart(&synthetic, &node.groups) && action::validate_structure(&synthetic).is_ok() {
        return Ok(());
    }
    let mut listed = node.action.clone();
    list_groups(&mut listed, &node.groups);
    listed.inputs.extend(node.all_dependencies().map(generated));
    action::validate_structure(&listed)
}

struct Graph {
    /// The nodes in planning order (taken when dispatched), by id: indexing
    /// moves no node.
    nodes: Vec<Option<PlannedAction>>,
    index: BTreeMap<String, usize>,
    remaining: BTreeMap<String, usize>,
    consumers: BTreeMap<String, Vec<String>>,
    ready: BTreeSet<String>,
}

fn validate(nodes: Vec<PlannedAction>, jobs: usize) -> Result<Graph, String> {
    if jobs == 0 {
        return Err("scheduler jobs must be greater than zero".into());
    }
    let mut index = BTreeMap::new();
    for (position, node) in nodes.iter().enumerate() {
        let id = &node.action.id;
        if index.insert(id.clone(), position).is_some() {
            return Err(format!("action '{id}': duplicate action id"));
        }
    }
    // Every node's checks are independent of the others: they run up front
    // (on several threads for large graphs), and the loop below reports the
    // first failure in id order, exactly as a sequential pass does.
    let ordered: Vec<(&String, &PlannedAction)> = index
        .iter()
        .map(|(id, &position)| (id, &nodes[position]))
        .collect();
    let checks = action::try_map_ordered(&ordered, |&(id, node)| {
        for dependency in node.all_dependencies() {
            let producer = index
                .get(&dependency.action_id)
                .map(|&position| &nodes[position])
                .ok_or_else(|| {
                    format!(
                        "action '{id}': unknown dependency '{}'",
                        dependency.action_id
                    )
                })?;
            if dependency.output_index >= producer.action.outputs.len() {
                return Err(format!(
                    "action '{id}': dependency '{}' output index {} is out of range ({} outputs)",
                    dependency.action_id,
                    dependency.output_index,
                    producer.action.outputs.len()
                ));
            }
        }
        validate_node(node).map_err(|e| format!("action '{id}': {e}"))
    });
    let mut remaining = BTreeMap::new();
    let mut consumers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for ((id, node), check) in ordered.into_iter().zip(checks) {
        check?;
        let mut count = 0;
        for dependency in node.all_dependencies() {
            consumers
                .entry(dependency.action_id.clone())
                .or_default()
                .push(id.clone());
            count += 1;
        }
        remaining.insert(id.clone(), count);
    }
    let ready: BTreeSet<_> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    // Kahn's algorithm avoids recursion even for very deep generated graphs.
    // Multiple outputs from one producer are counted/decremented separately.
    let mut pending = remaining.clone();
    let mut visit = ready.clone();
    let mut visited = 0;
    while let Some(id) = visit.pop_first() {
        visited += 1;
        if let Some(children) = consumers.get(&id) {
            for child in children {
                let count = pending.get_mut(child).expect("known consumer");
                *count -= 1;
                if *count == 0 {
                    visit.insert(child.clone());
                }
            }
        }
    }
    if visited != index.len() {
        let blocked: Vec<_> = pending
            .iter()
            .filter(|(_, count)| **count != 0)
            .map(|(id, _)| format!("'{id}'"))
            .collect();
        return Err(format!(
            "dependency cycle: actions in or blocked by a cycle: {}",
            blocked.join(", ")
        ));
    }
    Ok(Graph {
        nodes: nodes.into_iter().map(Some).collect(),
        index,
        remaining,
        consumers,
        ready,
    })
}

/// Execute a fully validated graph with at most `jobs` concurrent actions.
///
/// Results are keyed deterministically by action id. Generated inputs use the
/// producer's actual published paths and declaration-order output indices.
/// Duplicate input destinations follow `action::validate_structure` semantics:
/// their contents/modes must agree at execution time. Source existence and
/// contents are likewise checked by execution, not by structural validation.
/// Failures include action ids; failures from all in-flight workers are reported.
/// No new work starts after failure is observed; running processes are not killed.
pub fn execute_graph(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
) -> Result<BTreeMap<String, crate::action::ActionResult>, String> {
    // Process strings cross Bazel's internalToPlatform boundary first: lone
    // analysis-model bytes decode with replacement (bazel_bytes.rs).
    execute_with(cache, nodes, jobs, &|cache: &Path, action: &Action| {
        action::execute(cache, &crate::bazel_bytes::platform_action(action))
    })
}

/// One dispatched action: the node and its producers' results in
/// dependency order (shared with, not copied by, the dispatching thread).
struct Job {
    id: String,
    node: PlannedAction,
    producers: Vec<Arc<ActionResult>>,
}

/// A finished job: `None` when it was skipped because a failure had been
/// published before it started.
type Completion = (String, Option<Result<ActionResult, String>>);

/// Run one job on a worker: its generated inputs (each producer's published
/// output, in dependency order) and tree listings, then the deferred
/// completion and the action itself.
fn run_job<F>(job: Job, cache: &Path, failed: &AtomicBool, execute: &F) -> Completion
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    let Job {
        id,
        mut node,
        producers,
    } = job;
    // A failure can arrive while this job waits for its worker. Do not
    // invoke another action in that case.
    if failed.load(Ordering::Acquire) {
        return (id, None);
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // The groups' inputs are listed in the action now, for this one
        // execution (PA-1): their sources after the action's own, their
        // generated inputs after its own generated ones.
        let groups = std::mem::take(&mut node.groups);
        list_groups(&mut node.action, &groups);
        let mut listings = TreeListings::new();
        let dependencies = std::mem::take(&mut node.dependencies);
        let shared = groups.iter().flat_map(|group| group.dependencies.iter());
        for (dependency, producer) in dependencies.iter().chain(shared).zip(&producers) {
            if let Some(listing) = producer.trees.get(&dependency.output_index) {
                listings.insert(dependency.destination.clone(), listing.clone());
            }
            node.action.inputs.push((
                producer.outputs[dependency.output_index].clone(),
                dependency.destination.clone(),
            ));
        }
        if let Some(deferred) = &node.deferred {
            deferred.apply(&listings, &mut node.action)?;
            // The deferred command line replaced the parameter files; the
            // groups' empty files stay.
            list_empty_files(&mut node.action, &groups);
        }
        let result = execute(cache, &node.action)?;
        if result.outputs.len() != node.action.outputs.len() {
            return Err(format!(
                "executor returned {} outputs, expected {}",
                result.outputs.len(),
                node.action.outputs.len()
            ));
        }
        Ok(result)
    }))
    .unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        Err(format!("worker panicked: {message}"))
    });
    if result.is_err() {
        // Publish cancellation before sending, so another ready node cannot
        // slip through behind a queued failure.
        failed.store(true, Ordering::Release);
    }
    (id, Some(result))
}

// The worker seam allows concurrency/lifecycle tests without sleeps or shell
// timing assumptions. Production always delegates to action::execute (after
// the platform-string conversion), retaining its atomic per-key fs2 cache
// locking.
//
// Actions run on a pool of at most `jobs` worker threads, started when every
// existing worker is busy, instead of one thread per action: dispatching a
// ready action is a channel send, and the dispatching thread no longer
// copies generated inputs or tree listings (the worker assembles them from
// the shared producer results). Which actions run, in which order and how
// many at once is exactly as before: ready actions in id order, at most
// `jobs` in flight, none after a failure is observed.
fn execute_with<F>(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    execute: &F,
) -> Result<BTreeMap<String, ActionResult>, String>
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    let mut graph = validate(nodes, jobs)?;
    let failed = AtomicBool::new(false);
    let mut results: BTreeMap<String, Arc<ActionResult>> = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let (job_sender, job_receiver) = mpsc::channel::<Job>();
    let job_receiver = Mutex::new(job_receiver);
    let (done_sender, done_receiver) = mpsc::channel::<Completion>();
    thread::scope(|scope| {
        let mut job_sender = Some(job_sender);
        let mut workers = 0;
        let mut running = BTreeSet::new();
        loop {
            while running.len() < jobs && !failed.load(Ordering::Acquire) {
                let Some(id) = graph.ready.pop_first() else {
                    break;
                };
                let node = graph.nodes[graph.index[&id]]
                    .take()
                    .expect("ready node exists");
                // Every job in flight has a worker of its own.
                if workers == running.len() {
                    let done = done_sender.clone();
                    let (receiver, failed) = (&job_receiver, &failed);
                    let spawn = thread::Builder::new().spawn_scoped(scope, move || {
                        loop {
                            let job = {
                                let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
                                receiver.recv()
                            };
                            // Closed: everything was dispatched.
                            let Ok(job) = job else {
                                return;
                            };
                            if done.send(run_job(job, cache, failed, execute)).is_err() {
                                return;
                            }
                        }
                    });
                    match spawn {
                        Ok(_) => workers += 1,
                        Err(error) => {
                            failed.store(true, Ordering::Release);
                            errors.insert(id, format!("cannot spawn worker: {error}"));
                            continue;
                        }
                    }
                }
                let producers = node
                    .all_dependencies()
                    .map(|dependency| Arc::clone(&results[&dependency.action_id]))
                    .collect();
                job_sender
                    .as_ref()
                    .expect("dispatching")
                    .send(Job {
                        id: id.clone(),
                        node,
                        producers,
                    })
                    .expect("workers outlive dispatching");
                running.insert(id);
            }
            if running.is_empty() {
                break;
            }
            // Every dispatched job sends one completion, including caught
            // panics and jobs skipped on cancellation.
            let (id, outcome) = done_receiver.recv().expect("workers send completion");
            assert!(running.remove(&id), "completion from a running job");
            match outcome {
                Some(Ok(result)) => {
                    results.insert(id.clone(), Arc::new(result));
                    if let Some(children) = graph.consumers.get(&id) {
                        for child in children {
                            let count = graph.remaining.get_mut(child).expect("known consumer");
                            *count -= 1;
                            if *count == 0 {
                                graph.ready.insert(child.clone());
                            }
                        }
                    }
                }
                Some(Err(error)) => {
                    errors.insert(id, error);
                }
                None => {}
            }
        }
        // Idle workers see the closed channel and exit; the scope joins them.
        job_sender.take();
    });
    if errors.is_empty() {
        Ok(results
            .into_iter()
            .map(|(id, result)| {
                let result = Arc::into_inner(result).expect("workers are joined");
                (id, result)
            })
            .collect())
    } else {
        Err(errors
            .into_iter()
            .map(|(id, error)| format!("action '{id}': {error}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::time::Duration;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    const TIMEOUT: Duration = Duration::from_secs(20);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "native-scheduler-test-{}-{}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn node(id: &str) -> PlannedAction {
        PlannedAction {
            action: Action {
                id: id.into(),
                command: "printf ok > out".into(),
                inputs: vec![],
                outputs: vec!["out".into()],
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
                ..Default::default()
            },
            dependencies: vec![],
            deferred: None,
            groups: Vec::new(),
        }
    }

    fn dependency(id: &str, index: usize, destination: &str) -> Dependency {
        Dependency {
            action_id: id.into(),
            output_index: index,
            destination: destination.into(),
        }
    }

    fn success(action: &Action) -> Result<ActionResult, String> {
        Ok(ActionResult {
            outputs: action.outputs.clone(),
            cache_hit: false,
            trees: BTreeMap::new(),
            remote_hit: false,
        })
    }

    #[test]
    fn native_write_executable_feeds_direct_process() {
        let fixture = Fixture::new();
        let mut producer = node("producer");
        producer.action.command.clear();
        producer.action.write = Some(action::WriteAction {
            content: b"#!/bin/sh\nfalse\nprintf '%s:%s' \"$1\" \"$VALUE\" > out\n".to_vec(),
            executable: true,
        });
        let mut consumer = node("consumer");
        consumer.action.command.clear();
        consumer.action.program = Some("./generated tool".into());
        consumer.action.arguments = vec!["literal $1".into()];
        consumer
            .action
            .environment
            .insert("VALUE".into(), "雪".into());
        consumer.dependencies = vec![dependency("producer", 0, "generated tool")];
        let graph = vec![consumer, producer];
        let first = execute_graph(&fixture.cache(), graph.clone(), 2).unwrap();
        assert_eq!(
            fs::read(&first["consumer"].outputs[0]).unwrap(),
            "literal $1:雪".as_bytes()
        );
        assert!(first.values().all(|r| !r.cache_hit));
        assert!(
            execute_graph(&fixture.cache(), graph, 2)
                .unwrap()
                .values()
                .all(|r| r.cache_hit)
        );
    }

    #[test]
    fn invalid_direct_program_rejected_before_any_action() {
        let mut direct = node("direct");
        direct.action.program = Some("/bin/true".into());
        invalid(vec![direct.clone()], 2, "empty shell command");
        direct.action.command.clear();
        direct.action.program = Some(String::new());
        invalid(vec![direct], 2, "direct program must be nonempty");
    }

    #[test]
    fn native_write_producer_feeds_shell_arguments_and_environment() {
        let fixture = Fixture::new();
        let mut producer = node("producer");
        producer.action.command.clear();
        producer.action.write = Some(action::WriteAction {
            content: b"native\0bytes".to_vec(),
            executable: false,
        });
        let mut consumer = node("consumer");
        consumer.dependencies = vec![dependency("producer", 0, "generated")];
        consumer.action.command =
            "cat generated > out; printf '%s:%s' \"$1\" \"$VALUE\" >> out".into();
        consumer.action.arguments = vec!["literal $1".into()];
        consumer.action.shell_pad = true;
        consumer
            .action
            .environment
            .insert("VALUE".into(), "雪".into());
        let graph = vec![consumer, producer];
        let first = execute_graph(&fixture.cache(), graph.clone(), 2).unwrap();
        assert!(first.values().all(|r| !r.cache_hit));
        assert_eq!(
            fs::read(&first["consumer"].outputs[0]).unwrap(),
            "native\0bytesliteral $1:雪".as_bytes()
        );
        assert!(
            execute_graph(&fixture.cache(), graph, 2)
                .unwrap()
                .values()
                .all(|r| r.cache_hit)
        );
    }

    #[test]
    fn write_generated_inputs_and_invalid_shell_values_rejected_before_any_action() {
        let mut write = node("write");
        write.action.command.clear();
        write.action.write = Some(action::WriteAction {
            content: vec![0],
            executable: false,
        });
        write.dependencies = vec![dependency("producer", 0, "generated")];
        invalid(vec![node("producer"), write], 2, "write action");
        let mut shell = node("shell");
        shell.action.arguments.push("bad\0argument".into());
        invalid(vec![shell.clone()], 2, "argument contains a NUL");
        shell.action.arguments.clear();
        shell
            .action
            .environment
            .insert("bad=key".into(), "value".into());
        invalid(vec![shell], 2, "environment key");
    }

    #[test]
    fn chain_fanout_fanin_and_repeated_graph_cache_hits() {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        fs::write(&source, "hello").unwrap();
        let mut root = node("root");
        root.action.inputs.push((source, "src".into()));
        // Deliberately reverse lexical order to test declaration-order indices.
        root.action.outputs = vec!["z".into(), "a".into()];
        root.action.command = "cat src > z; printf second > a".into();
        let mut left = node("left");
        left.dependencies = vec![dependency("root", 0, "generated")];
        left.action.command = "cat generated > out".into();
        let mut right = node("right");
        right.dependencies = vec![
            dependency("root", 1, "first"),
            dependency("root", 0, "second"),
        ];
        right.action.command = "cat first second > out".into();
        let mut end = node("end");
        end.dependencies = vec![dependency("left", 0, "l"), dependency("right", 0, "r")];
        end.action.command = "cat l r > out".into();
        let graph = vec![end, right, root, left];
        let first = execute_graph(&fixture.cache(), graph.clone(), 3).unwrap();
        assert_eq!(
            first.keys().map(String::as_str).collect::<Vec<_>>(),
            ["end", "left", "right", "root"]
        );
        assert!(first.values().all(|r| !r.cache_hit));
        assert_eq!(
            fs::read(&first["end"].outputs[0]).unwrap(),
            b"hellosecondhello"
        );
        let second = execute_graph(&fixture.cache(), graph, 2).unwrap();
        assert!(second.values().all(|r| r.cache_hit));
        for (id, result) in first {
            assert_eq!(result.outputs, second[&id].outputs);
        }
    }

    fn invalid(graph: Vec<PlannedAction>, jobs: usize, expected: &str) {
        let fixture = Fixture::new();
        let marker = fixture.0.join("ran");
        let mut independent = node("000-independent");
        independent.action.command = format!("touch '{}'; printf ok > out", marker.display());
        let mut graph = graph;
        graph.insert(0, independent);
        let error = execute_graph(&fixture.cache(), graph, jobs).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(
            !fixture.cache().exists(),
            "validation created cache: {error}"
        );
        assert!(
            !marker.exists(),
            "validation ran independent action: {error}"
        );
    }

    #[test]
    fn validates_ids_edges_indices_cycles_and_jobs_before_any_action() {
        invalid(vec![node("duplicate"), node("duplicate")], 4, "duplicate");
        let mut a = node("a");
        a.dependencies.push(dependency("missing", 0, "input"));
        invalid(
            vec![a.clone()],
            4,
            "action 'a': unknown dependency 'missing'",
        );
        a.dependencies = vec![dependency("b", 1, "input")];
        invalid(vec![a.clone(), node("b")], 4, "output index 1");
        a.dependencies = vec![dependency("a", 0, "input")];
        invalid(vec![a.clone()], 4, "cycle");
        a.dependencies = vec![dependency("b", 0, "input")];
        let mut b = node("b");
        b.dependencies = vec![dependency("a", 0, "input")];
        invalid(vec![a, b], 4, "'a', 'b'");
        invalid(vec![node("a")], 0, "jobs must be greater than zero");
    }

    #[test]
    fn large_graphs_report_the_first_invalid_action_in_id_order() {
        // Enough nodes to check them on several threads.
        let mut graph: Vec<PlannedAction> = (0..300)
            .map(|i| {
                let mut node = node(&format!("n{i:03}"));
                if i > 0 {
                    node.dependencies = vec![dependency(&format!("n{:03}", i - 1), 0, "input")];
                }
                node
            })
            .collect();
        graph[250].action.outputs = vec!["../escape".into()];
        graph[120].dependencies = vec![dependency("missing", 0, "input")];
        graph[200].dependencies = vec![dependency("n100", 3, "input")];
        invalid(
            graph.clone(),
            4,
            "action 'n120': unknown dependency 'missing'",
        );
        graph[120].dependencies.clear();
        invalid(
            graph.clone(),
            4,
            "action 'n200': dependency 'n100' output index 3 is out of range (1 outputs)",
        );
        graph[200].dependencies.clear();
        invalid(
            graph.clone(),
            4,
            "action 'n250': unsafe mapped path '../escape'",
        );
        graph[250].action.outputs = vec!["out".into()];
        let fixture = Fixture::new();
        let results = execute_with(&fixture.cache(), graph, 8, &|_: &Path, action: &Action| {
            success(action)
        })
        .unwrap();
        assert_eq!(results.len(), 300);
    }

    #[test]
    fn validates_complete_generated_and_source_namespaces_before_any_action() {
        for destination in ["/absolute", "../escape", "a//b", "", "out", "out/child"] {
            let mut consumer = node("consumer");
            consumer.dependencies = vec![dependency("producer", 0, destination)];
            invalid(vec![node("producer"), consumer], 4, "action 'consumer'");
        }
        let mut consumer = node("consumer");
        consumer
            .action
            .inputs
            .push(("does-not-exist".into(), "src".into()));
        consumer.dependencies = vec![dependency("producer", 0, "src/child")];
        invalid(
            vec![node("producer"), consumer.clone()],
            4,
            "overlapping input",
        );
        consumer.action.inputs.clear();
        consumer.dependencies = vec![
            dependency("producer", 0, "src"),
            dependency("producer", 0, "src/child"),
        ];
        invalid(vec![node("producer"), consumer], 4, "overlapping input");
        let mut malformed = node("malformed");
        malformed.action.outputs = vec!["parent".into(), "parent/child".into()];
        invalid(vec![malformed.clone()], 4, "action 'malformed'");
        malformed.action.outputs = vec!["../escape".into()];
        invalid(vec![malformed.clone()], 4, "unsafe mapped path");
        malformed.action.outputs.clear();
        malformed.action.command = "true\0".into();
        invalid(vec![malformed], 4, "NUL");
    }

    #[test]
    fn independent_workers_overlap_and_never_exceed_jobs() {
        // Controller-driven waves prove overlap without measuring wall time.
        // Timeouts only prevent a broken scheduler from hanging the test suite.
        for jobs in [1, 2, 4] {
            let active = AtomicUsize::new(0);
            let maximum = AtomicUsize::new(0);
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let worker = |_: &Path, action: &Action| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                started_tx.send(action.id.clone()).unwrap();
                release_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
                success(action)
            };
            thread::scope(|scope| {
                let handle = scope.spawn(|| {
                    let graph = (0..12).map(|i| node(&format!("node-{i:02}"))).collect();
                    execute_with(Path::new("unused"), graph, jobs, &worker)
                });
                for _ in 0..12 / jobs {
                    for _ in 0..jobs {
                        started_rx.recv_timeout(TIMEOUT).unwrap();
                    }
                    assert_eq!(active.load(Ordering::SeqCst), jobs);
                    for _ in 0..jobs {
                        release_tx.send(()).unwrap();
                    }
                }
                assert_eq!(handle.join().unwrap().unwrap().len(), 12);
            });
            assert_eq!(maximum.load(Ordering::SeqCst), jobs);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        }
    }

    /// Actions run on a pool of at most `jobs` worker threads, reused from
    /// action to action, never on the dispatching thread.
    #[test]
    fn workers_are_a_pool_of_at_most_jobs_threads() {
        for jobs in [1, 2, 3, 8] {
            let threads = Mutex::new(std::collections::HashSet::new());
            let worker = |_: &Path, action: &Action| {
                threads.lock().unwrap().insert(thread::current().id());
                success(action)
            };
            // Independent actions and a chain that keeps finishing and
            // dispatching, 60 actions in all.
            let mut graph: Vec<PlannedAction> =
                (0..40).map(|i| node(&format!("free-{i:02}"))).collect();
            for i in 0..20 {
                let mut link = node(&format!("chain-{i:02}"));
                if i > 0 {
                    link.dependencies =
                        vec![dependency(&format!("chain-{:02}", i - 1), 0, "input")];
                }
                graph.push(link);
            }
            let results = execute_with(Path::new("unused"), graph, jobs, &worker).unwrap();
            assert_eq!(results.len(), 60);
            let threads = threads.into_inner().unwrap();
            assert!(
                threads.len() <= jobs,
                "{} worker threads for jobs={jobs}",
                threads.len()
            );
            assert!(!threads.contains(&thread::current().id()));
        }
    }

    #[test]
    fn failed_dependency_and_queued_independent_action_never_start() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("dependent-ran");
        let mut bad = node("a-fail");
        bad.action.command = "echo intentional-failure >&2; exit 7".into();
        let mut dependent = node("dependent");
        dependent.dependencies = vec![dependency("a-fail", 0, "input")];
        dependent.action.command = format!("touch '{}'; cat input > out", marker.display());
        let mut queued = node("z-queued");
        queued.action.command = dependent.action.command.clone();
        let error = execute_graph(&fixture.cache(), vec![dependent, queued, bad], 1).unwrap_err();
        assert!(error.contains("a-fail") && error.contains("intentional-failure"));
        assert!(!marker.exists());
    }

    #[test]
    fn failure_waits_for_running_actions_and_collects_their_errors() {
        let (started_tx, started_rx) = mpsc::channel();
        let (fail_tx, fail_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let fail_rx = Mutex::new(fail_rx);
        let finish_rx = Mutex::new(finish_rx);
        let finished = AtomicBool::new(false);
        let worker = |_: &Path, action: &Action| {
            started_tx.send(action.id.clone()).unwrap();
            match action.id.as_str() {
                "a-fail" => {
                    fail_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    Err("first failure".into())
                }
                "b-running" => {
                    finish_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    finished.store(true, Ordering::SeqCst);
                    Err("in-flight failure".into())
                }
                _ => panic!("unexpected new action after failure"),
            }
        };
        thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel();
            let worker = &worker;
            let finished = &finished;
            let handle = scope.spawn(move || {
                let mut dependent = node("dependent");
                dependent.dependencies = vec![dependency("a-fail", 0, "input")];
                let result = execute_with(
                    Path::new("unused"),
                    vec![
                        node("a-fail"),
                        node("b-running"),
                        dependent,
                        node("z-queued"),
                    ],
                    2,
                    worker,
                );
                assert!(
                    finished.load(Ordering::SeqCst),
                    "returned before in-flight work ended"
                );
                done_tx.send(()).unwrap();
                result
            });
            let started: BTreeSet<_> = (0..2)
                .map(|_| started_rx.recv_timeout(TIMEOUT).unwrap())
                .collect();
            assert_eq!(
                started,
                BTreeSet::from(["a-fail".into(), "b-running".into()])
            );
            fail_tx.send(()).unwrap();
            assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            finish_tx.send(()).unwrap();
            let error = handle.join().unwrap().unwrap_err();
            assert!(error.contains("action 'a-fail': first failure"), "{error}");
            assert!(
                error.contains("action 'b-running': in-flight failure"),
                "{error}"
            );
            assert!(started_rx.try_recv().is_err());
        });
    }

    #[test]
    fn panicking_worker_returns_actionable_error_without_hanging() {
        for string_payload in [true, false] {
            let calls = AtomicUsize::new(0);
            let worker = |_: &Path, _: &Action| -> Result<ActionResult, String> {
                calls.fetch_add(1, Ordering::SeqCst);
                if string_payload {
                    panic!("intentional panic");
                }
                std::panic::panic_any(42_u32);
            };
            let mut dependent = node("dependent");
            dependent.dependencies = vec![dependency("a-panic", 0, "input")];
            let error = execute_with(
                Path::new("unused"),
                vec![dependent, node("a-panic"), node("z-queued")],
                1,
                &worker,
            )
            .unwrap_err();
            assert!(
                error.contains("action 'a-panic': worker panicked"),
                "{error}"
            );
            assert!(error.contains(if string_payload {
                "intentional panic"
            } else {
                "non-string panic payload"
            }));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn empty_graph_does_not_create_cache_and_zero_jobs_is_still_invalid() {
        let fixture = Fixture::new();
        assert!(
            execute_graph(&fixture.cache(), vec![], 2)
                .unwrap()
                .is_empty()
        );
        assert!(execute_graph(&fixture.cache(), vec![], 0).is_err());
        assert!(!fixture.cache().exists());
    }

    /// A group below `root`: sources `(path, destination)`, generated
    /// inputs `(producer, destination)`, empty files.
    fn group(
        root: &str,
        sources: &[(&Path, &str)],
        generated: &[(&str, &str)],
        empty: &[&str],
    ) -> Arc<InputGroup> {
        let inputs: Vec<(PathBuf, PathBuf)> = sources
            .iter()
            .map(|(path, destination)| (path.to_path_buf(), destination.into()))
            .collect();
        let dependencies: Vec<Dependency> = generated
            .iter()
            .map(|(producer, destination)| dependency(producer, 0, destination))
            .collect();
        let empty_files: Vec<PathBuf> = empty.iter().map(PathBuf::from).collect();
        let mut paths: Vec<&Path> = inputs.iter().map(|(_, d)| d.as_path()).collect();
        paths.extend(dependencies.iter().map(|d| d.destination.as_path()));
        paths.extend(empty_files.iter().map(PathBuf::as_path));
        let valid = group_namespace_valid(Path::new(root), paths);
        Arc::new(InputGroup {
            root: root.into(),
            sources: inputs.iter().map(|(path, _)| path.clone()).collect(),
            inputs,
            dependencies,
            tree_inputs: Vec::new(),
            constant_metadata: Vec::new(),
            empty_files,
            valid,
        })
    }

    /// The scheduler's validation of a node before PA-1: its listed action
    /// with every generated input's destination.
    fn listed_validation(node: &PlannedAction) -> Result<(), String> {
        let listed = node.flattened();
        let mut synthetic = listed.action.clone();
        for dependency in &listed.dependencies {
            synthetic.inputs.push((
                PathBuf::from(GENERATED_SOURCE),
                dependency.destination.clone(),
            ));
        }
        action::validate_structure(&synthetic)
    }

    #[test]
    fn grouped_nodes_validate_exactly_like_their_listed_form() {
        let source = Path::new("/nonexistent/source");
        let tool = group(
            "tool.runfiles",
            &[
                (source, "tool.runfiles/_main/a"),
                (source, "tool.runfiles/_main/b"),
            ],
            &[("producer", "tool.runfiles/_main/gen")],
            &["tool.runfiles/_main/__init__.py"],
        );
        let mut consumer = node("consumer");
        consumer.groups = vec![tool.clone()];
        let cases: Vec<(PlannedAction, bool)> = vec![
            // Apart: decided without listing.
            (consumer.clone(), true),
            // An output inside the tree, an input equal to an entry, an
            // input containing the tree, a parameter file in it.
            (
                {
                    let mut n = consumer.clone();
                    n.action.outputs = vec!["tool.runfiles/_main/a".into()];
                    n
                },
                false,
            ),
            (
                {
                    let mut n = consumer.clone();
                    n.action
                        .inputs
                        .push((source.into(), "tool.runfiles/_main/b".into()));
                    n
                },
                false,
            ),
            (
                {
                    let mut n = consumer.clone();
                    n.action
                        .inputs
                        .push((source.into(), "tool.runfiles".into()));
                    n
                },
                false,
            ),
            (
                {
                    let mut n = consumer.clone();
                    n.action
                        .extra
                        .param_files
                        .push(("tool.runfiles/_main/__init__.py".into(), Vec::new()));
                    n
                },
                false,
            ),
            // A group whose own entries overlap, a group whose root holds
            // another group's.
            (
                {
                    let mut n = consumer.clone();
                    n.groups = vec![group(
                        "t.runfiles",
                        &[(source, "t.runfiles/x"), (source, "t.runfiles/x/y")],
                        &[],
                        &[],
                    )];
                    n
                },
                false,
            ),
            (
                {
                    let mut n = consumer.clone();
                    n.groups = vec![
                        tool.clone(),
                        group(
                            "tool.runfiles/_main",
                            &[(source, "tool.runfiles/_main/c")],
                            &[],
                            &[],
                        ),
                    ];
                    n
                },
                false,
            ),
        ];
        for (index, (node, apart)) in cases.into_iter().enumerate() {
            assert_eq!(
                groups_apart(&node.action, &node.groups),
                apart,
                "case {index}"
            );
            assert_eq!(
                validate_node(&node),
                listed_validation(&node),
                "case {index}"
            );
        }
        // Only the listed form of an invalid case reports its error.
        let mut overlapping = consumer.clone();
        overlapping.action.outputs = vec!["tool.runfiles/_main/a".into()];
        assert!(
            validate_node(&overlapping)
                .unwrap_err()
                .contains("overlapping input/output path 'tool.runfiles/_main/a'")
        );
    }

    #[test]
    fn grouped_nodes_execute_with_the_keys_of_their_listed_form() {
        let fixture = Fixture::new();
        let data = fixture.0.join("data");
        fs::write(&data, "data").unwrap();
        let mut producer = node("producer");
        producer.action.command = "printf generated > out".into();
        let tool = group(
            "tool.runfiles",
            &[(&data, "tool.runfiles/_main/data")],
            &[("producer", "tool.runfiles/_main/gen")],
            &["tool.runfiles/_main/__init__.py"],
        );
        let list = "(cd tool.runfiles && find . -type f | LC_ALL=C sort && cat _main/data _main/gen) > out";
        let mut plain = node("plain");
        plain.action.command = list.into();
        plain.groups = vec![tool.clone()];
        // A deferred command line replaces the parameter files; the
        // group's empty files stay.
        let mut deferred = node("deferred");
        deferred.action.command = format!("{list}; echo \"$1\" >> out");
        deferred.action.shell_pad = true;
        deferred.groups = vec![tool];
        // A command line expanding to `completed` without parameter files.
        deferred.deferred = Some(Deferred::new(DeferredSpec::CommandLine {
            spec: crate::command_line::CommandLineSpec {
                executable: Vec::new(),
                parts: vec![crate::command_line::Part {
                    groups: vec![crate::command_line::Group::Strings(vec![
                        "completed".into(),
                    ])],
                    flag_per_line: false,
                    param: None,
                }],
                primary_output: "out".into(),
                max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
            },
            progress: "Action out".into(),
            extra_param_files: Vec::new(),
        }));
        let grouped = vec![producer.clone(), plain, deferred];
        let first = execute_graph(&fixture.cache(), grouped.clone(), 2).unwrap();
        assert!(first.values().all(|result| !result.cache_hit));
        let expected = "./_main/__init__.py\n./_main/data\n./_main/gen\ndatagenerated";
        assert_eq!(
            fs::read_to_string(&first["plain"].outputs[0]).unwrap(),
            expected
        );
        assert_eq!(
            fs::read_to_string(&first["deferred"].outputs[0]).unwrap(),
            format!("{expected}completed\n")
        );
        // The listed nodes are the same actions: every one is a cache hit.
        let listed: Vec<PlannedAction> = grouped.iter().map(PlannedAction::flattened).collect();
        assert!(listed.iter().all(|node| node.groups.is_empty()));
        assert_eq!(listed[1].action.inputs.len(), 1);
        assert_eq!(listed[1].dependencies.len(), 1);
        assert_eq!(listed[1].action.extra.param_files.len(), 1);
        let second = execute_graph(&fixture.cache(), listed, 2).unwrap();
        assert!(second.values().all(|result| result.cache_hit));
        for (id, result) in first {
            assert_eq!(result.outputs, second[&id].outputs);
        }
    }

    #[test]
    fn duplicate_generated_destinations_use_existing_action_content_rules() {
        let fixture = Fixture::new();
        let mut consumer = node("consumer");
        consumer.dependencies = vec![
            dependency("producer", 0, "input"),
            dependency("producer", 0, "input"),
        ];
        consumer.action.command = "cat input > out".into();
        let result = execute_graph(&fixture.cache(), vec![consumer, node("producer")], 2).unwrap();
        assert_eq!(fs::read(&result["consumer"].outputs[0]).unwrap(), b"ok");
    }
}
