//! MODULE.bazel.lock `moduleExtensions` entries and `facts`, as Bazel 9.3
//! writes, validates and reuses them. Paths are relative to
//! `src/main/java/com/google/devtools/build/lib/` of the pinned source.
//!
//! - An entry is LockFileModuleExtension (bazel/bzlmod/LockFileModuleExtension.java:30-73)
//!   keyed by ModuleExtensionEvalFactors (`general` or `os:OS,arch:ARCH`,
//!   bazel/bzlmod/ModuleExtensionEvalFactors.java). Gson writes its fields as
//!   `bzlTransitiveDigest`, `usagesDigest` (base64, GsonTypeAdapterUtil.java:258-269),
//!   `recordedInputs` (RepoRecordedInput.WithValue strings), `generatedRepoSpecs`
//!   (RepoSpec: `repoRuleId`, `attributes`) and an optional `moduleExtensionMetadata`.
//! - `bzlTransitiveDigest` is BzlLoadValue's transitive digest for a Bzlmod
//!   key: SHA-256 over the file's SHA-256 followed by the transitive digests of
//!   its loads in program order (skyframe/BzlLoadFunction.java:858-879; no
//!   builtins digest for Bzlmod keys, :1350-1365).
//! - `usagesDigest` is SingleExtensionUsagesValue.hashForEvaluation
//!   (bazel/bzlmod/SingleExtensionUsagesValue.java:78-99): SHA-256 of the UTF-16LE
//!   code units (Guava hashUnencodedChars) of the compact Gson JSON of the
//!   trimmed usages value.
use crate::extension_calls::RecordedValue;
use crate::extension_metadata::{ExtensionMetadata, UseAllRepos};
use crate::label::Label;
use crate::lockfile::{Json, java_string_cmp, write_string};
use crate::repository_catalog::OrdinaryGroup;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

/// RepoRule.LEGACY_BUILTIN_ATTRIBUTES (bazel/repository/RepoRule.java:71-72):
/// never stored in a RepoSpec.
pub(crate) const LEGACY_BUILTIN_ATTRIBUTES: [&str; 4] =
    ["name", "tags", "deprecation", "visibility"];

// ---- Gson compact output ---------------------------------------------------

/// `new GsonBuilder().disableHtmlEscaping().create().toJson(...)`: no
/// whitespace; a member whose value is null is skipped (serializeNulls is
/// false, JsonWriter.nullValue drops the deferred name), null array elements
/// are kept.
pub(crate) fn gson_compact(value: &Json) -> String {
    let mut out = String::new();
    write_compact(&mut out, value);
    out
}

fn write_compact(out: &mut String, value: &Json) {
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
                write_compact(out, item);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            let mut first = true;
            for (name, item) in members {
                if matches!(item, Json::Null) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                write_string(out, name);
                out.push(':');
                write_compact(out, item);
            }
            out.push('}');
        }
    }
}

/// Object members with null values removed recursively, as any Gson writer
/// with serializeNulls=false emits them (FactsAdapter.write, AttributeValuesAdapter.write).
pub(crate) fn drop_null_members(value: &Json) -> Json {
    match value {
        Json::Array(items) => Json::Array(items.iter().map(drop_null_members).collect()),
        Json::Object(members) => Json::Object(
            members
                .iter()
                .filter(|(_, item)| !matches!(item, Json::Null))
                .map(|(name, item)| (name.clone(), drop_null_members(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---- Digests ---------------------------------------------------------------

/// SingleExtensionUsagesValue.hashForEvaluation: base64 SHA-256 of the
/// UTF-16LE code units of the compact JSON.
pub(crate) fn usages_digest(value: &Json) -> String {
    let text = gson_compact(value);
    let mut hash = Sha256::new();
    for unit in text.encode_utf16() {
        hash.update(unit.to_le_bytes());
    }
    base64(&hash.finalize())
}

/// BazelModuleContext.bzlTransitiveDigest of `root` from the loader's
/// captured sources and load graph.
pub(crate) fn bzl_transitive_digest(
    root: &Label,
    load_graph: &HashMap<Label, Vec<Label>>,
    sources: &BTreeMap<String, crate::loading::SourceProvenance>,
) -> Result<String, String> {
    fn visit(
        label: &Label,
        load_graph: &HashMap<Label, Vec<Label>>,
        sources: &BTreeMap<String, crate::loading::SourceProvenance>,
        memo: &mut HashMap<Label, [u8; 32]>,
    ) -> Result<[u8; 32], String> {
        if let Some(digest) = memo.get(label) {
            return Ok(*digest);
        }
        let source = sources
            .get(&label.to_string())
            .ok_or_else(|| format!("internal error: no captured source for {label}"))?;
        let mut fingerprint = Sha256::new();
        fingerprint.update(Sha256::digest(&source.bytes));
        for load in load_graph.get(label).map(Vec::as_slice).unwrap_or_default() {
            fingerprint.update(visit(load, load_graph, sources, memo)?);
        }
        let digest: [u8; 32] = fingerprint.finalize().into();
        memo.insert(label.clone(), digest);
        Ok(digest)
    }
    Ok(base64(&visit(
        root,
        load_graph,
        sources,
        &mut HashMap::new(),
    )?))
}

// ---- Usages value ----------------------------------------------------------

/// AttributeValuesAdapter.serializeObjToString for a String
/// (bazel/bzlmod/AttributeValuesAdapter.java:145-165): strings that would read
/// back as a Label or as an escaped string are wrapped in single quotes.
pub(crate) fn escape_attribute_string(text: &str) -> String {
    if text.starts_with("@@") || (text.starts_with('\'') && text.ends_with('\'')) {
        format!("'{text}'")
    } else {
        text.to_owned()
    }
}

/// A MODULE.bazel tag value (owned JSON of the kwargs value) as
/// AttributeValuesAdapter.serializeObject writes it (:58-90). Floats, ints
/// outside 32 bits and non-string dict keys make Bazel throw; they fail here.
fn tag_value_json(value: &serde_json::Value) -> Result<Json, String> {
    Ok(match value {
        serde_json::Value::Null => Json::Null,
        serde_json::Value::Bool(b) => Json::Bool(*b),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(int) if i32::try_from(int).is_ok() => Json::Number(int.to_string()),
            _ => {
                return Err(format!(
                    "the tag value {number} cannot be serialized into MODULE.bazel.lock (Bazel's AttributeValuesAdapter supports only 32-bit ints)"
                ));
            }
        },
        serde_json::Value::String(text) => Json::String(escape_attribute_string(text)),
        serde_json::Value::Array(items) => {
            Json::Array(items.iter().map(tag_value_json).collect::<Result<_, _>>()?)
        }
        serde_json::Value::Object(members) => {
            if members.contains_key(crate::repository_rule::KEY_REPR) {
                return Err(
                    "a tag dict with a non-string key cannot be serialized into MODULE.bazel.lock"
                        .to_owned(),
                );
            }
            Json::Object(
                members
                    .iter()
                    .map(|(key, item)| Ok((escape_attribute_string(key), tag_value_json(item)?)))
                    .collect::<Result<_, String>>()?,
            )
        }
    })
}

/// SingleExtensionUsagesValue.trimForEvaluation of `group`, as a Gson object
/// (field order of the generated AutoValue adapters): per module key the
/// usage without proxies and repo overrides, tags at Location.BUILTIN.
pub(crate) fn usages_value(group: &OrdinaryGroup) -> Result<Json, String> {
    let builtin = Json::Object(vec![
        ("file".into(), Json::String("<builtin>".into())),
        ("line".into(), Json::Number("0".into())),
        ("column".into(), Json::Number("0".into())),
    ]);
    let mut usages = Vec::new();
    let mut modules = Vec::new();
    for participant in &group.participants {
        let mut tags: Vec<_> = participant.tags.iter().collect();
        tags.sort_by_key(|tag| tag.ordinal);
        let tags = tags
            .into_iter()
            .map(|tag| {
                let mut attributes = Vec::new();
                for name in &tag.attribute_order {
                    if let Some(value) = tag.attributes.get(name) {
                        attributes.push((name.clone(), tag_value_json(value)?));
                    }
                }
                // Attributes retained without a recorded order (not expected).
                for (name, value) in &tag.attributes {
                    if !tag.attribute_order.contains(name) {
                        attributes.push((name.clone(), tag_value_json(value)?));
                    }
                }
                Ok(Json::Object(vec![
                    ("tagName".into(), Json::String(tag.name.clone())),
                    ("attributeValues".into(), Json::Object(attributes)),
                    ("devDependency".into(), Json::Bool(tag.dev_dependency)),
                    ("location".into(), builtin.clone()),
                ]))
            })
            .collect::<Result<Vec<_>, String>>()?;
        usages.push((
            participant.module_key.clone(),
            Json::Object(vec![
                (
                    "extensionBzlFile".into(),
                    Json::String(participant.extension_bzl_file.clone()),
                ),
                ("extensionName".into(), Json::String(group.symbol.clone())),
                ("proxies".into(), Json::Array(Vec::new())),
                ("tags".into(), Json::Array(tags)),
                ("repoOverrides".into(), Json::Object(Vec::new())),
            ]),
        ));
        modules.push(Json::Object(vec![
            ("name".into(), Json::String(participant.name.clone())),
            ("version".into(), Json::String(participant.version.clone())),
            ("key".into(), Json::String(participant.module_key.clone())),
        ]));
    }
    Ok(Json::Object(vec![
        ("extensionUsages".into(), Json::Object(usages)),
        (
            "extensionUniqueName".into(),
            Json::String(group.prefix.clone()),
        ),
        ("abridgedModules".into(), Json::Array(modules)),
        ("repoMappings".into(), Json::Object(Vec::new())),
        // Native MODULE evaluation rejects the root module's override_repo/inject_repo
        // and ignores them elsewhere, as Bazel does.
        ("repoOverrides".into(), Json::Object(Vec::new())),
    ]))
}

// ---- Recorded inputs -------------------------------------------------------

/// RepoRecordedInput.escape (rules/repository/RepoRecordedInput.java:226-229).
pub(crate) fn escape(text: Option<&str>) -> String {
    match text {
        None => "\\0".to_owned(),
        Some(text) => text
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace(' ', "\\s"),
    }
}

/// RepoRecordedInput.unescape (:231-258).
pub(crate) fn unescape(text: &str) -> Option<String> {
    if text == "\\0" {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                's' => ' ',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// One RepoRecordedInput.WithValue: `key` is `PREFIX:` plus the escaped
/// input (`RepoRecordedInput.toString`, :272-274).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedInput {
    pub key: String,
    pub value: Option<String>,
}

impl RecordedInput {
    pub(crate) fn file(path: &str, value: String) -> Self {
        Self {
            key: format!("FILE:{}", escape(Some(path))),
            value: Some(value),
        }
    }

    pub(crate) fn dirents(path: &str, value: String) -> Self {
        Self {
            key: format!("DIRENTS:{}", escape(Some(path))),
            value: Some(value),
        }
    }

    pub(crate) fn env(name: &str, value: Option<String>) -> Self {
        Self {
            key: format!("ENV:{}", escape(Some(name))),
            value,
        }
    }

    pub(crate) fn repo_mapping(from: &str, apparent: &str, canonical: Option<String>) -> Self {
        Self {
            key: format!(
                "REPO_MAPPING:{}",
                escape(Some(&format!("{from},{apparent}")))
            ),
            value: canonical,
        }
    }

    /// RepoRecordedInput.WithValue.toString (:153-157).
    pub(crate) fn lockfile_string(&self) -> String {
        format!("{} {}", self.key, escape(self.value.as_deref()))
    }
}

/// StarlarkBaseExternalContext.recordInputWithValue: the first value recorded
/// for an input wins; later observations of the same input are dropped.
pub(crate) fn push_recorded(list: &mut Vec<RecordedInput>, input: RecordedInput) {
    if !list.iter().any(|known| known.key == input.key) {
        list.push(input);
    }
}

/// RepoRecordedInput.File.fileValueToMarkerValue (:463-480) from the native
/// file state (`missing`, `directory`, `file:<sha256>`).
pub(crate) fn file_marker_value(native_state: &str) -> Result<String, String> {
    match native_state {
        "missing" => Ok("ENOENT".to_owned()),
        "directory" => Ok("DIR".to_owned()),
        state => state
            .strip_prefix("file:")
            .map(str::to_owned)
            .ok_or_else(|| {
                "a module extension recorded a special (non-regular) file input; its MODULE.bazel.lock value is not implemented".to_owned()
            }),
    }
}

/// RepoRecordedInput.Dirents.directoryListingValueToMarkerValue (:578-587):
/// Fingerprint.addStrings of the sorted entry names (a varint count, then each
/// name as a varint UTF-8 length and its UTF-8 bytes). Names are Bazel's
/// Latin-1 internal strings of the raw bytes, so a byte >= 0x80 is encoded as
/// two UTF-8 bytes.
pub(crate) fn dirents_marker_value(names: &[Vec<u8>]) -> String {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    let mut names = names.to_vec();
    names.sort();
    let mut message = Vec::new();
    varint(&mut message, names.len() as u64);
    for name in &names {
        let mut encoded = Vec::with_capacity(name.len());
        for &byte in name {
            if byte < 0x80 {
                encoded.push(byte);
            } else {
                encoded.push(0xc0 | (byte >> 6));
                encoded.push(0x80 | (byte & 0x3f));
            }
        }
        varint(&mut message, encoded.len() as u64);
        message.extend_from_slice(&encoded);
    }
    hex(&Sha256::digest(&message))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// RepoRecordedInput.RepoCacheFriendlyPath (:313-376): a path inside the
/// workspace or an external repository, or an absolute host path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FriendlyPath {
    pub repository: Option<String>,
    pub path: String,
}

impl std::fmt::Display for FriendlyPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.repository {
            Some(repository) => write!(f, "@@{repository}//{}", self.path),
            None => f.write_str(&self.path),
        }
    }
}

impl FriendlyPath {
    /// RepoCacheFriendlyPath.parse (:341-351); None is a parse failure.
    fn parse(text: &str) -> Option<Self> {
        if text.starts_with("//") || text.starts_with('@') {
            let slash = text.find("//")?;
            let skip = if text.starts_with("@@") {
                2
            } else if text.starts_with('@') {
                1
            } else {
                0
            };
            let repository = text.get(skip..slash)?;
            // RepositoryName.create validation.
            if !repository
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'+'))
            {
                return None;
            }
            return Some(Self {
                repository: Some(repository.to_owned()),
                path: text[slash + 2..].to_owned(),
            });
        }
        text.starts_with('/').then(|| Self {
            repository: None,
            path: text.to_owned(),
        })
    }

    /// The friendly path of a file named by a canonical label
    /// (`@@foo//abc/def:ghi.bzl` is stored as `@@foo//abc/def/ghi.bzl`).
    pub(crate) fn of_label(label: &Label) -> Self {
        Self {
            repository: Some(label.repository.clone()),
            path: if label.package.is_empty() {
                label.name.clone()
            } else {
                format!("{}/{}", label.package, label.name)
            },
        }
    }
}

/// What validating recorded inputs needs from the current invocation.
pub(crate) trait InputProbe {
    /// RepoRecordedInput.File value (`DIR`, `ENOENT` or the hex digest), or
    /// the reason the file cannot be inspected.
    fn file(&self, path: &FriendlyPath) -> Result<String, String>;
    /// RepoRecordedInput.Dirents value.
    fn dirents(&self, path: &FriendlyPath) -> Result<String, String>;
    /// The repository environment value of `name`.
    fn env(&self, name: &str) -> Result<Option<String>, String>;
    /// RepositoryMapping.get(apparent).getName() in repository `from` (the
    /// apparent name itself when it is not visible), None when `from` does
    /// not exist.
    fn mapping(&self, from: &str, apparent: &str) -> Result<Option<String>, String>;
}

/// RepoRecordedInput.isAnyValueOutdated over every batch
/// (SingleExtensionEvalFunction.didRecordedInputsChange, :437-454): the
/// first outdated input in list order and its describeChange text.
pub(crate) fn outdated_input(inputs: &[Json], probe: &dyn InputProbe) -> Option<String> {
    const MALFORMED: &str = "malformed marker file entry encountered";
    for input in inputs {
        let Json::String(text) = input else {
            return Some(MALFORMED.to_owned());
        };
        // WithValue.parse (:118-130).
        let Some(space) = text.find(' ').filter(|index| *index > 0) else {
            return Some(MALFORMED.to_owned());
        };
        let Some(raw) = unescape(&text[..space]) else {
            return Some(MALFORMED.to_owned());
        };
        let old = unescape(&text[space + 1..]);
        let Some((prefix, rest)) = raw.split_once(':') else {
            return Some(MALFORMED.to_owned());
        };
        let reason = match prefix {
            "FILE" | "DIRENTS" => {
                let Some(path) = FriendlyPath::parse(rest) else {
                    return Some(MALFORMED.to_owned());
                };
                let current = if prefix == "FILE" {
                    probe.file(&path)
                } else {
                    probe.dirents(&path)
                };
                match current {
                    Err(reason) => Some(reason),
                    Ok(value) if Some(&value) == old.as_ref() => None,
                    Ok(_) if prefix == "FILE" => {
                        Some(format!("file info or contents of {path} changed"))
                    }
                    Ok(_) => Some(format!("directory entries of {path} changed")),
                }
            }
            "DIRTREE" => Some(
                "a DIRTREE recorded input (repository_ctx.watch_tree) cannot be validated for a module extension by the native profile".to_owned(),
            ),
            "ENV" => match probe.env(rest) {
                Err(reason) => Some(reason),
                Ok(value) if value == old => None,
                Ok(value) => {
                    let show = |value: &Option<String>| {
                        value
                            .as_ref()
                            .map_or_else(|| "<unset>".to_owned(), |value| format!("'{value}'"))
                    };
                    Some(format!(
                        "environment variable {rest} changed: {} -> {}",
                        show(&old),
                        show(&value)
                    ))
                }
            },
            "REPO_MAPPING" => {
                let Some((from, apparent)) = rest.split_once(',') else {
                    return Some(MALFORMED.to_owned());
                };
                match probe.mapping(from, apparent) {
                    Err(reason) => Some(reason),
                    Ok(None) => Some(format!("source repo @@{from} doesn't exist anymore")),
                    Ok(value) if value == old => None,
                    Ok(value) => {
                        let show = |value: &Option<String>| {
                            value
                                .clone()
                                .unwrap_or_else(|| "<doesn't exist>".to_owned())
                        };
                        Some(format!(
                            "canonical name for @{apparent} in @@{from} changed: {} -> {}",
                            show(&old),
                            show(&value)
                        ))
                    }
                }
            }
            _ => Some(MALFORMED.to_owned()),
        };
        if reason.is_some() {
            return reason;
        }
    }
    None
}

// ---- Repo specs ------------------------------------------------------------

/// Bazel's internalToUnicode (StringEncoding): an extension string holds the
/// Latin-1 chars of its UTF-8 bytes (crate::repository_text).
pub(crate) fn internal_to_unicode(text: &str) -> Result<String, String> {
    let bytes = crate::repository_rule::latin1_bytes(text).ok_or_else(|| {
        format!("repository attribute string {text:?} is not a Latin-1 internal string")
    })?;
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "repository attribute string {text:?} is not valid UTF-8; its MODULE.bazel.lock serialization is not implemented"
        )
    })
}

/// Label.getUnambiguousCanonicalForm of a native canonical label string.
fn unambiguous_label(text: &str) -> String {
    if text.starts_with("@@") {
        text.to_owned()
    } else {
        format!("@@{text}")
    }
}

fn attribute_string(text: &str) -> Result<Json, String> {
    Ok(Json::String(escape_attribute_string(&internal_to_unicode(
        text,
    )?)))
}

fn attribute_label(text: &str) -> Json {
    Json::String(unambiguous_label(text))
}

/// A type-checked repository rule attribute value as AttributeValuesAdapter
/// writes it: Labels in unambiguous canonical form, strings escaped.
pub(crate) fn attribute_json(value: &crate::custom_rule::AttrValue) -> Result<Json, String> {
    use crate::custom_rule::AttrValue;
    let strings = |items: &[String]| -> Result<Json, String> {
        Ok(Json::Array(
            items
                .iter()
                .map(|item| attribute_string(item))
                .collect::<Result<_, _>>()?,
        ))
    };
    let labels =
        |items: &[String]| Json::Array(items.iter().map(|item| attribute_label(item)).collect());
    Ok(match value {
        AttrValue::String(text) => attribute_string(text)?,
        AttrValue::Strings(items) => strings(items)?,
        AttrValue::StringListDict(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, items)| {
                    Ok((
                        escape_attribute_string(&internal_to_unicode(key)?),
                        strings(items)?,
                    ))
                })
                .collect::<Result<_, String>>()?,
        ),
        AttrValue::StringDict(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, item)| {
                    Ok((
                        escape_attribute_string(&internal_to_unicode(key)?),
                        attribute_string(item)?,
                    ))
                })
                .collect::<Result<_, String>>()?,
        ),
        AttrValue::Int(int) => Json::Number(int.to_string()),
        AttrValue::Bool(b) => Json::Bool(*b),
        AttrValue::Label(Some(label)) => attribute_label(label),
        AttrValue::Label(None) => Json::Null,
        AttrValue::Labels(items) => labels(items),
        AttrValue::Ints(items) => Json::Array(
            items
                .iter()
                .map(|int| Json::Number(int.to_string()))
                .collect(),
        ),
        AttrValue::LabelKeyedStringDict(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, item)| Ok((unambiguous_label(key), attribute_string(item)?)))
                .collect::<Result<_, String>>()?,
        ),
        AttrValue::StringKeyedLabelDict(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, item)| {
                    Ok((
                        escape_attribute_string(&internal_to_unicode(key)?),
                        attribute_label(item),
                    ))
                })
                .collect::<Result<_, String>>()?,
        ),
        AttrValue::LabelListDict(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, items)| {
                    Ok((
                        escape_attribute_string(&internal_to_unicode(key)?),
                        labels(items),
                    ))
                })
                .collect::<Result<_, String>>()?,
        ),
        AttrValue::Output(_) | AttrValue::Outputs(_) => {
            return Err("output attributes cannot appear in a RepoSpec".to_owned());
        }
    })
}

/// RepoRuleId.toString (bazel/bzlmod/RepoRuleId.java:31-34) of a native
/// definition identity `<bzl label>%<name>`.
pub(crate) fn repo_rule_id(identity: &str) -> Result<String, String> {
    let (label, name) = identity
        .split_once('%')
        .ok_or_else(|| format!("invalid repository rule identity {identity:?}"))?;
    Ok(format!("{}%{name}", unambiguous_label(label)))
}

/// The bzl label and exported rule name of a lockfile repoRuleId
/// (GsonTypeAdapterUtil.java:135-156).
pub(crate) fn parse_repo_rule_id(id: &str) -> Result<(Label, String), String> {
    let (label, name) = id
        .split_once('%')
        .ok_or_else(|| format!("repoRuleId {id:?} without a bzl file label is not supported"))?;
    let label = label.strip_prefix("@@").unwrap_or(label);
    let canonical = if label.starts_with("//") {
        label.to_owned()
    } else {
        format!("@@{label}")
    };
    Ok((Label::parse_canonical(&canonical)?, name.to_owned()))
}

/// AttributeValuesAdapter.deserializeStringToObject (:167-180).
fn recorded_string(text: &str) -> RecordedValue {
    if let Some(label) = text.strip_prefix("@@") {
        return RecordedValue::Label(if label.starts_with("//") {
            label.to_owned()
        } else {
            text.to_owned()
        });
    }
    let unquoted = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .filter(|_| text.len() >= 2)
        .unwrap_or(text);
    RecordedValue::String(crate::host_os::latin1_text(unquoted.as_bytes()))
}

/// AttributeValuesAdapter.deserializeObject (:93-131) as a native recorded
/// repository call value.
pub(crate) fn recorded_value(json: &Json) -> Result<RecordedValue, String> {
    Ok(match json {
        Json::Null => RecordedValue::None,
        Json::Bool(b) => RecordedValue::Bool(*b),
        Json::Number(text) => RecordedValue::Int(
            text.parse::<i32>()
                .map_err(|_| format!("lockfile attribute number {text} is not a 32-bit int"))?,
        ),
        Json::String(text) => recorded_string(text),
        Json::Array(items) => {
            RecordedValue::List(items.iter().map(recorded_value).collect::<Result<_, _>>()?)
        }
        Json::Object(members) => {
            let keys: Vec<_> = members
                .iter()
                .map(|(key, _)| recorded_string(key))
                .collect();
            let values = members
                .iter()
                .map(|(_, value)| recorded_value(value))
                .collect::<Result<Vec<_>, _>>()?;
            if keys
                .iter()
                .any(|key| matches!(key, RecordedValue::Label(_)))
            {
                RecordedValue::LabelKeyedDict(keys.into_iter().zip(values).collect())
            } else {
                RecordedValue::Dict(
                    keys.into_iter()
                        .map(|key| match key {
                            RecordedValue::String(key) => key,
                            _ => unreachable!("non-label keys are strings"),
                        })
                        .zip(values)
                        .collect(),
                )
            }
        }
    })
}

// ---- Metadata --------------------------------------------------------------

/// LockfileModuleExtensionMetadata.of (bazel/bzlmod/LockfileModuleExtensionMetadata.java:148-159):
/// absent for ModuleExtensionMetadata.DEFAULT (which includes Facts.EMPTY).
pub(crate) fn metadata_json(metadata: Option<&ExtensionMetadata>) -> Option<Json> {
    let metadata = metadata?;
    let is_default = metadata.explicit_root_module_direct_deps.is_none()
        && metadata.explicit_root_module_direct_dev_deps.is_none()
        && metadata.use_all_repos == UseAllRepos::No
        && !metadata.reproducible
        && !metadata.has_facts;
    if is_default {
        return None;
    }
    let names = |names: &[String]| Json::Array(names.iter().cloned().map(Json::String).collect());
    let mut members = Vec::new();
    if let Some(deps) = &metadata.explicit_root_module_direct_deps {
        members.push(("explicitRootModuleDirectDeps".into(), names(deps)));
    }
    if let Some(deps) = &metadata.explicit_root_module_direct_dev_deps {
        members.push(("explicitRootModuleDirectDevDeps".into(), names(deps)));
    }
    let use_all = match metadata.use_all_repos {
        UseAllRepos::No => "NO",
        UseAllRepos::Regular => "REGULAR",
        UseAllRepos::Dev => "DEV",
    };
    members.push(("useAllRepos".into(), Json::String(use_all.into())));
    members.push(("reproducible".into(), Json::Bool(metadata.reproducible)));
    Some(Json::Object(members))
}

/// The ExtensionMetadata (without facts) of a lockfile `moduleExtensionMetadata`.
pub(crate) fn metadata_from_json(json: &Json) -> Result<ExtensionMetadata, String> {
    let Json::Object(members) = json else {
        return Err("moduleExtensionMetadata is not an object".to_owned());
    };
    let field = |name: &str| {
        members
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v)
    };
    let names = |name: &str| -> Result<Option<Vec<String>>, String> {
        match field(name) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Json::String(text) => Ok(text.clone()),
                    _ => Err(format!("moduleExtensionMetadata {name} must be strings")),
                })
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(format!("moduleExtensionMetadata {name} must be a list")),
        }
    };
    let use_all_repos = match field("useAllRepos") {
        Some(Json::String(mode)) if mode == "NO" => UseAllRepos::No,
        Some(Json::String(mode)) if mode == "REGULAR" => UseAllRepos::Regular,
        Some(Json::String(mode)) if mode == "DEV" => UseAllRepos::Dev,
        _ => return Err("moduleExtensionMetadata has an invalid useAllRepos".to_owned()),
    };
    Ok(ExtensionMetadata {
        explicit_root_module_direct_deps: names("explicitRootModuleDirectDeps")?,
        explicit_root_module_direct_dev_deps: names("explicitRootModuleDirectDevDeps")?,
        use_all_repos,
        reproducible: matches!(field("reproducible"), Some(Json::Bool(true))),
        has_facts: false,
        facts: None,
    })
}

// ---- Entries ---------------------------------------------------------------

/// The last member `name` of a lockfile object (generated adapters keep the
/// last duplicate).
pub(crate) fn member<'a>(value: &'a Json, name: &str) -> Option<&'a Json> {
    match value {
        Json::Object(members) => members
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v),
        _ => None,
    }
}

/// LockFileModuleExtension in Gson field order.
pub(crate) fn entry_json(
    bzl_transitive_digest: &str,
    usages_digest: &str,
    recorded_inputs: &[RecordedInput],
    generated_repo_specs: Vec<(String, Json)>,
    metadata: Option<Json>,
) -> Json {
    let mut members = vec![
        (
            "bzlTransitiveDigest".into(),
            Json::String(bzl_transitive_digest.to_owned()),
        ),
        (
            "usagesDigest".into(),
            Json::String(usages_digest.to_owned()),
        ),
        (
            "recordedInputs".into(),
            Json::Array(
                recorded_inputs
                    .iter()
                    .map(|input| Json::String(input.lockfile_string()))
                    .collect(),
            ),
        ),
        (
            "generatedRepoSpecs".into(),
            Json::Object(generated_repo_specs),
        ),
    ];
    if let Some(metadata) = metadata {
        members.push(("moduleExtensionMetadata".into(), metadata));
    }
    Json::Object(members)
}

/// LockFileModuleExtension.isReproducible.
pub(crate) fn entry_is_reproducible(entry: &Json) -> bool {
    member(entry, "moduleExtensionMetadata")
        .and_then(|metadata| member(metadata, "reproducible"))
        .is_some_and(|value| matches!(value, Json::Bool(true)))
}

/// ModuleExtensionEvalFactors.toString.
pub(crate) fn factors_key(os: Option<&str>, arch: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(os) = os.filter(|os| !os.is_empty()) {
        parts.push(format!("os:{os}"));
    }
    if let Some(arch) = arch.filter(|arch| !arch.is_empty()) {
        parts.push(format!("arch:{arch}"));
    }
    if parts.is_empty() {
        "general".to_owned()
    } else {
        parts.join(",")
    }
}

/// ModuleExtensionEvalFactors.parse: (os, arch).
fn parse_factors(key: &str) -> (String, String) {
    if key == "general" {
        return (String::new(), String::new());
    }
    let (mut os, mut arch) = (String::new(), String::new());
    for part in key.split(',') {
        if let Some(value) = part.strip_prefix("os:") {
            os = value.to_owned();
        } else if let Some(value) = part.strip_prefix("arch:") {
            arch = value.to_owned();
        }
    }
    (os, arch)
}

/// ModuleExtensionEvalFactors.hasSameDependenciesAs.
fn same_dependencies(a: &str, b: &str) -> bool {
    let (a_os, a_arch) = parse_factors(a);
    let (b_os, b_arch) = parse_factors(b);
    a_os.is_empty() == b_os.is_empty() && a_arch.is_empty() == b_arch.is_empty()
}

/// ModuleExtensionId.LEXICOGRAPHIC_COMPARATOR (bazel/bzlmod/ModuleExtensionId.java:39-43)
/// on lockfile keys `LABEL%NAME[%ISOLATION]`: label (repository, package,
/// name), then the extension name, then the isolation key (absent first).
pub(crate) fn compare_extension_ids(a: &str, b: &str) -> Ordering {
    fn parts(id: &str) -> (String, String, String, String, Option<String>) {
        let mut split = id.splitn(3, '%');
        let label = split.next().unwrap_or_default();
        let name = split.next().unwrap_or_default().to_owned();
        let isolation = split.next().map(str::to_owned);
        let (repository, rest) = match label.strip_prefix("@@") {
            Some(rest) => rest.split_once("//").unwrap_or((rest, "")),
            None => ("", label.strip_prefix("//").unwrap_or(label)),
        };
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        let (package, target) = rest.split_once(':').unwrap_or((rest, ""));
        (
            repository.to_owned(),
            package.to_owned(),
            target.to_owned(),
            name,
            isolation,
        )
    }
    let (a, b) = (parts(a), parts(b));
    java_string_cmp(&a.0, &b.0)
        .then_with(|| java_string_cmp(&a.1, &b.1))
        .then_with(|| java_string_cmp(&a.2, &b.2))
        .then_with(|| java_string_cmp(&a.3, &b.3))
        .then_with(|| match (&a.4, &b.4) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => java_string_cmp(x, y),
        })
}

/// One SingleExtensionValue done in this invocation: what
/// BazelLockFileModule.afterCommand collects (:117-136).
#[derive(Clone, Debug)]
pub(crate) struct ExtensionResult {
    pub id: String,
    pub factors: String,
    pub entry: Json,
    /// Facts of the value (possibly Facts.EMPTY, an empty object).
    pub facts: Json,
    pub facts_version: i64,
}

/// BazelLockFileModule.combineModuleExtensions (:262-330).
pub(crate) fn combine_extensions(
    old: &[(String, Json)],
    new: &[ExtensionResult],
    has_usages: &dyn Fn(&str) -> bool,
    reproducible: bool,
) -> Vec<(String, Json)> {
    let mut updated: Vec<(String, Vec<(String, Json)>)> = Vec::new();
    for (id, per_factors) in old {
        if !has_usages(id) {
            continue;
        }
        let Json::Object(per_factors) = per_factors else {
            continue;
        };
        match new.iter().find(|result| &result.id == id) {
            None => updated.push((id.clone(), per_factors.clone())),
            Some(result) => {
                let mut keep: Vec<_> = per_factors
                    .iter()
                    .filter(|(factors, _)| {
                        same_dependencies(factors, &result.factors) && factors != &result.factors
                    })
                    .cloned()
                    .collect();
                if keep.is_empty() {
                    continue;
                }
                keep.sort_by(|a, b| java_string_cmp(&a.0, &b.0));
                updated.push((id.clone(), keep));
            }
        }
    }
    for result in new {
        if entry_is_reproducible(&result.entry) != reproducible {
            continue;
        }
        match updated.iter_mut().find(|(id, _)| id == &result.id) {
            Some((_, entries)) => {
                entries.retain(|(factors, _)| factors != &result.factors);
                entries.push((result.factors.clone(), result.entry.clone()));
                entries.sort_by(|a, b| java_string_cmp(&a.0, &b.0));
            }
            None => updated.push((
                result.id.clone(),
                vec![(result.factors.clone(), result.entry.clone())],
            )),
        }
    }
    updated.sort_by(|a, b| compare_extension_ids(&a.0, &b.0));
    updated
        .into_iter()
        .map(|(id, entries)| (id, Json::Object(entries)))
        .collect()
}

/// Lockfile map members (ID -> value).
pub(crate) type Members = Vec<(String, Json)>;

/// filterRelevantFacts / filterRelevantFactsVersions (:226-249) of the old
/// facts combined with the facts of this invocation's results.
pub(crate) fn combine_facts(
    old_facts: &[(String, Json)],
    old_versions: &[(String, Json)],
    new: &[ExtensionResult],
    has_usages: &dyn Fn(&str) -> bool,
) -> (Members, Members) {
    let mut facts: Vec<(String, Json)> = old_facts.to_vec();
    let mut versions: Vec<(String, Json)> = old_versions.to_vec();
    for result in new {
        facts.retain(|(id, _)| id != &result.id);
        facts.push((result.id.clone(), result.facts.clone()));
        versions.retain(|(id, _)| id != &result.id);
        versions.push((
            result.id.clone(),
            Json::Number(result.facts_version.to_string()),
        ));
    }
    facts.retain(|(id, value)| {
        has_usages(id) && !matches!(value, Json::Object(members) if members.is_empty())
    });
    facts.sort_by(|a, b| compare_extension_ids(&a.0, &b.0));
    versions.retain(|(id, version)| {
        facts.iter().any(|(kept, _)| kept == id)
            && !matches!(version, Json::Number(text) if text.parse::<i64>() == Ok(0))
    });
    versions.sort_by(|a, b| compare_extension_ids(&a.0, &b.0));
    (facts, versions)
}

/// What one module extension's evaluation reads of the two lockfiles
/// (SingleExtensionEvalFunction.java:144-213): crate::repository's
/// extension_lock, and the check that an evaluated result stands where the
/// next command reads it (crate::registry_session::RegistrySession::
/// note_settled_extension).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LockSelection {
    /// The facts passed to the extension: the workspace lockfile's when
    /// present (and of `facts_version`), else the hidden lockfile's.
    pub facts: Vec<(String, Json)>,
    /// The workspace lockfile's facts (of `facts_version`).
    pub workspace_facts: Vec<(String, Json)>,
    /// The hidden lockfile's facts of `facts_version`, if any.
    pub hidden_facts: Option<Vec<(String, Json)>>,
    /// The entry for `factors`: the workspace lockfile's, else the hidden one's.
    pub locked: Option<Json>,
}

/// [`LockSelection`] of extension `id` for `factors`.
pub(crate) fn lock_selection(
    workspace: &crate::lockfile::Lockfile,
    hidden: &crate::lockfile::Lockfile,
    id: &str,
    factors: &str,
    facts_version: i64,
) -> LockSelection {
    let facts_of = |lockfile: &crate::lockfile::Lockfile| {
        lockfile
            .facts
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, value)| match value {
                Json::Object(members) => members.clone(),
                _ => Vec::new(),
            })
    };
    let hidden_facts =
        facts_of(hidden).filter(|_| facts_version_of(&hidden.facts_versions, id) == facts_version);
    let mut workspace_facts = Vec::new();
    let mut facts = Vec::new();
    if let Some(found) = facts_of(workspace) {
        if facts_version_of(&workspace.facts_versions, id) == facts_version {
            workspace_facts = found.clone();
            facts = found;
        }
    } else if let Some(found) = &hidden_facts {
        facts = found.clone();
    }
    let entry_of = |lockfile: &crate::lockfile::Lockfile| {
        lockfile
            .module_extensions
            .iter()
            .find(|(key, _)| key == id)
            .and_then(|(_, per_factors)| member(per_factors, factors))
            .cloned()
    };
    let locked = entry_of(workspace).or_else(|| entry_of(hidden));
    LockSelection {
        facts,
        workspace_facts,
        hidden_facts,
        locked,
    }
}

/// Whether the facts keep a reproducible `entry` from being reused
/// (SingleExtensionEvalFunction.tryGettingValueFromLockFile, :389-397): the
/// facts passed differ from those of its most recent evaluation.
pub(crate) fn facts_block_reuse(
    entry: &Json,
    facts: &[(String, Json)],
    hidden_facts: Option<&[(String, Json)]>,
) -> bool {
    entry_is_reproducible(entry) && !facts.is_empty() && Some(facts) != hidden_facts
}

/// `entry` of extension `id` for `factors` as a later command parses it from
/// a lockfile this command writes (serialized, parsed again).
pub(crate) fn reparsed_entry(id: &str, factors: &str, entry: &Json) -> Option<Json> {
    let lockfile = crate::lockfile::Lockfile {
        module_extensions: vec![(
            id.to_owned(),
            Json::Object(vec![(factors.to_owned(), entry.clone())]),
        )],
        ..Default::default()
    };
    let parsed = crate::lockfile::Lockfile::parse(
        &lockfile.serialize(),
        crate::lockfile::LockfileMode::Update,
    )
    .ok()?;
    let (_, per_factors) = parsed
        .module_extensions
        .into_iter()
        .find(|(key, _)| key == id)?;
    member(&per_factors, factors).cloned()
}

/// The lockfile's factsVersions value of `id` (default 0).
pub(crate) fn facts_version_of(versions: &[(String, Json)], id: &str) -> i64 {
    versions
        .iter()
        .find(|(key, _)| key == id)
        .and_then(|(_, value)| match value {
            Json::Number(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

/// SingleExtensionEvalFunction's outdated-lockfile message (:504-512).
pub(crate) fn outdated_message(reason: &str) -> String {
    format!(
        "MODULE.bazel.lock is no longer up-to-date because {reason}. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_recorded_input_strings_match_bazel() {
        assert_eq!(escape(Some("s 1\\x\ny")), "s\\s1\\\\x\\ny");
        assert_eq!(escape(None), "\\0");
        assert_eq!(unescape("s\\s1\\\\x\\ny").as_deref(), Some("s 1\\x\ny"));
        assert_eq!(unescape("\\0"), None);
        assert_eq!(
            RecordedInput::env("LOCKEXT_STATIC", Some("s 1".into())).lockfile_string(),
            "ENV:LOCKEXT_STATIC s\\s1"
        );
        assert_eq!(
            RecordedInput::env("LOCKEXT_MISSING", None).lockfile_string(),
            "ENV:LOCKEXT_MISSING \\0"
        );
        assert_eq!(
            RecordedInput::repo_mapping("", "bazel_tools", Some("bazel_tools".into()))
                .lockfile_string(),
            "REPO_MAPPING:,bazel_tools bazel_tools"
        );
    }

    #[test]
    fn dirents_fingerprint_matches_bazel_capture() {
        // FILE:@@//dir of the reference capture (BUILD.bazel, one.txt, two.txt).
        let names: Vec<Vec<u8>> = ["two.txt", "BUILD.bazel", "one.txt"]
            .iter()
            .map(|name| name.as_bytes().to_vec())
            .collect();
        assert_eq!(
            dirents_marker_value(&names),
            "5a5736b653c61e7ab983256db6a482802c3501344c90a6a4a84430e9196e7544"
        );
    }

    #[test]
    fn usages_digest_hashes_utf16_of_compact_gson() {
        // The reference capture's //:platform_ext.bzl%plat usages.
        let text = r#"{"extensionUsages":{"<root>":{"extensionBzlFile":"@lockroot//:platform_ext.bzl","extensionName":"plat","proxies":[],"tags":[],"repoOverrides":{}}},"extensionUniqueName":"+plat","abridgedModules":[{"name":"lockroot","version":"1.0","key":"<root>"}],"repoMappings":{},"repoOverrides":{}}"#;
        let value = crate::lockfile::parse_json(text).unwrap();
        assert_eq!(gson_compact(&value), text);
        assert_eq!(
            usages_digest(&value),
            "j49h65UOoGKujssnbPELHNwIbjP/+H0ocxH7jrzyoEE="
        );
        let with_null = crate::lockfile::parse_json(r#"{"a":null,"b":[null]}"#).unwrap();
        assert_eq!(gson_compact(&with_null), r#"{"b":[null]}"#);
    }

    #[test]
    fn attribute_strings_and_labels_round_trip_like_the_adapter() {
        assert_eq!(escape_attribute_string("@@raw"), "'@@raw'");
        assert_eq!(escape_attribute_string("'quoted'"), "''quoted''");
        assert_eq!(escape_attribute_string("'"), "'''");
        assert_eq!(escape_attribute_string("x"), "x");
        assert_eq!(
            recorded_string("'@@raw'"),
            RecordedValue::String("@@raw".into())
        );
        assert_eq!(
            recorded_string("@@//:data.txt"),
            RecordedValue::Label("//:data.txt".into())
        );
        assert_eq!(
            recorded_string("@@lockdep+//:lib"),
            RecordedValue::Label("@@lockdep+//:lib".into())
        );
        assert_eq!(
            repo_rule_id("//:repo.bzl%gen_repo").unwrap(),
            "@@//:repo.bzl%gen_repo"
        );
        let (label, name) = parse_repo_rule_id("@@lockdep+//:dep_repo.bzl%dep_repo").unwrap();
        assert_eq!(
            (label.to_string(), name.as_str()),
            ("@@lockdep+//:dep_repo.bzl".into(), "dep_repo")
        );
    }

    #[test]
    fn extension_ids_sort_like_the_lexicographic_comparator() {
        let mut ids = vec![
            "@@lockdep+//:dep_ext.bzl%dep_ext",
            "//:platform_ext.bzl%plat",
            "//:ext.bzl%ext",
            "//:meta_ext.bzl%meta",
            "//a:ext.bzl%z",
            "//:ext.bzl%a",
        ];
        ids.sort_by(|a, b| compare_extension_ids(a, b));
        assert_eq!(
            ids,
            [
                "//:ext.bzl%a",
                "//:ext.bzl%ext",
                "//:meta_ext.bzl%meta",
                "//:platform_ext.bzl%plat",
                "//a:ext.bzl%z",
                "@@lockdep+//:dep_ext.bzl%dep_ext"
            ]
        );
        assert_eq!(
            factors_key(Some("linux"), Some("amd64")),
            "os:linux,arch:amd64"
        );
        assert_eq!(factors_key(None, None), "general");
        assert!(same_dependencies(
            "os:linux,arch:amd64",
            "os:osx,arch:aarch64"
        ));
        assert!(!same_dependencies("general", "os:linux"));
    }

    #[test]
    fn combine_prefers_new_results_and_drops_unused_extensions() {
        let entry = |digest: &str| entry_json(digest, "u", &[], Vec::new(), None);
        let old = vec![
            (
                "//:a.bzl%a".to_owned(),
                Json::Object(vec![
                    ("os:linux".into(), entry("old-linux")),
                    ("os:osx".into(), entry("old-osx")),
                ]),
            ),
            (
                "//:gone.bzl%g".to_owned(),
                Json::Object(vec![("general".into(), entry("g"))]),
            ),
            (
                "//:kept.bzl%k".to_owned(),
                Json::Object(vec![("general".into(), entry("k"))]),
            ),
        ];
        let new = vec![ExtensionResult {
            id: "//:a.bzl%a".into(),
            factors: "os:linux".into(),
            entry: entry("new-linux"),
            facts: Json::Object(Vec::new()),
            facts_version: 0,
        }];
        let used = |id: &str| id != "//:gone.bzl%g";
        let combined = combine_extensions(&old, &new, &used, false);
        let keys: Vec<_> = combined.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(keys, ["//:a.bzl%a", "//:kept.bzl%k"]);
        let Json::Object(per) = &combined[0].1 else {
            panic!()
        };
        assert_eq!(
            per.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["os:linux", "os:osx"]
        );
        assert_eq!(
            member(&per[0].1, "bzlTransitiveDigest"),
            Some(&Json::String("new-linux".into()))
        );
        // A reproducible new result replaces the same-factors entry without
        // being recorded in the workspace lockfile.
        let mut reproducible = new.clone();
        reproducible[0].entry = entry_json(
            "r",
            "u",
            &[],
            Vec::new(),
            Some(Json::Object(vec![
                ("useAllRepos".into(), Json::String("NO".into())),
                ("reproducible".into(), Json::Bool(true)),
            ])),
        );
        let combined = combine_extensions(&old, &reproducible, &used, false);
        let Json::Object(per) = &combined[0].1 else {
            panic!()
        };
        assert_eq!(
            per.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["os:osx"]
        );
    }
}
