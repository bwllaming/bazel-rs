//! `dump` (runtime/commands/DumpCommand.java, docs/COMMANDS.md). bazel-rs
//! has no server: every in-memory section prints what a fresh genuine
//! Bazel 9.3.1 server (`--batch`) prints — no packages, no rules, an empty
//! Skyframe graph — and `--action_cache` lists the native action cache of
//! the cache directory in CompactPersistentActionCache.dump's layout.
//! `--rule_classes` is the genuine 9.3.1 rule class table
//! (resources/dump_rule_classes.txt, captured from `bazel dump
//! --rule_classes`). Only the command line is read (no rc files).
use std::path::{Path, PathBuf};

/// The genuine `dump` usage (BlazeCommandUtils.expandHelpTopic with the
/// DumpOptions, LONG verbosity), printed when no output is selected.
const HELP: &str = include_str!("../resources/dump_help.txt");
const RULE_CLASSES: &str = include_str!("../resources/dump_rule_classes.txt");
const WARNING: &str = "WARNING: This information is intended for consumption by developers only, and may change at any time. Script against it at your own risk!";
const SKYFRAME_MODES: &[&str] = &[
    "off",
    "summary",
    "count",
    "value",
    "deps",
    "rdeps",
    "function_graph",
    "active_directories",
    "active_directories_frontier_deps",
];

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    packages: bool,
    action_cache: bool,
    rule_classes: bool,
    rules: bool,
    skylark_memory: Option<String>,
    skyframe: String,
    /// `--memory`: whether it names a subject (`<flags>:<type>:<node>`).
    memory: Option<bool>,
}

/// Converter.Contextless for booleans (BooleanConverter).
fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Some(true),
        "false" | "0" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

/// DumpCommand.MemoryModeConverter.convert: whether a subject is named.
fn memory_mode(input: &str) -> Result<bool, String> {
    let items: Vec<&str> = input.splitn(3, ':').collect();
    let (mut collection, mut display) = (None, None);
    for word in items[0].split(',') {
        if word.starts_with("needle=") {
            continue;
        }
        match word {
            "shallow" | "deep" | "transitive" | "full" => collection = Some(word),
            "summary" | "count" | "bytes" => display = Some(word),
            "notransient" | "noconfig" | "noprecomputed" | "noworkspacestatus" => {}
            _ => return Err(format!("Unrecognized word '{word}'")),
        }
    }
    let Some(collection) = collection else {
        return Err("No collection type specified".into());
    };
    if display.is_none() {
        return Err("No display mode specified".into());
    }
    if collection == "full" {
        return Ok(false);
    }
    if items.len() != 3 {
        return Err("Should be in the form: <flags>:<node type>:<node>".into());
    }
    match items[1].to_ascii_lowercase().as_str() {
        "starlark_module" | "package" | "configured_target" => Ok(true),
        _ => Err("Invalid subject type".into()),
    }
}

/// Whether `name` is an option of the `dump` command (the common
/// options), and whether it is a boolean.
fn dump_option(name: &str) -> Option<bool> {
    let bit = crate::option_metadata::COMMANDS
        .iter()
        .position(|command| *command == "dump")?;
    crate::option_metadata::OPTIONS
        .iter()
        .find(|option| option.name == name && option.commands & (1 << bit) != 0)
        .map(|option| option.boolean)
}

/// The command line (OptionsParser): the options, or the error and exit 2.
fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        skyframe: "off".into(),
        ..Options::default()
    };
    let mut residue = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        let Some(body) = arg.strip_prefix("--") else {
            residue.push(arg.clone());
            continue;
        };
        if body.is_empty() {
            residue.extend(args[index..].iter().cloned());
            break;
        }
        let (name, value) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (body, None),
        };
        let boolean_flag = |field: &mut bool, value: Option<String>| -> Result<(), String> {
            *field = match value {
                None => true,
                Some(value) => boolean(&value).ok_or_else(|| {
                    format!("While parsing option {arg}: '{value}' is not a boolean")
                })?,
            };
            Ok(())
        };
        match name {
            "packages" => boolean_flag(&mut options.packages, value)?,
            "action_cache" => boolean_flag(&mut options.action_cache, value)?,
            "rule_classes" => boolean_flag(&mut options.rule_classes, value)?,
            "rules" => boolean_flag(&mut options.rules, value)?,
            "nopackages" | "noaction_cache" | "norule_classes" | "norules" if value.is_none() => {
                match name {
                    "nopackages" => options.packages = false,
                    "noaction_cache" => options.action_cache = false,
                    "norule_classes" => options.rule_classes = false,
                    _ => options.rules = false,
                }
            }
            "skyframe" | "skylark_memory" | "memory" | "skykey_filter" => {
                let (shown, value) = match value {
                    Some(value) => (arg.clone(), value),
                    None if index < args.len() => {
                        index += 1;
                        (
                            format!("{arg}={}", args[index - 1]),
                            args[index - 1].clone(),
                        )
                    }
                    None => return Err(format!("Expected value after {arg}")),
                };
                match name {
                    "skyframe" => {
                        let mode = value.to_ascii_lowercase();
                        if !SKYFRAME_MODES.contains(&mode.as_str()) {
                            return Err(format!(
                                "While parsing option {shown}: Not a valid Skyframe Dump option: '{value}' (should be off, summary, count, value, deps, rdeps, function_graph, active_directories or active_directories_frontier_deps)"
                            ));
                        }
                        options.skyframe = mode;
                    }
                    "skylark_memory" => options.skylark_memory = Some(value),
                    "memory" => {
                        options.memory =
                            Some(memory_mode(&value).map_err(|error| {
                                format!("While parsing option {shown}: {error}")
                            })?);
                    }
                    _ => {}
                }
            }
            _ => {
                // The common options every command accepts (UI, rc and
                // client options): accepted and without effect here.
                let plain = name
                    .strip_prefix("no")
                    .filter(|plain| dump_option(plain) == Some(true));
                match (dump_option(name), plain) {
                    (Some(false), _) if value.is_none() && index < args.len() => index += 1,
                    (Some(_), _) | (None, Some(_)) => {}
                    (None, None) => {
                        return Err(format!("{arg} :: Unrecognized option: {arg}"));
                    }
                }
            }
        }
    }
    if !residue.is_empty() {
        return Err(format!("Unrecognized arguments: {}", residue.join(" ")));
    }
    Ok(options)
}

/// Howard Hinnant's civil_from_days: (year, month, day) of a day number.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The action cache's Timestamp (epoch minutes) as Instant.toString.
fn timestamp(seconds: i64) -> String {
    let minutes = seconds.div_euclid(60);
    let days = minutes.div_euclid(1440);
    let minute_of_day = minutes.rem_euclid(1440);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:00Z",
        minute_of_day / 60,
        minute_of_day % 60
    )
}

fn decode_hex(text: &str) -> Option<String> {
    let bytes = (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// CompactPersistentActionCache.dump over the native action cache
/// (`<cache>/actions/<key>/manifest.json`): per entry its primary output,
/// the manifest size as the packed length, its time (to the minute) and
/// the action key as the digest, ordered by time then key; then the
/// string indexer of the primary outputs.
fn action_cache(cache: Option<&Path>) -> String {
    let mut entries = Vec::new();
    if let Some(cache) = cache
        // snapshot-unrecorded: `dump` runs no analysis; it only lists the cache.
        && let Ok(listing) = std::fs::read_dir(cache.join("actions"))
    {
        for entry in listing.flatten() {
            let manifest = entry.path().join("manifest.json");
            // snapshot-unrecorded: `dump` runs no analysis; it only lists the cache.
            let Ok(bytes) = std::fs::read(&manifest) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(key) = json.get("key").and_then(|key| key.as_str()) else {
                continue;
            };
            let output = json
                .get("outputs")
                .and_then(|outputs| outputs.get(0))
                .and_then(|output| output.get("name"))
                .and_then(|name| name.as_str())
                .and_then(decode_hex)
                .unwrap_or_default();
            // snapshot-unrecorded: `dump` runs no analysis; it only lists the cache.
            let seconds = std::fs::metadata(&manifest)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs() as i64);
            entries.push((seconds, key.to_owned(), output, bytes.len()));
        }
    }
    entries.sort();
    let mut out = format!("Action cache ({} records):\n", entries.len());
    for (index, (seconds, key, output, packed)) in entries.iter().enumerate() {
        out.push_str(&format!("  {index} -> {output}\n"));
        out.push_str(&format!("  packed_len = {packed}\n"));
        out.push_str(&format!("  timestamp = {}\n", timestamp(*seconds)));
        out.push_str(&format!("  digest = {key}\n"));
    }
    out.push_str(&format!("String indexer ({} records):\n", entries.len()));
    for (index, (_, _, output, _)) in entries.iter().enumerate() {
        out.push_str(&format!("  {index} <=> {output}\n"));
    }
    out
}

/// The cache directory a build in the current directory uses.
fn cache_directory(explicit: Option<&str>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    match explicit {
        Some(cache) => Some(cwd.join(cache)),
        None => {
            let root = crate::workspace::workspace_root(&cwd).ok()?;
            Some(crate::worktree::detect(&root).default_cache())
        }
    }
}

/// DumpCommand.exec: the exit code (stdout and stderr written).
pub fn run(args: &[String], cache_dir: Option<&str>) -> Result<i32, String> {
    let options = match parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("ERROR: {error}");
            return Ok(2);
        }
    };
    let any = options.packages
        || options.action_cache
        || options.rule_classes
        || options.rules
        || options.skylark_memory.is_some()
        || options.skyframe != "off"
        || options.memory.is_some();
    if !any {
        // NO_OUTPUT_SPECIFIED.
        eprint!("{HELP}");
        return Ok(7);
    }
    eprintln!("{WARNING}");
    let mut out = String::new();
    let mut code = 0;
    if options.packages {
        out.push_str("0 packages\n\n");
    }
    if options.action_cache {
        out.push_str(&action_cache(cache_directory(cache_dir).as_deref()));
        out.push('\n');
    }
    if options.rule_classes {
        out.push_str(RULE_CLASSES);
        out.push('\n');
    }
    if options.rules {
        out.push_str("No rules in Bazel server, please run a build command first.\n");
    }
    if options.skylark_memory.is_some() {
        out.push_str("Cannot dump Starlark heap without running in memory tracking mode. Please refer to the user manual for the dump commnd for information how to turn on memory tracking.\n");
    }
    match options.memory {
        Some(true) => {
            print!("{out}");
            out.clear();
            eprintln!("ERROR: The requested node is not present.");
            code = 7;
        }
        Some(false) => out.push_str("{\n}\n"),
        None => {}
    }
    match options.skyframe.as_str() {
        "summary" => out.push_str("Node count: 0\nEdge count: 0\n"),
        "function_graph" => out.push_str("digraph {\n}\n"),
        _ => {}
    }
    print!("{out}");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_like_dump_options() {
        let parsed = parse(&["--rules".into(), "--skyframe".into(), "COUNT".into()]).unwrap();
        assert!(parsed.rules);
        assert_eq!(parsed.skyframe, "count");
        assert_eq!(
            parse(&["--rules=maybe".into()]).unwrap_err(),
            "While parsing option --rules=maybe: 'maybe' is not a boolean"
        );
        assert_eq!(
            parse(&["--bogus".into()]).unwrap_err(),
            "--bogus :: Unrecognized option: --bogus"
        );
        assert_eq!(
            parse(&["--rules".into(), "extra".into()]).unwrap_err(),
            "Unrecognized arguments: extra"
        );
        assert_eq!(
            parse(&["--memory=shallow,count".into()]).unwrap_err(),
            "While parsing option --memory=shallow,count: Should be in the form: <flags>:<node type>:<node>"
        );
        assert!(
            parse(&["--color=no".into(), "--rules".into()])
                .unwrap()
                .rules
        );
        assert!(parse(&["--keep_going".into()]).is_err());
    }

    #[test]
    fn timestamps_are_epoch_minutes() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_793_773_119), "2026-11-04T06:18:00Z");
    }
}
