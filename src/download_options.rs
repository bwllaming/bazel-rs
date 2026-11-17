//! The downloader options of every command (docs/RC_OPTIONS.md, "Downloader
//! options"), installed once per process by the CLI like the proxies
//! (crate::download_proxy). Pinned source: `/tmp/opencode/bazel-9.3.1-reference`;
//! paths are relative to `src/main/java/com/google/devtools/build/lib/`.
//!
//! * `--http_connector_attempts`, `--http_timeout_scaling`
//!   (runtime/CommonCommandOptions.java:667-681, CommandEnvironment.java
//!   :302-325): the attempts per URL and the scaled connect and read
//!   timeouts of every HTTP download (bazel/repository/downloader/
//!   HttpConnector.java:57-110).
//! * `--downloader_config` (bazel/repository/RepositoryOptions.java:278-291):
//!   the URL rewriter (downloader/UrlRewriter.java, UrlRewriterConfig.java),
//!   applied to repository downloads (DownloadManager.downloadInExecutor
//!   :206-215) and registry files (downloadAndReadOneUrlForBzlmod :436-449).
//! * `--distdir` (RepositoryOptions.java:190-201, DownloadManager.java
//!   :278-329): directories searched for an archive by name and checksum
//!   before the network.
//! * `--module_mirrors` (RepositoryOptions.java:112-133,
//!   bazel/BazelRepositoryModule.java:623-652, bzlmod/IndexRegistry.java
//!   :460-485): mirrors of registry modules' source archives.
//!
//! Library callers and unit tests see Bazel's defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The installed settings ([`install`]).
#[derive(Debug, Default)]
pub struct Settings {
    /// `--http_connector_attempts` when positive (HttpConnector.java:83).
    pub attempts: Option<u32>,
    /// `--http_timeout_scaling` when positive (CommandEnvironment.java:302-307).
    pub timeout_scaling: Option<f64>,
    pub rewriter: Option<UrlRewriter>,
    /// Absolute `--distdir` directories, in order.
    pub distdir: Vec<PathBuf>,
    /// `--module_mirrors` per normalized registry URL
    /// (BazelRepositoryModule.java:623-652); empty without the option.
    pub module_mirrors: BTreeMap<String, Vec<String>>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

fn settings() -> Option<&'static Settings> {
    SETTINGS.get()
}

/// Install the command's settings (once; a later call is ignored).
pub fn install(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

/// Java's `Double.parseDouble`: optional surrounding whitespace (`<= ' '`),
/// an optional `d`/`D`/`f`/`F` suffix, `NaN` and `[+-]Infinity` spelled
/// exactly; `None` for anything else.
pub fn parse_java_double(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(|c: char| c <= ' ');
    let unsuffixed = trimmed
        .strip_suffix(['d', 'D', 'f', 'F'])
        .filter(|rest| !rest.is_empty() && !rest.ends_with(['e', 'E', '+', '-']))
        .unwrap_or(trimmed);
    match unsuffixed {
        "NaN" | "+NaN" | "-NaN" => return Some(f64::NAN),
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust also accepts `inf`, `infinity` and `nan` in any case; Java does
    // not (only the spellings above).
    if unsuffixed
        .trim_start_matches(['+', '-'])
        .starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return None;
    }
    unsuffixed.parse::<f64>().ok()
}

/// HttpConnector.MAX_ATTEMPTS (java:56), or `--http_connector_attempts`.
pub fn connect_attempts() -> u32 {
    settings()
        .and_then(|settings| settings.attempts)
        .unwrap_or(8)
}

/// `unscaled` scaled by `--http_timeout_scaling` (HttpConnector.scale,
/// java:105-107: rounded to milliseconds).
pub fn scaled(unscaled: Duration) -> Duration {
    match settings().and_then(|settings| settings.timeout_scaling) {
        Some(scaling) => {
            let millis = (unscaled.as_millis() as f64 * scaling).round();
            if millis.is_finite() && millis >= 1.0 {
                Duration::from_millis(millis.min(u64::MAX as f64) as u64)
            } else {
                Duration::from_millis(1)
            }
        }
        None => unscaled,
    }
}

/// CommandEnvironment.java:302-325: the attempts (0 and negative values
/// select the default, HttpConnector.java:83) and the timeout scaling (a
/// non-positive factor is 1.0; Bazel's "Ignoring request to scale http
/// timeouts by a non-positive factor" warning is reported before the UI
/// listens, so genuine never shows it, and neither does this).
pub fn http_settings(attempts: Option<i32>, scaling: Option<f64>) -> Settings {
    Settings {
        attempts: attempts
            .filter(|attempts| *attempts > 0)
            .map(|attempts| attempts as u32),
        // A NaN factor: Java's `(float) x <= 0` is false, its scaled
        // timeouts round to 0 (none); 1.0 here (documented difference).
        timeout_scaling: scaling.filter(|scaling| *scaling > 0.0 && *scaling != 1.0),
        ..Settings::default()
    }
}

/// One `rewrite` directive.
#[derive(Debug)]
struct Rewrite {
    pattern: fancy_regex::Regex,
    /// `^(?:PATTERN)$` for Matcher.matches.
    full: fancy_regex::Regex,
    replacements: Vec<String>,
}

/// UrlRewriterConfig + UrlRewriter: `allow HOST`, `block HOST` (`*` blocks
/// every host), `rewrite PATTERN REPLACEMENT` (repeated patterns collect
/// their replacements in order) and `all_blocked_message TEXT`; `#` starts a
/// comment line.
#[derive(Debug, Default)]
pub struct UrlRewriter {
    allow: Vec<String>,
    block: Vec<String>,
    rewrites: Vec<Rewrite>,
    all_blocked_message: Option<String>,
}

impl UrlRewriter {
    /// UrlRewriterConfig.parseConfig (java:93-179). Errors are Bazel's
    /// `Failed to parse downloader config at FILE:LINE: …`
    /// (BazelRepositoryModule.java:436-446).
    pub fn parse(file: &str, text: &str) -> Result<Self, String> {
        let mut rewriter = Self::default();
        for (index, line) in text.lines().enumerate() {
            let location = format!("{file}:{}", index + 1);
            let fail = |message: String| {
                format!("Failed to parse downloader config at {location}: {message}")
            };
            let parts: Vec<&str> = line.split_whitespace().collect();
            let Some(first) = parts.first() else {
                continue;
            };
            if first.starts_with('#') {
                continue;
            }
            match *first {
                "allow" | "block" => {
                    if parts.len() != 2 {
                        return Err(fail(format!(
                            "Only the host name is allowed after `{first}`: {line}"
                        )));
                    }
                    let host = parts[1].to_owned();
                    let list = if *first == "allow" {
                        &mut rewriter.allow
                    } else {
                        &mut rewriter.block
                    };
                    if !list.contains(&host) {
                        list.push(host);
                    }
                }
                "rewrite" => {
                    if parts.len() != 3 {
                        return Err(fail(format!(
                            "Only the matching pattern and rewrite pattern is allowed after `rewrite`: {line}"
                        )));
                    }
                    let invalid = |error: fancy_regex::Error| {
                        fail(format!(
                            "Invalid regex in `rewrite`: {error} in `{}`",
                            parts[1]
                        ))
                    };
                    let pattern = fancy_regex::Regex::new(parts[1]).map_err(invalid)?;
                    let full =
                        fancy_regex::Regex::new(&format!("^(?:{})$", parts[1])).map_err(invalid)?;
                    // ImmutableMultimap.asMap: one entry per distinct
                    // pattern, in first-appearance order.
                    match rewriter
                        .rewrites
                        .iter_mut()
                        .find(|rewrite| rewrite.pattern.as_str() == parts[1])
                    {
                        Some(rewrite) => rewrite.replacements.push(parts[2].to_owned()),
                        None => rewriter.rewrites.push(Rewrite {
                            pattern,
                            full,
                            replacements: vec![parts[2].to_owned()],
                        }),
                    }
                }
                "all_blocked_message" => {
                    if parts.len() == 1 {
                        return Err(fail(
                            "all_blocked_message must be followed by a message".to_owned(),
                        ));
                    }
                    if rewriter.all_blocked_message.is_some() {
                        return Err(fail(
                            "At most one all_blocked_message directive is allowed".to_owned(),
                        ));
                    }
                    // line.substring("all_blocked_message".length() + 1).
                    let start = line.find("all_blocked_message").unwrap_or(0)
                        + "all_blocked_message".len()
                        + 1;
                    rewriter.all_blocked_message =
                        Some(line.get(start..).unwrap_or_default().to_owned());
                }
                _ => return Err(fail(format!("Unable to parse: {line}"))),
            }
        }
        Ok(rewriter)
    }

    /// UrlRewriter.getDownloaderUrlRewriter (java:87-140): the file relative
    /// to the workspace root.
    pub fn load(workspace: &Path, config: &str) -> Result<Self, String> {
        let path = workspace.join(config);
        // snapshot-unrecorded: read once before planning; the rewriter only chooses where a checksum-verified download comes from, and a snapshot hit downloads nothing.
        let text = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "Failed to parse downloader config: Unable to find downloader config file {}",
                    path.display()
                )
            } else {
                format!(
                    "Failed to parse downloader config: {}: {error}",
                    path.display()
                )
            }
        })?;
        Self::parse(config, &String::from_utf8_lossy(&text))
    }

    /// UrlRewriter.amend (java:150-153): every URL's rewrites, in order.
    pub fn amend(&self, urls: &[String]) -> Result<Vec<String>, String> {
        let mut result = Vec::new();
        for url in urls {
            result.extend(self.rewrite(url)?);
        }
        Ok(result)
    }

    /// UrlRewriter.rewrite (java:200-226).
    fn rewrite(&self, url: &str) -> Result<Vec<String>, String> {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Ok(vec![url.to_owned()]);
        };
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Ok(vec![url.to_owned()]);
        }
        let candidates = self.apply_rewrite_rules(url, scheme, rest)?;
        Ok(candidates
            .into_iter()
            .filter(|candidate| self.allowed(candidate) || !self.blocked(candidate))
            .collect())
    }

    fn allowed(&self, url: &str) -> bool {
        self.allow.iter().any(|host| matching_host(url, host))
    }

    fn blocked(&self, url: &str) -> bool {
        self.block
            .iter()
            .any(|host| host == "*" || matching_host(url, host))
    }

    /// UrlRewriter.applyRewriteRules (java:255-277).
    fn apply_rewrite_rules(
        &self,
        url: &str,
        scheme: &str,
        without_scheme: &str,
    ) -> Result<Vec<String>, String> {
        let mut rewritten: Vec<String> = Vec::new();
        let mut matched = false;
        for rewrite in &self.rewrites {
            if !rewrite.full.is_match(without_scheme).unwrap_or(false) {
                continue;
            }
            matched = true;
            for replacement in &rewrite.replacements {
                let text = replace_first(&rewrite.pattern, without_scheme, replacement)?;
                if !rewritten.contains(&text) {
                    rewritten.push(text);
                }
            }
        }
        if !matched {
            return Ok(vec![url.to_owned()]);
        }
        Ok(rewritten
            .into_iter()
            .map(|text| {
                if text.starts_with("http://") || text.starts_with("https://") {
                    text
                } else {
                    format!("{scheme}://{text}")
                }
            })
            .collect())
    }

    /// DownloadManager.getRewriterBlockedAllUrlsMessage (java:500-511).
    pub fn blocked_all_message(&self, original: &[String]) -> String {
        let mut message = format!(
            "Configured URL rewriter blocked all URLs: [{}]",
            original.join(", ")
        );
        if let Some(extra) = &self.all_blocked_message {
            message.push_str(" - ");
            message.push_str(extra);
        }
        message
    }
}

/// UrlRewriter.isMatchingHostName (java:250-253): the URL's host is `host`
/// or a subdomain of it.
fn matching_host(url: &str, host: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|url_host| url_host == host || url_host.ends_with(&format!(".{host}")))
}

/// Matcher.replaceFirst (java.util.regex): the leftmost match of `pattern`
/// in `text` replaced by the expanded `replacement` (`$N`, `${name}`, `\x`).
fn replace_first(
    pattern: &fancy_regex::Regex,
    text: &str,
    replacement: &str,
) -> Result<String, String> {
    let Some(captures) = pattern
        .captures(text)
        .map_err(|error| format!("downloader config regex failed: {error}"))?
    else {
        return Ok(text.to_owned());
    };
    let whole = captures.get(0).expect("group 0 of a match");
    let groups = captures.len() - 1;
    let mut expanded = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next) => expanded.push(next),
                None => return Err("character to be escaped is missing".to_owned()),
            },
            '$' => match chars.peek().copied() {
                Some('{') => {
                    chars.next();
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) if c.is_ascii_alphanumeric() => name.push(c),
                            _ => {
                                return Err(
                                    "named capturing group is missing trailing '}'".to_owned()
                                );
                            }
                        }
                    }
                    let group = pattern
                        .capture_names()
                        .position(|candidate| candidate == Some(name.as_str()))
                        .ok_or_else(|| format!("No group with name {{{name}}}"))?;
                    if let Some(value) = captures.get(group) {
                        expanded.push_str(value.as_str());
                    }
                }
                Some(digit @ '0'..='9') => {
                    chars.next();
                    let mut number = digit.to_digit(10).unwrap_or(0) as usize;
                    if number > groups {
                        return Err(format!("No group {number}"));
                    }
                    // Further digits while the group exists.
                    while let Some(next @ '0'..='9') = chars.peek().copied() {
                        let candidate = number * 10 + next.to_digit(10).unwrap_or(0) as usize;
                        if candidate > groups {
                            break;
                        }
                        number = candidate;
                        chars.next();
                    }
                    if let Some(value) = captures.get(number) {
                        expanded.push_str(value.as_str());
                    }
                }
                _ => return Err("Illegal group reference".to_owned()),
            },
            c => expanded.push(c),
        }
    }
    Ok(format!(
        "{}{expanded}{}",
        &text[..whole.start()],
        &text[whole.end()..]
    ))
}

/// The URLs of a download after the installed rewriter: `Ok(urls)` (empty
/// only when `urls` is), or Bazel's "blocked all URLs" message when the
/// rewriter left none.
pub fn rewrite_urls(urls: &[String]) -> Result<Vec<String>, String> {
    let Some(rewriter) = settings().and_then(|settings| settings.rewriter.as_ref()) else {
        return Ok(urls.to_vec());
    };
    let rewritten = rewriter.amend(urls)?;
    if rewritten.is_empty() && !urls.is_empty() {
        return Err(rewriter.blocked_all_message(urls));
    }
    Ok(rewritten)
}

/// Whether a URL rewriter is installed.
pub fn rewriting() -> bool {
    settings().is_some_and(|settings| settings.rewriter.is_some())
}

/// DownloadManager.getUrlBaseName (java:553-575): the last segment of the
/// URL's decoded path.
pub fn url_base_name(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let path = percent_decode(parsed.path());
    let name = path.rsplit('/').next().unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// URI.getPath's decoding of `%XX` escapes (UTF-8, lossy).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(hex) = text.get(at + 1..at + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            at += 3;
            continue;
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// DownloadManager.FS_UNSAFE_CHARS (java:513-520): every character but
/// `[a-zA-Z0-9._-]` replaced by `_`.
pub fn fs_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The distdir lookup of DownloadManager.downloadInExecutor (java:278-329)
/// after a cache miss: each `--distdir` (`INFO: non-existent distdir D`,
/// `WARNING: distdir D is not a directory`) is searched for each candidate
/// name (the main URL's base name, then the download's own file name);
/// `matches` checks a candidate's checksum. `None` without `--distdir`.
pub fn distdir_lookup(names: &[String], matches: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let settings = settings()?;
    // A repository rule's events: held by a running speculation (replayed
    // when it is adopted), else printed.
    let event = |text: String| {
        if !crate::repository_speculation::defer_stderr(&crate::ui::marked(&text)) {
            crate::ui::note(&text);
        }
    };
    for directory in &settings.distdir {
        // snapshot-unrecorded: a distdir only supplies a file whose checksum the download already pins; it changes no repository content.
        match std::fs::metadata(directory) {
            Err(_) => event(format!(
                "INFO: non-existent distdir {}",
                directory.display()
            )),
            Ok(metadata) if !metadata.is_dir() => event(format!(
                "WARNING: distdir {} is not a directory",
                directory.display()
            )),
            Ok(_) => {
                for name in names {
                    let candidate = directory.join(name);
                    // snapshot-unrecorded: as above, a checksum-verified candidate.
                    if std::fs::metadata(&candidate).is_ok_and(|metadata| metadata.is_file())
                        && matches(&candidate)
                    {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

/// The distdir candidate names (DownloadManager.getCandidateFileNames,
/// java:544-551): the main URL's base name, then `destination` when it
/// differs.
pub fn candidate_names(main_url: Option<&str>, destination: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(base) = main_url.and_then(url_base_name)
        && base != destination
    {
        names.push(base);
    }
    names.push(destination.to_owned());
    names
}

/// Whether `--distdir` is set.
pub fn has_distdir() -> bool {
    settings().is_some_and(|settings| !settings.distdir.is_empty())
}

/// BazelRepositoryModule.normalizeBaseUrl (java:683-687).
fn normalize_base_url(url: &str) -> String {
    url.trim_end_matches('/').to_owned()
}

/// BazelRepositoryModule.java:623-652: `entries` (the `--module_mirrors`
/// assignments in order; the empty key is every registry without its own)
/// over the normalized registries. Fails for a registry not listed.
pub fn module_mirrors(
    registries: &[String],
    entries: &[(String, Vec<String>)],
) -> Result<BTreeMap<String, Vec<String>>, String> {
    if entries.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut ordered: Vec<(String, Vec<String>)> = Vec::new();
    for (key, values) in entries {
        let key = normalize_base_url(key);
        let mut mirrors = Vec::new();
        for value in values {
            let value = normalize_base_url(value);
            if !mirrors.contains(&value) {
                mirrors.push(value);
            }
        }
        // toImmutableMap with last-wins merging keeps the first position.
        match ordered.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = mirrors,
            None => ordered.push((key, mirrors)),
        }
    }
    let registries: Vec<String> = registries.iter().map(|r| normalize_base_url(r)).collect();
    let unknown: Vec<&str> = ordered
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !key.is_empty() && !registries.iter().any(|r| r == key))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "--module_mirrors references registries not listed in --registries: {}",
            unknown.join(", ")
        ));
    }
    let default = ordered
        .iter()
        .find(|(key, _)| key.is_empty())
        .map(|(_, mirrors)| mirrors.clone())
        .unwrap_or_default();
    Ok(registries
        .into_iter()
        .map(|registry| {
            let mirrors = ordered
                .iter()
                .find(|(key, _)| *key == registry)
                .map_or_else(|| default.clone(), |(_, mirrors)| mirrors.clone());
            (registry, mirrors)
        })
        .collect())
}

/// The normalized key of `registry` in the module mirrors.
pub fn registry_key(registry: &crate::registry::Registry) -> String {
    match registry {
        crate::registry::Registry::Url(base) => normalize_base_url(base),
        crate::registry::Registry::LocalPath(path) => {
            normalize_base_url(&format!("file://{}", path.display()))
        }
    }
}

/// IndexRegistry.createArchiveRepoSpec's mirror list (java:463-470): the
/// `--module_mirrors` of `registry`, then the registry's own
/// `bazel_registry.json` mirrors, without duplicates.
pub fn archive_mirrors(
    registry: &crate::registry::Registry,
    registry_mirrors: &[String],
) -> Vec<String> {
    let mut mirrors: Vec<String> = settings()
        .and_then(|settings| settings.module_mirrors.get(&registry_key(registry)))
        .cloned()
        .unwrap_or_default();
    for mirror in registry_mirrors {
        if !mirrors.contains(mirror) {
            mirrors.push(mirror.clone());
        }
    }
    mirrors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_doubles() {
        for (text, value) in [
            ("10.0", 10.0),
            (" 5 ", 5.0),
            ("1e3", 1000.0),
            ("2d", 2.0),
            ("-1", -1.0),
        ] {
            assert_eq!(parse_java_double(text), Some(value), "{text}");
        }
        assert_eq!(parse_java_double("Infinity"), Some(f64::INFINITY));
        for text in ["x", "inf", "infinity", "nan", "", "1e"] {
            assert_eq!(parse_java_double(text), None, "{text}");
        }
    }

    #[test]
    fn http_settings_follow_command_environment() {
        let settings = http_settings(Some(3), Some(2.5));
        assert_eq!(settings.attempts, Some(3));
        assert_eq!(settings.timeout_scaling, Some(2.5));
        for (attempts, scaling) in [
            (Some(0), Some(0.0)),
            (Some(-1), Some(-2.0)),
            (None, Some(f64::NAN)),
        ] {
            let settings = http_settings(attempts, scaling);
            assert_eq!(settings.attempts, None);
            assert_eq!(settings.timeout_scaling, None);
        }
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|url| (*url).to_owned()).collect()
    }

    #[test]
    fn rewriter_rewrites_allows_and_blocks_like_bazel() {
        let rewriter = UrlRewriter::parse(
            "dl.cfg",
            "# comment\n\
             rewrite github\\.com/(.*) mirror.example/github.com/$1\n\
             rewrite github\\.com/(.*) github.com/$1\n\
             rewrite (.*)\\.example\\.org/(.*) https://cache.local/$1/$2\n\
             block evil.com\n\
             allow ok.evil.com\n\
             all_blocked_message see go/mirrors\n",
        )
        .unwrap();
        assert_eq!(
            rewriter
                .amend(&urls(&[
                    "https://github.com/a/b.tgz",
                    "http://x.example.org/y"
                ]))
                .unwrap(),
            urls(&[
                "https://mirror.example/github.com/a/b.tgz",
                "https://github.com/a/b.tgz",
                "https://cache.local/x/y",
            ])
        );
        assert_eq!(
            rewriter
                .amend(&urls(&[
                    "https://sub.evil.com/a",
                    "https://ok.evil.com/b",
                    "file:///x"
                ]))
                .unwrap(),
            urls(&["https://ok.evil.com/b", "file:///x"])
        );
        assert_eq!(
            rewriter.blocked_all_message(&urls(&["https://evil.com/a"])),
            "Configured URL rewriter blocked all URLs: [https://evil.com/a] - see go/mirrors"
        );
        let all = UrlRewriter::parse("c", "block *\n").unwrap();
        assert!(all.amend(&urls(&["https://a/b"])).unwrap().is_empty());
    }

    #[test]
    fn rewriter_parse_errors_name_the_line() {
        for (text, message) in [
            (
                "allow",
                "Only the host name is allowed after `allow`: allow",
            ),
            (
                "rewrite a",
                "Only the matching pattern and rewrite pattern is allowed after `rewrite`: rewrite a",
            ),
            (
                "all_blocked_message",
                "all_blocked_message must be followed by a message",
            ),
            ("bogus x", "Unable to parse: bogus x"),
        ] {
            let error = UrlRewriter::parse("f.cfg", &format!("\n{text}")).unwrap_err();
            assert_eq!(
                error,
                format!("Failed to parse downloader config at f.cfg:2: {message}")
            );
        }
    }

    #[test]
    fn java_replacement_expansion() {
        let pattern = fancy_regex::Regex::new("a(b)(c)").unwrap();
        assert_eq!(
            replace_first(&pattern, "xabcy", "[$2$1\\$]").unwrap(),
            "x[cb$]y"
        );
        assert_eq!(replace_first(&pattern, "abc", "$10").unwrap(), "b0");
        assert!(replace_first(&pattern, "abc", "$9").is_err());
    }

    #[test]
    fn module_mirrors_per_registry_and_default() {
        let registries = urls(&["https://bcr.bazel.build/", "https://other/"]);
        let map = module_mirrors(
            &registries,
            &[
                (String::new(), urls(&["https://m1/", "https://m2"])),
                ("https://other".into(), urls(&["https://o"])),
            ],
        )
        .unwrap();
        assert_eq!(
            map["https://bcr.bazel.build"],
            urls(&["https://m1", "https://m2"])
        );
        assert_eq!(map["https://other"], urls(&["https://o"]));
        assert_eq!(
            module_mirrors(&registries, &[("https://x/".into(), urls(&["https://m"]))])
                .unwrap_err(),
            "--module_mirrors references registries not listed in --registries: https://x"
        );
        assert!(module_mirrors(&registries, &[]).unwrap().is_empty());
    }

    #[test]
    fn distdir_candidates() {
        assert_eq!(
            candidate_names(Some("https://h/p/a%20b.zip?x=1"), "a_b.zip"),
            urls(&["a b.zip", "a_b.zip"])
        );
        assert_eq!(
            candidate_names(Some("https://h/a.zip"), "a.zip"),
            urls(&["a.zip"])
        );
        assert_eq!(fs_safe("a b+c.tar.gz"), "a_b_c.tar.gz");
    }
}
