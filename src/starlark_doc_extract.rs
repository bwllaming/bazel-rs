//! The native `starlark_doc_extract` rule's extractor (docs/STARLARK_DOC_EXTRACT.md):
//! the `ModuleInfo` (src/main/protobuf/stardoc_output.proto) of a loaded
//! `.bzl` module, as Bazel 9.3.1's ModuleInfoExtractor builds it
//! (lib/starlarkdocextract/*.java), with its binary and text encodings
//! (`ModuleInfo.toByteArray`, `TextFormat.printer().print`).
//!
//! Bazel sources cited below are relative to
//! `src/main/java/com/google/devtools/build/lib/` at 9.3.1.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use starlark::environment::FrozenModule;
use starlark::syntax::ast::{AssignTargetP, AstLiteral, AstStmt, ExprP, StmtP};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, Value};

use crate::custom_rule::{AttrKind, AttrSpec, AttrValue};
use crate::label::Label;
use crate::providers::ProviderRequirement;

// ---------------------------------------------------------------------------
// The proto model: fields in field-number order, as both encoders emit them.

#[derive(Debug, Clone, PartialEq)]
enum Field {
    Str(u32, &'static str, String),
    Bool(u32, &'static str, bool),
    Enum(u32, &'static str, i32, &'static str),
    Msg(u32, &'static str, Message),
}

/// A proto3 message; scalar fields at their default value are omitted,
/// message fields are present once set (even when empty).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Message(Vec<Field>);

impl Message {
    fn str(&mut self, number: u32, name: &'static str, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.0.push(Field::Str(number, name, value));
        }
    }

    fn bool(&mut self, number: u32, name: &'static str, value: bool) {
        if value {
            self.0.push(Field::Bool(number, name, value));
        }
    }

    fn enumeration(&mut self, number: u32, name: &'static str, value: (i32, &'static str)) {
        if value.0 != 0 {
            self.0.push(Field::Enum(number, name, value.0, value.1));
        }
    }

    fn msg(&mut self, number: u32, name: &'static str, value: Message) {
        self.0.push(Field::Msg(number, name, value));
    }

    /// Fields in field-number order (repeated fields keep their order).
    fn sorted(&self) -> Vec<&Field> {
        let mut fields: Vec<&Field> = self.0.iter().collect();
        fields.sort_by_key(|field| match field {
            Field::Str(n, ..) | Field::Bool(n, ..) | Field::Enum(n, ..) | Field::Msg(n, ..) => *n,
        });
        fields
    }

    /// `MessageLite.toByteArray`: fields in field-number order.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for field in self.sorted() {
            match field {
                Field::Str(number, _, value) => {
                    varint(out, u64::from(number << 3 | 2));
                    varint(out, value.len() as u64);
                    out.extend_from_slice(value.as_bytes());
                }
                Field::Bool(number, _, value) => {
                    varint(out, u64::from(number << 3));
                    varint(out, u64::from(*value));
                }
                Field::Enum(number, _, value, _) => {
                    varint(out, u64::from(number << 3));
                    // Negative enum values are sign-extended (none here).
                    varint(out, i64::from(*value) as u64);
                }
                Field::Msg(number, _, message) => {
                    let bytes = message.to_bytes();
                    varint(out, u64::from(number << 3 | 2));
                    varint(out, bytes.len() as u64);
                    out.extend_from_slice(&bytes);
                }
            }
        }
    }

    /// `TextFormat.printer().print(message)` (protobuf-java
    /// TextFormat.Printer with escapeNonAscii = true).
    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        self.print(&mut out, 0);
        out
    }

    fn print(&self, out: &mut String, indent: usize) {
        for field in self.sorted() {
            out.extend(std::iter::repeat_n(' ', indent));
            match field {
                Field::Str(_, name, value) => {
                    out.push_str(name);
                    out.push_str(": \"");
                    escape_bytes(value.as_bytes(), out);
                    out.push_str("\"\n");
                }
                Field::Bool(_, name, value) => {
                    out.push_str(name);
                    out.push_str(if *value { ": true\n" } else { ": false\n" });
                }
                Field::Enum(_, name, _, symbol) => {
                    out.push_str(name);
                    out.push_str(": ");
                    out.push_str(symbol);
                    out.push('\n');
                }
                Field::Msg(_, name, message) => {
                    out.push_str(name);
                    out.push_str(" {\n");
                    message.print(out, indent + 2);
                    out.extend(std::iter::repeat_n(' ', indent));
                    out.push_str("}\n");
                }
            }
        }
    }
}

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

/// TextFormat.escapeBytes (protobuf-java TextFormatEscaper.escapeBytes) of
/// the string's UTF-8 bytes.
fn escape_bytes(bytes: &[u8], out: &mut String) {
    for &byte in bytes {
        match byte {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(byte as char),
            _ => {
                out.push('\\');
                out.push((b'0' + ((byte >> 6) & 3)) as char);
                out.push((b'0' + ((byte >> 3) & 7)) as char);
                out.push((b'0' + (byte & 7)) as char);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Strings: Starlark.trimDocString and Printer.appendQuoted.

/// StringModule.LATIN1_WHITESPACE (the default semantics' strip set).
/// Bazel holds `.bzl` strings as the Latin-1 chars of their UTF-8 bytes
/// (StarlarkUtil.createParserInput), so docstrings are trimmed bytewise:
/// a 0x85/0xA0 byte is whitespace, also inside a UTF-8 sequence.
fn is_latin1_space(byte: u8) -> bool {
    matches!(
        byte,
        b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0x1c..=0x1f | b' ' | 0x85 | 0xa0
    )
}

fn strip_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| !is_latin1_space(b))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn strip_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|&b| !is_latin1_space(b))
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

/// Starlark.expandTabs (net/starlark/java/eval/Starlark.java:631-660) over
/// the Latin-1 chars (bytes).
fn expand_tabs(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut column = 0usize;
    for &byte in text {
        match byte {
            b'\n' | b'\r' => {
                out.push(byte);
                column = 0;
            }
            b'\t' => {
                let spaces = 8 - column % 8;
                out.extend(std::iter::repeat_n(b' ', spaces));
                column += spaces;
            }
            _ => {
                out.push(byte);
                column += 1;
            }
        }
    }
    out
}

/// Java `String.lines()`: split at `\n`, `\r` or `\r\n`, without a final
/// empty line.
fn java_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                i += if text.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Starlark.trimDocString (net/starlark/java/eval/Starlark.java:586-624)
/// over the Latin-1 chars of the UTF-8 text; the result is decoded again
/// (StringEncoding.internalToUnicode: invalid sequences become U+FFFD).
pub(crate) fn trim_doc_string(doc: &str) -> String {
    let expanded = expand_tabs(doc.as_bytes());
    let lines = java_lines(&expanded);
    let Some(first) = lines.first() else {
        return String::new();
    };
    let mut min_leading = usize::MAX;
    for line in &lines[1..] {
        let stripped = strip_start(line);
        if !stripped.is_empty() {
            min_leading = min_leading.min(line.len() - stripped.len());
        }
    }
    if min_leading == usize::MAX {
        min_leading = 0;
    }
    let mut result = strip_end(strip_start(first)).to_vec();
    for line in &lines[1..] {
        if !result.is_empty() {
            result.push(b'\n');
        }
        if line.len() > min_leading {
            result.extend_from_slice(strip_end(&line[min_leading..]));
        }
    }
    String::from_utf8_lossy(strip_end(&result)).into_owned()
}

/// Printer.appendQuoted (net/starlark/java/eval/Printer.java:225-259).
fn quoted(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn quote(text: &str) -> String {
    let mut out = String::new();
    quoted(text, &mut out);
    out
}

/// Java `String.trim()`: strips chars <= U+0020 at both ends.
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

// ---------------------------------------------------------------------------
// Docstrings: DocstringUtils.parseDocstring
// (src/tools/starlark/java/com/google/devtools/starlark/common/DocstringUtils.java).

#[derive(Debug, Default)]
struct DocstringInfo {
    summary: String,
    parameters: Vec<(String, String)>,
    returns: String,
    deprecated: String,
    long_description: String,
}

#[derive(Debug)]
struct DocstringError {
    message: String,
    line: usize,
}

struct DocstringParser<'a> {
    lines: Vec<&'a str>,
    /// 1-based; `> lines.len()` at EOF.
    line_number: usize,
    line: &'a str,
    special_sections_started: bool,
    errors: Vec<DocstringError>,
}

fn indentation(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

impl<'a> DocstringParser<'a> {
    fn new(doc: &'a str) -> Self {
        // Splitter.on("\n").
        let lines: Vec<&str> = doc.split('\n').collect();
        let mut parser = Self {
            lines,
            line_number: 0,
            line: "",
            special_sections_started: false,
            errors: Vec::new(),
        };
        parser.next_line();
        parser
    }

    fn next_line(&mut self) -> bool {
        if self.line_number < self.lines.len() {
            self.line = self.lines[self.line_number];
            self.line_number += 1;
            true
        } else {
            self.line = "";
            self.line_number = self.lines.len() + 1;
            false
        }
    }

    fn eof(&self) -> bool {
        self.line_number > self.lines.len()
    }

    fn error(&mut self, message: impl Into<String>) {
        let line = self.line_number;
        self.error_at(line, message);
    }

    fn error_at(&mut self, line: usize, message: impl Into<String>) {
        self.errors.push(DocstringError {
            message: message.into(),
            line,
        });
    }

    fn parse(&mut self) -> DocstringInfo {
        let summary = self.line.to_owned();
        let mut non_standard_deprecation = self.check_non_standard_deprecation(self.line);
        if !self.next_line() {
            return DocstringInfo {
                summary,
                deprecated: non_standard_deprecation,
                ..DocstringInfo::default()
            };
        }
        if !self.line.is_empty() {
            self.error("the one-line summary should be followed by a blank line");
        } else {
            self.next_line();
        }
        let mut long_description: Vec<&str> = Vec::new();
        let mut params = Vec::new();
        let mut returns = String::new();
        let mut deprecated = String::new();
        let mut body_after_sections_reported = false;
        while !self.eof() {
            match self.line {
                "Args:" | "Arguments:" => {
                    self.check_section_start(!params.is_empty());
                    if !returns.is_empty() {
                        self.error("'Args:' section should go before the 'Returns:' section");
                    }
                    if !deprecated.is_empty() {
                        self.error("'Args:' section should go before the 'Deprecated:' section");
                    }
                    let parsed = self.parse_parameters();
                    params.extend(parsed);
                }
                "Returns:" => {
                    self.check_section_start(!returns.is_empty());
                    if !deprecated.is_empty() {
                        self.error("'Returns:' section should go before the 'Deprecated:' section");
                    }
                    returns = self.parse_section_after_heading();
                }
                "Deprecated:" => {
                    self.check_section_start(!deprecated.is_empty());
                    deprecated = self.parse_section_after_heading();
                }
                line => {
                    if self.special_sections_started && !body_after_sections_reported {
                        self.error("description body should go before the special sections");
                        body_after_sections_reported = true;
                    }
                    if deprecated.is_empty() && non_standard_deprecation.is_empty() {
                        non_standard_deprecation = self.check_non_standard_deprecation(line);
                    }
                    if line.starts_with("Returns: ") {
                        self.error(
                            "the return value should be documented in a section, like this:\n\n\
                             Returns:\n  <documentation here>\n\n\
                             For more details, please have a look at the documentation.",
                        );
                    }
                    long_description.push(line);
                    self.next_line();
                }
            }
        }
        if deprecated.is_empty() {
            deprecated = non_standard_deprecation;
        }
        DocstringInfo {
            summary,
            parameters: params,
            returns,
            deprecated,
            long_description: long_description.join("\n"),
        }
    }

    fn check_section_start(&mut self, duplicate: bool) {
        self.special_sections_started = true;
        if duplicate {
            let message = format!("duplicate '{}' section", self.line);
            self.error(message);
        }
    }

    fn check_non_standard_deprecation(&mut self, line: &str) -> String {
        if line.to_lowercase().starts_with("deprecated:") || line.contains("DEPRECATED") {
            self.error(
                "use a 'Deprecated:' section for deprecations, similar to a 'Returns:' section:\n\n\
                 Deprecated:\n  <reason and alternative>\n\n\
                 For more details, please have a look at the documentation.",
            );
            return line.to_owned();
        }
        String::new()
    }

    fn parse_parameters(&mut self) -> Vec<(String, String)> {
        let section_line = self.line_number;
        self.next_line();
        let mut params = Vec::new();
        let mut expected_indentation: Option<usize> = None;
        while !self.eof() {
            if self.line.is_empty() {
                self.next_line();
                continue;
            }
            let actual = indentation(self.line);
            if actual == 0 {
                break;
            }
            let expected = *expected_indentation.get_or_insert(actual);
            if expected != actual {
                self.error(format!(
                    "inconsistent indentation of parameter lines (before: {expected}; here: {actual} spaces)"
                ));
            }
            let param_line = self.line_number;
            let trimmed = &self.line[actual..];
            let Some((name, description)) = match_param_line(trimmed) else {
                self.error(
                    "invalid parameter documentation (expected format: \"parameter_name: documentation\"). For more details, please have a look at the documentation.",
                );
                self.next_line();
                continue;
            };
            let mut description = description.to_owned();
            self.parse_continued_description(actual, &mut description);
            let description = java_trim(&description).to_owned();
            if description.is_empty() {
                self.error_at(
                    param_line,
                    format!("empty parameter description for '{name}'"),
                );
            }
            params.push((name.to_owned(), description));
        }
        if params.is_empty() {
            self.error_at(section_line, "section is empty or badly formatted");
        }
        params
    }

    fn parse_continued_description(&mut self, baseline: usize, description: &mut String) {
        let mut buffer = Vec::new();
        let mut continuation = usize::MAX;
        while self.next_line() {
            if !self.line.is_empty() {
                if indentation(self.line) <= baseline {
                    break;
                }
                continuation = continuation.min(indentation(self.line));
            }
            buffer.push(self.line);
        }
        for line in buffer {
            description.push('\n');
            if !line.is_empty() {
                description.push_str(&line[continuation..]);
            }
        }
    }

    fn parse_section_after_heading(&mut self) -> String {
        let section_line = self.line_number;
        self.next_line();
        let mut contents = String::new();
        let mut first = true;
        while !self.eof() {
            let trimmed = if self.line.is_empty() {
                self.line
            } else if indentation(self.line) == 0 {
                break;
            } else if indentation(self.line) < 2 {
                self.error(
                    "text in a section has to be indented by two spaces (relative to the left margin of the docstring)",
                );
                &self.line[indentation(self.line)..]
            } else {
                &self.line[2..]
            };
            if !first {
                contents.push('\n');
            }
            contents.push_str(trimmed);
            self.next_line();
            first = false;
        }
        let result = java_trim(&contents).to_owned();
        if result.is_empty() {
            self.error_at(section_line, "section is empty");
        }
        result
    }
}

/// `\s*(?<name>[*\w]+)( \(\s*(?<attributes>.*)\s*\))?: (?<description>.*)`
/// matched against the whole (indentation-stripped) line. Java's `\w` is
/// `[a-zA-Z_0-9]` and `.` excludes line terminators (none in a line).
fn match_param_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start_matches([' ', '\t', '\n', '\u{0b}', '\u{0c}', '\r']);
    let name_len = line
        .bytes()
        .take_while(|b| *b == b'*' || *b == b'_' || b.is_ascii_alphanumeric())
        .count();
    if name_len == 0 {
        return None;
    }
    let (name, rest) = line.split_at(name_len);
    if let Some(description) = rest.strip_prefix(": ") {
        return Some((name, description));
    }
    // ` (attributes): description`: the greedy `.*` inside the parentheses
    // backtracks to the LAST `): ` of the line.
    let attributes = rest.strip_prefix(" (")?;
    let close = attributes.rfind("): ")?;
    Some((name, &attributes[close + 3..]))
}

// ---------------------------------------------------------------------------
// Labels: LabelRenderer (starlarkdocextract/LabelRenderer.java).

/// Renders labels with the main repository mapping (apparent name ->
/// canonical name) and an optional explicit main repository name.
#[derive(Debug, Clone, Default)]
pub(crate) struct LabelRenderer {
    pub mapping: BTreeMap<String, String>,
    pub main_repo_name: Option<String>,
}

impl LabelRenderer {
    fn repository(&self, label: &Label) -> String {
        if label.repository.is_empty() {
            return String::new();
        }
        self.mapping
            .iter()
            .find(|(apparent, canonical)| !apparent.is_empty() && **canonical == label.repository)
            .map_or_else(
                || format!("@@{}", label.repository),
                |(apparent, _)| format!("@{apparent}"),
            )
    }

    /// Label.getDisplayForm (shorthand = false) or getShorthandDisplayForm,
    /// with the main repository renamed when `main_repo_name` is set.
    pub(crate) fn render_label(&self, label: &Label, shorthand: bool) -> String {
        let repository = self.repository(label);
        let text = if shorthand {
            crate::platforms::shorthand_display_form(label, &self.mapping)
        } else {
            format!("{repository}//{}:{}", label.package, label.name)
        };
        match &self.main_repo_name {
            Some(main) if !text.starts_with('@') => {
                if shorthand && text == format!("//:{main}") {
                    format!("@{main}")
                } else {
                    format!("@{main}{text}")
                }
            }
            _ => text,
        }
    }

    /// [`Self::render_label`] (shorthand) of a canonical label string.
    pub(crate) fn render(&self, canonical: &str) -> String {
        match Label::parse_canonical(canonical) {
            Ok(label) => self.render_label(&label, true),
            Err(_) => canonical.to_owned(),
        }
    }

    /// LabelRenderer.repr: the Starlark repr with `Label(...)` constructors
    /// over the display form.
    fn repr(&self, value: Value<'_>, out: &mut String, depth: usize) {
        if depth > 64 {
            out.push_str("...");
            return;
        }
        if let Some(canonical) = crate::starlark_types::unpack_label(value) {
            out.push_str("Label(");
            let shown = Label::parse_canonical(&canonical)
                .map(|label| self.render_label(&label, false))
                .unwrap_or(canonical);
            quoted(&shown, out);
            out.push(')');
        } else if let Some(text) = value.unpack_str() {
            quoted(text, out);
        } else if let Some(list) = ListRef::from_value(value) {
            out.push('[');
            for (index, item) in list.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                self.repr(item, out, depth + 1);
            }
            out.push(']');
        } else if let Some(tuple) = TupleRef::from_value(value) {
            out.push('(');
            for (index, item) in tuple.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                self.repr(item, out, depth + 1);
            }
            if tuple.len() == 1 {
                out.push(',');
            }
            out.push(')');
        } else if let Some(dict) = DictRef::from_value(value) {
            out.push('{');
            for (index, (key, item)) in dict.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                self.repr(key, out, depth + 1);
                out.push_str(": ");
                self.repr(item, out, depth + 1);
            }
            out.push('}');
        } else {
            out.push_str(&value.to_repr());
        }
    }

    /// LabelRenderer.reprWithoutLabelConstructor of an attribute value
    /// (Attribute.valueToStarlark of the stored default).
    fn attr_value_repr(&self, value: &AttrValue) -> String {
        let label = |text: &str| quote(&self.render(text));
        let list = |items: Vec<String>| format!("[{}]", items.join(", "));
        let dict = |entries: Vec<(String, String)>| {
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!("{key}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        match value {
            AttrValue::String(text) => quote(text),
            AttrValue::Strings(items) => list(items.iter().map(|item| quote(item)).collect()),
            AttrValue::StringListDict(entries) => dict(
                entries
                    .iter()
                    .map(|(key, items)| {
                        (
                            quote(key),
                            list(items.iter().map(|item| quote(item)).collect()),
                        )
                    })
                    .collect(),
            ),
            AttrValue::StringDict(entries) => dict(
                entries
                    .iter()
                    .map(|(key, value)| (quote(key), quote(value)))
                    .collect(),
            ),
            AttrValue::Int(number) => number.to_string(),
            AttrValue::Bool(value) => if *value { "True" } else { "False" }.to_owned(),
            AttrValue::Label(None) | AttrValue::Output(None) => "None".to_owned(),
            AttrValue::Label(Some(text)) => label(text),
            AttrValue::Output(Some(text)) => label(text),
            AttrValue::Labels(items) | AttrValue::Outputs(items) => {
                list(items.iter().map(|item| label(item)).collect())
            }
            AttrValue::Ints(items) => list(items.iter().map(i32::to_string).collect()),
            AttrValue::LabelKeyedStringDict(entries) => dict(
                entries
                    .iter()
                    .map(|(key, value)| (label(key), quote(value)))
                    .collect(),
            ),
            AttrValue::StringKeyedLabelDict(entries) => dict(
                entries
                    .iter()
                    .map(|(key, value)| (quote(key), label(value)))
                    .collect(),
            ),
            AttrValue::LabelListDict(entries) => dict(
                entries
                    .iter()
                    .map(|(key, items)| {
                        (
                            quote(key),
                            list(items.iter().map(|item| label(item)).collect()),
                        )
                    })
                    .collect(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Views of the documentable values (built by their defining modules).

/// One attribute of a rule, macro, aspect, repository rule or tag class.
#[derive(Debug, Clone)]
pub(crate) struct DocAttr {
    pub name: String,
    pub spec: AttrSpec,
    /// Attribute.starlarkDefined: declared in Starlark (not built in).
    pub starlark_defined: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RuleDoc {
    pub exported: Option<String>,
    /// The canonical label of the `.bzl` file that exported the rule.
    pub file: String,
    pub doc: Option<String>,
    pub test: bool,
    pub executable: bool,
    pub attrs: Vec<DocAttr>,
    pub advertised: Vec<ProviderRequirement>,
}

#[derive(Debug, Clone)]
pub(crate) struct MacroDoc {
    pub exported: Option<String>,
    pub file: String,
    pub doc: Option<String>,
    pub finalizer: bool,
    pub attrs: Vec<DocAttr>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderDoc<'v> {
    pub exported: Option<String>,
    pub file: String,
    pub doc: Option<String>,
    /// The schema with each field's doc, in declaration order.
    pub fields: Option<Vec<(String, Option<String>)>>,
    pub init: Option<Value<'v>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AspectDoc {
    pub exported: Option<String>,
    pub file: String,
    pub doc: Option<String>,
    /// `None` when attr_aspects is a function.
    pub attr_aspects: Option<Vec<String>>,
    pub attrs: Vec<DocAttr>,
}

#[derive(Debug, Clone)]
pub(crate) struct RepositoryRuleDoc {
    pub exported: Option<String>,
    pub file: String,
    pub doc: Option<String>,
    pub attrs: Vec<DocAttr>,
    pub environ: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleExtensionDoc {
    pub file: String,
    pub doc: Option<String>,
    /// (tag name, doc, attributes) in declaration order.
    pub tag_classes: Vec<(String, Option<String>, Vec<DocAttr>)>,
}

enum Documentable<'v> {
    Rule(RuleDoc),
    Macro(MacroDoc),
    Provider(ProviderDoc<'v>),
    Function,
    Aspect(AspectDoc),
    RepositoryRule(RepositoryRuleDoc),
    ModuleExtension(ModuleExtensionDoc),
    Other,
}

fn classify(value: Value<'_>) -> Documentable<'_> {
    if let Some(rule) = crate::custom_rule::rule_doc(value) {
        Documentable::Rule(rule)
    } else if let Some(definition) = crate::symbolic_macro::macro_doc(value) {
        Documentable::Macro(definition)
    } else if let Some(provider) = crate::providers::provider_doc(value) {
        Documentable::Provider(provider)
    } else if starlark::eval::def_documentation(value).is_some() {
        Documentable::Function
    } else if let Some(aspect) = crate::rule_declarations::aspect_doc(value) {
        Documentable::Aspect(aspect)
    } else if let Some(rule) = crate::repository_rule::repository_rule_doc(value) {
        Documentable::RepositoryRule(rule)
    } else if let Some(extension) = crate::module_extension::module_extension_doc(value) {
        Documentable::ModuleExtension(extension)
    } else {
        Documentable::Other
    }
}

/// The exported name of a StarlarkExportable (None: not exported).
fn exported_of<'a>(value: &'a Documentable<'_>) -> Option<Option<&'a str>> {
    match value {
        Documentable::Rule(rule) => Some(rule.exported.as_deref()),
        Documentable::Macro(definition) => Some(definition.exported.as_deref()),
        Documentable::Provider(provider) => Some(provider.exported.as_deref()),
        Documentable::Aspect(aspect) => Some(aspect.exported.as_deref()),
        Documentable::RepositoryRule(rule) => Some(rule.exported.as_deref()),
        _ => None,
    }
}

/// Struct-like values a traversal recurses into (Structure: struct() and
/// provider instances), with their fields sorted by name.
fn structure_fields<'v>(value: Value<'v>, heap: Heap<'v>) -> Option<Vec<(String, Value<'v>)>> {
    if value.get_type() != "struct" {
        return None;
    }
    let mut names = value.dir_attr();
    names.sort();
    names.dedup();
    let mut fields = Vec::new();
    for name in names {
        if let Some(field) = value.get_attr(&name, heap).ok().flatten() {
            fields.push((name, field));
        }
    }
    Some(fields)
}

// ---------------------------------------------------------------------------
// The source scan: globals, module docstring and Sphinx doc comments.

/// What ModuleInfoExtractor reads from the compiled Program of the module
/// (Program.getDocCommentsMap/getUnusedDocCommentLines, the resolver's
/// global order, the toplevel docstring).
#[derive(Debug, Default)]
pub(crate) struct SourceInfo {
    /// Global (non-load) bindings in first-binding order.
    pub globals: Vec<String>,
    pub module_docstring: Option<String>,
    /// Global name -> (doc comment text, first line, 1-based).
    pub doc_comments: HashMap<String, (String, usize)>,
    /// 1-based lines of `#:` comments attached to no global.
    pub unused_doc_comment_lines: Vec<usize>,
}

fn string_literal(stmt: &AstStmt) -> Option<String> {
    if let StmtP::Expression(expr) = &stmt.node
        && let ExprP::Literal(AstLiteral::String(text)) = &expr.node
    {
        return Some(text.node.clone());
    }
    None
}

fn bound_identifiers(
    target: &AssignTargetP<starlark::syntax::ast::AstNoPayload>,
    out: &mut Vec<String>,
) {
    match target {
        AssignTargetP::Identifier(ident) => out.push(ident.node.ident.clone()),
        AssignTargetP::Tuple(items) => {
            for item in items {
                bound_identifiers(&item.node, out);
            }
        }
        AssignTargetP::Index(_) | AssignTargetP::Dot(..) => {}
    }
}

fn top_level_statements(stmt: &AstStmt) -> Vec<&AstStmt> {
    match &stmt.node {
        StmtP::Statements(items) => items.iter().flat_map(top_level_statements).collect(),
        _ => vec![stmt],
    }
}

impl SourceInfo {
    pub(crate) fn scan(filename: &str, source: &str) -> Result<SourceInfo, String> {
        let dialect = Dialect {
            enable_keyword_only_arguments: true,
            enable_load_reexport: false,
            ..Dialect::Standard
        };
        let ast = AstModule::parse(filename, source.to_owned(), &dialect)
            .map_err(|error| format!("cannot parse {filename}: {error}"))?;
        let line_of = |offset: u32| -> usize {
            ast.file_span(starlark::codemap::Span::new(
                starlark::codemap::Pos::new(offset),
                starlark::codemap::Pos::new(offset),
            ))
            .resolve_span()
            .begin
            .line
                + 1
        };
        let lines: Vec<&str> = source.split('\n').collect();
        // `#:` comments: (line, text, whole-line, start offset).
        let mut doc_lines = Vec::new();
        for span in ast.comments() {
            let text = ast
                .file_span(*span)
                .source_span()
                .trim_end_matches('\r')
                .to_owned();
            if !text.starts_with("#:") {
                continue;
            }
            let line = line_of(span.begin().get());
            let line_text = lines.get(line - 1).copied().unwrap_or("");
            let whole_line = line_text.trim_start().starts_with("#:");
            doc_lines.push((line, text, whole_line, span.begin().get()));
        }
        // Lexer.docComments: blocks of whole-line `#:` comments on
        // consecutive lines.
        let mut blocks: Vec<Vec<usize>> = Vec::new();
        for (index, (line, _, whole, _)) in doc_lines.iter().enumerate() {
            if !whole {
                continue;
            }
            match blocks.last_mut() {
                Some(block)
                    if doc_lines[*block.last().expect("nonempty")].0 + 1 == *line
                        && doc_lines[*block.last().expect("nonempty")].2 =>
                {
                    block.push(index)
                }
                _ => blocks.push(vec![index]),
            }
        }
        let text_of = |indices: &[usize]| {
            indices
                .iter()
                .map(|&index| {
                    let text = &doc_lines[index].1;
                    text.strip_prefix("#: ")
                        .unwrap_or_else(|| &text[2..])
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut used = BTreeSet::new();
        let mut info = SourceInfo::default();
        let mut seen = BTreeSet::new();
        let statements = top_level_statements(ast.statement());
        if let Some(first) = statements.first() {
            info.module_docstring = string_literal(first);
        }
        for stmt in statements {
            let mut names = Vec::new();
            let mut docs: Option<Vec<usize>> = None;
            match &stmt.node {
                StmtP::Assign(assign) => {
                    bound_identifiers(&assign.lhs.node, &mut names);
                    let span = ast.file_span(stmt.span).resolve_span();
                    let (start_line, end_line) = (span.begin.line + 1, span.end.line + 1);
                    let end = stmt.span.end().get();
                    // A trailing `#:` right after the assignment, else a
                    // block ending on the line before it (Parser
                    // .maybeParseTrailingDocComment).
                    let trailing = doc_lines.iter().position(|(line, _, whole, offset)| {
                        !whole && *line == end_line && *offset >= end
                    });
                    docs = match trailing {
                        Some(index) => Some(vec![index]),
                        None => blocks
                            .iter()
                            .find(|block| {
                                doc_lines[*block.last().expect("nonempty")].0 + 1 == start_line
                            })
                            .cloned(),
                    };
                }
                StmtP::AssignModify(target, ..) => bound_identifiers(&target.node, &mut names),
                StmtP::Def(def) => names.push(def.name.node.ident.clone()),
                _ => {}
            }
            for name in names {
                if seen.insert(name.clone()) {
                    info.globals.push(name.clone());
                    if let Some(indices) = &docs {
                        info.doc_comments
                            .insert(name, (text_of(indices), doc_lines[indices[0]].0));
                        used.extend(indices.iter().copied());
                    }
                }
            }
        }
        info.unused_doc_comment_lines = (0..doc_lines.len())
            .filter(|index| !used.contains(index))
            .map(|index| doc_lines[index].0)
            .collect();
        Ok(info)
    }
}

// ---------------------------------------------------------------------------
// The extractor (ModuleInfoExtractor and its helpers).

/// The extraction inputs other than the module.
pub(crate) struct ExtractOptions<'a> {
    /// The module's label (canonical).
    pub label: &'a Label,
    /// The module's file name for error prefixes (BazelModuleContext.filename).
    pub filename: &'a str,
    pub renderer: LabelRenderer,
    /// `symbol_names` (empty: every public symbol).
    pub symbol_names: &'a [String],
    pub allow_unused_doc_comments: bool,
}

fn is_public_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_alphabetic)
}

struct Extractor<'a, 'v> {
    options: &'a ExtractOptions<'a>,
    source: &'a SourceInfo,
    provider_names: HashMap<String, String>,
    heap: Heap<'v>,
    rules: Vec<Message>,
    providers: Vec<Message>,
    functions: Vec<Message>,
    aspects: Vec<Message>,
    extensions: Vec<Message>,
    repository_rules: Vec<Message>,
    macros: Vec<Message>,
    others: Vec<Message>,
}

const IMPLICIT_MACRO_NAME_DOC: &str = "A unique name for this macro instance. Normally, this is also the name for the macro's main or only target. The names of any other targets that this macro might create will be this name with a string suffix.";
const IMPLICIT_MACRO_VISIBILITY_DOC: &str = "The visibility to be passed to this macro's exported targets. It always implicitly includes the location where this macro is instantiated, so this attribute only needs to be explicitly set if you want the macro's targets to be additionally visible somewhere else.";

/// AttributeType enum values (stardoc_output.proto).
fn attribute_type(kind: &AttrKind, name: &str) -> (i32, &'static str) {
    match kind {
        AttrKind::String if name == "name" => (1, "NAME"),
        AttrKind::Int => (2, "INT"),
        AttrKind::Label => (3, "LABEL"),
        AttrKind::String => (4, "STRING"),
        AttrKind::StringList => (5, "STRING_LIST"),
        AttrKind::IntList => (6, "INT_LIST"),
        AttrKind::LabelList => (7, "LABEL_LIST"),
        AttrKind::Bool => (8, "BOOLEAN"),
        AttrKind::LabelKeyedStringDict => (9, "LABEL_STRING_DICT"),
        AttrKind::StringDict => (10, "STRING_DICT"),
        AttrKind::StringListDict => (11, "STRING_LIST_DICT"),
        AttrKind::Output => (12, "OUTPUT"),
        AttrKind::OutputList => (13, "OUTPUT_LIST"),
        AttrKind::StringKeyedLabelDict => (14, "LABEL_DICT_UNARY"),
        AttrKind::LabelListDict => (15, "LABEL_LIST_DICT"),
    }
}

fn origin_key(name: Option<&str>, file: Option<String>) -> Message {
    let mut key = Message::default();
    if let Some(name) = name {
        key.str(1, "name", name);
    }
    if let Some(file) = file {
        key.str(2, "file", file);
    }
    key
}

fn implicit_name(doc: &str) -> Message {
    let mut attribute = Message::default();
    attribute.str(1, "name", "name");
    attribute.str(2, "doc_string", doc);
    attribute.enumeration(3, "type", (1, "NAME"));
    attribute.bool(4, "mandatory", true);
    attribute
}

impl<'v> Extractor<'_, 'v> {
    fn error(&self, message: String) -> String {
        format!("in {}: {message}", self.options.filename)
    }

    fn render(&self, canonical: &str) -> String {
        self.options.renderer.render(canonical)
    }

    /// ProviderNameGroupExtractor.buildProviderNameGroup.
    fn provider_name_group(&self, group: &[ProviderRequirement]) -> Message {
        let mut message = Message::default();
        for provider in group {
            let documented = self
                .provider_names
                .get(&provider.id)
                .cloned()
                .unwrap_or_else(|| provider.display_name.clone());
            message.0.push(Field::Str(1, "provider_name", documented));
        }
        for provider in group {
            let file = match provider.id.rsplit_once('%') {
                Some((file, _)) if Label::parse_canonical(file).is_ok() => self.render(file),
                _ => "<native>".to_owned(),
            };
            message.msg(
                2,
                "origin_key",
                origin_key(Some(&provider.display_name), Some(file)),
            );
        }
        message
    }

    /// AttributeInfoExtractor.buildAttributeInfo.
    fn attribute(&self, attribute: &DocAttr) -> Message {
        let spec = &attribute.spec;
        let mut message = Message::default();
        message.str(1, "name", attribute.name.as_str());
        if let Some(doc) = &spec.declaration.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        message.enumeration(3, "type", attribute_type(&spec.kind, &attribute.name));
        message.bool(4, "mandatory", spec.mandatory);
        for group in &spec.required_providers {
            message.msg(5, "provider_name_group", self.provider_name_group(group));
        }
        if !spec.mandatory {
            let default = if spec.declaration.computed_default || spec.declaration.late_bound {
                "<unrepresentable value>".to_owned()
            } else {
                self.options.renderer.attr_value_repr(&spec.default)
            };
            message.0.push(Field::Str(6, "default_value", default));
        }
        let nonconfigurable = spec.declaration.nonconfigurable
            || matches!(spec.kind, AttrKind::Output | AttrKind::OutputList);
        message.bool(7, "nonconfigurable", nonconfigurable);
        message.bool(8, "natively_defined", !attribute.starlark_defined);
        for value in &spec.declaration.values {
            message.0.push(Field::Str(9, "values", value.repr.clone()));
        }
        message
    }

    /// AttributeInfoExtractor.addDocumentableAttributes.
    fn attributes(
        &self,
        target: &mut Message,
        number: u32,
        implicit: &[Message],
        implicit_names: &[&str],
        attributes: &[DocAttr],
        natively_defined: bool,
    ) {
        for attribute in implicit {
            target.msg(number, "attribute", attribute.clone());
        }
        for attribute in attributes {
            if implicit_names.contains(&attribute.name.as_str()) {
                continue;
            }
            if (attribute.starlark_defined || natively_defined) && is_public_name(&attribute.name) {
                target.msg(number, "attribute", self.attribute(attribute));
            }
        }
    }

    fn check_no_doc_comments(&self, name: &str, what: &str, expected: &str) -> Result<(), String> {
        if let Some((_, line)) = self.source.doc_comments.get(name)
            && !self.options.allow_unused_doc_comments
        {
            return Err(self.error(format!(
                "unexpected doc comment for {name} on line {line}; API documentation for a {what} must be provided in {expected}"
            )));
        }
        Ok(())
    }

    /// StarlarkFunctionInfoExtractor.fromNameAndFunction.
    fn function(&self, name: &str, function: Value<'v>) -> Result<Message, String> {
        let documentation =
            starlark::eval::def_documentation(function).expect("classified as a function");
        let mut message = Message::default();
        message.str(1, "function_name", name);
        let mut parameter_docs: HashMap<String, String> = HashMap::new();
        if let Some(raw) = &documentation.docstring {
            let doc = trim_doc_string(raw);
            let mut parser = DocstringParser::new(&doc);
            let info = parser.parse();
            if !parser.errors.is_empty() {
                let location = java_location(&documentation.location, self.options);
                let mut text = format!(
                    "Unable to generate documentation for function {name} (defined at {location}) due to malformed docstring. Parse errors:\n"
                );
                for error in &parser.errors {
                    text.push_str(&format!(
                        "  {location} line {}: {}\n",
                        error.line,
                        error.message.replace('\n', " ")
                    ));
                }
                // Bazel's event output drops the message's final newline.
                return Err(text.trim_end_matches('\n').to_owned());
            }
            let mut description = info.summary.clone();
            if !info.summary.is_empty() && !info.long_description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(&info.long_description);
            message.0.push(Field::Str(3, "doc_string", description));
            for (parameter, doc) in info.parameters {
                parameter_docs.entry(parameter).or_insert(doc);
            }
            if !info.returns.is_empty() {
                let mut returns = Message::default();
                returns.str(1, "doc_string", info.returns);
                message.msg(4, "return", returns);
            }
            if !info.deprecated.is_empty() {
                let mut deprecated = Message::default();
                deprecated.str(1, "doc_string", info.deprecated);
                message.msg(5, "deprecated", deprecated);
            }
        }
        // Remove the empty doc_string Field::Str pushed unconditionally
        // above: proto3 omits empty strings.
        message
            .0
            .retain(|field| !matches!(field, Field::Str(_, _, value) if value.is_empty()));
        for parameter in self.parameters(function, &documentation.parameter_names, &parameter_docs)
        {
            message.msg(2, "parameter", parameter);
        }
        // getFunctionOriginKey: the first global of the defining module
        // bound to this very function, and that module's label.
        let global = documentation
            .module_globals
            .iter()
            .find(|(_, value)| value.ptr_eq(function))
            .map(|(name, _)| name.as_str());
        let file = Label::parse_canonical(documentation.location.file.filename())
            .ok()
            .map(|label| self.options.renderer.render_label(&label, true));
        message.msg(6, "origin_key", origin_key(global, file));
        Ok(message)
    }

    fn parameters(
        &self,
        function: Value<'v>,
        names: &[String],
        docs: &HashMap<String, String>,
    ) -> Vec<Message> {
        let Some(spec) = function.parameters_spec() else {
            return Vec::new();
        };
        let params = spec.documentation_with_default_value_formatter(
            vec![starlark::typing::Ty::any(); spec.len()],
            HashMap::new(),
            |value: &Value<'_>| {
                let mut out = String::new();
                self.options.renderer.repr(*value, &mut out, 0);
                out
            },
        );
        let ordinary = |param: &starlark::docs::DocParam, role: (i32, &'static str)| {
            let mut message = Message::default();
            message.str(1, "name", param.name.as_str());
            if let Some(doc) = docs.get(&param.name) {
                message.0.push(Field::Str(2, "doc_string", doc.clone()));
            }
            match &param.default_value {
                Some(default) => {
                    message
                        .0
                        .push(Field::Str(3, "default_value", default.clone()));
                }
                None => message.bool(4, "mandatory", true),
            }
            message.enumeration(5, "role", role);
            message
        };
        // The parameter list names `*args`/`**kwargs` generically: the
        // name as written is the parameter's local name. Source order:
        // ordinary parameters, `*args`, keyword-only ones, `**kwargs`.
        let ordinary_count = params.pos_only.len() + params.pos_or_named.len();
        let args_name = names.get(ordinary_count).cloned();
        let kwargs_name = names.last().cloned();
        let special =
            |param: &starlark::docs::DocParam, prefix: &str, role: (i32, &'static str)| {
                let name = if prefix == "*" {
                    &args_name
                } else {
                    &kwargs_name
                }
                .clone()
                .unwrap_or_else(|| param.name.clone());
                let mut message = Message::default();
                message.str(1, "name", name.as_str());
                if let Some(doc) = docs.get(&format!("{prefix}{name}")) {
                    message.0.push(Field::Str(2, "doc_string", doc.clone()));
                }
                message.enumeration(5, "role", role);
                message
            };
        let mut out = Vec::new();
        for param in params.pos_only.iter().chain(&params.pos_or_named) {
            out.push(ordinary(param, (1, "PARAM_ROLE_ORDINARY")));
        }
        for param in &params.named_only {
            out.push(ordinary(param, (3, "PARAM_ROLE_KEYWORD_ONLY")));
        }
        if let Some(param) = &params.args {
            out.push(special(param, "*", (4, "PARAM_ROLE_VARARGS")));
        }
        if let Some(param) = &params.kwargs {
            out.push(special(param, "**", (5, "PARAM_ROLE_KWARGS")));
        }
        // proto3 omits empty strings (an empty parameter doc).
        for message in &mut out {
            message
                .0
                .retain(|field| !matches!(field, Field::Str(_, _, value) if value.is_empty()));
        }
        out
    }

    fn rule(&self, name: &str, rule: &RuleDoc) -> Message {
        let mut message = Message::default();
        message.str(1, "rule_name", name);
        if let Some(doc) = &rule.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        self.attributes(
            &mut message,
            3,
            &[implicit_name("A unique name for this target.")],
            &["name"],
            &rule.attrs,
            false,
        );
        message.msg(
            4,
            "origin_key",
            origin_key(rule.exported.as_deref(), Some(self.render(&rule.file))),
        );
        if !rule.advertised.is_empty() {
            message.msg(
                5,
                "advertised_providers",
                self.provider_name_group(&rule.advertised),
            );
        }
        message.bool(6, "test", rule.test);
        message.bool(7, "executable", rule.executable || rule.test);
        message
    }

    fn macro_info(&self, name: &str, definition: &MacroDoc) -> Message {
        let mut message = Message::default();
        message.str(1, "macro_name", name);
        if let Some(doc) = &definition.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        let mut visibility = Message::default();
        visibility.str(1, "name", "visibility");
        visibility.str(2, "doc_string", IMPLICIT_MACRO_VISIBILITY_DOC);
        visibility.enumeration(3, "type", (7, "LABEL_LIST"));
        visibility.bool(7, "nonconfigurable", true);
        visibility.bool(8, "natively_defined", true);
        self.attributes(
            &mut message,
            3,
            &[implicit_name(IMPLICIT_MACRO_NAME_DOC), visibility],
            &["name", "visibility"],
            &definition.attrs,
            true,
        );
        message.msg(
            4,
            "origin_key",
            origin_key(
                definition.exported.as_deref(),
                Some(self.render(&definition.file)),
            ),
        );
        message.bool(5, "finalizer", definition.finalizer);
        message
    }

    fn provider(&self, name: &str, provider: &ProviderDoc<'v>) -> Result<Message, String> {
        let mut message = Message::default();
        message.str(1, "provider_name", name);
        if let Some(doc) = &provider.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        for (field, doc) in provider.fields.iter().flatten() {
            if is_public_name(field) {
                let mut info = Message::default();
                info.str(1, "name", field.as_str());
                if let Some(doc) = doc {
                    info.str(2, "doc_string", trim_doc_string(doc));
                }
                message.msg(3, "field_info", info);
            }
        }
        message.msg(
            4,
            "origin_key",
            origin_key(
                provider.exported.as_deref(),
                Some(self.render(&provider.file)),
            ),
        );
        if let Some(init) = provider.init
            && starlark::eval::def_documentation(init).is_some()
        {
            message.msg(5, "init", self.function(name, init)?);
        }
        Ok(message)
    }

    fn aspect(&self, name: &str, aspect: &AspectDoc) -> Message {
        let mut message = Message::default();
        message.str(1, "aspect_name", name);
        if let Some(doc) = &aspect.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        for attribute in aspect.attr_aspects.iter().flatten() {
            if is_public_name(attribute) {
                message
                    .0
                    .push(Field::Str(3, "aspect_attribute", attribute.clone()));
            }
        }
        self.attributes(&mut message, 4, &[], &[], &aspect.attrs, false);
        message.msg(
            5,
            "origin_key",
            origin_key(aspect.exported.as_deref(), Some(self.render(&aspect.file))),
        );
        message
    }

    fn module_extension(&self, name: &str, extension: &ModuleExtensionDoc) -> Message {
        let mut message = Message::default();
        message.str(1, "extension_name", name);
        if let Some(doc) = &extension.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        for (tag, doc, attrs) in &extension.tag_classes {
            let mut info = Message::default();
            info.str(1, "tag_name", tag.as_str());
            if let Some(doc) = doc {
                info.str(2, "doc_string", trim_doc_string(doc));
            }
            self.attributes(&mut info, 3, &[], &[], attrs, false);
            message.msg(3, "tag_class", info);
        }
        message.msg(
            4,
            "origin_key",
            origin_key(None, Some(self.render(&extension.file))),
        );
        message
    }

    fn repository_rule(&self, name: &str, rule: &RepositoryRuleDoc) -> Message {
        let mut message = Message::default();
        message.str(1, "rule_name", name);
        if let Some(doc) = &rule.doc {
            message.str(2, "doc_string", trim_doc_string(doc));
        }
        self.attributes(
            &mut message,
            3,
            &[implicit_name("A unique name for this repository.")],
            &["name"],
            &rule.attrs,
            false,
        );
        for name in &rule.environ {
            message.0.push(Field::Str(4, "environ", name.clone()));
        }
        message.msg(
            5,
            "origin_key",
            origin_key(rule.exported.as_deref(), Some(self.render(&rule.file))),
        );
        message
    }

    fn wanted(&self, name: &str) -> bool {
        self.options.symbol_names.is_empty() || self.options.symbol_names.iter().any(|s| s == name)
    }

    /// GlobalsVisitor.maybeVisit for the documentation pass.
    fn visit(&mut self, name: &str, value: Value<'v>, ancestor_wanted: bool) -> Result<(), String> {
        let kind = classify(value);
        if ancestor_wanted || self.wanted(name) {
            if let Some(None) = exported_of(&kind) {
                return Ok(());
            }
            match kind {
                Documentable::Rule(rule) => {
                    self.check_no_doc_comments(name, "rule", "the doc argument to rule()")?;
                    let message = self.rule(name, &rule);
                    self.rules.push(message);
                }
                Documentable::Macro(definition) => {
                    self.check_no_doc_comments(name, "macro", "the doc argument to macro()")?;
                    let message = self.macro_info(name, &definition);
                    self.macros.push(message);
                }
                Documentable::Provider(provider) => {
                    self.check_no_doc_comments(name, "provider", "the doc argument to provider()")?;
                    let message = self.provider(name, &provider)?;
                    self.providers.push(message);
                }
                Documentable::Function => {
                    self.check_no_doc_comments(
                        name,
                        "function",
                        "a docstring at the top of the function body",
                    )?;
                    let message = self.function(name, value)?;
                    self.functions.push(message);
                }
                Documentable::Aspect(aspect) => {
                    self.check_no_doc_comments(name, "aspect", "the doc argument to aspect()")?;
                    let message = self.aspect(name, &aspect);
                    self.aspects.push(message);
                }
                Documentable::RepositoryRule(rule) => {
                    self.check_no_doc_comments(
                        name,
                        "repository rule",
                        "the doc argument to repository_rule()",
                    )?;
                    let message = self.repository_rule(name, &rule);
                    self.repository_rules.push(message);
                }
                Documentable::ModuleExtension(extension) => {
                    self.check_no_doc_comments(
                        name,
                        "module extension",
                        "the doc argument to module_extension()",
                    )?;
                    let message = self.module_extension(name, &extension);
                    self.extensions.push(message);
                }
                Documentable::Other => {
                    if let Some((doc, _)) = self.source.doc_comments.get(name) {
                        let mut message = Message::default();
                        message.str(1, "name", name);
                        message.str(2, "doc", doc.as_str());
                        message.str(3, "type_name", value.get_type());
                        self.others.push(message);
                    }
                    if let Some(fields) = structure_fields(value, self.heap) {
                        for (field, field_value) in fields {
                            if is_public_name(&field) {
                                self.visit(&format!("{name}.{field}"), field_value, true)?;
                            }
                        }
                    }
                }
            }
        } else if let Some(fields) = structure_fields(value, self.heap) {
            for (field, field_value) in fields {
                if is_public_name(&field) {
                    self.visit(&format!("{name}.{field}"), field_value, false)?;
                }
            }
        }
        Ok(())
    }
}

/// ProviderQualifiedNameCollector: provider identity -> its first
/// qualified name, over every public global and struct field.
fn collect_provider_names<'v>(
    name: &str,
    value: Value<'v>,
    heap: Heap<'v>,
    names: &mut HashMap<String, String>,
) {
    match classify(value) {
        Documentable::Provider(provider) => {
            if let Some(exported) = &provider.exported {
                names
                    .entry(format!("{}%{exported}", provider.file))
                    .or_insert_with(|| name.to_owned());
            }
        }
        Documentable::Other => {
            if let Some(fields) = structure_fields(value, heap) {
                for (field, field_value) in fields {
                    if is_public_name(&field) {
                        collect_provider_names(
                            &format!("{name}.{field}"),
                            field_value,
                            heap,
                            names,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Java Location.toString of a function: `file:line:column` with the
/// module's file path (natively known for the extracted module only; other
/// modules show their label).
fn java_location(location: &starlark::codemap::FileSpan, options: &ExtractOptions<'_>) -> String {
    let span = location.resolve_span();
    let file = location.file.filename();
    format!(
        "{}:{}:{}",
        if file == options.label.to_string() {
            options.filename
        } else {
            file
        },
        span.begin.line + 1,
        span.begin.column + 1
    )
}

/// ModuleInfoExtractor.extractFrom: the module's ModuleInfo, or the
/// ExtractionException message.
pub(crate) fn extract(
    module: &FrozenModule,
    source: &SourceInfo,
    options: &ExtractOptions<'_>,
) -> Result<Message, String> {
    if !options.allow_unused_doc_comments && !source.unused_doc_comment_lines.is_empty() {
        let lines = &source.unused_doc_comment_lines;
        return Err(format!(
            "in {}: unexpected or conflicting doc comments on line{} {}; a doc comment must be attached to the declaration of a global variable",
            options.filename,
            if lines.len() > 1 { "s" } else { "" },
            lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut frozen = Vec::new();
    for name in &source.globals {
        if !is_public_name(name) {
            continue;
        }
        if let Ok(Some(value)) = module.get_option(name) {
            frozen.push((name.clone(), value));
        }
    }
    starlark::environment::Module::with_temp_heap(|temp| {
        let heap = temp.heap();
        let globals: Vec<(String, Value<'_>)> = frozen
            .iter()
            .map(|(name, value)| (name.clone(), heap.access_owned_frozen_value(value)))
            .collect();
        let mut provider_names = HashMap::new();
        for (name, value) in &globals {
            collect_provider_names(name, *value, heap, &mut provider_names);
        }
        let mut extractor = Extractor {
            options,
            source,
            provider_names,
            heap,
            rules: Vec::new(),
            providers: Vec::new(),
            functions: Vec::new(),
            aspects: Vec::new(),
            extensions: Vec::new(),
            repository_rules: Vec::new(),
            macros: Vec::new(),
            others: Vec::new(),
        };
        for (name, value) in &globals {
            extractor.visit(name, *value, false)?;
        }
        let mut info = Message::default();
        for (number, name, messages) in [
            (1, "rule_info", &extractor.rules),
            (2, "provider_info", &extractor.providers),
            (3, "func_info", &extractor.functions),
            (4, "aspect_info", &extractor.aspects),
            (7, "module_extension_info", &extractor.extensions),
            (8, "repository_rule_info", &extractor.repository_rules),
            (9, "macro_info", &extractor.macros),
            (10, "starlark_other_symbol_info", &extractor.others),
        ] {
            for message in messages {
                info.msg(number, name, message.clone());
            }
        }
        if let Some(doc) = &source.module_docstring {
            info.str(5, "module_docstring", trim_doc_string(doc));
        }
        info.str(
            6,
            "file",
            options.renderer.render_label(options.label, true),
        );
        Ok(info)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_doc_string_follows_starlark() {
        assert_eq!(
            trim_doc_string("  Summary.  \n\n    Detail\n      more\n  "),
            "Summary.\n\nDetail\n  more"
        );
        assert_eq!(trim_doc_string(""), "");
        assert_eq!(trim_doc_string("\n\n  a\n"), "a");
        assert_eq!(trim_doc_string("x\n\ty"), "x\ny");
    }

    #[test]
    fn docstring_sections_parse_like_bazel() {
        let doc = "Does it.\n\nLong.\n\nArgs:\n  a: the a\n    continued.\n  *args (list): rest\n\nReturns:\n  A value.\n\nDeprecated:\n  Use b.";
        let mut parser = DocstringParser::new(doc);
        let info = parser.parse();
        assert!(parser.errors.is_empty(), "{:?}", parser.errors);
        assert_eq!(info.summary, "Does it.");
        assert_eq!(info.long_description, "Long.\n");
        assert_eq!(
            info.parameters,
            vec![
                ("a".to_owned(), "the a\ncontinued.".to_owned()),
                ("*args".to_owned(), "rest".to_owned())
            ]
        );
        assert_eq!(info.returns, "A value.");
        assert_eq!(info.deprecated, "Use b.");
        let mut parser = DocstringParser::new("Summary\nno blank line");
        parser.parse();
        assert_eq!(parser.errors[0].line, 2);
    }

    #[test]
    fn text_format_escapes_bytes() {
        let mut out = String::new();
        escape_bytes("a'\"\\\n\u{e9}\u{7}".as_bytes(), &mut out);
        assert_eq!(out, "a\\'\\\"\\\\\\n\\303\\251\\a");
    }

    #[test]
    fn binary_encoding_orders_fields() {
        let mut message = Message::default();
        message.str(6, "file", "f");
        let mut inner = Message::default();
        inner.str(1, "rule_name", "r");
        message.msg(1, "rule_info", inner);
        message.msg(1, "rule_info", Message::default());
        assert_eq!(message.to_bytes(), b"\x0a\x03\x0a\x01r\x0a\x00\x32\x01f");
        assert_eq!(
            message.to_text(),
            "rule_info {\n  rule_name: \"r\"\n}\nrule_info {\n}\nfile: \"f\"\n"
        );
    }

    #[test]
    fn doc_comments_attach_to_globals() {
        let source = "\"\"\"Doc.\"\"\"\n\n#: First.\n#: Second.\nA = 1\nB = 2  #: Trailing.\n\n#: Unused.\n\nC = 3\n#:x\nD, E = 4, 5\n";
        let info = SourceInfo::scan("//:x.bzl", source).unwrap();
        assert_eq!(info.globals, ["A", "B", "C", "D", "E"]);
        assert_eq!(info.module_docstring.as_deref(), Some("Doc."));
        assert_eq!(info.doc_comments["A"], ("First.\nSecond.".to_owned(), 3));
        assert_eq!(info.doc_comments["B"].0, "Trailing.");
        assert_eq!(info.doc_comments["E"].0, "x");
        assert_eq!(info.unused_doc_comment_lines, [8]);
    }

    #[test]
    fn label_renderer_renames_the_main_repository() {
        let renderer = LabelRenderer {
            mapping: BTreeMap::from([("dep".to_owned(), "dep+".to_owned())]),
            main_repo_name: Some("main".to_owned()),
        };
        assert_eq!(renderer.render("//pkg:pkg"), "@main//pkg");
        assert_eq!(renderer.render("//:main"), "@main");
        assert_eq!(renderer.render("@@dep+//a:b"), "@dep//a:b");
        assert_eq!(renderer.render("@@other+//a:b"), "@@other+//a:b");
    }
}
