//! Repository-aware labels.
//!
//! The grammar, validation and error texts are Bazel 9.3's
//! (`crate::bazel_label`: `LabelParser.Parts`, `LabelValidator`,
//! `RepositoryName.validate`); `parse_in` is `Label.parseWithPackageContext`
//! (cmdline/Label.java:177-245) with the caller's repository mapping.
//! Package names may contain any printable ASCII character except `:` and `\`
//! (LabelValidator.java:50-61), target names additionally any non-ASCII
//! character (:63-76); `@repo` means `@repo//:repo` (LabelParser.java:115-126).
//! Like `Label.createUnvalidated`, a trailing `/.` of a target name is dropped
//! (LabelParser.java:227-230). `parse` remains main-repository-only.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use crate::bazel_label::Parts;

/// Generated-file root of the default configuration
/// (crate::config_registry::Configurations::output_root).
pub(crate) const GENERATED_EXEC_ROOT: &str = "bazel-out/k8-fastbuild/bin";

/// Split a generated execution path into its configuration's root,
/// `bazel-out/<mnemonic>/bin` with a single safe mnemonic component, and the
/// root-relative rest (`None` for any other path). The root is a property of
/// the configured target (crate::config_registry); every configuration's
/// root has this shape.
pub(crate) fn split_generated_root(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("bazel-out/")?;
    let (mnemonic, rest) = rest.split_once('/')?;
    if mnemonic.is_empty()
        || mnemonic == "."
        || mnemonic == ".."
        || !mnemonic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
    {
        return None;
    }
    let relative = rest.strip_prefix("bin/")?;
    let root_length = "bazel-out/".len() + mnemonic.len() + "/bin".len();
    Some((&path[..root_length], relative))
}

/// The short path of a generated (derived) file from its path below its
/// output root: Artifact.getRunfilesPath (actions/Artifact.java:636-662)
/// under the legacy, non-sibling output layout (`ArtifactRoot.isLegacy`,
/// RootType.OUTPUT), where a leading `external` segment is `..` whoever owns
/// the artifact — `external/r/x` is `../r/x` for a rule of `@r` and for a
/// main-repository rule's shareable artifact alike. The one rule of every
/// generated File: declared outputs (crate::analysis_runtime), the native
/// rules' and predeclared outputs, and the engine's Artifact File
/// (crate::engine `Artifact::compute_file`) — so a File and its Artifact
/// never differ (review F2 of d2fd11c). Source Files keep their rule: a
/// main-repository source below `external/` keeps it (source roots are not
/// legacy roots).
pub(crate) fn generated_short_path(root_relative: &str) -> String {
    match root_relative.strip_prefix("external") {
        Some("") => "..".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("..{rest}"),
        _ => root_relative.to_owned(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PackageId {
    pub repository: String,
    pub package: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Label {
    /// Canonical repository name; empty for the main repository.
    pub repository: String,
    pub package: String,
    pub name: String,
}

impl Label {
    pub fn parse(input: &str, current_package: &str) -> Result<Label, String> {
        let input = if let Some(main) = input.strip_prefix("@@//") {
            format!("//{main}")
        } else if let Some(main) = input.strip_prefix("@//") {
            format!("//{main}")
        } else if input.starts_with('@') {
            return Err(format!(
                "external repository labels are unsupported in this implementation: {input:?}; use //package:target for the main repository"
            ));
        } else {
            input.to_owned()
        };

        Self::parse_in(&input, "", current_package, &BTreeMap::new())
    }

    pub fn parse_canonical(input: &str) -> Result<Self, String> {
        if !input.starts_with("//") && !input.starts_with("@@") {
            return Err(format!(
                "expected canonical //package:target or @@repository//package:target label: {input:?}"
            ));
        }
        Self::parse_in_with(input, "", "", &BTreeMap::new(), true)
    }

    /// Allocation-free upper bound for a resolved label's formatted payload.
    /// Includes mapped repository names, relative package context, and the
    /// duplicated basename in absolute shorthand. Charge before parse_in copies.
    pub(crate) fn resolution_size_bound(
        input: &str,
        repository: &str,
        current_package: &str,
        mapping: &BTreeMap<String, String>,
    ) -> usize {
        let repository_len = if input.starts_with("@@") {
            0 // Already included in input's byte count.
        } else if let Some(apparent) = input.strip_prefix('@') {
            // `@alias` alone is `@alias//:alias`: the alias is the whole rest.
            let alias = apparent
                .split_once("//")
                .map_or(apparent, |(alias, _)| alias);
            mapping.get(alias).map_or(0, String::len)
        } else {
            repository.len()
        };
        let package_len = if input.starts_with('@') || input.starts_with("//") {
            0
        } else {
            current_package.len()
        };
        input
            .len()
            .saturating_mul(2)
            .saturating_add(repository_len)
            .saturating_add(package_len)
            .saturating_add(5)
    }

    /// Parse user-written label text. Repository names follow
    /// RepositoryName.validate (LabelParser.Parts.validateAndCreate,
    /// cmdline/LabelParser.java:72, RepositoryName.java:178-196): the
    /// bracketed [`unknown_repository`] encoding is NOT accepted here
    /// (`invalid repository name '[unknown repo ...]': repo names may contain
    /// only ...`); only [`Label::parse_canonical`] round-trips it.
    pub fn parse_in(
        input: &str,
        repository: &str,
        current_package: &str,
        mapping: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        Self::parse_in_with(input, repository, current_package, mapping, false)
    }

    fn parse_in_with(
        input: &str,
        repository: &str,
        current_package: &str,
        mapping: &BTreeMap<String, String>,
        internal: bool,
    ) -> Result<Self, String> {
        // A native-produced invalid-repository label (RepositoryName
        // toNonVisible, cmdline/RepositoryName.java:230-276) round-trips
        // through its canonical string; user text never reaches here
        // (Parts::parse rejects the bracketed name with
        // RepositoryName.validate's text).
        if internal
            && let Some(explicit) = input.strip_prefix("@@")
            && let Some((canonical, rest)) = explicit.split_once("//")
            && let Some(unresolvable) = unresolvable_repository(canonical)
        {
            validate_repository_name(unresolvable.name)?;
            let parts = absolute_parts(&input[input.len() - rest.len() - 2..])?;
            return Ok(Label {
                repository: canonical.to_owned(),
                package: parts.pkg.to_owned(),
                name: parts.target.to_owned(),
            });
        }
        let parts = Parts::parse(input)?;
        // Label.parseWithPackageContextInternal (Label.java:234-245).
        if parts.pkg_ends_with_triple_dots {
            return Err(format!(
                "invalid label '{input}': package name cannot contain '...'"
            ));
        }
        if !parts.pkg.is_empty() && !parts.pkg_is_absolute {
            return Err(format!(
                "invalid label '{input}': absolute label must begin with '@' or '//'"
            ));
        }
        // Label.computeRepoNameWithRepoContext (Label.java:177-192).
        let repository = match parts.repo {
            // ABSOLUTE_PACKAGE_NAMES (Label.java:94-99) are reserved only
            // when no explicit repository prefix is present.
            None if matches!(parts.pkg, "visibility" | "conditions") => "",
            None => repository,
            Some(canonical) if parts.repo_is_canonical => canonical,
            Some(alias) => mapping.get(alias).map(String::as_str).ok_or_else(|| {
                format!("unknown apparent repository @{alias} from canonical repository {repository:?}; extension repository not materialized or dependency not declared")
            })?,
        };
        validate_repository_name(repository)?;
        let package = if parts.pkg_is_absolute {
            parts.pkg
        } else {
            validate_package_path(current_package)?;
            current_package
        };
        Ok(Label {
            repository: repository.to_owned(),
            package: package.to_owned(),
            name: parts.target.to_owned(),
        })
    }

    pub fn package_id(&self) -> PackageId {
        PackageId {
            repository: self.repository.clone(),
            package: self.package.clone(),
        }
    }

    /// Validate a raw dependency without requiring the apparent repository to
    /// exist. Native rules retain raw labels until their dependencies are
    /// analyzed; an unrelated target may legally mention an unavailable repo.
    pub fn validate_syntax(input: &str, package: &str) -> Result<(), String> {
        let mut mapping = BTreeMap::new();
        if let Some(apparent) = input.strip_prefix('@').filter(|_| !input.starts_with("@@")) {
            // `@name` alone is `@name//:name` (LabelParser.java:115-126).
            let name = apparent.split_once("//").map_or(apparent, |(name, _)| name);
            mapping.insert(name.to_owned(), String::new());
        }
        Self::parse_in(input, "", package, &mapping).map(|_| ())
    }

    /// `Label.create(PackageIdentifier, String)` (Label.java:303-311).
    pub fn with_name(&self, name: &str) -> Result<Self, String> {
        if unknown_repository_name(&self.repository).is_none() {
            validate_repository_name(&self.repository)?;
        }
        validate_package_path(&self.package)?;
        crate::bazel_label::validate_and_process_target_name(&self.package, name)?;
        Ok(Self {
            name: name.strip_suffix("/.").unwrap_or(name).to_owned(),
            ..self.clone()
        })
    }

    pub fn canonical_starlark(&self) -> String {
        format!("@@{}//{}:{}", self.repository, self.package, self.name)
    }

    pub fn package_exec_path(&self) -> PathBuf {
        if self.repository.is_empty() {
            PathBuf::from(&self.package)
        } else {
            PathBuf::from("external")
                .join(&self.repository)
                .join(&self.package)
        }
    }

    pub fn source_exec_path(&self) -> PathBuf {
        self.package_exec_path().join(&self.name)
    }

    /// File metadata only: never use this path for execution or source lookup.
    pub fn short_path(&self) -> PathBuf {
        if self.repository.is_empty() {
            self.source_exec_path()
        } else {
            PathBuf::from("..")
                .join(&self.repository)
                .join(&self.package)
                .join(&self.name)
        }
    }

    /// The execution path of this label's generated file under `root`, the
    /// output root of the configuration it is generated in.
    pub fn generated_exec_path(&self, root: &str) -> PathBuf {
        PathBuf::from(root).join(self.source_exec_path())
    }

    /// The short path of this label's generated file
    /// ([`generated_short_path`] of its root-relative path): the label's
    /// [`Label::short_path`] except in a main-repository package below
    /// `external/`, which a derived artifact's runfiles path rebases too.
    pub fn generated_short_path(&self) -> String {
        generated_short_path(&self.source_exec_path().to_string_lossy())
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.repository.is_empty() {
            write!(f, "@@{}", self.repository)?;
        }
        write!(f, "//{}:{}", self.package, self.name)
    }
}

/// The repository name RepositoryMapping.get gives an apparent name the
/// context repository does not see (cmdline/RepositoryMapping.java:133-140):
/// RepositoryName.createUnvalidated(name).toNonVisible(context,
/// SpellChecker.didYouMean(name, mapping keys)), whose canonical form is
/// `@@[unknown repo 'NAME' requested from @@CONTEXT SUGGESTION]`
/// (cmdline/RepositoryName.java:268-276; the main repository's form is
/// `@@`). This is the value of [`Label::repository`], without the `@@`.
pub fn unknown_repository(name: &str, requester: &str, suggestion: &str) -> String {
    format!("[unknown repo '{name}' requested from @@{requester}{suggestion}]")
}

/// The parts of an [`unknown_repository`] name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnresolvableRepository<'a> {
    /// The apparent name as written (may be empty for `@//` outside the main
    /// repository).
    pub name: &'a str,
    /// The canonical context repository (empty: the main repository).
    pub requester: &'a str,
    /// `""` or ` (did you mean 'x'?)`.
    pub suggestion: &'a str,
}

impl UnresolvableRepository<'_> {
    /// RepositoryName.getContextRepoDisplayString (RepositoryName.java:250-258).
    pub fn context_display(&self) -> String {
        context_display(self.requester)
    }

    /// RepositoryFetchFunction.compute for a non-visible name
    /// (bazel/repository/RepositoryFetchFunction.java:149-153).
    pub fn not_visible(&self) -> String {
        format!(
            "No repository visible as '@{}' from {}",
            self.name,
            self.context_display()
        )
    }
}

/// RepositoryName.getContextRepoDisplayString (RepositoryName.java:250-258):
/// `main repository` or `repository '@@C'`.
pub fn context_display(requester: &str) -> String {
    if requester.is_empty() {
        "main repository".to_owned()
    } else {
        format!("repository '@@{requester}'")
    }
}

/// Parse an [`unknown_repository`] name.
pub fn unresolvable_repository(repository: &str) -> Option<UnresolvableRepository<'_>> {
    let rest = repository
        .strip_prefix("[unknown repo '")?
        .strip_suffix(']')?;
    let (name, rest) = rest.split_once("' requested from @@")?;
    if name.contains(['\'', '/']) {
        return None;
    }
    // Canonical repository names contain no space; the suggestion starts with one.
    let (requester, suggestion) = match rest.find(' ') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if !(suggestion.is_empty()
        || suggestion
            .strip_prefix(" (did you mean '")
            .and_then(|s| s.strip_suffix("'?)"))
            .is_some_and(|word| !word.contains(['\'', '/'])))
        || requester.contains(['/', '\'', '[', ']'])
    {
        return None;
    }
    Some(UnresolvableRepository {
        name,
        requester,
        suggestion,
    })
}

/// RepositoryName.getName(), the repository part Label.compareTo and
/// PackageIdentifier.compareTo order by (cmdline/PackageIdentifier.java:295-307):
/// for a non-visible repository only the apparent name, not the context
/// repository or suggestion of its canonical form.
pub fn repository_sort_name(repository: &str) -> &str {
    unresolvable_repository(repository).map_or(repository, |parts| parts.name)
}

/// The apparent name of an [`unknown_repository`] name, for any context
/// repository.
pub fn unknown_repository_name(repository: &str) -> Option<&str> {
    unresolvable_repository(repository).map(|parts| parts.name)
}

/// The BuildFileNotFoundException PackageLookupFunction reports for a
/// package in a non-visible repository: `no such package '@@R//PKG': The
/// repository '@@R' could not be resolved: No repository visible as '@N'
/// from CONTEXT` (skyframe/PackageLookupFunction.java, RepositoryFetchFunction.java:149-153).
pub fn unresolvable_package_message(package: &PackageId) -> Option<String> {
    let parts = unresolvable_repository(&package.repository)?;
    Some(format!(
        "no such package '@@{repository}//{}': The repository '@@{repository}' could not be resolved: {}",
        package.package,
        parts.not_visible(),
        repository = package.repository,
    ))
}

/// Label.parseWithPackageContext of `@NAME//pkg:target` text whose apparent
/// repository the context repository does not see (Label.java:177-207):
/// the label keeps an [`unknown_repository`]. `None` for any other text.
pub fn deferred_label(text: &str, requester: &str, suggestion: &str) -> Option<Label> {
    let parts = crate::bazel_label::Parts::parse(text).ok()?;
    let name = parts.repo.filter(|_| !parts.repo_is_canonical)?;
    if !parts.pkg_is_absolute || parts.pkg_ends_with_triple_dots {
        return None;
    }
    Some(Label {
        repository: unknown_repository(name, requester, suggestion),
        package: parts.pkg.to_owned(),
        name: parts.target.to_owned(),
    })
}

/// The parts of `//pkg[:name]`, which must be a complete absolute label.
fn absolute_parts(absolute: &str) -> Result<Parts<'_>, String> {
    let parts = Parts::parse(absolute)?;
    if parts.pkg_ends_with_triple_dots {
        return Err(format!(
            "invalid label '{absolute}': package name cannot contain '...'"
        ));
    }
    Ok(parts)
}

/// `RepositoryName.validate` (cmdline/RepositoryName.java:178-196), which also
/// rules out `/`: a canonical repository name is a single path component.
pub(crate) fn validate_repository_name(repository: &str) -> Result<(), String> {
    crate::bazel_label::validate_repo_name(repository)
}

/// `LabelValidator.validatePackageName` (cmdline/LabelValidator.java:95-139)
/// with `LabelParser`'s `invalid package name '...'` prefix.
pub(crate) fn validate_package_path(package: &str) -> Result<(), String> {
    match crate::bazel_label::validate_package_name(package) {
        None => Ok(()),
        Some(error) => Err(crate::bazel_label::sanitize(format!(
            "invalid package name '{package}': {error}"
        ))),
    }
}

/// Whether `package` is a valid package name (PackageLookupFunction.java:91-96
/// answers `invalidPackageName`, which is no package, otherwise).
pub(crate) fn is_valid_package_name(package: &str) -> bool {
    crate::bazel_label::validate_package_name(package).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_reservation_includes_mapping_and_relative_package_expansion() {
        let owner = "owner".repeat(1000);
        let package = "package".repeat(1000);
        let mapping = BTreeMap::from([("short".to_owned(), "mapped".repeat(1000))]);
        for text in [
            "@short//:x",
            "//a/b",
            ":x",
            "x",
            "@@explicit//a:b",
            "@@//:main",
            "//visibility:public",
        ] {
            let actual = Label::parse_in(text, &owner, &package, &mapping).unwrap();
            assert!(
                Label::resolution_size_bound(text, &owner, &package, &mapping)
                    >= actual.to_string().len()
            );
        }
        assert!(
            Label::resolution_size_bound("@short//:x", "", "", &mapping) >= mapping["short"].len()
        );
    }

    #[test]
    fn absolute_and_main_repository_aliases() {
        for input in ["//foo:bar", "@//foo:bar", "@@//foo:bar"] {
            let label = Label::parse(input, "elsewhere").unwrap();
            assert_eq!(label.package, "foo");
            assert_eq!(label.name, "bar");
            assert_eq!(label.to_string(), "//foo:bar");
        }
        assert_eq!(Label::parse("//:root", "p").unwrap().to_string(), "//:root");
        assert_eq!(Label::parse("@@//:root", "").unwrap().package, "");
    }

    #[test]
    fn shorthand_uses_last_package_component() {
        for input in ["//foo/bar", "@//foo/bar", "@@//foo/bar"] {
            assert_eq!(
                Label::parse(input, "").unwrap().to_string(),
                "//foo/bar:bar"
            );
        }
        assert_eq!(Label::parse("//foo", "").unwrap().to_string(), "//foo:foo");
    }

    #[test]
    fn relative_labels_preserve_package_and_target_paths() {
        for input in ["bar", ":bar"] {
            assert_eq!(Label::parse(input, "foo").unwrap().to_string(), "//foo:bar");
        }
        for input in ["src/file.rs", ":src/file.rs"] {
            assert_eq!(
                Label::parse(input, "foo").unwrap().to_string(),
                "//foo:src/file.rs"
            );
        }
        assert_eq!(Label::parse("target", "").unwrap().to_string(), "//:target");
        assert!(Label::parse("//pkg-1.2/a_b+c:x-y.z", "").is_ok());
    }

    #[test]
    fn rejects_external_repositories_explicitly() {
        for input in [
            "@repo//foo:bar",
            "@@repo+//foo:bar",
            "@repo",
            "@@repo",
            "@",
            "@@",
        ] {
            assert!(
                Label::parse(input, "")
                    .unwrap_err()
                    .contains("external repository")
            );
        }
    }

    #[test]
    fn rejects_invalid_labels_with_bazel_texts() {
        for input in [
            "",
            ":",
            "//",
            "//:",
            "//foo:",
            "///foo:bar",
            "//foo//bar:x",
            "//foo/:x",
            "//foo/../bar:x",
            "//./foo:x",
            "//foo:../x",
            "//foo:/x",
            "//foo:x/",
            "//foo:x//y",
            "//foo:x/./y",
            "../x",
            "/x",
            "foo:bar",
            "//foo:x:y",
            "//foo:x\\y",
            "//foo:x\0y",
            "//foo:x\ny",
            "//café:x",
            "//foo/...",
            "//foo/...:all",
        ] {
            assert!(Label::parse(input, "").is_err(), "accepted {input:?}");
        }
        for (input, error) in [
            (
                "//foo:x:y",
                "invalid target name 'x:y': target names may not contain ':'",
            ),
            (
                "//foo:x\ny",
                "invalid target name 'x<?>y': target names may not contain non-printable characters: '\\x0A'",
            ),
            (
                "//café:x",
                "invalid package name 'café': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
            ),
            (
                "foo:bar",
                "invalid label 'foo:bar': absolute label must begin with '@' or '//'",
            ),
            (
                "//foo/...",
                "invalid label '//foo/...': package name cannot contain '...'",
            ),
            (
                "//a/b/",
                "invalid package name 'a/b/': package names may not end with '/' (perhaps you meant \":\"?)",
            ),
        ] {
            assert_eq!(Label::parse(input, "").unwrap_err(), error, "{input:?}");
        }
    }

    #[test]
    fn accepts_the_full_bazel_label_alphabet() {
        // LabelValidator.java:39-76: printable ASCII except ':' and '\\' in
        // package names, plus any non-ASCII character in target names.
        for (input, package, name) in [
            ("//foo bar:a b", "foo bar", "a b"),
            (
                "//src/app/(beyond-work)",
                "src/app/(beyond-work)",
                "(beyond-work)",
            ),
            ("//locales/tt@iqtelif:x", "locales/tt@iqtelif", "x"),
            ("//p:a+b@c=d", "p", "a+b@c=d"),
            ("//p:café", "p", "café"),
            ("//p:x%2fy", "p", "x%2fy"),
            ("//p:x@y", "p", "x@y"),
            (
                "//p:!\"#$%&'()*+,;<=>?[]^_`{|}~",
                "p",
                "!\"#$%&'()*+,;<=>?[]^_`{|}~",
            ),
            ("//p:dir/.", "p", "dir"),
            ("//p:.", "p", "."),
            ("//p/.x/..y:z", "p/.x/..y", "z"),
        ] {
            let label = Label::parse(input, "").unwrap();
            assert_eq!(
                (label.package.as_str(), label.name.as_str()),
                (package, name),
                "{input:?}"
            );
        }
    }

    #[test]
    fn repository_only_shorthand_names_the_repository_target() {
        let mapping = BTreeMap::from([("go_shared".into(), "+oci+go_shared".into())]);
        let label = Label::parse_in("@go_shared", "rules_oci+", "oci", &mapping).unwrap();
        assert_eq!(label.to_string(), "@@+oci+go_shared//:go_shared");
        assert_eq!(
            Label::parse_canonical("@@gawk").unwrap().to_string(),
            "@@gawk//:gawk"
        );
        assert!(
            Label::parse_in("@missing", "", "", &mapping)
                .unwrap_err()
                .starts_with("unknown apparent repository @missing")
        );
        Label::validate_syntax("@anything", "p").unwrap();
        assert_eq!(
            Label::validate_syntax("@a b", "p").unwrap_err(),
            "invalid repository name 'a b': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'"
        );
        assert!(
            Label::resolution_size_bound("@go_shared", "", "", &mapping) >= label.to_string().len()
        );
    }

    #[test]
    fn relative_labels_validate_current_package() {
        for package in ["../outside", "/absolute", "a//b", "a/./b", "a\\b"] {
            assert!(Label::parse(":x", package).is_err());
        }
        // Absolute labels do not depend on the caller's package context.
        assert!(Label::parse("//valid:x", "../unused").is_ok());
    }

    #[test]
    fn parsed_labels_round_trip() {
        for input in ["//:root", "//a/b", "//a:b/c.txt", "@//a:b", "relative"] {
            let label = Label::parse(input, "context").unwrap();
            assert_eq!(Label::parse(&label.to_string(), "ignored").unwrap(), label);
        }
    }

    #[test]
    fn unresolvable_repositories_round_trip_with_context_and_suggestion() {
        let name = unknown_repository("x_armv6", "dep++ext+hub", " (did you mean 'x_amd64'?)");
        assert_eq!(
            name,
            "[unknown repo 'x_armv6' requested from @@dep++ext+hub (did you mean 'x_amd64'?)]"
        );
        let parts = unresolvable_repository(&name).unwrap();
        assert_eq!(
            (parts.name, parts.requester, parts.suggestion),
            ("x_armv6", "dep++ext+hub", " (did you mean 'x_amd64'?)")
        );
        assert_eq!(
            parts.not_visible(),
            "No repository visible as '@x_armv6' from repository '@@dep++ext+hub'"
        );
        let main = unresolvable_repository("[unknown repo 'nope' requested from @@]").unwrap();
        assert_eq!(
            main.not_visible(),
            "No repository visible as '@nope' from main repository"
        );
        let empty = unresolvable_repository("[unknown repo '' requested from @@dep+]").unwrap();
        assert_eq!(empty.name, "");
        for invalid in [
            "dep+",
            "[unknown repo 'a' requested from @@x y]",
            "[unknown repo 'a/b' requested from @@]",
            "[unknown repo 'a' requested from @@c/d]",
        ] {
            assert!(unresolvable_repository(invalid).is_none(), "{invalid}");
        }
        let label = deferred_label("@x//p:t", "m+", " (did you mean 'y'?)").unwrap();
        let text = label.to_string();
        assert_eq!(
            text,
            "@@[unknown repo 'x' requested from @@m+ (did you mean 'y'?)]//p:t"
        );
        assert_eq!(Label::parse_canonical(&text).unwrap(), label);
        assert_eq!(
            unresolvable_package_message(&label.package_id()).unwrap(),
            "no such package '@@[unknown repo 'x' requested from @@m+ (did you mean 'y'?)]//p': The repository '@@[unknown repo 'x' requested from @@m+ (did you mean 'y'?)]' could not be resolved: No repository visible as '@x' from repository '@@m+'"
        );
        assert!(deferred_label("@@x//p:t", "", "").is_none());
        assert!(deferred_label("@x//p:../t", "", "").is_none());
    }

    #[test]
    fn bracketed_repository_names_round_trip_only_internally() {
        let text = "@@[unknown repo 'nope' requested from @@m+ (did you mean 'mope'?)]//p:t";
        let label = Label::parse_canonical(text).unwrap();
        assert_eq!(label.to_string(), text);
        let error = Label::parse_in(text, "", "", &BTreeMap::new()).unwrap_err();
        assert_eq!(
            error,
            "invalid repository name '[unknown repo 'nope' requested from @@m+ (did you mean 'mope'?)]': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'"
        );
        assert!(
            Label::parse_in("@[x]//p:t", "", "", &BTreeMap::new())
                .unwrap_err()
                .starts_with("invalid repository name '[x]': repo names may contain only")
        );
        assert_eq!(
            repository_sort_name("[unknown repo 'nope' requested from @@m+]"),
            "nope"
        );
        assert_eq!(repository_sort_name("dep+"), "dep+");
    }

    #[test]
    fn repository_context_mapping_and_canonical_round_trip() {
        let mapping = BTreeMap::from([("alias".into(), "dep+1.2".into())]);
        for input in [":x", "x", "//p:x", "@@self+//p:x"] {
            assert_eq!(
                Label::parse_in(input, "self+", "p", &mapping)
                    .unwrap()
                    .to_string(),
                "@@self+//p:x"
            );
        }
        let label = Label::parse_in("@alias//p:x", "self+", "p", &mapping).unwrap();
        assert_eq!(label.repository, "dep+1.2");
        assert_eq!(Label::parse_canonical(&label.to_string()).unwrap(), label);
        assert!(Label::parse_in("@dep+1.2//p:x", "self+", "p", &mapping).is_err());
        assert!(Label::parse_in("@//p:x", "self+", "p", &mapping).is_err());
        assert_eq!(
            Label::parse_in("@@//p:x", "self+", "p", &mapping)
                .unwrap()
                .repository,
            ""
        );
        assert_eq!(
            Label::parse("//p:x", "").unwrap().canonical_starlark(),
            "@@//p:x"
        );
        for input in ["@repo//p:x", ":x", "x", "@repo"] {
            assert!(Label::parse_canonical(input).is_err());
        }
    }

    #[test]
    fn reserved_packages_only_escape_unqualified_repository_context() {
        let mapping = BTreeMap::from([("self_alias".into(), "self+".into())]);
        for package in ["visibility", "conditions"] {
            assert_eq!(
                Label::parse_in(&format!("//{package}:x"), "self+", "p", &mapping)
                    .unwrap()
                    .repository,
                ""
            );
            for input in [
                format!("@self_alias//{package}:x"),
                format!("@@self+//{package}:x"),
            ] {
                assert_eq!(
                    Label::parse_in(&input, "self+", "p", &mapping)
                        .unwrap()
                        .repository,
                    "self+"
                );
            }
            assert_eq!(
                Label::parse_in(":x", "self+", package, &mapping)
                    .unwrap()
                    .repository,
                "self+"
            );
        }
    }

    #[test]
    fn repository_paths_and_output_names_preserve_identity() {
        let label = Label::parse_canonical("@@dep+//p:q/input.txt").unwrap();
        assert_eq!(
            label.source_exec_path(),
            PathBuf::from("external/dep+/p/q/input.txt")
        );
        assert_eq!(label.short_path(), PathBuf::from("../dep+/p/q/input.txt"));
        assert_eq!(label.package_exec_path(), PathBuf::from("external/dep+/p"));
        let output = label.with_name("generated/file.txt").unwrap();
        assert_eq!(output.package_id(), label.package_id());
        assert_eq!(
            output.generated_exec_path(GENERATED_EXEC_ROOT),
            PathBuf::from("bazel-out/k8-fastbuild/bin/external/dep+/p/generated/file.txt")
        );
        let main = Label::parse("//p:q/input.txt", "").unwrap();
        assert_eq!(main.source_exec_path(), PathBuf::from("p/q/input.txt"));
        assert_eq!(main.short_path(), main.source_exec_path());
        assert_eq!(
            main.generated_exec_path(GENERATED_EXEC_ROOT),
            PathBuf::from("bazel-out/k8-fastbuild/bin/p/q/input.txt")
        );
        assert_eq!(
            main.generated_exec_path("bazel-out/k8-opt-exec-ST-0123456789ab/bin"),
            PathBuf::from("bazel-out/k8-opt-exec-ST-0123456789ab/bin/p/q/input.txt")
        );
        for name in ["../x", "/x", "//other:x", "@@other//p:x", "a//b"] {
            assert!(label.with_name(name).is_err());
        }
        assert_eq!(
            label.with_name("a:b").unwrap_err(),
            "invalid target name 'a:b': target names may not contain ':'"
        );
        assert_eq!(label.with_name("d/.").unwrap().name, "d");
    }

    /// Artifact.getRunfilesPath of a derived artifact under the legacy
    /// layout: a leading `external` segment is `..`, whoever owns it.
    #[test]
    fn generated_short_paths_rebase_external_whoever_owns_them() {
        for (relative, expected) in [
            ("p/x.txt", "p/x.txt"),
            ("external/r+/p/x.txt", "../r+/p/x.txt"),
            ("external/somerepo/x.txt", "../somerepo/x.txt"),
            ("external", ".."),
            ("externalx/y", "externalx/y"),
            ("p/external/x", "p/external/x"),
        ] {
            assert_eq!(generated_short_path(relative), expected, "{relative}");
        }
        let external = Label::parse_canonical("@@dep+//p:q/out.txt").unwrap();
        assert_eq!(external.generated_short_path(), "../dep+/p/q/out.txt");
        assert_eq!(
            PathBuf::from(external.generated_short_path()),
            external.short_path()
        );
        let main = Label::parse("//p:out.txt", "").unwrap();
        assert_eq!(main.generated_short_path(), "p/out.txt");
        // A main-repository package below external/: its sources keep the
        // root-relative path, its generated files are rebased.
        let below = Label::parse("//external/dep:out.txt", "").unwrap();
        assert_eq!(below.short_path(), PathBuf::from("external/dep/out.txt"));
        assert_eq!(below.generated_short_path(), "../dep/out.txt");
    }

    #[test]
    fn generated_roots_split_any_configuration_mnemonic() {
        for (path, root, rest) in [
            (
                "bazel-out/k8-fastbuild/bin/p/out",
                "bazel-out/k8-fastbuild/bin",
                "p/out",
            ),
            (
                "bazel-out/k8-opt-exec-ST-0123456789ab/bin/external/r+/x",
                "bazel-out/k8-opt-exec-ST-0123456789ab/bin",
                "external/r+/x",
            ),
            (
                "bazel-out/k8-fastbuild/bin/out",
                "bazel-out/k8-fastbuild/bin",
                "out",
            ),
        ] {
            assert_eq!(split_generated_root(path), Some((root, rest)), "{path}");
        }
        for path in [
            "p/out",
            "bazel-out/k8-fastbuild/bin",
            "bazel-out/k8-fastbuild/bin2/p",
            "bazel-out//bin/p",
            "bazel-out/../bin/p",
            "bazel-out/./bin/p",
            "bazel-out/a b/bin/p",
            "bazel-out/k8-fastbuild/genfiles/p",
            "external/bazel-out/k8-fastbuild/bin/p",
        ] {
            assert_eq!(split_generated_root(path), None, "{path}");
        }
    }

    #[test]
    fn unsafe_repository_names_are_rejected() {
        for repository in [".", "..", "a/b", "a\\b", "a\0b", "a b", "a~b", "@x", "café"] {
            assert!(Label::parse_canonical(&format!("@@{repository}//p:x")).is_err());
        }
        assert_eq!(
            Label::parse_canonical("@@..//p:x").unwrap_err(),
            "invalid repository name '..': repo names are not allowed to be '..'"
        );
        assert!(Label::parse_canonical("@@a_B-1.2+//p:x").is_ok());
    }
}
