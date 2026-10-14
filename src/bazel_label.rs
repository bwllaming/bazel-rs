//! Faithful port of Bazel 9.3's label *grammar* and its exact syntax errors
//! (`LabelParser.Parts`, `LabelValidator.validatePackageName` /
//! `validateTargetName`, `RepositoryName.validate`, `Label.parseWithRepoContext`,
//! `StringUtilities.sanitizeControlChars`). Repository *resolution* is left to
//! the caller. Strings are processed as bytes, like Bazel's Latin-1 internal
//! strings: every non-ASCII byte is an ordinary (target-allowed,
//! package-disallowed) character.
//!
//! Used where Bazel's full grammar and diagnostics are observable (bzl
//! `visibility()` specifications); ordinary native labels keep `crate::label`.

/// The validated parts of a raw label (`LabelParser.Parts`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Parts<'a> {
    /// The `@repo` / `@@repo` part without its `@`s.
    pub repo: Option<&'a str>,
    pub repo_is_canonical: bool,
    pub pkg_is_absolute: bool,
    pub pkg: &'a str,
    pub pkg_ends_with_triple_dots: bool,
    pub target: &'a str,
}

/// `LabelParser.syntaxErrorf`: control characters are escaped.
fn sanitize(message: String) -> String {
    if !message.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return message;
    }
    let mut out = String::with_capacity(message.len());
    for c in message.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => out.push_str("<?>"),
            c => out.push(c),
        }
    }
    out
}

/// `RepositoryName.validate` (RepositoryName.java:178-196).
pub(crate) fn validate_repo_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "_builtins" {
        return Ok(());
    }
    if name == "." || name == ".." {
        return Err(sanitize(format!(
            "invalid repository name '{name}': repo names are not allowed to be '{name}'"
        )));
    }
    // VALID_REPO_NAME = [\w\-.+]* with Java's ASCII-only \w.
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
    {
        return Err(sanitize(format!(
            "invalid repository name '{name}': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'"
        )));
    }
    Ok(())
}

/// `LabelValidator.validatePackageName`: `None` on success.
pub(crate) fn validate_package_name(name: &str) -> Option<&'static str> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes[0] == b'/' {
        return Some("package names may not start with '/'");
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b" !\"#$%&'()*+,-./;<=>?@[]^_`{|}~".contains(&b))
    {
        return Some(
            "package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
        );
    }
    if bytes[bytes.len() - 1] == b'/' {
        return Some("package names may not end with '/'");
    }
    // Backwards, treating the position before the start as a '/'.
    let mut non_dot = false;
    let mut last_slash = true;
    for index in (-1..bytes.len() as isize).rev() {
        let c = if index >= 0 {
            bytes[index as usize]
        } else {
            b'/'
        };
        if c == b'/' {
            if last_slash {
                return Some("package names may not contain '//' path separators");
            }
            if !non_dot {
                return Some("package name component contains only '.' characters");
            }
            non_dot = false;
            last_slash = true;
        } else {
            if c != b'.' {
                non_dot = true;
            }
            last_slash = false;
        }
    }
    None
}

fn always_allowed_target_byte(b: u8) -> bool {
    // javaLetterOrDigit, both punctuation sets, and every non-ASCII char.
    b.is_ascii_alphanumeric() || b" \"#$&'()*+,;<=>?[]{|}~!%-@^_`".contains(&b) || b >= 128
}

/// `LabelValidator.validateTargetName`: `None` on success.
pub(crate) fn validate_target_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Some("empty target name".into());
    }
    match bytes[0] {
        b'/' => return Some("target names may not start with '/'".into()),
        b'.' => {
            if name.starts_with("../") || name == ".." {
                return Some("target names may not contain up-level references '..'".into());
            } else if name == "." {
                return None;
            } else if name.starts_with("./") {
                return Some("target names may not contain '.' as a path segment".into());
            }
        }
        _ => {}
    }
    if name.ends_with('\r') {
        return Some(
            "target names may not end with carriage returns (perhaps the input source is CRLF-terminated)"
                .into(),
        );
    }
    for (index, &c) in bytes.iter().enumerate() {
        if always_allowed_target_byte(c) || c == b'.' {
            continue;
        }
        if c == b'/' {
            let rest = &name[index..];
            if rest.starts_with("/../") {
                return Some("target names may not contain up-level references '..'".into());
            } else if rest.starts_with("/./") {
                return Some("target names may not contain '.' as a path segment".into());
            } else if rest.starts_with("//") {
                return Some("target names may not contain '//' path separators".into());
            }
            continue;
        }
        if c <= 0x1f || c == 0x7f {
            return Some(format!(
                "target names may not contain non-printable characters: '\\x{c:02X}'"
            ));
        }
        return Some(format!("target names may not contain '{}'", c as char));
    }
    let last = bytes[bytes.len() - 1];
    if last == b'.' {
        if name.ends_with("/..") {
            return Some("target names may not contain up-level references '..'".into());
        } else if name.ends_with("/.") {
            return None;
        }
    }
    if last == b'/' {
        return Some("target names may not end with '/'".into());
    }
    None
}

fn perhaps_you_meant(pkg: &str, target: &str) -> String {
    if pkg.ends_with(&format!("/{target}")) {
        format!(" (perhaps you meant \":{target}\"?)")
    } else {
        String::new()
    }
}

fn strip_trailing_triple_dots(pkg: &str) -> &str {
    if let Some(stripped) = pkg.strip_suffix("/...") {
        stripped
    } else if pkg == "..." {
        ""
    } else {
        pkg
    }
}

impl<'a> Parts<'a> {
    /// `LabelParser.Parts.parse` including `validateAndCreate`.
    pub(crate) fn parse(raw: &'a str) -> Result<Self, String> {
        let repo_is_canonical = raw.starts_with("@@");
        let double_slash = raw.find("//");
        let (repo, start, pkg_is_absolute) = if raw.starts_with('@') {
            let skip = if repo_is_canonical { 2 } else { 1 };
            match double_slash {
                None => {
                    // "@foo" is synonymous with "@foo//:foo".
                    let repo = &raw[skip..];
                    return Self::validate_and_create(
                        Some(repo),
                        repo_is_canonical,
                        true,
                        "",
                        false,
                        repo,
                    );
                }
                Some(index) => (Some(&raw[skip..index]), index + 2, true),
            }
        } else {
            let absolute = double_slash == Some(0);
            (None, if absolute { 2 } else { 0 }, absolute)
        };
        let colon = raw[start..].find(':').map(|index| index + start);
        let raw_pkg = &raw[start..colon.unwrap_or(raw.len())];
        let triple = raw_pkg.ends_with("/...") || raw_pkg == "...";
        let (pkg, target) = if colon.is_none() && triple {
            (strip_trailing_triple_dots(raw_pkg), "")
        } else if colon.is_none() && !pkg_is_absolute {
            ("", &raw[start..])
        } else {
            let pkg = strip_trailing_triple_dots(raw_pkg);
            let target = match colon {
                Some(colon) => &raw[colon + 1..],
                None => &pkg[pkg.rfind('/').map_or(0, |index| index + 1)..],
            };
            (pkg, target)
        };
        Self::validate_and_create(
            repo,
            repo_is_canonical,
            pkg_is_absolute,
            pkg,
            triple,
            target,
        )
    }

    fn validate_and_create(
        repo: Option<&'a str>,
        repo_is_canonical: bool,
        pkg_is_absolute: bool,
        pkg: &'a str,
        pkg_ends_with_triple_dots: bool,
        target: &'a str,
    ) -> Result<Self, String> {
        if let Some(repo) = repo {
            validate_repo_name(repo)?;
        }
        if let Some(error) = validate_package_name(pkg) {
            return Err(sanitize(format!(
                "invalid package name '{pkg}': {error}{}",
                perhaps_you_meant(pkg, target)
            )));
        }
        let target = if pkg_ends_with_triple_dots && target.is_empty() {
            target
        } else {
            if let Some(error) = validate_target_name(target) {
                return Err(sanitize(format!(
                    "invalid target name '{target}': {error}{}",
                    perhaps_you_meant(pkg, target)
                )));
            }
            target.strip_suffix("/.").unwrap_or(target)
        };
        Ok(Self {
            repo,
            repo_is_canonical,
            pkg_is_absolute,
            pkg,
            pkg_ends_with_triple_dots,
            target,
        })
    }

    /// The syntax half of `Label.parseWithRepoContext` (Label.java:200-208).
    pub(crate) fn parse_with_repo_context(raw: &'a str) -> Result<Self, String> {
        let parts = Self::parse(raw)?;
        if parts.pkg_ends_with_triple_dots {
            return Err(sanitize(format!(
                "invalid label '{raw}': package name cannot contain '...'"
            )));
        }
        if !parts.pkg_is_absolute {
            return Err(sanitize(format!(
                "invalid label '{raw}': absolute label must begin with '@' or '//'"
            )));
        }
        Ok(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<(Option<&str>, bool, &str, &str), String> {
        Parts::parse_with_repo_context(raw)
            .map(|parts| (parts.repo, parts.repo_is_canonical, parts.pkg, parts.target))
    }

    #[test]
    fn label_parser_table_and_shorthands() {
        assert_eq!(parse("//foo/bar"), Ok((None, false, "foo/bar", "bar")));
        assert_eq!(
            parse("//foo/bar:quux"),
            Ok((None, false, "foo/bar", "quux"))
        );
        assert_eq!(parse("@repo"), Ok((Some("repo"), false, "", "repo")));
        assert_eq!(parse("@@repo"), Ok((Some("repo"), true, "", "repo")));
        assert_eq!(
            parse("@repo//foo/bar"),
            Ok((Some("repo"), false, "foo/bar", "bar"))
        );
        assert_eq!(parse("@@//c:x"), Ok((Some(""), true, "c", "x")));
        assert_eq!(parse("@//:x"), Ok((Some(""), false, "", "x")));
        assert_eq!(parse("//foo bar:a b"), Ok((None, false, "foo bar", "a b")));
        assert_eq!(parse("//a:b/."), Ok((None, false, "a", "b")));
        assert_eq!(parse("//a:caf\u{e9}"), Ok((None, false, "a", "caf\u{e9}")));
    }

    #[test]
    fn exact_bazel_syntax_errors() {
        for (raw, error) in [
            (
                "//b:foo:__pkg__",
                "invalid target name 'foo:__pkg__': target names may not contain ':'",
            ),
            (
                "//b/../c:__pkg__",
                "invalid package name 'b/../c': package name component contains only '.' characters",
            ),
            (
                "@package_metadata:__pkg__",
                "invalid repository name 'package_metadata:__pkg__': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'",
            ),
            (
                "@.//a",
                "invalid repository name '.': repo names are not allowed to be '.'",
            ),
            (
                "//a//b:x",
                "invalid package name 'a//b': package names may not contain '//' path separators",
            ),
            (
                "//a/:x",
                "invalid package name 'a/': package names may not end with '/'",
            ),
            (
                "//a/b/",
                "invalid package name 'a/b/': package names may not end with '/' (perhaps you meant \":\"?)",
            ),
            (
                "//a\\b:x",
                "invalid package name 'a\\b': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
            ),
            (
                "//caf\u{e9}:x",
                "invalid package name 'caf\u{e9}': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
            ),
            ("//a:", "invalid target name '': empty target name"),
            (
                "//a:/x",
                "invalid target name '/x': target names may not start with '/'",
            ),
            (
                "//a:../x",
                "invalid target name '../x': target names may not contain up-level references '..'",
            ),
            (
                "//a:./x",
                "invalid target name './x': target names may not contain '.' as a path segment",
            ),
            (
                "//a:x/../y",
                "invalid target name 'x/../y': target names may not contain up-level references '..'",
            ),
            (
                "//a:x//y",
                "invalid target name 'x//y': target names may not contain '//' path separators",
            ),
            (
                "//a:x/",
                "invalid target name 'x/': target names may not end with '/'",
            ),
            (
                "//a:x\\y",
                "invalid target name 'x\\y': target names may not contain '\\'",
            ),
            (
                "//a:x\ty",
                "invalid target name 'x<?>y': target names may not contain non-printable characters: '\\x09'",
            ),
            (
                "//a:x\r",
                "invalid target name 'x\\r': target names may not end with carriage returns (perhaps the input source is CRLF-terminated)",
            ),
            (
                "//foo/...",
                "invalid label '//foo/...': package name cannot contain '...'",
            ),
            (
                "foo:bar",
                "invalid label 'foo:bar': absolute label must begin with '@' or '//'",
            ),
        ] {
            assert_eq!(parse(raw).unwrap_err(), error, "{raw:?}");
        }
        // The (perhaps you meant ...) suffix is appended for package errors.
        assert_eq!(
            parse("//a/b/:b").unwrap_err(),
            "invalid package name 'a/b/': package names may not end with '/'"
        );
        assert_eq!(
            parse("//a/./b").unwrap_err(),
            "invalid package name 'a/./b': package name component contains only '.' characters (perhaps you meant \":b\"?)"
        );
    }
}
