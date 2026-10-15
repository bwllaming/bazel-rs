//! `config_setting` analysis for `values`, `define_values` and `flag_values`
//! (rules/config/ConfigSetting.java). `constraint_values` (N16c) fails
//! explicitly when non-empty.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use std::collections::{BTreeMap, BTreeSet};

use crate::configuration::{self, Configuration, LabelParser, OptionValue, SettingValue};
use crate::configured_attributes::{ConfigMatching, Diff, MatchResult};
use crate::custom_rule::AttrValue;
use crate::label::Label;
use crate::native_rules::NativeTarget;
use crate::starlark_options::{self, BuildSettingInfo, ConversionError, SettingType};

/// A `flag_values` prerequisite as UserDefinedFlagMatch sees it
/// (ConfigSetting.java:633-797), after crate::engine analyzed and validated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagPrerequisite {
    /// AliasProvider.getDependencyLabel (analysis/AliasProvider.java:79-82):
    /// the key as written.
    pub specified: Label,
    /// TransitiveInfoCollection.getLabel: the end of an alias chain
    /// (rules/AliasConfiguredTarget.java:178-185).
    pub actual: Label,
    /// The `flag_values` value for the key.
    pub value: String,
    /// The (forwarded) BuildSettingProvider.
    pub setting: Option<BuildSettingInfo>,
}

/// ConfigSetting.PARSE_ERROR_MESSAGE (ConfigSetting.java:377-378).
const PARSE_ERROR_MESSAGE: &str = "error while parsing configuration settings: ";

/// ConfigSetting.DEPRECATED_PRE_PLATFORMS_FLAGS (ConfigSetting.java:95-97).
const DEPRECATED_PRE_PLATFORMS_FLAGS: &[&str] = &["cpu", "host_cpu", "crosstool_top"];

fn dict(target: &NativeTarget, name: &str) -> Vec<(String, String)> {
    match target.attrs.get(name) {
        Some(AttrValue::StringDict(entries) | AttrValue::LabelKeyedStringDict(entries)) => {
            entries.clone()
        }
        _ => Vec::new(),
    }
}

/// ConfigSetting.toOptionLabel (ConfigSetting.java:581-587): `//command_line_option:NAME`.
fn option_label(name: &str) -> Label {
    Label {
        repository: String::new(),
        package: "command_line_option".into(),
        name: name.into(),
    }
}

/// ConfigSetting.create (ConfigSetting.java:111-166) after the load-time-only
/// checks of crate::native_rules::analysis_error. Errors are RuleContext
/// attribute errors (`in values attribute of config_setting rule L: ...`,
/// analysis/EventHandlingErrorReporter.java:92-101), all reported.
/// `flags` are the analyzed `flag_values` prerequisites in attribute order;
/// `package_labels` parses labels in the `config_setting`'s package context.
pub fn analyze(
    label: &Label,
    target: &NativeTarget,
    configuration: &Configuration,
    flags: &[FlagPrerequisite],
    package_labels: &LabelParser<'_>,
) -> Result<ConfigMatching, String> {
    let shown = crate::rule_declarations::canonical_form(label);
    // getSettings (ConfigSetting.java:169-258): values entries, then
    // define_values as ("define", "K=V"), grouped by key in first-seen key
    // order (MultimapBuilder.linkedHashKeys().arrayListValues()). No
    // --flag_alias is modelled, so no entry is an alias.
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    let defines = dict(target, "define_values")
        .into_iter()
        .map(|(key, value)| ("define".to_owned(), format!("{key}={value}")));
    for (key, value) in dict(target, "values").into_iter().chain(defines) {
        match grouped.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, values)) => values.push(value),
            None => grouped.push((key, vec![value])),
        }
    }
    let settings = grouped
        .into_iter()
        .flat_map(|(key, values)| values.into_iter().map(move |value| (key.clone(), value)))
        .collect::<Vec<_>>();
    let mut errors = Vec::new();
    // diffNativeFlags (ConfigSetting.java:403-417): every entry is checked.
    let mut native = MatchResult::Match;
    for (name, expected) in &settings {
        let result = match check_option_value(configuration, name, expected) {
            Ok(result) => result,
            Err(error) => {
                errors.push(format!(
                    "in values attribute of config_setting rule {shown}: {error}"
                ));
                MatchResult::NoMatch(Vec::new())
            }
        };
        native = native.combine(result);
    }
    // UserDefinedFlagMatch (ConfigSetting.java:141-143): its attribute
    // errors follow the values errors; any error fails the rule (:146-148).
    let (user_defined, flag_values) =
        user_defined_flags(&shown, flags, configuration, package_labels, &mut errors);
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    // diffConstraintValues (ConfigSetting.java:144).
    if !target.labels("constraint_values").is_empty() {
        return Err(format!(
            "in config_setting rule {shown}: config_setting constraint_values (target platform constraint analysis) not implemented in the native profile"
        ));
    }
    Ok(ConfigMatching {
        label: label.clone(),
        settings,
        flags: flag_values,
        constraints: BTreeSet::new(),
        // Stream.of(userDefinedFlags, nativeFlags, constraintValues).reduce(combine)
        // with the constraint result MATCH.
        result: user_defined.combine(native).combine(MatchResult::Match),
    })
}

/// ConfigSetting.maybeCanonicalizeLabel (ConfigSetting.java:799-834): the
/// expected value of a label-typed setting, parsed in the config_setting's
/// package, as Label.getUnambiguousCanonicalForm (`@@repo//pkg:name`).
fn maybe_canonicalize_label(
    value: &str,
    setting: Option<&BuildSettingInfo>,
    package_labels: &LabelParser<'_>,
) -> String {
    if setting.is_none_or(|setting| setting.ty != SettingType::Label) {
        return value.to_owned();
    }
    match package_labels(value) {
        Ok(label) => format!("@@{}//{}:{}", label.repository, label.package, label.name),
        Err(_) => value.to_owned(),
    }
}

/// UserDefinedFlagMatch.fromAttributeValueAndPrerequisites
/// (ConfigSetting.java:633-797) for build settings (config_feature_flag is
/// not a Bazel 9.3 rule of this profile). Attribute errors are appended to
/// `errors`; returns the match result and `getSpecifiedFlagValues` (keyed by
/// the end of each alias chain).
fn user_defined_flags(
    shown: &str,
    flags: &[FlagPrerequisite],
    configuration: &Configuration,
    package_labels: &LabelParser<'_>,
    errors: &mut Vec<String>,
) -> (MatchResult, BTreeMap<Label, String>) {
    let attribute_error = |message: String| {
        format!("in flag_values attribute of config_setting rule {shown}: {message}")
    };
    let shown_label = crate::rule_declarations::canonical_form;
    let mut specified_values: Vec<(Label, String)> = Vec::new();
    let mut diffs = Vec::new();
    let mut found_duplicate = false;
    for flag in flags {
        let specified = shown_label(&flag.specified);
        let value = maybe_canonicalize_label(&flag.value, flag.setting.as_ref(), package_labels);
        // LinkedHashMap.put: a repeated key keeps its position.
        match specified_values
            .iter_mut()
            .find(|(actual, _)| *actual == flag.actual)
        {
            Some(entry) => {
                found_duplicate = true;
                entry.1 = value.clone();
            }
            None => specified_values.push((flag.actual.clone(), value.clone())),
        }
        let Some(setting) = &flag.setting else {
            errors.push(attribute_error(format!(
                "error while parsing user-defined configuration values: flag_values keys must be build settings or feature flags and {specified} is not"
            )));
            continue;
        };
        let configured = setting.value(configuration);
        // BUILD_SETTING_CONVERTERS with a null conversion context: the value
        // is already canonical (CoreOptionConverters.convertOptionsLabel, :345-350).
        let converted = match starlark_options::convert(
            setting.ty,
            &value,
            &configuration::parse_canonical_option_label,
        ) {
            Ok(converted) => converted,
            Err(ConversionError::Invalid) => {
                errors.push(attribute_error(format!(
                    "error while parsing user-defined configuration values: '{value}' cannot be converted to {specified} type {}",
                    setting.ty.name()
                )));
                continue;
            }
            Err(ConversionError::IntRange) => {
                errors.push(attribute_error(format!(
                    "'{value}' for {specified}: int build setting values outside the signed 64-bit range are not implemented in the native profile"
                )));
                continue;
            }
        };
        let diff = || Diff {
            what: flag.specified.clone(),
            got: converted.to_string(),
            want: configured.to_string(),
        };
        // List-valued settings match if any entry equals the single expected
        // value (:722-756); allow_multiple settings are not modelled.
        if let SettingValue::StringList(actual_values) = &configured {
            let SettingValue::StringList(expected) = &converted else {
                unreachable!("string_list settings convert to lists");
            };
            if expected.len() != 1 {
                errors.push(attribute_error(format!(
                    "\"{value}\" not a valid value for flag {specified}. Only single, exact values are allowed. If you want to match multiple values, consider Skylib's selects.config_setting_group"
                )));
            } else if !actual_values.contains(&expected[0]) {
                diffs.push(diff());
            }
        } else if configured != converted {
            diffs.push(diff());
        }
    }
    if found_duplicate {
        // collectAliases (:611-619): specified labels grouped by actual label.
        let mut grouped: Vec<(&Label, Vec<String>)> = Vec::new();
        for flag in flags {
            match grouped
                .iter_mut()
                .find(|(actual, _)| **actual == flag.actual)
            {
                Some((_, aliases)) => aliases.push(shown_label(&flag.specified)),
                None => grouped.push((&flag.actual, vec![shown_label(&flag.specified)])),
            }
        }
        for (actual, aliases) in grouped {
            if aliases.len() > 1 {
                errors.push(attribute_error(format!(
                    "flag '{}' referenced multiple times as ['{}']",
                    shown_label(actual),
                    aliases.join("', '")
                )));
            }
        }
    }
    let result = if diffs.is_empty() {
        MatchResult::Match
    } else {
        MatchResult::NoMatch(diffs)
    };
    (result, specified_values.into_iter().collect())
}

/// ConfigSetting.checkOptionValue (ConfigSetting.java:420-482). `Err` is the
/// attribute error message (after the rule-context prefix).
fn check_option_value(
    configuration: &Configuration,
    name: &str,
    expected: &str,
) -> Result<MatchResult, String> {
    // --incompatible_disable_select_on defaults to empty (CoreOptions.java:111-119).
    if configuration::fragment_option(name) == Some(true) {
        let mut message = format!("{PARSE_ERROR_MESSAGE}select() on '{name}' is not allowed.");
        if DEPRECATED_PRE_PLATFORMS_FLAGS.contains(&name) {
            message.push_str(
                " Use platform constraints instead: https://bazel.build/docs/configurable-attributes#platforms.",
            );
        }
        return Err(message);
    }
    let canonical = configuration::canonical_name(name);
    if configuration::fragment_option(canonical).is_none() {
        return Err(format!("{PARSE_ERROR_MESSAGE}unknown option: '{name}'"));
    }
    let Some(option) = configuration::option(canonical) else {
        // A real Bazel option whose value the native profile does not model
        // (including test options, which trimming may hide, :452-461, and the
        // deprecated --cpu family's warnings, :440-447): never guess a match.
        return Err(format!(
            "select() on build option '{name}' is not implemented in the native profile (modelled options: {})",
            configuration::OPTIONS
                .iter()
                .map(|option| option.name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    // OptionsParser.parse("--NAME=VALUE") with a null conversion context
    // (CoreOptionConverters.convertOptionsLabel, :345-350).
    let parsed = configuration::convert(
        option,
        expected,
        &configuration::parse_canonical_option_label,
    )
    .map_err(|error| {
        format!("{PARSE_ERROR_MESSAGE}While parsing option --{name}={expected}: {error}")
    })?;
    let actual = configuration
        .native
        .value(canonical)
        .expect("modelled option value");
    Ok(option_matches(
        canonical,
        option.allow_multiple,
        &actual,
        &parsed,
    ))
}

/// ConfigSetting.optionMatches (ConfigSetting.java:507-579). Modelled option
/// values are never null.
fn option_matches(
    name: &str,
    allow_multiple: bool,
    actual: &OptionValue,
    expected: &OptionValue,
) -> MatchResult {
    let no_match = |want: String, got: String| {
        MatchResult::NoMatch(vec![Diff {
            what: option_label(name),
            got,
            want,
        }])
    };
    if !allow_multiple {
        return if actual == expected {
            MatchResult::Match
        } else {
            no_match(expected.to_string(), actual.to_string())
        };
    }
    // The only modelled multi-value option is --define (a Map.Entry list).
    let (OptionValue::Assignments(actual_list), OptionValue::Assignments(expected_list)) =
        (actual, expected)
    else {
        unreachable!("multi-value option {name} is an assignment list");
    };
    if actual_list.is_empty() || expected_list.is_empty() {
        let shown = |list: &[(String, String)], value: &OptionValue| {
            if list.is_empty() {
                "<empty>".to_owned()
            } else {
                value.to_string()
            }
        };
        return if actual_list.is_empty() && expected_list.is_empty() {
            MatchResult::Match
        } else {
            no_match(shown(expected_list, expected), shown(actual_list, actual))
        };
    }
    // Iterables.getOnlyElement(expectedList): one converted assignment.
    let (key, value) = &expected_list[0];
    for (actual_key, actual_value) in actual_list.iter().rev() {
        if actual_key == key {
            return if actual_value == value {
                MatchResult::Match
            } else {
                no_match(
                    format!("{key}={value}"),
                    format!("{actual_key}={actual_value}"),
                )
            };
        }
    }
    no_match(format!("{key}={value}"), format!("<key {key} not found>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(values: &[(&str, &str)], defines: &[(&str, &str)]) -> NativeTarget {
        let dict = |entries: &[(&str, &str)]| {
            AttrValue::StringDict(
                entries
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        };
        let mut target = NativeTarget {
            rule_class: "config_setting".into(),
            ..Default::default()
        };
        target.attrs.insert("values".into(), dict(values));
        target.attrs.insert("define_values".into(), dict(defines));
        target
    }

    fn configuration(args: &[&str]) -> Configuration {
        let mut options = configuration::CommandLineOptions::default();
        let mut index = 0;
        while index < args.len() {
            match options
                .parse(args[index], args.get(index + 1).copied())
                .unwrap()
            {
                configuration::Parsed::Consumed(next) => index += 1 + usize::from(next),
                configuration::Parsed::NotModelled => panic!("{}", args[index]),
            }
        }
        options
            .configuration(&configuration::parse_canonical_option_label)
            .unwrap()
    }

    fn result(values: &[(&str, &str)], defines: &[(&str, &str)], args: &[&str]) -> MatchResult {
        let label = Label::parse_canonical("//c:x").unwrap();
        analyze(
            &label,
            &target(values, defines),
            &configuration(args),
            &[],
            &|_| unreachable!("no labels"),
        )
        .unwrap()
        .result
    }

    #[test]
    fn values_match_modelled_options_like_bazel() {
        let matched = MatchResult::Match;
        assert_eq!(
            result(&[("compilation_mode", "fastbuild")], &[], &[]),
            matched
        );
        assert_eq!(
            result(&[("compilation_mode", "OPT")], &[], &["-c", "opt"]),
            matched
        );
        assert_eq!(result(&[("stamp", "yes")], &[], &["--stamp"]), matched);
        assert_eq!(result(&[("strip", "sometimes")], &[], &[]), matched);
        assert_eq!(result(&[("force_pic", "0")], &[], &[]), matched);
        assert_eq!(
            result(
                &[("platforms", "@bazel_tools//tools:host_platform")],
                &[],
                &[]
            ),
            matched
        );
        assert_eq!(
            result(
                &[(
                    "experimental_platforms",
                    "@bazel_tools//tools:host_platform"
                )],
                &[],
                &["--platforms=@bazel_tools//tools:host_platform,//p:b"]
            ),
            matched
        );
        assert_eq!(
            result(&[("platforms", "//p:a")], &[], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("platforms"),
                got: "[@@bazel_tools//tools:host_platform]".into(),
                want: "[//p:a]".into(),
            }])
        );
        assert_eq!(
            result(&[("compilation_mode", "dbg")], &[], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("compilation_mode"),
                got: "fastbuild".into(),
                want: "dbg".into(),
            }])
        );
        // --define: last value per key wins; missing keys and empty lists differ.
        assert_eq!(
            result(&[], &[("a", "2")], &["--define=a=1", "--define=a=2"]),
            matched
        );
        assert_eq!(
            result(&[("define", "a=1")], &[], &["--define=a=2"]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("define"),
                got: "a=2".into(),
                want: "a=1".into(),
            }])
        );
        assert_eq!(
            result(&[], &[("b", "1")], &["--define=a=2"]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("define"),
                got: "<key b not found>".into(),
                want: "b=1".into(),
            }])
        );
        assert_eq!(
            result(&[], &[("b", "1")], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("define"),
                got: "<empty>".into(),
                want: "[b=1]".into(),
            }])
        );
    }

    #[test]
    fn values_errors_are_bazel_attribute_errors() {
        let label = Label::parse_canonical("//c:x").unwrap();
        let error = |values: &[(&str, &str)]| {
            analyze(
                &label,
                &target(values, &[]),
                &Configuration::default(),
                &[],
                &|_| unreachable!("no labels"),
            )
            .unwrap_err()
        };
        assert_eq!(
            error(&[("nope", "1")]),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: unknown option: 'nope'"
        );
        assert_eq!(
            error(&[("platform_mappings", "x")]),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: select() on 'platform_mappings' is not allowed."
        );
        assert_eq!(
            error(&[("compilation_mode", "fast"), ("stamp", "maybe")]),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --compilation_mode=fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)\nin values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --stamp=maybe: 'maybe' is not a boolean"
        );
        assert!(
            error(&[("cpu", "k8")]).contains(
                "select() on build option 'cpu' is not implemented in the native profile"
            )
        );
    }

    fn flag(
        specified: &str,
        actual: &str,
        value: &str,
        setting: Option<(&str, SettingType, SettingValue)>,
    ) -> FlagPrerequisite {
        FlagPrerequisite {
            specified: Label::parse_canonical(specified).unwrap(),
            actual: Label::parse_canonical(actual).unwrap(),
            value: value.into(),
            setting: setting.map(|(label, ty, default)| BuildSettingInfo {
                label: Label::parse_canonical(label).unwrap(),
                ty,
                default,
                is_flag: true,
                repeatable: false,
            }),
        }
    }

    fn flags_result(
        flags: &[FlagPrerequisite],
        configuration: &Configuration,
    ) -> Result<ConfigMatching, String> {
        let label = Label::parse_canonical("//c:x").unwrap();
        let mut target = target(&[], &[]);
        target.attrs.insert(
            "flag_values".into(),
            AttrValue::LabelKeyedStringDict(vec![("//f:x".into(), "1".into())]),
        );
        analyze(&label, &target, configuration, flags, &|text| {
            let text = text.strip_prefix(':').unwrap_or(text);
            if text.starts_with("//") || text.starts_with("@@") {
                Label::parse_canonical(text)
            } else if text.contains(':') {
                Err("bad label".into())
            } else {
                Label::parse_canonical(&format!("//c:{text}"))
            }
        })
    }

    #[test]
    fn flag_values_match_through_the_setting_type() {
        let bool_flag = || Some(("//f:b", SettingType::Bool, SettingValue::Bool(false)));
        let default = Configuration::default();
        let matching =
            flags_result(&[flag("//f:b", "//f:b", "False", bool_flag())], &default).unwrap();
        assert_eq!(matching.result, MatchResult::Match);
        assert_eq!(
            matching.flags,
            BTreeMap::from([(Label::parse_canonical("//f:b").unwrap(), "False".into())])
        );
        // NoMatch diffs: `got` is the expected value, `want` the configured
        // one (ConfigSetting.java:757-762).
        let set = Configuration::new(
            Default::default(),
            BTreeMap::from([(
                Label::parse_canonical("//f:b").unwrap(),
                SettingValue::Bool(true),
            )]),
        );
        assert_eq!(
            flags_result(&[flag("//f:a", "//f:b", "0", bool_flag())], &set)
                .unwrap()
                .result,
            MatchResult::NoMatch(vec![Diff {
                what: Label::parse_canonical("//f:a").unwrap(),
                got: "false".into(),
                want: "true".into(),
            }])
        );
        // Lists match by containment of a single expected value.
        let list = || {
            Some((
                "//f:l",
                SettingType::StringList,
                SettingValue::StringList(vec!["a".into(), "b".into()]),
            ))
        };
        assert_eq!(
            flags_result(&[flag("//f:l", "//f:l", "b", list())], &default)
                .unwrap()
                .result,
            MatchResult::Match
        );
        assert_eq!(
            flags_result(&[flag("//f:l", "//f:l", "c", list())], &default)
                .unwrap()
                .result,
            MatchResult::NoMatch(vec![Diff {
                what: Label::parse_canonical("//f:l").unwrap(),
                got: "[c]".into(),
                want: "[a, b]".into(),
            }])
        );
        // Label values are canonicalized in the config_setting's package.
        let label_flag = || {
            Some((
                "//f:lf",
                SettingType::Label,
                SettingValue::Label(Label::parse_canonical("//c:dflt").unwrap()),
            ))
        };
        let matching = flags_result(
            &[flag("//f:lf", "//c:dflt", ":dflt", label_flag())],
            &default,
        )
        .unwrap();
        assert_eq!(matching.result, MatchResult::Match);
        assert_eq!(matching.flags.values().collect::<Vec<_>>(), ["@@//c:dflt"]);
        let error = flags_result(
            &[flag("//f:lf", "//c:dflt", "a:b:c", label_flag())],
            &default,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "in flag_values attribute of config_setting rule //c:x: error while parsing user-defined configuration values: 'a:b:c' cannot be converted to //f:lf type label"
        );
        assert_eq!(
            flags_result(
                &[flag(
                    "//f:i",
                    "//f:i",
                    "99999999999999999999",
                    Some(("//f:i", SettingType::Int, SettingValue::Int(0)))
                )],
                &default
            )
            .unwrap_err(),
            "in flag_values attribute of config_setting rule //c:x: '99999999999999999999' for //f:i: int build setting values outside the signed 64-bit range are not implemented in the native profile"
        );
        // Every error is reported, then duplicates by actual label.
        assert_eq!(
            flags_result(
                &[
                    flag("//f:fg", "//f:fg", "1", None),
                    flag("//f:b", "//f:b", "maybe", bool_flag()),
                    flag("//f:a", "//f:b", "1", bool_flag()),
                ],
                &default
            )
            .unwrap_err(),
            "in flag_values attribute of config_setting rule //c:x: error while parsing user-defined configuration values: flag_values keys must be build settings or feature flags and //f:fg is not\nin flag_values attribute of config_setting rule //c:x: error while parsing user-defined configuration values: 'maybe' cannot be converted to //f:b type boolean\nin flag_values attribute of config_setting rule //c:x: flag '//f:b' referenced multiple times as ['//f:b', '//f:a']"
        );
    }

    #[test]
    fn settings_group_define_values_with_values_define_entries() {
        let label = Label::parse_canonical("//c:x").unwrap();
        let matching = analyze(
            &label,
            &target(&[("define", "a=1"), ("stamp", "0")], &[("b", "2")]),
            &configuration(&["--define=a=1", "--define=b=2"]),
            &[],
            &|_| unreachable!("no labels"),
        )
        .unwrap();
        assert_eq!(
            matching.settings,
            [
                ("define".to_owned(), "a=1".to_owned()),
                ("define".into(), "b=2".into()),
                ("stamp".into(), "0".into())
            ]
        );
        assert_eq!(matching.result, MatchResult::Match);
    }
}
