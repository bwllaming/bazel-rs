//! The cache of a git repository's worktrees (docs/WORKTREES.md).
//!
//! **The default cache root.** Without `--cache-dir` the cache is found from
//! the git metadata above the workspace, without running git: the walk goes
//! up from the workspace root to the first `.git` entry.
//!
//! * A `.git` directory is a main checkout: the cache is
//!   `<workspace>/bazel-rs-out`, as it always was.
//! * A `.git` file (`gitdir: X`) whose git directory `X` has a `commondir`
//!   is a linked worktree (`git worktree add`): the common directory `C`
//!   (`commondir` relative to `X`) belongs to the main checkout `M` — the
//!   parent of a non-bare `C` named `.git`, or `C`'s `core.worktree` — and
//!   the cache is the main checkout's own default,
//!   `M/<the workspace relative to its git root>/bazel-rs-out`. Every
//!   worktree of a repository thus shares one cache (repositories, actions,
//!   downloads and test results are stored by content).
//! * Anything else falls back to `<workspace>/bazel-rs-out`: no `.git` at
//!   all, a `.git` file without `commondir` (a submodule, `--separate-git-dir`:
//!   a main checkout whose git directory lives elsewhere), a bare repository,
//!   malformed or unreadable metadata, and a main checkout that lacks the
//!   workspace directory.
//!
//! Every path is canonical (symlinks resolved), so a worktree reached through
//! a symlink uses the same cache. `--cache-dir` still overrides the default.
//!
//! **Per-worktree state.** What belongs to one workspace — the analysis
//! snapshots, the last test result key per test, the execution roots of
//! `run` of a test, the hidden lockfile — lives in
//! `<cache>/worktrees/<name>-<hash>/` ([`state_directory`]), `<name>` the
//! workspace directory's name and `<hash>` 64 bits of the SHA-256 of its
//! canonical path, whatever chose the cache (the default or `--cache-dir`).
//!
//! This module reads the git layout with plain `std::fs`: it runs before the
//! analysis snapshot's recording window, and what it decides — the cache
//! root and the workspace — is part of the snapshot key.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// The name of the default cache directory below a workspace (or the
/// same workspace in the main checkout).
pub const CACHE_NAME: &str = "bazel-rs-out";
/// `<cache>/worktrees`: one directory of per-worktree state per workspace.
pub const WORKTREES: &str = "worktrees";
/// The longest `<name>` part of a state directory name.
const NAME_LIMIT: usize = 40;
/// The largest `.git` file, `commondir` or `config` read.
const METADATA_LIMIT: u64 = 1 << 20;

/// What the git metadata above a workspace says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checkout {
    /// No `.git` entry at or above the workspace.
    NoGit,
    /// A main checkout rooted at `root` (a `.git` directory, or a `.git`
    /// file whose git directory has no `commondir`).
    Main { root: PathBuf },
    /// The linked worktree rooted at `root` of the main checkout `main`;
    /// `main_workspace` is the same workspace there.
    Linked {
        root: PathBuf,
        main: PathBuf,
        main_workspace: PathBuf,
    },
    /// The metadata at `root` cannot be used (the default falls back to
    /// the workspace's own cache), and why.
    Unusable { root: PathBuf, reason: String },
}

/// A workspace and its git layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The canonical workspace root.
    pub workspace: PathBuf,
    pub checkout: Checkout,
}

impl Layout {
    /// The cache root without `--cache-dir`.
    pub fn default_cache(&self) -> PathBuf {
        match &self.checkout {
            Checkout::Linked { main_workspace, .. } => main_workspace.join(CACHE_NAME),
            _ => self.workspace.join(CACHE_NAME),
        }
    }

    /// The same workspace in the main checkout, for a linked worktree.
    pub fn main_workspace(&self) -> Option<&Path> {
        match &self.checkout {
            Checkout::Linked { main_workspace, .. } => Some(main_workspace),
            _ => None,
        }
    }

    /// The note for a linked worktree that still has the cache it used
    /// before it shared its main checkout's (`<workspace>/bazel-rs-out`)
    /// while `cache` — the default — is another directory; None otherwise.
    pub fn legacy_cache_note(&self, cache: &Path) -> Option<String> {
        self.main_workspace()?;
        let legacy = self.workspace.join(CACHE_NAME);
        // A directory, not followed: a link there is the user's own choice.
        let is_directory = fs::symlink_metadata(&legacy).is_ok_and(|metadata| metadata.is_dir());
        if !is_directory || same_directory(&legacy, cache) {
            return None;
        }
        Some(format!(
            "bazel-rs: {} is no longer used: this git worktree shares the cache of its main checkout, {}; delete {} to free its space",
            legacy.display(),
            cache.display(),
            legacy.display()
        ))
    }
}

fn same_directory(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// The git layout of `workspace` (its root directory).
pub fn detect(workspace: &Path) -> Layout {
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let checkout = checkout(&workspace);
    Layout {
        workspace,
        checkout,
    }
}

fn checkout(workspace: &Path) -> Checkout {
    for root in workspace.ancestors() {
        let dot_git = root.join(".git");
        let Ok(entry) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        let unusable = |reason: String| Checkout::Unusable {
            root: root.to_path_buf(),
            reason,
        };
        // A `.git` link counts as what it names.
        let metadata = if entry.file_type().is_symlink() {
            match fs::metadata(&dot_git) {
                Ok(metadata) => metadata,
                Err(error) => return unusable(format!("{}: {error}", dot_git.display())),
            }
        } else {
            entry
        };
        if metadata.is_dir() {
            return Checkout::Main {
                root: root.to_path_buf(),
            };
        }
        if !metadata.is_file() {
            return unusable(format!(
                "{} is neither a file nor a directory",
                dot_git.display()
            ));
        }
        return match linked(workspace, root, &dot_git) {
            Ok(Some((main, main_workspace))) => Checkout::Linked {
                root: root.to_path_buf(),
                main,
                main_workspace,
            },
            Ok(None) => Checkout::Main {
                root: root.to_path_buf(),
            },
            Err(reason) => unusable(reason),
        };
    }
    Checkout::NoGit
}

/// The main checkout and its workspace of the linked worktree whose `.git`
/// file is `dot_git` (at `root`, an ancestor of `workspace`); None for a
/// `.git` file whose git directory has no `commondir` (a main checkout).
fn linked(
    workspace: &Path,
    root: &Path,
    dot_git: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let text = read_small(dot_git)?;
    // setup.c read_gitfile_gently: the first line, "gitdir: <path>".
    let line = text.lines().next().unwrap_or_default();
    let Some(value) = line.strip_prefix("gitdir:") else {
        return Err(format!("{} has no `gitdir:` line", dot_git.display()));
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{} names no git directory", dot_git.display()));
    }
    let git_directory = canonical_directory(&root.join(value), "the git directory")?;
    let commondir = git_directory.join("commondir");
    let common = match fs::symlink_metadata(&commondir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", commondir.display())),
        Ok(_) => read_small(&commondir)?,
    };
    let common = common.trim_end_matches(['\n', '\r']);
    if common.is_empty() {
        return Err(format!("{} is empty", commondir.display()));
    }
    let common = canonical_directory(&git_directory.join(common), "the common git directory")?;
    let config = common.join("config");
    let (bare, worktree) = match fs::symlink_metadata(&config) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, None),
        Err(error) => return Err(format!("{}: {error}", config.display())),
        Ok(_) => core_settings(&read_small(&config)?),
    };
    if bare {
        return Err(format!("{} is a bare repository", common.display()));
    }
    let main = match worktree {
        // config.c: core.worktree relative to the git directory.
        Some(worktree) => canonical_directory(&common.join(worktree), "core.worktree")?,
        None if common.file_name().is_some_and(|name| name == ".git") => common
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("{} has no parent", common.display()))?,
        None => {
            return Err(format!(
                "{} is not the `.git` directory of a main checkout",
                common.display()
            ));
        }
    };
    if main == root {
        return Ok(None);
    }
    let relative = workspace
        .strip_prefix(root)
        .map_err(|_| format!("{} is not below {}", workspace.display(), root.display()))?;
    let main_workspace =
        canonical_directory(&main.join(relative), "the main checkout's workspace")?;
    Ok(Some((main, main_workspace)))
}

fn read_small(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(METADATA_LIMIT)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(text)
}

fn canonical_directory(path: &Path, what: &str) -> Result<PathBuf, String> {
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("{what} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{what} {} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

/// `core.bare` and `core.worktree` of a git config file (config.c, the
/// subset this needs: `[core]` without a subsection, `key = value` lines,
/// `#`/`;` comments, quoted values, a bare key meaning true).
fn core_settings(config: &str) -> (bool, Option<String>) {
    let mut in_core = false;
    let mut bare = false;
    let mut worktree = None;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header
                .split([']', ' ', '\t', '"'])
                .next()
                .unwrap_or_default();
            in_core = name.eq_ignore_ascii_case("core") && !header.contains('"');
            continue;
        }
        if !in_core {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((key, value)) => (key.trim(), Some(config_value(value))),
            None => (line, None),
        };
        if key.eq_ignore_ascii_case("bare") {
            bare = value.as_deref().is_none_or(|value| {
                ["true", "yes", "on", "1"]
                    .iter()
                    .any(|word| value.eq_ignore_ascii_case(word))
            });
        } else if key.eq_ignore_ascii_case("worktree") {
            worktree = value.filter(|value| !value.is_empty());
        }
    }
    (bare, worktree)
}

fn config_value(value: &str) -> String {
    let value = value.trim();
    if let Some(quoted) = value.strip_prefix('"')
        && let Some((inside, _)) = quoted.split_once('"')
    {
        return inside.to_owned();
    }
    value
        .split(['#', ';'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The state directory name of `workspace`: `<name>-<hash>`.
pub fn state_name(workspace: &Path) -> String {
    let canonical = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let name: String = canonical
        .components()
        .next_back()
        .and_then(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(NAME_LIMIT)
        .collect();
    let name = if name.is_empty() || name.chars().all(|c| c == '.') {
        "root".to_owned()
    } else {
        name
    };
    format!("{name}-{hash}")
}

/// `<cache>/worktrees/<state_name(workspace)>`.
pub fn state_directory_for(cache: &Path, workspace: &Path) -> PathBuf {
    cache.join(WORKTREES).join(state_name(workspace))
}

/// The workspace of this process ([`install`]): its state directory name.
static CURRENT: OnceLock<String> = OnceLock::new();
/// An explicit Bazel output base replaces only mutable per-workspace state,
/// never the content-addressed cache shared by linked worktrees.
static OUTPUT_BASE: OnceLock<PathBuf> = OnceLock::new();

/// Install the startup `--output_base`. Resolve relative paths against the
/// client cwd, including existing symlink ancestors, without creating anything
/// (`info` is read-only). The CLI calls this once after rc startup evaluation.
pub fn install_output_base(cwd: &Path, path: &str) -> Result<(), String> {
    let path = resolve_output_base(cwd, path)?;
    OUTPUT_BASE
        .set(path)
        .map_err(|_| "output base was already installed".to_owned())
}

fn resolve_output_base(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("--output_base requires a nonempty path".to_owned());
    }
    let absolute = cwd.join(path);
    let mut resolved = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => {
                resolved.push(other);
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => {
                        if !canonical.is_dir() {
                            return Err(format!(
                                "output base {} is not a directory",
                                resolved.display()
                            ));
                        }
                        resolved = canonical;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("output base {}: {error}", resolved.display()));
                    }
                }
            }
        }
    }
    Ok(resolved)
}

/// Effective output base for a named workspace: the explicit startup value,
/// otherwise its default per-worktree directory. Unlike `state_directory_for`,
/// this honors the current invocation's override (not for foreign seed paths).
pub fn output_base_for(cache: &Path, workspace: &Path) -> PathBuf {
    OUTPUT_BASE
        .get()
        .cloned()
        .unwrap_or_else(|| state_directory_for(cache, workspace))
}

/// Make `workspace` the workspace of this process: [`state_directory`]
/// answers its state directory from now on. Called once by the CLI (one
/// process serves one workspace); never by tests, which share a process.
pub fn install(workspace: &Path) {
    let _ = CURRENT.set(state_name(workspace));
}

/// The per-worktree state directory of this process's workspace below
/// `cache` ([`install`]); without an installed workspace (library use and
/// unit tests) `cache` itself, the flat layout.
pub fn state_directory(cache: &Path) -> PathBuf {
    if let Some(path) = OUTPUT_BASE.get() {
        return path.clone();
    }
    match CURRENT.get() {
        Some(name) => cache.join(WORKTREES).join(name),
        None => cache.to_path_buf(),
    }
}

/// Where a workspace's path is named (docs/WORKTREES.md): in the bytes of a
/// file a repository rule wrote, the target of a link it left, an entry of
/// another workspace's hidden lockfile. The paths of the directories below
/// the workspace that every worktree shares — the cache of a main checkout,
/// `<workspace>/bazel-rs-out` — name no dependence on where the workspace
/// is: a worktree sharing that cache sees the very same paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceMentions {
    workspace: PathBuf,
    /// Directories strictly below the workspace (as spelled and canonical).
    excluded: Vec<PathBuf>,
}

impl WorkspaceMentions {
    /// The mentions of `workspace` (canonical) except those of the
    /// `excluded` directories' paths; one not below the workspace is never
    /// where a mention of it starts, so it is dropped.
    pub fn new(workspace: &Path, excluded: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut excluded: Vec<PathBuf> = excluded
            .into_iter()
            .filter(|path| path.as_path() != workspace && path.starts_with(workspace))
            .collect();
        excluded.sort();
        excluded.dedup();
        Self {
            workspace: workspace.to_path_buf(),
            excluded,
        }
    }

    /// The mentions of `workspace` except the paths into `cache` (the
    /// repository store, the cache root or any directory of it), spelled as
    /// given and canonical.
    pub fn except_cache(workspace: &Path, cache: &Path) -> Self {
        let canonical = fs::canonicalize(cache).ok();
        Self::new(
            workspace,
            [Some(cache.to_path_buf()), canonical].into_iter().flatten(),
        )
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Whether `bytes` name the workspace: its path at a boundary (followed
    /// by the end or by a byte that continues no file name: anything but an
    /// ASCII letter or digit, `_` and `-`; so `/a/code-2` does not name
    /// `/a/code`) other than as the start of an excluded directory's path at
    /// a boundary. The file system root names nothing apart.
    pub fn named_in(&self, bytes: &[u8]) -> bool {
        let needle = self.workspace.as_os_str().as_encoded_bytes();
        let Some((&first, _)) = needle.split_first().filter(|_| needle.len() > 1) else {
            return false;
        };
        let mut from = 0;
        while let Some(offset) = bytes[from..].iter().position(|&byte| byte == first) {
            let rest = &bytes[from + offset..];
            if rest.starts_with(needle)
                && at_boundary(rest, needle.len())
                && !self.excluded.iter().any(|excluded| {
                    let excluded = excluded.as_os_str().as_encoded_bytes();
                    rest.starts_with(excluded) && at_boundary(rest, excluded.len())
                })
            {
                return true;
            }
            from += offset + 1;
        }
        false
    }

    /// Whether `path` — `..` resolved lexically, never through the file
    /// system — is the workspace or below it, and not an excluded directory
    /// or below one.
    pub fn contains(&self, path: &Path) -> bool {
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other),
            }
        }
        resolved.starts_with(&self.workspace)
            && !self
                .excluded
                .iter()
                .any(|excluded| resolved.starts_with(excluded))
    }
}

/// Whether the path `rest[..len]` ends at a file name boundary.
fn at_boundary(rest: &[u8], len: usize) -> bool {
    rest.get(len)
        .is_none_or(|&byte| !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
}

/// Where a workspace's hidden lockfile may be seeded from when its own
/// does not exist yet: `path`, and for another workspace's lockfile the
/// mentions of that workspace's path, whose entries must be dropped (they
/// would point this workspace's repositories into the other one); None:
/// the same workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    pub path: PathBuf,
    pub foreign: Option<WorkspaceMentions>,
}

/// The seeds of the hidden lockfile of `layout`'s workspace in `cache`, in
/// order: the workspace's own hidden lockfile of the flat layout (before
/// per-worktree state), then — in a linked worktree — the main checkout's,
/// in the per-worktree and the flat layout. `legacy` is the flat layout's
/// hidden lockfile path of a workspace.
pub fn hidden_lockfile_seeds(
    cache: &Path,
    layout: &Layout,
    name: &str,
    legacy: impl Fn(&Path) -> PathBuf,
) -> Vec<Seed> {
    let mut seeds = vec![Seed {
        path: legacy(&layout.workspace),
        foreign: None,
    }];
    if let Some(main) = layout.main_workspace() {
        let foreign = WorkspaceMentions::except_cache(main, cache);
        seeds.push(Seed {
            path: state_directory_for(cache, main).join(name),
            foreign: Some(foreign.clone()),
        });
        seeds.push(Seed {
            path: legacy(main),
            foreign: Some(foreign),
        });
    }
    seeds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory tree (canonical root).
    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(
                tempfile::Builder::new()
                    .prefix("worktree-layout-")
                    .tempdir_in("/tmp/opencode")
                    .unwrap(),
            )
        }
        fn root(&self) -> PathBuf {
            fs::canonicalize(self.0.path()).unwrap()
        }
        fn dir(&self, path: &str) -> PathBuf {
            let path = self.root().join(path);
            fs::create_dir_all(&path).unwrap();
            path
        }
        fn file(&self, path: &str, text: &str) -> PathBuf {
            let path = self.root().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
        /// A main checkout `main` (non-bare `.git`) with a linked worktree
        /// `wt` laid out like `git worktree add` does.
        fn repository(&self) -> (PathBuf, PathBuf) {
            let main = self.dir("main");
            self.file(
                "main/.git/config",
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
            );
            self.dir("main/.git/worktrees/wt");
            self.file("main/.git/worktrees/wt/commondir", "../..\n");
            let wt = self.dir("wt");
            self.file(
                "wt/.git",
                &format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
            );
            (main, wt)
        }
    }

    #[test]
    fn a_main_checkout_keeps_its_own_cache() {
        let tree = Tree::new();
        let (main, _) = tree.repository();
        let layout = detect(&main);
        assert_eq!(layout.checkout, Checkout::Main { root: main.clone() });
        assert_eq!(layout.default_cache(), main.join("bazel-rs-out"));
        // A workspace below the main checkout's root.
        let nested = tree.dir("main/sub/ws");
        assert_eq!(detect(&nested).default_cache(), nested.join("bazel-rs-out"));
        // No git at all.
        let plain = tree.dir("plain");
        let layout = detect(&plain);
        // (Unless the scratch directory itself lies in a checkout.)
        if !Path::new("/tmp/opencode/.git").exists() && !Path::new("/tmp/.git").exists() {
            assert_eq!(layout.checkout, Checkout::NoGit);
        }
        assert_eq!(layout.default_cache(), plain.join("bazel-rs-out"));
    }

    #[test]
    fn a_linked_worktree_shares_the_main_checkouts_cache() {
        let tree = Tree::new();
        let (main, wt) = tree.repository();
        let layout = detect(&wt);
        assert_eq!(
            layout.checkout,
            Checkout::Linked {
                root: wt.clone(),
                main: main.clone(),
                main_workspace: main.clone(),
            }
        );
        assert_eq!(layout.default_cache(), main.join("bazel-rs-out"));
        assert_eq!(layout.main_workspace(), Some(main.as_path()));
        // A relative gitdir (git worktree add --relative-paths) and CRLF.
        tree.file("wt/.git", "gitdir: ../main/.git/worktrees/wt\r\n");
        tree.file("main/.git/worktrees/wt/commondir", "../..\r\n");
        assert_eq!(detect(&wt).default_cache(), main.join("bazel-rs-out"));
        // core.worktree names the main checkout (a submodule's worktrees).
        let elsewhere = tree.dir("elsewhere");
        tree.file(
            "main/.git/config",
            &format!("[core]\n\tworktree = {}\n", elsewhere.display()),
        );
        assert_eq!(detect(&wt).default_cache(), elsewhere.join("bazel-rs-out"));
        tree.file(
            "main/.git/config",
            "[core]\n\tworktree = ../../elsewhere ; c\n",
        );
        assert_eq!(detect(&wt).default_cache(), elsewhere.join("bazel-rs-out"));
    }

    #[test]
    fn a_nested_workspace_maps_to_the_same_directory_of_the_main_checkout() {
        let tree = Tree::new();
        let (main, _) = tree.repository();
        let sub = tree.dir("wt/src/ws");
        tree.dir("main/src/ws");
        let layout = detect(&sub);
        assert_eq!(layout.main_workspace(), Some(main.join("src/ws").as_path()));
        assert_eq!(layout.default_cache(), main.join("src/ws/bazel-rs-out"));
        // The main checkout lacks the workspace (added on the worktree's
        // branch): the worktree keeps its own cache.
        let added = tree.dir("wt/added");
        let layout = detect(&added);
        assert!(
            matches!(&layout.checkout, Checkout::Unusable { reason, .. } if reason.contains("the main checkout's workspace")),
            "{layout:?}"
        );
        assert_eq!(layout.default_cache(), added.join("bazel-rs-out"));
    }

    #[test]
    fn bare_repositories_and_malformed_metadata_fall_back_to_the_workspace() {
        let tree = Tree::new();
        // A worktree of a bare repository.
        tree.file("repo.git/config", "[core]\n\tbare = true\n");
        tree.file("repo.git/worktrees/wt/commondir", "../..\n");
        let wt = tree.dir("bare-wt");
        tree.file(
            "bare-wt/.git",
            &format!(
                "gitdir: {}\n",
                tree.root().join("repo.git/worktrees/wt").display()
            ),
        );
        let fallback = |workspace: &Path, expected: &str| {
            let layout = detect(workspace);
            match &layout.checkout {
                Checkout::Unusable { reason, .. } => {
                    assert!(reason.contains(expected), "{reason}")
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(layout.default_cache(), workspace.join("bazel-rs-out"));
        };
        fallback(&wt, "is a bare repository");
        // `bare` alone means true; a common directory not named `.git`.
        tree.file("repo.git/config", "[core]\n\tbare\n");
        fallback(&wt, "is a bare repository");
        tree.file("repo.git/config", "[core]\n\tbare = false\n");
        fallback(&wt, "is not the `.git` directory of a main checkout");
        // Malformed `.git` files.
        let bad = tree.dir("bad");
        for (text, expected) in [
            ("garbage\n", "has no `gitdir:` line"),
            ("gitdir:   \n", "names no git directory"),
            ("gitdir: /nonexistent/x/y\n", "the git directory"),
        ] {
            tree.file("bad/.git", text);
            fallback(&bad, expected);
        }
        // An empty commondir; one naming nothing.
        tree.dir("gd");
        tree.file(
            "bad/.git",
            &format!("gitdir: {}\n", tree.root().join("gd").display()),
        );
        tree.file("gd/commondir", "\n");
        fallback(&bad, "is empty");
        tree.file("gd/commondir", "../missing\n");
        fallback(&bad, "the common git directory");
        // A `.git` file without commondir is a main checkout (a submodule,
        // --separate-git-dir).
        fs::remove_file(tree.root().join("gd/commondir")).unwrap();
        assert_eq!(detect(&bad).checkout, Checkout::Main { root: bad.clone() });
        // A dangling `.git` link.
        let dangling = tree.dir("dangling");
        std::os::unix::fs::symlink("/nonexistent/.git", dangling.join(".git")).unwrap();
        fallback(&dangling, "/.git");
    }

    #[test]
    fn a_symlinked_worktree_path_resolves_to_the_same_cache() {
        let tree = Tree::new();
        let (main, wt) = tree.repository();
        let link = tree.root().join("link-to-wt");
        std::os::unix::fs::symlink(&wt, &link).unwrap();
        let layout = detect(&link);
        assert_eq!(layout.workspace, wt);
        assert_eq!(layout.default_cache(), main.join("bazel-rs-out"));
        // The main checkout reached through a symlinked `.git`.
        let real_git = tree.root().join("main/.git");
        let moved = tree.root().join("moved.git");
        fs::rename(&real_git, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &real_git).unwrap();
        // The common directory is now `moved.git`, no `.git`: unusable,
        // unless core.worktree says where the main checkout is.
        let layout = detect(&link);
        assert!(
            matches!(layout.checkout, Checkout::Unusable { .. }),
            "{layout:?}"
        );
        tree.file(
            "moved.git/config",
            &format!("[core]\nworktree = {}\n", main.display()),
        );
        assert_eq!(detect(&link).default_cache(), main.join("bazel-rs-out"));
        // State directories name the canonical path.
        assert_eq!(state_name(&link), state_name(&wt));
    }

    #[test]
    fn the_legacy_cache_note_names_only_a_linked_worktrees_old_cache() {
        let tree = Tree::new();
        let (main, wt) = tree.repository();
        let layout = detect(&wt);
        let cache = layout.default_cache();
        assert_eq!(layout.legacy_cache_note(&cache), None);
        tree.dir("wt/bazel-rs-out/repositories");
        let note = layout.legacy_cache_note(&cache).unwrap();
        assert!(
            note.contains(&format!(
                "{} is no longer used",
                wt.join("bazel-rs-out").display()
            )),
            "{note}"
        );
        // Not when the old cache is the one in use, nor in a main checkout.
        assert_eq!(layout.legacy_cache_note(&wt.join("bazel-rs-out")), None);
        tree.dir("main/bazel-rs-out");
        let main_layout = detect(&main);
        assert_eq!(
            main_layout.legacy_cache_note(&main_layout.default_cache()),
            None
        );
    }

    #[test]
    fn state_directories_are_named_and_hashed_by_the_canonical_workspace() {
        let tree = Tree::new();
        let a = tree.dir("x/code");
        let b = tree.dir("y/code");
        let (na, nb) = (state_name(&a), state_name(&b));
        assert!(
            na.starts_with("code-") && nb.starts_with("code-"),
            "{na} {nb}"
        );
        assert_eq!(na.len(), "code-".len() + 16);
        assert_ne!(na, nb);
        assert_eq!(state_name(&a), na);
        let odd = tree.dir("we ird:näme");
        assert!(
            state_name(&odd).starts_with("we_ird_n_me-"),
            "{}",
            state_name(&odd)
        );
        let long = tree.dir(&"l".repeat(100));
        assert!(state_name(&long).starts_with(&format!("{}-", "l".repeat(40))));
        assert!(state_name(Path::new("/")).starts_with("root-"));
        assert_eq!(
            state_directory_for(Path::new("/c"), &a),
            Path::new("/c/worktrees").join(&na)
        );
        let layout = detect(&a);
        let seeds = hidden_lockfile_seeds(Path::new("/c"), &layout, "L", |ws| ws.join("legacy"));
        assert_eq!(
            seeds,
            [Seed {
                path: a.join("legacy"),
                foreign: None
            }]
        );
        let (main, wt) = tree.repository();
        let cache = main.join("bazel-rs-out");
        let seeds = hidden_lockfile_seeds(&cache, &detect(&wt), "L", |ws| ws.join("legacy"));
        // The main checkout's entries are foreign, except where they name
        // the shared cache inside it.
        let foreign = WorkspaceMentions::new(&main, [cache.clone()]);
        assert_eq!(
            seeds,
            [
                Seed {
                    path: wt.join("legacy"),
                    foreign: None
                },
                Seed {
                    path: state_directory_for(&cache, &main).join("L"),
                    foreign: Some(foreign.clone())
                },
                Seed {
                    path: main.join("legacy"),
                    foreign: Some(foreign)
                },
            ]
        );
    }

    /// docs/WORKTREES.md: a workspace's path is named at a file name
    /// boundary, never by a sibling sharing its prefix, and never by a path
    /// into the cache a main checkout holds (every worktree shares it).
    #[test]
    fn workspace_mentions_skip_siblings_and_the_shared_cache() {
        let mentions = WorkspaceMentions::new(
            Path::new("/home/u/code"),
            [
                PathBuf::from("/home/u/code/bazel-rs-out"),
                // Not below the workspace: never where a mention starts.
                PathBuf::from("/elsewhere/cache"),
                PathBuf::from("/home/u/code"),
            ],
        );
        assert_eq!(
            mentions.excluded,
            [PathBuf::from("/home/u/code/bazel-rs-out")]
        );
        for named in [
            &b"/home/u/code"[..],
            b"root = '/home/u/code'
",
            b"x /home/u/code/src/a.go y",
            b"/home/u/code:/usr/bin",
            b"/home/u/code.bak",
            // The cache's sibling, and a mention after an excluded one.
            b"/home/u/code/bazel-rs-out2/x",
            b"/home/u/code/bazel-rs-out/x /home/u/code/y",
        ] {
            assert!(
                mentions.named_in(named),
                "{}",
                String::from_utf8_lossy(named)
            );
        }
        for unnamed in [
            &b""[..],
            b"/home/u/cod",
            b"/home/u/code-wt1/src",
            b"/home/u/code_old",
            b"/home/u/code2",
            b"GOPATH=/home/u/code/bazel-rs-out/repositories/direct/stages/x
",
            b"/home/u/code/bazel-rs-out",
            b"/home/u/code/bazel-rs-out:/home/u/code-2",
        ] {
            assert!(
                !mentions.named_in(unnamed),
                "{}",
                String::from_utf8_lossy(unnamed)
            );
        }
        assert!(!WorkspaceMentions::new(Path::new("/"), []).named_in(b"/x"));
        for (path, inside) in [
            ("/home/u/code", true),
            ("/home/u/code/src/a.go", true),
            ("/home/u/code/sub/../a", true),
            ("/home/u/code/../code/a", true),
            ("/home/u/code/..", false),
            ("/home/u/code-wt1/a", false),
            ("/home/u/code/bazel-rs-out", false),
            ("/home/u/code/bazel-rs-out/repositories/x", false),
            ("/home/u/code/bazel-rs-out/../src", true),
            ("/home/u/code/bazel-rs-out2", true),
        ] {
            assert_eq!(mentions.contains(Path::new(path)), inside, "{path}");
        }
    }
}
