//! Filesystem discovery for a single main repository.
//!
//! Package paths use Bazel's package-name grammar (`label`). A directory whose
//! path is not a valid package name is no package (PackageLookupFunction
//! .java:91-96 `invalidPackageName`), silently, and neither is anything below
//! it. Symlink package directories are not supported; discovery never follows
//! directory symlinks.
//! MODULE.bazel, REPO.bazel and WORKSPACE markers only locate the workspace
//! root: Bazel 9.3 has no nested repositories inside a repository
//! (`skyframe/LocalRepositoryLookupFunction.java:27-35` always answers the
//! main repository), so they are neither package nor glob boundaries.

use crate::ignored::IgnoredSubdirectories;
use crate::label::{is_valid_package_name, validate_package_path};
use crate::observed_fs;
use std::io;
use std::path::{Path, PathBuf};

const MARKERS: [&str; 4] = ["MODULE.bazel", "REPO.bazel", "WORKSPACE.bazel", "WORKSPACE"];

pub fn workspace_root(start: &Path) -> Result<PathBuf, String> {
    let start = observed_fs::canonicalize(start).map_err(|e| {
        format!(
            "cannot resolve workspace starting path {}: {e}",
            start.display()
        )
    })?;
    let mut current = if observed_fs::is_dir(&start) {
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

/// Packages at or below `package` (`RecursivePkgFunction` /
/// `ProcessPackageDirectory`): subdirectories whose repository-relative path
/// has a `matching_entry` in `ignored` are not recursed into. The caller
/// checks `package` itself (`TargetPattern.TargetsBelowDirectory`).
pub fn discover_packages(
    root: &Path,
    package: &str,
    ignored: &IgnoredSubdirectories,
) -> Result<Vec<String>, String> {
    discover_packages_skipping(root, package, ignored, &[])
}

/// [`discover_packages`] that does not descend into `skip` (canonical native
/// store / `--cache-dir` prefixes that do not contain `root`; Bazel's output
/// base is never below the workspace).
pub fn discover_packages_skipping(
    root: &Path,
    package: &str,
    ignored: &IgnoredSubdirectories,
    skip: &[PathBuf],
) -> Result<Vec<String>, String> {
    validate_package_path(package)?;
    let root = canonical_root(root)?;
    let skip: Vec<&PathBuf> = skip
        .iter()
        .filter(|prefix| !root.starts_with(prefix))
        .collect();
    let directory = package_directory(&root, package)?;
    let mut pending = vec![(directory, package.to_owned())];
    let mut packages = Vec::new();
    while let Some((directory, package)) = pending.pop() {
        // A --deleted_packages package is none, but its subdirectories are
        // still traversed (PackageLookupValue.DELETED_PACKAGE_VALUE).
        if find_build_file(&root, &directory)?.is_some()
            && !crate::ignored::is_deleted_directory(&directory)
        {
            packages.push(package.clone());
        }
        // excludedPaths.filterForDirectory: only entries that can match below.
        let ignored = ignored.filter_for_directory(&package);
        // Only subdirectories matter here: the snapshot records this
        // listing projected to them (adding a source file to a package does
        // not invalidate it).
        let entries =
            observed_fs::read_dir_projected(&directory, &observed_fs::Projection::Directories)
                .map_err(|e| {
                    format!("cannot read package directory {}: {e}", directory.display())
                })?;
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
            // A non-UTF-8 name has a byte >= 0x80, which no package name has
            // (LabelValidator.java:50-61): neither it nor anything below it
            // is a package.
            let Some(name) = name.to_str() else {
                continue;
            };
            if matches!(name, ".git" | ".direnv" | "bazel-rs-out") {
                continue;
            }
            let child = if package.is_empty() {
                name.to_owned()
            } else {
                format!("{package}/{name}")
            };
            // ProcessPackageDirectory: "If this subdirectory is one of the
            // excluded paths, don't recurse into it." This precedes the
            // nested-repository and package-name checks below.
            if ignored.matching_entry(&child).is_some() {
                continue;
            }
            let path = entry.path();
            if skip.iter().any(|prefix| path.starts_with(prefix)) {
                continue;
            }
            // ProcessPackageDirectory looks the directory up as a package
            // (ProcessPackageDirectory.java:131-176): an invalid package name
            // is `invalidPackageName`, not an error, and its subdirectories'
            // paths are invalid package names too. Skip the subtree.
            if !is_valid_package_name(&child) {
                continue;
            }
            pending.push((path, child));
        }
    }
    packages.sort();
    Ok(packages)
}

fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    let resolved = observed_fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve workspace root {}: {e}", root.display()))?;
    if !observed_fs::is_dir(&resolved) {
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
            let metadata = observed_fs::symlink_metadata(&directory).map_err(|e| {
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
        }
    }
    Ok(directory)
}

fn is_regular_file(path: &Path) -> Result<bool, String> {
    match observed_fs::metadata(path) {
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

/// The repository's REPO.bazel with the same stays-inside-the-workspace
/// check as BUILD files (`find_build_file`). Like `RepoFileFunction`, any
/// existing path counts (`FileValue.exists()`, following symlinks; a
/// dangling symlink does not exist): a directory or other non-regular file
/// is an error reading it rather than an absent REPO.bazel.
pub fn repo_file(root: &Path) -> Result<Option<PathBuf>, String> {
    let root = canonical_root(root)?;
    let path = root.join("REPO.bazel");
    match observed_fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        // RepoFileFunction.java:110-116: reading fails with exactly this
        // IOException message (the cause is not part of it).
        Ok(_) => {
            return Err(format!(
                "error reading REPO.bazel file at {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "error reading REPO.bazel file at {}",
                path.display()
            ));
        }
    }
    let resolved = observed_fs::canonicalize(&path)
        .map_err(|e| format!("cannot resolve REPO.bazel file {}: {e}", path.display()))?;
    if !resolved.starts_with(&root) {
        return Err(format!(
            "REPO.bazel file {} resolves outside workspace {}",
            path.display(),
            root.display()
        ));
    }
    Ok(Some(path))
}

/// Whether `package` has a regular BUILD file (PackageLookupFunction's
/// `findPackageByBuildFile`); a missing directory is no package. Once a BUILD
/// file is found, the same policy checks as `package_build_file` apply.
pub(crate) fn is_package(root: &Path, package: &str) -> Result<bool, String> {
    if !is_valid_package_name(package) {
        return Ok(false);
    }
    let canonical = canonical_root(root)?;
    let mut directory = canonical.clone();
    directory.extend(package.split('/').filter(|part| !part.is_empty()));
    match observed_fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", directory.display())),
    }
    if find_build_file(&canonical, &directory)?.is_none()
        || crate::ignored::is_deleted_directory(&directory)
    {
        return Ok(false);
    }
    package_build_file(root, package).map(|_| true)
}

fn find_build_file(root: &Path, directory: &Path) -> Result<Option<PathBuf>, String> {
    for name in ["BUILD.bazel", "BUILD"] {
        let path = directory.join(name);
        if !is_regular_file(&path)? {
            continue;
        }
        let resolved = observed_fs::canonicalize(&path)
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn none() -> IgnoredSubdirectories {
        IgnoredSubdirectories::default()
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            loop {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = crate::test_temp::root().join(format!(
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
        assert!(discover_packages(&temp.0, "missing", &none()).is_err());
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
            assert!(
                discover_packages(&temp.0, package, &none()).is_err(),
                "{package:?}"
            );
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
            discover_packages(&temp.0, "", &none()).unwrap(),
            ["", "a", "a/b", "unpackaged/c", "z"]
        );
        assert_eq!(
            discover_packages(&temp.0, "a", &none()).unwrap(),
            ["a", "a/b"]
        );
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
        assert_eq!(
            discover_packages(&temp.0, "", &none()).unwrap(),
            [".other", "a"]
        );
    }

    #[test]
    fn real_target_and_bazel_named_packages_are_not_silently_omitted() {
        let temp = Temp::new();
        for name in ["target", "bazel-out", "bazel-project"] {
            temp.file(&format!("{name}/BUILD"));
        }
        assert_eq!(
            discover_packages(&temp.0, "", &none()).unwrap(),
            ["bazel-out", "bazel-project", "target"]
        );
    }

    #[test]
    fn recursive_discovery_skips_ignored_directories() {
        let temp = Temp::new();
        for file in [
            "BUILD",
            "pkg/BUILD",
            "pkg/sub/BUILD",
            "src/ts/BUILD",
            "src/ts/node_modules/dep/BUILD",
            "src/ts/app/node_modules/BUILD",
            "src/ts/app/BUILD",
            "src/python/.venv/lib/BUILD",
            "src/python/.venv/bin/BUILD",
            "src/python/BUILD",
            "node_modules/BUILD",
            "pkgs/BUILD",
            // Invalid package names are no packages, ignored or not.
            "src/ts/node_modules/café/BUILD",
            // Nested markers below an ignored directory are never inspected.
            "pkg/nested/MODULE.bazel",
        ] {
            temp.file(file);
        }
        assert!(
            !discover_packages(&temp.0, "", &IgnoredSubdirectories::default())
                .unwrap()
                .iter()
                .any(|package| package.contains("café"))
        );
        let ignored = IgnoredSubdirectories::new(
            vec!["pkg".into(), "src/python/.venv/lib".into()],
            vec!["**/node_modules".into()],
        );
        assert_eq!(
            discover_packages(&temp.0, "", &ignored).unwrap(),
            [
                "",
                "pkgs",
                "src/python",
                "src/python/.venv/bin",
                "src/ts",
                "src/ts/app"
            ]
        );
        assert_eq!(
            discover_packages(&temp.0, "src/ts", &ignored).unwrap(),
            ["src/ts", "src/ts/app"]
        );
        // The empty prefix (".bazelignore" line ".") skips every child.
        let everything = IgnoredSubdirectories::new(vec![String::new()], Vec::new());
        assert_eq!(discover_packages(&temp.0, "", &everything).unwrap(), [""]);
        // REPO.bazel content is evaluated by package loading, not here.
        fs::write(temp.0.join("REPO.bazel"), "repo(default_testonly = True)").unwrap();
        fs::write(temp.0.join(".bazelignore"), "pkg\n").unwrap();
        assert_eq!(
            discover_packages(&temp.0, "src/ts", &ignored).unwrap(),
            ["src/ts", "src/ts/app"]
        );
    }

    #[test]
    fn repo_file_must_be_a_regular_file_inside_the_workspace() {
        let temp = Temp::new();
        assert_eq!(repo_file(&temp.0).unwrap(), None);
        // FileValue.exists() is true for directories: reading then fails.
        fs::create_dir(temp.0.join("REPO.bazel")).unwrap();
        assert_eq!(
            repo_file(&temp.0).unwrap_err(),
            format!(
                "error reading REPO.bazel file at {}",
                temp.0.join("REPO.bazel").display()
            )
        );
        fs::remove_dir(temp.0.join("REPO.bazel")).unwrap();
        // A dangling symlink does not exist.
        std::os::unix::fs::symlink("missing", temp.0.join("REPO.bazel")).unwrap();
        assert_eq!(repo_file(&temp.0).unwrap(), None);
        fs::remove_file(temp.0.join("REPO.bazel")).unwrap();
        let outside = Temp::new();
        outside.file("REPO.bazel");
        std::os::unix::fs::symlink(outside.0.join("REPO.bazel"), temp.0.join("REPO.bazel"))
            .unwrap();
        assert!(
            repo_file(&temp.0)
                .unwrap_err()
                .contains("resolves outside workspace")
        );
        fs::remove_file(temp.0.join("REPO.bazel")).unwrap();
        temp.file("REPO.bazel");
        assert!(repo_file(&temp.0).unwrap().is_some());
    }

    #[test]
    fn nested_markers_are_ordinary_packages_like_bazel() {
        // LocalRepositoryLookupFunction.java:27-35: no nested repositories.
        let temp = Temp::new();
        temp.file("MODULE.bazel");
        temp.file("BUILD");
        let mut expected = vec![String::new()];
        for (index, marker) in MARKERS.iter().enumerate() {
            let nested = format!("nested{index}");
            temp.file(&format!("{nested}/{marker}"));
            temp.file(&format!("{nested}/BUILD"));
            temp.file(&format!("{nested}/child/BUILD"));
            assert_eq!(
                discover_packages(&temp.0, &nested, &none()).unwrap(),
                [nested.clone(), format!("{nested}/child")]
            );
            assert!(package_build_file(&temp.0, &format!("{nested}/child")).is_ok());
            expected.push(nested.clone());
            expected.push(format!("{nested}/child"));
        }
        assert_eq!(discover_packages(&temp.0, "", &none()).unwrap(), expected);
        // The native store below the workspace is skipped.
        temp.file("cache/repositories/trees/x/BUILD");
        let canonical = fs::canonicalize(&temp.0).unwrap();
        assert_eq!(
            discover_packages_skipping(&temp.0, "", &none(), &[canonical.join("cache")]).unwrap(),
            expected
        );
    }

    #[test]
    fn empty_tree_has_no_packages() {
        let temp = Temp::new();
        assert!(discover_packages(&temp.0, "", &none()).unwrap().is_empty());
    }

    #[test]
    fn package_names_follow_bazel_and_invalid_directories_are_no_packages() {
        let temp = Temp::new();
        for valid in [
            "space name",
            "app/(beyond-work)",
            "locales/tt@iqtelif",
            "a+b,c;d=e[f]{g}~h!",
            ".x/..y",
        ] {
            temp.file(&format!("{valid}/BUILD"));
        }
        // Invalid package names (non-ASCII, ':', '\\', a dot-only segment):
        // silently no package, nor anything below.
        for invalid in ["café", "a:b", "a\\b", "..."] {
            temp.file(&format!("{invalid}/BUILD"));
            temp.file(&format!("{invalid}/below/BUILD"));
        }
        assert_eq!(
            discover_packages(&temp.0, "", &none()).unwrap(),
            [
                ".x/..y",
                "a+b,c;d=e[f]{g}~h!",
                "app/(beyond-work)",
                "locales/tt@iqtelif",
                "space name"
            ]
        );
        assert!(is_package(&temp.0, "space name").unwrap());
        assert!(!is_package(&temp.0, "café").unwrap());
        assert_eq!(
            discover_packages(&temp.0, "café", &none()).unwrap_err(),
            "invalid package name 'café': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directories_are_no_packages() {
        use std::os::unix::ffi::OsStrExt;
        let temp = Temp::new();
        let name = std::ffi::OsStr::from_bytes(b"latin\xe9");
        fs::create_dir_all(temp.0.join(name).join("below")).unwrap();
        fs::write(temp.0.join(name).join("BUILD"), "").unwrap();
        temp.file("ok/BUILD");
        assert_eq!(discover_packages(&temp.0, "", &none()).unwrap(), ["ok"]);
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
        assert_eq!(discover_packages(&temp.0, "", &none()).unwrap(), ["real"]);
        for package in ["escape", "alias", "loop"] {
            assert!(discover_packages(&temp.0, package, &none()).is_err());
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
        assert!(discover_packages(&temp.0, "", &none()).is_err());
        fs::remove_file(temp.0.join("BUILD.bazel")).unwrap();
        symlink(temp.0.join("BUILD"), temp.0.join("BUILD.bazel")).unwrap();
        assert_eq!(
            package_build_file(&temp.0, "").unwrap(),
            temp.0.join("BUILD.bazel")
        );
    }
}
