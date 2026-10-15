//! Starlark build settings: their native BuildSettingProvider record, the
//! typed converters of `BUILD_SETTING_CONVERTERS` and the command-line parser
//! of `--//pkg:flag=value` options (runtime/StarlarkOptionsParser.java).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use std::collections::BTreeMap;

use crate::configuration::{Configuration, LabelParser, SettingValue};
use crate::label::Label;

/// The `Type` of a build setting (packages/BuildSetting.java:26-45).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingType {
    Bool,
    Int,
    String,
    StringList,
    /// NODEP_LABEL of label_flag/label_setting (rules/LabelBuildSettings.java:97).
    Label,
}

impl SettingType {
    /// `Type.toString` (packages/Type.java:352-354, 405-407, 464-466, 725-727;
    /// packages/BuildType.java:402-404).
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "boolean",
            Self::Int => "int",
            Self::String => "string",
            Self::StringList => "list(string)",
            Self::Label => "label",
        }
    }

    pub fn of(kind: crate::build_settings::BuildSettingKind) -> Self {
        use crate::build_settings::BuildSettingKind;
        match kind {
            BuildSettingKind::Bool => Self::Bool,
            BuildSettingKind::Int => Self::Int,
            BuildSettingKind::String => Self::String,
            BuildSettingKind::StringList => Self::StringList,
        }
    }
}

/// The native BuildSettingProvider (analysis/BuildSettingProvider.java:27-58)
/// of a build setting target: added for every build-setting rule
/// (analysis/RuleConfiguredTargetBuilder.java:250-259) and by label_flag /
/// label_setting (rules/LateBoundAlias.java:66-79). Not visible to Starlark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildSettingInfo {
    pub label: Label,
    pub ty: SettingType,
    /// `build_setting_default` as stored on the rule.
    pub default: SettingValue,
    /// `BuildSetting.isFlag`: settable on the command line.
    pub is_flag: bool,
    /// `BuildSetting.isRepeatableFlag` (`config.string_list(repeatable=True)`).
    pub repeatable: bool,
}

impl BuildSettingInfo {
    /// The value in `configuration`: the command-line value, else the default
    /// (StarlarkRuleContext.getBuildSettingValue, analysis/starlark/StarlarkRuleContext.java:738-757;
    /// LabelBuildSettings.ACTUAL, rules/LabelBuildSettings.java:62-83).
    /// `allow_multiple` settings are not modelled (crate::build_settings).
    pub fn value(&self, configuration: &Configuration) -> SettingValue {
        configuration
            .starlark
            .get(&self.label)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
}

/// A target loaded for a command-line option name
/// (StarlarkOptionsParser.BuildSettingLoader, runtime/StarlarkOptionsParser.java:70-81).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadedTarget {
    /// A target without an associated rule (an input file).
    NotRule(Label),
    BuildSetting(BuildSettingInfo),
    /// An `alias`; `actual` is `None` when it is a `select()`.
    Alias {
        label: Label,
        actual: Option<Label>,
    },
    /// Any other rule (or a rule's output file).
    OtherRule(Label),
}

impl LoadedTarget {
    fn label(&self) -> &Label {
        match self {
            Self::NotRule(label) | Self::OtherRule(label) | Self::Alias { label, .. } => label,
            Self::BuildSetting(info) => &info.label,
        }
    }
}

/// Why a conversion failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversionError {
    /// The converter rejected the input (OptionsParsingException).
    Invalid,
    /// A valid Starlark int outside the signed 64-bit range: Bazel accepts it
    /// (BigInteger), the native profile does not model it.
    IntRange,
}

/// `StarlarkInt.parse(input, 0)` (net/starlark/java/eval/StarlarkInt.java:99-168)
/// restricted to the signed 64-bit range.
pub fn parse_starlark_int(input: &str) -> Result<i64, ConversionError> {
    let (negative, rest) = match input.as_bytes().first() {
        None => return Err(ConversionError::Invalid),
        Some(b'+') => (false, &input[1..]),
        Some(b'-') => (true, &input[1..]),
        Some(_) => (false, input),
    };
    let mut base = 0u32;
    let mut digits = rest;
    if rest.len() > 1 && rest.starts_with('0') {
        let prefix = match rest.as_bytes()[1] {
            b'b' | b'B' => 2,
            b'o' | b'O' => 8,
            b'x' | b'X' => 16,
            _ => 0,
        };
        if prefix != 0 {
            base = prefix;
            digits = &rest[2..];
        }
    }
    if base == 0 {
        // No octal/decimal guessing for a leading 0 (:138-145).
        if rest.len() > 1 && rest.starts_with('0') {
            return Err(ConversionError::Invalid);
        }
        base = 10;
    }
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return Err(ConversionError::Invalid);
    }
    let mut magnitude: u128 = 0;
    let mut overflow = false;
    for c in digits.chars() {
        let digit = c.to_digit(base).ok_or(ConversionError::Invalid)?;
        match magnitude
            .checked_mul(u128::from(base))
            .and_then(|value| value.checked_add(u128::from(digit)))
        {
            Some(value) => magnitude = value,
            None => overflow = true,
        }
    }
    let limit = if negative {
        u128::from(i64::MAX as u64) + 1
    } else {
        u128::from(i64::MAX as u64)
    };
    if overflow || magnitude > limit {
        return Err(ConversionError::IntRange);
    }
    Ok(if negative {
        (magnitude as i128).wrapping_neg() as i64
    } else {
        magnitude as i64
    })
}

/// `BUILD_SETTING_CONVERTERS` (analysis/config/CoreOptionConverters.java:76-86):
/// StarlarkIntConverter (:110-127), BooleanConverter
/// (common/options/Converters.java:46-60), StringConverter,
/// CommaSeparatedOptionListConverter (Converters.java:252-323: empty input is
/// `[]`, empty elements are kept) and LabelConverter (convertOptionsLabel,
/// CoreOptionConverters.java:325-361: `//` prefixed unless `/` or `@`).
pub fn convert(
    ty: SettingType,
    input: &str,
    labels: &LabelParser<'_>,
) -> Result<SettingValue, ConversionError> {
    Ok(match ty {
        SettingType::Bool => {
            let lower = input.to_ascii_lowercase();
            if matches!(lower.as_str(), "true" | "1" | "yes" | "t" | "y") {
                SettingValue::Bool(true)
            } else if matches!(lower.as_str(), "false" | "0" | "no" | "f" | "n") {
                SettingValue::Bool(false)
            } else {
                return Err(ConversionError::Invalid);
            }
        }
        SettingType::Int => SettingValue::Int(parse_starlark_int(input)?),
        SettingType::String => SettingValue::String(input.to_owned()),
        SettingType::StringList => SettingValue::StringList(if input.is_empty() {
            Vec::new()
        } else {
            input.split(',').map(str::to_owned).collect()
        }),
        SettingType::Label => {
            let text = if input.starts_with('/') || input.starts_with('@') {
                input.to_owned()
            } else {
                format!("//{input}")
            };
            SettingValue::Label(labels(&text).map_err(|_| ConversionError::Invalid)?)
        }
    })
}

/// An OptionsParsingException: the message and its invalid argument.
struct Failure {
    message: String,
    argument: Option<String>,
}

impl Failure {
    fn new(message: String, argument: Option<&str>) -> Self {
        Self {
            message,
            argument: argument.map(str::to_owned),
        }
    }
}

/// `formatAliasChain` (StarlarkOptionsParser.java:419-421).
fn format_chain<'a>(chain: impl IntoIterator<Item = &'a Label>) -> String {
    chain
        .into_iter()
        .map(crate::rule_declarations::canonical_form)
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// StarlarkOptionsParser.loadBuildSetting (StarlarkOptionsParser.java:350-417):
/// follows unconfigured aliases to a build setting.
fn load_build_setting(
    name: &str,
    loader: &mut dyn FnMut(&str) -> Result<LoadedTarget, String>,
    cache: &mut BTreeMap<String, BuildSettingInfo>,
) -> Result<BuildSettingInfo, Failure> {
    if let Some(info) = cache.get(name) {
        return Ok(info.clone());
    }
    let mut next = name.to_owned();
    let mut chain: Vec<Label> = Vec::new();
    loop {
        let target = loader(&next).map_err(|error| {
            Failure::new(format!("Error loading option {name}: {error}"), Some(name))
        })?;
        let label = target.label().clone();
        if chain.contains(&label) {
            return Err(Failure::new(
                format!(
                    "Failed to load build setting '{name}' due to a cycle in alias chain: {}",
                    format_chain(chain.iter().chain(std::iter::once(&label)))
                ),
                Some(name),
            ));
        }
        chain.push(label);
        match target {
            LoadedTarget::BuildSetting(info) => {
                cache.insert(name.to_owned(), info.clone());
                return Ok(info);
            }
            LoadedTarget::Alias {
                actual: Some(actual),
                ..
            } => next = crate::rule_declarations::canonical_form(&actual),
            LoadedTarget::Alias { actual: None, .. } => {
                return Err(Failure::new(
                    format!(
                        "Failed to load build setting '{name}' as it resolves to an alias with an actual value that uses select(): {}. This is not supported as build settings are needed to determine the configuration the select is evaluated in.",
                        format_chain(&chain)
                    ),
                    Some(name),
                ));
            }
            LoadedTarget::NotRule(_) | LoadedTarget::OtherRule(_) => {
                return Err(Failure::new(
                    format!("Unrecognized option: {}", format_chain(&chain)),
                    Some(name),
                ));
            }
        }
    }
}

/// StarlarkOptionsParser.parseGivenArgs (StarlarkOptionsParser.java:159-281)
/// over the skipped `--//`, `--no//`, `--@`, `--no@` arguments, with
/// BlazeOptionHandler's error rendering (runtime/BlazeOptionHandler.java:417-450,
/// 623-640): `ARG :: MESSAGE` when the exception names an invalid argument,
/// plus `. Did you mean ARG=NEXT?` when that argument is followed by a
/// non-option. Only values different from the setting's default are kept
/// (`includeDefaultValues` is false, :240-254).
pub fn parse(
    args: &[String],
    command_args: &[String],
    loader: &mut dyn FnMut(&str) -> Result<LoadedTarget, String>,
    labels: &LabelParser<'_>,
) -> Result<BTreeMap<Label, SettingValue>, String> {
    parse_inner(args, loader, labels).map_err(|failure| {
        let Some(argument) = failure.argument else {
            return failure.message;
        };
        let mut message = failure.message;
        for pair in command_args.windows(2) {
            if pair[0] == argument && !pair[1].starts_with('-') {
                message = format!("{message}. Did you mean {}={}?", pair[0], pair[1]);
            }
        }
        format!("{argument} :: {message}")
    })
}

fn parse_inner(
    args: &[String],
    loader: &mut dyn FnMut(&str) -> Result<LoadedTarget, String>,
    labels: &LabelParser<'_>,
) -> Result<BTreeMap<Label, SettingValue>, Failure> {
    let mut cache = BTreeMap::new();
    // parseArg (:289-342): every argument is loaded before any conversion.
    let mut unparsed: Vec<(String, String, BuildSettingInfo)> = Vec::new();
    for arg in args {
        let Some(rest) = arg.strip_prefix("--") else {
            return Err(Failure::new(
                format!("Invalid options syntax: {arg}"),
                Some(arg),
            ));
        };
        let (name, value) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (rest, None),
        };
        if name.trim().is_empty() {
            return Err(Failure::new(
                format!("Invalid options syntax: {arg}"),
                Some(arg),
            ));
        }
        if let Some(value) = value {
            let info = load_build_setting(name, loader, &mut cache)?;
            let flag = crate::rule_declarations::canonical_form(&info.label);
            unparsed.push((flag, value.to_owned(), info));
            continue;
        }
        let (name, enabled) = match name.strip_prefix("no") {
            Some(name) => (name, false),
            None => (name, true),
        };
        let info = load_build_setting(name, loader, &mut cache)?;
        if info.ty == SettingType::Bool {
            let flag = crate::rule_declarations::canonical_form(&info.label);
            unparsed.push((flag, enabled.to_string(), info));
        } else if !enabled {
            return Err(Failure::new(
                format!("Illegal use of 'no' prefix on non-boolean option: {name}"),
                Some(name),
            ));
        } else {
            return Err(Failure::new(
                format!("Expected value after {arg}"),
                Some(arg),
            ));
        }
    }
    // Conversion in argument order (LinkedListMultimap entries, :179-225).
    let mut parsed: Vec<(BuildSettingInfo, SettingValue)> = Vec::new();
    for (flag, raw, info) in unparsed {
        // Internal (non-flag) settings are unrecognized (:185-189).
        if !info.is_flag {
            return Err(Failure::new(
                format!("Unrecognized option: {flag}={raw}"),
                None,
            ));
        }
        let ty = if info.repeatable {
            SettingType::String
        } else {
            info.ty
        };
        let value = convert(ty, &raw, labels).map_err(|error| match error {
            ConversionError::Invalid => Failure::new(
                format!(
                    "While parsing option {flag}={raw}: '{raw}' is not a {}",
                    ty.name()
                ),
                None,
            ),
            ConversionError::IntRange => Failure::new(
                format!(
                    "While parsing option {flag}={raw}: int build setting values outside the signed 64-bit range are not implemented in the native profile"
                ),
                None,
            ),
        })?;
        let existing = parsed.iter_mut().find(|(seen, _)| seen.label == info.label);
        let value = if info.repeatable {
            let SettingValue::String(element) = value else {
                unreachable!("repeatable element type is string");
            };
            let mut list = match existing.as_ref().map(|(_, value)| value) {
                Some(SettingValue::StringList(list)) => list.clone(),
                _ => Vec::new(),
            };
            list.push(element);
            SettingValue::StringList(list)
        } else {
            value
        };
        match existing {
            Some(entry) => entry.1 = value,
            None => parsed.push((info, value)),
        }
    }
    let mut options = BTreeMap::new();
    for (info, value) in parsed {
        if value != info.default {
            options.insert(info.label, value);
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn labels(options: &BTreeMap<Label, SettingValue>) -> BTreeSet<String> {
        options
            .keys()
            .map(crate::rule_declarations::canonical_form)
            .collect()
    }

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn setting(name: &str, ty: SettingType, default: SettingValue, flag: bool) -> LoadedTarget {
        LoadedTarget::BuildSetting(BuildSettingInfo {
            label: label(name),
            ty,
            default,
            is_flag: flag,
            repeatable: false,
        })
    }

    fn loader(name: &str) -> Result<LoadedTarget, String> {
        let name = name.strip_prefix("@@").unwrap_or(name);
        Ok(match name {
            "//:b" => setting("//:b", SettingType::Bool, SettingValue::Bool(false), true),
            "//:i" => setting("//:i", SettingType::Int, SettingValue::Int(3), true),
            "//:s" => setting(
                "//:s",
                SettingType::String,
                SettingValue::String("red".into()),
                true,
            ),
            "//:sl" => setting(
                "//:sl",
                SettingType::StringList,
                SettingValue::StringList(vec!["a".into()]),
                true,
            ),
            "//:r" => LoadedTarget::BuildSetting(BuildSettingInfo {
                label: label("//:r"),
                ty: SettingType::StringList,
                default: SettingValue::StringList(vec![]),
                is_flag: true,
                repeatable: true,
            }),
            "//:lf" => setting(
                "//:lf",
                SettingType::Label,
                SettingValue::Label(label("//:d")),
                true,
            ),
            "//:ls" => setting(
                "//:ls",
                SettingType::Label,
                SettingValue::Label(label("//:d")),
                false,
            ),
            "//:bs" => setting("//:bs", SettingType::Bool, SettingValue::Bool(false), false),
            "//:a" => LoadedTarget::Alias {
                label: label("//:a"),
                actual: Some(label("//:s")),
            },
            "//:a2" => LoadedTarget::Alias {
                label: label("//:a2"),
                actual: Some(label("//:a")),
            },
            "//:sel" => LoadedTarget::Alias {
                label: label("//:sel"),
                actual: None,
            },
            "//:c1" => LoadedTarget::Alias {
                label: label("//:c1"),
                actual: Some(label("//:c2")),
            },
            "//:c2" => LoadedTarget::Alias {
                label: label("//:c2"),
                actual: Some(label("//:c1")),
            },
            "//:fa" => LoadedTarget::Alias {
                label: label("//:fa"),
                actual: Some(label("//:fg")),
            },
            "//:fg" => LoadedTarget::OtherRule(label("//:fg")),
            "//:file.txt" => LoadedTarget::NotRule(label("//:file.txt")),
            other => return Err(format!("no such target '{other}'")),
        })
    }

    fn run(args: &[&str]) -> Result<BTreeMap<Label, SettingValue>, String> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        parse(&args, &args, &mut loader, &|text| {
            Label::parse_canonical(text)
        })
    }

    #[test]
    fn values_follow_build_setting_converters_and_drop_defaults() {
        let options = run(&[
            "--//:b",
            "--//:i=0x10",
            "--//:s=blue",
            "--//:sl=x,,y",
            "--//:sl=p,q",
            "--//:r=one",
            "--//:r=two",
            "--//:lf=other",
        ])
        .unwrap();
        assert_eq!(options[&label("//:b")], SettingValue::Bool(true));
        assert_eq!(options[&label("//:i")], SettingValue::Int(16));
        assert_eq!(options[&label("//:s")], SettingValue::String("blue".into()));
        assert_eq!(
            options[&label("//:sl")],
            SettingValue::StringList(vec!["p".into(), "q".into()])
        );
        assert_eq!(
            options[&label("//:r")],
            SettingValue::StringList(vec!["one".into(), "two".into()])
        );
        // convertOptionsLabel prefixes `//`: `other` is //other:other.
        assert_eq!(
            options[&label("//:lf")],
            SettingValue::Label(label("//other:other"))
        );
        // Values equal to the default are not stored (same identity as unset).
        for args in [
            &["--no//:b"][..],
            &["--//:b=FALSE"][..],
            &["--//:i=3"][..],
            &["--//:s=red"][..],
            &["--//:sl=a"][..],
            &["--//:s=blue", "--//:s=red"][..],
            &["--//:lf=//:d"][..],
            &["--//:a=red"][..],
        ] {
            assert!(run(args).unwrap().is_empty(), "{args:?}");
        }
        // An alias chain resolves to the setting's own label.
        assert_eq!(
            labels(&run(&["--//:a2=green"]).unwrap()),
            BTreeSet::from(["//:s".to_owned()])
        );
        assert_eq!(
            run(&["--//:sl="]).unwrap()[&label("//:sl")],
            SettingValue::StringList(vec![])
        );
    }

    #[test]
    fn errors_are_bazel_options_parsing_exceptions() {
        for (args, error) in [
            (
                &["--//:i=x"][..],
                "While parsing option //:i=x: 'x' is not a int",
            ),
            (
                &["--//:i=08"][..],
                "While parsing option //:i=08: '08' is not a int",
            ),
            (
                &["--//:b=maybe"][..],
                "While parsing option //:b=maybe: 'maybe' is not a boolean",
            ),
            (
                &["--//:lf=a:b:c"][..],
                "While parsing option //:lf=a:b:c: 'a:b:c' is not a label",
            ),
            (&["--//:ls=//:x"][..], "Unrecognized option: //:ls=//:x"),
            (&["--//:bs"][..], "Unrecognized option: //:bs=true"),
            (
                &["--no//:s"][..],
                "//:s :: Illegal use of 'no' prefix on non-boolean option: //:s",
            ),
            (&["--//:s"][..], "--//:s :: Expected value after --//:s"),
            (
                &["--//:s", "blue"][..],
                "--//:s :: Expected value after --//:s. Did you mean --//:s=blue?",
            ),
            (&["--//:ls"][..], "--//:ls :: Expected value after --//:ls"),
            (
                &["--//:nope=1"][..],
                "//:nope :: Error loading option //:nope: no such target '//:nope'",
            ),
            (&["--//:fg=1"][..], "//:fg :: Unrecognized option: //:fg"),
            (
                &["--//:fa=1"][..],
                "//:fa :: Unrecognized option: //:fa -> //:fg",
            ),
            (
                &["--//:file.txt=1"][..],
                "//:file.txt :: Unrecognized option: //:file.txt",
            ),
            (
                &["--//:sel=1"][..],
                "//:sel :: Failed to load build setting '//:sel' as it resolves to an alias with an actual value that uses select(): //:sel. This is not supported as build settings are needed to determine the configuration the select is evaluated in.",
            ),
            (
                &["--//:c1=1"][..],
                "//:c1 :: Failed to load build setting '//:c1' due to a cycle in alias chain: //:c1 -> //:c2 -> //:c1",
            ),
            // Loading precedes conversion: the later load error wins.
            (
                &["--//:i=x", "--//:nope=1"][..],
                "//:nope :: Error loading option //:nope: no such target '//:nope'",
            ),
        ] {
            assert_eq!(run(args).unwrap_err(), error, "{args:?}");
        }
        assert!(
            run(&["--//:i=99999999999999999999"])
                .unwrap_err()
                .contains("not implemented in the native profile")
        );
    }

    #[test]
    fn starlark_int_parse_matches_bazel() {
        for (input, expected) in [
            ("0", Ok(0)),
            ("-12", Ok(-12)),
            ("+7", Ok(7)),
            ("0b101", Ok(5)),
            ("0O17", Ok(15)),
            ("0xff", Ok(255)),
            ("-9223372036854775808", Ok(i64::MIN)),
            ("9223372036854775807", Ok(i64::MAX)),
            ("9223372036854775808", Err(ConversionError::IntRange)),
            ("", Err(ConversionError::Invalid)),
            ("012", Err(ConversionError::Invalid)),
            ("0x", Err(ConversionError::Invalid)),
            ("--1", Err(ConversionError::Invalid)),
            ("0x-1", Err(ConversionError::Invalid)),
            ("1_000", Err(ConversionError::Invalid)),
            ("12a", Err(ConversionError::Invalid)),
        ] {
            assert_eq!(parse_starlark_int(input), expected, "{input}");
        }
    }
}
