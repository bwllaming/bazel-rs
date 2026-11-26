//! Platform-based flags (B84): `platform(flags = [...])` sets its flags on
//! every configuration whose target platform it is.
//!
//! Bazel 9.3: PlatformFunction parses a platform's PlatformInfo.flags
//! relative to the platform's package, with default values included
//! (analysis/platform/PlatformFunction.java:102-125, ParsedFlagsFunction),
//! and BuildConfigurationKeyProducer merges them over the options of each
//! configuration it creates whose `--platforms` names that platform
//! (analysis/producers/BuildConfigurationKeyProducer.java:131-160,
//! ParsedFlagsValue.mergeWith): a native option takes the parsed value (an
//! allowMultiple option's list replaces the configuration's), a Starlark
//! flag is set, or removed when the value is its default. The native
//! profile applies them where a configuration is created from another
//! (crate::engine's transition and exec transition nodes); see
//! docs/PLATFORMS.md.

use std::collections::BTreeMap;

use crate::configuration::{Configuration, LabelParser, OptionValue, STARLARK_SKIPPED_PREFIXES};
use crate::label::Label;
use crate::starlark_options::LoadedTarget;

/// `configuration` with the platform `flags` merged over it. `loader`
/// loads a Starlark flag's target and `labels` parses labels, both relative
/// to the platform's package. Errors are the flag's parsing error.
pub(crate) fn apply(
    configuration: &Configuration,
    flags: &[String],
    loader: &mut dyn FnMut(&str) -> Result<LoadedTarget, String>,
    labels: &LabelParser<'_>,
) -> Result<Configuration, String> {
    // ParsedFlagsFunction.compute (:60-95): `--flag_alias` names become
    // their Starlark flag, then the Starlark-prefixed flags parse apart.
    let mut native_flags = Vec::new();
    let mut starlark_flags = Vec::new();
    for flag in flags {
        let Some(rest) = flag.strip_prefix("--") else {
            native_flags.push(flag.clone());
            continue;
        };
        let (name, value) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (rest, None),
        };
        let (bare, negated) = match (value, name.strip_prefix("no")) {
            (None, Some(bare)) => (bare, true),
            _ => (name, false),
        };
        let flag = match configuration.native.flag_aliases.get(bare) {
            Some(actual) => format!(
                "--{}{}{}",
                if negated { "no" } else { "" },
                crate::rule_declarations::canonical_form(actual),
                value.map(|value| format!("={value}")).unwrap_or_default()
            ),
            None => flag.clone(),
        };
        if STARLARK_SKIPPED_PREFIXES
            .iter()
            .any(|prefix| flag.starts_with(prefix))
        {
            starlark_flags.push(flag);
        } else {
            native_flags.push(flag);
        }
    }
    let mut native = configuration.native.clone();
    // The native options, grouped by option in first-occurrence order.
    let mut options: Vec<(&'static str, Vec<String>, Option<bool>)> = Vec::new();
    for flag in &native_flags {
        let Some(rest) = flag.strip_prefix("--") else {
            return Err(format!(
                "platform flag {flag}: only `--name[=value]` flags are implemented in the native profile"
            ));
        };
        let (name, value) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (rest, None),
        };
        let (option, enabled) = match (lookup(name), value) {
            (Some(option), Some(_)) => (option, None),
            (Some(option), None) if option.1 => (option, Some(true)),
            (Some(_), None) => return Err(format!("Expected value after {flag}")),
            (None, None) => match name.strip_prefix("no").and_then(lookup) {
                Some(option) if option.1 => (option, Some(false)),
                Some(_) => {
                    return Err(format!(
                        "Illegal use of 'no' prefix on non-boolean option: {flag}"
                    ));
                }
                None => return Err(unknown(flag, name)),
            },
            (None, Some(_)) => return Err(unknown(flag, name)),
        };
        let (option, _, non_configurable) = option;
        if non_configurable {
            return Err(format!(
                "platform flag {flag}: option '{option}' is not part of the configuration"
            ));
        }
        match options.iter_mut().find(|(seen, _, _)| *seen == option) {
            Some((_, values, last)) => {
                values.extend(value.map(str::to_owned));
                *last = enabled;
            }
            None => options.push((
                option,
                value.map(str::to_owned).into_iter().collect(),
                enabled,
            )),
        }
    }
    for (name, values, enabled) in &options {
        if *name == "define" {
            let option = crate::configuration::option(name).expect("the define option");
            let mut entries = Vec::new();
            for value in values {
                match crate::configuration::convert(option, value, labels)
                    .map_err(|error| format!("While parsing option --define={value}: {error}"))?
                {
                    OptionValue::Assignments(assignments) => entries.extend(assignments),
                    _ => unreachable!("the define converter"),
                }
            }
            native.define = crate::configuration::normalize_defines(entries);
            continue;
        }
        crate::starlark_transitions::set_native_from_flags(
            &mut native,
            name,
            values,
            *enabled,
            labels,
        )
        .map_err(|error| format!("platform flag --{name}: {error}"))?;
    }
    let mut starlark: BTreeMap<Label, _> = configuration.starlark.clone();
    if !starlark_flags.is_empty() {
        for (label, value, default) in
            crate::starlark_options::parse_with_defaults(&starlark_flags, loader, labels)?
        {
            if default {
                starlark.remove(&label);
            } else {
                starlark.insert(label, value);
            }
        }
    }
    Ok(Configuration::new(native, starlark))
}

/// A modelled native option by name or old name: its name, whether it
/// uses boolean syntax and whether it is NON_CONFIGURABLE.
fn lookup(name: &str) -> Option<(&'static str, bool, bool)> {
    let name = crate::configuration::canonical_name(name);
    if let Some(option) = crate::configuration::option(name) {
        return Some((
            option.name,
            option.kind.boolean_syntax(),
            option.non_configurable,
        ));
    }
    crate::configuration::FLAG_LIST_OPTIONS
        .iter()
        .find(|(option, _)| *option == name)
        .map(|(option, _)| (*option, false, false))
}

/// OptionsParserImpl's unrecognized option: an option of Bazel the native
/// profile does not model is an explicit error instead.
fn unknown(flag: &str, name: &str) -> String {
    let bare = name.strip_prefix("no").unwrap_or(name);
    if crate::configuration::fragment_option(crate::configuration::canonical_name(name)).is_some()
        || crate::configuration::fragment_option(bare).is_some()
    {
        format!("platform flag {flag}: this option is not implemented in the native profile")
    } else {
        format!("Unrecognized option: {flag}")
    }
}
