//! What the console says about executed actions, like Bazel 9.3.1
//! (docs/PROGRESS.md "Action output and failures"; genuine captures in
//! tests/fixtures/console_action_output):
//!
//! * a successful action that printed: `INFO: From <progress message>:`,
//!   then its stderr (on stderr) and its stdout (on stdout)
//!   (SkyframeActionExecutor.dumpRecordedOutErr, UiEventHandler
//!   .handleLocked), subject to `--output_filter`/`--auto_output_filter`
//!   and `--experimental_ui_max_stdouterr_bytes` (crate::output_filter);
//! * a failed spawn: `ERROR: <owner location>: <progress message> failed:
//!   (Exit N): <argv0> failed: error executing <Mnemonic> command (from
//!   <kind> rule target <label>) <command>` (SkyframeActionExecutor
//!   .printError, SpawnResult.getDetailMessage, CommandFailureUtils
//!   .describeCommandFailure), the sandbox suggestion, then the output.
//!   `--verbose_failures` prints the complete command (`(cd … && exec env
//!   - … )` with `# Configuration:` and `# Execution platform:`).
//!
//! The progress message is AbstractAction.describe(): the raw message
//! (`progress_message`, a genrule's `message`, the mnemonic's default) with
//! `%{label}`/`%{output}`/`%{input}` replaced, ` [for tool]` appended in a
//! tool (exec) configuration unless the action has no raw message
//! (SolibSymlinkAction, RunfilesTreeAction: `MNEMONIC output`).
//!
//! Once a build without `--keep_going` has a failed action, later
//! failures and outputs are not reported (SkyframeActionExecutor
//! .isBuilderAborting).

use crate::action::{Action, Transform};
use crate::shared_path::SharedPath;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The error text of an action failure this module already printed: the
/// scheduler and the CLI recognise it ([`is_reported`]) and print no
/// second message.
pub const REPORTED: &str = "\u{1}action failure reported\u{1}";

/// Whether `error` (possibly wrapped as `action 'ID': …`) is a failure
/// already printed.
pub fn is_reported(error: &str) -> bool {
    error.contains(REPORTED)
}

/// The runner of a reported failed action whose spawn succeeded (its
/// outputs were missing): SpawnStats counts the spawn under it.
pub fn reported_runner(error: &str) -> Option<&'static str> {
    const RUNNERS: [&str; 7] = [
        "linux-sandbox",
        "local",
        "processwrapper-sandbox",
        "remote",
        "worker",
        "remote cache hit",
        "disk cache hit",
    ];
    let runner = reported(error)?.runner;
    RUNNERS.into_iter().find(|known| *known == runner)
}

/// The parts of a reported failure's error ([`reported`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Reported<'a> {
    /// The runner of a spawn that succeeded (missing outputs), else empty.
    pub runner: &'a str,
    /// The printed location prefix (`LOCATION: `), possibly empty.
    pub location: &'a str,
    /// `DESCRIPTION failed: MESSAGE` (the ActionExecutionException's).
    pub message: &'a str,
    pub stderr: &'a str,
    pub stdout: &'a str,
}

/// The parts of `error` when it is a reported failure.
pub fn reported(error: &str) -> Option<Reported<'_>> {
    let index = error.find(REPORTED)?;
    let rest = &error[index + REPORTED.len()..];
    let mut head = rest.splitn(3, '\u{2}');
    let (runner, location, rest) = (head.next()?, head.next()?, head.next()?);
    let mut streams = rest.splitn(3, '\u{3}');
    Some(Reported {
        runner,
        location,
        message: streams.next()?,
        stderr: streams.next().unwrap_or(""),
        stdout: streams.next().unwrap_or(""),
    })
}

static KEEP_GOING: AtomicBool = AtomicBool::new(false);
static ABORTING: AtomicBool = AtomicBool::new(false);

/// `--keep_going` of this build: every failure is reported.
pub fn set_keep_going(keep_going: bool) {
    KEEP_GOING.store(keep_going, Ordering::Relaxed);
    ABORTING.store(false, Ordering::Relaxed);
}

/// The fields of an owner's console identity.
pub struct OwnerParts<'a> {
    /// Label.toString (`//p:t`, `@@repo//p:t`).
    pub label: &'a str,
    /// Rule.getLocation (`/abs/BUILD:L:C`).
    pub location: Option<&'a str>,
    /// `KIND rule`.
    pub target_kind: &'a str,
    /// AspectDescriptor.getDescription of the aspect path (empty for a
    /// configured target's own actions).
    pub aspects: &'a [String],
    /// BuildConfigurationValue.isToolConfiguration.
    pub tool: bool,
    /// The configuration checksum (the native ConfigId).
    pub configuration: &'a str,
}

/// The packed owner identity (one byte string, interned in snapshots).
pub fn owner(parts: &OwnerParts<'_>) -> SharedPath {
    let text = [
        parts.label,
        parts.location.unwrap_or(""),
        parts.target_kind,
        &parts.aspects.join(", "),
        if parts.tool { "1" } else { "0" },
        parts.configuration,
    ]
    .join("\0");
    SharedPath::from(text)
}

/// `owner` with the action's execution platform (Label.toString of the
/// resolved PlatformInfo).
pub fn with_platform(owner: &SharedPath, platform: &str) -> SharedPath {
    let mut text = owner.as_path().to_string_lossy().into_owned();
    text.push('\0');
    text.push_str(platform);
    SharedPath::from(text)
}

/// An unpacked owner.
#[derive(Debug, Default, PartialEq, Eq)]
struct Owner {
    label: String,
    location: Option<String>,
    target_kind: String,
    aspects: String,
    tool: bool,
    configuration: String,
    /// The execution platform, once planning resolved it.
    platform: Option<String>,
}

impl Owner {
    fn of(action: &Action) -> Self {
        if let Some(packed) = &action.extra.owner {
            let bytes = packed.as_path().as_os_str().as_bytes();
            let text = String::from_utf8_lossy(bytes);
            let fields: Vec<&str> = text.split('\0').collect();
            if fields.len() == 6 || fields.len() == 7 {
                return Self {
                    label: fields[0].to_owned(),
                    location: (!fields[1].is_empty()).then(|| fields[1].to_owned()),
                    target_kind: fields[2].to_owned(),
                    aspects: fields[3].to_owned(),
                    tool: fields[4] == "1",
                    configuration: fields[5].to_owned(),
                    platform: fields.get(6).map(|platform| (*platform).to_owned()),
                };
            }
        }
        // No recorded owner: the label of the action's id.
        let id = action.id.as_str();
        let end = id.find(['#', '[', '%']).unwrap_or(id.len());
        Self {
            label: id[..end].to_owned(),
            ..Self::default()
        }
    }

    /// ActionOwner.getDescription.
    fn description(&self) -> Option<String> {
        if self.target_kind.is_empty() {
            return None;
        }
        let target = format!("{} target {}", self.target_kind, self.label);
        Some(if self.aspects.is_empty() {
            target
        } else {
            // `aspect%s` with `size() >= 1`: always plural.
            format!("aspects [{}] on {target}", self.aspects)
        })
    }
}

/// Artifact.getRootRelativePath of an execution path.
fn root_relative(path: &Path) -> String {
    crate::aquery::root_relative(&path.to_string_lossy()).to_owned()
}

/// The raw progress message (getRawProgressMessage), `None` for the
/// actions that have none.
fn raw_progress(action: &Action) -> Option<String> {
    if let Some(progress) = &action.extra.progress {
        return Some(progress.as_path().to_string_lossy().into_owned());
    }
    let mnemonic = action.extra.mnemonic.as_str();
    if mnemonic == "BazelWorkspaceStatusAction" {
        // Bazel's one status action (both nodes here) names its stable
        // output (BazelWorkspaceStatusModule.java:87-98).
        return Some(format!("{mnemonic} stable-status.txt"));
    }
    match &action.extra.transform {
        Some(Transform::Symlink { progress, .. }) if progress.starts_with("SolibSymlink ") => {
            return None;
        }
        Some(
            Transform::Symlink { progress, .. } | Transform::UnresolvedSymlink { progress, .. },
        ) => return Some(progress.clone()),
        Some(Transform::RunfilesTree { .. }) => return None,
        _ => {}
    }
    if let Some(write) = &action.write {
        return Some(if action.id.ends_with("#repo_mapping") {
            "Writing repo mapping manifest for %{label}".to_owned()
        } else if write.executable {
            "Writing script %{output}".to_owned()
        } else {
            "Writing file %{output}".to_owned()
        });
    }
    Some(match mnemonic {
        // GenRuleBase: `Executing genrule LABEL`.
        "Genrule" => "Executing genrule %{label}".to_owned(),
        "" => "Action %{output}".to_owned(),
        _ => format!("{mnemonic} %{{output}}"),
    })
}

/// AbstractAction.describe().
pub fn describe(action: &Action) -> String {
    let owner = Owner::of(action);
    let output = action.outputs.first().map(|path| root_relative(path));
    let Some(raw) = raw_progress(action) else {
        // defaultProgressMessage: `MNEMONIC output`, no tool suffix.
        let mnemonic = match action.extra.mnemonic.as_str() {
            "" if matches!(action.extra.transform, Some(Transform::Symlink { .. })) => {
                "SolibSymlink"
            }
            "" => "Action",
            mnemonic => mnemonic,
        };
        return format!("{mnemonic} {}", output.unwrap_or_default());
    };
    let mut message = raw;
    if message.contains("%{label}") {
        message = message.replace("%{label}", &owner.label);
    }
    if let Some(output) = &output
        && message.contains("%{output}")
    {
        message = message.replace("%{output}", output);
    }
    if owner.tool {
        message.push_str(" [for tool]");
    }
    message
}

/// The owner label part of an action id (`Label.print(owner)` of
/// Reporter.showOutput).
fn owner_label(action: &Action) -> String {
    Owner::of(action).label
}

/// An action's recorded output as the console shows it: each stream, or
/// the `--experimental_ui_max_stdouterr_bytes` notice in its place
/// (crate::output_filter).
pub struct Logs<'a> {
    pub stdout: std::borrow::Cow<'a, [u8]>,
    pub stderr: std::borrow::Cow<'a, [u8]>,
}

impl<'a> Logs<'a> {
    /// The streams of action `id`, an oversized one replaced.
    pub fn of(id: &str, stdout: &'a [u8], stderr: &'a [u8]) -> Self {
        Self {
            stdout: crate::output_filter::small_enough("stdout", id, stdout),
            stderr: crate::output_filter::small_enough("stderr", id, stderr),
        }
    }

    /// Streams already checked against the limit (read from their files,
    /// crate::output_filter::oversized).
    pub fn checked(stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// No output.
    pub fn none() -> Self {
        Self::checked(Vec::new(), Vec::new())
    }
}

/// SkyframeActionExecutor.actuallyCompleteAction's dump of a successful
/// action's recorded output: `INFO: From DESCRIPTION:` with it.
pub fn report_output(action: &Action, logs: Logs<'_>) {
    if logs.stdout.is_empty() && logs.stderr.is_empty() {
        return;
    }
    if ABORTING.load(Ordering::Relaxed) {
        return;
    }
    if !crate::output_filter::shows_output(&owner_label(action)) {
        return;
    }
    crate::ui::event_with_output(
        &format!("INFO: From {}:", describe(action)),
        &logs.stdout,
        &logs.stderr,
        true,
    );
}

/// TerminationStatus.toShortString of a raw exit status (a signal is
/// 128 + its number).
pub fn short_status(raw: i32) -> String {
    if (129..=191).contains(&raw) {
        let signal = (raw - 128) as usize;
        match SIGNALS.get(signal) {
            Some(name) => (*name).to_owned(),
            None => format!("Signal {signal}"),
        }
    } else {
        format!("Exit {raw}")
    }
}

/// [`short_status`]'s inverse for a signal: the raw status (128 + the
/// signal) of `Killed`, `Signal 42`, ….
pub fn raw_status_of(status: &str) -> Option<i32> {
    if let Some(number) = status.strip_prefix("Signal ") {
        return number.parse::<i32>().ok().map(|signal| 128 + signal);
    }
    SIGNALS
        .iter()
        .skip(1)
        .position(|name| *name == status)
        .map(|index| 129 + index as i32)
}

/// glibc strsignal(3), TerminationStatus.SIGNAL_STRINGS.
const SIGNALS: [&str; 32] = [
    "",
    "Hangup",
    "Interrupt",
    "Quit",
    "Illegal instruction",
    "Trace/breakpoint trap",
    "Aborted",
    "Bus error",
    "Floating point exception",
    "Killed",
    "User defined signal 1",
    "Segmentation fault",
    "User defined signal 2",
    "Broken pipe",
    "Alarm clock",
    "Terminated",
    "Stack fault",
    "Child exited",
    "Continued",
    "Stopped (signal)",
    "Stopped",
    "Stopped (tty input)",
    "Stopped (tty output)",
    "Urgent I/O condition",
    "CPU time limit exceeded",
    "File size limit exceeded",
    "Virtual timer expired",
    "Profiling timer expired",
    "Window changed",
    "I/O possible",
    "Power failure",
    "Bad system call",
];

/// The raw exit status of a finished process (Process.waitFor: the exit
/// code, or 128 + the signal).
pub fn raw_exit(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 1,
    }
}

/// CommandFailureUtils.describeCommand: ABBREVIATED (`verbose` false) or
/// COMPLETE with the Linux ScriptUtil emitter.
#[allow(clippy::too_many_arguments)]
pub fn describe_command(
    verbose: bool,
    pretty: bool,
    argv: &[String],
    environment: &[(String, String)],
    cwd: &str,
    configuration: Option<&str>,
    platform: Option<&str>,
    runner: Option<&str>,
) -> String {
    let escape = |text: &str| crate::run_command::shell_escape(text);
    let mut message = String::new();
    if verbose {
        message.push('(');
        message.push_str(&format!("cd {} && \\\n  ", escape(cwd)));
        message.push_str("exec ");
        message.push_str("env - \\\n  ");
        let mut sorted: Vec<&(String, String)> = environment.iter().collect();
        sorted.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
        for (name, value) in sorted {
            message.push_str(&format!("  {}={} \\\n  ", escape(name), escape(value)));
        }
    }
    let size = argv.len();
    let mut remaining = size;
    for element in argv {
        if !verbose && message.len() + element.len() > 200 {
            message.push_str(&format!(
                " ... (remaining {remaining} argument{} skipped)",
                if remaining == 1 { "" } else { "s" }
            ));
            break;
        }
        if remaining < size {
            message.push_str(if pretty { " \\\n    " } else { " " });
        }
        message.push_str(&escape(element));
        remaining -= 1;
    }
    if verbose {
        message.push(')');
        if let Some(configuration) = configuration {
            message.push_str(&format!("\n# Configuration: {configuration}"));
        }
        if let Some(platform) = platform {
            message.push_str(&format!("\n# Execution platform: {platform}"));
        }
        if let Some(runner) = runner {
            message.push_str(&format!("\n# Runner: {runner}"));
        }
    }
    message
}

/// Print an action failure (`ERROR: LOCATION: DESCRIPTION failed:
/// MESSAGE` with the action's output) and return [`REPORTED`]; after a
/// first failure of a build without `--keep_going` nothing more prints.
pub fn report_failure(action: &Action, message: &str, logs: Logs<'_>) -> String {
    report_failure_with(action, message, logs, None)
}

fn report_failure_with(
    action: &Action,
    message: &str,
    logs: Logs<'_>,
    runner: Option<&str>,
) -> String {
    let location = location_prefix(&Owner::of(action));
    let message = format!("{} failed: {message}", describe(action));
    let text = format!("{location}{message}");
    // The error carries what was (or, once aborting, would have been)
    // printed ([`reported`]): the build event files' failure, `test`'s
    // summary, the callers' tests.
    let error = format!(
        "{REPORTED}{}\u{2}{location}\u{2}{message}\u{3}{}\u{3}{}",
        runner.unwrap_or(""),
        String::from_utf8_lossy(&logs.stderr),
        String::from_utf8_lossy(&logs.stdout)
    );
    let keep_going = KEEP_GOING.load(Ordering::Relaxed);
    if !keep_going && ABORTING.swap(true, Ordering::Relaxed) {
        return error;
    }
    crate::ui::event_with_output(&format!("ERROR: {text}"), &logs.stdout, &logs.stderr, false);
    error
}

/// SkyframeActionExecutor.checkOutputs: `LOCATION: output 'P' was not
/// created` per missing output (reportMissingOutputFile, a genrule's
/// explained; reportOutputTreeArtifactErrors for a tree), then the
/// failure `not all outputs were created or valid` (the output was
/// already dumped).
pub fn missing_outputs(
    action: &Action,
    missing: &[(&std::path::PathBuf, bool)],
    runner: Option<&str>,
) -> String {
    let keep_going = KEEP_GOING.load(Ordering::Relaxed);
    let location = location_prefix(&Owner::of(action));
    let genrule = action.extra.mnemonic == "Genrule";
    if keep_going || !ABORTING.load(Ordering::Relaxed) {
        for (path, tree) in missing {
            let pretty = root_relative(path);
            let message = if *tree {
                format!("output tree artifact {pretty} was not created")
            } else if genrule {
                format!(
                    "declared output '{pretty}' was not created by genrule. This is probably because the genrule actually didn't create this output, or because the output was a directory and the genrule was run remotely (note that only the contents of declared file outputs are copied from genrules run remotely)"
                )
            } else {
                format!("output '{pretty}' was not created")
            };
            crate::ui::event_with_output(&format!("ERROR: {location}{message}"), b"", b"", false);
        }
    }
    // The spawn itself succeeded: it counts under its runner
    // ([`reported_runner`]).
    report_failure_with(
        action,
        "not all outputs were created or valid",
        Logs::none(),
        runner,
    )
}

fn location_prefix(owner: &Owner) -> String {
    owner
        .location
        .as_deref()
        .map(|location| crate::location_printer::located(&format!("{location}: ")).into_owned())
        .unwrap_or_default()
}

/// A spawn that exited unsuccessfully (AbstractSpawnStrategy.exec's
/// SpawnExecException): `(Exit N): ARGV0 failed: error executing MNEMONIC
/// command (from DESCRIPTION) COMMAND`, the sandbox strategies'
/// suggestion appended. `cwd` is the spawn's execution root.
pub fn spawn_failure(
    action: &Action,
    raw_exit: i32,
    cwd: &Path,
    sandbox_suggestion: bool,
    logs: Logs<'_>,
) -> String {
    let message = format!(
        "({}): {}{}",
        short_status(raw_exit),
        command_failure(action, cwd),
        if sandbox_suggestion {
            crate::sandbox::SANDBOX_DEBUG_SUGGESTION
        } else {
            ""
        }
    );
    report_failure(action, &message, logs)
}

/// CommandFailureUtils.describeCommandFailure of the action's spawn.
pub fn command_failure(action: &Action, cwd: &Path) -> String {
    let verbose = crate::build_results::verbose_failures() || crate::sandbox::debug();
    let owner = Owner::of(action);
    let argv = crate::action::arguments(action);
    let short = argv
        .first()
        .map(|program| {
            Path::new(program)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| program.clone())
        })
        .unwrap_or_default();
    let environment: Vec<(String, String)> = crate::action::resolved_environment(action)
        .into_iter()
        .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
        .collect();
    let mut text = format!(
        "{short} failed: error executing {} command ",
        action.extra.mnemonic
    );
    if let Some(description) = owner.description() {
        text.push_str(&format!("(from {description}) "));
    }
    if verbose {
        text.push_str("\n  ");
    }
    let platform = owner
        .platform
        .as_deref()
        .unwrap_or(&action.remote_platform.label);
    text.push_str(&describe_command(
        verbose,
        false,
        &argv,
        &environment,
        &cwd.to_string_lossy(),
        (!owner.configuration.is_empty()).then_some(owner.configuration.as_str()),
        (!platform.is_empty()).then_some(platform),
        None,
    ));
    text
}

/// ActionExecutionContext.maybeReportSubcommand (:459-499): the SUBCOMMAND
/// event's text (`--subcommands`; `pretty` is `pretty_print`).
pub fn subcommand(action: &Action, cwd: &Path, runner: &str, pretty: bool) -> String {
    let owner = Owner::of(action);
    let platform = owner
        .platform
        .clone()
        .unwrap_or_else(|| action.remote_platform.label.clone());
    let mut reason = match owner.description() {
        Some(description) => description,
        None => owner.label.clone(),
    };
    reason.push_str(&format!(
        " [action '{}', configuration: {}",
        describe(action),
        owner.configuration
    ));
    if !platform.is_empty() {
        reason.push_str(&format!(", execution platform: {platform}"));
    }
    reason.push_str(&format!(", mnemonic: {}]", action.extra.mnemonic));
    let environment: Vec<(String, String)> = crate::action::resolved_environment(action)
        .into_iter()
        .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
        .collect();
    format!(
        "# {reason}\n{}",
        describe_command(
            true,
            pretty,
            &crate::action::arguments(action),
            &environment,
            &cwd.to_string_lossy(),
            (!owner.configuration.is_empty()).then_some(owner.configuration.as_str()),
            (!platform.is_empty()).then_some(platform.as_str()),
            Some(runner),
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genrule(owner: Option<SharedPath>) -> Action {
        Action {
            id: "//fail:exit3".into(),
            command: "source external/bazel_tools/tools/genrule/genrule-setup.sh; exit 3".into(),
            outputs: vec!["bazel-out/k8-fastbuild/bin/fail/e3.txt".into()],
            environment: [("PATH".to_owned(), "/bin:/usr/bin:/usr/local/bin".to_owned())].into(),
            extra: crate::action::ActionExtra {
                mnemonic: "Genrule".into(),
                owner,
                ..Default::default()
            },
            ..Action::default()
        }
    }

    fn packed(tool: bool) -> SharedPath {
        owner(&OwnerParts {
            label: "//fail:exit3",
            location: Some("/ws/fail/BUILD:2:8"),
            target_kind: "genrule rule",
            aspects: &[],
            tool,
            configuration: "abc",
        })
    }

    #[test]
    fn descriptions_follow_abstract_action() {
        assert_eq!(
            describe(&genrule(Some(packed(false)))),
            "Executing genrule //fail:exit3"
        );
        assert_eq!(
            describe(&genrule(Some(packed(true)))),
            "Executing genrule //fail:exit3 [for tool]"
        );
        let mut starlark = genrule(Some(packed(true)));
        starlark.extra.mnemonic = "NoMsg".into();
        assert_eq!(describe(&starlark), "NoMsg fail/e3.txt [for tool]");
        starlark.extra.progress = Some("Talking %{label} to %{output}".into());
        assert_eq!(
            describe(&starlark),
            "Talking //fail:exit3 to fail/e3.txt [for tool]"
        );
        let mut solib = genrule(Some(packed(true)));
        solib.extra.mnemonic = "SolibSymlink".into();
        solib.extra.transform = Some(Transform::Symlink {
            target: "p/lib.so".into(),
            require_executable: false,
            progress: "SolibSymlink _solib_k8/lib.so".into(),
        });
        assert_eq!(describe(&solib), "SolibSymlink fail/e3.txt");
        // No owner: the id's label.
        let mut bare = genrule(None);
        bare.id = "//tool:gen[4ff3ec3f]#0".into();
        assert_eq!(describe(&bare), "Executing genrule //tool:gen");
    }

    #[test]
    fn statuses_are_termination_status_short_strings() {
        assert_eq!(short_status(3), "Exit 3");
        assert_eq!(short_status(137), "Killed");
        assert_eq!(short_status(139), "Segmentation fault");
        assert_eq!(short_status(128), "Exit 128");
        assert_eq!(short_status(170), "Signal 42");
    }

    #[test]
    fn commands_abbreviate_and_complete_like_command_failure_utils() {
        let argv: Vec<String> = ["/bin/bash", "-c", "echo 'a b'"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let env = vec![("PATH".to_owned(), "/bin".to_owned())];
        assert_eq!(
            describe_command(false, false, &argv, &env, "/x", None, None, None),
            "/bin/bash -c 'echo '\\''a b'\\'''"
        );
        assert_eq!(
            describe_command(
                true,
                false,
                &argv,
                &env,
                "/x/execroot/_main",
                Some("c1"),
                Some("@@platforms//host:host"),
                None
            ),
            "(cd /x/execroot/_main && \\\n  exec env - \\\n    PATH=/bin \\\n  /bin/bash -c 'echo '\\''a b'\\''')\n# Configuration: c1\n# Execution platform: @@platforms//host:host"
        );
        let long = vec!["a".repeat(150), "b".repeat(60), "c".into()];
        assert_eq!(
            describe_command(false, false, &long, &[], "/", None, None, None),
            format!("{} ... (remaining 2 arguments skipped)", "a".repeat(150))
        );
    }

    #[test]
    fn owners_describe_targets_and_aspects() {
        let action = genrule(Some(packed(false)));
        assert_eq!(
            Owner::of(&action).description().as_deref(),
            Some("genrule rule target //fail:exit3")
        );
        let aspect = owner(&OwnerParts {
            label: "//p:t",
            location: None,
            target_kind: "talk rule",
            aspects: &["//p:defs.bzl%a".to_owned()],
            tool: false,
            configuration: "c",
        });
        let action = genrule(Some(aspect));
        assert_eq!(
            Owner::of(&action).description().as_deref(),
            Some("aspects [//p:defs.bzl%a] on talk rule target //p:t")
        );
    }
}
