//! The execution root's output tree (docs/INFO.md): `bazel info
//! execution_root`, `output_path` and `bazel-bin`/`bazel-genfiles`/
//! `bazel-testlogs` name directories below `<state>/execroot/_main`, the
//! native counterpart of Bazel's `<output_base>/execroot/<workspace>`
//! (BlazeDirectories.getExecRoot; the main repository's directory is `_main`
//! under Bzlmod). `<state>` is the workspace's state directory in the
//! cache, `<cache>/worktrees/<name>-<hash>` (crate::worktree,
//! docs/WORKTREES.md): the git worktrees sharing one cache each have their
//! own output tree, so one worktree's build never relinks another's
//! `bazel-bin` (the outputs themselves stay shared in the action cache).
//!
//! Native action outputs live in per-action cache entries
//! (`<cache>/actions/<key>/outputs/<exec path>`, docs/ACTION_OUTPUTS.md),
//! not in one tree. After an executing `build`, `test` or `run`
//! ([`publish`]) every output of the request's actions — executed or a
//! cache hit — is linked at its exec path below the execution root, so
//! `$(bazel-rs info bazel-bin)/pkg/tool` names the built file, as with
//! Bazel's output tree:
//!
//! * a regular file is a hardlink of the published output (one inode: the
//!   outputs are read-only, `0555`), so `/proc/self/exe` of a binary
//!   started through the tree is the tree path and its `.runfiles` sibling
//!   resolves; a symlink is the fallback when the hardlink fails (another
//!   filesystem, the inode's link limit);
//! * a directory output (a tree artifact) and an output symlink are a
//!   symlink to the published path;
//! * a runfiles tree (`Transform::RunfilesTree`, whose only declared output
//!   is `<exe>.runfiles/MANIFEST`) links the whole `<exe>.runfiles`
//!   directory.
//!
//! An entry already linking the same output is left alone (one `lstat` or
//! `readlink`), anything else at the path is replaced atomically (a
//! temporary link renamed over it). Entries of earlier builds stay until a
//! later build replaces them, like Bazel's output tree. Failing to update
//! the tree never fails the build (the caller warns).

use crate::action::ActionResult;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

/// The main repository's execution root directory name under Bzlmod
/// (RepositoryUtils.WORKSPACE_SYMLINK_NAME, bazel/repository/RepositoryUtils.java:41).
pub const MAIN_DIRECTORY: &str = "_main";

/// `<state>/execroot/_main` of this process's workspace
/// (crate::worktree::state_directory; without an installed workspace,
/// `<cache>/execroot/_main`).
pub fn execution_root(cache: &Path) -> PathBuf {
    execution_root_below(&crate::worktree::state_directory(cache))
}

/// `<output_base>/execroot/_main` (BlazeDirectories.getExecRoot), for the
/// native output base: a workspace's state directory.
pub fn execution_root_below(output_base: &Path) -> PathBuf {
    output_base.join("execroot").join(MAIN_DIRECTORY)
}

/// `<state>/execroot/_main/bazel-out` of this process's workspace
/// ([`execution_root`]).
pub fn output_path(cache: &Path) -> PathBuf {
    execution_root(cache).join("bazel-out")
}

/// `<output_base>/execroot/_main/bazel-out` (BlazeDirectories.getOutputPath).
pub fn output_path_below(output_base: &Path) -> PathBuf {
    execution_root_below(output_base).join("bazel-out")
}

/// The outcome of one [`publish`].
#[derive(Debug, Default)]
pub struct Published {
    /// Entries created or replaced.
    pub linked: usize,
    /// Entries that already linked their output.
    pub unchanged: usize,
    /// The first failure (the remaining outputs are still linked).
    pub error: Option<String>,
}

/// Link every output of `results` at its exec path below the execution root
/// of `cache`. `runfiles_trees` are the ids of the request's
/// `Transform::RunfilesTree` actions.
pub fn publish(
    cache: &Path,
    results: &BTreeMap<String, ActionResult>,
    runfiles_trees: &HashSet<String>,
) -> Published {
    let root = execution_root(cache);
    let actions = cache.join("actions");
    let mut tree = Tree {
        root: root.clone(),
        verified: HashSet::new(),
        published: Published::default(),
    };
    for (id, result) in results {
        let runfiles_tree = runfiles_trees.contains(id);
        for output in &result.outputs {
            let Some(exec) = exec_path(&actions, output) else {
                continue;
            };
            let (source, exec) = if runfiles_tree {
                // `<exe>.runfiles/MANIFEST`: the whole directory.
                match (output.parent(), exec.parent()) {
                    (Some(source), Some(exec)) => (source.to_path_buf(), exec.to_path_buf()),
                    _ => continue,
                }
            } else {
                (output.clone(), exec)
            };
            let destination = root.join(&exec);
            if let Err(error) = tree.link(&source, &destination) {
                tree.published.error.get_or_insert(error);
            }
        }
    }
    tree.published
}

/// Link one published directory at its exec path `exec` (below
/// `bazel-out/`) in the output tree of `cache`: a test action's testlogs
/// directory, published below `<cache>/tests/<key>/` (crate::test_runner).
pub fn link_directory(cache: &Path, exec: &Path, source: &Path) -> Result<(), String> {
    if exec.components().next() != Some(Component::Normal("bazel-out".as_ref()))
        || exec
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "internal error: {} is no output exec path",
            exec.display()
        ));
    }
    let root = execution_root(cache);
    let mut tree = Tree {
        root: root.clone(),
        verified: HashSet::new(),
        published: Published::default(),
    };
    tree.link(source, &root.join(exec))
}

/// The exec path of a published action output
/// `<cache>/actions/<key>/outputs/<exec path>`; only paths below
/// `bazel-out/` (every action output's root).
fn exec_path(actions: &Path, output: &Path) -> Option<PathBuf> {
    let relative = output.strip_prefix(actions).ok()?;
    let mut components = relative.components();
    let _key = components.next()?;
    if components.next()? != Component::Normal("outputs".as_ref()) {
        return None;
    }
    let exec = components.as_path();
    let mut parts = exec.components();
    if parts.next()? != Component::Normal("bazel-out".as_ref()) {
        return None;
    }
    // Exec paths are normalized; refuse anything that could leave the tree.
    if exec
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(exec.to_path_buf())
}

struct Tree {
    root: PathBuf,
    /// Directories of the tree known to be real directories in this run.
    verified: HashSet<PathBuf>,
    published: Published,
}

impl Tree {
    fn link(&mut self, source: &Path, destination: &Path) -> Result<(), String> {
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            // Nothing published at this path (never expected): nothing to link.
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io("cannot inspect", source, &error)),
        };
        let parent = destination
            .parent()
            .ok_or_else(|| format!("output tree path {} has no parent", destination.display()))?;
        self.real_directory(parent)?;
        let current = fs::symlink_metadata(destination).ok();
        if metadata.file_type().is_file() {
            if current.as_ref().is_some_and(|current| {
                current.file_type().is_file()
                    && current.dev() == metadata.dev()
                    && current.ino() == metadata.ino()
            }) {
                self.published.unchanged += 1;
                return Ok(());
            }
            let temporary = temporary(destination);
            let _ = fs::remove_file(&temporary);
            if fs::hard_link(source, &temporary).is_ok() {
                return self.replace(&temporary, destination);
            }
        } else if current
            .as_ref()
            .is_some_and(|current| current.file_type().is_symlink())
            && fs::read_link(destination).is_ok_and(|target| target == source)
        {
            self.published.unchanged += 1;
            return Ok(());
        }
        // A directory, a symlink, or a file that cannot be hardlinked.
        let temporary = temporary(destination);
        let _ = fs::remove_file(&temporary);
        std::os::unix::fs::symlink(source, &temporary)
            .map_err(|error| io("cannot link", &temporary, &error))?;
        self.replace(&temporary, destination)
    }

    /// Rename `temporary` over `destination`; a real directory there (an
    /// earlier build's tree below a path that is now one output) is removed
    /// first.
    fn replace(&mut self, temporary: &Path, destination: &Path) -> Result<(), String> {
        let renamed = fs::rename(temporary, destination).or_else(|error| {
            let directory =
                fs::symlink_metadata(destination).is_ok_and(|current| current.file_type().is_dir());
            if !directory {
                return Err(error);
            }
            fs::remove_dir_all(destination)?;
            fs::rename(temporary, destination)
        });
        match renamed {
            Ok(()) => {
                self.published.linked += 1;
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
                Err(io("cannot publish", destination, &error))
            }
        }
    }

    /// Make `directory` (below the tree root) a real directory: never write
    /// through a symlink an earlier build left (a tree artifact or runfiles
    /// directory link), which points into the action cache.
    fn real_directory(&mut self, directory: &Path) -> Result<(), String> {
        if self.verified.contains(directory) {
            return Ok(());
        }
        if directory == self.root || !directory.starts_with(&self.root) {
            fs::create_dir_all(directory)
                .map_err(|error| io("cannot create directory", directory, &error))?;
            self.verified.insert(directory.to_path_buf());
            return Ok(());
        }
        if let Some(parent) = directory.parent() {
            self.real_directory(parent)?;
        }
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                fs::remove_file(directory)
                    .map_err(|error| io("cannot replace", directory, &error))?;
                create_directory(directory)?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => create_directory(directory)?,
            Err(error) => return Err(io("cannot inspect", directory, &error)),
        }
        self.verified.insert(directory.to_path_buf());
        Ok(())
    }
}

fn create_directory(directory: &Path) -> Result<(), String> {
    match fs::create_dir(directory) {
        // A concurrent command created it.
        Err(error) if error.kind() != ErrorKind::AlreadyExists => {
            Err(io("cannot create directory", directory, &error))
        }
        _ => Ok(()),
    }
}

/// A sibling temporary name for `destination`, private to this process.
fn temporary(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    destination.with_file_name(format!(".{name}.bazel-rs-tmp.{}", std::process::id()))
}

fn io(what: &str, path: &Path, error: &std::io::Error) -> String {
    format!("{what} '{}': {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outputs: &[&Path]) -> ActionResult {
        ActionResult {
            outputs: outputs.iter().map(|path| path.to_path_buf()).collect(),
            cache_hit: false,
            remote_hit: false,
            trees: BTreeMap::new(),
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn exec_paths_are_below_bazel_out_only() {
        let actions = Path::new("/c/actions");
        assert_eq!(
            exec_path(
                actions,
                Path::new("/c/actions/k/outputs/bazel-out/k8-fastbuild/bin/p/f")
            ),
            Some(PathBuf::from("bazel-out/k8-fastbuild/bin/p/f"))
        );
        assert_eq!(
            exec_path(actions, Path::new("/c/actions/k/outputs/elsewhere/f")),
            None
        );
        assert_eq!(
            exec_path(actions, Path::new("/c/actions/k/other/bazel-out/f")),
            None
        );
        assert_eq!(exec_path(actions, Path::new("/d/bazel-out/f")), None);
    }

    #[test]
    fn publishes_files_trees_and_runfiles_directories() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        let bin = "bazel-out/k8-fastbuild/bin";
        let file = cache.join(format!("actions/a/outputs/{bin}/pkg/tool"));
        write(&file, "tool");
        let tree = cache.join(format!("actions/b/outputs/{bin}/pkg/tree"));
        write(&tree.join("x"), "x");
        let manifest = cache.join(format!(
            "actions/c/outputs/{bin}/pkg/tool.runfiles/MANIFEST"
        ));
        write(&manifest, "");
        write(&manifest.with_file_name("entry"), "e");
        let mut results = BTreeMap::new();
        results.insert("a".to_owned(), result(&[&file]));
        results.insert("b".to_owned(), result(&[&tree]));
        results.insert("c".to_owned(), result(&[&manifest]));
        let trees = HashSet::from(["c".to_owned()]);
        let published = publish(cache, &results, &trees);
        assert_eq!(published.error, None);
        assert_eq!(published.linked, 3);
        let out = output_path(cache).join("k8-fastbuild/bin/pkg");
        let linked = fs::symlink_metadata(out.join("tool")).unwrap();
        assert!(linked.file_type().is_file());
        assert_eq!(linked.ino(), fs::metadata(&file).unwrap().ino());
        assert_eq!(fs::read_link(out.join("tree")).unwrap(), tree);
        assert_eq!(fs::read_to_string(out.join("tree/x")).unwrap(), "x");
        assert_eq!(
            fs::read_to_string(out.join("tool.runfiles/entry")).unwrap(),
            "e"
        );
        // Unchanged entries are left alone.
        let again = publish(cache, &results, &trees);
        assert_eq!((again.linked, again.unchanged), (0, 3));
    }

    #[test]
    fn replaces_earlier_entries_without_writing_through_links() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        let bin = "bazel-out/k8-fastbuild/bin";
        // An earlier build's tree artifact at pkg/d, now a package
        // directory holding a file output.
        let tree = cache.join(format!("actions/old/outputs/{bin}/pkg/d"));
        write(&tree.join("keep"), "old");
        let mut results = BTreeMap::new();
        results.insert("old".to_owned(), result(&[&tree]));
        assert_eq!(publish(cache, &results, &HashSet::new()).error, None);
        let file = cache.join(format!("actions/new/outputs/{bin}/pkg/d/f"));
        write(&file, "new");
        // An earlier real directory where a file output now is.
        let replaced = cache.join(format!("actions/new/outputs/{bin}/pkg/g"));
        write(&replaced, "g");
        let stale = output_path(cache).join("k8-fastbuild/bin/pkg/g/stale");
        write(&stale, "stale");
        let mut results = BTreeMap::new();
        results.insert("new".to_owned(), result(&[&file, &replaced]));
        let published = publish(cache, &results, &HashSet::new());
        assert_eq!(published.error, None);
        let out = output_path(cache).join("k8-fastbuild/bin/pkg");
        assert!(!fs::symlink_metadata(out.join("d")).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(out.join("d/f")).unwrap(), "new");
        // The old action entry was not written through.
        assert!(!tree.join("f").exists());
        assert_eq!(fs::read_to_string(out.join("g")).unwrap(), "g");
    }
}
