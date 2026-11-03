//! Options spelled at their Bazel 9.3.1 default (docs/RC_OPTIONS.md
//! "Options at their default"). Explicitly passing an option's default value
//! is the same invocation as not passing it in genuine Bazel (an option
//! value equal to its default is the default configuration, the same
//! execution and output), so an option the native profile does not model
//! is accepted when its value is the default `help <command> --long`
//! documents (crate::help_topics, genuine 9.3.1; for an undocumented option
//! the source's `defaultValue`, crate::option_source_defaults) and fails
//! explicitly otherwise. Not covered (they keep the explicit unsupported error):
//!
//! * repeatable options (any occurrence adds a value) and options whose
//!   default is computed (`default: see description`) or absent;
//! * expansion options and options without a value converter (`Void`);
//! * options whose mere presence has an effect: `deprecationWarning`
//!   options (OptionsParserImpl warns when they are set) and
//!   `implicitRequirements` ones (`--check_tests_up_to_date`,
//!   `--fdo_instrument`, `--cs_fdo_instrument`), and the client-supplied
//!   options the Bazel client passes itself (`--isatty`, `--client_env`, …).

use crate::option_metadata::{COMMANDS, O, OPTIONS};

/// Options whose explicit presence warns or implies other options
/// (`deprecationWarning`/`implicitRequirements` in the 9.3.1 sources), or
/// that the client supplies itself.
const PRESENCE_MATTERS: &[&str] = &[
    "android_cpu",
    "android_crosstool_top",
    "android_grte_top",
    "android_sdk",
    "announce",
    "bes_best_effort",
    "build_python_zip",
    "check_fileset_dependencies_recursively",
    "check_tests_up_to_date",
    "cs_fdo_instrument",
    "experimental_skyframe_native_filesets",
    "experimental_skyframe_prepare_analysis",
    "experimental_spawn_scheduler",
    "fat_apk_cpu",
    "fdo_instrument",
    "incompatible_enable_android_toolchain_resolution",
    "incompatible_override_toolchain_transition",
    "order_results",
    "propeller_optimize_absolute_cc_profile",
    "propeller_optimize_absolute_ld_profile",
    "unconditional_warning",
    "use_target_platform_for_tests",
    // Supplied by the client (src/main/cpp/blaze.cc).
    "client_cwd",
    "client_env",
    "emacs",
    "invocation_id",
    "isatty",
    "build_request_id",
    "restart_reason",
    "startup_time",
    "extract_data_time",
    "command_wait_time",
    "rc_source",
    "terminal_columns",
    "ignore_client_env",
    "default_override",
    "tool_tag",
];

/// The genuine default of `name` for the command at `bit`: as `help
/// <command> --long` prints it (`default: "VALUE"`), else (an undocumented
/// option) the source's string-literal `defaultValue`
/// (crate::option_source_defaults).
fn default_for(name: &str, bit: usize) -> Option<String> {
    help_default(name, bit).or_else(|| {
        crate::option_source_defaults::SOURCE_DEFAULTS
            .iter()
            .find(|(option, _)| *option == name)
            .map(|(_, value)| (*value).to_owned())
    })
}

/// `help <command> --long`'s default of `name` (a documented option).
fn help_default(name: &str, bit: usize) -> Option<String> {
    crate::help_topics::OPTION_LINES
        .iter()
        .filter(|(_, _, mask)| mask & (1 << bit) != 0)
        .find_map(|(line, _, _)| {
            let line = line.strip_prefix("[no]").unwrap_or(line);
            let (given, rest) = line.split_once(' ')?;
            if given != name {
                return None;
            }
            let start = rest.rfind("default: \"")? + "default: \"".len();
            let value = rest[start..].strip_suffix("\")")?;
            Some(value.to_owned())
        })
}

/// The option `name` registered for the command at `bit` (its old name
/// resolved, crate::command_options).
fn find(name: &str, bit: usize) -> Option<&'static O> {
    let canonical = crate::command_options::COMMAND_OPTIONS
        .iter()
        .find(|(_, old, ..)| !old.is_empty() && *old == name)
        .map_or(name, |(new, ..)| *new);
    OPTIONS
        .iter()
        .find(|option| option.name == canonical && option.commands & (1 << bit) != 0)
}

/// BooleanConverter.
fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Some(true),
        "false" | "0" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

/// Whether `value` converts to the same value as `default` for `option`.
fn same_value(option: &O, value: &str, default: &str) -> bool {
    match option.converter {
        "Boolean" => boolean(value).is_some() && boolean(value) == boolean(default),
        "TriState" => {
            let tri = |text: &str| match text.to_ascii_lowercase().as_str() {
                "auto" => Some(0),
                "yes" | "true" | "1" => Some(1),
                "no" | "false" | "0" => Some(2),
                _ => None,
            };
            tri(value).is_some() && tri(value) == tri(default)
        }
        "Integer" | "Long" | "NonNegativeInteger" | "Percentage" => {
            let number = |text: &str| text.parse::<i64>().ok();
            number(value).is_some() && number(value) == number(default)
        }
        "Double" => {
            let number = |text: &str| text.parse::<f64>().ok();
            number(value).is_some() && number(value) == number(default)
        }
        // Enum converters (EnumConverter matches case-insensitively).
        "" => value.eq_ignore_ascii_case(default),
        _ => value == default,
    }
}

/// An option `command` registers that this profile does not otherwise
/// model: `Some(Ok(used_next))` at its default, the explicit unsupported
/// error otherwise; `None` when this module does not cover it.
pub fn default_option(
    command: &str,
    arg: &str,
    next: Option<&str>,
    origin: &str,
) -> Option<Result<bool, String>> {
    let bit = COMMANDS
        .iter()
        .position(|candidate| *candidate == command)?;
    let rest = arg.strip_prefix("--")?;
    let (name, value) = match rest.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (rest, None),
    };
    let (option, negated) = match find(name, bit) {
        Some(option) => (option, false),
        None => (
            name.strip_prefix("no")
                .and_then(|name| find(name, bit))
                .filter(|option| option.boolean)?,
            true,
        ),
    };
    if option.multiple
        || !option.expansion.is_empty()
        || matches!(option.converter, "Void")
        || PRESENCE_MATTERS.contains(&option.name)
    {
        return None;
    }
    let default = default_for(option.name, bit)?;
    let (given, used_next) = if negated {
        if value.is_some() {
            return Some(Err(format!(
                "{arg} :: Unexpected value after boolean option: {arg}"
            )));
        }
        ("false".to_owned(), false)
    } else if let Some(value) = value {
        (value.to_owned(), false)
    } else if option.boolean {
        ("true".to_owned(), false)
    } else {
        match next {
            Some(next) => (next.to_owned(), true),
            None => return Some(Err(format!("Expected value after {arg}"))),
        }
    };
    if same_value(option, &given, &default) {
        return Some(Ok(used_next));
    }
    let shown = if used_next {
        format!("{arg} {given}")
    } else {
        arg.to_owned()
    };
    // The unmodelled option's own message (crate main's unmodelled_option):
    // only the default is modelled (docs/RC_OPTIONS.md).
    let _ = default;
    Some(Err(format!(
        "unsupported option {shown:?}{origin}; options are never silently ignored"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_accepted_and_other_values_fail_explicitly() {
        // A boolean default spelled both ways.
        assert_eq!(
            default_option(
                "build",
                "--noexperimental_remote_mark_tool_inputs",
                None,
                ""
            ),
            Some(Ok(false))
        );
        assert_eq!(
            default_option(
                "build",
                "--experimental_remote_mark_tool_inputs=false",
                None,
                ""
            ),
            Some(Ok(false))
        );
        let error = default_option("build", "--experimental_remote_mark_tool_inputs", None, "")
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error,
            "unsupported option \"--experimental_remote_mark_tool_inputs\"; options are never silently ignored"
        );
        // An integer default with a separate value.
        assert_eq!(
            default_option("build", "--progress_report_interval", Some("0"), ""),
            Some(Ok(true))
        );
        // An undocumented option's source default.
        assert_eq!(
            default_option("build", "--analyze", None, ""),
            Some(Ok(false))
        );
        // Presence matters / repeatable / unregistered: not covered.
        assert_eq!(
            default_option("test", "--nocheck_tests_up_to_date", None, ""),
            None
        );
        assert_eq!(default_option("build", "--copt=-O2", None, ""), None);
        assert_eq!(
            default_option("query", "--progress_report_interval=0", None, ""),
            None
        );
        // Old names resolve (experimental_ui_attempt_to_print_relative_paths).
        assert_eq!(
            default_option(
                "build",
                "--noexperimental_ui_attempt_to_print_relative_paths",
                None,
                ""
            ),
            Some(Ok(false))
        );
    }
}
