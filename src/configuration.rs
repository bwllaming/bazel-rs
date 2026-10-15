//! Native build configuration: the modelled Bazel 9.3 build options, their
//! command-line converters and a stable configuration identity.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/`. Only the
//! options in [`OPTIONS`] are modelled; every other build option is an explicit
//! failure on the command line and in `config_setting(values=...)`
//! (docs/CONFIGURATION.md). Starlark build settings (`starlark`) are stored
//! only when different from their default (StarlarkOptionsParser.java:232-254,
//! crate::starlark_options).
use std::collections::BTreeMap;
use std::fmt;

use sha2::{Digest, Sha256};

use crate::label::Label;

/// Value shape and converter of a modelled option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    /// Converters.BooleanConverter (common/options/Converters.java:46-60).
    Bool,
    /// EnumConverter (common/options/EnumConverter.java:32-67): case-insensitive.
    Enum {
        type_name: &'static str,
        values: &'static [&'static str],
    },
    /// Converters.AssignmentConverter (common/options/Converters.java:483-501).
    Assignments,
    /// CoreOptionConverters.LabelListConverter (build/lib/analysis/config/CoreOptionConverters.java:143-158).
    LabelList,
    /// PlatformOptions.HostPlatformConverter (build/lib/analysis/PlatformOptions.java:218-227).
    Label,
}

/// One modelled native option (an `@Option` of a FragmentOptions class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionDef {
    pub name: &'static str,
    pub old_name: Option<&'static str>,
    pub abbrev: Option<char>,
    pub kind: OptionKind,
    /// `defaultValue` as written in the annotation.
    pub default: &'static str,
    pub allow_multiple: bool,
    /// OptionMetadataTag.NON_CONFIGURABLE (BuildOptionDetails.java:188-202).
    pub non_configurable: bool,
    pub source: &'static str,
}

const COMPILATION_MODES: &[&str] = &["fastbuild", "dbg", "opt"];
const STRIP_MODES: &[&str] = &["always", "sometimes", "never"];

/// `@bazel_tools//tools:host_platform` (PlatformOptions.java:46).
pub const DEFAULT_HOST_PLATFORM: &str = "@bazel_tools//tools:host_platform";

/// The modelled options, in a fixed order that is also the identity order.
pub const OPTIONS: &[OptionDef] = &[
    OptionDef {
        name: "compilation_mode",
        old_name: None,
        abbrev: Some('c'),
        // CompilationMode enum order FASTBUILD, DBG, OPT
        // (build/lib/analysis/config/CompilationMode.java:24-51).
        kind: OptionKind::Enum {
            type_name: "compilation mode",
            values: COMPILATION_MODES,
        },
        default: "fastbuild",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:396-404",
    },
    OptionDef {
        name: "stamp",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:349-355",
    },
    OptionDef {
        name: "define",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Assignments,
        default: "null",
        allow_multiple: true,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:255-267",
    },
    OptionDef {
        name: "strip",
        old_name: None,
        abbrev: None,
        // CppConfiguration.StripMode ALWAYS, SOMETIMES, NEVER
        // (build/lib/rules/cpp/CppConfiguration.java:119-134), StripModeConverter
        // (build/lib/rules/cpp/CppOptions.java:68-72).
        kind: OptionKind::Enum {
            type_name: "strip mode",
            values: STRIP_MODES,
        },
        default: "sometimes",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/cpp/CppOptions.java:337-347",
    },
    OptionDef {
        name: "force_pic",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/cpp/CppOptions.java:218-227",
    },
    OptionDef {
        name: "platforms",
        old_name: Some("experimental_platforms"),
        abbrev: None,
        kind: OptionKind::LabelList,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/PlatformOptions.java:82-96",
    },
    OptionDef {
        name: "host_platform",
        old_name: Some("experimental_host_platform"),
        abbrev: None,
        kind: OptionKind::Label,
        default: DEFAULT_HOST_PLATFORM,
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/PlatformOptions.java:52-64",
    },
];

/// The modelled option named `name` (its canonical name only).
pub fn option(name: &str) -> Option<&'static OptionDef> {
    OPTIONS.iter().find(|option| option.name == name)
}

/// BuildOptionDetails.getCanonicalName (BuildOptionDetails.java:174-176)
/// over every Bazel 9.3 fragment option (crate::bazel_options).
pub fn canonical_name(name: &str) -> &str {
    crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(_, old, _, _)| !old.is_empty() && *old == name)
        .map_or(name, |(canonical, _, _, _)| canonical)
}

/// Whether `name` (exactly, not an old name) is a Bazel 9.3 fragment option
/// (BuildOptionDetails.getOptionClass, :140-143), and whether it is
/// NON_CONFIGURABLE (BuildOptionDetails.isNonConfigurable, :188-202).
pub fn fragment_option(name: &str) -> Option<bool> {
    crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(option, _, _, _)| *option == name)
        .map(|(_, _, non_configurable, _)| *non_configurable)
}

/// SpellChecker.suggest (net/starlark/java/spelling/SpellChecker.java:83-96):
/// whether some word is within `min(5, (len + 1) / 2)` case-insensitive edits.
fn has_suggestion(input: &str, words: impl IntoIterator<Item = String>) -> bool {
    fn distance(a: &[char], b: &[char]) -> usize {
        let mut row = (0..=b.len()).collect::<Vec<_>>();
        for i in 1..=a.len() {
            let mut previous = row[0];
            row[0] = i;
            for j in 1..=b.len() {
                let old = row[j];
                row[j] =
                    (previous + usize::from(a[i - 1] != b[j - 1])).min(1 + row[j - 1].min(row[j]));
                previous = old;
            }
        }
        row[b.len()]
    }
    let input = input.to_lowercase().chars().collect::<Vec<_>>();
    let threshold = 5.min(input.len().div_ceil(2));
    words.into_iter().any(|word| {
        let word = word.to_lowercase().chars().collect::<Vec<_>>();
        input.len().abs_diff(word.len()) < threshold && distance(&input, &word) < threshold
    })
}

/// OptionsParserImpl.identifyOptionAndPossibleArgument for a `--` argument
/// that is not a native option of this profile
/// (common/options/OptionsParserImpl.java:776-824): `ARG :: Unrecognized
/// option: ARG` when the name (also without a `no` prefix) is not the name or
/// old name of any `@Option` of Bazel 9.3 and SpellChecker would suggest
/// nothing even among every option (`--NAME`, `--noNAME`; a superset of the
/// command's valid arguments). `None` otherwise: the caller keeps its
/// explicit "unsupported option" error.
pub fn unrecognized_option(arg: &str) -> Option<String> {
    let rest = arg.strip_prefix("--")?;
    let name = rest.split_once('=').map_or(rest, |(name, _)| name);
    if name.trim().is_empty() {
        return None;
    }
    let known = |name: &str| crate::bazel_options::ALL_OPTION_NAMES.contains(&name);
    if known(name) || name.strip_prefix("no").is_some_and(known) {
        return None;
    }
    let words = crate::bazel_options::ALL_OPTION_NAMES
        .iter()
        .flat_map(|name| [format!("--{name}"), format!("--no{name}")]);
    if has_suggestion(arg, words) {
        return None;
    }
    Some(invalid_argument(arg, format!("Unrecognized option: {arg}")))
}

/// A converted option value, compared like the Java objects Bazel stores.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptionValue {
    Bool(bool),
    Enum(&'static str),
    /// `List<Map.Entry<String, String>>`.
    Assignments(Vec<(String, String)>),
    Labels(Vec<Label>),
    Label(Label),
}

impl fmt::Display for OptionValue {
    /// Java `toString` of the stored value (Boolean, the enum's mode string,
    /// `AbstractCollection.toString`, `Map.Entry` as `k=v`, Label.toString =
    /// getCanonicalForm, cmdline/Label.java:479-492).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Enum(value) => f.write_str(value),
            Self::Assignments(entries) => {
                let items = entries
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>();
                write!(f, "[{}]", items.join(", "))
            }
            Self::Labels(labels) => {
                let items = labels
                    .iter()
                    .map(crate::rule_declarations::canonical_form)
                    .collect::<Vec<_>>();
                write!(f, "[{}]", items.join(", "))
            }
            Self::Label(label) => f.write_str(&crate::rule_declarations::canonical_form(label)),
        }
    }
}

/// Resolve a label-typed option value (already `//`-prefixed per
/// CoreOptionConverters.convertOptionsLabel, :331-361).
pub type LabelParser<'a> = dyn Fn(&str) -> Result<Label, String> + 'a;

/// `Label.parseCanonical` (the null conversion context of
/// CoreOptionConverters.convertOptionsLabel, :345-350): an apparent `@name`
/// is taken as the canonical repository name.
pub fn parse_canonical_option_label(text: &str) -> Result<Label, String> {
    let canonical = if let Some(rest) = text.strip_prefix("@//") {
        format!("//{rest}")
    } else if text.starts_with('@') && !text.starts_with("@@") {
        format!("@{text}")
    } else {
        text.to_owned()
    };
    let canonical = match canonical.split_once("//") {
        // `@@repo` alone denotes `@@repo//:repo`.
        None if canonical.starts_with("@@") => {
            format!("{canonical}//:{}", &canonical[2..])
        }
        _ => canonical,
    };
    Label::parse_canonical(&canonical)
}

/// The option's converter (without the `While parsing option` wrapping of
/// ParsedOptionDescription.getConvertedValue, common/options/ParsedOptionDescription.java:204-213).
pub fn convert(
    option: &OptionDef,
    input: &str,
    labels: &LabelParser<'_>,
) -> Result<OptionValue, String> {
    // CoreOptionConverters.convertOptionsLabel (:339-344): prefix "//" unless
    // the input starts with '/' or '@'.
    let label = |text: &str| -> Result<Label, String> {
        if text.starts_with('/') || text.starts_with('@') {
            labels(text)
        } else {
            labels(&format!("//{text}"))
        }
    };
    match option.kind {
        OptionKind::Bool => {
            let lower = input.to_ascii_lowercase();
            if matches!(lower.as_str(), "true" | "1" | "yes" | "t" | "y") {
                Ok(OptionValue::Bool(true))
            } else if matches!(lower.as_str(), "false" | "0" | "no" | "f" | "n") {
                Ok(OptionValue::Bool(false))
            } else {
                Err(format!("'{lower}' is not a boolean"))
            }
        }
        OptionKind::Enum { type_name, values } => values
            .iter()
            .find(|value| value.eq_ignore_ascii_case(input))
            .map(|value| OptionValue::Enum(value))
            .ok_or_else(|| {
                format!(
                    "Not a valid {type_name}: '{input}' (should be {})",
                    join_english_list(values)
                )
            }),
        OptionKind::Assignments => match input.find('=') {
            Some(position) if position > 0 => Ok(OptionValue::Assignments(vec![(
                input[..position].to_owned(),
                input[position + 1..].to_owned(),
            )])),
            _ => Err(
                "Variable definitions must be in the form of a 'name=value' assignment".to_owned(),
            ),
        },
        // Splitter.on(",").omitEmptyStrings().
        OptionKind::LabelList => input
            .split(',')
            .filter(|part| !part.is_empty())
            .map(label)
            .collect::<Result<Vec<_>, _>>()
            .map(OptionValue::Labels),
        OptionKind::Label => label(if input.is_empty() {
            DEFAULT_HOST_PLATFORM
        } else {
            input
        })
        .map(OptionValue::Label),
    }
}

/// Converters.joinEnglishList (common/options/Converters.java:240-250).
fn join_english_list(values: &[&str]) -> String {
    let mut text = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            text.push_str(if index + 1 < values.len() {
                ", "
            } else {
                " or "
            });
        }
        text.push_str(value);
    }
    if text.is_empty() {
        "nothing".to_owned()
    } else {
        text
    }
}

/// Values of the modelled native options.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeOptions {
    pub compilation_mode: &'static str,
    pub stamp: bool,
    /// Last value per key wins, sorted by key (CoreOptions.getNormalized,
    /// CoreOptions.java:1245-1248; FragmentOptions.normalizeEntries, :100-113).
    pub define: Vec<(String, String)>,
    pub strip: &'static str,
    pub force_pic: bool,
    /// Only the first entry (PlatformOptions.getNormalized, :189-200); empty
    /// becomes the host platform (PlatformMappingValue.computeMapping,
    /// skyframe/config/PlatformMappingValue.java:154-158).
    #[serde(serialize_with = "serialize_labels")]
    pub platforms: Vec<Label>,
    #[serde(serialize_with = "serialize_label")]
    pub host_platform: Label,
}

fn serialize_labels<S: serde::Serializer>(labels: &[Label], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(labels.iter().map(crate::rule_declarations::canonical_form))
}

fn serialize_label<S: serde::Serializer>(label: &Label, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&crate::rule_declarations::canonical_form(label))
}

/// The label of [`DEFAULT_HOST_PLATFORM`].
pub fn default_host_platform() -> Label {
    Label {
        repository: "bazel_tools".into(),
        package: "tools".into(),
        name: "host_platform".into(),
    }
}

impl Default for NativeOptions {
    fn default() -> Self {
        let host_platform = default_host_platform();
        Self {
            compilation_mode: "fastbuild",
            stamp: false,
            define: Vec::new(),
            strip: "sometimes",
            force_pic: false,
            platforms: vec![host_platform.clone()],
            host_platform,
        }
    }
}

impl NativeOptions {
    /// PlatformConfiguration.getTargetPlatform: the first `--platforms`
    /// entry (PlatformOptions.computeTargetPlatform, PlatformOptions.java:205-212).
    pub fn target_platform(&self) -> &Label {
        self.platforms.first().unwrap_or(&self.host_platform)
    }

    /// BuildOptionDetails.getOptionValue (BuildOptionDetails.java:145-158) of a
    /// modelled option.
    pub fn value(&self, name: &str) -> Option<OptionValue> {
        Some(match name {
            "compilation_mode" => OptionValue::Enum(self.compilation_mode),
            "stamp" => OptionValue::Bool(self.stamp),
            "define" => OptionValue::Assignments(self.define.clone()),
            "strip" => OptionValue::Enum(self.strip),
            "force_pic" => OptionValue::Bool(self.force_pic),
            "platforms" => OptionValue::Labels(self.platforms.clone()),
            "host_platform" => OptionValue::Label(self.host_platform.clone()),
            _ => return None,
        })
    }
}

/// A build setting value, compared like the Java objects Bazel stores
/// (Boolean, StarlarkInt, String, List<String>, Label).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    String(String),
    StringList(Vec<String>),
    Label(#[serde(serialize_with = "serialize_label")] Label),
}

impl fmt::Display for SettingValue {
    /// Java `toString` of the stored value (Boolean, StarlarkInt,
    /// AbstractCollection.toString, Label.toString = getCanonicalForm).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
            Self::StringList(values) => write!(f, "[{}]", values.join(", ")),
            Self::Label(label) => f.write_str(&crate::rule_declarations::canonical_form(label)),
        }
    }
}

/// SHA-256 over the canonical serialization of a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigId([u8; 32]);

impl fmt::Display for ConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// One build configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub id: ConfigId,
    pub native: NativeOptions,
    /// Starlark build settings that differ from their default.
    pub starlark: BTreeMap<Label, SettingValue>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self::new(NativeOptions::default(), BTreeMap::new())
    }
}

impl Configuration {
    pub fn new(native: NativeOptions, starlark: BTreeMap<Label, SettingValue>) -> Self {
        let id = ConfigId(Sha256::digest(canonical_serialization(&native, &starlark)).into());
        Self {
            id,
            native,
            starlark,
        }
    }

    /// The generated-file root of this configuration: Bazel's
    /// `bazel-out/<cpu>-<compilation_mode>/bin` mnemonic with the native
    /// profile's fixed `rust` platform part. The default configuration keeps
    /// the historical `rust-fastbuild` root, so existing action identities and
    /// caches are unchanged.
    pub fn output_root(&self) -> &'static str {
        match self.native.compilation_mode {
            "dbg" => "bazel-out/rust-dbg/bin",
            "opt" => "bazel-out/rust-opt/bin",
            _ => "bazel-out/rust-fastbuild/bin",
        }
    }
}

fn canonical_serialization(
    native: &NativeOptions,
    starlark: &BTreeMap<Label, SettingValue>,
) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Canonical<'a> {
        version: u32,
        native: &'a NativeOptions,
        starlark: Vec<(String, &'a SettingValue)>,
    }
    serde_json::to_vec(&Canonical {
        version: 1,
        native,
        starlark: starlark
            .iter()
            .map(|(label, value)| (crate::rule_declarations::canonical_form(label), value))
            .collect(),
    })
    .expect("configuration serialization")
}

/// Command-line options of one invocation, in command-line order, converted
/// once the main repository mapping is known (the second parsing round of
/// CoreOptionConverters.convertOptionsLabel, :351-357).
#[derive(Clone, Debug, Default)]
pub struct CommandLineOptions {
    /// (option, command-line form, unconverted value).
    entries: Vec<(&'static OptionDef, String, String)>,
    /// Starlark build setting arguments (skipped by the native parser), in
    /// command-line order.
    starlark: Vec<String>,
    /// `query` options: only CoreOptions (runtime/commands/QueryCommand.java:59-67,
    /// no usesConfigurationOptions, runtime/BlazeCommandUtils.java:74-98).
    query: bool,
}

/// OptionsParsingException with an invalid argument: BlazeOptionHandler prints
/// `ARG :: MESSAGE` (runtime/BlazeOptionHandler.java:104, 628-637).
fn invalid_argument(arg: &str, message: String) -> String {
    format!("{arg} :: {message}")
}

/// Result of [`CommandLineOptions::parse`] for one argument.
pub enum Parsed {
    /// Not a modelled configuration option.
    NotModelled,
    /// Consumed; the next argument was consumed too when `true`.
    Consumed(bool),
}

/// OptionsParser.STARLARK_SKIPPED_PREFIXES (common/options/OptionsParser.java:111-112):
/// the native parser skips these arguments; StarlarkOptionsParser parses them
/// for commands that analyze (runtime/BlazeOptionHandler.java:417-423).
pub const STARLARK_SKIPPED_PREFIXES: [&str; 4] = ["--//", "--no//", "--@", "--no@"];

impl CommandLineOptions {
    /// Options of the `query` command: the modelled CoreOptions
    /// (`--compilation_mode`, `--stamp`, `--define`) are accepted and validated
    /// but do not affect loading; the others remain unsupported there.
    pub fn for_query() -> Self {
        Self {
            query: true,
            ..Self::default()
        }
    }

    /// OptionsParserImpl.identifyOptionAndPossibleArgument
    /// (common/options/OptionsParserImpl.java:756-850) for the modelled options:
    /// `--name=value`, `--name value`, `-c value`, `--name`/`--noname` for
    /// booleans. Errors are Bazel's OptionsParsingException messages.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Parsed, String> {
        let query = self.query;
        let available =
            move |option: &&OptionDef| !query || option.source.contains("/CoreOptions.java");
        // OptionsParserImpl.java:515-525: a likely negative target pattern or a
        // mistyped Starlark option (no invalid-argument prefix).
        if arg.starts_with("-//") || arg.starts_with("-@") {
            return Err(format!(
                "Invalid options syntax: {arg}\nNote: Negative target patterns can only appear after the end of options marker ('--'). Flags corresponding to Starlark-defined build settings always start with '--', not '-'."
            ));
        }
        // OptionsParserImpl.java:535-550: skipped Starlark options keep their
        // order; `query` never parses them (it does not analyze).
        if STARLARK_SKIPPED_PREFIXES
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        {
            if !query {
                self.starlark.push(arg.to_owned());
            }
            return Ok(Parsed::Consumed(false));
        }
        // OptionsParserImpl.java:766-808: `-x` (abbreviation), `-x-`, `--name[=value]`;
        // anything else starting with '-' is invalid syntax.
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() != 2
            && !(arg.len() == 3 && arg.ends_with('-'))
        {
            return Err(invalid_argument(
                arg,
                format!("Invalid options syntax: {arg}"),
            ));
        }
        let (option, value, form, used_next) = if let Some(rest) = arg.strip_prefix("--") {
            let (name, value) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            if value.is_some() && name.trim().is_empty() {
                return Err(invalid_argument(
                    arg,
                    format!("Invalid options syntax: {arg}"),
                ));
            }
            let lookup = |name: &str| {
                OPTIONS
                    .iter()
                    .filter(available)
                    .find(|option| option.name == name || option.old_name == Some(name))
            };
            // `query` registers no PlatformOptions (QueryCommand.java:59-67):
            // OptionsParserImpl reports them as unknown (paired capture
            // B1/query_platforms_mac: `--platforms=//:mac :: Unrecognized
            // option: --platforms=//:mac`).
            if query
                && [
                    "platforms",
                    "experimental_platforms",
                    "host_platform",
                    "experimental_host_platform",
                ]
                .contains(&name)
            {
                return Err(invalid_argument(arg, format!("Unrecognized option: {arg}")));
            }
            let (option, negated) = match lookup(name) {
                Some(option) => (option, false),
                None => match name.strip_prefix("no").and_then(lookup) {
                    Some(option) => (option, true),
                    None => return Ok(Parsed::NotModelled),
                },
            };
            if negated {
                if option.kind != OptionKind::Bool {
                    return Err(invalid_argument(
                        arg,
                        format!("Illegal use of 'no' prefix on non-boolean option: {arg}"),
                    ));
                }
                if value.is_some() {
                    return Err(invalid_argument(
                        arg,
                        format!("Unexpected value after boolean option: {arg}"),
                    ));
                }
                (option, "0".to_owned(), arg.to_owned(), false)
            } else if let Some(value) = value {
                (option, value.to_owned(), arg.to_owned(), false)
            } else if option.kind == OptionKind::Bool {
                (option, "1".to_owned(), arg.to_owned(), false)
            } else {
                let value = next.ok_or_else(|| format!("Expected value after {arg}"))?;
                (option, value.to_owned(), format!("{arg} {value}"), true)
            }
        } else if arg.len() == 2
            && let Some(option) = OPTIONS
                .iter()
                .filter(available)
                .find(|option| option.abbrev.is_some_and(|c| arg == format!("-{c}")))
        {
            let value = next.ok_or_else(|| format!("Expected value after {arg}"))?;
            (option, value.to_owned(), format!("{arg} {value}"), true)
        } else {
            return Ok(Parsed::NotModelled);
        };
        // Non-label values convert in the first parsing round already.
        if !matches!(option.kind, OptionKind::LabelList | OptionKind::Label) {
            convert(option, &value, &|_| unreachable!("no labels"))
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
        }
        self.entries.push((option, form, value));
        Ok(Parsed::Consumed(used_next))
    }

    /// The Starlark build setting arguments, in command-line order
    /// (OptionsParserImpl.getSkippedArgs), for crate::starlark_options.
    pub fn starlark_args(&self) -> &[String] {
        &self.starlark
    }

    /// The configuration of the native options (without Starlark build
    /// settings, which need package loading: crate::starlark_options).
    /// `labels` resolves label options with the main repository mapping.
    pub fn configuration(&self, labels: &LabelParser<'_>) -> Result<Configuration, String> {
        let mut native = NativeOptions::default();
        let mut platforms = Vec::new();
        let mut defines = Vec::new();
        // Label.parseWithRepoContext keeps an apparent repository the main
        // repository does not see as `@@[unknown repo 'R' requested from @@]`
        // (cmdline/RepositoryName.java); the platform lookup then fails with
        // BuildFileNotFoundException (crate::engine, paired capture
        // B2/platforms*_nope_p).
        let platform_labels = |text: &str| {
            labels(text).or_else(|error| deferred_unknown_repository(text).ok_or(error))
        };
        for (option, form, value) in &self.entries {
            let parser: &LabelParser<'_> = if matches!(option.name, "platforms" | "host_platform") {
                &platform_labels
            } else {
                labels
            };
            let converted = convert(option, value, parser)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            match (option.name, converted) {
                ("compilation_mode", OptionValue::Enum(mode)) => native.compilation_mode = mode,
                ("strip", OptionValue::Enum(mode)) => native.strip = mode,
                ("stamp", OptionValue::Bool(value)) => native.stamp = value,
                ("force_pic", OptionValue::Bool(value)) => native.force_pic = value,
                ("define", OptionValue::Assignments(entries)) => defines.extend(entries),
                ("platforms", OptionValue::Labels(labels)) => platforms = labels,
                ("host_platform", OptionValue::Label(label)) => native.host_platform = label,
                _ => unreachable!("converter shape of {}", option.name),
            }
        }
        native.define = normalize_defines(defines);
        // PlatformOptions.getNormalized keeps only the first entry (:189-200);
        // an empty list becomes [--host_platform] (PlatformMappingValue.computeMapping,
        // skyframe/config/PlatformMappingValue.java:154-158). The platforms are
        // resolved and validated as platform targets by crate::engine
        // (docs/PLATFORMS.md); the default host platform spelled explicitly is
        // the same configuration as an unset option.
        platforms.truncate(1);
        native.platforms = if platforms.is_empty() {
            vec![native.host_platform.clone()]
        } else {
            platforms
        };
        Ok(Configuration::new(native, BTreeMap::new()))
    }
}

/// The repository name RepositoryName.createUnvalidated gives an apparent
/// repository that is not visible from the main repository
/// (`[unknown repo 'R' requested from @@]`).
fn unknown_repository(name: &str) -> String {
    format!("[unknown repo '{name}' requested from @@]")
}

/// `@R//pkg:name` with an apparent repository R as an unresolvable label.
fn deferred_unknown_repository(text: &str) -> Option<Label> {
    let parts = crate::bazel_label::Parts::parse(text).ok()?;
    let repository = parts.repo.filter(|_| !parts.repo_is_canonical)?;
    if repository.is_empty() {
        return None;
    }
    Some(Label {
        repository: unknown_repository(repository),
        package: parts.pkg.to_owned(),
        name: parts.target.to_owned(),
    })
}

/// The apparent repository name of a label made by
/// [`deferred_unknown_repository`].
pub fn unknown_repository_name(label: &Label) -> Option<&str> {
    label
        .repository
        .strip_prefix("[unknown repo '")?
        .strip_suffix("' requested from @@]")
}

/// FragmentOptions.normalizeEntries then sortEntries (CoreOptions.java:1199-1208).
fn normalize_defines(entries: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut normalized: Vec<(String, String)> = Vec::new();
    for (key, value) in entries {
        match normalized.iter_mut().find(|(seen, _)| *seen == key) {
            Some(entry) => entry.1 = value,
            None => normalized.push((key, value)),
        }
    }
    normalized.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(text: &str) -> Result<Label, String> {
        parse_canonical_option_label(text)
    }

    fn parsed(args: &[&str]) -> Result<Configuration, String> {
        let mut options = CommandLineOptions::default();
        let mut index = 0;
        while index < args.len() {
            match options.parse(args[index], args.get(index + 1).copied())? {
                Parsed::NotModelled => panic!("not modelled: {}", args[index]),
                Parsed::Consumed(next) => index += 1 + usize::from(next),
            }
        }
        options.configuration(&canonical)
    }

    #[test]
    fn converters_follow_bazel() {
        let stamp = option("stamp").unwrap();
        for (input, expected) in [("TRUE", true), ("yes", true), ("1", true), ("N", false)] {
            assert_eq!(
                convert(stamp, input, &canonical),
                Ok(OptionValue::Bool(expected))
            );
        }
        assert_eq!(
            convert(stamp, "Maybe", &canonical),
            Err("'maybe' is not a boolean".into())
        );
        let mode = option("compilation_mode").unwrap();
        assert_eq!(
            convert(mode, "OPT", &canonical),
            Ok(OptionValue::Enum("opt"))
        );
        assert_eq!(
            convert(mode, "fast", &canonical),
            Err("Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)".into())
        );
        assert_eq!(
            convert(option("strip").unwrap(), "x", &canonical),
            Err("Not a valid strip mode: 'x' (should be always, sometimes or never)".into())
        );
        let define = option("define").unwrap();
        assert_eq!(
            convert(define, "a=b=c", &canonical),
            Ok(OptionValue::Assignments(vec![("a".into(), "b=c".into())]))
        );
        for bad in ["=x", "x"] {
            assert_eq!(
                convert(define, bad, &canonical),
                Err("Variable definitions must be in the form of a 'name=value' assignment".into())
            );
        }
        let platforms = option("platforms").unwrap();
        assert_eq!(
            convert(platforms, "p:x,,@r//q:y", &canonical)
                .unwrap()
                .to_string(),
            "[//p:x, @@r//q:y]"
        );
        assert_eq!(
            convert(option("host_platform").unwrap(), "", &canonical)
                .unwrap()
                .to_string(),
            "@@bazel_tools//tools:host_platform"
        );
        assert_eq!(join_english_list(&[]), "nothing");
    }

    #[test]
    fn command_line_forms_and_normalization() {
        let configuration = parsed(&[
            "-c",
            "dbg",
            "--nostamp",
            "--stamp",
            "--define",
            "b=1",
            "--define=a=2",
            "--define=b=3",
            "--platforms=@bazel_tools//tools:host_platform,//p:two",
            "--strip=ALWAYS",
            "--force_pic=yes",
        ])
        .unwrap();
        let native = &configuration.native;
        assert_eq!(native.compilation_mode, "dbg");
        assert!(native.stamp && native.force_pic);
        assert_eq!(native.strip, "always");
        assert_eq!(
            native.define,
            [("a".to_owned(), "2".to_owned()), ("b".into(), "3".into())]
        );
        assert_eq!(native.platforms, [default_host_platform()]);
        assert_eq!(configuration.output_root(), "bazel-out/rust-dbg/bin");
        let mut options = CommandLineOptions::default();
        for (args, error) in [
            (
                &["--nostamp=1"][..],
                "--nostamp=1 :: Unexpected value after boolean option: --nostamp=1",
            ),
            (
                &["--nocompilation_mode"][..],
                "--nocompilation_mode :: Illegal use of 'no' prefix on non-boolean option: --nocompilation_mode",
            ),
            (&["-c=opt"][..], "-c=opt :: Invalid options syntax: -c=opt"),
            (&["--=x"][..], "--=x :: Invalid options syntax: --=x"),
            (&["-c"][..], "Expected value after -c"),
            (
                &["--compilation_mode", "x"][..],
                "While parsing option --compilation_mode x: Not a valid compilation mode: 'x' (should be fastbuild, dbg or opt)",
            ),
            (
                &["--stamp=maybe"][..],
                "While parsing option --stamp=maybe: 'maybe' is not a boolean",
            ),
        ] {
            assert_eq!(
                options
                    .parse(args[0], args.get(1).copied())
                    .err()
                    .as_deref(),
                Some(error)
            );
        }
        assert!(matches!(
            options.parse("--cpu=k8", None),
            Ok(Parsed::NotModelled)
        ));
        assert!(matches!(options.parse("-k", None), Ok(Parsed::NotModelled)));
        let empty = parsed(&["--platforms="]).unwrap();
        assert_eq!(empty.native.platforms, [default_host_platform()]);
        assert_eq!(empty, Configuration::default());
        // The default host platform spelled explicitly is the default
        // configuration.
        for args in [
            &["--platforms=@bazel_tools//tools:host_platform"][..],
            &["--host_platform=@@bazel_tools//tools:host_platform"][..],
            &["--host_platform="][..],
            &["--platforms=//p:x", "--platforms="][..],
        ] {
            assert_eq!(parsed(args).unwrap(), Configuration::default(), "{args:?}");
        }
        // Other platforms are configurations of their own (validated as
        // platform targets by crate::engine); without --platforms the target
        // platform is --host_platform (PlatformMappingValue.java:154-158).
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        for (args, target, host) in [
            (&["--platforms=//p:x"][..], "//p:x", DEFAULT_HOST_PLATFORM),
            (&["--host_platform", "//p:h"][..], "//p:h", "//p:h"),
            (
                &["--experimental_platforms=p:x,p:y", "--host_platform=//p:h"][..],
                "//p:x",
                "//p:h",
            ),
        ] {
            let configuration = parsed(args).unwrap();
            assert_eq!(configuration.native.platforms, [label(target)], "{args:?}");
            assert_eq!(*configuration.native.target_platform(), label(target));
            assert_eq!(
                configuration.native.host_platform,
                parse_canonical_option_label(host).unwrap()
            );
            assert_ne!(configuration.id, Configuration::default().id);
        }
        // An apparent repository the main repository does not see is kept
        // as an unresolvable label for the platform lookup.
        let mut options = CommandLineOptions::default();
        options.parse("--platforms=@nope//p:x", None).unwrap();
        let configuration = options
            .configuration(&|text| {
                if text.starts_with("@nope") {
                    Err("unknown apparent repository @nope".into())
                } else {
                    canonical(text)
                }
            })
            .unwrap();
        let target = configuration.native.target_platform();
        assert_eq!(unknown_repository_name(target), Some("nope"));
        assert_eq!(
            crate::rule_declarations::canonical_form(target),
            "@@[unknown repo 'nope' requested from @@]//p:x"
        );
        assert_eq!(deferred_unknown_repository("@@nope//:x"), None);
        // Starlark options are skipped by the native parser, in order
        // (OptionsParserImpl.java:535-550); `-//` and `-@` are invalid syntax.
        let mut options = CommandLineOptions::default();
        for arg in ["--//:a=1", "--no//p:b", "--@r//:c", "--no@r//:d"] {
            assert!(matches!(
                options.parse(arg, Some("next")),
                Ok(Parsed::Consumed(false))
            ));
        }
        assert_eq!(
            options.starlark_args(),
            ["--//:a=1", "--no//p:b", "--@r//:c", "--no@r//:d"]
        );
        for arg in ["-//:a=1", "-@r//:a"] {
            assert_eq!(
                options.parse(arg, None).err().unwrap(),
                format!(
                    "Invalid options syntax: {arg}\nNote: Negative target patterns can only appear after the end of options marker ('--'). Flags corresponding to Starlark-defined build settings always start with '--', not '-'."
                )
            );
        }
        // query accepts only the modelled CoreOptions.
        let mut query = CommandLineOptions::for_query();
        // ... and ignores Starlark options (it does not analyze).
        assert!(matches!(
            query.parse("--//:a=1", None),
            Ok(Parsed::Consumed(false))
        ));
        assert!(query.starlark_args().is_empty());
        assert!(matches!(
            query.parse("-c", Some("opt")),
            Ok(Parsed::Consumed(true))
        ));
        assert!(matches!(
            query.parse("--stamp", None),
            Ok(Parsed::Consumed(false))
        ));
        assert!(matches!(
            query.parse("--define=a=1", None),
            Ok(Parsed::Consumed(false))
        ));
        for arg in ["--strip=always", "--force_pic"] {
            assert!(
                matches!(query.parse(arg, None), Ok(Parsed::NotModelled)),
                "{arg}"
            );
        }
        // query has no PlatformOptions (QueryCommand.java:59-67).
        for arg in ["--platforms=//p:x", "--host_platform=//p:h", "--platforms"] {
            assert_eq!(
                query.parse(arg, Some("next")).err(),
                Some(format!("{arg} :: Unrecognized option: {arg}"))
            );
        }
    }

    #[test]
    fn config_id_is_stable_and_value_sensitive() {
        let default = Configuration::default();
        assert_eq!(default.id, Configuration::default().id);
        assert_eq!(default.output_root(), crate::label::GENERATED_EXEC_ROOT);
        // Pinned identity of the default configuration (canonical serialization v1).
        assert_eq!(
            default.id.to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    br#"{"version":1,"native":{"compilation_mode":"fastbuild","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform"},"starlark":[]}"#
                )
            )
        );
        let opt = parsed(&["-c", "opt"]).unwrap();
        let stamped = parsed(&["--stamp"]).unwrap();
        let defined = parsed(&["--define=a=1"]).unwrap();
        let ids = [default.id, opt.id, stamped.id, defined.id];
        for (index, id) in ids.iter().enumerate() {
            assert!(!ids[index + 1..].contains(id));
        }
        // Equal option values give equal identities regardless of spelling.
        assert_eq!(parsed(&["--compilation_mode=OPT"]).unwrap().id, opt.id);
        assert_eq!(
            parsed(&["--define=a=0", "--define=a=1"]).unwrap().id,
            defined.id
        );
        assert_eq!(opt.output_root(), "bazel-out/rust-opt/bin");
        // Starlark build settings take part in the identity; only values
        // different from the default are stored (crate::starlark_options).
        let flag = Label::parse_canonical("//:flag").unwrap();
        let set = |value: SettingValue| {
            Configuration::new(
                NativeOptions::default(),
                BTreeMap::from([(flag.clone(), value)]),
            )
        };
        assert_ne!(set(SettingValue::Bool(true)).id, default.id);
        assert_ne!(
            set(SettingValue::Bool(true)).id,
            set(SettingValue::String("true".into())).id
        );
        assert_eq!(
            set(SettingValue::Label(flag.clone())).id,
            set(SettingValue::Label(flag.clone())).id
        );
    }

    #[test]
    fn unrecognized_options_are_certain_and_without_suggestions() {
        for arg in ["--:s=blue", "--s=blue", "--//x", "--zzzzzzzzzzzzzzzz=1"] {
            assert_eq!(
                unrecognized_option(arg),
                Some(format!("{arg} :: Unrecognized option: {arg}")),
                "{arg}"
            );
        }
        // Real Bazel options (also `no`-prefixed or old names) and arguments
        // close to one (SpellChecker would suggest it) stay undecided.
        for arg in [
            "--keep_going",
            "--nokeep_going",
            "--cpu=k8",
            "--experimental_platforms=//:p",
            "--keep-going",
            "--stmp",
            "-k",
            "--=x",
        ] {
            assert_eq!(unrecognized_option(arg), None, "{arg}");
        }
    }

    #[test]
    fn setting_values_print_like_java_objects() {
        assert_eq!(SettingValue::Bool(true).to_string(), "true");
        assert_eq!(SettingValue::Int(-3).to_string(), "-3");
        assert_eq!(SettingValue::String("a b".into()).to_string(), "a b");
        assert_eq!(
            SettingValue::StringList(vec!["a".into(), "".into()]).to_string(),
            "[a, ]"
        );
        assert_eq!(
            SettingValue::Label(Label::parse_canonical("@@r+//p:x").unwrap()).to_string(),
            "@@r+//p:x"
        );
    }

    #[test]
    fn option_tables_know_old_and_non_configurable_names() {
        assert_eq!(canonical_name("experimental_platforms"), "platforms");
        assert_eq!(canonical_name("cpu"), "cpu");
        assert_eq!(fragment_option("cpu"), Some(false));
        assert_eq!(fragment_option("platform_mappings"), Some(true));
        assert_eq!(fragment_option("experimental_platforms"), None);
        assert_eq!(fragment_option("is exec configuration"), None);
        assert_eq!(fragment_option("no_such_option"), None);
        for option in OPTIONS {
            assert_eq!(fragment_option(option.name), Some(option.non_configurable));
            assert_eq!(
                option.old_name.map(canonical_name),
                option.old_name.map(|_| option.name)
            );
        }
    }
}
