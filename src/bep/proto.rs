//! A small dynamic protobuf message with the three encodings the Build
//! Event Protocol file transports write (docs/BEP.md):
//!
//! * binary: the wire format (BinaryFormatFileTransport writes each event
//!   length-delimited, `writeDelimitedTo`);
//! * JSON: protobuf-java's `JsonFormat.printer().omittingInsignificantWhitespace()`
//!   (JsonFormatFileTransport): lowerCamelCase names, 64-bit integers as
//!   strings, enums by name, bytes in base64, `Timestamp`/`Duration` in
//!   their RFC 3339 / `1.5s` forms, maps as objects, and Gson's HTML-safe
//!   string escaping (`=` is `\u003d`);
//! * text: protobuf-java's `TextFormat` (TextFormatFileTransport):
//!   snake_case names, two-space indentation, strings with octal escapes of
//!   every non-ASCII byte.
//!
//! Fields print in field-number order in all three (protobuf-java's
//! `getAllFields` order); singular scalars at their proto3 default are
//! left out, singular messages are printed even when empty.

use std::fmt::Write as _;

/// One field value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    I32(i32),
    I64(i64),
    U32(u32),
    U64(u64),
    /// An enum value: its number and name.
    Enum(i32, &'static str),
    Msg(Msg),
    /// google.protobuf.Timestamp (seconds, nanos).
    Timestamp(i64, i32),
    /// google.protobuf.Duration (seconds, nanos).
    Duration(i64, i32),
    /// One `map<string, string>` entry.
    Entry(String, String),
    /// An empty repeated field, printed only by the pretty JSON form
    /// (JsonFormat's `alwaysPrintFieldsWithNoPresence`: `[]`).
    EmptyList,
}

/// One field: number, snake_case name, whether it is repeated, value.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub number: u32,
    pub name: &'static str,
    pub repeated: bool,
    pub value: Value,
}

/// A message: its set fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Msg {
    pub fields: Vec<Field>,
}

impl Msg {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(mut self, number: u32, name: &'static str, repeated: bool, value: Value) -> Self {
        self.fields.push(Field {
            number,
            name,
            repeated,
            value,
        });
        self
    }

    /// A field as given, also at its default value (JsonFormat's
    /// `alwaysPrintFieldsWithNoPresence` prints those).
    pub fn raw(self, number: u32, name: &'static str, value: Value) -> Self {
        self.push(number, name, false, value)
    }

    /// A repeated field's elements; an empty one is kept as [`Value::EmptyList`].
    pub fn list(mut self, number: u32, name: &'static str, values: Vec<Value>) -> Self {
        if values.is_empty() {
            return self.push(number, name, true, Value::EmptyList);
        }
        for value in values {
            self = self.push(number, name, true, value);
        }
        self
    }

    /// A singular string (left out when empty).
    pub fn str(self, number: u32, name: &'static str, value: impl Into<String>) -> Self {
        let value = value.into();
        if value.is_empty() {
            return self;
        }
        self.push(number, name, false, Value::Str(value))
    }

    /// One element of a repeated string.
    pub fn strs<I, S>(mut self, number: u32, name: &'static str, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for value in values {
            self = self.push(number, name, true, Value::Str(value.into()));
        }
        self
    }

    pub fn bytes(self, number: u32, name: &'static str, value: Vec<u8>) -> Self {
        if value.is_empty() {
            return self;
        }
        self.push(number, name, false, Value::Bytes(value))
    }

    pub fn bytes_list<I>(mut self, number: u32, name: &'static str, values: I) -> Self
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        for value in values {
            self = self.push(number, name, true, Value::Bytes(value));
        }
        self
    }

    pub fn bool(self, number: u32, name: &'static str, value: bool) -> Self {
        if !value {
            return self;
        }
        self.push(number, name, false, Value::Bool(value))
    }

    pub fn i32(self, number: u32, name: &'static str, value: i32) -> Self {
        if value == 0 {
            return self;
        }
        self.push(number, name, false, Value::I32(value))
    }

    pub fn i64(self, number: u32, name: &'static str, value: i64) -> Self {
        if value == 0 {
            return self;
        }
        self.push(number, name, false, Value::I64(value))
    }

    pub fn u32(self, number: u32, name: &'static str, value: u32) -> Self {
        if value == 0 {
            return self;
        }
        self.push(number, name, false, Value::U32(value))
    }

    pub fn u64(self, number: u32, name: &'static str, value: u64) -> Self {
        if value == 0 {
            return self;
        }
        self.push(number, name, false, Value::U64(value))
    }

    /// An enum (left out at number 0).
    pub fn enumeration(self, number: u32, name: &'static str, value: (i32, &'static str)) -> Self {
        if value.0 == 0 {
            return self;
        }
        self.push(number, name, false, Value::Enum(value.0, value.1))
    }

    /// A singular message (always set).
    pub fn msg(self, number: u32, name: &'static str, value: Msg) -> Self {
        self.push(number, name, false, Value::Msg(value))
    }

    /// A singular message, when given.
    pub fn opt_msg(self, number: u32, name: &'static str, value: Option<Msg>) -> Self {
        match value {
            Some(value) => self.msg(number, name, value),
            None => self,
        }
    }

    /// Elements of a repeated message field.
    pub fn msgs<I>(mut self, number: u32, name: &'static str, values: I) -> Self
    where
        I: IntoIterator<Item = Msg>,
    {
        for value in values {
            self = self.push(number, name, true, Value::Msg(value));
        }
        self
    }

    /// A Timestamp from milliseconds since the epoch.
    pub fn timestamp_millis(self, number: u32, name: &'static str, millis: i64) -> Self {
        self.push(
            number,
            name,
            false,
            Value::Timestamp(
                millis.div_euclid(1000),
                (millis.rem_euclid(1000) * 1_000_000) as i32,
            ),
        )
    }

    /// A Duration.
    pub fn duration(self, number: u32, name: &'static str, value: std::time::Duration) -> Self {
        self.push(
            number,
            name,
            false,
            Value::Duration(value.as_secs() as i64, value.subsec_nanos() as i32),
        )
    }

    /// A `map<string, string>` field's entries, in the given order.
    pub fn map<I>(mut self, number: u32, name: &'static str, entries: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        for (key, value) in entries {
            self = self.push(number, name, true, Value::Entry(key, value));
        }
        self
    }

    /// The fields in field-number order (stable: repeated fields keep their
    /// order).
    fn ordered(&self) -> Vec<&Field> {
        let mut fields: Vec<&Field> = self.fields.iter().collect();
        fields.sort_by_key(|field| field.number);
        fields
    }

    // ---- binary -----------------------------------------------------------

    /// The wire-format encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        for field in self.ordered() {
            encode_field(out, field.number, &field.value);
        }
    }

    // ---- JSON -------------------------------------------------------------

    /// JsonFormat's compact form.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.json_into(&mut out);
        out
    }

    fn json_into(&self, out: &mut String) {
        out.push('{');
        let fields = self.ordered();
        let mut index = 0;
        let mut first = true;
        while index < fields.len() {
            let field = fields[index];
            let mut end = index + 1;
            while end < fields.len() && fields[end].number == field.number {
                end += 1;
            }
            if !first {
                out.push(',');
            }
            first = false;
            json_string(out, &camel(field.name));
            out.push(':');
            if matches!(field.value, Value::Entry(..)) {
                out.push('{');
                for (position, entry) in fields[index..end].iter().enumerate() {
                    if position > 0 {
                        out.push(',');
                    }
                    if let Value::Entry(key, value) = &entry.value {
                        json_string(out, key);
                        out.push(':');
                        json_string(out, value);
                    }
                }
                out.push('}');
            } else if field.repeated && !matches!(field.value, Value::EmptyList) {
                out.push('[');
                for (position, element) in fields[index..end].iter().enumerate() {
                    if position > 0 {
                        out.push(',');
                    }
                    json_value(out, &element.value);
                }
                out.push(']');
            } else {
                json_value(out, &field.value);
            }
            index = end;
        }
        out.push('}');
    }

    // ---- text -------------------------------------------------------------

    /// TextFormat's form (each field on its own line, two-space nesting).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.text_into(&mut out, 0);
        out
    }

    fn text_into(&self, out: &mut String, indent: usize) {
        for field in self.ordered() {
            let pad = "  ".repeat(indent);
            match &field.value {
                Value::Msg(message) => {
                    let _ = writeln!(out, "{pad}{} {{", field.name);
                    message.text_into(out, indent + 1);
                    let _ = writeln!(out, "{pad}}}");
                }
                Value::Timestamp(seconds, nanos) | Value::Duration(seconds, nanos) => {
                    let _ = writeln!(out, "{pad}{} {{", field.name);
                    let inner = Msg::new()
                        .i64(1, "seconds", *seconds)
                        .i32(2, "nanos", *nanos);
                    inner.text_into(out, indent + 1);
                    let _ = writeln!(out, "{pad}}}");
                }
                Value::Entry(key, value) => {
                    let _ = writeln!(out, "{pad}{} {{", field.name);
                    let inner = Msg::new()
                        .push(1, "key", false, Value::Str(key.clone()))
                        .push(2, "value", false, Value::Str(value.clone()));
                    inner.text_into(out, indent + 1);
                    let _ = writeln!(out, "{pad}}}");
                }
                Value::EmptyList => {}
                scalar => {
                    let _ = write!(out, "{pad}{}: ", field.name);
                    text_scalar(out, scalar);
                    out.push('\n');
                }
            }
        }
    }
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn key(out: &mut Vec<u8>, number: u32, wire: u8) {
    varint(out, (u64::from(number) << 3) | u64::from(wire));
}

fn length_delimited(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    key(out, number, 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn encode_field(out: &mut Vec<u8>, number: u32, value: &Value) {
    match value {
        Value::EmptyList => {}
        Value::Str(text) => length_delimited(out, number, text.as_bytes()),
        Value::Bytes(bytes) => length_delimited(out, number, bytes),
        Value::Bool(value) => {
            key(out, number, 0);
            varint(out, u64::from(*value));
        }
        // Negative int32/enum values are sign-extended to 10 bytes.
        Value::I32(value) | Value::Enum(value, _) => {
            key(out, number, 0);
            varint(out, i64::from(*value) as u64);
        }
        Value::I64(value) => {
            key(out, number, 0);
            varint(out, *value as u64);
        }
        Value::U32(value) => {
            key(out, number, 0);
            varint(out, u64::from(*value));
        }
        Value::U64(value) => {
            key(out, number, 0);
            varint(out, *value);
        }
        Value::Msg(message) => length_delimited(out, number, &message.encode()),
        Value::Timestamp(seconds, nanos) | Value::Duration(seconds, nanos) => {
            let inner = Msg::new()
                .i64(1, "seconds", *seconds)
                .i32(2, "nanos", *nanos);
            length_delimited(out, number, &inner.encode());
        }
        Value::Entry(map_key, map_value) => {
            let mut inner = Vec::new();
            length_delimited(&mut inner, 1, map_key.as_bytes());
            length_delimited(&mut inner, 2, map_value.as_bytes());
            length_delimited(out, number, &inner);
        }
    }
}

/// Write `bytes` length-delimited (varint length prefix) to `out`.
pub fn delimited(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 5);
    varint(&mut out, bytes.len() as u64);
    out.extend_from_slice(bytes);
    out
}

/// lowerCamelCase json_name of a snake_case field name (protoc's ToJsonName).
fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for character in name.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            out.extend(character.to_uppercase());
            upper = false;
        } else {
            out.push(character);
        }
    }
    out
}

/// Gson's HTML-safe string escaping (protobuf-java's JsonFormat uses it).
pub fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            control if (control as u32) < 0x20 || control == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn json_value(out: &mut String, value: &Value) {
    match value {
        Value::Str(text) => json_string(out, text),
        Value::Bytes(bytes) => {
            use base64::Engine as _;
            json_string(
                out,
                &base64::engine::general_purpose::STANDARD.encode(bytes),
            );
        }
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::I32(value) => {
            let _ = write!(out, "{value}");
        }
        Value::U32(value) => {
            let _ = write!(out, "{value}");
        }
        Value::I64(value) => {
            let _ = write!(out, "\"{value}\"");
        }
        Value::U64(value) => {
            let _ = write!(out, "\"{value}\"");
        }
        Value::Enum(_, name) => json_string(out, name),
        Value::Msg(message) => message.json_into(out),
        Value::Timestamp(seconds, nanos) => json_string(out, &rfc3339(*seconds, *nanos)),
        Value::Duration(seconds, nanos) => {
            json_string(out, &format!("{seconds}{}s", fraction(*nanos)))
        }
        Value::Entry(key, value) => {
            out.push('{');
            json_string(out, key);
            out.push(':');
            json_string(out, value);
            out.push('}');
        }
        Value::EmptyList => out.push_str("[]"),
    }
}

impl Msg {
    /// JsonFormat's default (pretty) form: two-space indentation, `": "`
    /// after names, repeated values joined by `, `.
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::new();
        self.pretty_into(&mut out, 0);
        out
    }

    fn pretty_into(&self, out: &mut String, level: usize) {
        out.push_str("{\n");
        let fields = self.ordered();
        let mut index = 0;
        let mut first = true;
        while index < fields.len() {
            let field = fields[index];
            let mut end = index + 1;
            while end < fields.len() && fields[end].number == field.number {
                end += 1;
            }
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&"  ".repeat(level + 1));
            json_string(out, &camel(field.name));
            out.push_str(": ");
            if matches!(field.value, Value::Entry(..)) {
                out.push_str("{\n");
                for (position, entry) in fields[index..end].iter().enumerate() {
                    if position > 0 {
                        out.push_str(",\n");
                    }
                    if let Value::Entry(key, value) = &entry.value {
                        out.push_str(&"  ".repeat(level + 2));
                        json_string(out, key);
                        out.push_str(": ");
                        json_string(out, value);
                    }
                }
                out.push('\n');
                out.push_str(&"  ".repeat(level + 1));
                out.push('}');
            } else if field.repeated && !matches!(field.value, Value::EmptyList) {
                out.push('[');
                for (position, element) in fields[index..end].iter().enumerate() {
                    if position > 0 {
                        out.push_str(", ");
                    }
                    pretty_value(out, &element.value, level + 1);
                }
                out.push(']');
            } else {
                pretty_value(out, &field.value, level + 1);
            }
            index = end;
        }
        if !first {
            out.push('\n');
        }
        out.push_str(&"  ".repeat(level));
        out.push('}');
    }
}

fn pretty_value(out: &mut String, value: &Value, level: usize) {
    match value {
        Value::Msg(message) => message.pretty_into(out, level),
        other => json_value(out, other),
    }
}

/// Timestamps/Durations' fraction: none, 3, 6 or 9 digits
/// (com.google.protobuf.util.Timestamps.formatNanos).
fn fraction(nanos: i32) -> String {
    if nanos == 0 {
        String::new()
    } else if nanos % 1_000_000 == 0 {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    }
}

/// `2026-10-26T14:30:45.199Z` (Timestamps.toString).
pub fn rfc3339(seconds: i64, nanos: i32) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        fraction(nanos)
    )
}

/// Howard Hinnant's days-to-civil conversion (proleptic Gregorian).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// TextFormat.escapeBytes over UTF-8 bytes.
fn text_escape(out: &mut String, bytes: &[u8]) {
    out.push('"');
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
            other => {
                let _ = write!(out, "\\{other:03o}");
            }
        }
    }
    out.push('"');
}

fn text_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Str(text) => text_escape(out, text.as_bytes()),
        Value::Bytes(bytes) => text_escape(out, bytes),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::I32(value) => {
            let _ = write!(out, "{value}");
        }
        Value::I64(value) => {
            let _ = write!(out, "{value}");
        }
        Value::U32(value) => {
            let _ = write!(out, "{value}");
        }
        Value::U64(value) => {
            let _ = write!(out, "{value}");
        }
        Value::Enum(_, name) => out.push_str(name),
        Value::Msg(_)
        | Value::Timestamp(..)
        | Value::Duration(..)
        | Value::Entry(..)
        | Value::EmptyList => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_and_binary_follow_protobuf_java() {
        let message = Msg::new()
            .str(1, "uuid", "a=b'c")
            .i64(2, "start_time_millis", 1_793_025_045_199)
            .timestamp_millis(9, "start_time", 1_793_025_045_199)
            .strs(3, "path_prefix", ["bazel-out", "bin"])
            .map(4, "make_variable", [("A".to_owned(), "1".to_owned())])
            .msg(5, "empty", Msg::new())
            .enumeration(6, "status", (1, "PASSED"))
            .i32(7, "count", 0);
        assert_eq!(
            message.to_json(),
            r#"{"uuid":"a\u003db\u0027c","startTimeMillis":"1793025045199","pathPrefix":["bazel-out","bin"],"makeVariable":{"A":"1"},"empty":{},"status":"PASSED","startTime":"2026-10-26T14:30:45.199Z"}"#
        );
        assert_eq!(
            message.to_text(),
            "uuid: \"a=b\\'c\"\nstart_time_millis: 1793025045199\npath_prefix: \"bazel-out\"\npath_prefix: \"bin\"\nmake_variable {\n  key: \"A\"\n  value: \"1\"\n}\nempty {\n}\nstatus: PASSED\nstart_time {\n  seconds: 1793025045\n  nanos: 199000000\n}\n"
        );
        let binary = Msg::new().str(1, "a", "x").i32(2, "b", 150).encode();
        assert_eq!(binary, vec![0x0a, 1, b'x', 0x10, 0x96, 0x01]);
        assert_eq!(delimited(&binary)[0], 6);
    }

    #[test]
    fn pretty_json_follows_json_format_printer() {
        let message = Msg::new()
            .strs(1, "command_args", ["a", "b"])
            .list(2, "environment_variables", Vec::new())
            .msgs(
                4,
                "inputs",
                [Msg::new()
                    .str(1, "path", "p")
                    .raw(3, "is_tool", Value::Bool(false))],
            )
            .raw(15, "exit_code", Value::I32(0));
        assert_eq!(
            message.to_json_pretty(),
            "{\n  \"commandArgs\": [\"a\", \"b\"],\n  \"environmentVariables\": [],\n  \"inputs\": [{\n    \"path\": \"p\",\n    \"isTool\": false\n  }],\n  \"exitCode\": 0\n}"
        );
        assert_eq!(Msg::new().to_json_pretty(), "{\n}");
    }

    #[test]
    fn durations_and_non_ascii_text() {
        let message = Msg::new()
            .duration(1, "d", std::time::Duration::from_millis(4))
            .str(2, "s", "é");
        assert_eq!(message.to_json(), r#"{"d":"0.004s","s":"é"}"#);
        assert!(message.to_text().contains("s: \"\\303\\251\""));
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00Z");
    }
}
