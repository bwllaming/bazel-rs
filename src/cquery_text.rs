//! `cquery --output=jsonproto|textproto`: the same CqueryResult (or, with
//! `--noproto:include_configurations`, QueryResult) message as `proto`,
//! printed like protobuf-java's `JsonFormat.printer()` and
//! `TextFormat.printer()` (ProtoOutputFormatterCallback.writeData).
//!
//! The wire writer ([`crate::cquery_proto`]) streams one length-delimited
//! record per target/configuration; [`Transcoder`] decodes each record with
//! the upstream schema descriptors (`cquery_schema.pb`, the descriptor set of
//! analysis_v2.proto/build.proto, see tests/fixtures/cquery_proto) and prints
//! it as an element of the single message's repeated fields. Retained
//! memory is one record, as for the binary formats.
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::OnceLock;

const SCHEMA: &[u8] = include_bytes!("cquery_schema.pb");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Double,
    Float,
    Int64,
    Uint64,
    Int32,
    Fixed64,
    Fixed32,
    Bool,
    String,
    Message,
    Bytes,
    Uint32,
    Enum,
    Sfixed32,
    Sfixed64,
    Sint32,
    Sint64,
}

#[derive(Debug)]
struct Field {
    name: String,
    json_name: String,
    number: u32,
    repeated: bool,
    kind: Kind,
    type_name: String,
    /// A proto3 field without explicit presence: default values are not
    /// printed.
    implicit_presence: bool,
}

#[derive(Debug, Default)]
struct Message {
    fields: Vec<Field>,
}

#[derive(Debug, Default)]
struct Schema {
    messages: HashMap<String, Message>,
    enums: HashMap<String, HashMap<i32, String>>,
}

fn varint(bytes: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| invalid("truncated varint"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("overlong varint"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("cquery proto transcoding: {message}"),
    )
}

#[derive(Debug, Clone, Copy)]
enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
}

fn fields(bytes: &[u8]) -> io::Result<Vec<(u32, Wire<'_>)>> {
    let mut at = 0;
    let mut result = Vec::new();
    while at < bytes.len() {
        let tag = varint(bytes, &mut at)?;
        let number = u32::try_from(tag >> 3).map_err(|_| invalid("field number"))?;
        let value = match tag & 7 {
            0 => Wire::Varint(varint(bytes, &mut at)?),
            1 => {
                let slice = bytes.get(at..at + 8).ok_or_else(|| invalid("fixed64"))?;
                at += 8;
                Wire::Fixed64(u64::from_le_bytes(slice.try_into().expect("8 bytes")))
            }
            2 => {
                let length =
                    usize::try_from(varint(bytes, &mut at)?).map_err(|_| invalid("length"))?;
                let slice = bytes
                    .get(at..at + length)
                    .ok_or_else(|| invalid("truncated field"))?;
                at += length;
                Wire::Bytes(slice)
            }
            5 => {
                let slice = bytes.get(at..at + 4).ok_or_else(|| invalid("fixed32"))?;
                at += 4;
                Wire::Fixed32(u32::from_le_bytes(slice.try_into().expect("4 bytes")))
            }
            _ => return Err(invalid("unsupported wire type")),
        };
        result.push((number, value));
    }
    Ok(result)
}

fn string(value: Wire<'_>) -> String {
    match value {
        Wire::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

fn wire_number(value: Wire<'_>) -> u64 {
    match value {
        Wire::Varint(value) | Wire::Fixed64(value) => value,
        Wire::Fixed32(value) => value.into(),
        Wire::Bytes(_) => 0,
    }
}

fn camel(name: &str) -> String {
    let mut result = String::new();
    let mut upper = false;
    for character in name.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            result.extend(character.to_uppercase());
            upper = false;
        } else {
            result.push(character);
        }
    }
    result
}

impl Schema {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let mut schema = Schema::default();
        for (number, file) in fields(bytes)? {
            if number != 1 {
                continue;
            }
            let Wire::Bytes(file) = file else { continue };
            let entries = fields(file)?;
            let package = entries
                .iter()
                .find(|(number, _)| *number == 2)
                .map(|(_, value)| string(*value))
                .unwrap_or_default();
            let proto3 = entries
                .iter()
                .any(|(number, value)| *number == 12 && string(*value) == "proto3");
            let scope = if package.is_empty() {
                String::new()
            } else {
                format!(".{package}")
            };
            for (number, value) in entries {
                let Wire::Bytes(bytes) = value else { continue };
                match number {
                    4 => schema.message(&scope, bytes, proto3)?,
                    5 => schema.enumeration(&scope, bytes)?,
                    _ => {}
                }
            }
        }
        Ok(schema)
    }

    fn message(&mut self, scope: &str, bytes: &[u8], proto3: bool) -> io::Result<()> {
        let entries = fields(bytes)?;
        let name = entries
            .iter()
            .find(|(number, _)| *number == 1)
            .map(|(_, value)| string(*value))
            .unwrap_or_default();
        let full = format!("{scope}.{name}");
        let mut message = Message::default();
        for (number, value) in &entries {
            let Wire::Bytes(bytes) = value else { continue };
            match number {
                2 => {
                    let mut field = Field {
                        name: String::new(),
                        json_name: String::new(),
                        number: 0,
                        repeated: false,
                        kind: Kind::String,
                        type_name: String::new(),
                        implicit_presence: false,
                    };
                    let mut label = 1;
                    let mut proto3_optional = false;
                    for (number, value) in fields(bytes)? {
                        match number {
                            1 => field.name = string(value),
                            3 => field.number = number_u32(value),
                            4 => label = wire_number(value),
                            5 => {
                                field.kind = match wire_number(value) {
                                    1 => Kind::Double,
                                    2 => Kind::Float,
                                    3 => Kind::Int64,
                                    4 => Kind::Uint64,
                                    5 => Kind::Int32,
                                    6 => Kind::Fixed64,
                                    7 => Kind::Fixed32,
                                    8 => Kind::Bool,
                                    9 => Kind::String,
                                    11 => Kind::Message,
                                    12 => Kind::Bytes,
                                    13 => Kind::Uint32,
                                    14 => Kind::Enum,
                                    15 => Kind::Sfixed32,
                                    16 => Kind::Sfixed64,
                                    17 => Kind::Sint32,
                                    18 => Kind::Sint64,
                                    _ => return Err(invalid("unsupported field type")),
                                }
                            }
                            6 => field.type_name = string(value),
                            10 => field.json_name = string(value),
                            17 => proto3_optional = wire_number(value) != 0,
                            _ => {}
                        }
                    }
                    field.repeated = label == 3;
                    if field.json_name.is_empty() {
                        field.json_name = camel(&field.name);
                    }
                    field.implicit_presence = proto3
                        && !field.repeated
                        && !proto3_optional
                        && field.kind != Kind::Message;
                    message.fields.push(field);
                }
                3 => self.message(&full, bytes, proto3)?,
                4 => self.enumeration(&full, bytes)?,
                _ => {}
            }
        }
        message.fields.sort_by_key(|field| field.number);
        self.messages.insert(full, message);
        Ok(())
    }

    fn enumeration(&mut self, scope: &str, bytes: &[u8]) -> io::Result<()> {
        let entries = fields(bytes)?;
        let mut name = String::new();
        let mut values = HashMap::new();
        for (number, value) in entries {
            match number {
                1 => name = string(value),
                2 => {
                    let Wire::Bytes(bytes) = value else { continue };
                    let mut constant = String::new();
                    let mut index = 0i32;
                    for (number, value) in fields(bytes)? {
                        match number {
                            1 => constant = string(value),
                            2 => index = number_u32(value) as i32,
                            _ => {}
                        }
                    }
                    values.entry(index).or_insert(constant);
                }
                _ => {}
            }
        }
        self.enums.insert(format!("{scope}.{name}"), values);
        Ok(())
    }
}

fn number_u32(value: Wire<'_>) -> u32 {
    wire_number(value) as u32
}

fn schema() -> &'static Schema {
    static SCHEMA_CELL: OnceLock<Schema> = OnceLock::new();
    SCHEMA_CELL.get_or_init(|| Schema::parse(SCHEMA).expect("embedded cquery schema parses"))
}

/// One decoded scalar or nested message of a field.
enum Value<'a> {
    Scalar(Wire<'a>),
}

/// The values of `message`'s fields in field-number order (getAllFields),
/// packed repeated scalars expanded.
fn decoded<'a, 'm>(
    message: &'m Message,
    bytes: &'a [u8],
) -> io::Result<Vec<(&'m Field, Vec<Value<'a>>)>> {
    let mut values: HashMap<u32, Vec<Value<'a>>> = HashMap::new();
    for (number, value) in fields(bytes)? {
        let Some(field) = message.fields.iter().find(|field| field.number == number) else {
            continue;
        };
        let entry = values.entry(number).or_default();
        let scalar_numeric = !matches!(field.kind, Kind::String | Kind::Bytes | Kind::Message);
        match value {
            Wire::Bytes(packed) if scalar_numeric && field.repeated => {
                let mut at = 0;
                while at < packed.len() {
                    let item = match field.kind {
                        Kind::Fixed64 | Kind::Sfixed64 | Kind::Double => {
                            let slice = packed.get(at..at + 8).ok_or_else(|| invalid("packed"))?;
                            at += 8;
                            Wire::Fixed64(u64::from_le_bytes(slice.try_into().expect("8")))
                        }
                        Kind::Fixed32 | Kind::Sfixed32 | Kind::Float => {
                            let slice = packed.get(at..at + 4).ok_or_else(|| invalid("packed"))?;
                            at += 4;
                            Wire::Fixed32(u32::from_le_bytes(slice.try_into().expect("4")))
                        }
                        _ => Wire::Varint(varint(packed, &mut at)?),
                    };
                    entry.push(Value::Scalar(item));
                }
            }
            value => {
                if !field.repeated {
                    // The last occurrence of a singular field wins.
                    entry.clear();
                }
                entry.push(Value::Scalar(value));
            }
        }
    }
    let mut result = Vec::new();
    for field in &message.fields {
        if let Some(values) = values.remove(&field.number) {
            if field.implicit_presence
                && values
                    .iter()
                    .all(|Value::Scalar(value)| is_default(field.kind, *value))
            {
                continue;
            }
            result.push((field, values));
        }
    }
    Ok(result)
}

fn is_default(kind: Kind, value: Wire<'_>) -> bool {
    match (kind, value) {
        (Kind::String | Kind::Bytes, Wire::Bytes(bytes)) => bytes.is_empty(),
        (_, Wire::Bytes(_)) => false,
        (_, value) => wire_number(value) == 0,
    }
}

/// Gson's HTML-safe string escaping (JsonFormat's printer).
fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", character as u32))
            }
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// TextFormat.escapeBytes over the UTF-8 bytes.
fn text_string(bytes: &[u8], out: &mut String) {
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
            byte => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('"');
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// The scalar's text: `json` quotes 64-bit integers and enum names.
fn scalar(field: &Field, value: Wire<'_>, json: bool, out: &mut String) {
    let raw = wire_number(value);
    match field.kind {
        Kind::String => {
            let Wire::Bytes(bytes) = value else { return };
            if json {
                json_string(&String::from_utf8_lossy(bytes), out);
            } else {
                text_string(bytes, out);
            }
        }
        Kind::Bytes => {
            let Wire::Bytes(bytes) = value else { return };
            text_string(bytes, out);
        }
        Kind::Bool => out.push_str(if raw != 0 { "true" } else { "false" }),
        Kind::Int32 | Kind::Sfixed32 => out.push_str(&(raw as i32).to_string()),
        Kind::Sint32 => out.push_str(&(((raw >> 1) as i32) ^ -((raw & 1) as i32)).to_string()),
        Kind::Uint32 | Kind::Fixed32 => out.push_str(&(raw as u32).to_string()),
        Kind::Int64 | Kind::Sfixed64 | Kind::Sint64 | Kind::Uint64 | Kind::Fixed64 => {
            let text = match field.kind {
                Kind::Uint64 | Kind::Fixed64 => raw.to_string(),
                Kind::Sint64 => (((raw >> 1) as i64) ^ -((raw & 1) as i64)).to_string(),
                _ => (raw as i64).to_string(),
            };
            if json {
                out.push('"');
                out.push_str(&text);
                out.push('"');
            } else {
                out.push_str(&text);
            }
        }
        Kind::Double => out.push_str(&f64::from_bits(raw).to_string()),
        Kind::Float => out.push_str(&f32::from_bits(raw as u32).to_string()),
        Kind::Enum => {
            let index = raw as i32;
            match schema()
                .enums
                .get(&field.type_name)
                .and_then(|values| values.get(&index))
            {
                Some(name) if json => {
                    out.push('"');
                    out.push_str(name);
                    out.push('"');
                }
                Some(name) => out.push_str(name),
                None => out.push_str(&index.to_string()),
            }
        }
        Kind::Message => {}
    }
}

/// JsonFormat.PrinterImpl.print of one message at indentation `level`
/// (the opening brace is already positioned).
fn json_message(type_name: &str, bytes: &[u8], level: usize, out: &mut String) -> io::Result<()> {
    let message = schema()
        .messages
        .get(type_name)
        .ok_or_else(|| invalid("unknown message type"))?;
    out.push_str("{\n");
    let values = decoded(message, bytes)?;
    for (index, (field, values)) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        indent(out, level + 1);
        json_string(&field.json_name, out);
        out.push_str(": ");
        if field.repeated {
            out.push('[');
        }
        for (position, Value::Scalar(value)) in values.iter().enumerate() {
            if position > 0 {
                out.push_str(", ");
            }
            if field.kind == Kind::Message {
                let Wire::Bytes(bytes) = value else {
                    return Err(invalid("message field"));
                };
                json_message(&field.type_name, bytes, level + 1, out)?;
            } else {
                scalar(field, *value, true, out);
            }
        }
        if field.repeated {
            out.push(']');
        }
    }
    if !values.is_empty() {
        out.push('\n');
    }
    indent(out, level);
    out.push('}');
    Ok(())
}

/// TextFormat.Printer.print of one message's fields at `level`.
fn text_fields(type_name: &str, bytes: &[u8], level: usize, out: &mut String) -> io::Result<()> {
    let message = schema()
        .messages
        .get(type_name)
        .ok_or_else(|| invalid("unknown message type"))?;
    for (field, values) in decoded(message, bytes)? {
        for Value::Scalar(value) in values {
            indent(out, level);
            out.push_str(&field.name);
            if field.kind == Kind::Message {
                let Wire::Bytes(bytes) = value else {
                    return Err(invalid("message field"));
                };
                out.push_str(" {\n");
                text_fields(&field.type_name, bytes, level + 1, out)?;
                indent(out, level);
                out.push_str("}\n");
            } else {
                out.push_str(": ");
                scalar(field, value, false, out);
                out.push('\n');
            }
        }
    }
    Ok(())
}

/// A `Write` sink for the streamed (length-delimited) wire records that
/// prints them as one JSON or text message.
pub struct Transcoder<W: Write> {
    out: W,
    json: bool,
    /// The record type: CqueryResult, or QueryResult without configurations.
    root: &'static str,
    pending: Vec<u8>,
    /// The repeated root field currently open in JSON output.
    open: Option<u32>,
}

impl<W: Write> Transcoder<W> {
    pub fn new(out: W, json: bool, configurations: bool) -> Self {
        Self {
            out,
            json,
            root: if configurations {
                ".analysis.CqueryResult"
            } else {
                ".blaze_query.QueryResult"
            },
            pending: Vec::new(),
            open: None,
        }
    }

    fn record(&mut self, bytes: &[u8]) -> io::Result<()> {
        let message = schema()
            .messages
            .get(self.root)
            .ok_or_else(|| invalid("unknown root message"))?;
        let mut out = String::new();
        for (field, values) in decoded(message, bytes)? {
            for Value::Scalar(value) in values {
                let Wire::Bytes(element) = value else {
                    return Err(invalid("root element"));
                };
                if self.json {
                    match self.open {
                        Some(number) if number == field.number => out.push_str(", "),
                        Some(_) => {
                            out.push_str("],\n  ");
                            json_string(&field.json_name, &mut out);
                            out.push_str(": [");
                        }
                        None => {
                            out.push_str("{\n  ");
                            json_string(&field.json_name, &mut out);
                            out.push_str(": [");
                        }
                    }
                    self.open = Some(field.number);
                    json_message(&field.type_name, element, 1, &mut out)?;
                } else {
                    out.push_str(&field.name);
                    out.push_str(" {\n");
                    text_fields(&field.type_name, element, 1, &mut out)?;
                    out.push_str("}\n");
                }
            }
        }
        self.out.write_all(out.as_bytes())
    }

    fn drain(&mut self) -> io::Result<()> {
        loop {
            let mut at = 0;
            let length = match varint(&self.pending, &mut at) {
                Ok(length) => length as usize,
                Err(_) if self.pending.len() < 10 => return Ok(()),
                Err(error) => return Err(error),
            };
            if self.pending.len() < at + length {
                return Ok(());
            }
            let record: Vec<u8> = self.pending[at..at + length].to_vec();
            self.pending.drain(..at + length);
            self.record(&record)?;
        }
    }

    /// Close the message; returns the sink.
    pub fn finish(mut self) -> io::Result<W> {
        self.drain()?;
        if !self.pending.is_empty() {
            return Err(invalid("truncated record"));
        }
        if self.json {
            let tail = if self.open.is_some() {
                "]\n}\n"
            } else {
                "{\n}\n"
            };
            self.out.write_all(tail.as_bytes())?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Transcoder<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        self.drain()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_cquery_messages() {
        let schema = schema();
        for name in [
            ".analysis.CqueryResult",
            ".analysis.ConfiguredTarget",
            ".analysis.Configuration",
            ".blaze_query.QueryResult",
            ".blaze_query.Target",
            ".blaze_query.Attribute",
        ] {
            assert!(schema.messages.contains_key(name), "{name}");
        }
        assert_eq!(
            schema.enums[".blaze_query.Target.Discriminator"]
                .get(&1)
                .map(String::as_str),
            Some("RULE")
        );
    }

    #[test]
    fn escaping_matches_protobuf_java() {
        let mut json = String::new();
        json_string("a<b>'=&\"\\\n\u{1}é", &mut json);
        assert_eq!(
            json,
            "\"a\\u003cb\\u003e\\u0027\\u003d\\u0026\\\"\\\\\\n\\u0001é\""
        );
        let mut text = String::new();
        text_string("a'\"\\\né".as_bytes(), &mut text);
        assert_eq!(text, "\"a\\'\\\"\\\\\\n\\303\\251\"");
    }
}
