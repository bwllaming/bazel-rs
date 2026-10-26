//! The workspace status action (BazelWorkspaceStatusModule.java):
//! `--workspace_status_command`'s script output combined with Bazel's
//! built-in keys into `bazel-out/stable-status.txt` and
//! `bazel-out/volatile-status.txt` (WorkspaceStatusAction.java,
//! BazelWorkspaceStatusAction.java:74-259 of the pinned Bazel 9.3).
//!
//! The files are lists of `key value` lines (the key is everything before
//! the first space, the value everything after it; a missing value leaves a
//! trailing space after the key, `printStatusMap` :151-158). Keys are sorted
//! (TreeMap). Stable keys start with `STABLE_` or are one of
//! BUILD_EMBED_LABEL, BUILD_HOST, BUILD_USER (`isStableKey` :132-137); every
//! other key the script emits is volatile. The built-in keys come from
//! Bazel itself, not the script — BUILD_EMBED_LABEL (`--embed_label`),
//! BUILD_HOST and BUILD_USER go to the stable file, BUILD_TIMESTAMP and
//! FORMATTED_DATE to the volatile one — and a script key of the same name
//! overwrites the built-in value (`parseWorkspaceStatus` results are put
//! after them, :190-198).
//!
//! The action runs the script once per build through `/bin/sh -c` with the
//! client environment and the workspace as working directory
//! (`BazelWorkspaceStatusActionContext.getCommand` :305-320; the flag value
//! is a `PathFragment`, so `./x` normalizes to `x` before the shell sees
//! it, OptionsUtils.PathFragmentConverter). It is volatile and executes
//! unconditionally (`isVolatile`/`executeUnconditionally` :186-194): the
//! volatile file is rewritten every build and is a constant-metadata
//! artifact whose changing content does not rebuild consumers, while the
//! stable file is only rewritten when its content changes
//! (`FileSystemUtils.maybeUpdateContent` :205-215). A failing script fails
//! the build as an ordinary action failure — catastrophe=false, exit code 1
//! (`createExecutionException` builds an ActionExecutionException with
//! `catastrophe=false`, :205-213, and the WorkspaceStatus failure detail of
//! WorkspaceStatusAction.createDetailedExitCode; it is NOT an
//! infrastructure/catastrophe failure): `Failed to determine workspace
//! status: Process exited with status N`, or `Process terminated by signal
//! N` when the shell died by a signal (TerminationStatus.toString :230-238,
//! via BadExitStatusException/AbnormalTerminationException :122-127); the
//! script's stderr passes through to the build's stderr (:113-119).

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// The `--workspace_status_command`/`--embed_label` values
/// (WorkspaceStatusAction.Options:70-97; a module command option of
/// `build`, BazelWorkspaceStatusModule.getCommandOptions:323-328).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceStatusOptions {
    /// `--workspace_status_command` ("" unset: no script runs, the files
    /// carry only the built-in keys).
    pub command: String,
    /// `--embed_label`.
    pub embed_label: String,
}

impl WorkspaceStatusOptions {
    pub fn is_empty(&self) -> bool {
        self.command.is_empty() && self.embed_label.is_empty()
    }
}

/// The computed status of one build: the parsed maps and the exact bytes of
/// both status files.
#[derive(Clone, Debug)]
pub struct WorkspaceStatus {
    pub stable: BTreeMap<String, String>,
    pub volatile: BTreeMap<String, String>,
    pub stable_bytes: Vec<u8>,
    pub volatile_bytes: Vec<u8>,
}

/// Why the workspace status could not be determined: the text after
/// `Failed to determine workspace status: `.
#[derive(Clone, Debug)]
pub struct WorkspaceStatusError(pub String);

/// One computed workspace status of a build (or its failure); computed at
/// most once per build.
#[derive(Debug)]
pub enum ComputedWorkspaceStatus {
    Ok(WorkspaceStatus),
    Failed(WorkspaceStatusError),
}

impl ComputedWorkspaceStatus {
    /// The status, or the shared failure text of
    /// `BazelWorkspaceStatusAction stable-status.txt failed: ...`.
    pub fn failure_text(&self) -> String {
        match self {
            Self::Failed(error) => {
                format!("Failed to determine workspace status: {}", error.0)
            }
            Self::Ok(_) => String::new(),
        }
    }

    /// A clone behind the OnceLock boundary (the computed status is
    /// shared by every target's planning; WorkspaceStatus is Clone).
    pub fn clone_status(&self) -> Self {
        match self {
            Self::Ok(status) => Self::Ok(status.clone()),
            Self::Failed(error) => Self::Failed(WorkspaceStatusError(error.0.clone())),
        }
    }
}

/// BazelWorkspaceStatusAction.isStableKey (:132-137).
pub(crate) fn is_stable_key(key: &str) -> bool {
    key.starts_with("STABLE_") || matches!(key, "BUILD_EMBED_LABEL" | "BUILD_HOST" | "BUILD_USER")
}

/// BazelWorkspaceStatusAction.parseWorkspaceStatus (:139-149): each line
/// split at the first space; only lines with a value count; values are
/// trimmed; a later key overwrites an earlier one.
fn parse_workspace_status(input: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for line in input.trim().split('\n') {
        if let Some((key, value)) = line.split_once(' ') {
            result.insert(key.to_owned(), value.trim().to_owned());
        }
    }
    result
}

/// BazelWorkspaceStatusAction.printStatusMap (:151-158): `key value` lines
/// joined with newlines plus one trailing newline.
fn print_status_map(map: &BTreeMap<String, String>) -> Vec<u8> {
    let mut text = String::new();
    for (key, value) in map {
        text.push_str(key);
        text.push(' ');
        text.push_str(value);
        text.push('\n');
    }
    text.into_bytes()
}

/// BazelWorkspaceStatusAction.format (:80-85): the DateTimeFormatter
/// pattern `yyyy MMM dd HH mm ss EEE` in UTC, English field names.
fn formatted_date(timestamp_seconds: i64) -> String {
    let days = timestamp_seconds.div_euclid(86_400);
    let seconds_of_day = timestamp_seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days: days since 1970-03-01 based.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let year = if month <= 2 { year + 1 } else { year };
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];
    // 1970-01-01 was a Thursday (weekday index 4 with Sunday as 0).
    let weekday =
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][(days + 4).rem_euclid(7) as usize];
    format!(
        "{:04} {} {:02} {:02} {:02} {:02} {}",
        year,
        month_name,
        day,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
        weekday
    )
}

/// BazelWorkspaceStatusAction.getCurrentTimeMillis (:231-243):
/// SOURCE_DATE_EPOCH (seconds) when the client environment has a valid one,
/// the current time otherwise.
pub(crate) fn current_time_millis(
    client_env: &std::collections::BTreeMap<String, std::ffi::OsString>,
) -> i64 {
    if let Some(value) = client_env.get("SOURCE_DATE_EPOCH") {
        let value = value.to_string_lossy();
        let value = value.trim();
        if !value.is_empty()
            && let Ok(seconds) = value.parse::<i64>()
        {
            return seconds.saturating_mul(1000);
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// System.getProperty("user.name") on Linux: the passwd name of the
/// process uid (HotSpot's get_user_name), with USER as the fallback of a
/// missing passwd entry.
fn user_name() -> String {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|fields| fields.split_whitespace().next())
            .map(str::to_owned);
        if let Some(uid) = uid
            && let Ok(passwd) = std::fs::read_to_string("/etc/passwd")
            && let Some(entry) = passwd
                .lines()
                .find(|line| line.split(':').nth(2) == Some(uid.as_str()))
            && let Some(name) = entry.split(':').next()
            && !name.is_empty()
        {
            return name.to_owned();
        }
    }
    std::env::var("USER").unwrap_or_default()
}

/// NetUtil.computeShortHostName (:67-73): the local host's name
/// (InetAddress.getLocalHost().getHostName()), "unknown" when it cannot be
/// determined. On Linux the JVM reads the container's hostname
/// (/etc/hostname of the UTS namespace), which differs from the host's
/// /proc/sys/kernel/hostname in a container.
fn short_host_name() -> String {
    for source in ["/etc/hostname", "/proc/sys/kernel/hostname"] {
        if let Ok(host) = std::fs::read_to_string(source)
            && let host = host.trim()
            && !host.is_empty()
        {
            return host.to_owned();
        }
    }
    "unknown".to_owned()
}

/// The normalized `--workspace_status_command` the shell sees
/// (OptionsUtils.PathFragmentConverter: `./status.sh` becomes `status.sh`).
pub(crate) fn normalized_command(command: &str) -> String {
    if command.starts_with('/') {
        return command.to_owned();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in command.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // PathFragment.create rejects parent traversal; keep the
                // literal segment so the shell reports it like Bazel.
                parts.push(part);
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// WorkspaceStatusAction.OneLineStringConverter (:216-233): `--embed_label`
/// rejects a value containing a newline.
pub fn validate_embed_label(embed_label: &str) -> Result<(), String> {
    if embed_label.contains('\n') {
        return Err("Value must not contain multiple lines".to_owned());
    }
    Ok(())
}

/// Run the workspace status command and compute both status files. The
/// script's stderr is the build's stderr (:113-119): written to stderr
/// here. Non-empty, it keeps the analysis snapshot from being saved
/// (crate::snapshot: a hit would print it a second time, at another place).
pub(crate) fn compute(
    options: &WorkspaceStatusOptions,
    workspace: &std::path::Path,
    client_env: &std::collections::BTreeMap<String, std::ffi::OsString>,
    now_millis: i64,
) -> Result<WorkspaceStatus, WorkspaceStatusError> {
    let (status, stderr) = compute_quiet(options, workspace, client_env, now_millis);
    if !stderr.is_empty() {
        let _ = std::io::stderr().write_all(&stderr);
        crate::observed_fs::taint(crate::observed_fs::Taint::WorkspaceStatusStderr, || {
            format!("{} bytes", stderr.len())
        });
    }
    status
}

/// [`compute`] without printing: the status and the script's stderr (an
/// analysis snapshot hit computes the status before anything prints).
pub(crate) fn compute_quiet(
    options: &WorkspaceStatusOptions,
    workspace: &std::path::Path,
    client_env: &std::collections::BTreeMap<String, std::ffi::OsString>,
    now_millis: i64,
) -> (Result<WorkspaceStatus, WorkspaceStatusError>, Vec<u8>) {
    let mut stderr = Vec::new();
    let status = compute_into(options, workspace, client_env, now_millis, &mut stderr);
    (status, stderr)
}

fn compute_into(
    options: &WorkspaceStatusOptions,
    workspace: &std::path::Path,
    client_env: &std::collections::BTreeMap<String, std::ffi::OsString>,
    now_millis: i64,
    stderr: &mut Vec<u8>,
) -> Result<WorkspaceStatus, WorkspaceStatusError> {
    validate_embed_label(&options.embed_label).map_err(WorkspaceStatusError)?;
    let mut script_output = String::new();
    if !options.command.is_empty() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(normalized_command(&options.command))
            .current_dir(workspace)
            .env_clear()
            .envs(client_env.iter())
            .stdin(Stdio::null());
        // Started by the server itself (crate::signals::Defaults::Jvm).
        crate::signals::reset_in_child(&mut command, crate::signals::Defaults::Jvm);
        let output = crate::spawn_lock::output(&mut command)
            .map_err(|error| WorkspaceStatusError(format!("exec failed: {error}")))?;
        stderr.extend_from_slice(&output.stderr);
        if !output.status.success() {
            // TerminationStatus.toString (:230-238): a signal death reads
            // `Process terminated by signal N`, a normal non-zero exit
            // `Process exited with status N`.
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = output.status.signal() {
                    return Err(WorkspaceStatusError(format!(
                        "Process terminated by signal {signal}"
                    )));
                }
            }
            return Err(WorkspaceStatusError(format!(
                "Process exited with status {}",
                output.status.code().unwrap_or(-1)
            )));
        }
        script_output = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    let mut stable = BTreeMap::new();
    let mut volatile = BTreeMap::new();
    stable.insert("BUILD_EMBED_LABEL".to_owned(), options.embed_label.clone());
    stable.insert("BUILD_HOST".to_owned(), short_host_name());
    stable.insert("BUILD_USER".to_owned(), user_name());
    let timestamp_seconds = now_millis / 1000;
    volatile.insert("BUILD_TIMESTAMP".to_owned(), timestamp_seconds.to_string());
    volatile.insert(
        "FORMATTED_DATE".to_owned(),
        formatted_date(timestamp_seconds),
    );
    for (key, value) in parse_workspace_status(&script_output) {
        if is_stable_key(&key) {
            stable.insert(key, value);
        } else {
            volatile.insert(key, value);
        }
    }
    Ok(WorkspaceStatus {
        stable_bytes: print_status_map(&stable),
        volatile_bytes: print_status_map(&volatile),
        stable,
        volatile,
    })
}

/// WorkspaceStatusAction.parseValues (:120-134): the status-file parse of
/// a consumer (BuildInfoFileWriteAction.execute :123-129): each line split
/// at the first space, only two-item lines, values NOT trimmed (the file
/// format's `key value` bytes are final).
pub(crate) fn parse_values(file: &[u8]) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let text = String::from_utf8_lossy(file);
    for line in text.split('\n') {
        if let Some((key, value)) = line.split_once(' ') {
            result.insert(key.to_owned(), value.to_owned());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, std::ffi::OsString> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), std::ffi::OsString::from(*value)))
            .collect()
    }

    #[test]
    fn built_in_keys_and_file_bytes() {
        let status = compute(
            &WorkspaceStatusOptions {
                command: String::new(),
                embed_label: "rel-9.3".to_owned(),
            },
            std::path::Path::new("."),
            &env(&[]),
            1_700_000_000_000,
        )
        .unwrap();
        // The exact host/user values depend on the machine; the keys, the
        // order, the empty-value trailing space and the volatile rows do not.
        assert!(
            status
                .stable_bytes
                .starts_with(b"BUILD_EMBED_LABEL rel-9.3\n")
        );
        assert!(status.stable.contains_key("BUILD_HOST"));
        assert!(status.stable.contains_key("BUILD_USER"));
        assert_eq!(
            String::from_utf8_lossy(&status.volatile_bytes),
            "BUILD_TIMESTAMP 1700000000\nFORMATTED_DATE 2023 Nov 14 22 13 20 Tue\n"
        );
        // The pinned capture environment overrides the built-ins through the
        // script (a special stable key overwrites the built-in value).
        let mut with_script = status;
        with_script
            .stable
            .insert("BUILD_USER".to_owned(), "stampuser".to_owned());
        with_script.stable_bytes = print_status_map(&with_script.stable);
        assert!(
            String::from_utf8_lossy(&with_script.stable_bytes).ends_with("BUILD_USER stampuser\n")
        );
    }

    #[test]
    fn empty_embed_label_keeps_its_trailing_space() {
        let bytes = print_status_map(&BTreeMap::from([(
            "BUILD_EMBED_LABEL".to_owned(),
            String::new(),
        )]));
        assert_eq!(bytes, b"BUILD_EMBED_LABEL \n");
    }

    #[test]
    fn parse_helpers() {
        let parsed = parse_workspace_status("A 1\nSTABLE_X spaced  value\nnospacenkey\n\n");
        assert_eq!(parsed.get("A").map(String::as_str), Some("1"));
        // Inner spacing survives the trim (:144).
        assert_eq!(
            parsed.get("STABLE_X").map(String::as_str),
            Some("spaced  value")
        );
        assert!(!parsed.contains_key("nospacenkey"));
        let values = parse_values(b"BUILD_EMBED_LABEL \nBUILD_TIMESTAMP 123\nignored\n");
        assert_eq!(
            values.get("BUILD_EMBED_LABEL").map(String::as_str),
            Some("")
        );
        assert_eq!(
            values.get("BUILD_TIMESTAMP").map(String::as_str),
            Some("123")
        );
    }

    #[test]
    fn command_normalization_matches_path_fragment() {
        assert_eq!(normalized_command("./status.sh"), "status.sh");
        assert_eq!(normalized_command("././x/y"), "x/y");
        assert_eq!(normalized_command("/abs/script.sh"), "/abs/script.sh");
        assert_eq!(normalized_command("echo hi"), "echo hi");
    }

    #[test]
    fn source_date_epoch_pins_the_timestamp() {
        let pinned = env(&[("SOURCE_DATE_EPOCH", "1700000000")]);
        assert_eq!(current_time_millis(&pinned), 1_700_000_000_000);
        let empty = env(&[("SOURCE_DATE_EPOCH", "")]);
        assert_ne!(current_time_millis(&empty), 1_700_000_000_000);
    }

    #[test]
    fn formatted_date_matches_the_java_pattern() {
        assert_eq!(formatted_date(1_700_000_000), "2023 Nov 14 22 13 20 Tue");
        assert_eq!(formatted_date(0), "1970 Jan 01 00 00 00 Thu");
        assert_eq!(formatted_date(951_782_400), "2000 Feb 29 00 00 00 Tue");
        assert_eq!(formatted_date(1_792_511_811), "2026 Oct 20 15 56 51 Tue");
    }

    #[test]
    fn a_failing_script_reports_the_exit_status() {
        let error = compute(
            &WorkspaceStatusOptions {
                command: "exit 3".to_owned(),
                embed_label: String::new(),
            },
            std::path::Path::new("."),
            &env(&[]),
            0,
        )
        .unwrap_err();
        assert_eq!(error.0, "Process exited with status 3");
    }

    #[test]
    fn a_signalled_script_reports_the_signal() {
        // TerminationStatus.toString: `Process terminated by signal N`.
        // The direct child here is /bin/sh; if IT is the one killed
        // (kill -TERM $$ in the -c string), the wait status is a signal
        // death. Through a script file the intermediate shell reports
        // 128+15 instead (the integration test covers that shape).
        let error = compute(
            &WorkspaceStatusOptions {
                command: "kill -TERM $$".to_owned(),
                embed_label: String::new(),
            },
            std::path::Path::new("."),
            &env(&[]),
            0,
        )
        .unwrap_err();
        assert_eq!(error.0, "Process terminated by signal 15");
    }

    #[test]
    fn embed_label_rejects_multiple_lines() {
        // WorkspaceStatusAction.OneLineStringConverter (:216-233).
        assert_eq!(
            validate_embed_label("a\nb").unwrap_err(),
            "Value must not contain multiple lines"
        );
        assert!(validate_embed_label("one line").is_ok());
        let error = compute(
            &WorkspaceStatusOptions {
                command: String::new(),
                embed_label: "a\nb".to_owned(),
            },
            std::path::Path::new("."),
            &env(&[]),
            0,
        )
        .unwrap_err();
        assert_eq!(error.0, "Value must not contain multiple lines");
    }
}
