//! The workspace `MODULE.bazel.lock`, read and written like Bazel 9.3.
//!
//! Reading follows BazelLockFileFunction.java:246-356: a missing file and,
//! outside `--lockfile_mode=error`, a file of another `lockFileVersion` are
//! the empty lockfile; malformed files are an error. Writing follows
//! BazelLockFileModule.java:148-189 and 336-356: Gson pretty printing
//! (GsonTypeAdapterUtil.java:401-405, `disableHtmlEscaping`) followed by a
//! newline, only when the new value differs from the value read.
//!
//! `registryFileHashes` and `selectedYankedVersions` are computed by native
//! resolution; `moduleExtensions`, `facts` and `factsVersions` by module
//! extension evaluation and lockfile reuse (crate::lockfile_extensions).
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// BazelLockFileValue.java:50.
pub const LOCK_FILE_VERSION: i64 = 28;
/// LabelConstants.MODULE_LOCKFILE_NAME.
pub const LOCKFILE_NAME: &str = "MODULE.bazel.lock";

/// RepositoryOptions.LockfileMode (RepositoryOptions.java:443-455); default
/// `update` (:359-372).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockfileMode {
    Off,
    #[default]
    Update,
    Refresh,
    Error,
}

impl LockfileMode {
    /// EnumConverter (case-insensitive), with Bazel's option-parsing message.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "update" => Ok(Self::Update),
            "refresh" => Ok(Self::Refresh),
            "error" => Ok(Self::Error),
            _ => Err(format!(
                "While parsing option --lockfile_mode={value}: Not a valid Lockfile mode: '{value}' (should be off, update, refresh or error)"
            )),
        }
    }

    /// BazelLockFileModule.ENABLED_IN_MODES (:53-54).
    pub fn writes(self) -> bool {
        matches!(self, Self::Update | Self::Refresh)
    }
}

/// A JSON value that keeps number spellings and member order, so preserved
/// lockfile sections serialize exactly as Gson re-serializes them.
#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl PartialEq for Json {
    /// Java value equality: maps compare without order, lists with order.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number(a), Self::Number(b)) | (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => members_eq(a, b),
            _ => false,
        }
    }
}

fn members_eq(a: &[(String, Json)], b: &[(String, Json)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(key, value)| b.iter().any(|(k, v)| k == key && v == value))
}

/// The value of a lockfile, BazelLockFileValue.java:119-154.
#[derive(Clone, Debug)]
pub struct Lockfile {
    pub version: i64,
    /// URL -> lowercase SHA-256 hex, or None for Bazel's "not found".
    pub registry_file_hashes: BTreeMap<String, Option<String>>,
    /// ModuleKey string ("name@version") -> yanked info, in map order.
    pub selected_yanked_versions: Vec<(String, String)>,
    /// Extension ID -> eval factors -> LockFileModuleExtension, as read (number
    /// spellings and member order kept) or computed (crate::lockfile_extensions).
    pub module_extensions: Vec<(String, Json)>,
    pub facts: Vec<(String, Json)>,
    pub facts_versions: Vec<(String, Json)>,
}

impl Default for Lockfile {
    /// BazelLockFileValue.EMPTY_LOCKFILE (:53, builder defaults :109-117).
    fn default() -> Self {
        Self {
            version: LOCK_FILE_VERSION,
            registry_file_hashes: BTreeMap::new(),
            selected_yanked_versions: Vec::new(),
            module_extensions: Vec::new(),
            facts: Vec::new(),
            facts_versions: Vec::new(),
        }
    }
}

impl PartialEq for Lockfile {
    /// AutoValue equality of BazelLockFileValue (ImmutableMap equality).
    fn eq(&self, other: &Self) -> bool {
        let yanked = |list: &[(String, String)]| list.iter().cloned().collect::<BTreeMap<_, _>>();
        self.version == other.version
            && self.registry_file_hashes == other.registry_file_hashes
            && yanked(&self.selected_yanked_versions) == yanked(&other.selected_yanked_versions)
            && members_eq(&self.module_extensions, &other.module_extensions)
            && members_eq(&self.facts, &other.facts)
            && members_eq(&self.facts_versions, &other.facts_versions)
    }
}

const NOT_FOUND_MARKER: &str = "not found";

impl Lockfile {
    /// Keys of `facts` entries, for the native facts guard.
    pub fn has_facts_for(&self, extension_id: &str) -> bool {
        self.facts
            .iter()
            .any(|(key, _)| key == extension_id || key.starts_with(&format!("{extension_id}%")))
    }

    pub fn has_extension_entry_for(&self, extension_id: &str) -> bool {
        self.module_extensions
            .iter()
            .any(|(key, _)| key == extension_id || key.starts_with(&format!("{extension_id}%")))
    }

    /// BazelLockFileModule.afterCommand's pruning without new results:
    /// combineModuleExtensions keeps entries of extensions that still have
    /// usages (:270-280); filterRelevantFacts keeps facts of used extensions
    /// that are not Facts.EMPTY (:226-235); filterRelevantFactsVersions keeps
    /// non-zero versions of kept facts (:237-249). File order is kept (Bazel
    /// sorts by ModuleExtensionId.LEXICOGRAPHIC_COMPARATOR, the order a
    /// Bazel-written file already has).
    pub fn prune_extensions(&mut self, has_usages: impl Fn(&str) -> bool) {
        self.module_extensions.retain(|(id, _)| has_usages(id));
        self.facts.retain(|(id, facts)| {
            has_usages(id) && !matches!(facts, Json::Object(members) if members.is_empty())
        });
        let facts = &self.facts;
        self.facts_versions.retain(|(id, version)| {
            facts.iter().any(|(kept, _)| kept == id)
                && !matches!(version, Json::Number(text) if text.parse::<i64>() == Ok(0))
        });
    }

    /// Gson pretty JSON of BazelLockFileValue plus the trailing newline
    /// (BazelLockFileModule.updateLockfile, :336-356).
    pub fn serialize(&self) -> String {
        let mut hashes: Vec<_> = self.registry_file_hashes.iter().collect();
        // ImmutableSortedMap.copyOf: natural java.lang.String (UTF-16) order.
        hashes.sort_by(|a, b| java_string_cmp(a.0, b.0));
        let top = Json::Object(vec![
            (
                "lockFileVersion".into(),
                Json::Number(self.version.to_string()),
            ),
            (
                "registryFileHashes".into(),
                Json::Object(
                    hashes
                        .into_iter()
                        .map(|(url, hash)| {
                            let value = hash.clone().unwrap_or_else(|| NOT_FOUND_MARKER.into());
                            (url.clone(), Json::String(value))
                        })
                        .collect(),
                ),
            ),
            (
                "selectedYankedVersions".into(),
                Json::Object(
                    self.selected_yanked_versions
                        .iter()
                        .map(|(key, info)| (key.clone(), Json::String(info.clone())))
                        .collect(),
                ),
            ),
            (
                "moduleExtensions".into(),
                Json::Object(self.module_extensions.clone()),
            ),
            // FactsAdapter.write: Gson drops object members whose value is null.
            (
                "facts".into(),
                Json::Object(
                    self.facts
                        .iter()
                        .map(|(id, facts)| {
                            (
                                id.clone(),
                                crate::lockfile_extensions::drop_null_members(facts),
                            )
                        })
                        .collect(),
                ),
            ),
            (
                "factsVersions".into(),
                Json::Object(self.facts_versions.clone()),
            ),
        ]);
        let mut out = String::new();
        write_pretty(&mut out, &top, 0);
        out.push('\n');
        out
    }

    /// BazelLockFileFunction.getLockfileValue (:298-337) on file contents.
    /// Errors are complete user messages.
    pub fn parse(text: &str, mode: LockfileMode) -> Result<Self, String> {
        let unsupported = || {
            "The version of MODULE.bazel.lock is not supported by this version of Bazel. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile.".to_owned()
        };
        let version = match find_version(text) {
            Some(digits) => digits
                .parse::<i32>()
                .map(i64::from)
                .map_err(|_| bad_lockfile(&format!("For input string: \"{digits}\"")))?,
            None => -1,
        };
        if version != LOCK_FILE_VERSION {
            // "This is an old version, its information can't be used."
            return if mode == LockfileMode::Error {
                Err(unsupported())
            } else {
                Ok(Self::default())
            };
        }
        let value = parse_json(text).map_err(|error| bad_lockfile(&error))?;
        let lockfile = Self::from_json(value).map_err(|error| bad_lockfile(&error))?;
        match lockfile {
            Some(lockfile) => Ok(lockfile),
            // isValidLockfile (:339-356) failed.
            None if mode == LockfileMode::Error => Err(unsupported()),
            None => Ok(Self::default()),
        }
    }

    /// Ok(None) is a lockfile that fails BazelLockFileFunction.isValidLockfile.
    fn from_json(value: Json) -> Result<Option<Self>, String> {
        let Json::Object(members) = value else {
            return Err(format!(
                "java.lang.IllegalStateException: Expected BEGIN_OBJECT but was {}",
                kind(&value)
            ));
        };
        let mut lockfile = Self::default();
        let mut extensions_valid = true;
        // Generated AutoValue adapters keep the last duplicate property and
        // skip unknown ones.
        for (name, value) in members {
            match name.as_str() {
                "lockFileVersion" => match &value {
                    Json::Number(text) => {
                        lockfile.version = text.parse::<i32>().map(i64::from).map_err(|_| {
                            format!("java.lang.NumberFormatException: For input string: \"{text}\"")
                        })?;
                    }
                    other => return Err(format!("Expected an int but was {}", kind(other))),
                },
                "registryFileHashes" => {
                    let mut hashes = BTreeMap::new();
                    for (url, hash) in map_members(value, "registryFileHashes")? {
                        let Json::String(hash) = hash else {
                            return Err(format!("Expected a string but was {}", kind(&hash)));
                        };
                        let hash = if hash == NOT_FOUND_MARKER {
                            None
                        } else if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                            Some(hash.to_ascii_lowercase())
                        } else {
                            return Err(format!("Invalid checksum: {hash}"));
                        };
                        hashes.insert(url, hash);
                    }
                    lockfile.registry_file_hashes = hashes;
                }
                "selectedYankedVersions" => {
                    let mut yanked = Vec::new();
                    for (key, info) in map_members(value, "selectedYankedVersions")? {
                        if key != "<root>" && !key.contains('@') {
                            return Err(format!(
                                "Unable to parse ModuleKey {key} version from the lockfile"
                            ));
                        }
                        let Json::String(info) = info else {
                            return Err(format!("Expected a string but was {}", kind(&info)));
                        };
                        yanked.push((key, info));
                    }
                    lockfile.selected_yanked_versions = yanked;
                }
                "moduleExtensions" => {
                    if matches!(value, Json::Null) {
                        extensions_valid = false;
                        continue;
                    }
                    let members = map_members(value, "moduleExtensions")?;
                    extensions_valid = members
                        .iter()
                        .all(|(_, per_factors)| valid_extension(per_factors));
                    lockfile.module_extensions = members;
                }
                "facts" => lockfile.facts = map_members(value, "facts")?,
                "factsVersions" => {
                    let members = map_members(value, "factsVersions")?;
                    for (_, version) in &members {
                        if !matches!(version, Json::Number(text) if text.parse::<i32>().is_ok()) {
                            return Err(format!("Expected an int but was {}", kind(version)));
                        }
                    }
                    lockfile.facts_versions = members;
                }
                _ => {}
            }
        }
        Ok(extensions_valid.then_some(lockfile))
    }
}

/// Map adapters reject duplicate keys (Gson MapTypeAdapterFactory); a null
/// map reaches the AutoValue builder's null check.
fn map_members(value: Json, field: &str) -> Result<Vec<(String, Json)>, String> {
    match value {
        Json::Object(members) => {
            let mut seen = std::collections::BTreeSet::new();
            for (key, _) in &members {
                if !seen.insert(key.clone()) {
                    return Err(format!("duplicate key: {key}"));
                }
            }
            Ok(members)
        }
        Json::Null => Err(format!("Null {field}")),
        other => Err(format!(
            "java.lang.IllegalStateException: Expected BEGIN_OBJECT but was {}",
            kind(&other)
        )),
    }
}

/// isValidLockfile: every extension entry has generatedRepoSpecs, each with
/// a repoRuleId.
fn valid_extension(per_factors: &Json) -> bool {
    let Json::Object(per_factors) = per_factors else {
        return false;
    };
    per_factors.iter().all(|(_, extension)| {
        let Json::Object(fields) = extension else {
            return false;
        };
        match fields
            .iter()
            .rev()
            .find(|(name, _)| name == "generatedRepoSpecs")
        {
            Some((_, Json::Object(specs))) => specs.iter().all(|(_, spec)| {
                matches!(spec, Json::Object(fields)
                    if fields.iter().rev().find(|(n, _)| n == "repoRuleId")
                        .is_some_and(|(_, id)| !matches!(id, Json::Null)))
            }),
            _ => false,
        }
    })
}

fn kind(value: &Json) -> &'static str {
    match value {
        Json::Null => "NULL",
        Json::Bool(_) => "BOOLEAN",
        Json::Number(_) => "NUMBER",
        Json::String(_) => "STRING",
        Json::Array(_) => "BEGIN_ARRAY",
        Json::Object(_) => "BEGIN_OBJECT",
    }
}

/// BazelLockFileFunction.java:279-294.
fn bad_lockfile(message: &str) -> String {
    let conflict = ["<<<<<<<", "=======", "|||||||", ">>>>>>>"]
        .iter()
        .any(|marker| message.contains(marker));
    let suffix = if conflict {
        " This looks like a merge conflict. See https://bazel.build/external/lockfile#merge-conflicts for advice."
    } else {
        " Try deleting it and rerun the build."
    };
    format!("Failed to read and parse the MODULE.bazel.lock file with error: {message}.{suffix}")
}

/// LOCKFILE_VERSION_PATTERN `"lockFileVersion":\s*(\d+)` (:232-233), first match.
fn find_version(text: &str) -> Option<&str> {
    let needle = "\"lockFileVersion\":";
    let mut start = 0;
    while let Some(found) = text[start..].find(needle) {
        let after = start + found + needle.len();
        let rest = &text[after..];
        // Java \s: [ \t\n\x0B\f\r].
        let trimmed = rest.trim_start_matches([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']);
        let digits = trimmed.len()
            - trimmed
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        if digits > 0 {
            return Some(&trimmed[..digits]);
        }
        start = after;
    }
    None
}

/// java.lang.String.compareTo: UTF-16 code unit order.
pub fn java_string_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Read the lockfile at `path` in `mode` (never called for `off`).
pub fn read(path: &Path, mode: LockfileMode) -> Result<Lockfile, String> {
    match crate::observed_fs::read(path) {
        Ok(bytes) => {
            // FileSystemUtils.readContent(UTF_8) decodes with replacement.
            let text = String::from_utf8_lossy(&bytes);
            Lockfile::parse(&text, mode)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Lockfile::default()),
        Err(error) => Err(bad_lockfile(&format!("{}: {error}", path.display()))),
    }
}

// ---- JSON syntax ----------------------------------------------------------

/// Strict RFC 8259 parsing (Gson's lenient extensions are not accepted).
pub fn parse_json(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        let consumed = &self.text[..self.pos.min(self.text.len())];
        let line = consumed.matches('\n').count() + 1;
        let column = consumed
            .rsplit('\n')
            .next()
            .map_or(0, |l| l.chars().count())
            + 1;
        let near: String = self.text[self.pos.min(self.text.len())..]
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        format!("malformed JSON ({what}) at line {line} column {column} near `{near}`")
    }

    fn skip_ws(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.bytes.get(self.pos) {
            Some(b'{') => self.container(true),
            Some(b'[') => self.container(false),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let begin = p.pos;
            while p.bytes.get(p.pos).is_some_and(u8::is_ascii_digit) {
                p.pos += 1;
            }
            p.pos - begin
        };
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let first = self.bytes.get(self.pos).copied();
        let count = digits(self);
        if count == 0 || (first == Some(b'0') && count > 1) {
            return Err(self.error("invalid number"));
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if digits(self) == 0 {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return Err(self.error("invalid number"));
            }
        }
        Ok(Json::Number(self.text[start..self.pos].to_owned()))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("invalid unicode escape"))?;
        self.pos += 4;
        Ok(u32::from_str_radix(digits, 16).expect("hex digits"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(self.error("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let escape = self.bytes.get(self.pos).copied();
                    self.pos += 1;
                    match escape {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if !self.text[self.pos..].starts_with("\\u") {
                                    return Err(self.error("unpaired surrogate"));
                                }
                                self.pos += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.error("unpaired surrogate"));
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                high
                            };
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| self.error("unpaired surrogate"))?,
                            );
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                0..=0x1f => return Err(self.error("control character in string")),
                _ => {
                    let ch = self.text[self.pos..].chars().next().expect("char boundary");
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn container(&mut self, object: bool) -> Result<Json, String> {
        self.depth += 1;
        if self.depth > 512 {
            return Err(self.error("nesting too deep"));
        }
        let close = if object { b'}' } else { b']' };
        self.pos += 1;
        let mut members = Vec::new();
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&close) {
            self.pos += 1;
        } else {
            loop {
                self.skip_ws();
                if object {
                    if self.bytes.get(self.pos) != Some(&b'"') {
                        return Err(self.error("expected name"));
                    }
                    let name = self.string()?;
                    self.skip_ws();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return Err(self.error("expected ':'"));
                    }
                    self.pos += 1;
                    self.skip_ws();
                    members.push((name, self.value()?));
                } else {
                    items.push(self.value()?);
                }
                self.skip_ws();
                match self.bytes.get(self.pos) {
                    Some(b',') => self.pos += 1,
                    Some(&b) if b == close => {
                        self.pos += 1;
                        break;
                    }
                    _ => {
                        return Err(self.error(if object {
                            "unterminated object"
                        } else {
                            "unterminated array"
                        }));
                    }
                }
            }
        }
        self.depth -= 1;
        Ok(if object {
            Json::Object(members)
        } else {
            Json::Array(items)
        })
    }
}

/// Gson JsonWriter string escaping without HTML escaping.
pub(crate) fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Gson pretty printing: two-space indent, `": "`, empty containers `{}`/`[]`.
pub(crate) fn write_pretty(out: &mut String, value: &Json, depth: usize) {
    let indent = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => out.push_str(n),
        Json::String(s) => write_string(out, s),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                write_pretty(out, item, depth + 1);
            }
            if !items.is_empty() {
                indent(out, depth);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (index, (name, item)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                write_string(out, name);
                out.push_str(": ");
                write_pretty(out, item, depth + 1);
            }
            if !members.is_empty() {
                indent(out, depth);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lockfile_serializes_like_gson_pretty_printing() {
        assert_eq!(
            Lockfile::default().serialize(),
            "{\n  \"lockFileVersion\": 28,\n  \"registryFileHashes\": {},\n  \"selectedYankedVersions\": {},\n  \"moduleExtensions\": {},\n  \"facts\": {},\n  \"factsVersions\": {}\n}\n"
        );
    }

    #[test]
    fn hashes_sort_and_not_found_marker_round_trip() {
        let mut lockfile = Lockfile::default();
        lockfile.registry_file_hashes.insert(
            "https://r/modules/b/1/MODULE.bazel".into(),
            Some("ab".repeat(32)),
        );
        lockfile
            .registry_file_hashes
            .insert("https://r/bazel_registry.json".into(), None);
        lockfile
            .selected_yanked_versions
            .push(("b@1".into(), "bad \"x\"\n".into()));
        let text = lockfile.serialize();
        assert!(text.contains("\"https://r/bazel_registry.json\": \"not found\",\n    \"https://r/modules/b/1/MODULE.bazel\""));
        assert!(text.contains("\"b@1\": \"bad \\\"x\\\"\\n\""));
        let parsed = Lockfile::parse(&text, LockfileMode::Error).unwrap();
        assert_eq!(parsed, lockfile);
        assert_eq!(parsed.serialize(), text);
    }

    #[test]
    fn preserved_sections_keep_spelling_order_and_unicode() {
        let text = "{\n  \"lockFileVersion\": 28,\n  \"moduleExtensions\": {\n    \"//:e.bzl%e\": {\n      \"general\": {\n        \"generatedRepoSpecs\": {\n          \"r\": {\n            \"repoRuleId\": \"@@x//:y.bzl%z\",\n            \"attributes\": {\n              \"n\": 1.50,\n              \"s\": \"\u{e9}<>&=\\u0001\",\n              \"l\": []\n            }\n          }\n        }\n      }\n    }\n  },\n  \"facts\": {\n    \"//:e.bzl%e\": {\n      \"k\": [\n        1,\n        2\n      ]\n    }\n  },\n  \"unknown\": 1\n}";
        let parsed = Lockfile::parse(text, LockfileMode::Update).unwrap();
        let written = parsed.serialize();
        assert!(written.contains("\"n\": 1.50"), "{written}");
        assert!(
            written.contains("\"s\": \"\u{e9}<>&=\\u0001\""),
            "{written}"
        );
        assert!(written.contains("\"l\": []"));
        assert!(!written.contains("unknown"));
        assert!(parsed.has_facts_for("//:e.bzl%e"));
        assert!(parsed.has_extension_entry_for("//:e.bzl%e"));
        assert_eq!(
            Lockfile::parse(&written, LockfileMode::Update).unwrap(),
            parsed
        );
    }

    #[test]
    fn pruning_keeps_used_extensions_nonempty_facts_and_nonzero_versions() {
        let text = r#"{"lockFileVersion": 28,
          "moduleExtensions": {
            "//:a.bzl%a": {"general": {"generatedRepoSpecs": {}}},
            "//:gone.bzl%g": {"general": {"generatedRepoSpecs": {}}},
            "//:i.bzl%i%<root>+proxy": {"general": {"generatedRepoSpecs": {}}}},
          "facts": {"//:a.bzl%a": {"k": 1}, "//:b.bzl%b": {}, "//:gone.bzl%g": {"k": 2}, "//:c.bzl%c": {"x": []}},
          "factsVersions": {"//:a.bzl%a": 2, "//:b.bzl%b": 3, "//:c.bzl%c": 0, "//:gone.bzl%g": 1}}"#;
        let mut lockfile = Lockfile::parse(text, LockfileMode::Update).unwrap();
        lockfile.prune_extensions(|id| {
            ["//:a.bzl%a", "//:b.bzl%b", "//:c.bzl%c"].contains(&id)
                || id.starts_with("//:i.bzl%i%<root>+")
        });
        let ids = |members: &[(String, Json)]| {
            members.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&lockfile.module_extensions),
            ["//:a.bzl%a", "//:i.bzl%i%<root>+proxy"]
        );
        assert_eq!(ids(&lockfile.facts), ["//:a.bzl%a", "//:c.bzl%c"]);
        assert_eq!(ids(&lockfile.facts_versions), ["//:a.bzl%a"]);
    }

    #[test]
    fn version_mismatch_is_empty_except_in_error_mode() {
        for text in ["{\"lockFileVersion\": 26}", "not json at all", "{}"] {
            assert_eq!(
                Lockfile::parse(text, LockfileMode::Update).unwrap(),
                Lockfile::default()
            );
            assert_eq!(
                Lockfile::parse(text, LockfileMode::Error).unwrap_err(),
                "The version of MODULE.bazel.lock is not supported by this version of Bazel. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
            );
        }
    }

    #[test]
    fn invalid_extension_entries_make_the_lockfile_empty() {
        let text = r#"{"lockFileVersion": 28, "registryFileHashes": {"u": "not found"}, "moduleExtensions": {"//:e.bzl%e": {"general": {}}}}"#;
        assert_eq!(
            Lockfile::parse(text, LockfileMode::Update).unwrap(),
            Lockfile::default()
        );
        assert!(Lockfile::parse(text, LockfileMode::Error).is_err());
    }

    #[test]
    fn malformed_current_version_lockfiles_are_errors_with_bazel_suffixes() {
        let error = Lockfile::parse("{\"lockFileVersion\": 28,", LockfileMode::Update).unwrap_err();
        assert!(
            error.starts_with("Failed to read and parse the MODULE.bazel.lock file with error: ")
        );
        assert!(
            error.ends_with(". Try deleting it and rerun the build."),
            "{error}"
        );
        let conflict = "{\"lockFileVersion\": 28,\n<<<<<<< HEAD\n}";
        let error = Lockfile::parse(conflict, LockfileMode::Update).unwrap_err();
        assert!(error.ends_with("This looks like a merge conflict. See https://bazel.build/external/lockfile#merge-conflicts for advice."), "{error}");
        let error = Lockfile::parse(
            r#"{"lockFileVersion": 28, "registryFileHashes": {"u": "xyz"}}"#,
            LockfileMode::Update,
        )
        .unwrap_err();
        assert!(error.contains("error: Invalid checksum: xyz."), "{error}");
        let error = Lockfile::parse(
            r#"{"lockFileVersion": 28, "registryFileHashes": {"u": "not found", "u": "not found"}}"#,
            LockfileMode::Update,
        )
        .unwrap_err();
        assert!(error.contains("duplicate key: u"), "{error}");
    }

    #[test]
    fn uppercase_hashes_are_equal_to_lowercase_ones() {
        let upper = format!(
            r#"{{"lockFileVersion": 28, "registryFileHashes": {{"u": "{}"}}}}"#,
            "AB".repeat(32)
        );
        let lower = upper
            .to_ascii_lowercase()
            .replace("lockfileversion", "lockFileVersion")
            .replace("registryfilehashes", "registryFileHashes");
        assert_eq!(
            Lockfile::parse(&upper, LockfileMode::Update).unwrap(),
            Lockfile::parse(&lower, LockfileMode::Update).unwrap()
        );
    }

    #[test]
    fn lockfile_modes_parse_case_insensitively_with_bazel_errors() {
        assert_eq!(
            LockfileMode::parse("REFRESH").unwrap(),
            LockfileMode::Refresh
        );
        assert_eq!(
            LockfileMode::parse("x").unwrap_err(),
            "While parsing option --lockfile_mode=x: Not a valid Lockfile mode: 'x' (should be off, update, refresh or error)"
        );
    }
}
