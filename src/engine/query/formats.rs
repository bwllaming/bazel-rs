//! `query --output=xml|proto|streamed_proto|streamed_jsonproto`
//! (query2/query/output/XmlOutputFormatter.java, ProtoOutputFormatter.java,
//! StreamedProtoOutputFormatter.java, StreamedJSONProtoOutputFormatter.java
//! and packages/AttributeFormatter.java, pinned Bazel 9.3.0). The
//! `blaze_query` messages (src/main/protobuf/build.proto) are built as a
//! small field tree per target, then written as protobuf wire bytes (fields
//! in number order, as protobuf-java serializes them) or as protobuf-java
//! `JsonFormat` text (also field-number order, Gson string escaping). See
//! docs/QUERY.md "Structured outputs".

use std::collections::{HashMap, HashSet};

use super::query_attrs::{self, AttrType, QueryAttribute, Shape};
use super::{Env, Eval, Kind};
use crate::custom_rule::{AttrValue, ConfigurablePart};
use crate::label::Label;
use crate::package::RuleKind;

// ----------------------------------------------------------------------
// Message tree.

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i32),
    Bool(bool),
    Enum(i32, &'static str),
    Msg(Msg),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Msg {
    fields: Vec<(u32, &'static str, bool, Value)>,
}

impl Msg {
    fn put(&mut self, number: u32, name: &'static str, repeated: bool, value: Value) {
        self.fields.push((number, name, repeated, value));
    }
    fn str(&mut self, number: u32, name: &'static str, value: impl Into<String>) {
        self.put(number, name, false, Value::Str(value.into()));
    }
    fn strs(&mut self, number: u32, name: &'static str, values: impl IntoIterator<Item = String>) {
        for value in values {
            self.put(number, name, true, Value::Str(value));
        }
    }
    fn int(&mut self, number: u32, name: &'static str, value: i32) {
        self.put(number, name, false, Value::Int(value));
    }
    fn boolean(&mut self, number: u32, name: &'static str, value: bool) {
        self.put(number, name, false, Value::Bool(value));
    }
    fn enumeration(&mut self, number: u32, name: &'static str, value: i32, text: &'static str) {
        self.put(number, name, false, Value::Enum(value, text));
    }
    fn msg(&mut self, number: u32, name: &'static str, value: Msg) {
        self.put(number, name, false, Value::Msg(value));
    }
    fn msgs(&mut self, number: u32, name: &'static str, value: Msg) {
        self.put(number, name, true, Value::Msg(value));
    }

    fn sorted(&self) -> Vec<&(u32, &'static str, bool, Value)> {
        let mut fields = self.fields.iter().collect::<Vec<_>>();
        fields.sort_by_key(|field| field.0);
        fields
    }

    /// Protobuf wire format (CodedOutputStream): fields in number order,
    /// int32 negatives sign extended, repeated scalars unpacked (proto2).
    pub(super) fn encode(&self, out: &mut Vec<u8>) {
        for (number, _, _, value) in self.sorted() {
            match value {
                Value::Str(text) => {
                    varint(out, u64::from(number << 3 | 2));
                    varint(out, text.len() as u64);
                    out.extend_from_slice(text.as_bytes());
                }
                Value::Int(n) | Value::Enum(n, _) => {
                    varint(out, u64::from(number << 3));
                    varint(out, i64::from(*n) as u64);
                }
                Value::Bool(b) => {
                    varint(out, u64::from(number << 3));
                    varint(out, u64::from(*b));
                }
                Value::Msg(msg) => {
                    let mut inner = Vec::new();
                    msg.encode(&mut inner);
                    varint(out, u64::from(number << 3 | 2));
                    varint(out, inner.len() as u64);
                    out.extend_from_slice(&inner);
                }
            }
        }
    }

    /// JsonFormat.printer().omittingInsignificantWhitespace().print(msg):
    /// set fields in field-number order (getAllFields), repeated fields as
    /// arrays, enums by name.
    pub(super) fn json(&self, out: &mut String) {
        out.push('{');
        let fields = self.sorted();
        let mut index = 0;
        let mut first = true;
        while index < fields.len() {
            let (number, name, repeated, _) = fields[index];
            let mut end = index;
            while end < fields.len() && fields[end].0 == *number {
                end += 1;
            }
            if !first {
                out.push(',');
            }
            first = false;
            gson_string(out, name);
            out.push(':');
            if *repeated {
                out.push('[');
                for (i, field) in fields[index..end].iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_value(out, &field.3);
                }
                out.push(']');
            } else {
                json_value(out, &fields[end - 1].3);
            }
            index = end;
        }
        out.push('}');
    }
}

fn json_value(out: &mut String, value: &Value) {
    match value {
        Value::Str(text) => gson_string(out, text),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Enum(_, name) => {
            out.push('"');
            out.push_str(name);
            out.push('"');
        }
        Value::Msg(msg) => msg.json(out),
    }
}

/// Gson's JsonWriter string escaping with HTML-safe replacements (the
/// default Gson instance JsonFormat uses): `"`, `\`, control characters,
/// U+2028/U+2029 and `< > & = '` (lowercase `\u00xx`).
fn gson_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(super) fn varint(out: &mut Vec<u8>, mut value: u64) {
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

// ----------------------------------------------------------------------
// XML tree (javax.xml DOM + the JDK Transformer with INDENT=yes).

#[derive(Clone, Debug, Default)]
pub(super) struct Xml {
    name: &'static str,
    attributes: Vec<(&'static str, String)>,
    children: Vec<Xml>,
}

impl Xml {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }
    fn attr(&mut self, name: &'static str, value: impl Into<String>) {
        if let Some(slot) = self.attributes.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value.into();
        } else {
            self.attributes.push((name, value.into()));
        }
    }
    fn named(mut self, name: impl Into<String>) -> Self {
        self.attr("name", name);
        self
    }

    pub(super) fn write(&self, out: &mut String, depth: usize) {
        out.push_str(&"    ".repeat(depth));
        out.push('<');
        out.push_str(self.name);
        // The DOM keeps attributes sorted by name.
        let mut attributes = self.attributes.iter().collect::<Vec<_>>();
        attributes.sort_by_key(|(name, _)| *name);
        for (name, value) in attributes {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            xml_escape(out, value);
            out.push('"');
        }
        if self.children.is_empty() {
            out.push_str("/>\n");
            return;
        }
        out.push_str(">\n");
        for child in &self.children {
            child.write(out, depth + 1);
        }
        out.push_str(&"    ".repeat(depth));
        out.push_str("</");
        out.push_str(self.name);
        out.push_str(">\n");
    }
}

/// XmlOutputFormatter does not call internalToUnicode: Bazel's internal
/// strings hold UTF-8 bytes as Latin-1 characters, so every non-ASCII byte
/// is written as its own character (genuine `é` prints as `Ã©`).
fn xml_escape(out: &mut String, value: &str) {
    for byte in value.bytes() {
        let c = char::from(byte);
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' | '\t' => out.push_str(&format!("&#{};", byte)),
            c if (byte < 0x20) || (0x7f..=0x9f).contains(&byte) => {
                out.push_str(&format!("&#{};", c as u32))
            }
            c => out.push(c),
        }
    }
}

// ----------------------------------------------------------------------
// Attribute values.

/// The raw value of one rule attribute: plain (None = null) or a select().
#[derive(Clone, Debug)]
enum Raw {
    Plain(Option<AttrValue>),
    Select(Vec<ConfigurablePart>),
}

/// One serialized attribute of a rule.
#[derive(Clone, Debug)]
struct OutAttr {
    name: String,
    ty: AttrType,
    explicit: bool,
    raw: Raw,
    /// Shape and tool flag for the dependency filter.
    attribute: QueryAttribute,
}

fn is_null(value: &AttrValue) -> bool {
    matches!(value, AttrValue::Label(None) | AttrValue::Output(None))
}

/// Type.getDefaultValue: the value of a `None` select() branch.
fn type_default(ty: AttrType) -> AttrValue {
    match ty {
        AttrType::Int => AttrValue::Int(0),
        AttrType::String => AttrValue::String(String::new()),
        AttrType::Label | AttrType::NodepLabel => AttrValue::Label(None),
        AttrType::Output => AttrValue::Output(None),
        AttrType::StringList | AttrType::License => AttrValue::Strings(Vec::new()),
        AttrType::LabelList | AttrType::NodepLabelList => AttrValue::Labels(Vec::new()),
        AttrType::OutputList => AttrValue::Outputs(Vec::new()),
        AttrType::StringDict => AttrValue::StringDict(Vec::new()),
        AttrType::LabelListDict => AttrValue::LabelListDict(Vec::new()),
        AttrType::StringListDict => AttrValue::StringListDict(Vec::new()),
        AttrType::Bool => AttrValue::Bool(false),
        AttrType::TriState => AttrValue::Int(-1),
        AttrType::IntList => AttrValue::Ints(Vec::new()),
        AttrType::LabelDictUnary => AttrValue::StringKeyedLabelDict(Vec::new()),
        AttrType::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
    }
}

/// One Selector: (condition, value with Type defaults for `None`, isValueSet).
type Selector = Vec<(String, AttrValue, bool)>;

fn selectors(parts: &[ConfigurablePart], ty: AttrType) -> Vec<Selector> {
    parts
        .iter()
        .map(|part| match part {
            ConfigurablePart::Plain(value) => {
                vec![("//conditions:default".to_owned(), value.clone(), true)]
            }
            ConfigurablePart::Select { branches, .. } => branches
                .iter()
                .map(|(key, value)| match value {
                    Some(value) => (key.clone(), value.clone(), true),
                    None => (key.clone(), type_default(ty), false),
                })
                .collect(),
        })
        .collect()
}

/// Type.concat of two values (lists and strings concatenate, dicts merge).
fn concat(left: &AttrValue, right: &AttrValue) -> AttrValue {
    use AttrValue as V;
    match (left, right) {
        (V::String(a), V::String(b)) => V::String(format!("{a}{b}")),
        (V::Strings(a), V::Strings(b)) => V::Strings([a.clone(), b.clone()].concat()),
        (V::Labels(a), V::Labels(b)) => V::Labels([a.clone(), b.clone()].concat()),
        (V::Outputs(a), V::Outputs(b)) => V::Outputs([a.clone(), b.clone()].concat()),
        (V::Ints(a), V::Ints(b)) => V::Ints([a.clone(), b.clone()].concat()),
        (V::StringDict(a), V::StringDict(b)) => V::StringDict(merge_entries(a, b)),
        (V::StringListDict(a), V::StringListDict(b)) => V::StringListDict(merge_entries(a, b)),
        (V::LabelListDict(a), V::LabelListDict(b)) => V::LabelListDict(merge_entries(a, b)),
        (V::StringKeyedLabelDict(a), V::StringKeyedLabelDict(b)) => {
            V::StringKeyedLabelDict(merge_entries(a, b))
        }
        (V::LabelKeyedStringDict(a), V::LabelKeyedStringDict(b)) => {
            V::LabelKeyedStringDict(merge_entries(a, b))
        }
        (_, right) => right.clone(),
    }
}

fn merge_entries<V: Clone>(a: &[(String, V)], b: &[(String, V)]) -> Vec<(String, V)> {
    let mut merged = a.to_vec();
    for (key, value) in b {
        match merged.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value.clone(),
            None => merged.push((key.clone(), value.clone())),
        }
    }
    merged
}

/// AggregatingAttributeMapper.getAllValues (packages/AggregatingAttributeMapper.java:610-690).
fn all_values(selectors: &[Selector], may_treat_multiple_as_none: bool) -> Vec<AttrValue> {
    if selectors.is_empty() {
        return Vec::new();
    }
    if selectors.len() == 1 {
        return selectors[0]
            .iter()
            .filter(|(_, value, _)| !is_null(value))
            .map(|(_, value, _)| value.clone())
            .collect();
    }
    let keys = selectors
        .iter()
        .map(|selector| {
            let mut keys = selector
                .iter()
                .map(|(key, _, _)| key.clone())
                .collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .collect::<Vec<_>>();
    let value_of = |offset: usize, key: &str| {
        selectors[offset]
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, value, _)| value.clone())
            .unwrap_or(AttrValue::Label(None))
    };
    // ArrayDeque push/pop: last pushed first.
    let mut nodes: Vec<(usize, String, AttrValue)> = Vec::new();
    for (key, value, _) in &selectors[0] {
        nodes.push((0, key.clone(), value.clone()));
    }
    let mut bound: HashMap<Vec<String>, (String, usize)> = HashMap::new();
    let mut result = Vec::new();
    let mut found = false;
    while let Some((offset, key, so_far)) = nodes.pop() {
        let next = offset + 1;
        if next >= selectors.len() {
            if !is_null(&so_far) {
                if found && may_treat_multiple_as_none {
                    return Vec::new();
                }
                found = true;
                result.push(so_far);
            }
            continue;
        }
        if let Some((bound_key, bound_offset)) = bound.get(&keys[next]).cloned()
            && bound_offset < offset
        {
            let value = concat(&so_far, &value_of(next, &bound_key));
            nodes.push((next, bound_key, value));
            continue;
        }
        bound.insert(keys[offset].clone(), (key.clone(), offset));
        if keys[offset] == keys[next] {
            let value = concat(&so_far, &value_of(next, &key));
            nodes.push((next, key, value));
            continue;
        }
        for (next_key, value, _) in &selectors[next] {
            nodes.push((next, next_key.clone(), concat(&so_far, value)));
        }
    }
    result
}

fn list_items(value: &AttrValue) -> Vec<AttrValue> {
    match value {
        AttrValue::Strings(items) => items.iter().cloned().map(AttrValue::String).collect(),
        AttrValue::Labels(items) => items
            .iter()
            .cloned()
            .map(|label| AttrValue::Label(Some(label)))
            .collect(),
        AttrValue::Outputs(items) => items
            .iter()
            .cloned()
            .map(|name| AttrValue::Output(Some(name)))
            .collect(),
        AttrValue::Ints(items) => items.iter().copied().map(AttrValue::Int).collect(),
        _ => Vec::new(),
    }
}

/// PossibleAttributeValues.forRuleAndAttribute (query2/query/output/PossibleAttributeValues.java).
fn possible_values(attr: &OutAttr, may_treat_multiple_as_none: bool) -> Vec<AttrValue> {
    match &attr.raw {
        Raw::Plain(value) => value.iter().filter(|v| !is_null(v)).cloned().collect(),
        Raw::Select(parts) => {
            let selectors = selectors(parts, attr.ty);
            if attr.ty == AttrType::LabelList {
                // getReachableLabels(attr, includeSelectKeys=false).
                let mut labels: Vec<String> = Vec::new();
                for selector in &selectors {
                    for (_, value, _) in selector {
                        if let AttrValue::Labels(items) = value {
                            for item in items {
                                if !labels.contains(item) {
                                    labels.push(item.clone());
                                }
                            }
                        }
                    }
                }
                return vec![AttrValue::Labels(labels)];
            }
            if attr.ty.is_list() {
                // getConcatenatedSelectorListsOfListType.
                return selectors
                    .iter()
                    .map(|selector| {
                        selector
                            .iter()
                            .map(|(_, value, _)| value.clone())
                            .reduce(|a, b| concat(&a, &b))
                            .unwrap_or_else(|| type_default(attr.ty))
                    })
                    .collect();
            }
            all_values(&selectors, may_treat_multiple_as_none)
        }
    }
}

fn is_scalar(ty: AttrType) -> bool {
    matches!(
        ty,
        AttrType::Int
            | AttrType::String
            | AttrType::Label
            | AttrType::NodepLabel
            | AttrType::Output
            | AttrType::Bool
            | AttrType::TriState
            | AttrType::License
    )
}

/// java.lang.String.hashCode over Bazel's internal (Latin-1 of UTF-8) form.
fn java_hash(text: &str) -> i32 {
    text.bytes()
        .fold(0i32, |h, b| h.wrapping_mul(31).wrapping_add(i32::from(b)))
}

/// java.util.HashMap iteration order of `entries` inserted in order
/// (bucket index of the spread hash, insertion order within a bucket).
fn java_hash_map_order<V>(entries: Vec<(String, V)>) -> Vec<(String, V)> {
    let mut capacity = 16usize;
    while entries.len() > capacity * 3 / 4 {
        capacity *= 2;
    }
    let mut indexed = entries
        .into_iter()
        .enumerate()
        .map(|(index, (key, value))| {
            let h = java_hash(&key) as u32;
            let spread = h ^ (h >> 16);
            ((spread as usize) & (capacity - 1), index, key, value)
        })
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(bucket, index, _, _)| (*bucket, *index));
    indexed
        .into_iter()
        .map(|(_, _, key, value)| (key, value))
        .collect()
}

fn hash_merge<V: Clone>(dicts: impl Iterator<Item = Vec<(String, V)>>) -> Vec<(String, V)> {
    let mut merged: Vec<(String, V)> = Vec::new();
    for dict in dicts {
        merged = merge_entries(&merged, &dict);
    }
    java_hash_map_order(merged)
}

/// ProtoOutputFormatter.getFlattenedAttributeValues.
fn flattened(attr: &OutAttr) -> Option<AttrValue> {
    use AttrValue as V;
    let mut values = possible_values(attr, is_scalar(attr.ty));
    if values.len() == 1 {
        return values.pop();
    }
    if is_scalar(attr.ty) {
        return None;
    }
    if attr.ty.is_list() {
        return values
            .into_iter()
            .reduce(|a, b| concat(&a, &b))
            .or_else(|| Some(type_default(attr.ty)));
    }
    let dicts = values.into_iter();
    Some(match attr.ty {
        AttrType::StringDict => V::StringDict(hash_merge(dicts.map(|v| match v {
            V::StringDict(d) => d,
            _ => Vec::new(),
        }))),
        AttrType::StringListDict => V::StringListDict(hash_merge(dicts.map(|v| match v {
            V::StringListDict(d) => d,
            _ => Vec::new(),
        }))),
        AttrType::LabelListDict => V::LabelListDict(hash_merge(dicts.map(|v| match v {
            V::LabelListDict(d) => d,
            _ => Vec::new(),
        }))),
        AttrType::LabelDictUnary => V::StringKeyedLabelDict(hash_merge(dicts.map(|v| match v {
            V::StringKeyedLabelDict(d) => d,
            _ => Vec::new(),
        }))),
        // Label keys hash as their canonical text here (Label.hashCode is
        // not modelled).
        _ => V::LabelKeyedStringDict(hash_merge(dicts.map(|v| match v {
            V::LabelKeyedStringDict(d) => d,
            _ => Vec::new(),
        }))),
    })
}

/// ProtoUtils.getDiscriminatorFromType.
fn discriminator(ty: AttrType) -> (i32, &'static str) {
    match ty {
        AttrType::Int => (1, "INTEGER"),
        AttrType::String | AttrType::NodepLabel => (2, "STRING"),
        AttrType::Label => (3, "LABEL"),
        AttrType::Output => (4, "OUTPUT"),
        AttrType::StringList | AttrType::NodepLabelList => (5, "STRING_LIST"),
        AttrType::LabelList => (6, "LABEL_LIST"),
        AttrType::OutputList => (7, "OUTPUT_LIST"),
        AttrType::License => (9, "LICENSE"),
        AttrType::StringDict => (10, "STRING_DICT"),
        AttrType::LabelListDict => (12, "LABEL_LIST_DICT"),
        AttrType::StringListDict => (13, "STRING_LIST_DICT"),
        AttrType::Bool => (14, "BOOLEAN"),
        AttrType::TriState => (15, "TRISTATE"),
        AttrType::IntList => (16, "INTEGER_LIST"),
        AttrType::LabelDictUnary => (19, "LABEL_DICT_UNARY"),
        AttrType::LabelKeyedStringDict => (21, "LABEL_KEYED_STRING_DICT"),
    }
}

/// AttributeFormatter.maybeSetNoDep.
fn nodep(ty: AttrType) -> Option<bool> {
    match ty {
        AttrType::String
        | AttrType::Label
        | AttrType::Output
        | AttrType::StringList
        | AttrType::LabelList
        | AttrType::LabelDictUnary
        | AttrType::LabelKeyedStringDict
        | AttrType::OutputList => Some(false),
        AttrType::NodepLabel | AttrType::NodepLabelList => Some(true),
        _ => None,
    }
}

/// Field numbers of the value fields of Build.Attribute and SelectorEntry.
struct ValueFields {
    int: u32,
    string: u32,
    boolean: u32,
    tristate: u32,
    string_dict: u32,
    string_list_dict: u32,
    int_list: u32,
    label_dict_unary: u32,
    label_keyed_string_dict: u32,
    /// AttributeBuilderAdapter's encodeBooleanAndTriStateAsIntegerAndString.
    legacy_encoding: bool,
}

const ATTRIBUTE_FIELDS: ValueFields = ValueFields {
    int: 3,
    string: 5,
    boolean: 14,
    tristate: 15,
    string_dict: 8,
    string_list_dict: 11,
    int_list: 17,
    label_dict_unary: 19,
    label_keyed_string_dict: 22,
    legacy_encoding: true,
};

const ENTRY_FIELDS: ValueFields = ValueFields {
    int: 2,
    string: 3,
    boolean: 4,
    tristate: 5,
    string_dict: 8,
    string_list_dict: 11,
    int_list: 13,
    label_dict_unary: 15,
    label_keyed_string_dict: 17,
    legacy_encoding: false,
};

/// The License of a `licenses` value (License.parseLicense): license types
/// upper-cased, `exception=` entries as exceptions; empty is NONE.
fn license(values: &[String]) -> (Vec<String>, Vec<String>) {
    let mut types = Vec::new();
    let mut exceptions = Vec::new();
    for value in values {
        if let Some(exception) = value.strip_prefix("exception=") {
            exceptions.push(exception.to_owned());
        } else {
            types.push(value.to_ascii_uppercase());
        }
    }
    if types.is_empty() {
        types.push("NONE".to_owned());
    }
    types.sort();
    types.dedup();
    (types, exceptions)
}

/// The structured-output options (QueryOptions.java, CommonQueryOptions.java).
#[derive(Clone, Debug)]
pub struct FormatOptions {
    pub xml_line_numbers: bool,
    pub xml_default_values: bool,
    pub proto_default_values: bool,
    pub proto_flatten_selects: bool,
    pub proto_locations: bool,
    pub proto_rule_inputs_and_outputs: bool,
    pub proto_synthetic_attribute_hash: bool,
    pub proto_instantiation_stack: bool,
    pub proto_definition_stack: bool,
    pub proto_starlark_rule_env: bool,
    pub proto_attribute_source_aspects: bool,
    pub proto_rule_classes: bool,
    /// `--proto:output_rule_attrs` (default `all`).
    pub proto_output_rule_attrs: Vec<String>,
    /// `--aspect_deps`: off, conservative (default) or precise.
    pub aspect_deps: String,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            xml_line_numbers: true,
            xml_default_values: false,
            proto_default_values: true,
            proto_flatten_selects: true,
            proto_locations: true,
            proto_rule_inputs_and_outputs: true,
            proto_synthetic_attribute_hash: false,
            proto_instantiation_stack: false,
            proto_definition_stack: false,
            proto_starlark_rule_env: true,
            proto_attribute_source_aspects: false,
            proto_rule_classes: false,
            proto_output_rule_attrs: vec!["all".to_owned()],
            aspect_deps: "conservative".to_owned(),
        }
    }
}

impl FormatOptions {
    /// Parse one structured-output boolean option.
    pub(super) fn parse_flag(&mut self, arg: &str) -> Option<Result<(), String>> {
        let flags: [(&str, &mut bool); 12] = [
            ("xml:line_numbers", &mut self.xml_line_numbers),
            ("xml:default_values", &mut self.xml_default_values),
            ("proto:default_values", &mut self.proto_default_values),
            ("proto:flatten_selects", &mut self.proto_flatten_selects),
            ("proto:locations", &mut self.proto_locations),
            (
                "proto:rule_inputs_and_outputs",
                &mut self.proto_rule_inputs_and_outputs,
            ),
            (
                "proto:include_synthetic_attribute_hash",
                &mut self.proto_synthetic_attribute_hash,
            ),
            (
                "proto:instantiation_stack",
                &mut self.proto_instantiation_stack,
            ),
            ("proto:definition_stack", &mut self.proto_definition_stack),
            (
                "proto:include_starlark_rule_env",
                &mut self.proto_starlark_rule_env,
            ),
            (
                "proto:include_attribute_source_aspects",
                &mut self.proto_attribute_source_aspects,
            ),
            ("proto:rule_classes", &mut self.proto_rule_classes),
        ];
        for (name, slot) in flags {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|value| *slot = value));
            }
        }
        None
    }
}

/// The digest of Bazel 9.3.0's bundled `@_builtins` (StarlarkBuiltinsValue
/// transitiveDigest of `@_builtins//:exports.bzl`). Bundled builtins live
/// in ConfiguredRuleClassProvider.BundledFileSystem, whose file digest is the
/// SHA-256 of the path `/virtual_builtins_bzl/<file>`, so the value depends
/// only on the builtins file names and load statements. Recomputed from the
/// genuine server jar by tests/fixtures/query_outputs/builtins_digest.py.
const BUILTINS_DIGEST: [u8; 32] =
    hex32("b18c6d6c285e248c230bf72deb29c34096852abf56327479ef422bf261c64717");

const fn hex32(text: &str) -> [u8; 32] {
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(bytes[2 * i]) << 4) | nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => c - b'a' + 10,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// TestTimeout.getDefaultForSize (packages/TestTimeout.java).
fn timeout_for_size(size: &str) -> &'static str {
    match size {
        "small" => "short",
        "large" => "long",
        "enormous" => "eternal",
        _ => "moderate",
    }
}

impl Env<'_> {
    // ------------------------------------------------------------------
    // Shared target facts.

    fn relativize(&self, location: String, repository: &str) -> String {
        match self.engine.repositories.root(repository) {
            Ok(root) => {
                let prefix = format!("{}/", root.display());
                location
                    .strip_prefix(&prefix)
                    .map(str::to_owned)
                    .unwrap_or(location)
            }
            Err(_) => location,
        }
    }

    /// The target kind the formatters see: `buildfiles`/`loadfiles` return
    /// a package's own BUILD file as the real InputFile.
    fn format_kind(&self, label: &Label) -> Kind {
        let kind = self.kind(label);
        if kind == Kind::Load
            && self.load_targets.get(label) == Some(&label.package_id())
            && self.is_build_file(label)
        {
            return Kind::Input;
        }
        kind
    }

    /// FormatUtils.getLocation for every target kind, including the
    /// FakeLoadTarget of `buildfiles`/`loadfiles` (its loading package's
    /// BUILD file, without line and column).
    fn format_location(&self, label: &Label) -> String {
        if self.format_kind(label) == Kind::Load {
            let Some(owner) = self.load_targets.get(label) else {
                return String::new();
            };
            let location = self.engine.packages[owner].build_file.display().to_string();
            return if self.options.relative_locations {
                self.relativize(location, &owner.repository)
            } else {
                location
            };
        }
        if self.kind(label) == Kind::Load {
            // A package's own BUILD file reached through buildfiles().
            let location = format!("{}:1:1", self.package(label).build_file.display());
            return if self.options.relative_locations {
                self.relativize(location, &label.repository)
            } else {
                location
            };
        }
        self.location(label)
    }

    /// Package.getOrComputeTransitivelyLoadedStarlarkFiles: the BUILD
    /// file's loads, depth first, each file once.
    fn transitive_loads(&self, label: &Label) -> Vec<Label> {
        let package = self.package(label);
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = package
            .direct_loads
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        while let Some(next) = stack.pop() {
            if !seen.insert(next.clone()) {
                continue;
            }
            if let Some(loads) = package.load_graph.get(&next) {
                stack.extend(loads.iter().rev().cloned());
            }
            result.push(next);
        }
        result
    }

    fn is_build_file(&self, label: &Label) -> bool {
        self.engine.packages.contains_key(&label.package_id())
            && self.build_file_name(&label.package_id()).as_deref() == Some(label.name.as_str())
    }

    fn package_features(&self, label: &Label) -> Vec<String> {
        let features = &self.package(label).args.features;
        features
            .on
            .iter()
            .cloned()
            .chain(features.off.iter().map(|f| format!("-{f}")))
            .collect()
    }

    fn label_text(&self, owner: &Label, text: &str) -> String {
        self.parse_label(owner, text)
            .map(|label| self.print_label(&label))
            .unwrap_or_else(|_| text.to_owned())
    }

    fn canonical_text(&self, owner: &Label, text: &str) -> String {
        self.parse_label(owner, text)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or_else(|_| text.to_owned())
    }

    fn output_text(&self, owner: &Label, name: &str) -> String {
        self.print_label(&Label {
            repository: owner.repository.clone(),
            package: owner.package.clone(),
            name: name.to_owned(),
        })
    }

    /// Rule.getAttributes with the values the formatters see.
    fn out_attributes(&mut self, label: &Label) -> Vec<OutAttr> {
        let attributes = self.rule_attributes(label);
        let rule = self.rule(label).clone();
        let package_license = self.package(label).args.license.clone();
        let mut out = Vec::new();
        for attribute in attributes.iter() {
            let mut explicit_override = None;
            let mut raw = match &attribute.select {
                Some(parts) => Raw::Select(parts.clone()),
                None => Raw::Plain(attribute.value.clone()),
            };
            if attribute.name == "visibility" {
                raw = Raw::Plain(Some(AttrValue::Labels(
                    rule.visibility
                        .declared_labels()
                        .iter()
                        .map(crate::rule_declarations::canonical_form)
                        .collect(),
                )));
            }
            if attribute.ty == AttrType::License && attribute.name == "licenses" {
                let explicit =
                    rule.native
                        .as_ref()
                        .and_then(|native| match native.attrs.get("licenses") {
                            Some(AttrValue::Strings(values)) => Some(values.clone()),
                            _ => None,
                        });
                let values = explicit
                    .or_else(|| package_license.clone())
                    .unwrap_or_else(|| vec!["none".to_owned()]);
                raw = Raw::Plain(Some(AttrValue::Strings(values)));
            }
            if attribute.name == "visibility" {
                explicit_override = Some(rule.explicit_visibility);
            }
            if attribute.name == "stamp"
                && matches!(rule.kind, RuleKind::Genrule)
                && !attribute.explicit
            {
                raw = Raw::Plain(Some(AttrValue::Int(0)));
            }
            if attribute.name == "shard_count" && !attribute.explicit && rule.custom.is_some() {
                raw = Raw::Plain(Some(AttrValue::Int(-1)));
            }
            if attribute.name == "timeout" && !attribute.explicit && rule.custom.is_some() {
                raw = Raw::Plain(Some(AttrValue::String(
                    timeout_for_size(&query_attrs::rule_size(&rule)).to_owned(),
                )));
            }
            if attribute.name == "size" && !attribute.explicit && rule.custom.is_some() {
                raw = Raw::Plain(Some(AttrValue::String(query_attrs::rule_size(&rule))));
            }
            out.push(OutAttr {
                name: attribute.name.clone(),
                ty: attribute.ty,
                explicit: explicit_override.unwrap_or(attribute.explicit),
                raw,
                attribute: attribute.clone(),
            });
        }
        let implicit = |name: &str, ty: AttrType, value: AttrValue| OutAttr {
            name: name.to_owned(),
            ty,
            explicit: false,
            raw: Raw::Plain(Some(value)),
            attribute: QueryAttribute {
                name: name.to_owned(),
                public_name: name.to_owned(),
                shape: Shape::Other,
                ty,
                explicit: false,
                tool: false,
                value: None,
                select: None,
                default: None,
                aspects: Vec::new(),
            },
        };
        if matches!(rule.kind, RuleKind::Native("test_suite"))
            && !out.iter().any(|attr| attr.name == "$implicit_tests")
        {
            out.push(implicit(
                "$implicit_tests",
                AttrType::LabelList,
                AttrValue::Labels(Vec::new()),
            ));
        }
        // Rule-class positions of the private attributes the native
        // attribute model leaves out (BaseRuleClasses.java:226-299,
        // ConfigRuleClasses.java, GenRuleBaseRule.java, BazelGenRuleRule.java).
        let insert =
            |out: &mut Vec<OutAttr>, attr: OutAttr, after: Option<&str>, before: Option<&str>| {
                let position = after
                    .and_then(|name| out.iter().position(|a| a.name == name).map(|i| i + 1))
                    .or_else(|| before.and_then(|name| out.iter().position(|a| a.name == name)))
                    .unwrap_or(out.len());
                out.insert(position, attr);
            };
        if rule.custom.is_none() {
            if !matches!(
                rule.kind,
                RuleKind::Native("alias" | "label_flag" | "label_setting")
            ) {
                insert(
                    &mut out,
                    implicit(
                        ":action_listener",
                        AttrType::LabelList,
                        AttrValue::Labels(Vec::new()),
                    ),
                    Some("features"),
                    None,
                );
            }
            if matches!(rule.kind, RuleKind::Native("config_setting")) {
                insert(
                    &mut out,
                    implicit(
                        ":flag_alias_settings",
                        AttrType::LabelList,
                        AttrValue::Labels(Vec::new()),
                    ),
                    None,
                    Some("constraint_values"),
                );
            }
            if matches!(rule.kind, RuleKind::Genrule) {
                let executable = rule.native.as_deref().is_some_and(|native| {
                    matches!(native.attrs.get("executable"), Some(AttrValue::Bool(true)))
                });
                insert(
                    &mut out,
                    implicit(
                        "$is_executable",
                        AttrType::Bool,
                        AttrValue::Bool(executable),
                    ),
                    Some("executable"),
                    None,
                );
                if let Some(index) = out.iter().position(|a| a.name == "$genrule_setup") {
                    let setup = out.remove(index);
                    insert(&mut out, setup, None, Some("stamp"));
                }
            }
        }
        out
    }

    /// Write `value` of type `ty` into the value fields of `msg`.
    fn write_value(
        &self,
        msg: &mut Msg,
        fields: &ValueFields,
        ty: AttrType,
        value: &AttrValue,
        owner: &Label,
    ) {
        use AttrValue as V;
        match (ty, value) {
            (_, v) if is_null(v) => {}
            (AttrType::Int, V::Int(n)) => msg.int(fields.int, "intValue", *n),
            (AttrType::String, V::String(text)) => msg.str(fields.string, "stringValue", text),
            (AttrType::Label | AttrType::NodepLabel, V::Label(Some(text))) => {
                msg.str(fields.string, "stringValue", self.label_text(owner, text))
            }
            (AttrType::Label | AttrType::NodepLabel, V::Labels(items)) if items.len() == 1 => msg
                .str(
                    fields.string,
                    "stringValue",
                    self.label_text(owner, &items[0]),
                ),
            (AttrType::Output, V::Output(Some(name))) => {
                msg.str(fields.string, "stringValue", self.output_text(owner, name))
            }
            (AttrType::StringList, V::Strings(items)) => {
                msg.strs(6, "stringListValue", items.iter().cloned())
            }
            (AttrType::LabelList | AttrType::NodepLabelList, V::Labels(items)) => msg.strs(
                6,
                "stringListValue",
                items.iter().map(|item| self.label_text(owner, item)),
            ),
            (AttrType::OutputList, V::Outputs(names)) => msg.strs(
                6,
                "stringListValue",
                names.iter().map(|name| self.output_text(owner, name)),
            ),
            (AttrType::IntList, V::Ints(items)) => {
                for item in items {
                    msg.put(fields.int_list, "intListValue", true, Value::Int(*item));
                }
            }
            (AttrType::Bool, V::Bool(b)) => {
                msg.boolean(fields.boolean, "booleanValue", *b);
                if fields.legacy_encoding {
                    msg.str(
                        fields.string,
                        "stringValue",
                        if *b { "true" } else { "false" },
                    );
                    msg.int(fields.int, "intValue", i32::from(*b));
                }
            }
            (AttrType::Bool, V::Int(n)) => {
                self.write_value(msg, fields, ty, &V::Bool(*n != 0), owner)
            }
            (AttrType::TriState, V::Int(n)) => {
                let (number, name, text) = match n {
                    1 => (1, "YES", "yes"),
                    0 => (0, "NO", "no"),
                    _ => (2, "AUTO", "auto"),
                };
                msg.enumeration(fields.tristate, "tristateValue", number, name);
                if fields.legacy_encoding {
                    msg.str(fields.string, "stringValue", text);
                    msg.int(fields.int, "intValue", *n);
                }
            }
            (AttrType::TriState, V::Bool(b)) => {
                self.write_value(msg, fields, ty, &V::Int(i32::from(*b)), owner)
            }
            (AttrType::License, V::Strings(values)) => {
                let (types, exceptions) = license(values);
                let mut license = Msg::default();
                license.strs(1, "licenseType", types);
                license.strs(
                    2,
                    "exception",
                    exceptions.iter().map(|e| self.canonical_text(owner, e)),
                );
                msg.msg(7, "license", license);
            }
            (AttrType::StringDict, V::StringDict(entries)) => {
                for (key, value) in entries {
                    let mut entry = Msg::default();
                    entry.str(1, "key", key);
                    entry.str(2, "value", value);
                    msg.msgs(fields.string_dict, "stringDictValue", entry);
                }
            }
            (AttrType::StringListDict, V::StringListDict(entries)) => {
                for (key, values) in entries {
                    let mut entry = Msg::default();
                    entry.str(1, "key", key);
                    entry.strs(2, "value", values.iter().cloned());
                    msg.msgs(fields.string_list_dict, "stringListDictValue", entry);
                }
            }
            // AttributeFormatter writes LABEL_LIST_DICT into
            // string_list_dict_value with Label.toString values.
            (AttrType::LabelListDict, V::LabelListDict(entries)) => {
                for (key, values) in entries {
                    let mut entry = Msg::default();
                    entry.str(1, "key", key);
                    entry.strs(
                        2,
                        "value",
                        values.iter().map(|value| self.canonical_text(owner, value)),
                    );
                    msg.msgs(fields.string_list_dict, "stringListDictValue", entry);
                }
            }
            (AttrType::LabelDictUnary, V::StringKeyedLabelDict(entries)) => {
                for (key, value) in entries {
                    let mut entry = Msg::default();
                    entry.str(1, "key", key);
                    entry.str(2, "value", self.label_text(owner, value));
                    msg.msgs(fields.label_dict_unary, "labelDictUnaryValue", entry);
                }
            }
            (AttrType::LabelKeyedStringDict, V::LabelKeyedStringDict(entries)) => {
                for (key, value) in entries {
                    let mut entry = Msg::default();
                    entry.str(1, "key", self.label_text(owner, key));
                    entry.str(2, "value", value);
                    msg.msgs(
                        fields.label_keyed_string_dict,
                        "labelKeyedStringDictValue",
                        entry,
                    );
                }
            }
            // A value whose shape does not match the declared type (native
            // model gaps) keeps its own shape rather than being dropped.
            (_, V::String(text)) => msg.str(fields.string, "stringValue", text),
            (_, V::Strings(items)) => msg.strs(6, "stringListValue", items.iter().cloned()),
            (_, V::Labels(items)) => msg.strs(
                6,
                "stringListValue",
                items.iter().map(|item| self.label_text(owner, item)),
            ),
            (_, V::Label(Some(text))) => {
                msg.str(fields.string, "stringValue", self.label_text(owner, text))
            }
            (_, V::Int(n)) => msg.int(fields.int, "intValue", *n),
            _ => {}
        }
    }

    /// AttributeFormatter.getAttributeProto.
    fn attribute_proto(
        &self,
        attr: &OutAttr,
        flatten: bool,
        explicit: bool,
        legacy: bool,
        owner: &Label,
    ) -> Msg {
        let mut msg = Msg::default();
        msg.str(1, "name", &attr.name);
        msg.boolean(13, "explicitlySpecified", explicit);
        if let Some(nodep) = nodep(attr.ty) {
            msg.boolean(20, "nodep", nodep);
        }
        let fields = ValueFields {
            legacy_encoding: legacy,
            ..ATTRIBUTE_FIELDS
        };
        match (&attr.raw, flatten) {
            (Raw::Select(parts), false) => {
                msg.enumeration(2, "type", 20, "SELECTOR_LIST");
                let (number, name) = discriminator(attr.ty);
                let mut list = Msg::default();
                list.enumeration(1, "type", number, name);
                for (part, selector) in parts.iter().zip(selectors(parts, attr.ty)) {
                    let mut element = Msg::default();
                    for (key, value, set) in &selector {
                        let mut entry = Msg::default();
                        entry.str(1, "label", self.label_text(owner, key));
                        self.write_value(&mut entry, &ENTRY_FIELDS, attr.ty, value, owner);
                        entry.boolean(16, "isDefaultValue", !set);
                        element.msgs(1, "entries", entry);
                    }
                    let has_default = selector
                        .iter()
                        .any(|(key, _, _)| query_attrs::is_default_condition(key));
                    element.boolean(2, "hasDefaultValue", has_default);
                    let no_match = match part {
                        ConfigurablePart::Select { no_match_error, .. } => no_match_error.clone(),
                        ConfigurablePart::Plain(_) => String::new(),
                    };
                    element.str(3, "noMatchError", no_match);
                    list.msgs(2, "elements", element);
                }
                msg.msg(21, "selectorList", list);
            }
            _ => {
                let (number, name) = discriminator(attr.ty);
                msg.enumeration(2, "type", number, name);
                let value = match &attr.raw {
                    Raw::Plain(value) => value.clone(),
                    Raw::Select(_) => flattened(attr),
                };
                if let Some(value) = value {
                    self.write_value(&mut msg, &fields, attr.ty, &value, owner);
                }
            }
        }
        msg
    }

    fn includes_attribute(&self, attr: &OutAttr) -> bool {
        let format = &self.options.format;
        let selected = match format.proto_output_rule_attrs.as_slice() {
            [all] if all == "all" => true,
            names => names.iter().any(|name| name == &attr.name),
        };
        (format.proto_default_values || attr.explicit) && selected
    }

    /// BzlLoadValue.getTransitiveDigest of a `.bzl` file loaded for BUILD
    /// evaluation (BzlLoadFunction.java:858-879): SHA-256 over the file's
    /// SHA-256, its loads' transitive digests in load order and the
    /// `@_builtins` digest.
    fn bzl_digest(
        &self,
        label: &Label,
        graph: &crate::loading::LoadGraph,
        memo: &mut HashMap<Label, [u8; 32]>,
    ) -> Result<[u8; 32], String> {
        use sha2::{Digest, Sha256};
        if let Some(digest) = memo.get(label) {
            return Ok(*digest);
        }
        let path = self
            .engine
            .repositories
            .root(&label.repository)?
            .join(&label.package)
            .join(&label.name);
        let bytes = crate::observed_fs::read(&path)
            .map_err(|e| format!("cannot digest {}: {e}", path.display()))?;
        let mut hash = Sha256::new();
        hash.update(Sha256::digest(&bytes));
        for load in graph.get(label).into_iter().flatten() {
            hash.update(self.bzl_digest(load, graph, memo)?);
        }
        hash.update(BUILTINS_DIGEST);
        let digest: [u8; 32] = hash.finalize().into();
        memo.insert(label.clone(), digest);
        Ok(digest)
    }

    /// RuleClass.getRuleDefinitionEnvironmentDigest of a Starlark rule.
    fn rule_definition_digest(&mut self, label: &Label) -> Eval<Option<[u8; 32]>> {
        let rule = self.rule(label);
        let Some(custom) = &rule.custom else {
            return Ok(None);
        };
        let definition = Label::parse_canonical(&custom.declaration.definition_environment_label)
            .map_err(super::Failure::Fatal)?;
        if let Some(digest) = self.digests.get(&definition) {
            return Ok(Some(*digest));
        }
        let graph = self.package(label).load_graph.clone();
        let mut memo = std::mem::take(&mut self.digests);
        let digest = self.bzl_digest(&definition, &graph, &mut memo);
        self.digests = memo;
        digest.map(Some).map_err(super::Failure::Fatal)
    }

    /// SyntheticAttributeHashCalculator.compute
    /// (query2/query/output/SyntheticAttributeHashCalculator.java).
    fn synthetic_attribute_hash(
        &self,
        label: &Label,
        attributes: &[OutAttr],
        serialized: &HashMap<String, Msg>,
        digest: Option<[u8; 32]>,
    ) -> String {
        use sha2::{Digest, Sha256};
        let mut out = Vec::new();
        if let Some(digest) = digest
            && self.options.format.proto_starlark_rule_env
        {
            varint(&mut out, 32);
            out.extend_from_slice(&digest);
        }
        for attr in attributes {
            if attr.name == "generator_location" {
                continue;
            }
            let msg = match (&attr.raw, serialized.get(&attr.name)) {
                (Raw::Plain(_), Some(msg)) => msg.clone(),
                _ => self.attribute_proto(attr, false, false, false, label),
            };
            msg.encode(&mut out);
        }
        // writeBoolNoTag(containsErrors), writeStringNoTag(extraData = "").
        out.push(0);
        out.push(0);
        base64(&Sha256::digest(&out))
    }

    /// Aspect dependencies of a rule by aspect (AspectResolver
    /// computeAspectDependencies): (aspect name, [(attribute, type, labels)]).
    #[allow(clippy::type_complexity)]
    fn aspect_attributes(
        &mut self,
        label: &Label,
    ) -> Eval<Vec<(String, Vec<(String, AttrType, Vec<Label>)>)>> {
        let mode = self.options.format.aspect_deps.clone();
        if mode == "off" || self.kind(label) != Kind::Rule {
            return Ok(Vec::new());
        }
        let attributes = self.rule_attributes(label);
        let mut result: Vec<(String, Vec<(String, AttrType, Vec<Label>)>)> = Vec::new();
        for attribute in attributes.iter() {
            for aspect in &attribute.aspects {
                if mode == "precise" {
                    // PreciseAspectResolver: only when a dependency of the
                    // attribute satisfies the aspect.
                    let mut applies = false;
                    for text in attribute.labels() {
                        let Ok(dep) = self.parse_label(label, &text) else {
                            continue;
                        };
                        if let Ok(Ok(Kind::Rule)) = self.lookup(&dep)
                            && query_attrs::aspect_applies(aspect, self.rule(&dep))
                        {
                            applies = true;
                        }
                    }
                    if !applies {
                        continue;
                    }
                }
                let mut entries = Vec::new();
                for (name, spec) in &aspect.attrs {
                    let shape = match spec.kind {
                        crate::custom_rule::AttrKind::Label => Shape::Label,
                        crate::custom_rule::AttrKind::LabelList => Shape::LabelList,
                        _ => continue,
                    };
                    let tool =
                        matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_));
                    let private = name.starts_with('_');
                    if (tool && !self.options.tool_deps) || (private && !self.options.implicit_deps)
                    {
                        continue;
                    }
                    let mut labels = Vec::new();
                    for text in query_attrs::value_labels(&spec.default, shape) {
                        if let Ok(dep) = self.parse_label(label, &text)
                            && !labels.contains(&dep)
                        {
                            labels.push(dep);
                        }
                    }
                    if labels.is_empty() {
                        continue;
                    }
                    let native_name = match name.strip_prefix('_') {
                        Some(rest) => format!("${rest}"),
                        None => name.clone(),
                    };
                    let ty = if shape == Shape::Label {
                        AttrType::Label
                    } else {
                        AttrType::LabelList
                    };
                    entries.push((native_name, ty, labels));
                }
                if entries.is_empty() {
                    continue;
                }
                match result.iter_mut().find(|(name, _)| *name == aspect.identity) {
                    Some(slot) => slot.1 = entries,
                    None => result.push((aspect.identity.clone(), entries)),
                }
            }
        }
        Ok(result)
    }

    /// Rule.getSortedLabels(dependencyFilter): the dependency attributes'
    /// labels (select keys included), sorted and distinct.
    fn sorted_rule_inputs(&mut self, label: &Label) -> Vec<Label> {
        let attributes = self.rule_attributes(label);
        let mut labels = Vec::new();
        for attribute in attributes.iter() {
            if !attribute.shape.is_dependency() || !self.options.admits(attribute) {
                continue;
            }
            for text in attribute.labels() {
                if let Ok(dep) = self.parse_label(label, &text) {
                    labels.push(dep);
                }
            }
        }
        labels.sort();
        labels.dedup();
        labels
    }

    fn rule_outputs(&self, label: &Label) -> Vec<String> {
        let mut seen = HashSet::new();
        self.rule(label)
            .outs
            .iter()
            .filter(|name| seen.insert(name.as_str().to_owned()))
            .map(|name| self.output_text(label, name))
            .collect()
    }

    fn stack_entries(&self, label: &Label, entries: &[(String, String)]) -> Vec<String> {
        entries
            .iter()
            .map(|(name, location)| {
                format!(
                    "{}: {name}",
                    self.relativize(location.clone(), &label.repository)
                )
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Proto.

    /// ProtoOutputFormatter.toTargetProtoBuffer.
    pub(super) fn proto_target(&mut self, label: &Label) -> Eval<Msg> {
        let format = self.options.format.clone();
        let mut target = Msg::default();
        match self.format_kind(label) {
            Kind::Rule => {
                let mut rule = Msg::default();
                rule.str(1, "name", self.print_label(label));
                rule.str(2, "ruleClass", self.rule(label).rule_class_name());
                if format.proto_locations {
                    rule.str(3, "location", self.format_location(label));
                }
                let attributes = self.out_attributes(label);
                let mut serialized = HashMap::new();
                let mut protos: Vec<Msg> = Vec::new();
                for attr in &attributes {
                    if !self.includes_attribute(attr) {
                        continue;
                    }
                    let msg = self.attribute_proto(
                        attr,
                        format.proto_flatten_selects,
                        attr.explicit,
                        true,
                        label,
                    );
                    serialized.insert(attr.name.clone(), msg.clone());
                    if !protos.contains(&msg) {
                        protos.push(msg);
                    }
                }
                protos.sort_by(|a, b| attr_name(a).cmp(attr_name(b)));
                for msg in protos {
                    rule.msgs(4, "attribute", msg);
                }
                let digest = self.rule_definition_digest(label)?;
                if format.proto_synthetic_attribute_hash {
                    let hash =
                        self.synthetic_attribute_hash(label, &attributes, &serialized, digest);
                    let mut msg = Msg::default();
                    msg.str(1, "name", "$internal_attr_hash");
                    msg.enumeration(2, "type", 2, "STRING");
                    msg.str(5, "stringValue", hash);
                    rule.msgs(4, "attribute", msg);
                }
                if let Some(digest) = digest {
                    let mut msg = Msg::default();
                    msg.str(1, "name", "$rule_implementation_hash");
                    msg.enumeration(2, "type", 2, "STRING");
                    msg.str(5, "stringValue", hex(&digest));
                    rule.msgs(4, "attribute", msg);
                }
                let aspects = self.aspect_attributes(label)?;
                let mut aspect_protos: Vec<Msg> = Vec::new();
                for (aspect, entries) in &aspects {
                    for (name, ty, labels) in entries {
                        let value = if *ty == AttrType::Label {
                            AttrValue::Label(Some(labels[0].to_string()))
                        } else {
                            AttrValue::Labels(labels.iter().map(ToString::to_string).collect())
                        };
                        let attr = OutAttr {
                            name: name.clone(),
                            ty: *ty,
                            explicit: false,
                            raw: Raw::Plain(Some(value)),
                            attribute: attributes[0].attribute.clone(),
                        };
                        let mut msg = self.attribute_proto(&attr, true, false, true, label);
                        if format.proto_attribute_source_aspects {
                            msg.str(23, "sourceAspectName", aspect.clone());
                        }
                        if !aspect_protos.contains(&msg) {
                            aspect_protos.push(msg);
                        }
                    }
                }
                aspect_protos.sort_by(|a, b| attr_name(a).cmp(attr_name(b)));
                for msg in aspect_protos {
                    rule.msgs(4, "attribute", msg);
                }
                if format.proto_attribute_source_aspects {
                    // Ordinary attributes carry an empty source aspect name.
                    for field in &mut rule.fields {
                        if let (4, _, _, Value::Msg(msg)) = field
                            && !msg.fields.iter().any(|f| f.0 == 23)
                            && msg.fields.iter().any(|f| f.0 == 13)
                        {
                            msg.str(23, "sourceAspectName", "");
                        }
                    }
                }
                if format.proto_rule_inputs_and_outputs {
                    let mut aspect_inputs: Vec<Label> = Vec::new();
                    for (_, entries) in &aspects {
                        for (_, _, labels) in entries {
                            for dep in labels {
                                if !aspect_inputs.contains(dep) {
                                    aspect_inputs.push(dep.clone());
                                }
                            }
                        }
                    }
                    let inputs = aspect_inputs
                        .iter()
                        .chain(self.sorted_rule_inputs(label).iter())
                        .map(|dep| self.print_label(dep))
                        .collect::<Vec<_>>();
                    rule.strs(5, "ruleInput", inputs);
                    rule.strs(6, "ruleOutput", self.rule_outputs(label));
                }
                rule.strs(7, "defaultSetting", self.package_features(label));
                if format.proto_instantiation_stack {
                    let stack = self.rule(label).call_stack.clone();
                    rule.strs(13, "instantiationStack", self.stack_entries(label, &stack));
                }
                if format.proto_definition_stack
                    && let Some(custom) = &self.rule(label).custom
                {
                    let stack = custom.declaration.definition_stack.clone();
                    rule.strs(14, "definitionStack", self.stack_entries(label, &stack));
                }
                target.enumeration(1, "type", 1, "RULE");
                target.msg(2, "rule", rule);
            }
            Kind::Output(owner) => {
                let mut file = Msg::default();
                file.str(1, "name", self.print_label(label));
                file.str(2, "generatingRule", self.print_label(&owner));
                if format.proto_locations {
                    file.str(3, "location", self.format_location(label));
                }
                target.enumeration(1, "type", 3, "GENERATED_FILE");
                target.msg(4, "generatedFile", file);
            }
            Kind::Input => {
                let mut file = Msg::default();
                file.str(1, "name", self.print_label(label));
                if format.proto_locations {
                    file.str(2, "location", self.format_location(label));
                }
                let build_file = self.is_build_file(label);
                if build_file {
                    let loads = self.transitive_loads(label);
                    file.strs(
                        3,
                        "subinclude",
                        loads.iter().map(|load| self.print_label(load)),
                    );
                    file.strs(6, "feature", self.package_features(label));
                    file.boolean(9, "packageContainsErrors", false);
                }
                let visibility = self.engine.source_visibility(label);
                file.strs(
                    4,
                    "packageGroup",
                    visibility
                        .group_labels()
                        .map(|group| self.print_label(group)),
                );
                file.strs(
                    5,
                    "visibilityLabel",
                    visibility
                        .declared_labels()
                        .iter()
                        .map(|group| self.print_label(group)),
                );
                target.enumeration(1, "type", 2, "SOURCE_FILE");
                target.msg(3, "sourceFile", file);
            }
            Kind::Load => {
                let mut file = Msg::default();
                file.str(1, "name", self.print_label(label));
                if format.proto_locations {
                    file.str(2, "location", self.format_location(label));
                }
                target.enumeration(1, "type", 2, "SOURCE_FILE");
                target.msg(3, "sourceFile", file);
            }
            Kind::PackageGroup => {
                let mut group = Msg::default();
                group.str(1, "name", self.print_label(label));
                if let RuleKind::PackageGroup(definition) = &self.rule(label).kind {
                    group.strs(
                        2,
                        "containedPackage",
                        definition.own_specs.package_strings(),
                    );
                    group.strs(
                        3,
                        "includedPackageGroup",
                        definition
                            .includes
                            .iter()
                            .map(|include| self.print_label(include)),
                    );
                }
                target.enumeration(1, "type", 4, "PACKAGE_GROUP");
                target.msg(5, "packageGroup", group);
            }
        }
        Ok(target)
    }

    // ------------------------------------------------------------------
    // XML.

    /// XmlOutputFormatter.createValueElement.
    fn xml_value(&self, ty: AttrType, values: &[AttrValue], owner: &Label) -> Xml {
        let multiple = values.len() > 1;
        if ty.is_list() {
            let element_ty = match ty {
                AttrType::StringList => AttrType::String,
                AttrType::LabelList | AttrType::NodepLabelList => AttrType::Label,
                AttrType::OutputList => AttrType::Output,
                _ => AttrType::Int,
            };
            let mut list = Xml::new("list");
            for value in values {
                for item in list_items(value) {
                    list.children.push(self.xml_value(
                        element_ty,
                        std::slice::from_ref(&item),
                        owner,
                    ));
                }
            }
            return list;
        }
        if ty.is_dict() {
            let mut dict = Xml::new("dict");
            let mut visited = HashSet::new();
            for value in values {
                let pairs: Vec<(Xml, Xml, String)> = match value {
                    AttrValue::StringDict(entries) => entries
                        .iter()
                        .map(|(k, v)| {
                            (
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(k.clone()),
                                    owner,
                                ),
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(v.clone()),
                                    owner,
                                ),
                                k.clone(),
                            )
                        })
                        .collect(),
                    AttrValue::StringListDict(entries) => entries
                        .iter()
                        .map(|(k, v)| {
                            (
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(k.clone()),
                                    owner,
                                ),
                                self.xml_value(
                                    AttrType::StringList,
                                    &[AttrValue::Strings(v.clone())],
                                    owner,
                                ),
                                k.clone(),
                            )
                        })
                        .collect(),
                    AttrValue::LabelListDict(entries) => entries
                        .iter()
                        .map(|(k, v)| {
                            (
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(k.clone()),
                                    owner,
                                ),
                                self.xml_value(
                                    AttrType::LabelList,
                                    &[AttrValue::Labels(v.clone())],
                                    owner,
                                ),
                                k.clone(),
                            )
                        })
                        .collect(),
                    AttrValue::StringKeyedLabelDict(entries) => entries
                        .iter()
                        .map(|(k, v)| {
                            (
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(k.clone()),
                                    owner,
                                ),
                                self.xml_scalar(
                                    AttrType::Label,
                                    &AttrValue::Label(Some(v.clone())),
                                    owner,
                                ),
                                k.clone(),
                            )
                        })
                        .collect(),
                    AttrValue::LabelKeyedStringDict(entries) => entries
                        .iter()
                        .map(|(k, v)| {
                            (
                                self.xml_scalar(
                                    AttrType::Label,
                                    &AttrValue::Label(Some(k.clone())),
                                    owner,
                                ),
                                self.xml_scalar(
                                    AttrType::String,
                                    &AttrValue::String(v.clone()),
                                    owner,
                                ),
                                k.clone(),
                            )
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                for (key, value, identity) in pairs {
                    if visited.insert(identity) {
                        let mut pair = Xml::new("pair");
                        pair.children.push(key);
                        pair.children.push(value);
                        dict.children.push(pair);
                    }
                }
            }
            return dict;
        }
        if ty == AttrType::License {
            let mut element = Xml::new("license");
            if multiple {
                element.attr("configurable", "yes");
            } else if let Some(AttrValue::Strings(values)) = values.first() {
                let (types, exceptions) = license(values);
                element.children.push(
                    self.xml_value(AttrType::LabelList, &[AttrValue::Labels(exceptions)], owner)
                        .named("exceptions"),
                );
                element.children.push(
                    self.xml_value(AttrType::StringList, &[AttrValue::Strings(types)], owner)
                        .named("license-types"),
                );
            }
            return element;
        }
        let mut element = Xml::new(xml_type_name(ty));
        if multiple {
            element.attr("configurable", "yes");
        } else if let Some(value) = values.first()
            && let Some(text) = self.xml_text(ty, value, owner)
        {
            element.attr("value", text);
        }
        element
    }

    fn xml_scalar(&self, ty: AttrType, value: &AttrValue, owner: &Label) -> Xml {
        self.xml_value(ty, std::slice::from_ref(value), owner)
    }

    fn xml_text(&self, ty: AttrType, value: &AttrValue, owner: &Label) -> Option<String> {
        Some(match value {
            AttrValue::Label(None) | AttrValue::Output(None) => return None,
            AttrValue::Label(Some(text)) => self.label_text(owner, text),
            AttrValue::Output(Some(name)) => self.output_text(owner, name),
            AttrValue::String(text) => text.clone(),
            AttrValue::Bool(b) => b.to_string(),
            AttrValue::Int(n) if ty == AttrType::TriState => match n {
                1 => "YES".to_owned(),
                0 => "NO".to_owned(),
                _ => "AUTO".to_owned(),
            },
            AttrValue::Int(n) if ty == AttrType::Bool => (*n != 0).to_string(),
            AttrValue::Int(n) => n.to_string(),
            AttrValue::Labels(items) if items.len() == 1 => self.label_text(owner, &items[0]),
            _ => return None,
        })
    }

    fn xml_location(&self, label: &Label) -> String {
        let location = self.format_location(label);
        if self.options.format.xml_line_numbers {
            return location;
        }
        match location.find(':') {
            Some(index) => location[..index].to_owned(),
            None => location,
        }
    }

    /// XmlOutputFormatter.createTargetElement.
    pub(super) fn xml_target(&mut self, label: &Label) -> Eval<Xml> {
        let mut element;
        match self.format_kind(label) {
            Kind::Rule => {
                element = Xml::new("rule");
                element.attr("class", self.rule(label).rule_class_name());
                let attributes = self.out_attributes(label);
                for attr in &attributes {
                    if !(attr.explicit || self.options.format.xml_default_values) {
                        continue;
                    }
                    let values = possible_values(attr, false);
                    let child = self
                        .xml_value(attr.ty, &values, label)
                        .named(attr.name.clone());
                    element.children.push(child);
                }
                for dep in self.sorted_rule_inputs(label) {
                    element
                        .children
                        .push(Xml::new("rule-input").named(self.print_label(&dep)));
                }
                let mut aspect_inputs: Vec<Label> = Vec::new();
                for (_, entries) in self.aspect_attributes(label)? {
                    for (_, _, labels) in entries {
                        for dep in labels {
                            if !aspect_inputs.contains(&dep) {
                                aspect_inputs.push(dep);
                            }
                        }
                    }
                }
                for dep in aspect_inputs {
                    element
                        .children
                        .push(Xml::new("rule-input").named(self.print_label(&dep)));
                }
                for output in self.rule_outputs(label) {
                    element.children.push(Xml::new("rule-output").named(output));
                }
                for feature in self.package_features(label) {
                    element
                        .children
                        .push(Xml::new("rule-default-setting").named(feature));
                }
            }
            Kind::PackageGroup => {
                element = Xml::new("package-group");
                if let RuleKind::PackageGroup(definition) = &self.rule(label).kind {
                    let includes = definition
                        .includes
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>();
                    element.children.push(
                        self.xml_value(AttrType::LabelList, &[AttrValue::Labels(includes)], label)
                            .named("includes"),
                    );
                    element.children.push(
                        self.xml_value(
                            AttrType::StringList,
                            &[AttrValue::Strings(definition.own_specs.package_strings())],
                            label,
                        )
                        .named("packages"),
                    );
                }
            }
            Kind::Output(owner) => {
                element = Xml::new("generated-file");
                element.attr("generating-rule", self.print_label(&owner));
            }
            Kind::Input => {
                element = Xml::new("source-file");
                if self.is_build_file(label) {
                    for load in self.transitive_loads(label) {
                        element
                            .children
                            .push(Xml::new("load").named(self.print_label(&load)));
                    }
                    for feature in self.package_features(label) {
                        element.children.push(Xml::new("feature").named(feature));
                    }
                    element.attr("package_contains_errors", "false");
                }
                let visibility = self.engine.source_visibility(label);
                for group in visibility.group_labels() {
                    element
                        .children
                        .push(Xml::new("package-group").named(self.print_label(group)));
                }
                for declared in visibility.declared_labels() {
                    element
                        .children
                        .push(Xml::new("visibility-label").named(self.print_label(&declared)));
                }
            }
            Kind::Load => element = Xml::new("source-file"),
        }
        element.attr("name", self.print_label(label));
        element.attr("location", self.xml_location(label));
        Ok(element)
    }

    /// The bytes of a structured formatter for `ordered` targets.
    pub(super) fn structured_output(&mut self, ordered: &[Label]) -> Eval<Vec<u8>> {
        let output = self.options.output.clone();
        let mut out = Vec::new();
        match output.as_str() {
            "xml" => {
                let mut text =
                    String::from("<?xml version=\"1.1\" encoding=\"UTF-8\" standalone=\"no\"?>\n");
                let mut query = Xml::new("query");
                query.attr("version", "2");
                for label in ordered {
                    query.children.push(self.xml_target(label)?);
                }
                query.write(&mut text, 0);
                out = text.into_bytes();
            }
            "proto" => {
                for label in ordered {
                    let mut bytes = Vec::new();
                    self.proto_target(label)?.encode(&mut bytes);
                    varint(&mut out, 1 << 3 | 2);
                    varint(&mut out, bytes.len() as u64);
                    out.extend_from_slice(&bytes);
                }
            }
            "streamed_proto" => {
                for label in ordered {
                    let mut bytes = Vec::new();
                    self.proto_target(label)?.encode(&mut bytes);
                    varint(&mut out, bytes.len() as u64);
                    out.extend_from_slice(&bytes);
                }
            }
            _ => {
                for label in ordered {
                    let mut text = String::new();
                    self.proto_target(label)?.json(&mut text);
                    out.extend_from_slice(text.as_bytes());
                    out.push(b'\n');
                }
            }
        }
        Ok(out)
    }
}

fn attr_name(msg: &Msg) -> &str {
    msg.fields
        .iter()
        .find_map(|(number, _, _, value)| match (number, value) {
            (1, Value::Str(name)) => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Type.toString of the scalar types XmlOutputFormatter names elements by.
fn xml_type_name(ty: AttrType) -> &'static str {
    match ty {
        AttrType::Int => "int",
        AttrType::String => "string",
        AttrType::Label | AttrType::NodepLabel => "label",
        AttrType::Output => "output",
        AttrType::Bool => "boolean",
        AttrType::TriState => "tristate",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_sign_extend_negative_int32() {
        let mut msg = Msg::default();
        msg.int(3, "intValue", -1);
        let mut out = Vec::new();
        msg.encode(&mut out);
        assert_eq!(out.len(), 11);
        assert_eq!(out[0], 3 << 3);
    }

    #[test]
    fn gson_escapes_html_characters() {
        let mut out = String::new();
        gson_string(&mut out, "a<b>&='\"\n\u{1}é");
        assert_eq!(out, r#""a\u003cb\u003e\u0026\u003d\u0027\"\n\u0001é""#);
    }

    #[test]
    fn xml_escapes_internal_latin1() {
        let mut out = String::new();
        xml_escape(&mut out, "<é>\n");
        assert_eq!(out, "&lt;Ã©&gt;&#10;");
    }

    #[test]
    fn java_hash_map_orders_by_bucket() {
        assert_eq!(java_hash("ab"), 97 * 31 + 98);
        let ordered = java_hash_map_order(vec![("b".to_owned(), 1), ("a".to_owned(), 2)]);
        assert_eq!(ordered[0].0, "a");
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
