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
//! Symlinks are never followed. A potentially matching symlink is an error,
//! unless its entire relevant contribution is excluded and `allow_empty` is
//! true. With `allow_empty=false`, pre-exclusion matches must be known, so even
//! excluded, potentially matching symlinks are errors. Irrelevant symlinks and
//! reserved trees are ignored. Special filesystem objects are unsupported.
//!
//! Directories matched by the repository's ignored subdirectories
//! (`.bazelignore` prefixes and REPO.bazel `ignore_directories()` patterns,
//! `crate::ignored`) are neither results nor traversed, exactly like Bazel's
//! `DirectoryDirentProducer`/`GlobFunction` `matchingEntry` checks on the
//! repository-relative directory path. Regular files are never checked.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

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

    fn matches(&self, path: &str) -> bool {
        self.match_states(path, MatchMode::Exclude)[self.components.len()]
    }

    fn matches_file(&self, path: &str) -> bool {
        self.match_states(path, MatchMode::IncludeFile)[self.components.len()]
    }

    fn covers_tree(&self, path: &str) -> bool {
        // A reachable suffix consisting solely of ** covers this directory
        // and every possible descendant, without inspecting a symlink target.
        let states = self.match_states(path, MatchMode::Exclude);
        (0..self.components.len())
            .any(|index| states[index] && self.components[index..].iter().all(|part| *part == "**"))
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

/// Return sorted, unique package-relative source paths.
///
/// When `allow_empty` is false, every include must match an eligible source
/// entry before exclusions, and the combined result after exclusions must
/// also be nonempty. Directories count only if `exclude_directories` is false.
pub fn glob(
    root: &Path,
    package: &str,
    includes: &[String],
    excludes: &[String],
    exclude_directories: bool,
    allow_empty: bool,
    ignored: &crate::ignored::IgnoredSubdirectories,
) -> Result<Vec<String>, String> {
    let includes = includes
        .iter()
        .map(|text| Pattern::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    let excludes = excludes
        .iter()
        .map(|text| Pattern::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    // Also validates package paths, regular BUILD files, symlink package
    // directories and nested repository crossings.
    let build = crate::workspace::package_build_file(root, package)?;
    let directory = build.parent().ok_or("package BUILD file has no parent")?;
    let mut pending = vec![(directory.to_path_buf(), String::new())];
    let mut matched = vec![false; includes.len()];
    let mut result = BTreeSet::new();
    while let Some((directory, relative)) = pending.pop() {
        if includes.is_empty() {
            break;
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| {
                format!(
                    "cannot read glob directory {}: {error}",
                    directory.display()
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                format!("cannot read glob entry in {}: {error}", directory.display())
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().into_string().map_err(|_| {
                format!(
                    "unsupported non-UTF-8 glob entry in {}",
                    directory.display()
                )
            })?;
            let path = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            let states: Vec<_> = includes
                .iter()
                .map(|pattern| pattern.states(&path))
                .collect();
            let direct = states.iter().any(|state| state[state.len() - 1]);
            let descend = states
                .iter()
                .any(|state| state[..state.len() - 1].iter().any(|v| *v));
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
            if kind.is_dir() || kind.is_symlink() {
                let repository_path = if package.is_empty() {
                    path.clone()
                } else {
                    format!("{package}/{path}")
                };
                // Bazel resolves a symlink dirent first and checks only the
                // resulting directories; a stat decides without traversal.
                if ignored.matching_entry(&repository_path).is_some()
                    && (kind.is_dir() || fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()))
                {
                    continue;
                }
            }
            let excluded = excludes.iter().any(|pattern| pattern.matches(&path));
            let tree_excluded = excludes.iter().any(|pattern| pattern.covers_tree(&path));
            if allow_empty && tree_excluded {
                continue;
            }
            if kind.is_symlink() {
                if allow_empty && excluded && !descend {
                    continue;
                }
                return Err(format!(
                    "glob could match unsupported symlink {} (symlinks are never followed)",
                    entry.path().display()
                ));
            }
            if kind.is_dir() {
                if is_boundary(&entry.path())? {
                    continue;
                }
                if descend {
                    pending.push((entry.path(), path.clone()));
                }
                if exclude_directories {
                    continue;
                }
            } else if !kind.is_file() {
                if direct {
                    return Err(format!(
                        "unsupported glob file type: {}",
                        entry.path().display()
                    ));
                }
                continue;
            }
            for (index, state) in states.iter().enumerate() {
                if state[state.len() - 1] && (kind.is_dir() || includes[index].matches_file(&path))
                {
                    matched[index] = true;
                    if !excluded {
                        result.insert(path.clone());
                    }
                }
            }
        }
    }
    if !allow_empty {
        for (pattern, matched) in includes.iter().zip(matched) {
            if !matched {
                return Err(format!(
                    "glob include pattern {:?} matched no source entries in //{package} (allow_empty=False)",
                    pattern.text
                ));
            }
        }
        if result.is_empty() {
            return Err(format!(
                "glob result is empty after exclusions in //{package} (allow_empty=False)"
            ));
        }
    }
    Ok(result.into_iter().collect())
}

fn is_boundary(directory: &Path) -> Result<bool, String> {
    for marker in [
        "BUILD.bazel",
        "BUILD",
        "MODULE.bazel",
        "REPO.bazel",
        "WORKSPACE.bazel",
        "WORKSPACE",
    ] {
        let path = directory.join(marker);
        match fs::symlink_metadata(&path) {
            // A symlink marker is conservatively a boundary, without following
            // it (including dangling or external marker symlinks).
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                return Ok(true);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn none() -> IgnoredSubdirectories {
        IgnoredSubdirectories::default()
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
                        .contains("matched no source entries"),
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
                .contains("after exclusions")
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
                &none()
            )
            .unwrap(),
            ["a.txt"]
        );
        for package in ["../p", "p/..", "/p", "p//x", "p/.", "missing"] {
            assert!(glob(&temp.0, package, &[], &[], true, true, &none()).is_err());
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
                .contains("after exclusions")
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
    fn packages_repositories_and_reserved_trees_are_boundaries() {
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
        temp.file("target/source.rs");
        temp.file("bazel-out/source.rs");
        assert_eq!(
            temp.run(&["**"], &[], false, false).unwrap(),
            [
                "BUILD",
                "bazel-out",
                "bazel-out/source.rs",
                "src",
                "src/good.rs",
                "target",
                "target/source.rs"
            ]
        );
        assert!(
            temp.run(&["boundary0/**"], &[], false, true)
                .unwrap()
                .is_empty()
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

    #[cfg(unix)]
    #[test]
    fn symlinks_fail_only_when_relevant_and_are_never_followed() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("src/a.rs");
        symlink(&temp.0, temp.0.join("loop")).unwrap();
        symlink("missing", temp.0.join("dangling")).unwrap();
        symlink(temp.0.join("src/a.rs"), temp.0.join("alias.rs")).unwrap();
        symlink(&temp.0, temp.0.join("bazel-rs-out")).unwrap();
        assert_eq!(
            temp.run(&["src/*.rs"], &[], true, false).unwrap(),
            ["src/a.rs"]
        );
        for pattern in ["**/*.rs", "loop/**/*.rs", "dangling", "alias.rs"] {
            assert!(
                temp.run(&[pattern], &[], true, true)
                    .unwrap_err()
                    .contains("symlink")
            );
        }
        assert_eq!(
            temp.run(
                &["**/*.rs"],
                &["loop/**", "dangling/**", "alias.rs/**"],
                true,
                true
            )
            .unwrap(),
            ["src/a.rs"]
        );
        assert!(
            temp.run(&["alias.rs"], &["alias.rs"], true, true)
                .unwrap()
                .is_empty()
        );
        assert!(temp.run(&["alias.rs"], &["alias.rs"], true, false).is_err());
        assert!(glob(&temp.0, "loop", &strings(&["**"]), &[], true, true, &none()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_markers_are_conservative_boundaries() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.file("nested/source.rs");
        symlink("missing", temp.0.join("nested/BUILD")).unwrap();
        assert_eq!(temp.run(&["**"], &[], false, false).unwrap(), ["BUILD"]);
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
            glob(&temp.0, "p", &strings(includes), &[], dirs, empty, ignored)
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
                .contains("matched no source entries")
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
            )
        };
        // Ignored symlink to a directory: skipped like the directory itself.
        // A symlink to a file is no directory, so ignoring cannot skip it.
        assert!(run(&ignored).unwrap_err().contains("unsupported symlink"));
        fs::remove_file(temp.0.join("linked.js")).unwrap();
        assert_eq!(run(&ignored).unwrap(), ["file.js", "real/a.js"]);
        assert!(run(&none()).unwrap_err().contains("unsupported symlink"));
    }
}
