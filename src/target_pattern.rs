//! Command-line target patterns: `TargetPattern.Parser.parse`
//! (cmdline/TargetPattern.java:814-857) and `SignedTargetPattern.parse`
//! (cmdline/SignedTargetPattern.java), on the Bazel label grammar
//! (`crate::bazel_label::Parts`). Repository resolution and evaluation are the
//! engine's (`Engine::select_filtered`).

/// The kind of a parsed pattern (`TargetPattern.Type`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `//p:t`, `@r//p`, `p:t` (relative): one explicit target.
    SingleTarget { target: String },
    /// `p/t` without a colon in the main repository: a path whose longest
    /// package prefix owns the rest (`InterpretPathAsTarget`), already
    /// resolved against the relative directory.
    PathAsTarget { path: String },
    /// `:all` (rules), `:*` / `:all-targets` (every target) of a package.
    TargetsInPackage {
        suffix: String,
        absolute: bool,
        rules_only: bool,
    },
    /// `p/...`, `p/...:all` (rules), `p/...:*` / `p/...:all-targets`.
    TargetsBelowDirectory { rules_only: bool },
}

/// A parsed, signed target pattern. `repo` is the `@repo`/`@@repo` part, if
/// any; `package` is the package (or directory) after the relative directory
/// was applied (`createPackageIdentifierFromParts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub original: String,
    pub negative: bool,
    pub repo: Option<String>,
    pub repo_is_canonical: bool,
    pub package: String,
    pub kind: Kind,
}

/// TargetPattern.java:773-780.
const ALL_RULES_IN_SUFFIXES: [&str; 1] = ["all"];
const ALL_TARGETS_IN_SUFFIXES: [&str; 2] = ["*", "all-targets"];

/// `PathFragment.getRelative` of two normalized relative paths.
fn relative(directory: &str, path: &str) -> String {
    match (directory.is_empty(), path.is_empty()) {
        (true, _) => path.to_owned(),
        (_, true) => directory.to_owned(),
        _ => format!("{directory}/{path}"),
    }
}

/// `SignedTargetPattern.parse` then `TargetPattern.Parser.parse` in the main
/// repository with `relative_directory` (the working directory below the
/// workspace root, TargetPattern.java:783-789).
pub(crate) fn parse(text: &str, relative_directory: &str) -> Result<Pattern, String> {
    let (negative, pattern) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let parts = crate::bazel_label::Parts::parse(pattern)?;
    let signed = |repo: Option<&str>, package: String, kind: Kind| Pattern {
        original: pattern.to_owned(),
        negative,
        repo: repo.map(str::to_owned),
        repo_is_canonical: parts.repo_is_canonical,
        package,
        kind,
    };
    // A pattern like `foo/bar/baz` is a file path (TargetPattern.java:821-831).
    if !parts.pkg_is_absolute
        && parts.pkg.is_empty()
        && !parts.pkg_ends_with_triple_dots
        && !pattern.contains(':')
    {
        let path = relative(relative_directory, parts.target);
        return Ok(signed(None, String::new(), Kind::PathAsTarget { path }));
    }
    let package = if parts.pkg_is_absolute {
        parts.pkg.to_owned()
    } else {
        relative(relative_directory, parts.pkg)
    };
    let all_rules = ALL_RULES_IN_SUFFIXES.contains(&parts.target);
    let all_targets = ALL_TARGETS_IN_SUFFIXES.contains(&parts.target);
    if parts.pkg_ends_with_triple_dots {
        if parts.target.is_empty() || all_rules {
            return Ok(signed(
                parts.repo,
                package,
                Kind::TargetsBelowDirectory { rules_only: true },
            ));
        } else if all_targets {
            return Ok(signed(
                parts.repo,
                package,
                Kind::TargetsBelowDirectory { rules_only: false },
            ));
        }
        return Err(format!(
            "Invalid target pattern {pattern}: '...' can only be used with wildcard targets"
        ));
    }
    if pattern.contains(':') && (all_rules || all_targets) {
        return Ok(signed(
            parts.repo,
            package,
            Kind::TargetsInPackage {
                suffix: parts.target.to_owned(),
                absolute: parts.pkg_is_absolute,
                rules_only: all_rules,
            },
        ));
    }
    Ok(signed(
        parts.repo,
        package,
        Kind::SingleTarget {
            target: parts.target.to_owned(),
        },
    ))
}

impl Pattern {
    /// The label text of this pattern's package in its repository, resolvable
    /// with the main repository's mapping (`@repo//pkg:name`).
    pub(crate) fn label_text(&self, name: &str) -> String {
        let repository = match (&self.repo, self.repo_is_canonical) {
            (None, _) => String::new(),
            (Some(repo), true) => format!("@@{repo}"),
            (Some(repo), false) => format!("@{repo}"),
        };
        format!("{repository}//{}:{name}", self.package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str, directory: &str) -> (String, Kind) {
        let pattern = parse(text, directory).unwrap();
        (pattern.package, pattern.kind)
    }

    #[test]
    fn target_pattern_parser_table() {
        let below = |rules_only| Kind::TargetsBelowDirectory { rules_only };
        let package = |suffix: &str, absolute, rules_only| Kind::TargetsInPackage {
            suffix: suffix.to_owned(),
            absolute,
            rules_only,
        };
        let single = |target: &str| Kind::SingleTarget {
            target: target.to_owned(),
        };
        for (text, directory, expected_package, expected) in [
            ("//...", "", "", below(true)),
            ("//...:all", "", "", below(true)),
            ("//...:*", "", "", below(false)),
            ("//...:all-targets", "", "", below(false)),
            ("//src/...", "x", "src", below(true)),
            ("...", "a/b", "a/b", below(true)),
            ("c/...:*", "a", "a/c", below(false)),
            ("//p:all", "", "p", package("all", true, true)),
            ("//p:*", "", "p", package("*", true, false)),
            (
                "//p:all-targets",
                "",
                "p",
                package("all-targets", true, false),
            ),
            (":all", "d", "d", package("all", false, true)),
            ("q:*", "d", "d/q", package("*", false, false)),
            ("//p", "", "p", single("p")),
            ("//p/all", "", "p/all", single("all")),
            ("//p:t", "", "p", single("t")),
            ("q:t", "d", "d/q", single("t")),
            (":t", "d", "d", single("t")),
            ("@r", "", "", single("r")),
            ("//a b/(c):d e", "", "a b/(c)", single("d e")),
        ] {
            assert_eq!(
                kind(text, directory),
                (expected_package.to_owned(), expected),
                "{text:?} in {directory:?}"
            );
        }
        assert_eq!(
            kind("p/t", "d"),
            (
                String::new(),
                Kind::PathAsTarget {
                    path: "d/p/t".into()
                }
            )
        );
        let negative = parse("-//x/...", "").unwrap();
        assert!(negative.negative);
        assert_eq!(negative.original, "//x/...");
        assert_eq!(parse("@r//x/...", "").unwrap().label_text("n"), "@r//x:n");
        assert_eq!(parse("@@r+//...", "").unwrap().label_text("n"), "@@r+//:n");
    }

    #[test]
    fn target_pattern_errors_are_bazel_texts() {
        for (text, error) in [
            (
                "//p/...:t",
                "Invalid target pattern //p/...:t: '...' can only be used with wildcard targets",
            ),
            (
                "//p:a:b",
                "invalid target name 'a:b': target names may not contain ':'",
            ),
            (
                "//caf\u{e9}/...",
                "invalid package name 'caf\u{e9}': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
            ),
            (
                "-//a//b",
                "invalid package name 'a//b': package names may not contain '//' path separators (perhaps you meant \":b\"?)",
            ),
        ] {
            assert_eq!(parse(text, "").unwrap_err(), error, "{text:?}");
        }
    }
}
