//! `help` (runtime/commands/HelpCommand.java): the index, `help COMMAND`
//! (the command's help template, then its options grouped by documentation
//! category in OptionsUsage's medium form, BlazeCommandUtils.expandHelpTopic),
//! `help startup_options`, `help target-syntax` and `help info-keys`. The
//! texts and option tables are generated from genuine Bazel 9.3.0
//! (src/help_topics.rs, src/option_metadata.rs); `help native` prints the
//! native profile's own notes. Not byte parity: the release header names
//! bazel-rs, `--long` prints no option descriptions (not kept).
use crate::help_topics::{OPTION_LINES, PAGES, TEMPLATES};
use crate::option_metadata::COMMANDS;

/// OptionFilterDescriptions.documentationOrder with the category texts.
const CATEGORIES: &[(&str, &str)] = &[
    (
        "BAZEL_CLIENT_OPTIONS",
        "Options that appear before the command and are parsed by the client",
    ),
    ("EXECUTION_STRATEGY", "Options that control build execution"),
    (
        "TOOLCHAIN",
        "Options that configure the toolchain used for action execution",
    ),
    (
        "OUTPUT_SELECTION",
        "Options that control the output of the command",
    ),
    (
        "OUTPUT_PARAMETERS",
        "Options that let the user configure the intended output, affecting its value, as opposed to its existence",
    ),
    (
        "INPUT_STRICTNESS",
        "Options that affect how strictly Bazel enforces valid build inputs (rule definitions,  flag combinations, etc.)",
    ),
    (
        "SIGNING",
        "Options that affect the signing outputs of a build",
    ),
    (
        "STARLARK_SEMANTICS",
        "This option affects semantics of the Starlark language or the build API accessible to BUILD files, .bzl files, or WORKSPACE files.",
    ),
    (
        "TESTING",
        "Options that govern the behavior of the test environment or test runner",
    ),
    ("QUERY", "Options relating to query output and semantics"),
    (
        "MOD_COMMAND",
        "Options relating to the output and semantics of the `mod` subcommand",
    ),
    ("BZLMOD", "Options relating to Bzlmod output and semantics"),
    (
        "BUILD_TIME_OPTIMIZATION",
        "Options that trigger optimizations of the build time",
    ),
    (
        "LOGGING",
        "Options that affect the verbosity, format or location of logging",
    ),
    (
        "GENERIC_INPUTS",
        "Options specifying or altering a generic input to a Bazel command that does not fall into other categories.",
    ),
    ("REMOTE", "Remote caching and execution options"),
    (
        "UNCATEGORIZED",
        "Miscellaneous options, not otherwise categorized.",
    ),
];

/// OptionsParser.HelpVerbosity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Short,
    Medium,
    Long,
}

/// The release header (`%80s` of `[product release]`).
fn header() -> String {
    format!(
        "{:>80}\n",
        format!(
            "[bazel-rs {}, Bazel {} compatible]",
            env!("CARGO_PKG_VERSION"),
            crate::COMPATIBILITY_VERSION
        )
    )
}

/// OptionsParser.describeOptions for the options of `command`: per
/// documentation category (in display order) its options' usage lines
/// (OptionsUsage), by name.
fn describe_options(command: &str, verbosity: Verbosity) -> String {
    let Some(bit) = COMMANDS.iter().position(|name| *name == command) else {
        return String::new();
    };
    let mut sections = Vec::new();
    for (category, title) in CATEGORIES {
        let mut lines: Vec<&str> = OPTION_LINES
            .iter()
            .filter(|(_, owner, commands)| commands & (1 << bit) != 0 && owner == category)
            .map(|(line, ..)| *line)
            .collect();
        if lines.is_empty() {
            continue;
        }
        let name = |line: &str| line.split(' ').next().unwrap_or(line).replace("[no]", "");
        lines.sort_by_key(|line| name(line));
        let mut section = format!("{title}:");
        for line in lines {
            section.push_str("\n  --");
            match verbosity {
                Verbosity::Short => section.push_str(line.split(' ').next().unwrap_or(line)),
                _ => section.push_str(line),
            }
        }
        sections.push(section);
    }
    sections.join("\n\n")
}

/// `help [--short|--medium|--long] [TOPIC]`: the page, or the error
/// (HelpCommand's `'X' is not a known command`, exit 2).
pub fn render(args: &[String], native_notes: &str) -> Result<String, String> {
    let mut verbosity = Verbosity::Medium;
    let mut topic = None;
    for arg in args {
        match arg.as_str() {
            "--short" | "--help_verbosity=short" => verbosity = Verbosity::Short,
            "--medium" | "--help_verbosity=medium" => verbosity = Verbosity::Medium,
            "--long" | "--help_verbosity=long" => verbosity = Verbosity::Long,
            other if topic.is_none() && !other.starts_with("--") => topic = Some(other),
            other => return Err(format!("{other} :: Unrecognized option: {other}")),
        }
    }
    let footer = if verbosity == Verbosity::Medium {
        "\n\n(Use 'help --long' for full details or --short to just enumerate options.)\n"
    } else {
        "\n"
    };
    let page = |name: &str| {
        PAGES
            .iter()
            .find(|(topic, _)| *topic == name)
            .map(|(_, text)| *text)
            .unwrap_or_default()
    };
    match topic {
        None => Ok(format!(
            "{}{}\n\nNative profile:\n  bazel-rs help native\n                   The native engine's supported subset.\n",
            header(),
            page("")
        )),
        Some("native") => Ok(native_notes.to_owned()),
        // The page ends with its own footer; info-keys has no header.
        Some("target-syntax") => Ok(format!("{}{}\n", header(), page("target-syntax"))),
        Some("info-keys") => Ok(format!("{}\n", page("info-keys"))),
        Some(topic) => {
            let Some((_, template)) = TEMPLATES.iter().find(|(name, _)| *name == topic) else {
                return Err(format!("'{topic}' is not a known command"));
            };
            let command = if topic == "startup_options" {
                "startup"
            } else {
                topic
            };
            let options = describe_options(command, verbosity);
            let mut text = format!("{}{}", header(), template.trim_end());
            if !options.is_empty() {
                text.push_str("\n\n");
                text.push_str(&options);
            }
            text.push_str(footer);
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_listings_follow_options_usage() {
        let build = render(&["build".to_owned()], "").unwrap();
        assert!(build.contains("\n  --[no]keep_going [-k] (a boolean; default: \"false\")\n"));
        assert!(
            build
                .contains("\n  --define (a 'name=value' assignment; may be used multiple times)\n")
        );
        let module = render(&["--short".to_owned(), "mod".to_owned()], "").unwrap();
        assert!(module.contains("\n  --[no]include_unused\n"), "{module}");
        assert!(!module.contains("(Use 'help --long'"));
    }

    #[test]
    fn unknown_topics_fail() {
        assert_eq!(
            render(&["bogus".to_owned()], "").unwrap_err(),
            "'bogus' is not a known command"
        );
        assert!(
            render(&["mod".to_owned()], "")
                .unwrap()
                .contains("Usage: bazel mod")
        );
    }
}
