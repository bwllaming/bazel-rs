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

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

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
    let shown = relative.display();
    let root = fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve repository root {}: {e}", root.display()))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe source path {shown}"));
    }
    let mut pending = VecDeque::new();
    push_front(&mut pending, relative);
    let mut resolved: Vec<OsString> = Vec::new();
    let mut hops = 0;
    // Each followed link with the components still pending after it: seeing
    // the same pair again means resolution would repeat forever.
    let mut followed: Vec<(PathBuf, VecDeque<OsString>)> = Vec::new();
    let current = |resolved: &[OsString]| {
        let mut path = root.clone();
        path.extend(resolved);
        path
    };
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            if resolved.pop().is_none() {
                return Err(format!(
                    "source symlink path {shown} resolves outside its repository root {} (native source resolution never follows links outside the repository)",
                    root.display()
                ));
            }
            continue;
        }
        resolved.push(part);
        let path = current(&resolved);
        let metadata = match fs::symlink_metadata(&path) {
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
            let target = fs::read_link(&path)
                .map_err(|e| format!("cannot read source symlink {}: {e}", path.display()))?;
            resolved.pop();
            if target.is_absolute() {
                let inside = target.strip_prefix(&root).map_err(|_| {
                    format!(
                        "source symlink {} points outside its repository root to {} (native source resolution never follows links outside the repository)",
                        path.display(),
                        target.display()
                    )
                })?;
                resolved.clear();
                push_front(&mut pending, inside);
            } else {
                push_front(&mut pending, &target);
            }
        } else if !pending.is_empty() && !metadata.is_dir() {
            let mut missing = path;
            missing.push(pending.front().expect("nonempty"));
            return Ok(Resolution::Missing(missing));
        }
    }
    Ok(Resolution::Resolved(current(&resolved)))
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
    }
}
