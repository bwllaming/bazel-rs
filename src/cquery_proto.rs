//! Bazel 9.3 cquery binary formats, independent of analysis and the CLI.
//!
//! DTOs retain resolved values (including defaults and presence) in binary snapshots.
//! The caller supplies labels, full configuration checksums, output-local IDs, and
//! ordering. This module neither constructs a graph nor invents identities.
//! [`CqueryWriter`] retains only a buffered sink and one reusable record buffer.
//! `streamed_proto` frames **CqueryResult** messages, not ConfiguredTarget messages.
//!
//! Wire contract: Bazel tag 9.3.0, analysis_v2.proto, build.proto,
//! ProtoOutputFormatterCallback.java and AttributeFormatter.java. See
//! `docs/CQUERY_PROTO.md` for scope, independent fixtures and limitations.

use std::io::{self, BufWriter, Write};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Rule(Rule),
    SourceFile(SourceFile),
    GeneratedFile(GeneratedFile),
    PackageGroup(PackageGroup),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub rule_class: String,
    pub location: Option<String>,
    pub attributes: Vec<Attribute>,
    pub rule_inputs: Vec<String>,
    pub rule_outputs: Vec<String>,
    pub default_settings: Vec<String>,
    pub configured_rule_inputs: Vec<ConfiguredRuleInput>,
    pub instantiation_stack: Vec<String>,
    pub definition_stack: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub name: String,
    pub location: Option<String>,
    pub subincludes: Vec<String>,
    pub package_groups: Vec<String>,
    pub visibility_labels: Vec<String>,
    pub features: Vec<String>,
    pub license: Option<License>,
    pub package_contains_errors: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedFile {
    pub name: String,
    pub generating_rule: String,
    pub location: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageGroup {
    pub name: String,
    pub contained_packages: Vec<String>,
    pub included_package_groups: Vec<String>,
}

/// A configured edge. For null-configuration dependencies BOTH options are None.
/// Unlike the deprecated inline target configuration, no "null" string is emitted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredRuleInput {
    pub label: String,
    pub configuration_checksum: Option<String>,
    pub configuration_id: Option<u32>,
}

/// A non-null configuration, supplied by analysis. IDs must be positive and stable
/// within this output; checksums are full native checksums, never shortened IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    pub id: u32,
    pub checksum: String,
    pub mnemonic: String,
    pub platform_name: String,
    pub is_tool: bool,
    pub fragments: Vec<Fragment>,
    pub fragment_options: Vec<FragmentOptions>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragment {
    pub name: String,
    pub fragment_option_names: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentOptions {
    pub name: String,
    pub options: Vec<ConfigurationOption>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationOption {
    pub name: Option<String>,
    pub value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
    pub explicitly_specified: Option<bool>,
    pub nodep: Option<bool>,
    /// Some("") for ordinary attributes, Some(aspect name) for aspect attributes;
    /// None for synthetic metadata. Emitted only when requested in WriterOptions.
    pub source_aspect_name: Option<String>,
}

impl Attribute {
    /// Construct an ordinary configured attribute with AttributeFormatter metadata.
    /// For NODEP_LABEL / NODEP_LABEL_LIST, override `nodep` with Some(true).
    pub fn new(name: impl Into<String>, value: AttributeValue, explicit: bool) -> Self {
        let nodep = match value.attribute_type() {
            AttributeType::String
            | AttributeType::Label
            | AttributeType::Output
            | AttributeType::StringList
            | AttributeType::LabelList
            | AttributeType::OutputList
            | AttributeType::LabelDictUnary
            | AttributeType::LabelKeyedStringDict => Some(false),
            _ => None,
        };
        Self {
            name: name.into(),
            value,
            explicitly_specified: Some(explicit),
            nodep,
            source_aspect_name: Some(String::new()),
        }
    }
}

/// Fully resolved, typed attribute values. Dict entries and lists retain order.
/// Unset distinguishes a null value from an empty string, false or zero.
/// Configured attributes must not contain unevaluated selector expressions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttributeValue {
    Integer(i32),
    String(String),
    Label(String),
    Output(String),
    StringList(Vec<String>),
    LabelList(Vec<String>),
    OutputList(Vec<String>),
    DistributionSet(Vec<String>),
    License(License),
    StringDict(Vec<(String, String)>),
    LabelDictUnary(Vec<(String, String)>),
    LabelKeyedStringDict(Vec<(String, String)>),
    LabelListDict(Vec<(String, Vec<String>)>),
    StringListDict(Vec<(String, Vec<String>)>),
    Boolean(bool),
    Tristate(Tristate),
    IntegerList(Vec<i32>),
    Unset(AttributeType),
}

impl AttributeValue {
    pub fn attribute_type(&self) -> AttributeType {
        match self {
            Self::Integer(_) => AttributeType::Integer,
            Self::String(_) => AttributeType::String,
            Self::Label(_) => AttributeType::Label,
            Self::Output(_) => AttributeType::Output,
            Self::StringList(_) => AttributeType::StringList,
            Self::LabelList(_) => AttributeType::LabelList,
            Self::OutputList(_) => AttributeType::OutputList,
            Self::DistributionSet(_) => AttributeType::DistributionSet,
            Self::License(_) => AttributeType::License,
            Self::StringDict(_) => AttributeType::StringDict,
            Self::LabelDictUnary(_) => AttributeType::LabelDictUnary,
            Self::LabelKeyedStringDict(_) => AttributeType::LabelKeyedStringDict,
            Self::LabelListDict(_) => AttributeType::LabelListDict,
            Self::StringListDict(_) => AttributeType::StringListDict,
            Self::Boolean(_) => AttributeType::Boolean,
            Self::Tristate(_) => AttributeType::Tristate,
            Self::IntegerList(_) => AttributeType::IntegerList,
            Self::Unset(kind) => *kind,
        }
    }
}

/// Values are build.proto Attribute.Discriminator numbers (not postcard indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum AttributeType {
    Integer = 1,
    String = 2,
    Label = 3,
    Output = 4,
    StringList = 5,
    LabelList = 6,
    OutputList = 7,
    DistributionSet = 8,
    License = 9,
    StringDict = 10,
    LabelListDict = 12,
    StringListDict = 13,
    Boolean = 14,
    Tristate = 15,
    IntegerList = 16,
    Unknown = 18,
    LabelDictUnary = 19,
    LabelKeyedStringDict = 21,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tristate {
    No = 0,
    Yes = 1,
    Auto = 2,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    pub license_types: Vec<String>,
    pub exceptions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Proto,
    StreamedProto,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transitions {
    #[default]
    None,
    Lite,
    Full,
}

/// Supported presentation flags. Graph traversal, label spelling, attribute
/// selection and relative locations belong to the caller. Unsupported CLI flags
/// must be rejected there, rather than silently accepted by this serializer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterOptions {
    pub include_configurations: bool,
    pub transitions: Transitions,
    pub include_default_values: bool,
    pub include_locations: bool,
    pub include_rule_inputs_and_outputs: bool,
    pub include_attribute_source_aspects: bool,
    pub include_instantiation_stack: bool,
    pub include_definition_stack: bool,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            include_configurations: true,
            transitions: Transitions::None,
            include_default_values: true,
            include_locations: true,
            include_rule_inputs_and_outputs: true,
            include_attribute_source_aspects: false,
            include_instantiation_stack: false,
            include_definition_stack: false,
        }
    }
}

/// Writes records immediately. Memory is O(largest record), not O(result count).
/// Finish explicitly to observe buffered I/O errors. On any error, discard the
/// output (it may already contain complete earlier records or a partial write).
pub struct CqueryWriter<W: Write> {
    out: BufWriter<W>,
    format: OutputFormat,
    options: WriterOptions,
    scratch: Encoder,
}

impl<W: Write> CqueryWriter<W> {
    pub fn new(out: W, format: OutputFormat, options: WriterOptions) -> Self {
        Self {
            out: BufWriter::new(out),
            format,
            options,
            scratch: Encoder(Vec::new()),
        }
    }

    /// Null configurations are represented by None. The deprecated inline
    /// Configuration contains only checksum ("null" for None), as Bazel/TD expect.
    pub fn write_target(
        &mut self,
        target: &Target,
        configuration: Option<&Configuration>,
    ) -> io::Result<()> {
        if let Some(configuration) = configuration {
            validate_configuration(configuration)?;
        }
        if self.options.transitions != Transitions::None
            && let Target::Rule(rule) = target
        {
            for edge in &rule.configured_rule_inputs {
                match (&edge.configuration_checksum, edge.configuration_id) {
                    (None, None) => {}
                    (Some(checksum), Some(id)) if valid_configuration(id, checksum) => {}
                    _ => {
                        return Err(invalid(
                            "configured edge needs both non-null checksum and ID",
                        ));
                    }
                }
            }
        }
        self.scratch.0.clear();
        let options = &self.options;
        // Field 1: CqueryResult.results, or QueryResult.target without configs.
        self.scratch.message(1, |out| {
            if options.include_configurations {
                out.message(1, |out| encode_target(out, target, options));
                out.message(2, |out| {
                    out.string(4, configuration.map_or("null", |c| c.checksum.as_str()));
                });
                if let Some(configuration) = configuration {
                    out.uint(3, u64::from(configuration.id));
                }
            } else {
                encode_target(out, target, options);
            }
        });
        self.write_record()
    }

    /// Emit one entry of the separate configuration table. Caller deduplicates it
    /// and includes configurations referenced only by edges. Null has no entry.
    pub fn write_configuration(&mut self, configuration: &Configuration) -> io::Result<()> {
        if !self.options.include_configurations {
            return Ok(());
        }
        validate_configuration(configuration)?;
        self.scratch.0.clear();
        self.scratch
            .message(2, |out| encode_configuration(out, configuration));
        self.write_record()
    }

    pub fn write_configurations<'a>(
        &mut self,
        configurations: impl IntoIterator<Item = &'a Configuration>,
    ) -> io::Result<()> {
        for configuration in configurations {
            self.write_configuration(configuration)?;
        }
        Ok(())
    }

    /// Retained record scratch capacity, excluding the fixed-size I/O buffer.
    pub fn scratch_capacity(&self) -> usize {
        self.scratch.0.capacity()
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        self.out.into_inner().map_err(|error| error.into_error())
    }

    fn write_record(&mut self) -> io::Result<()> {
        if self.format == OutputFormat::StreamedProto {
            let mut prefix = [0; 10];
            let len = varint_bytes(self.scratch.0.len() as u64, &mut prefix);
            self.out.write_all(&prefix[..len])?;
        }
        self.out.write_all(&self.scratch.0)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn valid_configuration(id: u32, checksum: &str) -> bool {
    id > 0 && !checksum.is_empty() && checksum != "null"
}

fn validate_configuration(configuration: &Configuration) -> io::Result<()> {
    if valid_configuration(configuration.id, &configuration.checksum) {
        Ok(())
    } else {
        Err(invalid(
            "configuration requires positive ID and non-null checksum",
        ))
    }
}

// A single scratch arena for nested messages. Reserve a maximum-sized length
// prefix, write children directly into it, then compact the prefix in place.
// No nested Vec allocations, size prepass, or graph-sized result builder.
struct Encoder(Vec<u8>);

impl Encoder {
    fn varint(&mut self, value: u64) {
        let mut bytes = [0; 10];
        let len = varint_bytes(value, &mut bytes);
        self.0.extend_from_slice(&bytes[..len]);
    }

    fn uint(&mut self, field: u32, value: u64) {
        self.varint(u64::from(field) << 3);
        self.varint(value);
    }

    fn int(&mut self, field: u32, value: i32) {
        // Protobuf int32 is NOT sint32: negatives are sign extended to 64 bits.
        self.uint(field, i64::from(value) as u64);
    }

    fn string(&mut self, field: u32, value: &str) {
        self.varint((u64::from(field) << 3) | 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value.as_bytes());
    }

    fn strings(&mut self, field: u32, values: &[String]) {
        for value in values {
            self.string(field, value);
        }
    }

    fn optional_string(&mut self, field: u32, value: &Option<String>) {
        if let Some(value) = value {
            self.string(field, value);
        }
    }

    fn optional_bool(&mut self, field: u32, value: Option<bool>) {
        if let Some(value) = value {
            self.uint(field, u64::from(value));
        }
    }

    fn nonempty_string(&mut self, field: u32, value: &str) {
        if !value.is_empty() {
            self.string(field, value);
        }
    }

    fn message(&mut self, field: u32, write: impl FnOnce(&mut Self)) {
        self.varint((u64::from(field) << 3) | 2);
        let start = self.0.len();
        self.0.resize(start + 10, 0);
        write(self);
        let end = self.0.len();
        let mut bytes = [0; 10];
        let len = varint_bytes((end - start - 10) as u64, &mut bytes);
        self.0[start..start + len].copy_from_slice(&bytes[..len]);
        self.0.copy_within(start + 10..end, start + len);
        self.0.truncate(end - (10 - len));
    }
}

fn varint_bytes(mut value: u64, bytes: &mut [u8; 10]) -> usize {
    let mut len = 0;
    while value >= 0x80 {
        bytes[len] = value as u8 | 0x80;
        value >>= 7;
        len += 1;
    }
    bytes[len] = value as u8;
    len + 1
}

fn encode_target(out: &mut Encoder, target: &Target, options: &WriterOptions) {
    match target {
        Target::Rule(rule) => {
            out.uint(1, 1);
            out.message(2, |out| encode_rule(out, rule, options));
        }
        Target::SourceFile(source) => {
            out.uint(1, 2);
            out.message(3, |out| {
                out.string(1, &source.name);
                if options.include_locations {
                    out.optional_string(2, &source.location);
                }
                out.strings(3, &source.subincludes);
                out.strings(4, &source.package_groups);
                out.strings(5, &source.visibility_labels);
                out.strings(6, &source.features);
                if let Some(license) = &source.license {
                    out.message(8, |out| encode_license(out, license));
                }
                out.optional_bool(9, source.package_contains_errors);
            });
        }
        Target::GeneratedFile(file) => {
            out.uint(1, 3);
            out.message(4, |out| {
                out.string(1, &file.name);
                out.string(2, &file.generating_rule);
                if options.include_locations {
                    out.optional_string(3, &file.location);
                }
            });
        }
        Target::PackageGroup(group) => {
            out.uint(1, 4);
            out.message(5, |out| {
                out.string(1, &group.name);
                out.strings(2, &group.contained_packages);
                out.strings(3, &group.included_package_groups);
            });
        }
    }
}

fn encode_rule(out: &mut Encoder, rule: &Rule, options: &WriterOptions) {
    out.string(1, &rule.name);
    out.string(2, &rule.rule_class);
    if options.include_locations {
        out.optional_string(3, &rule.location);
    }
    for attr in &rule.attributes {
        // Synthetic attributes have no explicitness metadata and are always
        // included. Aspect attributes aren't filtered by include_default_values
        // in upstream ProtoOutputFormatter.includeAspectAttribute either.
        if options.include_default_values
            || attr.explicitly_specified != Some(false)
            || attr
                .source_aspect_name
                .as_ref()
                .is_some_and(|s| !s.is_empty())
        {
            out.message(4, |out| encode_attribute(out, attr, options));
        }
    }
    if options.include_rule_inputs_and_outputs {
        out.strings(5, &rule.rule_inputs);
        out.strings(6, &rule.rule_outputs);
    }
    out.strings(7, &rule.default_settings);
    if options.include_instantiation_stack {
        out.strings(13, &rule.instantiation_stack);
    }
    if options.include_definition_stack {
        out.strings(14, &rule.definition_stack);
    }
    if options.transitions != Transitions::None {
        for edge in &rule.configured_rule_inputs {
            out.message(15, |out| {
                out.string(1, &edge.label);
                out.optional_string(2, &edge.configuration_checksum);
                if let Some(id) = edge.configuration_id {
                    out.uint(3, u64::from(id));
                }
            });
        }
    }
}

fn encode_attribute(out: &mut Encoder, attr: &Attribute, options: &WriterOptions) {
    out.string(1, &attr.name);
    out.uint(2, attr.value.attribute_type() as u64);
    match &attr.value {
        AttributeValue::Integer(value) => out.int(3, *value),
        AttributeValue::String(value)
        | AttributeValue::Label(value)
        | AttributeValue::Output(value) => out.string(5, value),
        AttributeValue::StringList(values)
        | AttributeValue::LabelList(values)
        | AttributeValue::OutputList(values)
        | AttributeValue::DistributionSet(values) => out.strings(6, values),
        AttributeValue::License(value) => out.message(7, |out| encode_license(out, value)),
        AttributeValue::StringDict(values) => encode_dict(out, 8, values),
        AttributeValue::LabelListDict(values) | AttributeValue::StringListDict(values) => {
            // AttributeFormatter 9.3 deliberately shares the string-list builder
            // for BOTH types: discriminator 12 still uses field 11, not field 10.
            for (key, values) in values {
                out.message(11, |out| {
                    out.string(1, key);
                    out.strings(2, values);
                });
            }
        }
        AttributeValue::Boolean(value) => {
            out.int(3, i32::from(*value));
            out.string(5, if *value { "true" } else { "false" });
        }
        AttributeValue::Tristate(value) => {
            let (int, string) = match value {
                Tristate::No => (0, "no"),
                Tristate::Yes => (1, "yes"),
                Tristate::Auto => (-1, "auto"),
            };
            out.int(3, int);
            out.string(5, string);
        }
        AttributeValue::IntegerList(_)
        | AttributeValue::LabelDictUnary(_)
        | AttributeValue::LabelKeyedStringDict(_)
        | AttributeValue::Unset(_) => {}
    }
    out.optional_bool(13, attr.explicitly_specified);
    match &attr.value {
        AttributeValue::Boolean(value) => out.uint(14, u64::from(*value)),
        AttributeValue::Tristate(value) => out.uint(15, *value as u64),
        AttributeValue::IntegerList(values) => {
            for value in values {
                out.int(17, *value);
            }
        }
        AttributeValue::LabelDictUnary(values) => encode_dict(out, 19, values),
        _ => {}
    }
    out.optional_bool(20, attr.nodep);
    if let AttributeValue::LabelKeyedStringDict(values) = &attr.value {
        encode_dict(out, 22, values);
    }
    if options.include_attribute_source_aspects {
        out.optional_string(23, &attr.source_aspect_name);
    }
}

fn encode_dict(out: &mut Encoder, field: u32, values: &[(String, String)]) {
    for (key, value) in values {
        out.message(field, |out| {
            out.string(1, key);
            out.string(2, value);
        });
    }
}

fn encode_license(out: &mut Encoder, license: &License) {
    out.strings(1, &license.license_types);
    out.strings(2, &license.exceptions);
}

fn encode_configuration(out: &mut Encoder, config: &Configuration) {
    out.uint(1, u64::from(config.id));
    out.nonempty_string(2, &config.mnemonic);
    out.nonempty_string(3, &config.platform_name);
    out.nonempty_string(4, &config.checksum);
    if config.is_tool {
        out.uint(5, 1);
    }
    for fragment in &config.fragments {
        out.message(6, |out| {
            out.nonempty_string(1, &fragment.name);
            out.strings(2, &fragment.fragment_option_names);
        });
    }
    for fragment in &config.fragment_options {
        out.message(7, |out| {
            out.nonempty_string(1, &fragment.name);
            for option in &fragment.options {
                out.message(2, |out| {
                    out.optional_string(1, &option.name);
                    out.optional_string(2, &option.value);
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute_bytes(value: AttributeValue) -> Vec<u8> {
        let mut out = Encoder(Vec::new());
        encode_attribute(
            &mut out,
            &Attribute::new("", value, false),
            &WriterOptions::default(),
        );
        out.0
    }

    #[test]
    fn required_and_optional_zero_empty_fields_are_not_omitted() {
        // Attribute name="", required type=INTEGER, int_value=0, explicit=false.
        assert_eq!(
            attribute_bytes(AttributeValue::Integer(0)),
            [0x0a, 0, 0x10, 1, 0x18, 0, 0x68, 0]
        );
        // No graph assumptions: proto2 required rule name/class both emitted.
        let mut out = Encoder(Vec::new());
        encode_target(
            &mut out,
            &Target::Rule(Rule::default()),
            &WriterOptions::default(),
        );
        assert_eq!(out.0, [8, 1, 18, 4, 10, 0, 18, 0]);
        let mut out = Encoder(Vec::new());
        encode_target(
            &mut out,
            &Target::GeneratedFile(GeneratedFile::default()),
            &WriterOptions::default(),
        );
        assert_eq!(out.0, [8, 3, 34, 4, 10, 0, 18, 0]);
    }

    #[test]
    fn signed_int32_is_sign_extended_not_zigzag_or_unsigned_32_bit() {
        let bytes = attribute_bytes(AttributeValue::Integer(-1));
        assert_eq!(
            bytes,
            [
                10, 0, 16, 1, 24, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1, 104, 0
            ]
        );
        let bytes = attribute_bytes(AttributeValue::IntegerList(vec![-1, 0]));
        assert_eq!(
            &bytes[6..],
            &[
                136, 1, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1, 136, 1, 0
            ]
        );
    }

    #[test]
    fn legacy_boolean_and_tristate_fields_include_default_values() {
        assert_eq!(
            attribute_bytes(AttributeValue::Boolean(false)),
            [
                10, 0, 16, 14, 24, 0, 42, 5, b'f', b'a', b'l', b's', b'e', 104, 0, 112, 0
            ]
        );
        assert_eq!(
            attribute_bytes(AttributeValue::Tristate(Tristate::No)),
            [10, 0, 16, 15, 24, 0, 42, 2, b'n', b'o', 104, 0, 120, 0]
        );
        assert!(attribute_bytes(AttributeValue::Boolean(true)).ends_with(&[112, 1]));
        assert!(attribute_bytes(AttributeValue::Tristate(Tristate::Auto)).ends_with(&[120, 2]));
        assert!(attribute_bytes(AttributeValue::Tristate(Tristate::Yes)).ends_with(&[120, 1]));
    }

    #[test]
    fn null_target_keeps_inline_deprecated_checksum_but_no_id_or_table() {
        let mut writer =
            CqueryWriter::new(Vec::new(), OutputFormat::Proto, WriterOptions::default());
        writer
            .write_target(&Target::SourceFile(SourceFile::default()), None)
            .unwrap();
        assert_eq!(
            writer.finish().unwrap(),
            [
                10, 16, 10, 6, 8, 2, 26, 2, 10, 0, 18, 6, 34, 4, b'n', b'u', b'l', b'l'
            ]
        );
    }

    #[test]
    fn presentation_options_filter_only_their_own_fields() {
        let mut aspect = Attribute::new("aspect", AttributeValue::Label("//:dep".into()), false);
        aspect.source_aspect_name = Some("//:defs.bzl%aspect".into());
        let synthetic = Attribute {
            name: "$rule_implementation_hash".into(),
            value: AttributeValue::String("hash".into()),
            explicitly_specified: None,
            nodep: None,
            source_aspect_name: None,
        };
        let mut rule = Rule {
            name: "//:rule".into(),
            rule_class: "rule".into(),
            location: Some("/BUILD:1:1".into()),
            attributes: vec![
                Attribute::new("default", AttributeValue::Integer(0), false),
                Attribute::new("explicit", AttributeValue::Integer(0), true),
                synthetic,
                aspect,
            ],
            rule_inputs: vec!["//:input".into()],
            rule_outputs: vec!["//:output".into()],
            configured_rule_inputs: vec![ConfiguredRuleInput {
                label: "//:source".into(),
                ..Default::default()
            }],
            instantiation_stack: vec!["BUILD:1:1".into()],
            definition_stack: vec!["defs.bzl:1:1".into()],
            ..Rule::default()
        };
        let options = WriterOptions {
            include_default_values: false,
            include_locations: false,
            include_rule_inputs_and_outputs: false,
            ..WriterOptions::default()
        };
        let mut filtered = Encoder(Vec::new());
        encode_rule(&mut filtered, &rule, &options);
        rule.location = None;
        rule.attributes.remove(0);
        rule.rule_inputs.clear();
        rule.rule_outputs.clear();
        rule.configured_rule_inputs.clear();
        rule.instantiation_stack.clear();
        rule.definition_stack.clear();
        let mut expected = Encoder(Vec::new());
        encode_rule(&mut expected, &rule, &WriterOptions::default());
        assert_eq!(filtered.0, expected.0);

        let options = WriterOptions {
            include_attribute_source_aspects: true,
            include_instantiation_stack: true,
            include_definition_stack: true,
            ..WriterOptions::default()
        };
        let mut with_aspect = Encoder(Vec::new());
        encode_rule(&mut with_aspect, &rule, &options);
        assert!(
            with_aspect
                .0
                .windows(b"//:defs.bzl%aspect".len())
                .any(|w| w == b"//:defs.bzl%aspect")
        );
    }

    #[test]
    fn invalid_configurations_and_half_populated_edges_fail_explicitly() {
        let target = Target::Rule(Rule::default());
        let mut writer =
            CqueryWriter::new(Vec::new(), OutputFormat::Proto, WriterOptions::default());
        assert_eq!(
            writer
                .write_target(&target, Some(&Configuration::default()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let target = Target::Rule(Rule {
            configured_rule_inputs: vec![ConfiguredRuleInput {
                label: "//:dep".into(),
                configuration_id: Some(1),
                configuration_checksum: None,
            }],
            ..Default::default()
        });
        // Suppressed transitions do not require materializing/validating edges.
        writer.write_target(&target, None).unwrap();
        let mut writer = CqueryWriter::new(
            Vec::new(),
            OutputFormat::Proto,
            WriterOptions {
                transitions: Transitions::Lite,
                ..Default::default()
            },
        );
        assert_eq!(
            writer.write_target(&target, None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn empty_result_is_empty_and_buffered_io_errors_are_propagated() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for format in [OutputFormat::Proto, OutputFormat::StreamedProto] {
            assert!(
                CqueryWriter::new(Vec::new(), format, WriterOptions::default())
                    .finish()
                    .unwrap()
                    .is_empty()
            );
            let mut writer = CqueryWriter::new(Failing, format, WriterOptions::default());
            writer
                .write_target(&Target::SourceFile(SourceFile::default()), None)
                .unwrap();
            assert!(writer.finish().is_err());
        }
    }
}
