//! Bazel 9.3 ignored subdirectories (`.bazelignore` and REPO.bazel
//! `ignore_directories()`), see docs/IGNORED_DIRECTORIES.md.
//!
//! * `IgnoredSubdirectories` mirrors `cmdline/IgnoredSubdirectories.java`:
//!   repository-relative path prefixes (from `.bazelignore`) and glob patterns
//!   (from `ignore_directories()`), `matching_entry` (prefixes first, then
//!   patterns, first match wins) and `filter_for_directory`.
//! * Pattern matching mirrors `vfs/UnixGlob.java` `matchesPattern`
//!   (`EXACT`/`PREFIX`/`CAN_MATCH_CHILD`) and the segment matcher
//!   `UnixGlob.matches(String, String, Map)`, including its fast paths and its
//!   `java.util.regex` translation. Bazel compares REPO.bazel strings and
//!   directory names in its Latin-1 "internal" encoding (one char per byte),
//!   so matching is byte-wise here.
//! * `.bazelignore` parsing mirrors `skyframe/IgnoredSubdirectoriesFunction.java`
//!   (`PathFragmentLineProcessor`, `PathFragment.create` normalization).

use crate::observed_fs;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Repository-relative ignored prefixes and patterns of one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoredSubdirectories {
    /// Normalized `.bazelignore` prefixes, deduplicated in first-seen order
    /// (`ImmutableSet`); `seen` indexes their texts.
    prefixes: Vec<Prefix>,
    seen: HashSet<String>,
    /// `ignore_directories()` strings in call order (`ImmutableList`).
    patterns: Vec<String>,
}

/// A `.bazelignore` entry. `text` is the normalized `PathFragment` string as
/// Bazel prints it; `internal` is the byte string a directory path must equal
/// (segment-wise prefix) for the entry to match, or `None` when no UTF-8
/// directory path can ever match it (see `latin1_internal`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Prefix {
    text: String,
    internal: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Exact,
    Prefix,
    CanMatchChild,
}

impl IgnoredSubdirectories {
    pub fn new(prefixes: Vec<String>, patterns: Vec<String>) -> Self {
        let mut result = Self {
            patterns,
            ..Self::default()
        };
        for prefix in prefixes {
            result.push_prefix(Prefix {
                internal: Some(prefix.clone()),
                text: prefix,
            });
        }
        result
    }

    fn push_prefix(&mut self, prefix: Prefix) {
        if self.seen.insert(prefix.text.clone()) {
            self.prefixes.push(prefix);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty() && self.patterns.is_empty()
    }

    pub fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.prefixes.iter().map(|p| p.text.as_str())
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// `IgnoredSubdirectories.matchingEntry`: the first prefix that is a
    /// segment-wise prefix of `directory`, else the first pattern matching a
    /// prefix of its segments.
    pub fn matching_entry(&self, directory: &str) -> Option<&str> {
        for prefix in &self.prefixes {
            if prefix
                .internal
                .as_deref()
                .is_some_and(|p| path_starts_with(directory, p))
            {
                return Some(&prefix.text);
            }
        }
        let path = segments(directory);
        self.patterns
            .iter()
            .find(|pattern| matches_pattern(&split_pattern(pattern), &path, MatchMode::Prefix))
            .map(String::as_str)
    }

    /// `IgnoredSubdirectories.filterForDirectory`: drop entries that cannot
    /// match anything under `directory`.
    pub fn filter_for_directory(&self, directory: &str) -> Self {
        let path = segments(directory);
        let prefixes: Vec<Prefix> = self
            .prefixes
            .iter()
            .filter(|p| {
                p.internal
                    .as_deref()
                    .is_some_and(|p| path_starts_with(p, directory))
            })
            .cloned()
            .collect();
        Self {
            seen: prefixes.iter().map(|p| p.text.clone()).collect(),
            prefixes,
            patterns: self
                .patterns
                .iter()
                .filter(|p| matches_pattern(&split_pattern(p), &path, MatchMode::CanMatchChild))
                .cloned()
                .collect(),
        }
    }
}

/// `PathFragment.startsWith` for relative normalized paths.
fn path_starts_with(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `PathFragment.segments()`: the empty fragment has no segments.
fn segments(path: &str) -> Vec<&str> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    }
}

/// `Splitter.on("/")`: empty segments are kept (and never match).
fn split_pattern(pattern: &str) -> Vec<&str> {
    pattern.split('/').collect()
}

/// `UnixGlob.matches(String[], String[])`.
pub fn matches(pattern: &str, path: &str) -> bool {
    matches_pattern(&split_pattern(pattern), &segments(path), MatchMode::Exact)
}

/// `UnixGlob.matchesPrefix`.
pub fn matches_prefix(pattern: &str, path: &str) -> bool {
    matches_pattern(&split_pattern(pattern), &segments(path), MatchMode::Prefix)
}

/// `UnixGlob.canMatchChild`.
pub fn can_match_child(pattern: &str, path: &str) -> bool {
    matches_pattern(
        &split_pattern(pattern),
        &segments(path),
        MatchMode::CanMatchChild,
    )
}

/// `UnixGlob.matchesPattern`, as a bottom-up table over (pattern index,
/// path index) so repeated `**` segments cannot cause exponential work. The
/// recurrence is exactly Bazel's recursive one.
fn matches_pattern(pattern: &[&str], path: &[&str], mode: MatchMode) -> bool {
    let (p, n) = (pattern.len(), path.len());
    let width = n + 1;
    let mut table = vec![false; (p + 1) * width];
    for i in (0..=p).rev() {
        for j in (0..=n).rev() {
            table[i * width + j] = if i == p {
                mode == MatchMode::Prefix || j == n
            } else if pattern[i] == "**" {
                table[(i + 1) * width + j] || (j < n && table[i * width + j + 1])
            } else if j == n {
                mode == MatchMode::CanMatchChild
            } else {
                segment_matches(pattern[i], path[j]) && table[(i + 1) * width + j + 1]
            };
        }
    }
    table[0]
}

/// `UnixGlob.matches(String pattern, String str, Map)` on Latin-1 internal
/// strings, i.e. bytes.
pub fn segment_matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    if pattern.is_empty() || name.is_empty() {
        return false;
    }
    if pattern == b"**" || pattern == b"*" {
        return true;
    }
    // A leading '.' must be matched explicitly.
    if name[0] == b'.' && pattern[0] != b'.' {
        return false;
    }
    let stars = pattern.iter().filter(|&&b| b == b'*').count();
    // Fast paths `*xyz` and `xyz*` compare the rest literally ('?', '(' ...).
    if pattern[0] == b'*' && stars == 1 {
        return name.ends_with(&pattern[1..]);
    }
    if pattern[pattern.len() - 1] == b'*' && stars == 1 {
        return name.starts_with(&pattern[..pattern.len() - 1]);
    }
    regex_matches(pattern, name)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Byte(u8),
    One,
    Any,
}

/// `makePatternFromWildcard`: `*`/`**` become `.*`, `?` becomes `.`, `(` and
/// `)` are dropped, everything else is literal. Java's `.` (no flags) does
/// not match line terminators; in Latin-1 those are `\n`, `\r` and U+0085.
fn regex_matches(pattern: &[u8], name: &[u8]) -> bool {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            b'*' => {
                if pattern.get(index + 1) == Some(&b'*') {
                    index += 1;
                }
                tokens.push(Token::Any);
            }
            b'?' => tokens.push(Token::One),
            b'(' | b')' => {}
            byte => tokens.push(Token::Byte(byte)),
        }
        index += 1;
    }
    let wild = |byte: u8| !matches!(byte, b'\n' | b'\r' | 0x85);
    // reachable[k]: tokens[..k] can match the consumed prefix of `name`.
    let mut reachable = vec![false; tokens.len() + 1];
    reachable[0] = true;
    let close = |reachable: &mut Vec<bool>| {
        for k in 0..tokens.len() {
            if reachable[k] && tokens[k] == Token::Any {
                reachable[k + 1] = true;
            }
        }
    };
    close(&mut reachable);
    for &byte in name {
        let mut next = vec![false; tokens.len() + 1];
        for (k, token) in tokens.iter().enumerate() {
            if !reachable[k] {
                continue;
            }
            match *token {
                Token::Byte(expected) if expected == byte => next[k + 1] = true,
                Token::One if wild(byte) => next[k + 1] = true,
                Token::Any if wild(byte) => next[k] = true,
                _ => {}
            }
        }
        close(&mut next);
        reachable = next;
    }
    reachable[tokens.len()]
}

/// `PathFragment.create` on a Unix path policy: collapse repeated and
/// trailing slashes, drop `.` segments and resolve `..` against a preceding
/// non-`..` segment (at the root of an absolute path, `..` is dropped).
pub fn normalize_path_fragment(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let absolute = path.starts_with('/');
    let mut kept: Vec<&str> = Vec::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            ".." if kept.last().is_some_and(|last| *last != "..") => {
                kept.pop();
            }
            ".." if absolute => {}
            _ => kept.push(segment),
        }
    }
    let joined = kept.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// `CharStreams.readLines`: `\n`, `\r\n` and `\r` terminate lines; a final
/// unterminated line is kept, a trailing terminator yields no empty line.
fn read_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let (mut start, mut index) = (0, 0);
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                start = index + 1;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The Latin-1 internal string for a Unicode `.bazelignore` path. Bazel
/// passes the UTF-8-decoded line to `PathFragment.create` unconverted, but
/// compares it with internal (one char per byte) directory paths. A char
/// above U+00FF therefore never matches; otherwise the chars are bytes, and
/// only valid UTF-8 byte strings can equal (a segment-aligned prefix of) a
/// UTF-8 directory path here.
fn latin1_internal(text: &str) -> Option<String> {
    let bytes = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// `IgnoredSubdirectoriesFunction.getIgnoredPrefixes` over the bytes of
/// `path` (displayed as `display`).
pub fn parse_bazelignore(
    display: &str,
    bytes: &[u8],
) -> Result<Vec<(String, Option<String>)>, String> {
    // InputStreamReader(UTF_8) replaces malformed input with U+FFFD.
    let text = String::from_utf8_lossy(bytes);
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = HashSet::new();
    for line in read_lines(&text) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains('\0') {
            // Path.of(line) -> InvalidPathException(input, reason).getMessage().
            return Err(format!(
                "Invalid path in {display}: Nul character not allowed: {line}"
            ));
        }
        let fragment = normalize_path_fragment(line);
        if seen.insert(fragment.clone()) {
            let internal = latin1_internal(&fragment);
            entries.push((fragment, internal));
        }
    }
    // Lines are all parsed (and NUL-checked) before the absolute-path check.
    if let Some((fragment, _)) = entries.iter().find(|(text, _)| text.starts_with('/')) {
        return Err(format!(
            "Invalid path in {display}: '{fragment}': cannot be an absolute path"
        ));
    }
    Ok(entries)
}

/// `IgnoredSubdirectoriesFunction.compute` for `repository`: REPO.bazel
/// `ignore_directories()` patterns, then `.bazelignore` prefixes at the
/// repository root (main and external repositories alike).
pub fn for_repository(
    repositories: &Arc<crate::repository::RepositorySet>,
    repository: &str,
) -> Result<IgnoredSubdirectories, String> {
    let patterns = crate::package::repo_ignored_directories(repositories, repository)?;
    let root = repositories.root(repository)?;
    let mut result = IgnoredSubdirectories {
        patterns,
        ..IgnoredSubdirectories::default()
    };
    for (text, internal) in read_bazelignore(root)? {
        result.push_prefix(Prefix { text, internal });
    }
    Ok(result)
}

/// Read `<root>/.bazelignore` when it is a regular file (`FileValue.isFile`,
/// following symlinks). Like REPO.bazel and BUILD files, it must resolve
/// inside the repository.
fn read_bazelignore(root: &Path) -> Result<Vec<(String, Option<String>)>, String> {
    let path = root.join(".bazelignore");
    match observed_fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(not_readable(&path, &error)),
    }
    let canonical_root = observed_fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve repository root {}: {e}", root.display()))?;
    let resolved = observed_fs::canonicalize(&path).map_err(|error| not_readable(&path, &error))?;
    if !resolved.starts_with(&canonical_root) {
        return Err(format!(
            ".bazelignore file {} resolves outside workspace {}",
            path.display(),
            canonical_root.display()
        ));
    }
    let bytes = observed_fs::read(&path).map_err(|error| not_readable(&path, &error))?;
    parse_bazelignore(&path.display().to_string(), &bytes)
}

fn not_readable(path: &Path, error: &io::Error) -> String {
    format!(
        "{} is not readable because: error '{error}'. Was it modified mid-build?",
        path.display()
    )
}

/// `PackageLookupValue.DELETED_PACKAGE_VALUE.getErrorMsg()`.
pub const DELETED_PACKAGE_MESSAGE: &str = "Package is considered deleted due to --deleted_packages";

/// `--deleted_packages` (pkgcache/PackageOptions.java, PackageLookupFunction
/// .java:98-99): packages Bazel treats as nonexistent. Their BUILD files are
/// no package boundary (globs of the parent package cross into them,
/// recursive patterns skip them but still descend), and loading one fails
/// with [`DELETED_PACKAGE_MESSAGE`]. Set once per command, before loading.
#[derive(Debug, Default)]
struct DeletedPackages {
    /// (canonical repository name as given, package path).
    ids: HashSet<(String, String)>,
    /// The main repository's deleted package directories (below the
    /// canonical workspace root): the filesystem-level boundary checks.
    main_directories: HashSet<std::path::PathBuf>,
}

static DELETED: std::sync::RwLock<Option<DeletedPackages>> = std::sync::RwLock::new(None);

/// `CommaSeparatedPackageNameListConverter` (PackageOptions.java:219-244):
/// each element is `PackageIdentifier.parse` (cmdline/PackageIdentifier.java
/// :145-153), so `pkg`, `//pkg` and `@repo//pkg` (the repository name taken
/// as is) name a package. Returns (repository, package) pairs.
pub fn parse_deleted_packages(input: &str) -> Result<Vec<(String, String)>, String> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for element in input.split(',') {
        if element.contains(':') {
            return Err(format!(
                "invalid package identifier '{element}': contains ':'"
            ));
        }
        // LabelParser.Parts.parse of `element + ":dummy_target"`.
        let (repository, package) = if let Some(rest) = element.strip_prefix('@') {
            let rest = rest.strip_prefix('@').unwrap_or(rest);
            match rest.split_once("//") {
                Some((repository, package)) => (repository, package),
                None => (rest, ""),
            }
        } else {
            ("", element.strip_prefix("//").unwrap_or(element))
        };
        let package = package
            .strip_suffix("/...")
            .or_else(|| package.strip_suffix("..."))
            .unwrap_or(package);
        if let Some(error) = crate::bazel_label::validate_package_name(package) {
            return Err(format!("invalid package name '{package}': {error}"));
        }
        found.push((repository.to_owned(), package.to_owned()));
    }
    Ok(found)
}

/// Install this command's `--deleted_packages` (`main_root`: the workspace).
pub fn set_deleted_packages(main_root: &Path, packages: &[(String, String)]) {
    let canonical =
        observed_fs::canonicalize(main_root).unwrap_or_else(|_| main_root.to_path_buf());
    let roots = [canonical, main_root.to_path_buf()];
    let deleted = DeletedPackages {
        ids: packages.iter().cloned().collect(),
        main_directories: packages
            .iter()
            .filter(|(repository, _)| repository.is_empty())
            .flat_map(|(_, package)| {
                roots.iter().map(move |root| {
                    let mut directory = root.clone();
                    directory.extend(package.split('/').filter(|part| !part.is_empty()));
                    directory
                })
            })
            .collect(),
    };
    *DELETED.write().unwrap_or_else(|e| e.into_inner()) =
        (!deleted.ids.is_empty()).then_some(deleted);
}

/// Whether `package` of `repository` is a `--deleted_packages` package.
pub fn is_deleted_package(repository: &str, package: &str) -> bool {
    DELETED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|deleted| {
            deleted
                .ids
                .contains(&(repository.to_owned(), package.to_owned()))
        })
}

/// Whether `directory` (below the canonical main workspace root) is a
/// deleted main-repository package: its BUILD file is no boundary.
pub fn is_deleted_directory(directory: &Path) -> bool {
    DELETED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|deleted| deleted.main_directories.contains(directory))
}

/// `NoSuchPackageException.getMessage` for a package inside an ignored
/// directory (`PackageLookupFunction` returns `DELETED_PACKAGE_VALUE`).
pub fn deleted_package_error(repository: &str, package: &str) -> String {
    // PackageIdentifier.toString: the path for main, else canonical form.
    let id = if repository.is_empty() {
        package.to_owned()
    } else {
        format!("@@{repository}//{package}")
    };
    format!("no such package '{id}': {DELETED_PACKAGE_MESSAGE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(prefixes: &[&str], patterns: &[&str]) -> IgnoredSubdirectories {
        IgnoredSubdirectories::new(
            prefixes.iter().map(|s| s.to_string()).collect(),
            patterns.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn segment_matching_follows_unix_glob_matches() {
        for (pattern, name, expected) in [
            ("*", ".hidden", true),
            ("**", ".hidden", true),
            ("", "a", false),
            ("a", "", false),
            ("*.js", "x.js", true),
            ("*.js", ".x.js", false),
            (".*", ".venv", true),
            ("node_*", "node_modules", true),
            ("node_modules", "node_modules", true),
            ("node_modules", "node_module", false),
            ("a*b", "axxb", true),
            ("a*b", "axxbc", false),
            ("a**b", "ab", true),
            ("?b", "ab", true),
            ("?b", ".b", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            // Fast paths compare the remainder literally.
            ("*?", "x?", true),
            ("*?", "xy", false),
            ("?*", "?y", true),
            ("?*", "xy", false),
            // Regex path drops parentheses and escapes the rest.
            ("(a)*b", "axb", true),
            ("(a)*b", "(a)xb", false),
            ("a.*c", "a.xc", true),
            ("a.*c", "abxc", false),
            ("[a]*", "[a]x", true),
            ("a*c?", "abcd", true),
            // Java's '.' never matches \n, \r or U+0085 (byte 0x85 here).
            ("a*b?", "a\nbc", false),
            ("a?x*y", "a\rxzy", false),
            ("a?x*y", "abxzy", true),
            ("a*Åb*", "aÅbc", true),
            ("x*b?", "xÅbc", false),
            // '?' matches one byte, not one Unicode character.
            ("a?b?", "aéb!", false),
            ("a??b?", "aéb!", true),
        ] {
            assert_eq!(
                segment_matches(pattern, name),
                expected,
                "{pattern:?} {name:?}"
            );
        }
    }

    #[test]
    fn path_matching_modes_follow_unix_glob_matches_pattern() {
        for (pattern, path, exact, prefix, child) in [
            ("**/node_modules", "node_modules", true, true, true),
            ("**/node_modules", "a/b/node_modules", true, true, true),
            ("**/node_modules", "a/node_modules/x", false, true, true),
            ("**/node_modules", "a/b", false, false, true),
            ("**/node_modules", "", false, false, true),
            ("a/*/b", "a/x/b", true, true, true),
            // A finished pattern cannot match a deeper child (CAN_MATCH_CHILD
            // only succeeds when the path runs out first).
            ("a/*/b", "a/x/b/c", false, true, false),
            ("a/*/b", "a/x", false, false, true),
            ("a/*/b", "a", false, false, true),
            ("a/*/b", "c", false, false, false),
            ("a/*/b", "a/x/c", false, false, false),
            // A bare '*' matches hidden names; compound patterns do not.
            ("a/*/b", "a/.x/b", true, true, true),
            ("a/x*/b", "a/.xy/b", false, false, false),
            ("**", "", true, true, true),
            ("**", "x/y", true, true, true),
            ("x/**", "x", true, true, true),
            // Empty segments never match: a trailing slash disables a pattern.
            ("node_modules/", "node_modules", false, false, true),
            ("node_modules/", "node_modules/x", false, false, false),
            ("/abs", "abs", false, false, false),
            ("", "", false, false, true),
            (
                "**/**/**/**/**/**/**/**/**/**/**/**/**/**/**/**/x",
                "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s",
                false,
                false,
                true,
            ),
        ] {
            assert_eq!(matches(pattern, path), exact, "exact {pattern} {path}");
            assert_eq!(
                matches_prefix(pattern, path),
                prefix,
                "prefix {pattern} {path}"
            );
            assert_eq!(
                can_match_child(pattern, path),
                child,
                "child {pattern} {path}"
            );
        }
    }

    #[test]
    fn matching_entry_checks_prefixes_then_patterns_in_order() {
        let ignored = set(
            &["src/ts/node_modules", "pkg", "src/ts/node_modules", ""],
            &["**/node_modules", "**/.venv"],
        );
        // Duplicate prefixes collapse (ImmutableSet) in first-seen order.
        assert_eq!(
            ignored.prefixes().collect::<Vec<_>>(),
            ["src/ts/node_modules", "pkg", ""]
        );
        let ignored = set(
            &["src/ts/node_modules", "pkg", "src/ts/node_modules"],
            &["**/node_modules", "**/.venv"],
        );
        for (path, expected) in [
            ("pkg", Some("pkg")),
            ("pkg/sub", Some("pkg")),
            ("pkgs", None),
            ("pk", None),
            ("", None),
            ("src/ts/node_modules/x", Some("src/ts/node_modules")),
            ("src/node_modules", Some("**/node_modules")),
            ("node_modules/deep/er", Some("**/node_modules")),
            ("src/python/.venv/lib", Some("**/.venv")),
            ("src/python/venv", None),
            ("src/ts", None),
        ] {
            assert_eq!(ignored.matching_entry(path), expected, "{path}");
        }
        // The empty prefix (".bazelignore" line ".") ignores everything,
        // including the root package; so does the pattern "**".
        assert_eq!(set(&[""], &[]).matching_entry(""), Some(""));
        assert_eq!(set(&[], &["**"]).matching_entry(""), Some("**"));
        assert!(set(&[], &[]).is_empty());
    }

    #[test]
    fn filter_for_directory_keeps_entries_that_can_match_below() {
        let ignored = set(
            &["src/ts/node_modules", "pkg", "src/tsx"],
            &["**/node_modules", "a/*/b", "src/*/x", "other/x"],
        );
        let filtered = ignored.filter_for_directory("src/ts");
        assert_eq!(
            filtered.prefixes().collect::<Vec<_>>(),
            ["src/ts/node_modules"]
        );
        assert_eq!(filtered.patterns(), ["**/node_modules", "src/*/x"]);
        let root = ignored.filter_for_directory("");
        assert_eq!(root, ignored);
        // Filtering never changes the answer for paths below the directory.
        for path in [
            "src/ts/node_modules",
            "src/ts/a/node_modules/b",
            "src/ts/x",
            "src/ts/y",
        ] {
            assert_eq!(
                filtered.matching_entry(path).is_some(),
                ignored.matching_entry(path).is_some(),
                "{path}"
            );
        }
    }

    #[test]
    fn path_fragment_normalization() {
        for (input, expected) in [
            ("", ""),
            (".", ""),
            ("./", ""),
            ("a/", "a"),
            ("a//b/", "a/b"),
            ("./a/./b", "a/b"),
            ("a/../b", "b"),
            ("..", ".."),
            ("../a", "../a"),
            ("a/../..", ".."),
            ("a/b/../../..", ".."),
            ("/", "/"),
            ("//a//", "/a"),
            ("/../a", "/a"),
            (" a ", " a "),
            ("a\\b", "a\\b"),
            ("...", "..."),
        ] {
            assert_eq!(normalize_path_fragment(input), expected, "{input:?}");
        }
    }

    #[test]
    fn bazelignore_lines_follow_path_fragment_line_processor() {
        let parse = |text: &[u8]| parse_bazelignore("/ws/.bazelignore", text);
        let texts = |text: &[u8]| {
            parse(text)
                .unwrap()
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
        };
        // Only empty lines and lines starting with '#' are skipped; nothing is
        // trimmed, trailing slashes and '.'/'..' segments are normalized.
        assert_eq!(
            texts(b"# comment\n\nnode_modules/\r\n  # not a comment\r./a/./b\n a \nsrc//x/\n#\nnode_modules"),
            ["node_modules", "  # not a comment", "a/b", " a ", "src/x"]
        );
        assert_eq!(texts(b"a\n\n\n"), ["a"]);
        assert_eq!(texts(b"\n\r\n\r"), Vec::<String>::new());
        assert_eq!(texts(b"."), [""]);
        assert_eq!(texts(b"a/../.."), [".."]);
        assert_eq!(
            parse(b"ok\n/abs/dir/\n").unwrap_err(),
            "Invalid path in /ws/.bazelignore: '/abs/dir': cannot be an absolute path"
        );
        assert_eq!(
            parse(b"/\n").unwrap_err(),
            "Invalid path in /ws/.bazelignore: '/': cannot be an absolute path"
        );
        assert_eq!(
            parse(b"/abs\na\0b\n").unwrap_err(),
            "Invalid path in /ws/.bazelignore: Nul character not allowed: a\0b"
        );
        // Non-ASCII lines are Unicode in Bazel but compared to Latin-1
        // internal directory names, so they cannot match UTF-8 names.
        let entries = parse("é\nÃ©\n\u{FEFF}x\n".as_bytes()).unwrap();
        assert_eq!(entries[0], ("é".to_owned(), None));
        assert_eq!(entries[1], ("Ã©".to_owned(), Some("é".to_owned())));
        assert_eq!(entries[2], ("\u{FEFF}x".to_owned(), None));
        let entries = parse(b"\xffa\n").unwrap();
        assert_eq!(entries, [("\u{FFFD}a".to_owned(), None)]);
    }
}
