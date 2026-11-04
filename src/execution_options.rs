//! Build request and execution options that real rc files set
//! (docs/RC_OPTIONS.md "Execution and output options"). Each has its Bazel
//! 9.3.1 effect natively, or is accepted only where its genuine effect is
//! what the native profile already does:
//!
//! * `--check_up_to_date` (ExecutionOptions.java:200-211): CheckUpToDateFilter
//!   prohibits running an action the action cache misses
//!   (crate::action::set_check_up_to_date): `action 'X' is not up-to-date`.
//! * `--modify_execution_info` / `--[no]incompatible_modify_execution_info_additive`
//!   (CoreOptions.java:963-1012; analysis/config/ExecutionInfoModifier.java):
//!   applied to every spawn's execution requirements by mnemonic once the
//!   plan is final ([`apply_execution_info_modifiers`]), so the keys, the
//!   strategy choice and remote cacheability see them.
//! * `--local_resources` (ExecutionOptions.java:332-363, ResourceConverter
//!   .AssignmentConverter): `cpu` and `memory` bound how many local actions
//!   run at once — each needs AbstractAction.DEFAULT_RESOURCE_SET (1 CPU,
//!   250 MB), and one runs even when it does not fit
//!   (`--allow_one_action_on_resource_unavailable`).
//! * `--progress_report_interval` (ExecutionOptions, an int in 0-3600): the
//!   interval of the non-curses progress reports, which the native console
//!   does not print — accepted and validated (pure UI).
//! * `--[no]action_cache_store_output_metadata` (old name
//!   `--experimental_action_cache_store_output_metadata`): accepted (the
//!   native action cache always stores its outputs; it only lets genuine's
//!   cache serve remote-only outputs, no result changes).
//! * `--symlink_prefix` / `--experimental_convenience_symlinks`: bazel-rs
//!   creates no convenience symlinks, which is genuine's behaviour with
//!   `--symlink_prefix=/` or `=ignore`; those are accepted, `=clean` deletes
//!   the existing `<prefix>{bin,testlogs,out,<workspace>}` symlinks, and a
//!   combination that would create symlinks fails explicitly.
//! * `--toolchain_resolution_debug` (PlatformOptions, a RegexFilter):
//!   crate::toolchains' debug messages.
//! * The output filters (crate::output_filter).

use crate::rc_options::value_option;

/// BuildRequestOptions.ConvenienceSymlinksMode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConvenienceSymlinks {
    #[default]
    Normal,
    Clean,
    Ignore,
    LogOnly,
}

/// The options of this module of one command.
#[derive(Clone, Debug)]
pub struct ExecutionOptions {
    pub output: crate::output_filter::Options,
    pub check_up_to_date: bool,
    pub progress_report_interval: i64,
    /// `--symlink_prefix` (`None`: `bazel-`).
    pub symlink_prefix: Option<String>,
    /// `--experimental_convenience_symlinks` (`None`: not given, `normal`).
    pub convenience_symlinks: Option<ConvenienceSymlinks>,
    /// `--modify_execution_info` values in order, each converted.
    pub modify_execution_info: Vec<Vec<Modifier>>,
    pub modify_execution_info_additive: bool,
    /// `--local_resources` (name, value) in order.
    pub local_resources: Vec<(String, f64)>,
    /// `--toolchain_resolution_debug` as given (default `-.*`).
    pub toolchain_resolution_debug: Option<String>,
    /// `--ui_event_filters` values in order (filtered, unfiltered kinds).
    pub ui_event_filters: Vec<(Vec<&'static str>, Vec<&'static str>)>,
}

/// EventKind (events/EventKind.java) in declaration order.
const EVENT_KINDS: [&str; 16] = [
    "FATAL",
    "ERROR",
    "WARNING",
    "INFO",
    "PROGRESS",
    "DEBUG",
    "START",
    "FINISH",
    "SUBCOMMAND",
    "STDOUT",
    "STDERR",
    "PASS",
    "FAIL",
    "TIMEOUT",
    "CANCELLED",
    "DEPCHECKER",
];

/// UiOptions.EventFiltersConverter (runtime/UiOptions.java:52-110): a
/// comma-separated list of event kinds, `-KIND` filters one out, `+KIND`
/// shows it again, a bare `KIND` starts from everything filtered; the
/// empty value filters every kind.
pub fn parse_event_filters(input: &str) -> Result<(Vec<&'static str>, Vec<&'static str>), String> {
    if input.is_empty() {
        return Ok((EVENT_KINDS.to_vec(), Vec::new()));
    }
    let mut filtered: Vec<&'static str> = Vec::new();
    let mut unfiltered: Vec<&'static str> = Vec::new();
    for filter in input.split(',') {
        if !filter.starts_with(['+', '-']) {
            filtered = EVENT_KINDS.to_vec();
            unfiltered.clear();
        }
        if filter.is_empty() {
            continue;
        }
        let name = filter.strip_prefix(['+', '-']).unwrap_or(filter);
        let kind = EVENT_KINDS
            .iter()
            .find(|kind| kind.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                format!(
                    "Not a valid event kind: '{name}' (should be {})",
                    EVENT_KINDS
                        .iter()
                        .map(|kind| kind.to_ascii_lowercase())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        if filter.starts_with('-') {
            if !filtered.contains(kind) {
                filtered.push(kind);
            }
            unfiltered.retain(|seen| seen != kind);
        } else {
            if !unfiltered.contains(kind) {
                unfiltered.push(kind);
            }
            filtered.retain(|seen| seen != kind);
        }
    }
    Ok((filtered, unfiltered))
}

/// UiOptions.getFilteredEventKinds over every `--ui_event_filters` value:
/// the kinds the console drops, or an explicit error for kinds the native
/// console cannot drop (its errors, test results, action output and
/// progress are no filterable events).
pub fn filtered_event_kinds(
    filters: &[(Vec<&'static str>, Vec<&'static str>)],
) -> Result<Vec<&'static str>, String> {
    let mut kinds: Vec<&'static str> = Vec::new();
    for (filtered, unfiltered) in filters {
        for kind in filtered {
            if !kinds.contains(kind) {
                kinds.push(kind);
            }
        }
        kinds.retain(|kind| !unfiltered.contains(kind));
    }
    // START/FINISH are never displayed by the default UI; INFO, WARNING
    // and DEBUG are the native notes' kinds (crate::ui::set_hidden_kinds).
    let unsupported: Vec<&str> = kinds
        .iter()
        .copied()
        .filter(|kind| !matches!(*kind, "INFO" | "WARNING" | "DEBUG" | "START" | "FINISH"))
        .collect();
    if !unsupported.is_empty() {
        return Err(format!(
            "unsupported option --ui_event_filters: filtering {} events is not modelled by the native console (only INFO, WARNING, DEBUG, START and FINISH, docs/RC_OPTIONS.md); options are never silently ignored",
            unsupported.join(", ")
        ));
    }
    Ok(kinds
        .into_iter()
        .filter(|kind| !matches!(*kind, "START" | "FINISH"))
        .collect())
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            output: Default::default(),
            check_up_to_date: false,
            progress_report_interval: 0,
            symlink_prefix: None,
            convenience_symlinks: None,
            modify_execution_info: Vec::new(),
            modify_execution_info_additive: true,
            local_resources: Vec::new(),
            toolchain_resolution_debug: None,
            ui_event_filters: Vec::new(),
        }
    }
}

/// One `regex=[+-]key` expression (ExecutionInfoModifier.Expression).
#[derive(Clone, Debug)]
pub struct Modifier {
    pattern: fancy_regex::Regex,
    remove: bool,
    key: String,
}

/// ExecutionInfoModifier.Converter: comma-separated `regex=[+-]key`
/// expressions (`^(?<pattern>.+)=(?<sign>[+-])(?<key>.+)$`, greedy: the
/// last `=` followed by a sign splits), each regex a valid pattern; the
/// empty value is no modifier.
pub fn parse_modifiers(input: &str) -> Result<Vec<Modifier>, String> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut modifiers = Vec::new();
    for spec in input.split(',') {
        let split = spec
            .char_indices()
            .filter(|(index, c)| {
                *c == '='
                    && *index > 0
                    && matches!(spec[index + 1..].chars().next(), Some('+' | '-'))
                    && spec.len() > index + 2
            })
            .map(|(index, _)| index)
            .next_back();
        let Some(index) = split else {
            return Err(format!("malformed expression '{spec}'"));
        };
        let regex = &spec[..index];
        crate::output_filter::compile(regex)?;
        modifiers.push(Modifier {
            // Matcher.matches(): the whole mnemonic.
            pattern: fancy_regex::Regex::new(&format!("^(?:{regex})$"))
                .map_err(|error| format!("Not a valid regular expression: {error}"))?,
            remove: spec[index + 1..].starts_with('-'),
            key: spec[index + 2..].to_owned(),
        });
    }
    Ok(modifiers)
}

/// ResourceConverter.AssignmentConverter: `name=value`, the value a double
/// or `HOST_CPUS`/`HOST_RAM` optionally followed by `-<float>` or
/// `*<float>`, at least 0.
pub fn parse_resource(input: &str) -> Result<(String, f64), String> {
    let (name, value) = match input.find('=') {
        Some(index) if index > 0 => (&input[..index], &input[index + 1..]),
        _ => {
            return Err(
                "Variable definitions must be in the form of a 'name=value' assignment".to_owned(),
            );
        }
    };
    Ok((name.to_owned(), resource_value(value)?))
}

/// ResourceConverter.DoubleConverter over the HOST_CPUS/HOST_RAM keywords
/// (util/ResourceConverter.java:113-238): a double, or a keyword with an
/// optional `-<float>`/`*<float>` operation; at least 0.
pub fn resource_value(value: &str) -> Result<f64, String> {
    let description = "an integer, or a keyword (\"HOST_CPUS\", \"HOST_RAM\"), optionally followed by an operation ([-|*]<float>) eg. \"HOST_CPUS\", \"HOST_CPUS*.5\"";
    let number = if let Ok(number) = value.trim().parse::<f64>() {
        if number < 0.0 {
            return Err(format!("Value '({number:.6})' must be at least 0.000000."));
        }
        number
    } else {
        let syntax = || {
            format!(
                "Parameter '{value}' does not follow correct syntax. This flag takes {description}."
            )
        };
        let (keyword, rest) = if let Some(rest) = value.strip_prefix("HOST_CPUS") {
            (
                std::thread::available_parallelism().map_or(1, usize::from) as f64,
                rest,
            )
        } else if let Some(rest) = value.strip_prefix("HOST_RAM") {
            (
                crate::memory_limit::mem_total_mb().unwrap_or(0) as f64,
                rest,
            )
        } else {
            return Err(syntax());
        };
        if rest.is_empty() {
            keyword
        } else {
            // `[-*][0-9]?(?:.[0-9]+)?`
            let operand = &rest[1..];
            let valid = {
                let digits = operand
                    .strip_prefix(|c: char| c.is_ascii_digit())
                    .unwrap_or(operand);
                digits.is_empty()
                    || (digits.len() > 1 && digits[1..].bytes().all(|b| b.is_ascii_digit()))
            };
            if !valid {
                return Err(syntax());
            }
            let operand: f32 = operand
                .parse()
                .map_err(|_| format!("'{operand} is not a float"))?;
            match &rest[..1] {
                "-" => keyword - f64::from(operand),
                "*" => keyword * f64::from(operand),
                _ => return Err(syntax()),
            }
        }
    };
    Ok(number)
}

impl ExecutionOptions {
    /// One option of this module; `build_command`: the command takes build
    /// options (BuildRequestOptions, ExecutionOptions, PlatformOptions),
    /// else only the CoreOptions and UiOptions here. `Some(used_next)`.
    pub fn parse(
        &mut self,
        build_command: bool,
        arg: &str,
        next: Option<&str>,
    ) -> Option<Result<bool, String>> {
        use crate::configuration::boolean_option;
        // CoreOptions (every configuration-option command, `query` too).
        if let Some(value) = boolean_option(arg, "incompatible_modify_execution_info_additive") {
            return Some(value.map(|value| {
                self.modify_execution_info_additive = value;
                false
            }));
        }
        if let Some(parsed) = value_option(arg, next, "modify_execution_info") {
            return Some(parsed.and_then(|(form, value, used)| {
                let modifiers = parse_modifiers(value)
                    .map_err(|error| format!("While parsing option {form}: {error}"))?;
                self.modify_execution_info.push(modifiers);
                Ok(used)
            }));
        }
        // UiOptions: every command.
        if (arg.starts_with("--experimental_ui_max_stdouterr_bytes")
            || arg.starts_with("--noexperimental_ui_max_stdouterr_bytes"))
            && let Some(result) = self.output.parse(arg, next)
        {
            return Some(result);
        }
        if let Some(parsed) = value_option(arg, next, "ui_event_filters") {
            return Some(parsed.and_then(|(form, value, used)| {
                self.ui_event_filters.push(
                    parse_event_filters(value)
                        .map_err(|error| format!("While parsing option {form}: {error}"))?,
                );
                Ok(used)
            }));
        }
        if !build_command {
            return None;
        }
        if let Some(result) = self.output.parse(arg, next) {
            return Some(result);
        }
        if let Some(value) = boolean_option(arg, "check_up_to_date") {
            return Some(value.map(|value| {
                self.check_up_to_date = value;
                false
            }));
        }
        for name in [
            "action_cache_store_output_metadata",
            "experimental_action_cache_store_output_metadata",
        ] {
            if let Some(value) = boolean_option(arg, name) {
                return Some(value.map(|_| false));
            }
        }
        if let Some(parsed) = value_option(arg, next, "progress_report_interval") {
            return Some(parsed.and_then(|(form, value, used)| {
                self.progress_report_interval = crate::output_filter::range_int(value, 0, 3600)
                    .map_err(|error| format!("While parsing option {form}: {error}"))?;
                Ok(used)
            }));
        }
        if let Some(parsed) = value_option(arg, next, "symlink_prefix") {
            return Some(parsed.map(|(_, value, used)| {
                self.symlink_prefix = Some(value.to_owned());
                used
            }));
        }
        // A BoolOrEnumConverter: `--experimental_convenience_symlinks` is
        // `normal`, `--noexperimental_convenience_symlinks` is `ignore`.
        match arg {
            "--experimental_convenience_symlinks" => {
                self.convenience_symlinks = Some(ConvenienceSymlinks::Normal);
                return Some(Ok(false));
            }
            "--noexperimental_convenience_symlinks" => {
                self.convenience_symlinks = Some(ConvenienceSymlinks::Ignore);
                return Some(Ok(false));
            }
            _ => {}
        }
        if let Some(value) = arg.strip_prefix("--experimental_convenience_symlinks=") {
            self.convenience_symlinks = Some(match value.to_ascii_lowercase().as_str() {
                "normal" | "true" | "1" | "yes" | "t" | "y" => ConvenienceSymlinks::Normal,
                "clean" => ConvenienceSymlinks::Clean,
                "ignore" | "false" | "0" | "no" | "f" | "n" => ConvenienceSymlinks::Ignore,
                "log_only" => ConvenienceSymlinks::LogOnly,
                _ => {
                    return Some(Err(format!(
                        "While parsing option {arg}: Not a valid convenience symlinks mode: '{value}' (should be normal, clean, ignore or log_only)"
                    )));
                }
            });
            return Some(Ok(false));
        }
        if let Some(parsed) = value_option(arg, next, "local_resources") {
            return Some(parsed.and_then(|(form, value, used)| {
                self.local_resources.push(
                    parse_resource(value)
                        .map_err(|error| format!("While parsing option {form}: {error}"))?,
                );
                Ok(used)
            }));
        }
        if let Some(parsed) = value_option(arg, next, "toolchain_resolution_debug") {
            return Some(parsed.and_then(|(form, value, used)| {
                crate::coverage::RegexFilter::parse(value)
                    .map_err(|error| format!("While parsing option {form}: {error}"))?;
                self.toolchain_resolution_debug = Some(value.to_owned());
                Ok(used)
            }));
        }
        None
    }

    /// The convenience symlink mode the native profile can honor: no
    /// symlinks are ever created, so a combination that would create them
    /// fails explicitly; `clean` removes `prefix`-named symlinks.
    pub fn check_convenience_symlinks(&self) -> Result<Option<String>, String> {
        let prefix = self.symlink_prefix.as_deref().unwrap_or("bazel-");
        if prefix == "/" {
            return Ok(None);
        }
        match self.convenience_symlinks {
            Some(ConvenienceSymlinks::Ignore) => Ok(None),
            Some(ConvenienceSymlinks::Clean) => Ok(Some(prefix.to_owned())),
            // The default (neither option given): the native profile's own
            // documented behaviour (no symlinks).
            None if self.symlink_prefix.is_none() => Ok(None),
            mode => Err(format!(
                "unsupported option --symlink_prefix={prefix} with --experimental_convenience_symlinks={}: bazel-rs creates no convenience symlinks (only --symlink_prefix=/ and --experimental_convenience_symlinks=ignore|clean are modelled, docs/RC_OPTIONS.md); options are never silently ignored",
                if mode == Some(ConvenienceSymlinks::LogOnly) {
                    "log_only"
                } else {
                    "normal"
                }
            )),
        }
    }

    /// The local resources' cap on concurrently running local actions
    /// (ResourceManager with DEFAULT_RESOURCE_SET 1 CPU / 250 MB, at least
    /// one): `None` without `--local_resources` cpu or memory.
    pub fn local_action_cap(&self) -> Option<usize> {
        let last = |name: &str| {
            self.local_resources
                .iter()
                .rev()
                .find(|(resource, _)| resource == name)
                .map(|(_, value)| *value)
        };
        let cpu = last("cpu").map(|cpu| cpu.floor());
        let memory = last("memory").map(|memory| (memory / 250.0).floor());
        [cpu, memory]
            .into_iter()
            .flatten()
            .map(|cap| (cap.max(1.0)) as usize)
            .min()
    }
}

/// This command's `--modify_execution_info` values and
/// `--incompatible_modify_execution_info_additive`.
static MODIFIERS: std::sync::RwLock<(Vec<Vec<Modifier>>, bool)> =
    std::sync::RwLock::new((Vec::new(), true));

/// Install this command's execution info modifiers.
pub fn install_modifiers(modifiers: Vec<Vec<Modifier>>, additive: bool) {
    *MODIFIERS.write().unwrap_or_else(|error| error.into_inner()) = (modifiers, additive);
}

/// BuildConfigurationValue.modifyExecutionInfo over an insertion-ordered
/// map (ExecutionInfoModifier.apply: a removal drops the key, an addition
/// puts `""`, keeping an existing key's position), for aquery's TestRunner
/// keys (crate::engine::aquery_test).
pub fn modify_ordered(mnemonic: &str, info: &mut Vec<(String, String)>) {
    let guard = MODIFIERS.read().unwrap_or_else(|error| error.into_inner());
    let (modifiers, additive) = &*guard;
    let applied: &[Vec<Modifier>] = if *additive {
        modifiers
    } else {
        modifiers.last().map_or(&[], std::slice::from_ref)
    };
    for expression in applied.iter().flatten() {
        if !expression.pattern.is_match(mnemonic).unwrap_or(false) {
            continue;
        }
        if expression.remove {
            info.retain(|(key, _)| *key != expression.key);
        } else if let Some(entry) = info.iter_mut().find(|(key, _)| *key == expression.key) {
            entry.1 = String::new();
        } else {
            info.push((expression.key.clone(), String::new()));
        }
    }
}

/// BuildConfigurationValue.modifiedExecutionInfo for every spawn of a
/// planned request (SpawnAction, StarlarkAction, Genrule, CppCompile,
/// Javac, TestRunner …: every action that executes a process; file writes
/// and the native internal transforms take no execution info).
pub fn apply_execution_info_modifiers(nodes: &mut [crate::scheduler::PlannedAction]) {
    let guard = MODIFIERS.read().unwrap_or_else(|error| error.into_inner());
    let (modifiers, additive) = &*guard;
    if modifiers.iter().all(Vec::is_empty) {
        return;
    }
    for node in nodes {
        let action = &mut node.action;
        if action.write.is_some() || action.extra.transform.is_some() {
            continue;
        }
        let mnemonic = action.extra.mnemonic.clone();
        modify(
            modifiers,
            *additive,
            &mnemonic,
            &mut action.extra.execution_info,
            &mut action.execution_info,
        );
    }
}

/// `--experimental_convenience_symlinks=clean`
/// (OutputDirectoryLinksUtils.removeOutputDirectoryLinks): delete the
/// workspace's `PREFIX{bin,testlogs,out,genfiles,<workspace name>}`
/// symlinks; anything that is not a symlink is left alone.
pub fn clean_convenience_symlinks(workspace: &std::path::Path, prefix: &str) {
    let name = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for suffix in ["bin", "testlogs", "out", "genfiles", name.as_str()] {
        let link = workspace.join(format!("{prefix}{suffix}"));
        // snapshot-unrecorded: a workspace cleanup the command performs
        // before planning, never an input of analysis or planning.
        if std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            let _ = std::fs::remove_file(&link);
        }
    }
}

/// ExecutionInfoModifier.apply (additive: every value in order; else only
/// the last) to one spawn's requirements and rule-tag requirements.
pub fn modify(
    modifiers: &[Vec<Modifier>],
    additive: bool,
    mnemonic: &str,
    requirements: &mut std::collections::BTreeMap<String, String>,
    tags: &mut std::collections::BTreeSet<String>,
) {
    let applied: &[Vec<Modifier>] = if additive {
        modifiers
    } else {
        modifiers.last().map_or(&[], std::slice::from_ref)
    };
    for expressions in applied {
        for expression in expressions {
            if !expression.pattern.is_match(mnemonic).unwrap_or(false) {
                continue;
            }
            if expression.remove {
                requirements.remove(&expression.key);
                tags.remove(&expression.key);
            } else {
                requirements.insert(expression.key.clone(), String::new());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn modifiers_parse_and_apply_like_bazel() {
        let modifiers = parse_modifiers("Genrule=+no-remote,Cpp.*=-requires-x,.*=+a=b").unwrap();
        assert_eq!(modifiers.len(), 3);
        // Greedy: the last `=[+-]` splits (`.*=+a=b`: regex `.*`, key `a=b`).
        assert_eq!(modifiers[2].key, "a=b");
        assert_eq!(
            parse_modifiers("Genrule").unwrap_err(),
            "malformed expression 'Genrule'"
        );
        assert_eq!(
            parse_modifiers("=+x").unwrap_err(),
            "malformed expression '=+x'"
        );
        let mut requirements = BTreeMap::from([("requires-x".to_owned(), String::new())]);
        let mut tags = BTreeSet::from(["requires-x".to_owned()]);
        modify(
            std::slice::from_ref(&modifiers),
            true,
            "CppCompile",
            &mut requirements,
            &mut tags,
        );
        assert_eq!(requirements.keys().collect::<Vec<_>>(), [&"a=b".to_owned()]);
        assert!(tags.is_empty());
        // Non-additive: only the last value applies.
        let mut requirements = BTreeMap::new();
        modify(
            &[modifiers, parse_modifiers("Other=+y").unwrap()],
            false,
            "Genrule",
            &mut requirements,
            &mut BTreeSet::new(),
        );
        assert!(requirements.is_empty());
    }

    #[test]
    fn resources_parse_like_bazel() {
        assert_eq!(parse_resource("cpu=2").unwrap(), ("cpu".to_owned(), 2.0));
        let cpus = std::thread::available_parallelism().map_or(1, usize::from) as f64;
        assert_eq!(parse_resource("cpu=HOST_CPUS*.5").unwrap().1, cpus * 0.5);
        assert_eq!(parse_resource("cpu=HOST_CPUS-1").unwrap().1, cpus - 1.0);
        assert!(
            parse_resource("cpu=lots")
                .unwrap_err()
                .starts_with("Parameter 'lots' does not follow correct syntax.")
        );
        assert_eq!(
            parse_resource("cpu").unwrap_err(),
            "Variable definitions must be in the form of a 'name=value' assignment"
        );
        let mut options = ExecutionOptions::default();
        for arg in ["--local_resources=cpu=3", "--local_resources=memory=1000"] {
            options.parse(true, arg, None).unwrap().unwrap();
        }
        assert_eq!(options.local_action_cap(), Some(3));
        options
            .parse(true, "--local_resources=memory=300", None)
            .unwrap()
            .unwrap();
        assert_eq!(options.local_action_cap(), Some(1));
    }

    #[test]
    fn convenience_symlinks_accept_only_what_the_native_profile_does() {
        let check = |args: &[&str]| {
            let mut options = ExecutionOptions::default();
            for arg in args {
                options.parse(true, arg, None).unwrap().unwrap();
            }
            options.check_convenience_symlinks()
        };
        assert_eq!(check(&[]), Ok(None));
        assert_eq!(check(&["--symlink_prefix=/"]), Ok(None));
        assert_eq!(
            check(&["--experimental_convenience_symlinks=ignore"]),
            Ok(None)
        );
        assert_eq!(check(&["--noexperimental_convenience_symlinks"]), Ok(None));
        assert_eq!(
            check(&["--experimental_convenience_symlinks=clean"]),
            Ok(Some("bazel-".to_owned()))
        );
        assert!(check(&["--symlink_prefix=.b/"]).is_err());
        assert!(check(&["--experimental_convenience_symlinks=log_only"]).is_err());
        assert!(check(&["--experimental_convenience_symlinks=normal"]).is_err());
        assert!(
            ExecutionOptions::default()
                .parse(true, "--experimental_convenience_symlinks=x", None)
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn event_filters_follow_the_converter() {
        let filters = |values: &[&str]| {
            filtered_event_kinds(
                &values
                    .iter()
                    .map(|value| parse_event_filters(value).unwrap())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(filters(&["-info,-debug"]), Ok(vec!["INFO", "DEBUG"]));
        assert_eq!(filters(&["-info", "+info"]), Ok(vec![]));
        assert_eq!(filters(&["-WARNING,+warning,-start"]), Ok(vec![]));
        // A bare kind filters every other kind, errors included.
        assert!(filters(&["info"]).unwrap_err().contains("FATAL, ERROR"));
        assert!(filters(&[""]).is_err());
        assert!(
            parse_event_filters("-nope")
                .unwrap_err()
                .starts_with("Not a valid event kind: 'nope'")
        );
    }

    #[test]
    fn other_options_validate() {
        let mut options = ExecutionOptions::default();
        assert_eq!(
            options.parse(true, "--progress_report_interval=3601", None),
            Some(Err(
                "While parsing option --progress_report_interval=3601: '3601' should be <= 3600"
                    .into()
            ))
        );
        assert_eq!(
            options.parse(true, "--check_up_to_date", None),
            Some(Ok(false))
        );
        assert!(options.check_up_to_date);
        assert_eq!(
            options.parse(
                true,
                "--experimental_action_cache_store_output_metadata",
                None
            ),
            Some(Ok(false))
        );
        assert_eq!(options.parse(false, "--check_up_to_date", None), None);
        assert_eq!(
            options.parse(false, "--modify_execution_info=x=+y", None),
            Some(Ok(false))
        );
    }
}
