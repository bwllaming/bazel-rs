//! Repository-aware labels for the intentionally bounded native implementation.
//!
//! This is a stricter subset of Bazel's label grammar: path components contain
//! only ASCII letters, digits, `_`, `-`, `.`, and `+`. Empty, `.` and `..`
//! components are rejected instead of normalized. Relative names (including
//! `dir/file`) stay in the current package. `parse` remains main-repository-only.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PackageId {
    pub repository: String,
    pub package: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
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
            apparent
                .split_once("//")
                .and_then(|(alias, _)| mapping.get(alias))
                .map_or(0, String::len)
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
        let unqualified_absolute = input.starts_with("//");
        let (repository, input) = if let Some(explicit) = input.strip_prefix("@@") {
            let (canonical, rest) = explicit.split_once("//").ok_or_else(|| {
                format!("repository-only label shorthand is unsupported: {input:?}; use @@repository//:target")
            })?;
            (canonical, &input[input.len() - rest.len() - 2..])
        } else if let Some(apparent) = input.strip_prefix('@') {
            let (alias, rest) = apparent.split_once("//").ok_or_else(|| {
                format!("repository-only label shorthand is unsupported: {input:?}; use @repository//:target")
            })?;
            validate_repository_name(alias)?;
            let canonical = mapping.get(alias).ok_or_else(|| {
                format!("unknown apparent repository @{alias} from canonical repository {repository:?}; extension repository not materialized or dependency not declared")
            })?;
            (canonical.as_str(), &input[input.len() - rest.len() - 2..])
        } else {
            (repository, input)
        };
        validate_repository_name(repository)?;
        let (package, name) = if let Some(absolute) = input.strip_prefix("//") {
            if let Some((package, name)) = absolute.split_once(':') {
                (package, name)
            } else {
                let name = absolute.rsplit('/').next().unwrap_or("");
                (absolute, name)
            }
        } else {
            (current_package, input.strip_prefix(':').unwrap_or(input))
        };
        validate_package_path(package)?;
        validate_path(name, "target name", false)?;
        Ok(Label {
            // Bazel 9.3 Label.computeRepoNameWithRepoContext reserves these
            // package names only when no explicit repository prefix is present.
            repository: if unqualified_absolute && matches!(package, "visibility" | "conditions") {
                String::new()
            } else {
                repository.to_owned()
            },
            package: package.to_owned(),
            name: name.to_owned(),
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
            let (name, _) = apparent
                .split_once("//")
                .ok_or_else(|| format!("unsupported repository label shorthand: {input:?}"))?;
            validate_repository_name(name)?;
            mapping.insert(name.to_owned(), String::new());
        }
        Self::parse_in(input, "", package, &mapping).map(|_| ())
    }

    pub fn with_name(&self, name: &str) -> Result<Self, String> {
        validate_repository_name(&self.repository)?;
        validate_package_path(&self.package)?;
        validate_path(name, "target name", false)?;
        Ok(Self {
            name: name.to_owned(),
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

    pub fn generated_exec_path(&self) -> PathBuf {
        PathBuf::from("bazel-out/rust-fastbuild/bin").join(self.source_exec_path())
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

pub(crate) fn validate_repository_name(repository: &str) -> Result<(), String> {
    if repository.contains('/') {
        return Err(format!(
            "invalid repository name {repository:?}: expected a single safe path component"
        ));
    }
    validate_path(repository, "repository name", true)
}

pub(crate) fn validate_package_path(package: &str) -> Result<(), String> {
    validate_path(package, "package path", true)
}

fn validate_path(value: &str, kind: &str, allow_empty: bool) -> Result<(), String> {
    if allow_empty && value.is_empty() {
        return Ok(());
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "invalid {kind} {value:?}: empty, '.' and '..' path components are not allowed"
            ));
        }
        if !component
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
        {
            return Err(format!(
                "unsupported {kind} {value:?}: this implementation accepts only ASCII letters, digits, '_', '-', '.', '+' and '/' (a stricter subset of Bazel label syntax)"
            ));
        }
    }
    Ok(())
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
    fn rejects_unsafe_or_unsupported_labels() {
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
            "//foo:x y",
            "//café:x",
            "//foo:x%2fy",
            "//foo:x@y",
        ] {
            assert!(Label::parse(input, "").is_err(), "accepted {input:?}");
        }
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
        for input in ["@repo//p:x", ":x", "x", "@@repo"] {
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
            output.generated_exec_path(),
            PathBuf::from("bazel-out/rust-fastbuild/bin/external/dep+/p/generated/file.txt")
        );
        let main = Label::parse("//p:q/input.txt", "").unwrap();
        assert_eq!(main.source_exec_path(), PathBuf::from("p/q/input.txt"));
        assert_eq!(main.short_path(), main.source_exec_path());
        assert_eq!(
            main.generated_exec_path(),
            PathBuf::from("bazel-out/rust-fastbuild/bin/p/q/input.txt")
        );
        for name in ["../x", "/x", "//other:x", "@@other//p:x", "a//b"] {
            assert!(label.with_name(name).is_err());
        }
    }

    #[test]
    fn unsafe_repository_names_are_rejected() {
        for repository in [".", "..", "a/b", "a\\b", "a\0b", "a b", "a~b", "@x", "café"] {
            assert!(Label::parse_canonical(&format!("@@{repository}//p:x")).is_err());
        }
        assert!(Label::parse_canonical("@@a_B-1.2+//p:x").is_ok());
    }
}
