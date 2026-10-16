//! Deterministic, source-only Bazel glob subset (no external dependencies).
//!
//! Bare `*` matches even leading dots; compound patterns must start with `.`
//! to include hidden names (Bazel's documented hidden-file rule). Exclusions
//! filter already discovered paths and do not apply that hidden-name rule.
//! `**` is permitted
//! only as a whole component and matches zero or more components. Other glob
//! dialects (`?`, brackets, braces, backslash escapes) are explicitly rejected.
//! Paths must be relative, UTF-8, and contain no empty, dot, or parent components.
//! Subpackages and nested workspaces are excluded, including their directory
//! names. The implementation reserves `.git`, `.direnv`, and `bazel-rs-out`
//! directories at every depth; generated outputs live in the latter, not in
//! the source tree. Ordinary source directories named `target` or `bazel-out`
//! are not assumed to be generated outputs.
//! Include traversal is filesystem-type-aware: `plain/**` cannot match a
//! regular file named `plain`, although it can match a directory named `plain`.
//! Exclusions are path filters, so their zero-component `**` match does not
//! require a directory.
//!
//! Symlinks are followed as in Bazel (see [`glob`]): links to files and
//! directories count as such under the link's own path, dangling links are
//! skipped, and symlink cycles and infinite expansions are Bazel's errors even
//! for entries an exclusion removes (Bazel's Skyframe glob evaluates only the
//! include patterns). Where links may point is the shared
//! `source_links::LinkPolicy` (docs/ARCHIVE_SYMLINKS.md). Reserved trees are
//! ignored. Special filesystem objects are unsupported.
//!
//! Directories matched by the repository's ignored subdirectories
//! (`.bazelignore` prefixes and REPO.bazel `ignore_directories()` patterns,
//! `crate::ignored`) are neither results nor traversed, exactly like Bazel's
//! `DirectoryDirentProducer`/`GlobFunction` `matchingEntry` checks on the
//! repository-relative directory path. Regular files are never checked.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Pattern<'a> {
    text: &'a str,
    components: Vec<&'a str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    IncludeDirectory,
    IncludeFile,
    Exclude,
}

impl<'a> Pattern<'a> {
    fn parse(text: &'a str) -> Result<Self, String> {
        let components: Vec<_> = text.split('/').collect();
        for component in &components {
            if component.is_empty() || matches!(*component, "." | "..") {
                return Err(format!(
                    "invalid glob pattern {text:?}: paths must be relative with no empty, '.' or '..' components"
                ));
            }
            if component.contains("**") && *component != "**" {
                return Err(format!(
                    "invalid glob pattern {text:?}: '**' must be an entire path component"
                ));
            }
            if component
                .chars()
                .any(|c| c.is_control() || matches!(c, '?' | '[' | ']' | '{' | '}' | '\\' | ':'))
            {
                return Err(format!(
                    "unsupported glob pattern {text:?}: only '*' and whole-component '**' wildcards are supported; escapes, control characters and ':' are not supported"
                ));
            }
        }
        Ok(Self { text, components })
    }

    // NFA states are positions between pattern components. Epsilon closure
    // lets ** consume zero components; its self-loop consumes one component.
    // This is iterative even for adversarially deep paths and patterns.
    fn states(&self, path: &str) -> Vec<bool> {
        self.match_states(path, MatchMode::IncludeDirectory)
    }

    fn match_states(&self, path: &str, mode: MatchMode) -> Vec<bool> {
        let mut states = vec![false; self.components.len() + 1];
        states[0] = true;
        self.close(&mut states);
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            let mut next = vec![false; states.len()];
            for (index, pattern) in self.components.iter().enumerate() {
                if states[index] {
                    if *pattern == "**" {
                        next[index] = true;
                        // Consuming this component also finishes **. This is
                        // different from stepping into an unconsumed trailing
                        // ** after matching a regular file with a prior segment.
                        next[index + 1] = true;
                    } else if match mode {
                        MatchMode::Exclude => wildcard_matches(pattern, component),
                        _ => component_matches(pattern, component),
                    } {
                        next[index + 1] = true;
                    }
                }
            }
            // Include discovery cannot continue a pattern after reaching a
            // regular file, even if the remaining ** could match zero segments.
            // Exclusions instead match complete path strings, without statting.
            if mode != MatchMode::IncludeFile || components.peek().is_some() {
                self.close(&mut next);
            }
            states = next;
        }
        states
    }

    fn close(&self, states: &mut [bool]) {
        for (index, component) in self.components.iter().enumerate() {
            if states[index] && *component == "**" {
                states[index + 1] = true;
            }
        }
    }

    /// Whether some include traversal consumed the last component of `path`
    /// with a wildcard component (Bazel's PatternWithWildcardProducer, which
    /// alone checks directory links for unbounded ancestor expansion; a
    /// literal component is a plain FileValue lookup,
    /// PatternWithoutWildcardProducer.java:70-97).
    fn reached_by_wildcard(&self, path: &str) -> bool {
        let (parent, last) = match path.rsplit_once('/') {
            Some((parent, last)) => (Some(parent), last),
            None => (None, path),
        };
        let states = match parent {
            Some(parent) => self.states(parent),
            None => {
                let mut states = vec![false; self.components.len() + 1];
                states[0] = true;
                self.close(&mut states);
                states
            }
        };
        self.components.iter().enumerate().any(|(index, pattern)| {
            states[index]
                && pattern.contains('*')
                && (*pattern == "**" || component_matches(pattern, last))
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.match_states(path, MatchMode::Exclude)[self.components.len()]
    }

    fn matches_file(&self, path: &str) -> bool {
        self.match_states(path, MatchMode::IncludeFile)[self.components.len()]
    }
}

fn component_matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && pattern != "*" && !pattern.starts_with('.') {
        return false;
    }
    wildcard_matches(pattern, name)
}

fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let (mut p, mut n) = (0, 0);
    let mut star = None;
    let mut retry = 0;
    while n < name.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = n;
        } else if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if let Some(position) = star {
            retry += 1;
            n = retry;
            p = position + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// What a `glob()` call produced: its results, or the package-level errors
/// Bazel reports for it after the BUILD file was evaluated.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GlobOutcome {
    /// Sorted, unique package-relative paths (empty after an I/O error).
    pub paths: Vec<String>,
    /// The legacy globber's IOException, already in StarlarkNativeModule's
    /// `error globbing [INCLUDES] - [EXCLUDES] op=OP: MESSAGE` form
    /// (packages/StarlarkNativeModule.java:851-876).
    pub io_error: Option<String>,
    /// The Skyframe glob's FileSymlinkInfiniteExpansionException
    /// (PatternWithWildcardProducer.java:181-197).
    pub symlink_issue: Option<crate::source_links::SymlinkIssue>,
}

/// Bazel's report of a Skyframe glob symlink issue: the package error
/// `Symlink issue while evaluating globs: …` (PackageFunction.java:576-584)
/// for the package display `package`. Genuine Bazel 9.3 prints no
/// `infinite symlink expansion detected` uniqueness event for it (paired
/// capture, tests/fixtures/glob_symlinks/reference_receipt.json A2).
pub fn symlink_issue_error(issue: &crate::source_links::SymlinkIssue, package: &str) -> String {
    format!(
        "no such package '{package}': Symlink issue while evaluating globs: {}",
        issue.user_friendly()
    )
}

/// Native safety cap on directory entries visited below symlinked host
/// directories in one glob pattern (Bazel has none).
const MAX_HOST_ENTRIES: usize = 100_000;

/// A directory still to be listed by one pattern's traversal.
struct Pending {
    /// The logical path (below the canonical repository root, through links);
    /// listed and statted through the kernel like Bazel's UnixGlob.
    logical: PathBuf,
    /// Its symlink-free path.
    real: PathBuf,
    /// The root that owns it (the repository, a materialized root reached by
    /// a link, or a followed host directory).
    anchor: PathBuf,
    /// Reached through a link to a host path outside every repository.
    host: bool,
    relative: String,
}

/// One include pattern's traversal result.
enum Walk {
    Done(BTreeSet<String>),
    /// An I/O error of UnixGlob's `statIfFound`/`readdir`.
    Io(String),
}

/// UnixGlob's `[unix_jni.cc:382] PATH (strerror)` stat failure text
/// (unix_jni.cc PostException via `NativePosixFiles.stat`).
fn jni_error(path: &Path, error: &io::Error) -> String {
    let text = error.to_string();
    let text = text
        .rsplit_once(" (os error ")
        .map_or(text.as_str(), |(message, _)| message);
    format!("[unix_jni.cc:382] {} ({text})", path.display())
}

/// Return the results of `glob()` like Bazel 9.3's SINGLE_GLOBS_HYBRID
/// globbing (PackageFunction.java:1533, PackageFunctionWithSingleGlobsDep):
///
/// * Results and I/O errors come from the legacy globber (GlobCache over
///   UnixGlob, `vfs/UnixGlob.java:771-882`), one include pattern at a time in
///   order (`GlobCache.globUnsorted`, packages/GlobCache.java:261-291): symlink
///   dirents are statted following links (`statIfFound`, SyscallCache.java:61-63)
///   on their logical path, so a dangling link is skipped and a symlink cycle,
///   a link to its own descendant or a recursion through a link to an ancestor
///   fails in the kernel (`Too many levels of symbolic links`, after 40 links
///   in one path) exactly where Bazel's does; that I/O error is reported in
///   [`GlobOutcome::io_error`] and wins over everything else at package level
///   (PackageFunction.java:551-591: thrown before the Skyframe globs exist).
/// * Directories with a BUILD/BUILD.bazel file (followed) are subpackages
///   (`GlobCache.isSubPackage`, :131-156); ignored subdirectories are neither
///   results nor traversed. No other marker is a boundary: Bazel 9.3 has no
///   nested repositories (`LocalRepositoryLookupFunction.java:27-35`).
/// * Without an I/O error, the Skyframe glob's only observable error is an
///   unbounded ancestor expansion of a directory link reached by a wildcard
///   component (PatternWithWildcardProducer.java:181-197, a link to the
///   package directory or an ancestor package that the legacy globber does not
///   traverse because it has a BUILD file), in [`GlobOutcome::symlink_issue`].
/// * `allow_empty=False` failures use GlobberUtils' texts
///   (packages/GlobberUtils.java:24-55).
///
/// Where links may point is `links` (`source_links::LinkPolicy`).
#[allow(clippy::too_many_arguments)]
pub fn glob(
    root: &Path,
    package: &str,
    includes: &[String],
    excludes: &[String],
    exclude_directories: bool,
    allow_empty: bool,
    ignored: &crate::ignored::IgnoredSubdirectories,
    links: &crate::source_links::LinkPolicy,
) -> Result<GlobOutcome, String> {
    let patterns = includes
        .iter()
        .map(|text| Pattern::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    let exclusions = excludes
        .iter()
        .map(|text| Pattern::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    // Also validates package paths and regular BUILD files.
    crate::workspace::package_build_file(root, package)?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve repository root {}: {error}", root.display()))?;
    let directory = if package.is_empty() {
        canonical_root.clone()
    } else {
        canonical_root.join(package)
    };
    let walker = Walker {
        canonical_root: &canonical_root,
        directory: &directory,
        package,
        exclude_directories,
        ignored,
        links,
    };
    let mut values = crate::source_links::FileValues::default();
    let mut symlink_issue = None;
    let mut results = BTreeSet::new();
    for (pattern, text) in patterns.iter().zip(includes) {
        match walker.walk(pattern, &mut values, &mut symlink_issue)? {
            Walk::Io(error) => {
                let excluded = if excludes.is_empty() {
                    String::new()
                } else {
                    format!(" - [{}]", excludes.join(", "))
                };
                let operation = if exclude_directories {
                    "FILES"
                } else {
                    "FILES_AND_DIRS"
                };
                return Ok(GlobOutcome {
                    paths: Vec::new(),
                    io_error: Some(format!(
                        "error globbing [{}]{excluded} op={operation}: {error}",
                        includes.join(", ")
                    )),
                    symlink_issue: None,
                });
            }
            Walk::Done(paths) => {
                if !allow_empty && paths.is_empty() {
                    return Err(format!(
                        "glob pattern '{text}' didn't match anything, but allow_empty is set to False (the default value of allow_empty can be set with --incompatible_disallow_empty_glob)."
                    ));
                }
                results.extend(paths);
            }
        }
    }
    results.retain(|path| !exclusions.iter().any(|pattern| pattern.matches(path)));
    if !allow_empty && results.is_empty() {
        return Err("all files in the glob have been excluded, but allow_empty is set to False (the default value of allow_empty can be set with --incompatible_disallow_empty_glob).".to_owned());
    }
    Ok(GlobOutcome {
        paths: results.into_iter().collect(),
        io_error: None,
        symlink_issue,
    })
}

struct Walker<'a> {
    canonical_root: &'a Path,
    directory: &'a Path,
    package: &'a str,
    exclude_directories: bool,
    ignored: &'a crate::ignored::IgnoredSubdirectories,
    links: &'a crate::source_links::LinkPolicy,
}

impl Walker<'_> {
    fn walk(
        &self,
        pattern: &Pattern<'_>,
        values: &mut crate::source_links::FileValues,
        symlink_issue: &mut Option<crate::source_links::SymlinkIssue>,
    ) -> Result<Walk, String> {
        use crate::source_links::{FileKind, SymlinkIssue};
        let mut pending = vec![Pending {
            logical: self.directory.to_path_buf(),
            real: self.directory.to_path_buf(),
            anchor: self.canonical_root.to_path_buf(),
            host: false,
            relative: String::new(),
        }];
        let mut result = BTreeSet::new();
        let mut host_entries = 0usize;
        while let Some(current) = pending.pop() {
            let listing = match fs::read_dir(&current.logical) {
                Ok(listing) => listing,
                Err(error) => return Ok(Walk::Io(jni_error(&current.logical, &error))),
            };
            let mut entries = listing.collect::<Result<Vec<_>, _>>().map_err(|error| {
                format!(
                    "cannot read glob entry in {}: {error}",
                    current.logical.display()
                )
            })?;
            if current.host {
                host_entries += entries.len();
                if host_entries > MAX_HOST_ENTRIES {
                    return Err(format!(
                        "glob pattern '{}' visits more than {MAX_HOST_ENTRIES} entries below symlinked host directories in //{} (native safety cap; Bazel has none)",
                        pattern.text, self.package
                    ));
                }
            }
            entries.sort_by_key(|entry| entry.file_name());
            let mut subdirectories = Vec::new();
            for entry in entries {
                let name = entry.file_name().into_string().map_err(|_| {
                    format!(
                        "unsupported non-UTF-8 glob entry in {}",
                        current.logical.display()
                    )
                })?;
                let path = if current.relative.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", current.relative)
                };
                let state = pattern.states(&path);
                let direct = state[state.len() - 1];
                let descend = state[..state.len() - 1].iter().any(|v| *v);
                if !direct && !descend {
                    continue;
                }
                let kind = entry.file_type().map_err(|error| {
                    format!(
                        "cannot inspect glob entry {}: {error}",
                        entry.path().display()
                    )
                })?;
                if (kind.is_dir() || kind.is_symlink())
                    && matches!(name.as_str(), ".git" | ".direnv" | "bazel-rs-out")
                {
                    continue;
                }
                let logical = current.logical.join(&name);
                let (is_directory, is_file, real, anchor, host) = if kind.is_symlink() {
                    // UnixGlob.processSymlink: statIfFound(FOLLOW) on the
                    // logical path; a missing target is skipped.
                    let metadata = match fs::metadata(&logical) {
                        Ok(metadata) => metadata,
                        Err(error)
                            if error.kind() == io::ErrorKind::NotFound
                                || error.kind() == io::ErrorKind::NotADirectory =>
                        {
                            continue;
                        }
                        Err(error) => return Ok(Walk::Io(jni_error(&logical, &error))),
                    };
                    // An ignored directory link is neither a result nor
                    // traversed (GlobCache.globCacheShouldTraverseDirectory),
                    // so nothing is read through it: skip it before the native
                    // link policy (e.g. `bazel-*` convenience links listed in
                    // .bazelignore or REPO.bazel ignore_directories()).
                    if metadata.is_dir() {
                        let repository_path = if self.package.is_empty() {
                            path.clone()
                        } else {
                            format!("{}/{path}", self.package)
                        };
                        if self.ignored.matching_entry(&repository_path).is_some() {
                            continue;
                        }
                    }
                    let real = match fs::canonicalize(&logical) {
                        Ok(real) => real,
                        Err(error) => return Ok(Walk::Io(jni_error(&logical, &error))),
                    };
                    // The Skyframe glob's "more strict than necessary" check
                    // for directory links a wildcard reaches.
                    if metadata.is_dir()
                        && symlink_issue.is_none()
                        && pattern.reached_by_wildcard(&path)
                        && let Ok(value) = values.get(&logical)
                        && value.kind == FileKind::Directory
                        && let Some(chain) = &value.ancestor_expansion
                    {
                        *symlink_issue = Some(SymlinkIssue::InfiniteExpansion(chain.clone()));
                    }
                    let Some(anchor) = self.links.anchor(self.canonical_root, &real) else {
                        return Err(format!(
                            "glob symlink {} resolves to {}, outside its repository root {} (native glob follows links only within the repository, into repositories materialized in this invocation, or for repository-rule-generated repositories into host paths outside the repository store)",
                            entry.path().display(),
                            real.display(),
                            self.canonical_root.display()
                        ));
                    };
                    let host = current.host || anchor == real;
                    (metadata.is_dir(), metadata.is_file(), real, anchor, host)
                } else {
                    (
                        kind.is_dir(),
                        kind.is_file(),
                        current.real.join(&name),
                        current.anchor.clone(),
                        current.host,
                    )
                };
                if is_directory {
                    let repository_path = if self.package.is_empty() {
                        path.clone()
                    } else {
                        format!("{}/{path}", self.package)
                    };
                    // GlobCache.globCacheShouldTraverseDirectory: ignored
                    // directories, then subpackages; the native store is skipped.
                    if self.ignored.matching_entry(&repository_path).is_some()
                        || self.links.skips(&anchor, &real)
                    {
                        continue;
                    }
                    if is_subpackage(&logical)? {
                        continue;
                    }
                    if descend {
                        subdirectories.push(Pending {
                            logical,
                            real,
                            anchor,
                            host,
                            relative: path.clone(),
                        });
                    }
                    if self.exclude_directories {
                        continue;
                    }
                } else if !is_file {
                    if direct {
                        return Err(format!(
                            "unsupported glob file type: {}",
                            entry.path().display()
                        ));
                    }
                    continue;
                }
                if direct && (is_directory || pattern.matches_file(&path)) {
                    result.insert(path);
                }
            }
            // Depth-first in name order.
            pending.extend(subdirectories.into_iter().rev());
        }
        Ok(Walk::Done(result))
    }
}

/// A subpackage: Bazel's package locator follows links
/// (`FileValue.isFile()`, PackageLookupFunction.java:321), so a BUILD link to
/// a file counts, a dangling or directory BUILD does not. MODULE.bazel,
/// REPO.bazel and WORKSPACE files are no boundaries.
fn is_subpackage(directory: &Path) -> Result<bool, String> {
    for marker in ["BUILD.bazel", "BUILD"] {
        let path = directory.join(marker);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(true),
            Ok(_) => {}
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.kind() == io::ErrorKind::NotADirectory => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect glob boundary {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignored::IgnoredSubdirectories;
    use crate::source_links::LinkPolicy;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn none() -> IgnoredSubdirectories {
        IgnoredSubdirectories::default()
    }

    /// The package-level result of one glob() call: Bazel's package errors
    /// for I/O errors and Skyframe symlink issues.
    #[allow(clippy::too_many_arguments)]
    fn glob(
        root: &Path,
        package: &str,
        includes: &[String],
        excludes: &[String],
        exclude_directories: bool,
        allow_empty: bool,
        ignored: &IgnoredSubdirectories,
        links: &LinkPolicy,
    ) -> Result<Vec<String>, String> {
        let outcome = super::glob(
            root,
            package,
            includes,
            excludes,
            exclude_directories,
            allow_empty,
            ignored,
            links,
        )?;
        if let Some(error) = outcome.io_error {
            return Err(format!("no such package '{package}': {error}"));
        }
        if let Some(issue) = outcome.symlink_issue {
            return Err(symlink_issue_error(&issue, package));
        }
        Ok(outcome.paths)
    }

    fn ignoring(prefixes: &[&str], patterns: &[&str]) -> IgnoredSubdirectories {
        IgnoredSubdirectories::new(strings(prefixes), strings(patterns))
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "bazel-rs-glob-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            let temp = Self(path);
            temp.file("BUILD");
            temp
        }

        fn file(&self, path: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "not parsed by glob").unwrap();
        }

        fn run(
            &self,
            includes: &[&str],
            excludes: &[&str],
            dirs: bool,
            empty: bool,
        ) -> Result<Vec<String>, String> {
            glob(
                &self.0,
                "",
                &strings(includes),
                &strings(excludes),
                dirs,
                empty,
                &IgnoredSubdirectories::default(),
                &LinkPolicy::default(),
            )
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn component_wildcards_substrings_case_and_unicode() {
        for (pattern, name, expected) in [
            ("*", "", true),
            ("a*b", "ab", true),
            ("a*b", "axyzb", true),
            ("a*b", "axby", false),
            ("*ab*bc", "ababc", true),
            ("a*b*c", "abc", true),
            ("a*b*c", "axbycz", false),
            ("*", ".hidden", true),
            ("*.rs", ".hidden.rs", false),
            (".*.rs", ".hidden.rs", true),
            ("*.rs", "Main.RS", false),
            ("é*界", "é世界", true),
            ("*é", "世界é", true),
            ("abc", "abc", true),
            ("abc", "ab", false),
        ] {
            assert_eq!(
                component_matches(pattern, name),
                expected,
                "{pattern:?} {name:?}"
            );
        }
    }

    #[test]
    fn recursive_components_match_zero_or_many_components() {
        for (pattern, path, expected) in [
            ("**/a.rs", "a.rs", true),
            ("**/a.rs", "x/y/a.rs", true),
            ("x/**/a.rs", "x/a.rs", true),
            ("x/**/a.rs", "x/y/z/a.rs", true),
            ("*/a.rs", "x/y/a.rs", false),
            ("*.rs", "x/a.rs", false),
            ("x/**", "x", true),
            ("**/**/a", "a", true),
            ("**/x/**/a", "y/x/z/a", true),
            ("**/x/**/a", "y/z/a", false),
        ] {
            assert_eq!(
                Pattern::parse(pattern).unwrap().matches(path),
                expected,
                "{pattern} {path}"
            );
        }
    }

    #[test]
    fn invalid_and_unsupported_patterns_are_errors_even_in_excludes() {
        let temp = Temp::new();
        for pattern in [
            "", "/x", "a/", "a//b", ".", "..", "a/../b", "./a", "a/./b", "a**", "***", "**x/a",
            "a?", "[ab]", "{a,b}", "a\\b", "C:/x", "a\0b", "a\nb",
        ] {
            assert!(
                temp.run(&[pattern], &[], true, true).is_err(),
                "{pattern:?}"
            );
            assert!(
                temp.run(&["*"], &[pattern], true, true).is_err(),
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn sorted_unique_hidden_files_and_exclusions() {
        let temp = Temp::new();
        for path in [
            "z.rs",
            "a.rs",
            "Main.RS",
            ".hidden.rs",
            "x/b.rs",
            "x/.dot.rs",
            "x/n/c.rs",
        ] {
            temp.file(path);
        }
        assert_eq!(
            temp.run(
                &["**/*.rs", ".*.rs", "*.rs", "a.rs"],
                &["x/**/.dot.rs", "**/c.rs"],
                true,
                false
            )
            .unwrap(),
            [".hidden.rs", "a.rs", "x/b.rs", "z.rs"]
        );
        assert_eq!(
            temp.run(&["*.rs"], &[], true, false).unwrap(),
            ["a.rs", "z.rs"]
        );
    }

    #[test]
    fn hidden_directory_matching_follows_bazel_compound_pattern_rule() {
        let temp = Temp::new();
        temp.file(".hidden/a.rs");
        temp.file(".hidden/.dot.rs");
        assert_eq!(
            temp.run(&["*/*.rs"], &[], true, false).unwrap(),
            [".hidden/a.rs"]
        );
        assert_eq!(
            temp.run(&["**/.*.rs"], &[], true, false).unwrap(),
            [".hidden/.dot.rs"]
        );
        assert!(
            temp.run(&["*hidden/*.rs"], &[], true, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn trailing_recursive_includes_require_a_directory_prefix() {
        let temp = Temp::new();
        temp.file("plain");
        temp.file("dir/child");
        for pattern in ["plain/**", "plain/**/**", "**/plain/**"] {
            for exclude_directories in [true, false] {
                assert!(
                    temp.run(&[pattern], &[], exclude_directories, true)
                        .unwrap()
                        .is_empty(),
                    "{pattern}"
                );
                assert!(
                    temp.run(&[pattern], &[], exclude_directories, false)
                        .unwrap_err()
                        .contains("didn't match anything"),
                    "{pattern}"
                );
            }
        }
        assert_eq!(
            temp.run(&["dir/**"], &[], false, false).unwrap(),
            ["dir", "dir/child"]
        );
        assert_eq!(
            temp.run(&["*/**"], &[], true, false).unwrap(),
            ["dir/child"]
        );
        for pattern in ["**", "**/*", "**/**"] {
            assert_eq!(
                temp.run(&[pattern], &[], true, false).unwrap(),
                ["BUILD", "dir/child", "plain"],
                "{pattern}"
            );
        }
        // Exclusions remain path filters: unlike include traversal, their
        // zero-component recursive suffix does not require a directory.
        assert_eq!(
            temp.run(&["**"], &["plain/**"], true, false).unwrap(),
            ["BUILD", "dir/child"]
        );
        assert_eq!(
            temp.run(&["**"], &["dir/**"], false, false).unwrap(),
            ["BUILD", "plain"]
        );
        assert_eq!(
            temp.run(&["**"], &["dir/*"], false, false).unwrap(),
            ["BUILD", "dir", "plain"]
        );
    }

    #[test]
    fn exclusions_match_hidden_names_without_include_discovery_restrictions() {
        let temp = Temp::new();
        for path in [
            ".hidden.txt",
            "visible.txt",
            "nested/.hidden.txt",
            ".dir/.hidden.txt",
            ".dir/visible.txt",
            "keep.rs",
        ] {
            temp.file(path);
        }
        assert_eq!(
            temp.run(&["*"], &["**/*.txt"], true, true).unwrap(),
            ["BUILD", "keep.rs"]
        );
        assert_eq!(
            temp.run(&["**"], &["**/*.txt"], true, false).unwrap(),
            ["BUILD", "keep.rs"]
        );
        assert_eq!(
            temp.run(&["*"], &["*.txt"], true, false).unwrap(),
            ["BUILD", "keep.rs"]
        );
        assert_eq!(
            temp.run(&["**/*.txt"], &[], true, false).unwrap(),
            [".dir/visible.txt", "visible.txt"]
        );
        assert_eq!(
            temp.run(&[".dir/**"], &["*dir/**"], false, true).unwrap(),
            Vec::<String>::new()
        );
        assert!(
            temp.run(&[".hidden.txt"], &["**/*.txt"], true, false)
                .unwrap_err()
                .contains("have been excluded")
        );
    }

    #[test]
    fn package_relative_and_package_validation() {
        let temp = Temp::new();
        temp.file("p/BUILD.bazel");
        temp.file("p/a.txt");
        temp.file("outside.txt");
        assert_eq!(
            glob(
                &temp.0,
                "p",
                &strings(&["*.txt"]),
                &[],
                true,
                false,
                &none(),
                &LinkPolicy::default()
            )
            .unwrap(),
            ["a.txt"]
        );
        for package in ["../p", "p/..", "/p", "p//x", "p/.", "missing"] {
            assert!(
                glob(
                    &temp.0,
                    package,
                    &[],
                    &[],
                    true,
                    true,
                    &none(),
                    &LinkPolicy::default()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn directories_and_empty_directories_are_optional_but_root_is_never_returned() {
        let temp = Temp::new();
        fs::create_dir_all(temp.0.join("empty")).unwrap();
        temp.file("dir/file");
        assert_eq!(
            temp.run(&["**"], &[], true, false).unwrap(),
            ["BUILD", "dir/file"]
        );
        assert_eq!(
            temp.run(&["**"], &[], false, false).unwrap(),
            ["BUILD", "dir", "dir/file", "empty"]
        );
        assert!(temp.run(&["empty"], &[], true, false).is_err());
        assert_eq!(temp.run(&["empty"], &[], false, false).unwrap(), ["empty"]);
    }

    #[test]
    fn allow_empty_checks_each_include_then_combined_exclusions() {
        let temp = Temp::new();
        temp.file("a.txt");
        assert!(
            temp.run(&["a.txt", "missing"], &[], true, false)
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            temp.run(&["*.txt"], &["a.txt"], true, false)
                .unwrap_err()
                .contains("have been excluded")
        );
        assert!(temp.run(&[], &[], true, false).is_err());
        assert!(temp.run(&[], &[], true, true).unwrap().is_empty());
        assert!(temp.run(&["missing"], &[], true, true).unwrap().is_empty());
        assert_eq!(
            temp.run(&["a.txt", "BUILD"], &["a.txt"], true, false)
                .unwrap(),
            ["BUILD"]
        );
    }

    #[test]
    fn only_build_files_and_reserved_trees_are_boundaries() {
        let temp = Temp::new();
        for (index, marker) in [
            "BUILD",
            "BUILD.bazel",
            "MODULE.bazel",
            "REPO.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
        ]
        .iter()
        .enumerate()
        {
            temp.file(&format!("boundary{index}/{marker}"));
            temp.file(&format!("boundary{index}/deep/source.rs"));
        }
        for name in [".git", ".direnv", "bazel-rs-out"] {
            temp.file(&format!("{name}/generated.rs"));
            temp.file(&format!("src/{name}/generated.rs"));
        }
        temp.file("src/good.rs");
        // Bazel 9.3 has no nested repositories: MODULE.bazel, REPO.bazel and
        // WORKSPACE files are ordinary sources (LocalRepositoryLookupFunction).
        assert_eq!(
            temp.run(&["**/*.rs"], &[], true, false).unwrap(),
            [
                "boundary2/deep/source.rs",
                "boundary3/deep/source.rs",
                "boundary4/deep/source.rs",
                "boundary5/deep/source.rs",
                "src/good.rs"
            ]
        );
        assert_eq!(
            temp.run(&["boundary2/*"], &[], true, false).unwrap(),
            ["boundary2/MODULE.bazel"]
        );
        assert!(
            temp.run(&["boundary0/**"], &[], false, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn blocked_store_prefixes_are_skipped_unless_they_own_the_root() {
        let temp = Temp::new();
        temp.file("cache/repositories/trees/x/BUILD.bazel");
        temp.file("cache/repositories/trees/x/data.rs");
        temp.file("cache/repositories/trees/.stage-1/f.rs");
        temp.file("src/a.rs");
        let root = canonical(&temp);
        let policy = LinkPolicy {
            blocked: vec![root.join("cache")],
            ..LinkPolicy::default()
        };
        let run = |root: &Path, policy: &LinkPolicy| {
            glob(
                root,
                "",
                &strings(&["**/*.rs"]),
                &[],
                true,
                true,
                &none(),
                policy,
            )
        };
        assert_eq!(run(&temp.0, &policy).unwrap(), ["src/a.rs"]);
        assert_eq!(
            run(&temp.0, &LinkPolicy::default()).unwrap(),
            ["cache/repositories/trees/.stage-1/f.rs", "src/a.rs"]
        );
        // A repository inside the store globs its own tree.
        assert_eq!(
            run(&root.join("cache/repositories/trees/x"), &policy).unwrap(),
            ["data.rs"]
        );
    }

    #[test]
    fn marker_directories_do_not_create_boundaries() {
        let temp = Temp::new();
        temp.file("src/BUILD/source.rs");
        temp.file("src/WORKSPACE/source.rs");
        assert_eq!(
            temp.run(&["**/*.rs"], &[], true, false).unwrap(),
            ["src/BUILD/source.rs", "src/WORKSPACE/source.rs"]
        );
    }

    fn canonical(temp: &Temp) -> PathBuf {
        fs::canonicalize(&temp.0).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn file_directory_and_dangling_links_are_followed_like_bazel() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("src/a.rs");
        temp.file("src/deep/b.rs");
        symlink(temp.0.join("src/a.rs"), temp.0.join("alias.rs")).unwrap();
        symlink("src", temp.0.join("dirlink")).unwrap();
        symlink("missing", temp.0.join("dangling.rs")).unwrap();
        symlink("src/missing", temp.0.join("dangling-dir")).unwrap();
        symlink(&temp.0, temp.0.join("bazel-rs-out")).unwrap();
        // Results use the link's own path, sorted.
        assert_eq!(
            temp.run(&["**/*.rs"], &[], true, false).unwrap(),
            [
                "alias.rs",
                "dirlink/a.rs",
                "dirlink/deep/b.rs",
                "src/a.rs",
                "src/deep/b.rs"
            ]
        );
        assert_eq!(
            temp.run(&["*"], &[], false, false).unwrap(),
            ["BUILD", "alias.rs", "dirlink", "src"]
        );
        assert_eq!(
            temp.run(&["*"], &[], true, false).unwrap(),
            ["BUILD", "alias.rs"]
        );
        // Dangling links are silently skipped, even literally named.
        assert!(
            temp.run(&["dangling.rs"], &[], true, false)
                .unwrap_err()
                .contains("didn't match anything")
        );
        assert!(
            temp.run(&["dangling-dir/**"], &[], false, true)
                .unwrap()
                .is_empty()
        );
        // Exclusions filter followed results.
        assert_eq!(
            temp.run(&["**/*.rs"], &["dirlink/**"], true, false)
                .unwrap(),
            ["alias.rs", "src/a.rs", "src/deep/b.rs"]
        );
        // A trailing ** through a file link matches nothing, as for files.
        assert!(
            temp.run(&["alias.rs/**"], &[], true, true)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_are_legacy_globbing_io_errors_even_when_excluded() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("src/a.rs");
        symlink("loop", temp.0.join("loop")).unwrap();
        let root = canonical(&temp);
        // UnixGlob's statIfFound fails in the kernel; the package fails with
        // StarlarkNativeModule's `error globbing` text, includes/excludes and
        // operation included (StarlarkNativeModule.java:851-876).
        for (includes, excludes, dirs, empty, shown) in [
            (&["*"][..], &[][..], true, true, "[*] op=FILES"),
            (&["**/*.rs"][..], &[][..], true, false, "[**/*.rs] op=FILES"),
            (
                &["*"][..],
                &["loop"][..],
                true,
                true,
                "[*] - [loop] op=FILES",
            ),
            (
                &["l*", "src/*"][..],
                &["**"][..],
                false,
                true,
                "[l*, src/*] - [**] op=FILES_AND_DIRS",
            ),
            (&["loop"][..], &[][..], true, true, "[loop] op=FILES"),
        ] {
            assert_eq!(
                temp.run(includes, excludes, dirs, empty).unwrap_err(),
                format!(
                    "no such package '': error globbing {shown}: [unix_jni.cc:382] {}/loop (Too many levels of symbolic links)",
                    root.display()
                ),
                "{includes:?} {excludes:?}"
            );
        }
        // A link no include reaches is never statted.
        assert_eq!(
            temp.run(&["src/*"], &[], true, false).unwrap(),
            ["src/a.rs"]
        );
        // An earlier include's allow_empty failure precedes a later I/O error
        // (GlobCache.globUnsorted, one pattern at a time).
        assert!(
            temp.run(&["missing", "*"], &[], true, false)
                .unwrap_err()
                .starts_with("glob pattern 'missing' didn't match anything")
        );
        // Two-link cycles: the first link in name order (Bazel reports
        // whichever concurrent stat fails first).
        temp.file("p/BUILD");
        symlink("b", temp.0.join("p/a")).unwrap();
        symlink("a", temp.0.join("p/b")).unwrap();
        assert_eq!(
            glob(
                &temp.0,
                "p",
                &strings(&["*"]),
                &[],
                true,
                true,
                &none(),
                &LinkPolicy::default(),
            )
            .unwrap_err(),
            format!(
                "no such package 'p': error globbing [*] op=FILES: [unix_jni.cc:382] {}/p/a (Too many levels of symbolic links)",
                root.display()
            )
        );
        // A link to its own descendant loops in the kernel too.
        fs::remove_file(temp.0.join("loop")).unwrap();
        symlink("down/x", temp.0.join("down")).unwrap();
        assert!(
            temp.run(&["*"], &[], true, true)
                .unwrap_err()
                .ends_with("/down (Too many levels of symbolic links)")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_links_are_skyframe_expansions_or_legacy_recursion_errors() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("p/BUILD");
        temp.file("p/sub/a.rs");
        symlink(".", temp.0.join("p/up")).unwrap();
        symlink("..", temp.0.join("p/sub/back")).unwrap();
        let root = canonical(&temp);
        let run = |includes: &[&str], excludes: &[&str]| {
            glob(
                &temp.0,
                "p",
                &strings(includes),
                &strings(excludes),
                true,
                true,
                &none(),
                &LinkPolicy::default(),
            )
        };
        // A link to the package directory: the legacy globber does not
        // descend (a subpackage), the Skyframe glob reports the expansion,
        // "more strict than necessary" even for a single-level wildcard.
        let up = format!(
            "no such package 'p': Symlink issue while evaluating globs: Infinite symlink expansion: {0}/p/up- > {0}/p",
            root.display()
        );
        for includes in [&["**"][..], &["*"][..], &["u*/x"][..]] {
            assert_eq!(run(includes, &[]).unwrap_err(), up, "{includes:?}");
        }
        assert_eq!(run(&["*"], &["up"]).unwrap_err(), up);
        // A literal component is a plain FileValue lookup: no ancestor check.
        assert!(run(&["up"], &[]).unwrap().is_empty());
        assert_eq!(run(&["sub/*.rs"], &[]).unwrap(), ["sub/a.rs"]);
        let back = format!(
            "no such package 'p': Symlink issue while evaluating globs: Infinite symlink expansion: {0}/p/sub/back- > {0}/p",
            root.display()
        );
        assert_eq!(run(&["sub/*"], &[]).unwrap_err(), back);
        fs::remove_file(temp.0.join("p/up")).unwrap();
        fs::remove_file(temp.0.join("p/sub/back")).unwrap();
        // A link to an ancestor without a BUILD file: the legacy globber
        // recurses until the kernel refuses the 41st link of one path.
        temp.file("p/a/b/x.rs");
        symlink("..", temp.0.join("p/a/b/up")).unwrap();
        let error = run(&["**/x.rs"], &[]).unwrap_err();
        let deep = format!("{}/p/a/b/up{}", root.display(), "/b/up".repeat(40));
        assert_eq!(
            error,
            format!(
                "no such package 'p': error globbing [**/x.rs] op=FILES: [unix_jni.cc:382] {deep} (Too many levels of symbolic links)"
            )
        );
        // Literal and single-level patterns through it do not recurse.
        assert_eq!(run(&["a/b/up/b/x.rs"], &[]).unwrap(), ["a/b/up/b/x.rs"]);
        // A link into a sibling's descendant is finite.
        fs::remove_file(temp.0.join("p/a/b/up")).unwrap();
        symlink("sub", temp.0.join("p/side")).unwrap();
        assert_eq!(
            run(&["**/*.rs"], &[]).unwrap(),
            ["a/b/x.rs", "side/a.rs", "sub/a.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_directories_with_build_files_are_subpackages() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("other/BUILD");
        temp.file("other/x.rs");
        temp.file("plain/y.rs");
        symlink("other", temp.0.join("pkglink")).unwrap();
        symlink("plain", temp.0.join("dirlink")).unwrap();
        assert_eq!(
            temp.run(&["**/*.rs"], &[], true, false).unwrap(),
            ["dirlink/y.rs", "plain/y.rs"]
        );
        assert_eq!(
            temp.run(&["**"], &[], false, false).unwrap(),
            ["BUILD", "dirlink", "dirlink/y.rs", "plain", "plain/y.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_markers_follow_their_targets() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("nested/source.rs");
        temp.file("marked/source.rs");
        temp.file("real-build");
        symlink("missing", temp.0.join("nested/BUILD")).unwrap();
        symlink("../real-build", temp.0.join("marked/BUILD")).unwrap();
        // PackageLookupFunction: a dangling BUILD link is no BUILD file.
        assert_eq!(
            temp.run(&["**/*.rs"], &[], true, false).unwrap(),
            ["nested/source.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_targets_follow_the_shared_policy() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let outside_root = fs::canonicalize(outside.path()).unwrap();
        fs::create_dir_all(outside_root.join("jdk/bin")).unwrap();
        fs::create_dir_all(outside_root.join("store/stale")).unwrap();
        fs::write(outside_root.join("jdk/bin/java"), "java").unwrap();
        fs::write(outside_root.join("store/stale/f"), "stale").unwrap();
        symlink("missing.zip", outside_root.join("jdk/bin/src.zip")).unwrap();
        let temp = Temp::new();
        symlink(outside_root.join("jdk/bin"), temp.0.join("bin")).unwrap();
        symlink(outside_root.join("absent"), temp.0.join("gone")).unwrap();
        let run = |includes: &[&str], policy: &LinkPolicy| {
            glob(
                &temp.0,
                "",
                &strings(includes),
                &[],
                true,
                true,
                &none(),
                policy,
            )
        };
        // Main/archive/local trees: host targets stay an explicit error.
        let error = run(&["bin/**"], &LinkPolicy::default()).unwrap_err();
        assert!(error.contains("outside its repository root"), "{error}");
        // Dangling host links are skipped under any policy.
        assert!(run(&["gone"], &LinkPolicy::default()).unwrap().is_empty());
        // Repository-rule-generated trees may read host paths (local_jdk).
        let generated = LinkPolicy {
            host_reads: true,
            ..LinkPolicy::default()
        };
        assert_eq!(run(&["bin/**"], &generated).unwrap(), ["bin/java"]);
        // A materialized root admits the same link without host reads.
        let materialized = LinkPolicy {
            others: vec![("jdk+".to_owned(), outside_root.join("jdk"))],
            ..LinkPolicy::default()
        };
        assert_eq!(run(&["bin/*"], &materialized).unwrap(), ["bin/java"]);
        // Never into the repository store, even for generated trees.
        fs::remove_file(temp.0.join("bin")).unwrap();
        symlink(outside_root.join("store/stale"), temp.0.join("bin")).unwrap();
        let blocked = LinkPolicy {
            host_reads: true,
            blocked: vec![outside_root.join("store")],
            ..LinkPolicy::default()
        };
        let error = run(&["bin/*"], &blocked).unwrap_err();
        assert!(error.contains("outside its repository root"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_file_types_and_non_utf8_names_error() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::net::UnixListener;
        let temp = Temp::new();
        let listener = UnixListener::bind(temp.0.join("socket")).unwrap();
        assert!(
            temp.run(&["socket"], &[], true, true)
                .unwrap_err()
                .contains("file type")
        );
        drop(listener);
        fs::remove_file(temp.0.join("socket")).unwrap();
        fs::write(temp.0.join(std::ffi::OsString::from_vec(vec![0xff])), "").unwrap();
        assert!(
            temp.run(&["*"], &[], true, true)
                .unwrap_err()
                .contains("non-UTF-8")
        );
    }

    #[test]
    fn ignored_directories_are_neither_results_nor_traversed() {
        let temp = Temp::new();
        temp.file("p/BUILD");
        for path in [
            "p/a.js",
            "p/node_modules/dep/index.js",
            "p/src/node_modules/x.js",
            "p/src/keep.js",
            "p/.venv/lib/site.py",
            "p/vendor/lib.js",
            "p/vendored/lib.js",
            // A BUILD file inside an ignored directory is no boundary either.
            "p/gen/BUILD",
            "p/gen/x.js",
        ] {
            temp.file(path);
        }
        // A regular file whose path matches an entry is still a source.
        temp.file("p/files/node_modules");
        let ignored = ignoring(&["p/vendor", "p/gen"], &["**/node_modules", "**/.venv"]);
        let run = |includes: &[&str], dirs: bool, empty: bool, ignored: &IgnoredSubdirectories| {
            glob(
                &temp.0,
                "p",
                &strings(includes),
                &[],
                dirs,
                empty,
                ignored,
                &LinkPolicy::default(),
            )
        };
        assert_eq!(
            run(&["**"], false, false, &ignored).unwrap(),
            [
                "BUILD",
                "a.js",
                "files",
                "files/node_modules",
                "src",
                "src/keep.js",
                "vendored",
                "vendored/lib.js"
            ]
        );
        assert_eq!(
            run(&["**/*.js"], true, false, &ignored).unwrap(),
            ["a.js", "src/keep.js", "vendored/lib.js"]
        );
        // Without ignored entries, the same trees are globbed (gen is a
        // subpackage boundary).
        assert_eq!(
            run(&["**/*.js"], true, false, &none()).unwrap(),
            [
                "a.js",
                "node_modules/dep/index.js",
                "src/keep.js",
                "src/node_modules/x.js",
                "vendor/lib.js",
                "vendored/lib.js"
            ]
        );
        // An include that only reaches ignored directories matches nothing.
        assert!(
            run(&["node_modules/**"], false, false, &ignored)
                .unwrap_err()
                .contains("didn't match anything")
        );
        assert!(run(&["vendor"], false, true, &ignored).unwrap().is_empty());
        // Entries are repository-relative: the package prefix matters.
        assert_eq!(
            run(&["vendor/*.js"], true, false, &ignoring(&["vendor"], &[])).unwrap(),
            ["vendor/lib.js"]
        );
        // Deterministic: sorted and identical across runs.
        assert_eq!(
            run(&["**"], false, false, &ignored).unwrap(),
            run(&["**"], false, false, &ignored).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignored_host_directory_links_are_skipped_before_the_link_policy() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let host = Temp::new();
        temp.file("a.txt");
        host.file("x/h.txt");
        // A `bazel-out`-shaped convenience link to a host directory.
        symlink(&host.0, temp.0.join("bazel-out")).unwrap();
        let run = |ignored: &IgnoredSubdirectories| {
            glob(
                &temp.0,
                "",
                &strings(&["**/*.txt"]),
                &[],
                true,
                true,
                ignored,
                &LinkPolicy::default(),
            )
        };
        // Not ignored: the main repository's explicit host-link refusal.
        assert!(
            run(&none())
                .unwrap_err()
                .contains("outside its repository root")
        );
        // Ignored (.bazelignore entry or REPO.bazel ignore_directories):
        // neither traversed nor read, so no refusal.
        assert_eq!(run(&ignoring(&["bazel-out"], &[])).unwrap(), ["a.txt"]);
        assert_eq!(run(&ignoring(&[], &["bazel-*"])).unwrap(), ["a.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn ignored_symlinks_to_directories_are_skipped_without_traversal() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("real/a.js");
        temp.file("file.js");
        symlink(temp.0.join("real"), temp.0.join("node_modules")).unwrap();
        symlink(temp.0.join("file.js"), temp.0.join("linked.js")).unwrap();
        let ignored = ignoring(&[], &["**/node_modules", "linked.js"]);
        let run = |ignored: &IgnoredSubdirectories| {
            glob(
                &temp.0,
                "",
                &strings(&["**/*.js"]),
                &[],
                true,
                true,
                ignored,
                &LinkPolicy::default(),
            )
        };
        // Ignored symlink to a directory: skipped like the directory itself.
        // A symlink to a file is no directory, so ignoring cannot skip it.
        assert_eq!(
            run(&ignored).unwrap(),
            ["file.js", "linked.js", "real/a.js"]
        );
        assert_eq!(
            run(&none()).unwrap(),
            ["file.js", "linked.js", "node_modules/a.js", "real/a.js"]
        );
    }
}
