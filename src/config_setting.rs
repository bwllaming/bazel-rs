//! `config_setting` analysis for `values`, `define_values`, `flag_values` and
//! `constraint_values` (rules/config/ConfigSetting.java).
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
    /// The ConfigFeatureFlagProvider (`config_common.FeatureFlagInfo`) the
    /// key provides: a `config_feature_flag` or a Starlark rule returning it.
    pub feature_flag: Option<crate::feature_flags::FeatureFlagValue>,
}

/// A `constraint_values` prerequisite of a `config_setting`, after
/// crate::engine analyzed and validated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintPrerequisite {
    /// TransitiveInfoCollection.getLabel: the end of an alias chain.
    pub actual: Label,
    /// The (forwarded) ConstraintValueInfo, if the target provides one.
    pub value: Option<crate::platforms::ConstraintValueInfo>,
}

/// The error [`analyze`] returns when matching `constraint_values` needs the
/// target platform and none was given (crate::engine then resolves it).
pub const NEEDS_TARGET_PLATFORM: &str = "\u{0}config_setting needs the target platform";

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
/// `flags` are the analyzed `flag_values` prerequisites in attribute order,
/// `constraints` the `constraint_values` prerequisites and `target_platform`
/// the configuration's target platform (resolved by crate::engine when
/// `constraint_values` is non-empty: the rule then uses toolchain resolution,
/// ConfigRuleClasses.java:196-201); `package_labels` parses labels in the
/// `config_setting`'s package context. Rule warnings (`in config_setting
/// rule L: ...`, EventHandlingErrorReporter.java:76-79) are appended to
/// `warnings` in report order, also when the analysis fails: Skyframe
/// reports warnings at once but replays errors after the function
/// (SkyFunctionEnvironment.java:816-837, Event.storeForReplay), so they
/// precede every error of the rule.
#[allow(clippy::too_many_arguments)] // RuleContext's inputs plus its warning sink
pub fn analyze(
    label: &Label,
    target: &NativeTarget,
    configuration: &Configuration,
    flags: &[FlagPrerequisite],
    constraints: &[ConstraintPrerequisite],
    target_platform: Option<&crate::platforms::PlatformInfo>,
    package_labels: &LabelParser<'_>,
    warnings: &mut Vec<String>,
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
        let result = match check_option_value(label, configuration, name, expected, warnings) {
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
    // diffConstraintValues (ConfigSetting.java:144, 300-357).
    let constraint_result =
        diff_constraint_values(&shown, constraints, target_platform, &mut errors);
    if errors.iter().any(|error| error == NEEDS_TARGET_PLATFORM) {
        return Err(NEEDS_TARGET_PLATFORM.to_owned());
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(ConfigMatching {
        label: label.clone(),
        settings,
        flags: flag_values,
        constraints: specified_constraint_values(constraints),
        // Stream.of(userDefinedFlags, nativeFlags, constraintValues).reduce(combine).
        result: user_defined.combine(native).combine(constraint_result),
    })
}

/// ConfigSetting.diffConstraintValues (ConfigSetting.java:300-357). Attribute
/// and rule errors are appended to `errors` (which already holds the
/// `values`/`flag_values` errors: `ruleContext.hasErrors()` sees them too).
fn diff_constraint_values(
    shown: &str,
    constraints: &[ConstraintPrerequisite],
    target_platform: Option<&crate::platforms::PlatformInfo>,
    errors: &mut Vec<String>,
) -> MatchResult {
    let attribute_error = |message: String| {
        format!("in constraint_values attribute of config_setting rule {shown}: {message}")
    };
    let mut values = Vec::new();
    for prerequisite in constraints {
        match &prerequisite.value {
            Some(value) => values.push(value),
            None => errors.push(attribute_error(format!(
                "{} is not a constraint_value",
                crate::rule_declarations::canonical_form(&prerequisite.actual)
            ))),
        }
    }
    if !errors.is_empty() {
        return MatchResult::NoMatch(Vec::new());
    }
    if values.is_empty() {
        return MatchResult::Match;
    }
    let owned = values
        .iter()
        .map(|value| (*value).clone())
        .collect::<Vec<_>>();
    if let Err(message) = crate::platforms::validate_constraints(&owned) {
        errors.push(format!("in config_setting rule {shown}: {message}"));
        return MatchResult::NoMatch(Vec::new());
    }
    // The rule uses toolchain resolution (ConfigRuleClasses.java:196-201), so
    // Bazel always has a target platform here; crate::engine resolves the
    // default host platform only now that it is needed and analyzes again.
    let Some(platform) = target_platform else {
        errors.push(NEEDS_TARGET_PLATFORM.to_owned());
        return MatchResult::NoMatch(Vec::new());
    };
    if platform.constraints.contains_all(&values) {
        return MatchResult::Match;
    }
    let mut diffs = Vec::new();
    for value in values {
        let platform_value = platform.constraints.get(&value.setting);
        if platform_value.as_ref() != Some(value) {
            diffs.push(Diff {
                what: value.setting.label.clone(),
                want: value.label.name.clone(),
                got: platform_value.map_or_else(|| "<unset>".to_owned(), |value| value.label.name),
            });
        }
    }
    MatchResult::NoMatch(diffs)
}

/// ConfigSetting.getSpecifiedConstraintValues (ConfigSetting.java:841-852):
/// each prerequisite's label plus the refinement chain of its setting.
fn specified_constraint_values(constraints: &[ConstraintPrerequisite]) -> BTreeSet<Label> {
    let mut specified = BTreeSet::new();
    for prerequisite in constraints {
        specified.insert(prerequisite.actual.clone());
        if let Some(value) = &prerequisite.value {
            specified.extend(value.setting.refinement_chain());
        }
    }
    specified
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
/// (ConfigSetting.java:633-797) for feature flags (ConfigFeatureFlagProvider)
/// and build settings. Attribute errors are appended to
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
    // Only configuration-dependent errors are deferred (:637-638).
    let mut deferred = Vec::new();
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
        // A ConfigFeatureFlagProvider takes precedence over a
        // BuildSettingProvider (:656-680): config_feature_flag and Starlark
        // rules returning config_common.FeatureFlagInfo (rules_python's
        // flags return both).
        if let Some(feature) = &flag.feature_flag {
            if !feature.is_valid_value(&value) {
                errors.push(attribute_error(format!(
                    "error while parsing user-defined configuration values: '{value}' is not a valid value for '{specified}'"
                )));
                continue;
            }
            if let Some(error) = feature.error.as_ref().filter(|error| !error.is_empty()) {
                deferred.push(error.clone());
                continue;
            }
            let want = feature.flag_value().unwrap_or_default();
            if want != value {
                diffs.push(Diff {
                    what: flag.specified.clone(),
                    got: value.clone(),
                    want: want.to_owned(),
                });
            }
            continue;
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
        // A transition's raw StarlarkList/Tuple is a java.util.List too.
        if let Some(actual_values) = configured.string_items() {
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
    // :781-790: deferred errors make the match InError; attribute errors
    // fail the config_setting (crate::config_setting::analyze).
    let result = if !deferred.is_empty() {
        MatchResult::InError(deferred)
    } else if diffs.is_empty() {
        MatchResult::Match
    } else {
        MatchResult::NoMatch(diffs)
    };
    (result, specified_values.into_iter().collect())
}

/// ConfigSetting.checkOptionValue (ConfigSetting.java:420-482) of the
/// config_setting `label`. `Err` is the attribute error message (after the
/// rule-context prefix); rule warnings are appended to `warnings`.
fn check_option_value(
    label: &Label,
    configuration: &Configuration,
    name: &str,
    expected: &str,
    warnings: &mut Vec<String>,
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
    // :440-447: a main-repository config_setting on `cpu`, `host_cpu` or
    // `crosstool_top` (the name as written) warns before its value is
    // converted (RuleContext.ruleWarning at the rule's location); external
    // repositories' (@protobuf's) never do.
    if DEPRECATED_PRE_PLATFORMS_FLAGS.contains(&name) && label.repository.is_empty() {
        warnings.push(format!(
            "in config_setting rule {}: select() on {name} is deprecated. Use platform constraints instead: https://bazel.build/docs/configurable-attributes#platforms.",
            crate::rule_declarations::canonical_form(label)
        ));
    }
    let canonical = configuration::canonical_name(name);
    if configuration::fragment_option(canonical).is_none() {
        return Err(format!("{PARSE_ERROR_MESSAGE}unknown option: '{name}'"));
    }
    let Some(option) = configuration::option(canonical) else {
        // A real Bazel option whose value the native profile does not model
        // (including test options, which trimming may hide, :452-461):
        // never guess a match.
        return Err(format!(
            "select() on build option '{name}' is not implemented in the native profile (modelled options: {})",
            configuration::modelled_option_names()
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
            &[],
            None,
            &|_| unreachable!("no labels"),
            &mut Vec::new(),
        )
        .unwrap()
        .result
    }

    /// N28b: the JavaOptions version strings match exactly (no case
    /// folding), "" included.
    #[test]
    fn values_match_java_versions() {
        let matched = MatchResult::Match;
        for (name, default) in [
            ("java_runtime_version", "local_jdk"),
            ("java_language_version", ""),
            ("tool_java_runtime_version", "remotejdk_11"),
            ("tool_java_language_version", ""),
        ] {
            assert_eq!(result(&[(name, default)], &[], &[]), matched, "{name}");
            let set = format!("--{name}=17");
            assert_eq!(result(&[(name, "17")], &[], &[&set]), matched, "{name}");
            assert_eq!(
                result(&[(name, "17")], &[], &[]),
                MatchResult::NoMatch(vec![Diff {
                    what: option_label(name),
                    got: default.into(),
                    want: "17".into(),
                }])
            );
        }
        assert!(matches!(
            result(
                &[("java_runtime_version", "RemoteJDK_11")],
                &[],
                &["--java_runtime_version=remotejdk_11"]
            ),
            MatchResult::NoMatch(_)
        ));
    }

    /// The table-driven options (crate::configuration::VALUE_OPTIONS): the
    /// expected value converts like the command line (BooleanConverter, the
    /// case-insensitive StrictDepsMode EnumConverter, PlatformTypeConverter's
    /// lower case), diffs show the Java `toString` (`OFF`, `ERROR`), and
    /// conversion errors are attribute errors (paired captures
    /// tests/fixtures/value_options).
    #[test]
    fn values_match_table_driven_options() {
        let matched = MatchResult::Match;
        for (values, args) in [
            (
                &[
                    ("strict_proto_deps", "error"),
                    ("strict_public_imports", "OFF"),
                ][..],
                &[][..],
            ),
            (
                &[("strict_proto_deps", "Off")][..],
                &["--strict_proto_deps=off"][..],
            ),
            (
                &[("strict_proto_deps", "default")][..],
                &["--strict_proto_deps=DEFAULT"][..],
            ),
            (
                &[(
                    "experimental_proto_descriptor_sets_include_source_info",
                    "0",
                )][..],
                &[][..],
            ),
            (
                &[(
                    "experimental_proto_descriptor_sets_include_source_info",
                    "YES",
                )][..],
                &["--experimental_proto_descriptor_sets_include_source_info"][..],
            ),
            (
                &[
                    ("collect_code_coverage", "false"),
                    ("collect_code_coverage", "n"),
                ][..],
                &[][..],
            ),
            (&[("apple_platform_type", "MacOS")][..], &[][..]),
        ] {
            assert_eq!(result(values, &[], args), matched, "{values:?} {args:?}");
        }
        assert_eq!(
            result(&[("strict_proto_deps", "off")], &[], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("strict_proto_deps"),
                got: "ERROR".into(),
                want: "OFF".into(),
            }])
        );
        assert_eq!(
            result(&[("apple_platform_type", "")], &[], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("apple_platform_type"),
                got: "macos".into(),
                want: String::new(),
            }])
        );
        assert!(matches!(
            result(&[("collect_code_coverage", "true")], &[], &[]),
            MatchResult::NoMatch(_)
        ));
        // A transitioned configuration's collect_code_coverage (N47).
        let coverage = Configuration::new(
            configuration::NativeOptions {
                collect_code_coverage: true,
                ..Default::default()
            },
            BTreeMap::new(),
        );
        let label = Label::parse_canonical("//c:x").unwrap();
        let in_coverage = |values: &[(&str, &str)]| {
            analyze(
                &label,
                &target(values, &[]),
                &coverage,
                &[],
                &[],
                None,
                &|_| unreachable!("no labels"),
                &mut Vec::new(),
            )
        };
        assert_eq!(
            in_coverage(&[("collect_code_coverage", "1")])
                .unwrap()
                .result,
            matched
        );
        for (values, message) in [
            (
                &[("strict_proto_deps", "loose")][..],
                "While parsing option --strict_proto_deps=loose: Not a valid strict dependency checking level: 'loose' (should be off, warn, error, strict or default)",
            ),
            (
                &[(
                    "experimental_proto_descriptor_sets_include_source_info",
                    "maybe",
                )][..],
                "While parsing option --experimental_proto_descriptor_sets_include_source_info=maybe: 'maybe' is not a boolean",
            ),
            (
                &[("collect_code_coverage", "sometimes")][..],
                "While parsing option --collect_code_coverage=sometimes: 'sometimes' is not a boolean",
            ),
        ] {
            assert_eq!(
                in_coverage(values).unwrap_err(),
                format!(
                    "in values attribute of config_setting rule //c:x: error while parsing configuration settings: {message}"
                )
            );
        }
        // CoreOptions.enableRunfiles: TriStateConverter over the configured
        // TriState, AUTO unless set (tests/fixtures/enable_runfiles_option).
        assert_eq!(
            result(&[("enable_runfiles", "true")], &[], &[]),
            MatchResult::NoMatch(vec![Diff {
                what: option_label("enable_runfiles"),
                got: "AUTO".into(),
                want: "YES".into(),
            }])
        );
        for (values, args) in [
            (&[("enable_runfiles", "Auto")][..], &[][..]),
            (&[("enable_runfiles", "1")][..], &["--enable_runfiles"][..]),
            (
                &[("enable_runfiles", "n")][..],
                &["--noenable_runfiles"][..],
            ),
            (
                &[("enable_runfiles", "auto")][..],
                &["--enable_runfiles=no", "--enable_runfiles=AUTO"][..],
            ),
        ] {
            assert_eq!(result(values, &[], args), matched, "{values:?} {args:?}");
        }
        assert_eq!(
            in_coverage(&[("enable_runfiles", "maybe")]).unwrap_err(),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --enable_runfiles=maybe: 'maybe' is not a boolean"
        );
        // Real options outside both tables keep the explicit error.
        assert!(
            in_coverage(&[("build_runfile_links", "true")])
                .unwrap_err()
                .contains("select() on build option 'build_runfile_links' is not implemented in the native profile")
        );
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
                &[],
                None,
                &|_| unreachable!("no labels"),
                &mut Vec::new(),
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
        // A real option the native profile does not model is never guessed.
        assert!(
            error(&[("copt", "-O2")]).starts_with(
                "in values attribute of config_setting rule //c:x: select() on build option 'copt' is not implemented in the native profile (modelled options: compilation_mode, "
            )
        );
        // Label conversion errors of `crosstool_top` (Label.parseCanonical
        // texts; paired captures crosstool_bad/crosstool_empty).
        assert_eq!(
            error(&[("crosstool_top", "a:b:c")]),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --crosstool_top=a:b:c: invalid target name 'b:c': target names may not contain ':'"
        );
        assert_eq!(
            error(&[("crosstool_top", "")]),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --crosstool_top=: invalid target name '': empty target name"
        );
    }

    /// The deprecated pre-platforms flags (ConfigSetting.java:95-97,
    /// 440-447): matched through their converters (AutoCpuConverter; the
    /// LabelConverter with a null conversion context, Label.parseCanonical),
    /// with a rule warning for main-repository config_settings only (paired
    /// captures: tests/fixtures/pre_platforms_flags).
    #[test]
    fn pre_platforms_flags_match_and_warn_in_the_main_repository() {
        let run = |label: &str, values: &[(&str, &str)], args: &[&str]| {
            let mut warnings = Vec::new();
            let result = analyze(
                &Label::parse_canonical(label).unwrap(),
                &target(values, &[]),
                &configuration(args),
                &[],
                &[],
                None,
                &|_| unreachable!("no labels"),
                &mut warnings,
            )
            .map(|matching| matching.result);
            (result, warnings)
        };
        let warning = |label: &str, name: &str| {
            format!(
                "in config_setting rule {label}: select() on {name} is deprecated. Use platform constraints instead: https://bazel.build/docs/configurable-attributes#platforms."
            )
        };
        let host = configuration::host_cpu();
        // The defaults: the host CPU ("" converts to it) and
        // @bazel_tools//tools/cpp:toolchain (`@bazel_tools` is canonical).
        for values in [
            &[("cpu", host)][..],
            &[("cpu", "")][..],
            &[("host_cpu", host)][..],
            &[("host_cpu", "")][..],
            &[("crosstool_top", "@bazel_tools//tools/cpp:toolchain")][..],
            &[("crosstool_top", "@@bazel_tools//tools/cpp:toolchain")][..],
        ] {
            let (result, warnings) = run("//c:x", values, &[]);
            assert_eq!(result, Ok(MatchResult::Match), "{values:?}");
            assert_eq!(warnings, [warning("//c:x", values[0].0)], "{values:?}");
        }
        // Command-line values; diffs want the expected value and got the
        // configured one.
        assert_eq!(
            run("//c:x", &[("cpu", "arm")], &["--cpu=arm"]).0,
            Ok(MatchResult::Match)
        );
        assert_eq!(
            run("//c:x", &[("cpu", host)], &["--cpu=arm"]).0,
            Ok(MatchResult::NoMatch(vec![Diff {
                what: option_label("cpu"),
                got: "arm".into(),
                want: host.into(),
            }]))
        );
        assert_eq!(
            run("//c:x", &[("host_cpu", "ppc")], &["--host_cpu", "ppc"]).0,
            Ok(MatchResult::Match)
        );
        assert_eq!(
            run("//c:x", &[("cpu", "ppc")], &["--host_cpu", "ppc"]).0,
            Ok(MatchResult::NoMatch(vec![Diff {
                what: option_label("cpu"),
                got: host.into(),
                want: "ppc".into(),
            }]))
        );
        // Expected labels get `//` unless they start with `/` or `@` and are
        // parsed canonically: relative to neither the config_setting's
        // package nor its repository's mapping.
        let custom = ["--crosstool_top=//pkg:t"];
        for (expected, matched) in [
            ("//pkg:t", true),
            ("pkg:t", true),
            (":t", false),
            ("@@//pkg:t", true),
            ("@//pkg:t", true),
            ("@bazel_tools//tools/cpp:toolchain", false),
        ] {
            let (result, _) = run("//c:x", &[("crosstool_top", expected)], &custom);
            assert_eq!(
                matches!(result, Ok(MatchResult::Match)),
                matched,
                "{expected}: {result:?}"
            );
        }
        assert_eq!(
            run(
                "//c:x",
                &[("crosstool_top", "//external:android/crosstool")],
                &[]
            )
            .0,
            Ok(MatchResult::NoMatch(vec![Diff {
                what: option_label("crosstool_top"),
                got: "@@bazel_tools//tools/cpp:toolchain".into(),
                want: "//external:android/crosstool".into(),
            }]))
        );
        let ext = ["--crosstool_top=@@ext+//:t"];
        assert!(matches!(
            run("//c:x", &[("crosstool_top", "@ext//:t")], &ext).0,
            Ok(MatchResult::NoMatch(_))
        ));
        assert_eq!(
            run("//c:x", &[("crosstool_top", "@@ext+//:t")], &ext).0,
            Ok(MatchResult::Match)
        );
        // External repositories' config_settings (like @protobuf's) never
        // warn; several flags warn in `values` order, before any error.
        let (result, warnings) = run(
            "@@protobuf+//build_defs:x",
            &[
                ("crosstool_top", "//external:android/crosstool"),
                ("cpu", "win32"),
            ],
            &[],
        );
        assert!(matches!(result, Ok(MatchResult::NoMatch(_))));
        assert!(warnings.is_empty());
        let (result, warnings) = run(
            "//c:x",
            &[
                ("compilation_mode", "fast"),
                ("cpu", host),
                ("crosstool_top", "a:b:c"),
                ("host_cpu", host),
            ],
            &[],
        );
        assert_eq!(
            result.unwrap_err(),
            "in values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --compilation_mode=fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)\nin values attribute of config_setting rule //c:x: error while parsing configuration settings: While parsing option --crosstool_top=a:b:c: invalid target name 'b:c': target names may not contain ':'"
        );
        assert_eq!(
            warnings,
            [
                warning("//c:x", "cpu"),
                warning("//c:x", "crosstool_top"),
                warning("//c:x", "host_cpu")
            ]
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
                scope: Default::default(),
            }),
            feature_flag: None,
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
        analyze(
            &label,
            &target,
            configuration,
            flags,
            &[],
            None,
            &|text| {
                let text = text.strip_prefix(':').unwrap_or(text);
                if text.starts_with("//") || text.starts_with("@@") {
                    Label::parse_canonical(text)
                } else if text.contains(':') {
                    Err("bad label".into())
                } else {
                    Label::parse_canonical(&format!("//c:{text}"))
                }
            },
            &mut Vec::new(),
        )
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
    fn constraint_values_match_the_target_platform() {
        use crate::platforms::{
            ConstraintSettingInfo, ConstraintValueInfo, PlatformAttributes, create_platform,
        };
        use std::sync::Arc;
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        let setting = |text: &str, refined: Option<ConstraintValueInfo>| {
            Arc::new(ConstraintSettingInfo {
                label: label(text),
                default_value: None,
                refined: refined.map(Box::new),
            })
        };
        let os = setting("//os:os", None);
        let cpu = setting("//cpu:cpu", None);
        let value = |setting: &Arc<ConstraintSettingInfo>, text: &str| ConstraintValueInfo {
            setting: setting.clone(),
            label: label(text),
        };
        let linux = value(&os, "//os:linux");
        let distro = setting("//os:distro", Some(linux.clone()));
        let platform = create_platform(
            &label("//p:linux"),
            PlatformAttributes {
                constraint_values: vec![linux.clone()],
                ..Default::default()
            },
            &|label| crate::rule_declarations::canonical_form(label),
        )
        .unwrap();
        let mut target = target(&[], &[]);
        target.attrs.insert(
            "constraint_values".into(),
            AttrValue::Labels(vec!["//os:linux".into()]),
        );
        let prerequisite = |value: &ConstraintValueInfo| ConstraintPrerequisite {
            actual: value.label.clone(),
            value: Some(value.clone()),
        };
        let run = |constraints: &[ConstraintPrerequisite]| {
            analyze(
                &label("//c:x"),
                &target,
                &Configuration::default(),
                &[],
                constraints,
                Some(&platform),
                &|_| unreachable!("no labels"),
                &mut Vec::new(),
            )
        };
        let matched = run(&[prerequisite(&linux)]).unwrap();
        assert_eq!(matched.result, MatchResult::Match);
        assert_eq!(matched.constraints, BTreeSet::from([label("//os:linux")]));
        // Diffs: want the expected value's name, got the platform's or <unset>
        // (ConfigSetting.java:340-354).
        assert_eq!(
            run(&[
                prerequisite(&value(&os, "//os:osx")),
                prerequisite(&value(&cpu, "//cpu:arm64"))
            ])
            .unwrap()
            .result,
            MatchResult::NoMatch(vec![
                Diff {
                    what: label("//os:os"),
                    got: "linux".into(),
                    want: "osx".into()
                },
                Diff {
                    what: label("//cpu:cpu"),
                    got: "<unset>".into(),
                    want: "arm64".into()
                }
            ])
        );
        // A refining value specifies its refinement chain too.
        assert_eq!(
            run(&[prerequisite(&value(&distro, "//os:debian"))])
                .unwrap()
                .constraints,
            BTreeSet::from([label("//os:debian"), label("//os:linux")])
        );
        assert_eq!(
            run(&[
                ConstraintPrerequisite {
                    actual: label("//c:fg"),
                    value: None
                },
                prerequisite(&linux),
                ConstraintPrerequisite {
                    actual: label("@@r+//c:other"),
                    value: None
                },
            ])
            .unwrap_err(),
            "in constraint_values attribute of config_setting rule //c:x: //c:fg is not a constraint_value\nin constraint_values attribute of config_setting rule //c:x: @@r+//c:other is not a constraint_value"
        );
        assert_eq!(
            run(&[prerequisite(&linux), prerequisite(&value(&os, "//os:osx"))]).unwrap_err(),
            "in config_setting rule //c:x: Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx]"
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
            &[],
            None,
            &|_| unreachable!("no labels"),
            &mut Vec::new(),
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
