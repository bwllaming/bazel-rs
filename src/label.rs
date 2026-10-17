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
pub(crate) const GENERATED_EXEC_ROOT: &str = "bazel-out/rust-fastbuild/bin";

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
        Self::parse_in(input, "", "", &BTreeMap::new())
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

    pub fn parse_in(
        input: &str,
        repository: &str,
        current_package: &str,
        mapping: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        // An apparent repository the requesting main repository does not see
        // (RepositoryName.createUnvalidated, cmdline/RepositoryName.java):
        // kept so the failure belongs to the dependency edge at analysis.
        if let Some(rest) = input.strip_prefix("@@[unknown repo '")
            && let Some((name, absolute)) = rest.split_once("' requested from @@]")
            && absolute.starts_with("//")
        {
            let canonical = &input[2..input.len() - absolute.len()];
            if unknown_repository_name(canonical) == Some(name) {
                validate_repository_name(name)?;
                let parts = absolute_parts(absolute)?;
                return Ok(Label {
                    repository: canonical.to_owned(),
                    package: parts.pkg.to_owned(),
                    name: parts.target.to_owned(),
                });
            }
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
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.repository.is_empty() {
            write!(f, "@@{}", self.repository)?;
        }
        write!(f, "//{}:{}", self.package, self.name)
    }
}

/// The apparent name in `[unknown repo 'NAME' requested from @@]`, the
/// repository Bazel gives an apparent repository the main repository does not
/// see (cmdline/RepositoryName.java, RepositoryMapping.get).
pub fn unknown_repository_name(repository: &str) -> Option<&str> {
    repository
        .strip_prefix("[unknown repo '")?
        .strip_suffix("' requested from @@]")
        .filter(|name| !name.is_empty() && !name.contains('\''))
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
            PathBuf::from("bazel-out/rust-fastbuild/bin/external/dep+/p/generated/file.txt")
        );
        let main = Label::parse("//p:q/input.txt", "").unwrap();
        assert_eq!(main.source_exec_path(), PathBuf::from("p/q/input.txt"));
        assert_eq!(main.short_path(), main.source_exec_path());
        assert_eq!(
            main.generated_exec_path(GENERATED_EXEC_ROOT),
            PathBuf::from("bazel-out/rust-fastbuild/bin/p/q/input.txt")
        );
        assert_eq!(
            main.generated_exec_path("bazel-out/rust-opt-exec-ST-0123456789ab/bin"),
            PathBuf::from("bazel-out/rust-opt-exec-ST-0123456789ab/bin/p/q/input.txt")
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

    #[test]
    fn generated_roots_split_any_configuration_mnemonic() {
        for (path, root, rest) in [
            (
                "bazel-out/rust-fastbuild/bin/p/out",
                "bazel-out/rust-fastbuild/bin",
                "p/out",
            ),
            (
                "bazel-out/rust-opt-exec-ST-0123456789ab/bin/external/r+/x",
                "bazel-out/rust-opt-exec-ST-0123456789ab/bin",
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
            "bazel-out/rust-fastbuild/bin",
            "bazel-out/rust-fastbuild/bin2/p",
            "bazel-out//bin/p",
            "bazel-out/../bin/p",
            "bazel-out/./bin/p",
            "bazel-out/a b/bin/p",
            "bazel-out/rust-fastbuild/genfiles/p",
            "external/bazel-out/rust-fastbuild/bin/p",
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
