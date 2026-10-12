//! Filesystem discovery for a single main repository.
//!
//! Package paths use the bounded label grammar from `label`. Symlink package
//! directories are not supported; discovery never follows directory symlinks.
//! Nested repositories are boundaries, not packages in the outer repository.

use crate::label::validate_package_path;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MARKERS: [&str; 4] = ["MODULE.bazel", "REPO.bazel", "WORKSPACE.bazel", "WORKSPACE"];

pub fn workspace_root(start: &Path) -> Result<PathBuf, String> {
    let start = fs::canonicalize(start).map_err(|e| {
        format!(
            "cannot resolve workspace starting path {}: {e}",
            start.display()
        )
    })?;
    let mut current = if start.is_dir() {
        start.as_path()
    } else {
        start
            .parent()
            .ok_or_else(|| format!("no parent directory for {}", start.display()))?
    };
    loop {
        if has_marker(current)? {
            return Ok(current.to_path_buf());
        }
        current = current.parent().ok_or_else(|| {
            format!(
                "no workspace marker ({}) found above {}",
                MARKERS.join(", "),
                start.display()
            )
        })?;
    }
}

pub fn package_build_file(root: &Path, package: &str) -> Result<PathBuf, String> {
    validate_package_path(package)?;
    let root = canonical_root(root)?;
    let directory = package_directory(&root, package)?;
    find_build_file(&root, &directory)?.ok_or_else(|| {
        format!(
            "package //{package} has no regular BUILD.bazel or BUILD file in {}",
            directory.display()
        )
    })
}

pub fn discover_packages(root: &Path, package: &str) -> Result<Vec<String>, String> {
    validate_package_path(package)?;
    let root = canonical_root(root)?;
    for name in [".bazelignore", "REPO.bazel"] {
        let path = root.join(name);
        match fs::read_to_string(&path) {
            Ok(text)
                if text
                    .lines()
                    .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#')) =>
            {
                return Err(format!(
                    "recursive discovery with {name} directives is not implemented; use explicit package labels"
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        }
    }
    let directory = package_directory(&root, package)?;
    let mut pending = vec![(directory, package.to_owned())];
    let mut packages = Vec::new();
    while let Some((directory, package)) = pending.pop() {
        if find_build_file(&root, &directory)?.is_some() {
            packages.push(package.clone());
        }
        let entries = fs::read_dir(&directory)
            .map_err(|e| format!("cannot read package directory {}: {e}", directory.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("cannot read entry in {}: {e}", directory.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("cannot inspect {}: {e}", entry.path().display()))?;
            // DirEntry::file_type does not follow symlinks, including loops.
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                format!(
                    "unsupported non-UTF-8 package directory in {}",
                    directory.display()
                )
            })?;
            if matches!(name, ".git" | ".direnv" | "bazel-rs-out") {
                continue;
            }
            let path = entry.path();
            if has_marker(&path)? {
                continue;
            }
            let child = if package.is_empty() {
                name.to_owned()
            } else {
                format!("{package}/{name}")
            };
            // Do not silently omit directories that could contain unsupported
            // packages: report the bounded grammar instead of false success.
            validate_package_path(&child)?;
            pending.push((path, child));
        }
    }
    packages.sort();
    Ok(packages)
}

fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve workspace root {}: {e}", root.display()))?;
    if !resolved.is_dir() {
        return Err(format!(
            "workspace root is not a directory: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn package_directory(root: &Path, package: &str) -> Result<PathBuf, String> {
    let mut directory = root.to_path_buf();
    if !package.is_empty() {
        for component in package.split('/') {
            if matches!(component, ".git" | ".direnv" | "bazel-rs-out") {
                return Err(format!(
                    "reserved implementation directory in package //{package}"
                ));
            }
            directory.push(component);
            let metadata = fs::symlink_metadata(&directory).map_err(|e| {
                format!(
                    "cannot inspect package directory {}: {e}",
                    directory.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "symlink package directories are unsupported: {}",
                    directory.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!(
                    "package path is not a directory: {}",
                    directory.display()
                ));
            }
            if has_marker(&directory)? {
                return Err(format!(
                    "package //{package} crosses a nested workspace boundary at {}",
                    directory.display()
                ));
            }
        }
    }
    Ok(directory)
}

fn is_regular_file(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

pub(crate) fn has_marker(directory: &Path) -> Result<bool, String> {
    for marker in MARKERS {
        if is_regular_file(&directory.join(marker))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_build_file(root: &Path, directory: &Path) -> Result<Option<PathBuf>, String> {
    for name in ["BUILD.bazel", "BUILD"] {
        let path = directory.join(name);
        if !is_regular_file(&path)? {
            continue;
        }
        let resolved = fs::canonicalize(&path)
            .map_err(|e| format!("cannot resolve build file {}: {e}", path.display()))?;
        if !resolved.starts_with(root) {
            return Err(format!(
                "build file {} resolves outside workspace {}",
                path.display(),
                root.display()
            ));
        }
        return Ok(Some(path));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            loop {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "bazel-rs-workspace-{}-{nonce}-{}",
                    std::process::id(),
                    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("cannot create test directory: {e}"),
                }
            }
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "").unwrap();
            path
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn every_marker_identifies_workspace_from_directory_or_file() {
        for marker in MARKERS {
            let temp = Temp::new();
            temp.file(marker);
            let source = temp.file("a/b/source.rs");
            assert_eq!(workspace_root(source.parent().unwrap()).unwrap(), temp.0);
            assert_eq!(workspace_root(&source).unwrap(), temp.0);
            assert_eq!(workspace_root(&temp.0).unwrap(), temp.0);
        }
    }

    #[test]
    fn nearest_workspace_wins() {
        let temp = Temp::new();
        temp.file("MODULE.bazel");
        temp.file("nested/WORKSPACE");
        let source = temp.file("nested/a/source.rs");
        assert_eq!(workspace_root(&source).unwrap(), temp.0.join("nested"));
    }

    #[test]
    fn marker_directories_are_not_markers() {
        let temp = Temp::new();
        temp.file("MODULE.bazel");
        fs::create_dir_all(temp.0.join("nested/WORKSPACE")).unwrap();
        assert_eq!(workspace_root(&temp.0.join("nested")).unwrap(), temp.0);
        assert!(workspace_root(&temp.0.join("missing")).is_err());
    }

    #[test]
    fn build_bazel_has_priority_with_legacy_fallback() {
        let temp = Temp::new();
        let legacy = temp.file("a/BUILD");
        assert_eq!(package_build_file(&temp.0, "a").unwrap(), legacy);
        let preferred = temp.file("a/BUILD.bazel");
        assert_eq!(package_build_file(&temp.0, "a").unwrap(), preferred);
        let root_build = temp.file("BUILD");
        assert_eq!(package_build_file(&temp.0, "").unwrap(), root_build);
    }

    #[test]
    fn only_regular_build_files_count() {
        let temp = Temp::new();
        fs::create_dir_all(temp.0.join("a/BUILD.bazel")).unwrap();
        assert!(package_build_file(&temp.0, "a").is_err());
        let legacy = temp.file("a/BUILD");
        assert_eq!(package_build_file(&temp.0, "a").unwrap(), legacy);
        temp.file("not-directory");
        assert!(package_build_file(&temp.0, "not-directory").is_err());
        assert!(package_build_file(&temp.0.join("not-directory"), "").is_err());
        assert!(discover_packages(&temp.0, "missing").is_err());
    }

    #[test]
    fn rejects_path_traversal_before_touching_filesystem() {
        let temp = Temp::new();
        for package in [
            "..",
            "../outside",
            "/absolute",
            "a/../b",
            "a//b",
            "a/",
            ".",
            "a\\b",
            "a\0b",
        ] {
            assert!(package_build_file(&temp.0, package).is_err(), "{package:?}");
            assert!(discover_packages(&temp.0, package).is_err(), "{package:?}");
        }
    }

    #[test]
    fn discovery_is_sorted_recursive_and_includes_start_package() {
        let temp = Temp::new();
        for file in [
            "BUILD",
            "z/BUILD",
            "a/BUILD.bazel",
            "a/BUILD",
            "a/b/BUILD",
            "unpackaged/c/BUILD",
        ] {
            temp.file(file);
        }
        assert_eq!(
            discover_packages(&temp.0, "").unwrap(),
            ["", "a", "a/b", "unpackaged/c", "z"]
        );
        assert_eq!(discover_packages(&temp.0, "a").unwrap(), ["a", "a/b"]);
    }

    #[test]
    fn discovery_ignores_output_and_metadata_directories() {
        let temp = Temp::new();
        for name in [".git", ".direnv", "bazel-rs-out"] {
            temp.file(&format!("{name}/BUILD"));
            temp.file(&format!("a/{name}/BUILD"));
        }
        temp.file("a/BUILD");
        temp.file(".other/BUILD");
        assert_eq!(discover_packages(&temp.0, "").unwrap(), [".other", "a"]);
    }

    #[test]
    fn real_target_and_bazel_named_packages_are_not_silently_omitted() {
        let temp = Temp::new();
        for name in ["target", "bazel-out", "bazel-project"] {
            temp.file(&format!("{name}/BUILD"));
        }
        assert_eq!(
            discover_packages(&temp.0, "").unwrap(),
            ["bazel-out", "bazel-project", "target"]
        );
    }

    #[test]
    fn recursive_discovery_rejects_unimplemented_ignore_directives() {
        let temp = Temp::new();
        temp.file("BUILD");
        for name in [".bazelignore", "REPO.bazel"] {
            fs::write(temp.0.join(name), "ignored_directories(['node_modules'])").unwrap();
            assert!(
                discover_packages(&temp.0, "")
                    .unwrap_err()
                    .contains("not implemented")
            );
            fs::remove_file(temp.0.join(name)).unwrap();
        }
    }

    #[test]
    fn nested_workspaces_are_never_discovered_or_explicitly_crossed() {
        let temp = Temp::new();
        temp.file("MODULE.bazel");
        temp.file("BUILD");
        for (index, marker) in MARKERS.iter().enumerate() {
            let nested = format!("nested{index}");
            temp.file(&format!("{nested}/{marker}"));
            temp.file(&format!("{nested}/BUILD"));
            temp.file(&format!("{nested}/child/BUILD"));
            assert!(
                discover_packages(&temp.0, &nested)
                    .unwrap_err()
                    .contains("nested workspace")
            );
            assert!(package_build_file(&temp.0, &format!("{nested}/child")).is_err());
        }
        assert_eq!(discover_packages(&temp.0, "").unwrap(), [""]);
    }

    #[test]
    fn empty_tree_has_no_packages() {
        let temp = Temp::new();
        assert!(discover_packages(&temp.0, "").unwrap().is_empty());
    }

    #[test]
    fn unsupported_directory_names_are_reported() {
        let temp = Temp::new();
        temp.file("space name/BUILD");
        assert!(
            discover_packages(&temp.0, "")
                .unwrap_err()
                .contains("stricter subset")
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_and_loops_are_not_followed() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let outside = Temp::new();
        outside.file("BUILD");
        temp.file("real/BUILD");
        symlink(&outside.0, temp.0.join("escape")).unwrap();
        symlink(temp.0.join("real"), temp.0.join("alias")).unwrap();
        symlink(&temp.0, temp.0.join("loop")).unwrap();
        assert_eq!(discover_packages(&temp.0, "").unwrap(), ["real"]);
        for package in ["escape", "alias", "loop"] {
            assert!(discover_packages(&temp.0, package).is_err());
            assert!(package_build_file(&temp.0, package).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn build_symlinks_cannot_escape_workspace() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let outside = Temp::new();
        let external = outside.file("BUILD");
        symlink(external, temp.0.join("BUILD.bazel")).unwrap();
        temp.file("BUILD");
        assert!(
            package_build_file(&temp.0, "")
                .unwrap_err()
                .contains("outside workspace")
        );
        assert!(discover_packages(&temp.0, "").is_err());
        fs::remove_file(temp.0.join("BUILD.bazel")).unwrap();
        symlink(temp.0.join("BUILD"), temp.0.join("BUILD.bazel")).unwrap();
        assert_eq!(
            package_build_file(&temp.0, "").unwrap(),
            temp.0.join("BUILD.bazel")
        );
    }
}
