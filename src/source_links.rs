//! Physical resolution of a repository-relative source path through symlinks.
//!
//! Bazel follows symlinks for source artifacts (FileValue resolution), so a
//! source label such as `//:link.txt` or `//:dirlink/file` denotes the content
//! of the link's referent. Native resolution follows links one component at a
//! time, relative targets from the link's directory and absolute targets only
//! when they lexically name the canonical repository root. A link revisited
//! with the same remaining path is reported as Bazel's `circular symlinks
//! detected` cycle; otherwise at most 40 hops are followed. A resolution that would leave the repository root is an
//! explicit unsupported error: unlike Bazel, native source reads never follow
//! a link outside the repository that owns the label. The resolved path has no
//! symlink components, so downstream input readers keep their no-symlink
//! checks. Concurrent hostile mutation of the tree is outside this model.

use crate::observed_fs;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_LINK_HOPS: usize = 40;

/// Outcome of resolving a source path that exists at least as a link.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The symlink-free physical path (a regular file, a directory or another
    /// non-link object; callers check the kind).
    Resolved(PathBuf),
    /// Resolution reached a missing component (a dangling link, or a path
    /// through a non-directory). The returned path is inside the root and does
    /// not exist, so it can stand for a missing source input, as Bazel treats a
    /// dangling source symlink as a missing input file.
    Missing(PathBuf),
}

fn push_front(pending: &mut VecDeque<OsString>, path: &Path) {
    for component in path.components().rev() {
        match component {
            Component::Normal(part) => pending.push_front(part.to_owned()),
            Component::ParentDir => pending.push_front("..".into()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
}

/// Bazel's symlink-cycle report (AbstractFileChainUniquenessFunction.java:45-52
/// with FileSymlinkCycleUniquenessFunction.java:64-82): the cycle's link paths,
/// rotated to start at the smallest path (`canonicalize`, :63-77).
fn cycle_message<'a>(cycle: impl Iterator<Item = &'a Path>) -> String {
    let mut links: Vec<String> = cycle.map(|link| link.display().to_string()).collect();
    let start = (0..links.len())
        .min_by(|a, b| links[*a].cmp(&links[*b]))
        .unwrap_or(0);
    links.rotate_left(start);
    format!(
        "circular symlinks detected\n[start of symlink cycle]\n{}\n[end of symlink cycle]",
        links.join("\n")
    )
}

/// Resolve `relative` (normal components only) below `root`.
pub fn resolve_in_root(root: &Path, relative: &Path) -> Result<Resolution, String> {
    resolve_across(root, relative, &[], &[])
}

/// The repository store's published trees as link anchors (PLAN-L2 update 1
/// §(c)): a target inside a published tree
/// (`<store>/{trees,direct/trees,git/trees,builtins/trees}/<id>` with its
/// manifest, crate::repository_store::published_tree) resolves into that
/// tree whatever the invocation materialized before — Bazel follows such a
/// link and fetches the target repository — so the resolution does not
/// depend on the order of demands. A tree no root of the invocation names
/// is verified against its manifest on its first crossing (memoized per
/// repository set) and recorded as a REPO of the analysis snapshot; a tree
/// the invocation materialized is recorded by its owner (a reused
/// publication as a REPO, a local repository's tree as a replay record,
/// whose observations stay validated one by one). Stages, trash and every
/// other store path stay blocked.
#[derive(Clone)]
pub struct StoreTrees {
    /// The canonical repository store.
    root: PathBuf,
    /// Trees verified (true) or refused (false) by a first crossing.
    verified: Arc<Mutex<BTreeMap<PathBuf, bool>>>,
}

impl StoreTrees {
    pub fn new(root: PathBuf, verified: Arc<Mutex<BTreeMap<PathBuf, bool>>>) -> Self {
        Self { root, verified }
    }

    /// Whether `path` lies in the store (lexically).
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// The published tree containing `path` when it anchors a link instead
    /// of `owner`, the longest materialized root containing `path`: a tree
    /// deeper than `owner` that is one of `materialized` (its owner verified
    /// it in this invocation) or passes its first-crossing verification.
    /// When `owner` already is the containing tree (a link within one store
    /// tree), nothing is checked: the lexical test needs no I/O.
    pub fn anchor<'a>(
        &self,
        path: &Path,
        owner: Option<&Path>,
        mut materialized: impl Iterator<Item = &'a Path>,
    ) -> Option<PathBuf> {
        let tree = crate::repository_store::store_tree_of(&self.root, path)?;
        if owner.is_some_and(|owner| owner.components().count() >= tree.root.components().count())
            || !crate::repository_store::is_published(&tree)
        {
            return None;
        }
        if materialized.any(|root| root == tree.root) {
            return Some(tree.root);
        }
        let accepted = *self
            .verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(tree.root.clone())
            .or_insert_with(|| crate::repository_store::verify_published_tree(&tree).is_ok());
        if !accepted {
            return None;
        }
        // REPO + INTEGRITY of the analysis snapshot (once per root; the
        // owner's record replaces it if the repository is materialized
        // later in the run).
        observed_fs::crossed_tree(tree.kind, &tree.id, &tree.root, &tree.manifest);
        Some(tree.root)
    }
}

impl std::fmt::Debug for StoreTrees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StoreTrees").field(&self.root).finish()
    }
}

/// Equal stores are interchangeable (one invocation's memo).
impl PartialEq for StoreTrees {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for StoreTrees {}

impl std::hash::Hash for StoreTrees {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.root.hash(state);
    }
}

/// Where symlinks of one repository may point for native reads (source
/// artifacts, `.bzl` loads and `glob()`), shared by every reader.
///
/// Bazel follows any symlink (FileFunction, skyframe/FileFunction.java:101-155),
/// including absolute host paths such as the JDK directories a `local_jdk`
/// repository links (local_java_repository.bzl `repository_ctx.symlink`).
/// Natively, links are followed inside the repository, into repositories
/// materialized in this invocation (`others`) and into the store's published
/// trees (`store`, in any order); `blocked` host prefixes (the repository
/// store with stale trees and private stages, and the `--cache-dir` root) are
/// never claimed by a shorter root. `host_reads` additionally admits any
/// other absolute host path, for reads only: it is granted solely to
/// repositories generated by repository rules (their links are created by
/// `repository_ctx`, as Bazel's are), never to the main repository, local
/// path, archive, registry or Git trees, and never for writes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LinkPolicy {
    pub others: Vec<(String, PathBuf)>,
    pub blocked: Vec<PathBuf>,
    pub host_reads: bool,
    pub store: Option<StoreTrees>,
}

impl LinkPolicy {
    /// The root a link target `real` (fully resolved, symlink-free) found in
    /// the repository rooted at canonical `start_root` belongs to when native
    /// reads may use it: the longest containing repository root (unless a
    /// longer blocked prefix contains `real`) or published store tree, or,
    /// with `host_reads`, the target itself when no blocked prefix contains
    /// it. `None` when the policy refuses the link. `others`, `blocked` and
    /// the store must be canonical (`RepositorySet::link_policy`).
    pub fn anchor(&self, start_root: &Path, real: &Path) -> Option<PathBuf> {
        let owner = self
            .others
            .iter()
            .map(|(_, root)| root.as_path())
            .chain(std::iter::once(start_root))
            .filter(|root| real.starts_with(root))
            .max_by_key(|root| root.components().count());
        if let Some(tree) = self.store_tree(start_root, real, owner) {
            return Some(tree);
        }
        match owner {
            Some(root) => {
                let depth = root.components().count();
                (!self
                    .blocked
                    .iter()
                    .any(|prefix| real.starts_with(prefix) && prefix.components().count() > depth))
                .then(|| root.to_path_buf())
            }
            None => (self.host_reads
                && !self.blocked.iter().any(|prefix| real.starts_with(prefix)))
            .then(|| real.to_path_buf()),
        }
    }

    /// The published store tree containing `path` that anchors a link from
    /// `start_root` instead of `owner` ([`StoreTrees::anchor`]).
    fn store_tree(&self, start_root: &Path, path: &Path, owner: Option<&Path>) -> Option<PathBuf> {
        self.store.as_ref()?.anchor(
            path,
            owner,
            self.others
                .iter()
                .map(|(_, root)| root.as_path())
                .chain(std::iter::once(start_root)),
        )
    }

    /// Which taint of the analysis snapshot, if any, a symlink hop from the
    /// repository rooted at `origin` into `anchor` needs (PLAN-L2 update 1
    /// §(c)): none within the repository, into a store tree (published,
    /// content-addressed: an anchor in any order), into the main repository
    /// (always materialized) or to a plain host path (`host`: outside the
    /// store and every live root, it resolves the same in any order, and
    /// each hop is recorded). A hop into another live root — a
    /// `local_path_override` module or a local registry source, whose use
    /// depends on what was materialized before — taints.
    pub fn hop_taint(
        &self,
        origin: &Path,
        anchor: &Path,
        host: bool,
    ) -> Option<observed_fs::Taint> {
        if anchor == origin || host {
            return None;
        }
        if self
            .store
            .as_ref()
            .is_some_and(|store| store.contains(anchor))
            || self
                .others
                .iter()
                .any(|(name, root)| name.is_empty() && root == anchor)
        {
            return None;
        }
        Some(observed_fs::Taint::CrossRepositorySymlink)
    }

    /// Whether native reads may use `real` ([`LinkPolicy::anchor`]).
    pub fn permits(&self, start_root: &Path, real: &Path) -> bool {
        self.anchor(start_root, real).is_some()
    }

    /// Whether a directory `real` reached below `anchor` (the root that owns
    /// the traversal) lies inside a blocked prefix that does not contain the
    /// anchor: the native repository store or `--cache-dir` inside a
    /// workspace, which traversals skip like Bazel never sees its output base.
    pub fn skips(&self, anchor: &Path, real: &Path) -> bool {
        self.blocked
            .iter()
            .any(|prefix| real.starts_with(prefix) && !anchor.starts_with(prefix))
    }
}

/// [`resolve_across`] under a [`LinkPolicy`]: with `host_reads`, a link
/// leaving every materialized root continues physically on the host (never
/// into a `blocked` prefix).
pub fn resolve_with_policy(
    root: &Path,
    relative: &Path,
    policy: &LinkPolicy,
) -> Result<Resolution, String> {
    let resolution = resolve_across_inner(root, relative, policy)?;
    if policy.host_reads
        && let Resolution::Resolved(path) = &resolution
    {
        let start = observed_fs::canonicalize(root)
            .map_err(|e| format!("cannot resolve repository root {}: {e}", root.display()))?;
        if !policy.permits(&start, path) {
            return Err(format!(
                "source symlink path {} resolves to {} inside the repository store (native source resolution never follows links into stale or private repository trees)",
                relative.display(),
                path.display()
            ));
        }
    }
    Ok(resolution)
}

/// Like [`resolve_in_root`], but a link leaving `root` may continue into
/// another repository root in `others` (canonical name, root path): the
/// roots materialized in the current invocation's repository set. The target
/// is matched against them by longest path prefix; resolution continues inside
/// that root with the same 40-hop budget and cycle detection. Anything else
/// (an arbitrary host path, a stale or staging tree, a repository not yet
/// materialized) stays an explicit error. `blocked` prefixes (the repository
/// store holding stale trees and private stages) never belong to a shorter
/// root that contains them. Callers must use the result for reads only, under
/// the label's logical identity.
pub fn resolve_across(
    root: &Path,
    relative: &Path,
    others: &[(String, PathBuf)],
    blocked: &[PathBuf],
) -> Result<Resolution, String> {
    resolve_across_inner(
        root,
        relative,
        &LinkPolicy {
            others: others.to_vec(),
            blocked: blocked.to_vec(),
            ..LinkPolicy::default()
        },
    )
}

fn resolve_across_inner(
    root: &Path,
    relative: &Path,
    policy: &LinkPolicy,
) -> Result<Resolution, String> {
    let LinkPolicy {
        others,
        blocked,
        host_reads,
        store,
    } = policy;
    let host_reads = *host_reads;
    let shown = relative.display();
    let start_root = observed_fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve repository root {}: {e}", root.display()))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe source path {shown}"));
    }
    let mut candidates: Vec<PathBuf> = others
        .iter()
        .filter_map(|(_, path)| observed_fs::canonicalize(path).ok())
        .collect();
    if !others.is_empty() {
        // The starting repository is materialized too.
        candidates.push(start_root.clone());
    }
    let blocked: Vec<PathBuf> = blocked
        .iter()
        .map(|path| observed_fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    // A hop into another live repository root taints the analysis
    // snapshot: whether it is materialized depends on the run (PLAN-L2
    // §1b, update 1 §(c); `LinkPolicy::hop_taint`).
    let origin = start_root.clone();
    // Longest-prefix root containing `path` (lexically absolute); a
    // published store tree anchors in any order (`StoreTrees`).
    let owner = |path: &Path| -> Option<PathBuf> {
        let found = candidates
            .iter()
            .filter(|candidate| path.starts_with(candidate))
            .max_by_key(|candidate| candidate.components().count());
        if let Some(store) = store
            && let Some(tree) = store.anchor(
                path,
                found.map(PathBuf::as_path),
                candidates
                    .iter()
                    .map(PathBuf::as_path)
                    .chain(std::iter::once(origin.as_path())),
            )
        {
            return Some(tree);
        }
        let found = found?;
        let depth = found.components().count();
        if blocked
            .iter()
            .any(|prefix| path.starts_with(prefix) && prefix.components().count() > depth)
        {
            return None;
        }
        Some(found.clone())
    };
    let mut root = start_root;
    let mut pending = VecDeque::new();
    push_front(&mut pending, relative);
    let mut resolved: Vec<OsString> = Vec::new();
    let mut hops = 0;
    // Each followed link with the components still pending after it: seeing
    // the same pair again means resolution would repeat forever.
    let mut followed: Vec<(PathBuf, VecDeque<OsString>)> = Vec::new();
    let current = |root: &Path, resolved: &[OsString]| {
        let mut path = root.to_path_buf();
        path.extend(resolved);
        path
    };
    // Re-root a lexically absolute host path into another materialized root.
    let reroot = |absolute: PathBuf,
                  root: &mut PathBuf,
                  resolved: &mut Vec<OsString>,
                  pending: &mut VecDeque<OsString>|
     -> bool {
        let owned = owner(&absolute);
        let host = owned.is_none();
        let Some(next) = owned.or_else(|| {
            // Host reads continue physically from the filesystem root; the
            // caller rejects a final path inside a blocked prefix.
            host_reads.then(|| PathBuf::from("/"))
        }) else {
            return false;
        };
        if let Some(taint) = policy.hop_taint(&origin, &next, host) {
            observed_fs::taint(taint, || {
                format!("{shown} in {} -> {}", origin.display(), absolute.display())
            });
        }
        let inside = absolute
            .strip_prefix(&next)
            .expect("owner contains path")
            .to_path_buf();
        *root = next;
        resolved.clear();
        push_front(pending, &inside);
        true
    };
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            if resolved.pop().is_none() && root.parent().is_some() {
                // Leaving the root lexically: the rest of the path continues
                // from the root's parent on the host.
                let mut absolute = root.parent().map(Path::to_path_buf).unwrap_or_default();
                let rest: Vec<OsString> = pending.drain(..).collect();
                for component in rest {
                    if component == ".." {
                        absolute.pop();
                    } else {
                        absolute.push(component);
                    }
                }
                let outside = root.clone();
                if !reroot(absolute.clone(), &mut root, &mut resolved, &mut pending) {
                    // Bazel reports a missing target as a missing file; only an
                    // existing outside target is the native restriction.
                    if observed_fs::metadata(&absolute).is_err() {
                        return Ok(Resolution::Missing(absolute));
                    }
                    return Err(format!(
                        "source symlink path {shown} resolves outside its repository root {} (native source resolution follows links only into repositories materialized in this invocation)",
                        outside.display()
                    ));
                }
            }
            continue;
        }
        resolved.push(part);
        let path = current(&root, &resolved);
        let metadata = match observed_fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.kind() == io::ErrorKind::NotADirectory =>
            {
                return Ok(Resolution::Missing(path));
            }
            Err(error) => return Err(format!("cannot inspect source path {shown}: {error}")),
        };
        if metadata.file_type().is_symlink() {
            let state = (path.clone(), pending.clone());
            if let Some(start) = followed.iter().position(|seen| *seen == state) {
                return Err(cycle_message(
                    followed[start..].iter().map(|(link, _)| link.as_path()),
                ));
            }
            followed.push(state);
            hops += 1;
            if hops > MAX_LINK_HOPS {
                return Err(format!(
                    "source symlink path {shown} exceeds {MAX_LINK_HOPS} symlink hops (cycle?)"
                ));
            }
            let target = observed_fs::read_link(&path)
                .map_err(|e| format!("cannot read source symlink {}: {e}", path.display()))?;
            resolved.pop();
            if target.is_absolute() {
                if let Ok(inside) = target.strip_prefix(&root) {
                    let inside = inside.to_path_buf();
                    resolved.clear();
                    push_front(&mut pending, &inside);
                } else {
                    let mut absolute = PathBuf::from("/");
                    for component in target.components() {
                        match component {
                            Component::Normal(part) => absolute.push(part),
                            Component::ParentDir => {
                                absolute.pop();
                            }
                            _ => {}
                        }
                    }
                    let mut rest = PathBuf::new();
                    rest.extend(pending.drain(..));
                    let joined = if rest.as_os_str().is_empty() {
                        absolute
                    } else {
                        absolute.join(&rest)
                    };
                    if !reroot(joined.clone(), &mut root, &mut resolved, &mut pending) {
                        if observed_fs::metadata(&joined).is_err() {
                            return Ok(Resolution::Missing(joined));
                        }
                        return Err(format!(
                            "source symlink {} points outside its repository root to {} (native source resolution follows links only into repositories materialized in this invocation)",
                            path.display(),
                            target.display()
                        ));
                    }
                }
            } else {
                push_front(&mut pending, &target);
            }
        } else if !pending.is_empty() && !metadata.is_dir() {
            let mut missing = path;
            missing.push(pending.front().expect("nonempty"));
            return Ok(Resolution::Missing(missing));
        }
    }
    Ok(Resolution::Resolved(current(&root, &resolved)))
}

/// A symlink problem found while resolving a path the way Bazel's
/// FileFunction does (skyframe/FileFunction.java:309-391).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymlinkIssue {
    /// FileSymlinkCycleException: the cycle's logical paths.
    Cycle(Vec<PathBuf>),
    /// FileSymlinkInfiniteExpansionException: the expansion chain.
    InfiniteExpansion(Vec<PathBuf>),
    /// Any other filesystem error, with its description.
    Io(String),
}

impl SymlinkIssue {
    /// The uniqueness event (AbstractFileChainUniquenessFunction.java:43-58)
    /// for the canonicalized chain (rotated to its smallest path, :63-77).
    pub fn event(&self) -> Option<String> {
        match self {
            Self::Cycle(cycle) => Some(cycle_message(cycle.iter().map(PathBuf::as_path))),
            Self::InfiniteExpansion(chain) => {
                let mut links: Vec<String> = chain
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let start = (0..links.len())
                    .min_by(|a, b| links[*a].cmp(&links[*b]))
                    .unwrap_or(0);
                links.rotate_left(start);
                Some(format!(
                    "infinite symlink expansion detected\n[start of symlink chain]\n{}\n[end of symlink chain]",
                    links.join("\n")
                ))
            }
            Self::Io(_) => None,
        }
    }

    /// FileSymlinkException.getUserFriendlyMessage
    /// (io/FileSymlinkCycleException.java:56-59,
    /// io/FileSymlinkInfiniteExpansionException.java:56-60).
    pub fn user_friendly(&self) -> String {
        let join = |chain: &[PathBuf]| {
            chain
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("- > ")
        };
        match self {
            Self::Cycle(cycle) => format!("Symlink cycle: {}", join(cycle)),
            Self::InfiniteExpansion(chain) => {
                format!("Infinite symlink expansion: {}", join(chain))
            }
            Self::Io(message) => message.clone(),
        }
    }
}

/// The resolved type of a [`FileValue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Missing,
    File,
    Directory,
    /// A special file (FIFO, socket, device).
    Other,
}

/// The subset of Bazel's FileValue (actions/FileValue.java) a glob needs.
#[derive(Clone, Debug)]
pub struct FileValue {
    /// The fully resolved, symlink-free path.
    pub real: PathBuf,
    pub kind: FileKind,
    /// logicalChainDuringResolution: every logical path considered.
    chain: Vec<PathBuf>,
    /// unboundedAncestorSymlinkExpansionChain: a link to an ancestor of a
    /// path in the chain was met (FileFunction.java:366-379).
    pub ancestor_expansion: Option<Vec<PathBuf>>,
}

#[derive(Default)]
struct ResolutionState {
    chain: Vec<PathBuf>,
    sorted: std::collections::BTreeSet<PathBuf>,
    ancestor_expansion: Option<Vec<PathBuf>>,
}

/// Safety bound on symlink hops of one resolution. Bazel has none: its
/// chain checks already end every resolution; this only bounds hostile trees.
const MAX_FILE_VALUE_HOPS: usize = 4096;

/// Memoized emulation of Bazel's FileFunction over absolute, canonical-rooted
/// logical paths: ancestors are resolved first (resolveFromAncestors,
/// FileFunction.java:179-249), symlink targets are lexically normalized
/// (Path.getRelative, :266-291), and every logical path is checked against
/// the chain for cycles, descendant expansion (errors) and ancestor expansion
/// (recorded) (:309-391). Reads metadata and link targets only.
#[derive(Default)]
pub struct FileValues {
    memo: std::collections::HashMap<PathBuf, Result<std::rc::Rc<FileValue>, SymlinkIssue>>,
    in_progress: std::collections::HashSet<PathBuf>,
}

impl FileValues {
    pub fn get(&mut self, path: &Path) -> Result<std::rc::Rc<FileValue>, SymlinkIssue> {
        if let Some(value) = self.memo.get(path) {
            return value.clone();
        }
        if !self.in_progress.insert(path.to_path_buf()) {
            // A Skyframe dependency cycle between FileValues (a link whose
            // target's ancestor resolves through the link itself).
            return Err(SymlinkIssue::Cycle(vec![path.to_path_buf()]));
        }
        let value = self.compute(path).map(std::rc::Rc::new);
        self.in_progress.remove(path);
        self.memo.insert(path.to_path_buf(), value.clone());
        value
    }

    fn compute(&mut self, path: &Path) -> Result<FileValue, SymlinkIssue> {
        let mut state = ResolutionState::default();
        let (mut real, mut metadata) = self.resolve_ancestors(path, &mut state)?;
        let mut hops = 0;
        while metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            hops += 1;
            if hops > MAX_FILE_VALUE_HOPS {
                return Err(SymlinkIssue::Io(format!(
                    "{} (Too many levels of symbolic links)",
                    path.display()
                )));
            }
            let target = observed_fs::read_link(&real).map_err(|error| {
                SymlinkIssue::Io(format!("cannot read symlink {}: {error}", real.display()))
            })?;
            let base = if target.is_absolute() {
                PathBuf::from("/")
            } else {
                real.parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
            };
            let next = normalize(&base.join(&target));
            check_seen(&next, &mut state)?;
            (real, metadata) = self.resolve_ancestors(&next, &mut state)?;
        }
        let kind = match metadata {
            None => FileKind::Missing,
            Some(metadata) if metadata.is_dir() => FileKind::Directory,
            Some(metadata) if metadata.is_file() => FileKind::File,
            Some(_) => FileKind::Other,
        };
        Ok(FileValue {
            real,
            kind,
            chain: state.chain,
            ancestor_expansion: state.ancestor_expansion,
        })
    }

    /// resolveFromAncestors: the path with its ancestors resolved, and its
    /// lstat (None when missing).
    fn resolve_ancestors(
        &mut self,
        path: &Path,
        state: &mut ResolutionState,
    ) -> Result<(PathBuf, Option<fs::Metadata>), SymlinkIssue> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            check_and_note(path, state)?;
            return Ok((path.to_path_buf(), lstat(path)?));
        };
        let parent = self.get(parent)?;
        let real = parent.real.join(name);
        if parent.kind != FileKind::Directory {
            return Ok((real, None));
        }
        for logical in &parent.chain {
            check_and_note(&logical.join(name), state)?;
        }
        let metadata = lstat(&real)?;
        Ok((real, metadata))
    }
}

fn lstat(path: &Path) -> Result<Option<fs::Metadata>, SymlinkIssue> {
    match observed_fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::NotADirectory =>
        {
            Ok(None)
        }
        Err(error) => Err(SymlinkIssue::Io(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

/// Lexical normalization of an absolute path (PathFragment semantics: `..`
/// at the root stays at the root).
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

fn check_and_note(path: &Path, state: &mut ResolutionState) -> Result<(), SymlinkIssue> {
    check_seen(path, state)?;
    state.sorted.insert(path.to_path_buf());
    state.chain.push(path.to_path_buf());
    Ok(())
}

/// checkPathSeenDuringPartialResolutionInternal (FileFunction.java:309-391).
fn check_seen(path: &Path, state: &mut ResolutionState) -> Result<(), SymlinkIssue> {
    let split = |at: &Path, with_path: bool| -> Vec<PathBuf> {
        let mut full = state.chain.clone();
        if with_path {
            full.push(path.to_path_buf());
        }
        let start = full.iter().position(|seen| seen == at).unwrap_or(0);
        full.split_off(start)
    };
    if state.sorted.contains(path) {
        return Err(SymlinkIssue::Cycle(split(path, false)));
    }
    if let Some(floor) = state
        .sorted
        .range::<Path, _>((std::ops::Bound::Unbounded, std::ops::Bound::Excluded(path)))
        .next_back()
        && path.starts_with(floor)
    {
        let floor = floor.clone();
        return Err(SymlinkIssue::InfiniteExpansion(split(&floor, true)));
    }
    if let Some(ceiling) = state
        .sorted
        .range::<Path, _>((std::ops::Bound::Included(path), std::ops::Bound::Unbounded))
        .next()
        && ceiling.starts_with(path)
        && state.ancestor_expansion.is_none()
    {
        let ceiling = ceiling.clone();
        state.ancestor_expansion = Some(split(&ceiling, true));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tree() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("repo/data/deep")).unwrap();
        fs::write(root.join("repo/data/real.txt"), b"real").unwrap();
        fs::write(root.join("outside.txt"), b"outside").unwrap();
        symlink("data/real.txt", root.join("repo/link.txt")).unwrap();
        symlink("data", root.join("repo/dirlink")).unwrap();
        symlink("../link.txt", root.join("repo/data/chain.txt")).unwrap();
        symlink("deep/../real.txt", root.join("repo/data/dots.txt")).unwrap();
        symlink(
            root.join("repo/data/real.txt"),
            root.join("repo/absolute.txt"),
        )
        .unwrap();
        symlink("../outside.txt", root.join("repo/escape.txt")).unwrap();
        symlink(
            root.join("outside.txt"),
            root.join("repo/absolute-escape.txt"),
        )
        .unwrap();
        symlink("missing", root.join("repo/dangling")).unwrap();
        symlink("data/real.txt/x", root.join("repo/through-file")).unwrap();
        symlink("loop", root.join("repo/loop")).unwrap();
        temp
    }

    #[test]
    fn follows_relative_absolute_directory_and_chained_links_inside_root() {
        let temp = tree();
        let root = temp.path().join("repo");
        let real = fs::canonicalize(root.join("data/real.txt")).unwrap();
        for name in [
            "link.txt",
            "dirlink/real.txt",
            "data/chain.txt",
            "dirlink/chain.txt",
            "data/dots.txt",
            "absolute.txt",
            "data/real.txt",
        ] {
            assert_eq!(
                resolve_in_root(&root, Path::new(name)).unwrap(),
                Resolution::Resolved(real.clone()),
                "{name}"
            );
        }
    }

    #[test]
    fn escapes_cycles_and_missing_targets_are_explicit() {
        let temp = tree();
        let root = temp.path().join("repo");
        for name in ["escape.txt", "absolute-escape.txt"] {
            let error = resolve_in_root(&root, Path::new(name)).unwrap_err();
            assert!(error.contains("outside its repository root"), "{error}");
        }
        let canonical_root = fs::canonicalize(&root).unwrap();
        assert_eq!(
            resolve_in_root(&root, Path::new("loop")).unwrap_err(),
            format!(
                "circular symlinks detected\n[start of symlink cycle]\n{}\n[end of symlink cycle]",
                canonical_root.join("loop").display()
            )
        );
        // Rotated to start at the smallest path, as Bazel canonicalizes cycles.
        symlink("z-cycle", root.join("a-cycle")).unwrap();
        symlink("a-cycle", root.join("z-cycle")).unwrap();
        for start in ["a-cycle", "z-cycle"] {
            assert_eq!(
                resolve_in_root(&root, Path::new(start)).unwrap_err(),
                format!(
                    "circular symlinks detected\n[start of symlink cycle]\n{}\n{}\n[end of symlink cycle]",
                    canonical_root.join("a-cycle").display(),
                    canonical_root.join("z-cycle").display()
                )
            );
        }
        // Revisiting a directory link with a different remaining path is not a cycle.
        symlink(".", root.join("data/self")).unwrap();
        assert_eq!(
            resolve_in_root(&root, Path::new("data/self/self/real.txt")).unwrap(),
            Resolution::Resolved(canonical_root.join("data/real.txt"))
        );
        let canonical = fs::canonicalize(&root).unwrap();
        assert_eq!(
            resolve_in_root(&root, Path::new("dangling")).unwrap(),
            Resolution::Missing(canonical.join("missing"))
        );
        assert_eq!(
            resolve_in_root(&root, Path::new("through-file")).unwrap(),
            Resolution::Missing(canonical.join("data/real.txt/x"))
        );
        assert!(resolve_in_root(&root, Path::new("../x")).is_err());
        // A missing target outside the root is a missing file, as in Bazel.
        symlink("../absent.txt", root.join("escape-missing")).unwrap();
        symlink(
            temp.path().join("absent.txt"),
            root.join("absolute-missing"),
        )
        .unwrap();
        for name in ["escape-missing", "absolute-missing"] {
            assert!(matches!(
                resolve_in_root(&root, Path::new(name)).unwrap(),
                Resolution::Missing(_)
            ));
        }
    }

    #[test]
    fn cross_repository_links_follow_only_materialized_roots() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        for dir in [
            "gen",
            "rules_cc/cc/private",
            "rules_cc/cc/nested/inner",
            "stale",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(base.join("rules_cc/cc/private/config.bzl"), b"cc").unwrap();
        fs::write(base.join("rules_cc/cc/nested/inner/x"), b"inner").unwrap();
        fs::write(base.join("stale/old.bzl"), b"stale").unwrap();
        // local_config_cc shape: an absolute link into another repository.
        symlink(
            base.join("rules_cc/cc/private/config.bzl"),
            base.join("gen/cc_toolchain_config.bzl"),
        )
        .unwrap();
        // Relative escape, then a link inside the target repository.
        symlink("../rules_cc/cc/link", base.join("gen/relative")).unwrap();
        symlink("private/config.bzl", base.join("rules_cc/cc/link")).unwrap();
        symlink(base.join("stale/old.bzl"), base.join("gen/stale.bzl")).unwrap();
        // A two-root cycle.
        symlink(base.join("rules_cc/back"), base.join("gen/forth")).unwrap();
        symlink(base.join("gen/forth"), base.join("rules_cc/back")).unwrap();
        // Nested roots: the longest prefix owns the target.
        symlink(
            base.join("rules_cc/cc/nested/inner/x"),
            base.join("gen/nested"),
        )
        .unwrap();
        let others = vec![
            ("rules_cc+".to_owned(), base.join("rules_cc")),
            ("nested+".to_owned(), base.join("rules_cc/cc/nested")),
        ];
        let gen_root = base.join("gen");
        for name in ["cc_toolchain_config.bzl", "relative"] {
            assert_eq!(
                resolve_across(&gen_root, Path::new(name), &others, &[]).unwrap(),
                Resolution::Resolved(base.join("rules_cc/cc/private/config.bzl")),
                "{name}"
            );
            // Without the materialized root the link stays an explicit error.
            let error = resolve_in_root(&gen_root, Path::new(name)).unwrap_err();
            assert!(error.contains("outside its repository root"), "{error}");
        }
        assert_eq!(
            resolve_across(&gen_root, Path::new("nested"), &others, &[]).unwrap(),
            Resolution::Resolved(base.join("rules_cc/cc/nested/inner/x"))
        );
        let error = resolve_across(&gen_root, Path::new("stale.bzl"), &others, &[]).unwrap_err();
        assert!(error.contains("outside its repository root"), "{error}");
        // A workspace root containing the repository store never claims the
        // store's stale trees.
        let mut with_main = others.clone();
        with_main.push((String::new(), base.clone()));
        assert!(resolve_across(&gen_root, Path::new("stale.bzl"), &with_main, &[]).is_ok());
        let error = resolve_across(
            &gen_root,
            Path::new("stale.bzl"),
            &with_main,
            &[base.join("stale")],
        )
        .unwrap_err();
        assert!(error.contains("outside its repository root"), "{error}");
        let error = resolve_across(&gen_root, Path::new("forth"), &others, &[]).unwrap_err();
        assert_eq!(
            error,
            format!(
                "circular symlinks detected\n[start of symlink cycle]\n{}\n{}\n[end of symlink cycle]",
                base.join("gen/forth").display(),
                base.join("rules_cc/back").display()
            )
        );
    }
    #[test]
    fn host_reads_follow_absolute_and_relative_host_links_but_never_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        for dir in ["gen", "host/jdk/bin", "store/stale"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(base.join("host/jdk/bin/java"), b"java").unwrap();
        fs::write(base.join("store/stale/f"), b"stale").unwrap();
        // local_jdk shape: absolute directory links to the host JDK; a host
        // link chain inside the host tree; a relative escape.
        symlink(base.join("host/jdk/bin"), base.join("gen/bin")).unwrap();
        symlink("jdk", base.join("host/current")).unwrap();
        symlink(base.join("host/current/bin/java"), base.join("gen/java")).unwrap();
        symlink("../host/jdk/bin/java", base.join("gen/relative")).unwrap();
        symlink(base.join("store/stale/f"), base.join("gen/stale")).unwrap();
        symlink(base.join("host/absent"), base.join("gen/dangling")).unwrap();
        let gen_root = base.join("gen");
        let java = base.join("host/jdk/bin/java");
        let host = LinkPolicy {
            host_reads: true,
            blocked: vec![base.join("store")],
            ..LinkPolicy::default()
        };
        for name in ["bin/java", "java", "relative"] {
            assert_eq!(
                resolve_with_policy(&gen_root, Path::new(name), &host).unwrap(),
                Resolution::Resolved(java.clone()),
                "{name}"
            );
            // Without host reads (main, archive, local trees) it stays an error.
            let error = resolve_with_policy(&gen_root, Path::new(name), &LinkPolicy::default())
                .unwrap_err();
            assert!(error.contains("outside its repository root"), "{error}");
        }
        let error = resolve_with_policy(&gen_root, Path::new("stale"), &host).unwrap_err();
        assert!(error.contains("repository store"), "{error}");
        assert!(matches!(
            resolve_with_policy(&gen_root, Path::new("dangling"), &host).unwrap(),
            Resolution::Missing(_)
        ));
        assert!(host.permits(&gen_root, &java));
        assert!(!host.permits(&gen_root, &base.join("store/stale/f")));
        assert!(!LinkPolicy::default().permits(&gen_root, &java));
    }

    /// PLAN-L2 update 1 §(c): a published store tree anchors a link in any
    /// order (verified on its first crossing); stages, trash and a tampered
    /// tree stay refused; only hops into live roots taint.
    #[test]
    fn published_store_trees_anchor_links_whatever_was_materialized() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let store = base.join("cache/repositories");
        let (tree, _) = crate::repository_store::materialize(
            &store,
            "dep",
            b"spec",
            b"provenance",
            |_, _| Ok(false),
            |stage| {
                fs::write(stage.join("defs.bzl"), b"VALUE = 1\n").map_err(|e| e.to_string())?;
                fs::create_dir(stage.join("sub")).map_err(|e| e.to_string())?;
                fs::write(stage.join("sub/x"), b"x").map_err(|e| e.to_string())?;
                Ok(crate::repository_store::Executed {
                    inputs: Vec::new(),
                    recorded: Vec::new(),
                })
            },
        )
        .unwrap();
        let tree = fs::canonicalize(tree).unwrap();
        fs::create_dir_all(base.join("gen")).unwrap();
        fs::create_dir_all(store.join("direct/stages/abc")).unwrap();
        fs::write(store.join("direct/stages/abc/defs.bzl"), b"").unwrap();
        let trash = store.join("direct/trees/.trash-1/tree");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("defs.bzl"), b"").unwrap();
        symlink(tree.join("defs.bzl"), base.join("gen/linked.bzl")).unwrap();
        symlink(tree.join("sub"), base.join("gen/dir")).unwrap();
        symlink(
            store.join("direct/stages/abc/defs.bzl"),
            base.join("gen/stage.bzl"),
        )
        .unwrap();
        symlink(trash.join("defs.bzl"), base.join("gen/trash.bzl")).unwrap();
        let gen_root = base.join("gen");
        let policy = |host_reads: bool| LinkPolicy {
            // The workspace contains the store (the default cache layout).
            others: vec![(String::new(), base.clone())],
            blocked: vec![store.clone(), base.join("cache")],
            host_reads,
            store: Some(StoreTrees::new(store.clone(), Arc::default())),
        };
        for host_reads in [false, true] {
            let policy = policy(host_reads);
            assert_eq!(
                resolve_with_policy(&gen_root, Path::new("linked.bzl"), &policy).unwrap(),
                Resolution::Resolved(tree.join("defs.bzl"))
            );
            assert_eq!(
                resolve_with_policy(&gen_root, Path::new("dir/x"), &policy).unwrap(),
                Resolution::Resolved(tree.join("sub/x"))
            );
            assert_eq!(
                policy.anchor(&gen_root, &tree.join("sub/x")),
                Some(tree.clone())
            );
            for refused in ["stage.bzl", "trash.bzl"] {
                assert!(
                    resolve_with_policy(&gen_root, Path::new(refused), &policy).is_err(),
                    "{refused}"
                );
            }
            // Without the store rule the link stays refused (the old,
            // order-dependent native rule when the tree was not materialized).
            let mut without = policy.clone();
            without.store = None;
            assert!(resolve_with_policy(&gen_root, Path::new("linked.bzl"), &without).is_err());
        }
        // Hop taints: store trees, the main repository and host paths none;
        // another live root one.
        let policy = policy(true);
        assert_eq!(policy.hop_taint(&gen_root, &tree, false), None);
        assert_eq!(policy.hop_taint(&gen_root, &base, false), None);
        assert_eq!(
            policy.hop_taint(&gen_root, Path::new("/usr/lib/jvm"), true),
            None
        );
        assert_eq!(policy.hop_taint(&gen_root, &gen_root, false), None);
        assert_eq!(
            policy.hop_taint(&gen_root, &base.join("local_override"), false),
            Some(observed_fs::Taint::CrossRepositorySymlink)
        );
        // A tree that no longer matches its manifest is refused on its first
        // crossing (memoized per repository set).
        fs::write(tree.join("defs.bzl"), b"tampered").unwrap();
        let fresh = LinkPolicy {
            store: Some(StoreTrees::new(store.clone(), Arc::default())),
            ..policy.clone()
        };
        assert!(resolve_with_policy(&gen_root, Path::new("linked.bzl"), &fresh).is_err());
        // ... unless this invocation materialized (and so verified) it.
        let mut materialized = fresh.clone();
        materialized.others.push(("dep+".to_owned(), tree.clone()));
        assert_eq!(
            resolve_with_policy(&gen_root, Path::new("linked.bzl"), &materialized).unwrap(),
            Resolution::Resolved(tree.join("defs.bzl"))
        );
    }

    #[test]
    fn file_values_follow_bazel_chain_rules() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(base.join("r/d")).unwrap();
        fs::write(base.join("r/d/f"), b"f").unwrap();
        symlink("d", base.join("r/l")).unwrap();
        symlink("l/f", base.join("r/lf")).unwrap();
        symlink("..", base.join("r/d/up")).unwrap();
        symlink("x/y", base.join("r/x")).unwrap();
        symlink("missing", base.join("r/dangling")).unwrap();
        let mut values = FileValues::default();
        let value = values.get(&base.join("r/l")).unwrap();
        assert_eq!(
            (value.real.clone(), value.kind),
            (base.join("r/d"), FileKind::Directory)
        );
        assert!(value.ancestor_expansion.is_none());
        let value = values.get(&base.join("r/l/f")).unwrap();
        assert_eq!(
            (value.real.clone(), value.kind),
            (base.join("r/d/f"), FileKind::File)
        );
        assert_eq!(
            values.get(&base.join("r/lf")).unwrap().real,
            base.join("r/d/f")
        );
        assert_eq!(
            values.get(&base.join("r/dangling")).unwrap().kind,
            FileKind::Missing
        );
        // A link to an ancestor resolves but records the expansion.
        let value = values.get(&base.join("r/d/up")).unwrap();
        assert_eq!(value.real, base.join("r"));
        assert_eq!(
            value.ancestor_expansion,
            Some(vec![base.join("r/d/up"), base.join("r")])
        );
        // A link to its own descendant is an infinite expansion.
        let issue = values.get(&base.join("r/x")).unwrap_err();
        assert_eq!(
            issue,
            SymlinkIssue::InfiniteExpansion(vec![base.join("r/x"), base.join("r/x/y")])
        );
        assert_eq!(
            issue.user_friendly(),
            format!(
                "Infinite symlink expansion: {0}/r/x- > {0}/r/x/y",
                base.display()
            )
        );
    }
}
