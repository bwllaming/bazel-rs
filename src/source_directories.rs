//! Source directories in an executing build (docs/SOURCE_DIRECTORIES.md).
//!
//! A source artifact may name a directory (a package directory used as a file
//! target, an external repository's directory such as rules_js's npm
//! `package`). Bazel 9.3 tracks such a directory by everything below it:
//! source directory tracking is on by default in Bazel
//! (skyframe/TrackSourceDirectoriesFlag.java: `BAZEL_TRACK_SOURCE_DIRECTORIES`
//! unset), so ArtifactFunction.createSourceValue
//! (skyframe/ArtifactFunction.java:274-364) traverses it with
//! RecursiveFilesystemTraversalFunction whenever an executing build needs
//! its value — as an action input, a requested output or a runfile — and
//! fingerprints every entry. The executor keys and stages the directory
//! (crate::action, `SourceDirectory`); this module reports the traversal's
//! failures with Bazel's texts before anything executes, the way the
//! planner reports missing input files.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::ignored::IgnoredSubdirectories;
use crate::observed_fs::{self, Projection};

/// A failed traversal: Bazel's separate error events (printed before the
/// consumer's errors) and the SourceArtifactException message
/// `error traversing directory 'LABEL': MESSAGE`
/// (ArtifactFunction.SourceArtifactException.create :545-557).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraversalError {
    pub events: Vec<String>,
    pub message: String,
}

/// The main repository's package boundaries below a source directory:
/// RecursiveFilesystemTraversalFunction.checkIfPackage tests every
/// subdirectory (`forChildEntry` clears skipTestingForSubpackage) of a
/// main-repository source directory for a package. External repositories are
/// not checked by default (`--incompatible_check_external_repo_source_dir_
/// package_boundary`, BuildLanguageOptions.java:767-778, default false).
pub struct Boundaries<'a> {
    pub ignored: &'a IgnoredSubdirectories,
}

/// Traverse the source directory `real` (its resolved path) named by
/// `label` (its canonical form) at the root-relative path `root_relative`
/// (the label's package path and name; the traversal's logical root), as
/// ArtifactFunction.createSourceValue does when the build needs its value.
/// `boundaries` is set for a main-repository directory. Entries are visited
/// in byte order, depth first; the first failure is reported (Bazel
/// evaluates the children in parallel, so with several failures its choice
/// may differ).
pub fn traversal_error(
    real: &Path,
    label: &str,
    root_relative: &str,
    boundaries: Option<&Boundaries<'_>>,
) -> Option<TraversalError> {
    let fail = |events: Vec<String>, message: String| {
        Some(TraversalError {
            events,
            message: format!("error traversing directory '{label}': {message}"),
        })
    };
    if let Err(error) = observed_fs::metadata(real) {
        return fail(
            Vec::new(),
            format!("Error while traversing directory {root_relative}: {error}"),
        );
    }
    // The directories the root lies in (itself included): a link back into
    // any of them expands without bound (FileFunction checks a link's target
    // against every ancestor of the link's path).
    let canonical = observed_fs::canonicalize(real).unwrap_or_else(|_| real.to_path_buf());
    let root_ancestors = canonical
        .ancestors()
        .filter_map(|ancestor| observed_fs::metadata(ancestor).ok())
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .collect::<Vec<_>>();
    // Directories to list: the logical path relative to the root, the real
    // path, and the identities of the directories it lies in (itself
    // included).
    type Pending = (PathBuf, PathBuf, Vec<(u64, u64)>);
    let mut pending: Vec<Pending> = vec![(PathBuf::new(), real.to_path_buf(), root_ancestors)];
    let logical = |relative: &Path| -> String {
        if relative.as_os_str().is_empty() {
            root_relative.to_owned()
        } else if root_relative.is_empty() {
            relative.display().to_string()
        } else {
            format!("{root_relative}/{}", relative.display())
        }
    };
    while let Some((relative, directory, ancestors)) = pending.pop() {
        let entries = match observed_fs::read_dir_projected(&directory, &Projection::All) {
            Ok(entries) => entries,
            Err(error) => {
                return fail(
                    Vec::new(),
                    format!(
                        "Error while traversing directory {}: {error}",
                        logical(&relative)
                    ),
                );
            }
        };
        let mut names = BTreeSet::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    names.insert(entry.file_name());
                }
                Err(error) => {
                    return fail(
                        Vec::new(),
                        format!(
                            "Error while traversing directory {}: {error}",
                            logical(&relative)
                        ),
                    );
                }
            }
        }
        // Pushed in reverse so that the depth-first walk visits names in
        // byte order.
        let mut subdirectories = Vec::new();
        for name in names {
            let child = relative.join(&name);
            let path = directory.join(&name);
            let error = |message: &str| {
                format!(
                    "Error while traversing directory {}: {message}",
                    logical(&child)
                )
            };
            let link = match observed_fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.file_type().is_symlink(),
                Err(e) => return fail(Vec::new(), error(&e.to_string())),
            };
            let metadata = match observed_fs::metadata(&path) {
                Ok(metadata) => metadata,
                // A dangling symlink is an entry (ResolvedFileFactory
                // .danglingSymlink), never an error.
                Err(e)
                    if link
                        && (e.kind() == std::io::ErrorKind::NotFound
                            || e.raw_os_error()
                                == Some(rustix::io::Errno::NOTDIR.raw_os_error())) =>
                {
                    continue;
                }
                // FileSymlinkCycleException (FileFunction), the cycle
                // reported once (FileSymlinkCycleUniquenessFunction).
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
                    let events = symlink_cycle(&path)
                        .map(|cycle| {
                            let mut event =
                                "circular symlinks detected\n[start of symlink cycle]\n".to_owned();
                            for element in cycle {
                                event.push_str(&format!("{}\n", element.display()));
                            }
                            event.push_str("[end of symlink cycle]");
                            event
                        })
                        .into_iter()
                        .collect();
                    return fail(events, error("Symlink cycle"));
                }
                Err(e) => return fail(Vec::new(), error(&e.to_string())),
            };
            if !metadata.is_dir() {
                continue;
            }
            let identity = (metadata.dev(), metadata.ino());
            if ancestors.contains(&identity) {
                // FileSymlinkInfiniteExpansionException: a link into a
                // directory it lies in, reported once as a chain from the
                // directory the link resolves to (FileSymlinkInfiniteExpansion
                // UniquenessFunction).
                let target = observed_fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let event = format!(
                    "infinite symlink expansion detected\n[start of symlink chain]\n{}\n{}\n[end of symlink chain]",
                    target.display(),
                    path.display()
                );
                return fail(vec![event], error("Infinite symlink expansion"));
            }
            if let Some(boundaries) = boundaries {
                let package = logical(&child);
                if boundaries.ignored.matching_entry(&package).is_none()
                    && !crate::ignored::is_deleted_directory(&path)
                    && crate::label::is_valid_package_name(&package)
                    && (observed_fs::is_file(path.join("BUILD.bazel"))
                        || observed_fs::is_file(path.join("BUILD")))
                {
                    // RecursiveFilesystemTraversalFunction.compute :232-242
                    // (errorInfo of DirectoryArtifactTraversalRequest: the
                    // artifact's root-relative path).
                    return fail(
                        Vec::new(),
                        format!(
                            "Directory artifact {root_relative} crosses package boundary into package rooted at {package}"
                        ),
                    );
                }
            }
            let mut inner = ancestors.clone();
            inner.push(identity);
            subdirectories.push((child, path, inner));
        }
        pending.extend(subdirectories.into_iter().rev());
    }
    None
}

/// The cycle of symlinks `start` leads into, each link's target joined
/// lexically to its directory, rotated to start at its smallest path
/// (AbstractFileChainUniquenessFunction.canonicalize); `None` when the chain
/// does not close within 40 links (the kernel's limit).
fn symlink_cycle(start: &Path) -> Option<Vec<PathBuf>> {
    let mut chain: Vec<PathBuf> = vec![start.to_path_buf()];
    for _ in 0..40 {
        let current = chain.last()?;
        let target = observed_fs::read_link(current).ok()?;
        let mut next = current.parent()?.to_path_buf();
        for component in target.components() {
            match component {
                std::path::Component::RootDir => next = PathBuf::from("/"),
                std::path::Component::ParentDir => {
                    next.pop();
                }
                std::path::Component::Normal(name) => next.push(name),
                _ => {}
            }
        }
        if let Some(at) = chain.iter().position(|seen| *seen == next) {
            let mut cycle = chain.split_off(at);
            let smallest = (0..cycle.len()).min_by(|&a, &b| cycle[a].cmp(&cycle[b]))?;
            cycle.rotate_left(smallest);
            return Some(cycle);
        }
        chain.push(next);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ignored() -> IgnoredSubdirectories {
        IgnoredSubdirectories::default()
    }

    #[test]
    fn plain_directories_dangling_links_and_files_pass() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pkg/dir");
        fs::create_dir_all(dir.join("sub/empty")).unwrap();
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::write(dir.join("sub/b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("missing", dir.join("dangling")).unwrap();
        std::os::unix::fs::symlink("sub", dir.join("link")).unwrap();
        let ignored = ignored();
        let boundaries = Boundaries { ignored: &ignored };
        assert_eq!(
            traversal_error(&dir, "//pkg:dir", "pkg/dir", Some(&boundaries)),
            None
        );
    }

    #[test]
    fn a_subpackage_crosses_the_boundary_in_the_main_repository_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pkg/dir");
        fs::create_dir_all(dir.join("a/sub")).unwrap();
        fs::write(dir.join("a/sub/BUILD"), "").unwrap();
        let ignored = ignored();
        let boundaries = Boundaries { ignored: &ignored };
        let error = traversal_error(&dir, "//pkg:dir", "pkg/dir", Some(&boundaries)).unwrap();
        assert_eq!(
            error.message,
            "error traversing directory '//pkg:dir': Directory artifact pkg/dir crosses package boundary into package rooted at pkg/dir/a/sub"
        );
        assert!(error.events.is_empty());
        assert_eq!(
            traversal_error(&dir, "@@r+//:dir", "dir", None),
            None,
            "external repositories are not checked by default"
        );
        // A link to the subpackage crosses at the link's (logical) path.
        let other = temp.path().join("pkg/other");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(dir.join("a"), other.join("l")).unwrap();
        let error = traversal_error(&other, "//pkg:other", "pkg/other", Some(&boundaries)).unwrap();
        assert!(
            error
                .message
                .ends_with("crosses package boundary into package rooted at pkg/other/l/sub"),
            "{error:?}"
        );
    }

    #[test]
    fn a_link_into_an_enclosing_directory_is_an_infinite_expansion() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pkg/loop");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("l.txt"), "l").unwrap();
        std::os::unix::fs::symlink("..", dir.join("up")).unwrap();
        let error = traversal_error(&dir, "//pkg:loop", "pkg/loop", None).unwrap();
        assert_eq!(
            error.message,
            "error traversing directory '//pkg:loop': Error while traversing directory pkg/loop/up: Infinite symlink expansion"
        );
        let pkg = fs::canonicalize(temp.path().join("pkg")).unwrap();
        assert_eq!(
            error.events,
            vec![format!(
                "infinite symlink expansion detected\n[start of symlink chain]\n{}\n{}\n[end of symlink chain]",
                pkg.display(),
                dir.join("up").display()
            )]
        );
    }

    #[test]
    fn a_symlink_cycle_fails() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cycle");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("b", dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", dir.join("b")).unwrap();
        let error = traversal_error(&dir, "//:cycle", "cycle", None).unwrap();
        assert_eq!(
            error.message,
            "error traversing directory '//:cycle': Error while traversing directory cycle/a: Symlink cycle"
        );
        assert_eq!(
            error.events,
            vec![format!(
                "circular symlinks detected\n[start of symlink cycle]\n{}\n{}\n[end of symlink cycle]",
                dir.join("a").display(),
                dir.join("b").display()
            )]
        );
    }
}
