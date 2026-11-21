//! Bounded native DAG execution over the action cache.
//!
//! Graph structure is checked before any action (or cache I/O). A failed or
//! panicking worker stops new scheduling; all in-flight workers are joined.
//! Cancellation does not kill processes: an already-running command, including
//! one blocked forever, must finish before this call can return. The action
//! executor's unsandboxed-command and cache trust limitations still apply.
//!
//! `bazel test` adds its test actions to the same graph as leaves
//! ([`Leaves`], [`execute_graph_with_leaves`]): a test action has no outputs
//! another action reads, and runs as soon as the actions producing its
//! inputs have finished, on the same pool of `jobs` workers, as Bazel's
//! TestRunnerActions are actions of the one execution graph
//! (skyframe/BuildDriverFunction.java:624-633). A failure interrupts the
//! leaves in flight (the flag they get), like Bazel's interrupt of in-flight
//! actions on a --nokeep_going error (AbstractQueueVisitor.awaitTermination).

use crate::action::{self, Action, ActionResult, Transform};
use crate::shared_path::SharedPath;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Dependency {
    /// The producer's action id, interned (crate::interner): an action's
    /// outputs are inputs of many actions.
    pub action_id: crate::interner::IStr,
    /// Index in the producer's declared outputs, not sorted output order.
    pub output_index: usize,
    #[serde(with = "crate::snapshot::codec::path")]
    pub destination: SharedPath,
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
    /// value, PLAN-L2 §1b: encodable only with a portable description,
    /// crate::snapshot::mapper).
    pub fn uses_map_each(&self) -> bool {
        match &*self.0 {
            DeferredSpec::CommandLine { spec, .. } => spec.deferred_map_each(),
            DeferredSpec::WriteArgs { snapshot, .. } => {
                crate::command_line::has_deferred_map_each(&snapshot.groups)
            }
        }
    }

    /// Every Starlark `map_each` the expansion calls, in order.
    pub(crate) fn mappers(&self) -> Vec<&crate::command_line::Mapper> {
        match &*self.0 {
            DeferredSpec::CommandLine { spec, .. } => spec.deferred_mappers().collect(),
            DeferredSpec::WriteArgs { snapshot, .. } => {
                crate::command_line::deferred_mappers(&snapshot.groups).collect()
            }
        }
    }

    /// Whether the expansion calls a `map_each` without a portable
    /// description: the request cannot be encoded (the snapshot blocker
    /// `map-each-deferred`).
    pub fn uses_unportable_map_each(&self) -> bool {
        self.mappers()
            .into_iter()
            .any(|mapper| mapper.portable().is_none())
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
                action.arguments = crate::interner::IStr::all(arguments);
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
    /// Where the groups are listed among the action's own inputs: `None`
    /// after all of them (a tool's runfiles trees), else the shared runs
    /// (the groups that are not runfiles trees, [`InputGroup::runfiles`])
    /// after the first [`GroupsAt::inputs`] own source inputs and the first
    /// [`GroupsAt::dependencies`] own generated inputs (a shared run in the
    /// middle of a C++ compile's inputs: the headers of its compilation
    /// context; the Python runtime in a PyCompile's), and the runfiles
    /// trees still after all of them (format 11). Groups listed in the
    /// middle have no tree, symlink or constant-metadata inputs and no
    /// empty files ([`InputGroup::listable_between`]).
    pub groups_at: Option<GroupsAt>,
}

/// A source input: (path, destination).
type SourceInput = (SharedPath, SharedPath);

/// Which of a node's groups a listing takes ([`PlannedAction::split_groups`]).
#[derive(Clone, Copy)]
enum Which {
    All,
    None,
    /// The shared runs (not runfiles trees).
    Runs,
    /// The runfiles trees.
    Trees,
}

/// A node's groups listed at one place ([`PlannedAction::groups_at`]).
struct GroupsBetween<'a> {
    groups: &'a [Arc<InputGroup>],
    which: Which,
}

impl<'a> GroupsBetween<'a> {
    fn iter(&self) -> impl Iterator<Item = &'a Arc<InputGroup>> + use<'a> {
        let which = self.which;
        self.groups.iter().filter(move |group| match which {
            Which::All => true,
            Which::None => false,
            Which::Runs => !group.runfiles,
            Which::Trees => group.runfiles,
        })
    }
}

/// [`PlannedAction::groups_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupsAt {
    pub inputs: u32,
    pub dependencies: u32,
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
///
/// A planned request encodes each group once, in a table its holders
/// index (crate::snapshot::codec); there is deliberately no serde derive:
/// with serde's `rc` feature an `Arc<InputGroup>` would serialize once per
/// holder.
#[derive(Clone, Debug)]
pub struct InputGroup {
    /// The directory every destination lies below (the runfiles tree).
    pub root: PathBuf,
    /// A tool's runfiles tree (an input of its own in aquery, the way
    /// Bazel's spawns hold the one runfiles tree artifact); otherwise the
    /// part below `root` of a run of inputs many actions list alike (a C++
    /// toolchain's files, crate::engine SharedInputs), listed in aquery.
    pub runfiles: bool,
    /// Source inputs (path, destination), in order.
    pub inputs: Vec<(SharedPath, SharedPath)>,
    /// Generated inputs, in order.
    pub dependencies: Vec<Dependency>,
    /// Destinations of tree artifacts (crate::action::ActionExtra::tree_inputs).
    pub tree_inputs: Vec<PathBuf>,
    /// Destinations of unresolved symlink artifacts
    /// (crate::action::ActionExtra::symlink_inputs).
    pub symlink_inputs: Vec<PathBuf>,
    /// Destinations of constant-metadata inputs
    /// (crate::action::ActionExtra::constant_metadata).
    pub constant_metadata: Vec<PathBuf>,
    /// Empty files (runfiles empty files): parameter files without content.
    pub empty_files: Vec<PathBuf>,
    /// Every source input's path, also the ones execroot planting drops,
    /// first occurrences in order (the engine's missing-input and
    /// source-directory checks), shared with the inputs' artifacts.
    pub sources: Vec<SharedPath>,
    /// Every destination (also of the source inputs planting drops) is a
    /// relative normal path strictly below `root`, none is or contains
    /// another ([`group_namespace_valid`]), and every tree input is a
    /// listed input ([`tree_inputs_listed`]). Never encoded: a decoded
    /// group recomputes it ([`InputGroup::listed_valid`]), as
    /// [`validate_node`] trusts it.
    pub valid: bool,
}

impl InputGroup {
    /// Whether the group lists only source and generated inputs, so it can
    /// be listed in the middle of an action's own ones
    /// ([`PlannedAction::groups_at`]).
    pub fn listable_between(&self) -> bool {
        self.tree_inputs.is_empty()
            && self.symlink_inputs.is_empty()
            && self.constant_metadata.is_empty()
            && self.empty_files.is_empty()
    }

    /// [`InputGroup::valid`] recomputed from what the group lists: the
    /// namespace of its inputs', dependencies' and empty files'
    /// destinations, and its tree inputs listed. A decoded group's
    /// (crate::snapshot::codec). The planner's flag also covers the
    /// destinations of the source inputs planting dropped, a superset: a
    /// group planned valid is valid here too, and a group that is not
    /// valid here fails the listed validation of every holder.
    pub fn listed_valid(&self) -> bool {
        group_namespace_valid(
            &self.root,
            self.inputs
                .iter()
                .map(|(_, destination)| destination.as_path())
                .chain(
                    self.dependencies
                        .iter()
                        .map(|dependency| dependency.destination.as_path()),
                )
                .chain(self.empty_files.iter().map(PathBuf::as_path))
                .collect(),
        ) && tree_inputs_listed(&self.inputs, &self.dependencies, &self.tree_inputs)
            && tree_inputs_listed(&self.inputs, &self.dependencies, &self.symlink_inputs)
    }
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

/// The rest of [`InputGroup::valid`]: every tree input of a group is a
/// listed input (a source input or a generated one), as validate_structure
/// requires of the executed action.
pub fn tree_inputs_listed(
    inputs: &[(SharedPath, SharedPath)],
    dependencies: &[Dependency],
    tree_inputs: &[PathBuf],
) -> bool {
    if tree_inputs.is_empty() {
        return true;
    }
    let listed: std::collections::HashSet<&Path> = inputs
        .iter()
        .map(|(_, destination)| destination.as_path())
        .chain(dependencies.iter().map(|d| d.destination.as_path()))
        .collect();
    tree_inputs
        .iter()
        .all(|tree| listed.contains(tree.as_path()))
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
        .map(|(_, destination)| destination.as_path())
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination.as_path()),
        )
        .chain(action.outputs.iter().map(PathBuf::as_path))
        .chain(
            action
                .extra
                .param_files
                .iter()
                .map(|(path, _)| path.as_path()),
        );
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
            groups_at: None,
        }
    }

    /// The own source inputs listed before the groups and after them.
    fn own_inputs(&self) -> (&[SourceInput], &[SourceInput]) {
        let inputs = &self.action.inputs;
        match self.groups_at {
            Some(at) => inputs.split_at((at.inputs as usize).min(inputs.len())),
            None => (inputs, &[]),
        }
    }

    /// The own generated inputs listed before the groups and after them.
    fn own_dependencies(&self) -> (&[Dependency], &[Dependency]) {
        match self.groups_at {
            Some(at) => self
                .dependencies
                .split_at((at.dependencies as usize).min(self.dependencies.len())),
            None => (&self.dependencies, &[]),
        }
    }

    /// The groups listed between the own inputs and the ones listed after
    /// all of them ([`PlannedAction::groups_at`]).
    fn split_groups(&self) -> (GroupsBetween<'_>, GroupsBetween<'_>) {
        let groups = &self.groups;
        match self.groups_at {
            // All of them after the own inputs (which `own_inputs` lists
            // entirely before them).
            None => (
                GroupsBetween {
                    groups,
                    which: Which::All,
                },
                GroupsBetween {
                    groups,
                    which: Which::None,
                },
            ),
            Some(_) => (
                GroupsBetween {
                    groups,
                    which: Which::Runs,
                },
                GroupsBetween {
                    groups,
                    which: Which::Trees,
                },
            ),
        }
    }

    /// The node's source inputs: its action's, each group's at
    /// [`PlannedAction::groups_at`] (after its own by default).
    pub fn inputs(&self) -> impl Iterator<Item = &(SharedPath, SharedPath)> {
        let (before, after) = self.own_inputs();
        let (between, last) = self.split_groups();
        before
            .iter()
            .chain(between.iter().flat_map(|group| group.inputs.iter()))
            .chain(after)
            .chain(last.iter().flat_map(|group| group.inputs.iter()))
    }

    /// The runfiles support actions (input manifest and tree, by id) of
    /// each tool whose runfiles tree this node stages, which the build
    /// builds with the node without staging them (B49): Bazel's spawn
    /// holds the tool's runfiles tree artifact, whose RunfilesTreeAction
    /// takes the tree's MANIFEST (SymlinkTreeAction over the
    /// `.runfiles_manifest`; RunfilesSupport.createRunfilesTreeArtifactAction,
    /// analysis/RunfilesSupport.java:467-490), while the sandbox stages
    /// the mapping only (SpawnInputExpander). A tree's support is named by
    /// its repository mapping manifest's producer (`O#repo_mapping`, an
    /// entry of every tree), the support's actions being `O#runfiles_manifest`
    /// and `O#runfiles_tree` (planned with links and manifests only; a
    /// caller keeps the ids that are planned).
    pub fn tool_runfiles_support_ids(&self) -> impl Iterator<Item = String> + '_ {
        self.groups
            .iter()
            .filter(|group| group.runfiles)
            .filter_map(|group| {
                group
                    .dependencies
                    .iter()
                    .find_map(|dependency| dependency.action_id.strip_suffix("#repo_mapping"))
            })
            .flat_map(|owner| {
                [
                    format!("{owner}#runfiles_manifest"),
                    format!("{owner}#runfiles_tree"),
                ]
            })
    }

    /// The node's generated inputs: its own, each group's at
    /// [`PlannedAction::groups_at`] (after its own by default).
    pub fn all_dependencies(&self) -> impl Iterator<Item = &Dependency> {
        let (before, after) = self.own_dependencies();
        let (between, last) = self.split_groups();
        before
            .iter()
            .chain(between.iter().flat_map(|group| group.dependencies.iter()))
            .chain(after)
            .chain(last.iter().flat_map(|group| group.dependencies.iter()))
    }

    /// The node with its shared runs listed in its action and dependencies
    /// and its runfiles trees still shared (after all of its own inputs,
    /// [`PlannedAction::groups_at`]): what aquery reports, a runfiles tree
    /// being one input of its own there.
    pub fn runs_listed(&self) -> PlannedAction {
        let (runs, trees): (Vec<_>, Vec<_>) = self
            .groups
            .iter()
            .cloned()
            .partition(|group| !group.runfiles);
        if runs.is_empty() {
            return self.clone();
        }
        let mut node = PlannedAction {
            action: self.action.clone(),
            dependencies: Vec::new(),
            deferred: self.deferred.clone(),
            groups: runs,
            groups_at: self.groups_at,
        };
        node.dependencies = {
            let (before, after) = self.own_dependencies();
            before
                .iter()
                .chain(
                    node.groups
                        .iter()
                        .flat_map(|group| group.dependencies.iter()),
                )
                .chain(after)
                .cloned()
                .collect()
        };
        let runs = std::mem::take(&mut node.groups);
        list_groups(&mut node.action, &runs, node.groups_at.take());
        node.groups = trees;
        node
    }

    /// The node with every group listed in its action and dependencies:
    /// exactly the node planning lists without sharing.
    pub fn flattened(&self) -> PlannedAction {
        let mut node = PlannedAction {
            action: self.action.clone(),
            dependencies: self.all_dependencies().cloned().collect(),
            deferred: self.deferred.clone(),
            groups: Vec::new(),
            groups_at: None,
        };
        list_groups(&mut node.action, &self.groups, self.groups_at);
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
/// and empty files in `action`, after its own (the source inputs after the
/// first `at.inputs` of its own, [`PlannedAction::groups_at`]).
fn list_groups(action: &mut Action, groups: &[Arc<InputGroup>], at: Option<GroupsAt>) {
    if let Some(at) = at {
        // The shared runs at the position, the runfiles trees after all.
        let position = (at.inputs as usize).min(action.inputs.len());
        action.inputs.splice(
            position..position,
            groups
                .iter()
                .filter(|group| !group.runfiles)
                .flat_map(|group| group.inputs.iter().cloned()),
        );
    }
    for group in groups {
        if at.is_none() || group.runfiles {
            action.inputs.extend(group.inputs.iter().cloned());
        }
        action
            .extra
            .tree_inputs
            .extend(group.tree_inputs.iter().cloned());
        action
            .extra
            .constant_metadata
            .extend(group.constant_metadata.iter().cloned());
        action
            .extra
            .symlink_inputs
            .extend(group.symlink_inputs.iter().cloned());
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
/// That shortcut holds because every check of validate_structure is local
/// to the paths of the own part or of one group's namespace; under
/// `BAZEL_RS_VERIFY_ARTIFACTS=1` (and in debug builds) a node accepted by
/// it is also checked listed, and a different result is an internal error
/// ([`verify_listing`]), so a later non-local check cannot silently
/// diverge.
pub fn validate_node(node: &PlannedAction) -> Result<(), String> {
    validate_node_checked(node, verify_listing())
}

/// Whether [`validate_node`] checks the nodes its shortcut accepts in
/// their listed form too: in debug builds, and in release builds under
/// `BAZEL_RS_VERIFY_ARTIFACTS=1` (read once per process), the switch of
/// crate::engine's artifact-index invariant checks.
fn verify_listing() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        cfg!(debug_assertions)
            || std::env::var_os("BAZEL_RS_VERIFY_ARTIFACTS").is_some_and(|value| value == "1")
    });
    *VERIFY
}

/// [`validate_node`]; `verify` also checks a node accepted without listing
/// its groups in its listed form and panics (an internal error) when that
/// fails.
fn validate_node_checked(node: &PlannedAction, verify: bool) -> Result<(), String> {
    let mut synthetic = node.action.clone();
    synthetic
        .inputs
        .extend(node.dependencies.iter().map(generated_input));
    if node.groups.is_empty() {
        return action::validate_structure(&synthetic);
    }
    if groups_apart(&synthetic, &node.groups) && action::validate_structure(&synthetic).is_ok() {
        if verify && let Err(error) = validate_listed(node) {
            panic!(
                "internal error (BAZEL_RS_VERIFY_ARTIFACTS): action '{}' validates with its shared input groups apart, but its listed form fails: {error}",
                node.action.id
            );
        }
        return Ok(());
    }
    validate_listed(node)
}

/// `action::validate_structure` of a node's listed form: its action with
/// every group listed and every generated input (own, then the groups').
fn validate_listed(node: &PlannedAction) -> Result<(), String> {
    let mut listed = node.action.clone();
    list_groups(&mut listed, &node.groups, node.groups_at);
    listed
        .inputs
        .extend(node.all_dependencies().map(generated_input));
    action::validate_structure(&listed)
}

/// A generated input in [`validate_node`]'s synthetic actions.
fn generated_input(dependency: &Dependency) -> (SharedPath, SharedPath) {
    static SOURCE: std::sync::LazyLock<SharedPath> =
        std::sync::LazyLock::new(|| SharedPath::from(GENERATED_SOURCE));
    (SharedPath::clone(&SOURCE), dependency.destination.clone())
}

struct Graph {
    /// The nodes in planning order (taken when dispatched), by id: indexing
    /// moves no node.
    nodes: Vec<Option<PlannedAction>>,
    index: BTreeMap<String, usize>,
    remaining: BTreeMap<String, usize>,
    consumers: BTreeMap<String, Vec<String>>,
    /// The ready actions, the most urgent first ([`Graph::ready_key`]).
    ready: BTreeSet<(std::cmp::Reverse<u32>, String)>,
    /// Per node (by planning position): the number of actions on the
    /// longest chain from it to an action nothing consumes, itself
    /// included; all 0 with one job ([`Graph::ready_key`]).
    levels: Vec<u32>,
}

impl Graph {
    /// `--keep_going`: `id` failed. Every action above it never runs
    /// (`doomed`), except a runfiles manifest or tree, which does not wait
    /// for the runfiles that failed ([`tolerates_failed`], B50).
    fn failed(&mut self, id: &str, doomed: &mut BTreeSet<String>) {
        let mut todo = vec![id.to_owned()];
        doomed.insert(id.to_owned());
        while let Some(id) = todo.pop() {
            let Some(children) = self.consumers.get(&id) else {
                continue;
            };
            for child in children.clone() {
                if doomed.contains(&child) {
                    continue;
                }
                let tolerates = self.nodes[self.index[&child]]
                    .as_ref()
                    .is_some_and(|node| tolerates_failed(node, &id));
                if tolerates {
                    let count = self.remaining.get_mut(&child).expect("known consumer");
                    *count -= 1;
                    if *count == 0 {
                        let key = self.ready_key(&child);
                        self.ready.insert(key);
                    }
                } else {
                    doomed.insert(child.clone());
                    todo.push(child);
                }
            }
        }
    }

    /// The ready order: the action heading the longest chain of dependent
    /// actions first, then by id. A ready action on the critical path (a
    /// toolchain builder or standard library that many compilations wait
    /// for) starts before a ready leaf of a short chain: with ids alone it
    /// waited behind every ready C++ compilation of bazel-gazelle (17 s;
    /// docs/OSS_CORPUS.md, P2). Bazel's evaluator orders ready work by
    /// depth likewise (PriorityTracker). With one job the order cannot
    /// change the wall time: ids alone, as before.
    fn ready_key(&self, id: &str) -> (std::cmp::Reverse<u32>, String) {
        let level = self
            .index
            .get(id)
            .map_or(0, |&position| self.levels[position]);
        (std::cmp::Reverse(level), id.to_owned())
    }
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
                .get(dependency.action_id.as_str())
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
                .entry(dependency.action_id.to_string())
                .or_default()
                .push(id.clone());
            count += 1;
        }
        remaining.insert(id.clone(), count);
    }
    let ready: BTreeSet<String> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    // Kahn's algorithm avoids recursion even for very deep generated graphs.
    // Multiple outputs from one producer are counted/decremented separately.
    let mut pending = remaining.clone();
    let mut visit = ready.clone();
    let mut visited = 0;
    let mut topological: Vec<usize> = Vec::with_capacity(index.len());
    while let Some(id) = visit.pop_first() {
        visited += 1;
        topological.push(index[&id]);
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
    // The longest chain below each node, consumers before producers.
    let mut levels = vec![0u32; nodes.len()];
    if jobs > 1 {
        for &position in topological.iter().rev() {
            let below = consumers
                .get(&nodes[position].action.id)
                .into_iter()
                .flatten()
                .map(|consumer| levels[index[consumer]])
                .max()
                .unwrap_or(0);
            levels[position] = below.saturating_add(1);
        }
    }
    let mut graph = Graph {
        nodes: nodes.into_iter().map(Some).collect(),
        index,
        remaining,
        consumers,
        ready: BTreeSet::new(),
        levels,
    };
    graph.ready = ready.iter().map(|id| graph.ready_key(id)).collect();
    Ok(graph)
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
    execute_graph_with_leaves(cache, nodes, jobs, None)
}

/// [`execute_graph`] with leaf jobs ([`Leaves`]: `bazel test`'s test
/// actions) scheduled in the same graph. The results are the actions' only;
/// a leaf reports through its own closure.
pub fn execute_graph_with_leaves(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    leaves: Option<&Leaves<'_>>,
) -> Result<BTreeMap<String, crate::action::ActionResult>, String> {
    // Process strings cross Bazel's internalToPlatform boundary first: lone
    // analysis-model bytes decode with replacement (bazel_bytes.rs).
    execute_with_leaves(
        cache,
        nodes,
        jobs,
        &|cache: &Path, action: &Action| {
            action::execute(cache, &crate::bazel_bytes::platform_action(action))
        },
        leaves,
    )
}

/// The outcome of a `--keep_going` execution ([`execute_graph_keep_going`]):
/// the results of the actions that ran and succeeded, and the error of each
/// action that failed, by id. An action whose input failed never runs and
/// has neither.
#[derive(Debug, Default)]
pub struct KeepGoingOutcome {
    pub results: BTreeMap<String, ActionResult>,
    pub errors: BTreeMap<String, String>,
}

/// [`execute_graph_with_leaves`] under `--keep_going` (docs/KEEP_GOING.md;
/// Bazel's evaluation with keepGoing, AbstractQueueVisitor without
/// failFast): a failed action stops only what depends on it — its
/// consumers and the leaves reading it never start — and every other
/// action and leaf runs. `Err` only for a graph that does not validate or
/// an infrastructure failure (a worker that cannot be spawned, a panic).
pub fn execute_graph_keep_going(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    leaves: Option<&Leaves<'_>>,
) -> Result<KeepGoingOutcome, String> {
    execute_outcome(
        cache,
        nodes,
        jobs,
        &|cache: &Path, action: &Action| {
            action::execute(cache, &crate::bazel_bytes::platform_action(action))
        },
        leaves,
        true,
    )
}

/// [`execute_graph`] that keeps the failures apart: the build stops at the
/// first failure (no `--keep_going`), the actions that completed keep their
/// results, and each failure is returned by action id.
pub fn execute_graph_outcome(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
) -> Result<KeepGoingOutcome, String> {
    execute_outcome(
        cache,
        nodes,
        jobs,
        &|cache: &Path, action: &Action| {
            action::execute(cache, &crate::bazel_bytes::platform_action(action))
        },
        None,
        false,
    )
}

/// The results a leaf reads: each dependency's id and result, in the order
/// of [`Leaves::dependencies`] (duplicates removed).
pub type LeafInputs = [(String, Arc<ActionResult>)];

/// Leaf jobs of the execution graph (PERF(TEST)): jobs that read the results
/// of planned actions and produce nothing another job reads — `bazel test`'s
/// test actions (TestRunnerAction, scheduled in Bazel's one execution graph,
/// skyframe/BuildDriverFunction.java:624-633 and TestCompletionFunction).
///
/// A leaf is ready once every action it depends on has succeeded; ready
/// leaves are dispatched before ready actions (lowest leaf index first),
/// on the same pool: actions and leaves in flight together never exceed
/// `jobs`, and at most [`Leaves::cap`] leaves run at once
/// (`--local_test_jobs`, ExecutionTool.configureResourceManager). After a
/// failure no leaf starts, and the leaves in flight see their interrupt flag
/// set. The scheduler returns once every dispatched leaf has returned.
pub struct Leaves<'a> {
    /// Per leaf, the ids of the planned actions whose results it reads.
    pub dependencies: Vec<Vec<String>>,
    /// At most this many leaves run at once; 0: only `jobs` bounds them.
    pub cap: usize,
    /// Run leaf `index` with its inputs ([`LeafRun`]).
    pub run: &'a LeafRun<'a>,
}

/// How a leaf runs: `(index, inputs, interrupt)`. The interrupt flag is set
/// when the graph failed: a running leaf stops its work (Bazel interrupts
/// in-flight actions on a --nokeep_going failure). The inputs' results must
/// not be kept beyond the call (the scheduler unwraps them at the end).
pub type LeafRun<'a> = dyn Fn(usize, &LeafInputs, &AtomicBool) + Sync + 'a;

/// Whether `node` still runs under `--keep_going` when its dependency
/// `failed` failed (B50): a runfiles manifest or tree names its runfiles by
/// path only. Bazel's SourceManifestAction has the runfiles' unresolved
/// symlinks as its only inputs (SourceManifestAction.getInputs) and its
/// SymlinkTreeAction the input manifest (and repository mapping), so both
/// run, and leave their files, for a binary whose link failed. An
/// unresolved symlink (read for its text) or the tree's input manifest
/// (read) is still waited for.
fn tolerates_failed(node: &PlannedAction, failed: &str) -> bool {
    let manifest = match &node.action.extra.transform {
        Some(Transform::RunfilesManifest { .. }) => None,
        Some(Transform::RunfilesTree { manifest, .. }) => Some(manifest.as_path()),
        _ => return false,
    };
    node.all_dependencies()
        .filter(|dependency| dependency.action_id == failed)
        .all(|dependency| {
            let destination = dependency.destination.as_path();
            Some(destination) != manifest && !node.action.extra.symlink_inputs.contains(destination)
        })
}

/// One dispatched action: the node and its producers' results in
/// dependency order (shared with, not copied by, the dispatching thread).
struct Job {
    id: String,
    node: PlannedAction,
    /// `None`: a producer that failed under `--keep_going`, which a
    /// runfiles manifest or tree does not wait for ([`tolerates_failed`]).
    producers: Vec<Option<Arc<ActionResult>>>,
}

/// One dispatched unit of work: an action, or a leaf with its inputs.
enum Work {
    Action(Box<Job>),
    Leaf(usize, Vec<(String, Arc<ActionResult>)>),
}

/// A finished job: `None` when it was skipped because a failure had been
/// published before it started.
type Completion = (String, Option<Result<ActionResult, String>>);

/// A finished unit of work: an action's completion, or a leaf's (with the
/// message of a panic it raised).
enum Done {
    Action(Completion),
    Leaf(usize, Option<String>),
}

/// The message of a caught panic.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload")
        .to_owned()
}

/// Run one leaf on a worker; a panic fails the graph.
fn run_leaf(
    leaves: &Leaves<'_>,
    leaf: usize,
    inputs: &LeafInputs,
    failed: &AtomicBool,
) -> Option<String> {
    let outcome = catch_unwind(AssertUnwindSafe(|| (leaves.run)(leaf, inputs, failed)));
    outcome.err().map(|payload| {
        failed.store(true, Ordering::Release);
        format!("worker panicked: {}", panic_message(&*payload))
    })
}

/// Run one job on a worker: its generated inputs (each producer's published
/// output, in dependency order) and tree listings, then the deferred
/// completion and the action itself.
fn run_job<F>(
    job: Job,
    cache: &Path,
    failed: &Arc<AtomicBool>,
    keep_going: bool,
    execute: &F,
) -> Completion
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
        crate::progress::action_done();
        return (id, None);
    }
    // The progress display's (or plain lines') running-action slot (None
    // when both are off).
    let running = crate::progress::Running::action(&node.action);
    // --profile / the build event files (crate::profile, crate::bep): the
    // action's span, dependencies (critical path) and outcome.
    let traced = (crate::profile::recording() || crate::bep::active()).then(|| {
        let dependencies: Vec<String> = node
            .all_dependencies()
            .map(|dependency| dependency.action_id.to_string())
            .collect();
        (
            std::time::Instant::now(),
            std::time::SystemTime::now(),
            dependencies,
        )
    });
    let result = catch_unwind(AssertUnwindSafe(|| {
        // The groups' inputs are listed in the action now, for this one
        // execution (PA-1): their sources after the action's own, their
        // generated inputs after its own generated ones.
        let listed: Vec<Dependency> = if node.groups.is_empty() {
            std::mem::take(&mut node.dependencies)
        } else {
            node.all_dependencies().cloned().collect()
        };
        let groups = std::mem::take(&mut node.groups);
        list_groups(&mut node.action, &groups, node.groups_at.take());
        node.dependencies.clear();
        let mut listings = TreeListings::new();
        for (dependency, producer) in listed.iter().zip(&producers) {
            let Some(producer) = producer else {
                // A failed producer's output (B50): named where the output
                // tree would hold it (paths only, never read).
                if let Some(Transform::RunfilesTree { executable, .. }) =
                    &mut node.action.extra.transform
                    && executable.as_deref() == Some(dependency.destination.as_path())
                {
                    *executable = None;
                }
                node.action.inputs.push((
                    crate::output_tree::execution_root(cache)
                        .join(&dependency.destination)
                        .into(),
                    dependency.destination.clone(),
                ));
                continue;
            };
            if let Some(listing) = producer.trees.get(&dependency.output_index) {
                listings.insert(dependency.destination.to_path_buf(), listing.clone());
            }
            node.action.inputs.push((
                producer.outputs[dependency.output_index].clone().into(),
                dependency.destination.clone(),
            ));
        }
        if let Some(deferred) = &node.deferred {
            deferred.apply(&listings, &mut node.action)?;
            // The deferred command line replaced the parameter files; the
            // groups' empty files stay.
            list_empty_files(&mut node.action, &groups);
        }
        let clock = std::time::Instant::now();
        if node.action.extra.java_compile.is_some() {
            node.action.execution_cancel = Some(Arc::clone(failed));
        }
        let mut result = execute(cache, &node.action)?;
        // The critical path through this action (CriticalPathComputer): a
        // cache hit adds nothing of its own.
        let own = if result.cache_hit {
            std::time::Duration::ZERO
        } else {
            clock.elapsed()
        };
        result.critical = own
            + producers
                .iter()
                .flatten()
                .map(|producer| producer.critical)
                .max()
                .unwrap_or_default();
        if result.outputs.len() != node.action.outputs.len() {
            return Err(format!(
                "executor returned {} outputs, expected {}",
                result.outputs.len(),
                node.action.outputs.len()
            ));
        }
        Ok(result)
    }))
    .unwrap_or_else(|payload| Err(format!("worker panicked: {}", panic_message(&*payload))));
    if let Some((started, start_time, dependencies)) = traced {
        let cache_hit = result.as_ref().is_ok_and(|result| result.cache_hit);
        crate::profile::action_done(&node.action, dependencies, started, cache_hit);
        crate::bep::action_done(
            &node.action,
            &result,
            start_time,
            std::time::SystemTime::now(),
        );
    }
    drop(running);
    crate::progress::action_done();
    if result.is_err() && !keep_going {
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
// many at once: ready actions by `Graph::ready_key` (the head of the longest
// chain first; in id order with one job), at most `jobs` in flight, none
// after a failure is observed.
#[cfg(test)]
fn execute_with<F>(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    execute: &F,
) -> Result<BTreeMap<String, ActionResult>, String>
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    execute_with_leaves(cache, nodes, jobs, execute, None)
}

/// The leaves' scheduling state ([`Leaves`]): unfinished dependencies per
/// leaf, the leaves waiting on each action, the ready leaves.
struct LeafState {
    dependencies: Vec<Vec<String>>,
    remaining: Vec<usize>,
    consumers: HashMap<String, Vec<usize>>,
    ready: BTreeSet<usize>,
    cap: usize,
    running: usize,
}

impl LeafState {
    /// The leaves' dependencies deduplicated and checked against the graph
    /// before any action runs (a leaf of an unknown action is an internal
    /// error of the caller).
    fn new(leaves: Option<&Leaves<'_>>, graph: &Graph, keep_going: bool) -> Result<Self, String> {
        let mut state = LeafState {
            dependencies: Vec::new(),
            remaining: Vec::new(),
            consumers: HashMap::new(),
            ready: BTreeSet::new(),
            cap: usize::MAX,
            running: 0,
        };
        let Some(leaves) = leaves else {
            return Ok(state);
        };
        if leaves.cap > 0 {
            state.cap = leaves.cap;
        }
        for (leaf, dependencies) in leaves.dependencies.iter().enumerate() {
            let mut unique: Vec<String> = Vec::new();
            // --keep_going: an action planning dropped (its inputs failed,
            // crate::engine) blocks the leaf for good.
            let mut blocked = false;
            for id in dependencies {
                if !graph.index.contains_key(id) {
                    if keep_going {
                        blocked = true;
                        continue;
                    }
                    return Err(format!(
                        "internal error: test action {leaf} depends on unknown action '{id}'"
                    ));
                }
                if !unique.contains(id) {
                    unique.push(id.clone());
                }
            }
            for id in &unique {
                state.consumers.entry(id.clone()).or_default().push(leaf);
            }
            if unique.is_empty() && !blocked {
                state.ready.insert(leaf);
            }
            state.remaining.push(unique.len() + usize::from(blocked));
            state.dependencies.push(unique);
        }
        Ok(state)
    }

    /// An action succeeded: the leaves it completes become ready.
    fn completed(&mut self, id: &str) {
        if let Some(leaves) = self.consumers.get(id) {
            for &leaf in leaves {
                self.remaining[leaf] -= 1;
                if self.remaining[leaf] == 0 {
                    self.ready.insert(leaf);
                }
            }
        }
    }

    /// The next leaf to dispatch, if one is ready and the cap allows it.
    fn next(&mut self) -> Option<usize> {
        if self.running >= self.cap {
            return None;
        }
        self.ready.pop_first()
    }
}

/// [`execute_with`] with leaves: ready leaves go first, and actions and
/// leaves in flight together never exceed `jobs`.
fn execute_with_leaves<F>(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    execute: &F,
    leaves: Option<&Leaves<'_>>,
) -> Result<BTreeMap<String, ActionResult>, String>
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    let KeepGoingOutcome { results, errors } =
        execute_outcome(cache, nodes, jobs, execute, leaves, false)?;
    if errors.is_empty() {
        Ok(results)
    } else {
        Err(errors
            .into_iter()
            .map(|(id, error)| format!("action '{id}': {error}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// The scheduler: [`execute_with_leaves`], and under `keep_going` a failed
/// action does not stop the graph ([`execute_graph_keep_going`]). Errors of
/// infrastructure (spawning a worker, a panic) always stop it and fail the
/// call under `keep_going`.
fn execute_outcome<F>(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    execute: &F,
    leaves: Option<&Leaves<'_>>,
    keep_going: bool,
) -> Result<KeepGoingOutcome, String>
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    let mut graph = validate(nodes, jobs)?;
    // Execution roots of this cache are reused while the graph runs
    // (crate::action::StashScope); the stashed ones go when it ends.
    let _stashes = crate::action::StashScope::enter(cache);
    crate::progress::execution_started(graph.nodes.len());
    // Infrastructure failures (worker spawn, panics) of a keep-going run.
    let mut fatal: Vec<String> = Vec::new();
    let mut lane = LeafState::new(leaves, &graph, keep_going)?;
    let failed = Arc::new(AtomicBool::new(false));
    let mut results: BTreeMap<String, Arc<ActionResult>> = BTreeMap::new();
    let mut errors = BTreeMap::new();
    // --keep_going: the actions that failed or never run because of a
    // failure ([`Graph::failed`]).
    let mut doomed: BTreeSet<String> = BTreeSet::new();
    let (job_sender, job_receiver) = mpsc::channel::<Work>();
    let job_receiver = Mutex::new(job_receiver);
    let (done_sender, done_receiver) = mpsc::channel::<Done>();
    thread::scope(|scope| {
        let mut job_sender = Some(job_sender);
        let mut workers = 0;
        let mut running = BTreeSet::new();
        loop {
            while running.len() + lane.running < jobs && !failed.load(Ordering::Acquire) {
                // A ready leaf first: a test starts as soon as its inputs
                // exist (it holds no action back: nothing reads its outputs).
                let leaf = lane.next();
                let id = match leaf {
                    Some(_) => None,
                    None => match graph.ready.pop_first() {
                        Some((_, id)) => Some(id),
                        None => break,
                    },
                };
                // Every job in flight has a worker of its own.
                if workers == running.len() + lane.running {
                    let done = done_sender.clone();
                    let (receiver, failed) = (&job_receiver, &failed);
                    let spawn = thread::Builder::new().spawn_scoped(scope, move || {
                        loop {
                            let work = {
                                let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
                                receiver.recv()
                            };
                            // Closed: everything was dispatched.
                            let Ok(work) = work else {
                                return;
                            };
                            let finished = match work {
                                Work::Action(job) => {
                                    Done::Action(run_job(*job, cache, failed, keep_going, execute))
                                }
                                Work::Leaf(leaf, inputs) => {
                                    let leaves = leaves.expect("leaf work has leaves");
                                    let panicked = run_leaf(leaves, leaf, &inputs, failed);
                                    // The inputs' results are released before
                                    // the completion: the dispatcher unwraps
                                    // them once every worker is joined.
                                    drop(inputs);
                                    Done::Leaf(leaf, panicked)
                                }
                            };
                            if done.send(finished).is_err() {
                                return;
                            }
                        }
                    });
                    match spawn {
                        Ok(_) => workers += 1,
                        Err(error) => {
                            failed.store(true, Ordering::Release);
                            let key = match (&id, leaf) {
                                (Some(id), _) => id.clone(),
                                (None, leaf) => format!("test action {}", leaf.unwrap_or(0)),
                            };
                            if keep_going {
                                fatal.push(format!("{key}: cannot spawn worker: {error}"));
                            }
                            errors.insert(key, format!("cannot spawn worker: {error}"));
                            continue;
                        }
                    }
                }
                let work = match (id, leaf) {
                    (Some(id), _) => {
                        let node = graph.nodes[graph.index[&id]]
                            .take()
                            .expect("ready node exists");
                        let producers = node
                            .all_dependencies()
                            .map(|dependency| results.get(dependency.action_id.as_str()).cloned())
                            .collect();
                        running.insert(id.clone());
                        Work::Action(Box::new(Job {
                            id,
                            node,
                            producers,
                        }))
                    }
                    (None, Some(leaf)) => {
                        let inputs = lane.dependencies[leaf]
                            .iter()
                            .map(|id| (id.clone(), Arc::clone(&results[id])))
                            .collect();
                        lane.running += 1;
                        Work::Leaf(leaf, inputs)
                    }
                    (None, None) => unreachable!("a job or a leaf was taken"),
                };
                job_sender
                    .as_ref()
                    .expect("dispatching")
                    .send(work)
                    .expect("workers outlive dispatching");
            }
            if running.is_empty() && lane.running == 0 {
                break;
            }
            // Every dispatched job sends one completion, including caught
            // panics and jobs skipped on cancellation.
            match done_receiver.recv().expect("workers send completion") {
                Done::Action((id, outcome)) => {
                    assert!(running.remove(&id), "completion from a running job");
                    match outcome {
                        Some(Ok(result)) => {
                            results.insert(id.clone(), Arc::new(result));
                            if let Some(children) = graph.consumers.get(&id) {
                                for child in children {
                                    let count =
                                        graph.remaining.get_mut(child).expect("known consumer");
                                    *count -= 1;
                                    if *count == 0 {
                                        let key = graph.ready_key(child);
                                        graph.ready.insert(key);
                                    }
                                }
                            }
                            lane.completed(&id);
                        }
                        Some(Err(error)) => {
                            if keep_going && error.starts_with("worker panicked: ") {
                                fatal.push(format!("action '{id}': {error}"));
                                failed.store(true, Ordering::Release);
                            }
                            if keep_going {
                                graph.failed(&id, &mut doomed);
                            }
                            errors.insert(id, error);
                        }
                        None => {}
                    }
                }
                Done::Leaf(leaf, panicked) => {
                    lane.running -= 1;
                    if let Some(error) = panicked {
                        if keep_going {
                            fatal.push(format!("test action {leaf}: {error}"));
                        }
                        errors.insert(format!("test action {leaf}"), error);
                    }
                }
            }
        }
        // Idle workers see the closed channel and exit; the scope joins them.
        job_sender.take();
    });
    if !fatal.is_empty() {
        return Err(fatal.join("\n"));
    }
    Ok(KeepGoingOutcome {
        results: results
            .into_iter()
            .map(|(id, result)| {
                let result = Arc::into_inner(result).expect("workers are joined");
                (id, result)
            })
            .collect(),
        errors,
    })
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
                let path = crate::test_temp::root().join(format!(
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
            groups_at: None,
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
            remote_executed: false,
            runner: None,
            additional_spawns: 0,
            critical: std::time::Duration::ZERO,
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

    /// With several jobs the ready action heading the longest chain starts
    /// first (`Graph::ready_key`); with one job the order stays by id.
    #[test]
    fn the_head_of_the_longest_chain_is_dispatched_first() {
        let graph = || {
            // `z0 -> z1 -> z2 -> z3` and ready leaves `a0`..`a5` (ids
            // before the chain's head).
            let mut graph: Vec<PlannedAction> = (0..6).map(|i| node(&format!("a{i}"))).collect();
            for i in 0..4 {
                let mut link = node(&format!("z{i}"));
                if i > 0 {
                    link.dependencies = vec![dependency(&format!("z{}", i - 1), 0, "in")];
                }
                graph.push(link);
            }
            graph
        };
        for (jobs, first) in [(2, "z0"), (1, "a0")] {
            let order = Mutex::new(Vec::new());
            let worker = |_: &Path, action: &Action| {
                order.lock().unwrap().push(action.id.clone());
                success(action)
            };
            execute_with(Path::new("unused"), graph(), jobs, &worker).unwrap();
            let order = order.into_inner().unwrap();
            assert_eq!(order.len(), 10);
            assert_eq!(order[0], first, "jobs={jobs}: {order:?}");
        }
        let validated = validate(graph(), 4).unwrap();
        let levels: Vec<u32> = validated.levels.clone();
        assert_eq!(levels, vec![1, 1, 1, 1, 1, 1, 4, 3, 2, 1]);
        assert_eq!(validate(graph(), 1).unwrap().levels, vec![0; 10]);
    }

    #[test]
    fn chain_fanout_fanin_and_repeated_graph_cache_hits() {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        fs::write(&source, "hello").unwrap();
        let mut root = node("root");
        root.action.inputs.push((source.into(), "src".into()));
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

    /// --keep_going: the failed action's consumer never starts, every
    /// independent action (also one queued behind the failure) runs.
    #[test]
    fn keep_going_runs_everything_not_depending_on_a_failure() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("dependent-ran");
        let mut bad = node("a-fail");
        bad.action.command = "echo intentional-failure >&2; exit 7".into();
        let mut dependent = node("dependent");
        dependent.dependencies = vec![dependency("a-fail", 0, "input")];
        dependent.action.command = format!("touch '{}'; cat input > out", marker.display());
        let mut after = node("m-after-dependent");
        after.dependencies = vec![dependency("dependent", 0, "input")];
        let queued = node("z-queued");
        let outcome = execute_graph_keep_going(
            &fixture.cache(),
            vec![dependent, after, queued, bad],
            1,
            None,
        )
        .unwrap();
        assert_eq!(outcome.results.keys().collect::<Vec<_>>(), ["z-queued"]);
        assert_eq!(outcome.errors.keys().collect::<Vec<_>>(), ["a-fail"]);
        assert!(outcome.errors["a-fail"].contains("intentional-failure"));
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

    /// PERF(TEST): a leaf runs once its dependencies succeeded, with their
    /// results, and goes before the ready actions (here with one job: the
    /// order of execution is the order of dispatch).
    #[test]
    fn leaves_run_after_their_dependencies_before_ready_actions() {
        let order = Mutex::new(Vec::new());
        let worker = |_: &Path, action: &Action| {
            order.lock().unwrap().push(action.id.clone());
            success(action)
        };
        let mut producer = node("p");
        producer.action.outputs = vec!["first".into(), "second".into()];
        let mut graph = vec![producer];
        graph.extend((1..=3).map(|i| node(&format!("x{i}"))));
        let seen = Mutex::new(Vec::new());
        let run = |leaf: usize, inputs: &LeafInputs, interrupt: &AtomicBool| {
            assert!(!interrupt.load(Ordering::SeqCst));
            order.lock().unwrap().push(format!("leaf{leaf}"));
            seen.lock().unwrap().push(
                inputs
                    .iter()
                    .map(|(id, result)| (id.clone(), result.outputs.clone()))
                    .collect::<Vec<_>>(),
            );
        };
        let leaves = Leaves {
            // Duplicates are one dependency; a leaf without any is ready at once.
            dependencies: vec![vec!["p".into(), "p".into()], vec!["x3".into()], vec![]],
            cap: 0,
            run: &run,
        };
        let results =
            execute_with_leaves(Path::new("unused"), graph, 1, &worker, Some(&leaves)).unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(
            order.into_inner().unwrap(),
            ["leaf2", "p", "leaf0", "x1", "x2", "x3", "leaf1"]
        );
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen[0], []);
        assert_eq!(
            seen[1],
            [(
                "p".to_owned(),
                vec![PathBuf::from("first"), "second".into()]
            )]
        );
        assert_eq!(seen[2], [("x3".to_owned(), vec![PathBuf::from("out")])]);
    }

    /// Actions and leaves in flight together never exceed `jobs`, and leaves
    /// never exceed their cap (`--local_test_jobs`). The test releases the
    /// held jobs one at a time, oldest first, and waits for the start the
    /// scheduler must make next (a leaf before an action, none when the
    /// only work left is leaves above the cap) — no timing assumption.
    #[test]
    fn leaves_share_the_jobs_and_respect_their_cap() {
        let active = AtomicUsize::new(0);
        let active_leaves = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let maximum_leaves = AtomicUsize::new(0);
        // Each start sends whether it is a leaf and its own release.
        let (started_tx, started_rx) = mpsc::channel::<(bool, mpsc::Sender<()>)>();
        let hold = |leaf: bool| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(count, Ordering::SeqCst);
            if leaf {
                let count = active_leaves.fetch_add(1, Ordering::SeqCst) + 1;
                maximum_leaves.fetch_max(count, Ordering::SeqCst);
            }
            let (release_tx, release_rx) = mpsc::channel();
            started_tx.send((leaf, release_tx)).unwrap();
            release_rx.recv_timeout(TIMEOUT).unwrap();
            if leaf {
                active_leaves.fetch_sub(1, Ordering::SeqCst);
            }
            active.fetch_sub(1, Ordering::SeqCst);
        };
        let worker = |_: &Path, action: &Action| {
            hold(false);
            success(action)
        };
        let run = |_: usize, _: &LeafInputs, _: &AtomicBool| hold(true);
        let leaves = Leaves {
            dependencies: vec![vec![]; 5],
            cap: 2,
            run: &run,
        };
        thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let graph = (0..5).map(|i| node(&format!("node-{i}"))).collect();
                execute_with_leaves(Path::new("unused"), graph, 3, &worker, Some(&leaves))
            });
            // Saturated at once: two leaves (the cap) and one action.
            let mut held: std::collections::VecDeque<(bool, mpsc::Sender<()>)> = (0..3)
                .map(|_| started_rx.recv_timeout(TIMEOUT).unwrap())
                .collect();
            assert_eq!(held.iter().filter(|(leaf, _)| *leaf).count(), 2);
            assert_eq!(active.load(Ordering::SeqCst), 3);
            let (mut leaves_left, mut actions_left) = (3, 4);
            let mut starts = 3;
            while let Some((leaf, release)) = held.pop_front() {
                release.send(()).unwrap();
                let leaves_held = held.iter().filter(|(leaf, _)| *leaf).count();
                // The freed job goes to a ready leaf under the cap first,
                // then to an action; nothing starts when neither fits.
                let next = if leaves_left > 0 && leaves_held < 2 {
                    Some(true)
                } else if actions_left > 0 {
                    Some(false)
                } else {
                    None
                };
                let Some(expected) = next else { continue };
                let (started, release) = started_rx.recv_timeout(TIMEOUT).unwrap();
                assert_eq!(started, expected, "after releasing a leaf: {leaf}");
                if started {
                    leaves_left -= 1;
                } else {
                    actions_left -= 1;
                }
                starts += 1;
                held.push_back((started, release));
            }
            assert_eq!(handle.join().unwrap().unwrap().len(), 5);
            assert_eq!((starts, leaves_left, actions_left), (10, 0, 0));
            assert!(started_rx.try_recv().is_err());
        });
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
        assert_eq!(maximum_leaves.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// A failed action interrupts the leaves in flight (their flag is set and
    /// the scheduler waits for them) and no other leaf or action starts.
    #[test]
    fn a_failure_interrupts_running_leaves_and_starts_no_more() {
        let (leaf_started_tx, leaf_started_rx) = mpsc::channel();
        let leaf_started_rx = Mutex::new(leaf_started_rx);
        let interrupted = AtomicBool::new(false);
        let worker = |_: &Path, action: &Action| match action.id.as_str() {
            "a-fail" => {
                leaf_started_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(TIMEOUT)
                    .unwrap();
                Err("intentional failure".into())
            }
            other => panic!("unexpected action {other} after the failure"),
        };
        let run = |leaf: usize, _: &LeafInputs, interrupt: &AtomicBool| {
            assert_eq!(leaf, 0, "a leaf started after the failure");
            leaf_started_tx.send(()).unwrap();
            let start = std::time::Instant::now();
            while !interrupt.load(Ordering::SeqCst) {
                assert!(start.elapsed() < TIMEOUT, "never interrupted");
                thread::sleep(Duration::from_millis(5));
            }
            interrupted.store(true, Ordering::SeqCst);
        };
        let leaves = Leaves {
            dependencies: vec![vec![], vec!["b-queued".into()]],
            cap: 0,
            run: &run,
        };
        let error = execute_with_leaves(
            Path::new("unused"),
            vec![node("a-fail"), node("b-queued")],
            2,
            &worker,
            Some(&leaves),
        )
        .unwrap_err();
        assert_eq!(error, "action 'a-fail': intentional failure");
        assert!(interrupted.load(Ordering::SeqCst));
    }

    /// A leaf of an action the graph does not plan is an internal error,
    /// found before any action runs; a panicking leaf fails the graph.
    #[test]
    fn unknown_leaf_dependencies_and_panicking_leaves_fail() {
        let calls = AtomicUsize::new(0);
        let worker = |_: &Path, action: &Action| {
            calls.fetch_add(1, Ordering::SeqCst);
            success(action)
        };
        let run = |_: usize, _: &LeafInputs, _: &AtomicBool| {};
        let unknown = Leaves {
            dependencies: vec![vec!["a".into(), "missing".into()]],
            cap: 0,
            run: &run,
        };
        let error = execute_with_leaves(
            Path::new("unused"),
            vec![node("a")],
            2,
            &worker,
            Some(&unknown),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "internal error: test action 0 depends on unknown action 'missing'"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let panics = |_: usize, _: &LeafInputs, _: &AtomicBool| panic!("intentional leaf panic");
        let panicking = Leaves {
            dependencies: vec![vec![]],
            cap: 0,
            run: &panics,
        };
        let mut dependent = node("dependent");
        dependent.dependencies = vec![dependency("a", 0, "input")];
        let error = execute_with_leaves(
            Path::new("unused"),
            vec![node("a"), dependent],
            1,
            &worker,
            Some(&panicking),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "action 'test action 0': worker panicked: intentional leaf panic"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
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
            runfiles: true,
            sources: inputs.iter().map(|(path, _)| path.clone().into()).collect(),
            inputs: inputs
                .into_iter()
                .map(|(path, destination)| (path.into(), destination.into()))
                .collect(),
            dependencies,
            tree_inputs: Vec::new(),
            symlink_inputs: Vec::new(),
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
                PathBuf::from(GENERATED_SOURCE).into(),
                dependency.destination.clone(),
            ));
        }
        action::validate_structure(&synthetic)
    }

    /// Format 11: a node holding a shared run (not a runfiles tree) in the
    /// middle of its own inputs and a tool's runfiles tree lists the run at
    /// `groups_at` and the tree after all of its own inputs — in every
    /// listing (inputs, generated inputs, the flattened node, the dispatch's
    /// `list_groups`) — and aquery's form lists the run alone.
    #[test]
    fn shared_runs_list_in_the_middle_and_runfiles_trees_after_all() {
        let source = Path::new("/nonexistent/source");
        let tool = group(
            "tool.runfiles",
            &[(source, "tool.runfiles/_main/a")],
            &[("tree_gen", "tool.runfiles/_main/gen")],
            &["tool.runfiles/_main/__init__.py"],
        );
        let run = {
            let mut run = (*group(
                "",
                &[(source, "rt/bin/python3x"), (source, "rt/lib/os.py")],
                &[("run_gen", "rt/lib/gen.py")],
                &[],
            ))
            .clone();
            run.runfiles = false;
            Arc::new(run)
        };
        let mut holder = node("pyc");
        holder.action.inputs = vec![
            (source.into(), "rt/bin/python3".into()),
            (source.into(), "pkg/src.py".into()),
            (source.into(), "tools/precompiler.py".into()),
        ];
        holder.dependencies = vec![
            dependency("exe_gen", 0, "bin/exe"),
            dependency("own_gen", 0, "bin/own"),
        ];
        holder.groups = vec![run.clone(), tool.clone()];
        holder.groups_at = Some(GroupsAt {
            inputs: 1,
            dependencies: 1,
        });
        let inputs: Vec<String> = holder
            .inputs()
            .map(|(_, destination)| destination.display().to_string())
            .collect();
        assert_eq!(
            inputs,
            [
                "rt/bin/python3",
                "rt/bin/python3x",
                "rt/lib/os.py",
                "pkg/src.py",
                "tools/precompiler.py",
                "tool.runfiles/_main/a"
            ]
        );
        let generated: Vec<String> = holder
            .all_dependencies()
            .map(|dependency| dependency.destination.display().to_string())
            .collect();
        assert_eq!(
            generated,
            [
                "bin/exe",
                "rt/lib/gen.py",
                "bin/own",
                "tool.runfiles/_main/gen"
            ]
        );
        let flattened = holder.flattened();
        assert!(flattened.groups.is_empty() && flattened.groups_at.is_none());
        assert_eq!(
            flattened
                .action
                .inputs
                .iter()
                .map(|(_, destination)| destination.display().to_string())
                .collect::<Vec<_>>(),
            inputs
        );
        assert_eq!(
            flattened
                .dependencies
                .iter()
                .map(|dependency| dependency.destination.display().to_string())
                .collect::<Vec<_>>(),
            generated
        );
        assert_eq!(
            flattened.action.extra.param_files,
            vec![(PathBuf::from("tool.runfiles/_main/__init__.py"), Vec::new())]
        );
        // aquery's form: the run listed at its place, the tree still held.
        let aquery = holder.runs_listed();
        assert!(aquery.groups_at.is_none());
        assert_eq!(aquery.groups.len(), 1);
        assert!(Arc::ptr_eq(&aquery.groups[0], &tool));
        assert_eq!(
            aquery
                .action
                .inputs
                .iter()
                .map(|(_, destination)| destination.display().to_string())
                .collect::<Vec<_>>(),
            inputs[..5]
        );
        assert_eq!(
            aquery
                .dependencies
                .iter()
                .map(|dependency| dependency.destination.display().to_string())
                .collect::<Vec<_>>(),
            generated[..3]
        );
        assert_eq!(
            aquery.inputs().count() + aquery.all_dependencies().count(),
            inputs.len() + generated.len()
        );
        // The validation lists it (the run's root is every path) like the
        // flattened node.
        assert_eq!(validate_node(&holder), listed_validation(&holder));
        // Without `groups_at`: every group after all of the own inputs.
        let mut after = holder.clone();
        after.groups_at = None;
        assert_eq!(
            after
                .inputs()
                .map(|(_, destination)| destination.display().to_string())
                .collect::<Vec<_>>(),
            [
                "rt/bin/python3",
                "pkg/src.py",
                "tools/precompiler.py",
                "rt/bin/python3x",
                "rt/lib/os.py",
                "tool.runfiles/_main/a"
            ]
        );
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
            // The shortcut alone and verified (B2) agree with the listed form.
            for verify in [false, true] {
                assert_eq!(
                    validate_node_checked(&node, verify),
                    listed_validation(&node),
                    "case {index}, verify {verify}"
                );
            }
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

    /// PA-1 follow-up B2: verification (BAZEL_RS_VERIFY_ARTIFACTS, debug
    /// builds) checks a node the shortcut accepts in its listed form too.
    /// A shortcut that disagrees with the listed check — here a group
    /// claiming a valid namespace its entries do not have, which the
    /// shortcut trusts — is an internal error instead of a silent accept.
    #[test]
    fn verified_validation_catches_a_shortcut_that_disagrees() {
        let source = Path::new("/nonexistent/source");
        let forged = Arc::new(InputGroup {
            root: "tool.runfiles".into(),
            runfiles: true,
            inputs: vec![(source.into(), "tool.runfiles/../escaped".into())],
            dependencies: Vec::new(),
            tree_inputs: Vec::new(),
            symlink_inputs: Vec::new(),
            constant_metadata: Vec::new(),
            empty_files: Vec::new(),
            sources: vec![source.into()],
            valid: true,
        });
        let mut consumer = node("consumer");
        consumer.groups = vec![forged];
        assert!(groups_apart(&consumer.action, &consumer.groups));
        assert!(
            listed_validation(&consumer)
                .unwrap_err()
                .contains("unsafe mapped path 'tool.runfiles/../escaped'")
        );
        assert_eq!(validate_node_checked(&consumer, false), Ok(()));
        let verified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_node_checked(&consumer, true)
        }))
        .unwrap_err();
        let message = verified
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.starts_with(
                "internal error (BAZEL_RS_VERIFY_ARTIFACTS): action 'consumer' validates with its shared input groups apart, but its listed form fails: "
            ) && message.contains("unsafe mapped path 'tool.runfiles/../escaped'"),
            "{message}"
        );
        // validate_node itself verifies in debug builds.
        if cfg!(debug_assertions) {
            let validated =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| validate_node(&consumer)));
            assert!(validated.is_err());
        }
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
