//! The soak's logs below the snapshot store root (`<cache>/snapshots/`,
//! L2-W5g): `verify.log` — one line per verify-mode mismatch
//! (crate::snapshot::verify) — and `miss.log` — one line per lookup that
//! missed (`kind=miss`, crate::snapshot::check) and per full analysis that
//! was not saved (`kind=not-saved`, crate::snapshot::save), in `on` and
//! `verify` mode alike, so a reason that repeats (a tool writing into a
//! store tree's directory, a printing local repository) shows in the log,
//! not only under EXPLAIN.
//!
//! Every line is binary-safe and space-separated, each field one token
//! (crate::observed_fs::escape; `-` when absent), the target patterns last:
//!
//! ```text
//! <UTC time> <unix seconds> key=<short G> command=<command> first=<class> differences=<n> taint=<name> path=<path> targets=<pattern>...
//! <UTC time> <unix seconds> kind=miss key=<short G> command=<command> reason=<class> path=<path> detail=<text> targets=<pattern>...
//! <UTC time> <unix seconds> kind=not-saved key=<short G> command=<command> reason=<class> path=- detail=<text> targets=<pattern>...
//! ```
//!
//! Each append is one `write` (O_APPEND: concurrent commands never
//! interleave within a line). A log that would grow past [`LIMIT`] bytes is
//! renamed to `<name>.1` first (replacing the previous generation): at most
//! about twice the limit per log. Best effort: a failed write or rotation
//! loses telemetry, never fails the command; two commands rotating at once
//! may lose a generation.

use crate::observed_fs::escape;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The verify-mode mismatch log.
pub const VERIFY_LOG: &str = "verify.log";
/// The miss and not-saved log.
pub const MISS_LOG: &str = "miss.log";
/// A log's size before it rotates.
pub const LIMIT: u64 = 1 << 20;
/// Bytes of a reason kept in a `detail=` field (before escaping).
const DETAIL: usize = 240;

/// `<cache>/snapshots/<name>`.
pub fn log_path(cache: &Path, name: &str) -> PathBuf {
    crate::snapshot::store::root(cache).join(name)
}

/// Append `line` (one line, with its newline) to the log at `path`,
/// rotating the log first when it would grow past [`LIMIT`].
pub fn append(path: &Path, line: &str) -> std::io::Result<()> {
    append_bounded(path, line, LIMIT)
}

fn append_bounded(path: &Path, line: &str, limit: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.len() > 0 && metadata.len() + line.len() as u64 > limit)
    {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, rotated);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// The two leading fields of a line: `<UTC time> <unix seconds>`.
pub fn now_fields() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    format!("{} {now}", utc_timestamp(now))
}

/// `seconds` since the epoch as `YYYY-MM-DDTHH:MM:SSZ` (UTC; Howard
/// Hinnant's civil_from_days).
pub fn utc_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60
    )
}

/// One field's value: escaped, or `-` when absent or empty.
pub fn token(value: Option<&OsStr>) -> String {
    match value {
        Some(value) if !value.is_empty() => escape(value),
        _ => "-".to_owned(),
    }
}

/// The `targets=<pattern> <pattern>...` tail (each pattern escaped).
pub fn targets_field(targets: &[String]) -> String {
    let targets: Vec<String> = targets
        .iter()
        .map(|target| escape(OsStr::new(target)))
        .collect();
    format!("targets={}", targets.join(" "))
}

/// The first [`DETAIL`] bytes of `text` (on a character boundary), escaped.
fn detail(text: &str) -> String {
    let mut end = text.len().min(DETAIL);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    token(Some(OsStr::new(&text[..end])))
}

/// A `verify.log` line: the first difference's class, the number of
/// differences, the first taint's name and the first differing path.
pub fn verify_line(
    key: &str,
    class: &str,
    differences: usize,
    taint: Option<&str>,
    path: Option<&OsStr>,
    command: &str,
    targets: &[String],
) -> String {
    format!(
        "{} key={key} command={} first={class} differences={differences} taint={} path={} {}\n",
        now_fields(),
        escape(OsStr::new(command)),
        token(taint.map(OsStr::new)),
        token(path),
        targets_field(targets),
    )
}

/// A `miss.log` line; `kind` is `miss` or `not-saved`, `path` already
/// escaped (validation names its paths escaped).
pub fn outcome_line(
    kind: &str,
    key: &str,
    reason: &str,
    path: Option<&str>,
    text: &str,
    command: &str,
    targets: &[String],
) -> String {
    format!(
        "{} kind={kind} key={key} command={} reason={reason} path={} detail={} {}\n",
        now_fields(),
        escape(OsStr::new(command)),
        path.filter(|path| !path.is_empty()).unwrap_or("-"),
        detail(text),
        targets_field(targets),
    )
}

/// What a `miss.log` line names besides its outcome: the cache whose store
/// holds the log, the command and its target patterns.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub cache: PathBuf,
    pub command: String,
    pub targets: Vec<String>,
}

impl Context {
    /// A lookup of the snapshot `key` missed for `reason` (best effort).
    pub fn log_miss(&self, key: &str, reason: &str) {
        let (class, path) = classify_miss(reason);
        let line = outcome_line(
            "miss",
            key,
            class,
            path.as_deref(),
            reason,
            &self.command,
            &self.targets,
        );
        let _ = append(&log_path(&self.cache, MISS_LOG), &line);
    }

    /// The full analysis of `key` was not saved, for `reason` (best effort).
    pub fn log_not_saved(&self, key: &str, reason: &str) {
        let class = classify_not_saved(reason);
        let line = outcome_line(
            "not-saved",
            key,
            &class,
            None,
            reason,
            &self.command,
            &self.targets,
        );
        let _ = append(&log_path(&self.cache, MISS_LOG), &line);
    }
}

/// The class of a hit attempt's miss reason (crate::snapshot::hit) and the
/// path it names (escaped, as validation spells it), if any: `absent`,
/// `unreadable`, `env`, a validation operation in lower case (`read`,
/// `readdir`, `stat`, `lstat`, `canon`, `readlink`, `integrity`), `tree`
/// (a tree verified against its manifest did not match), `replay`,
/// `status-stderr`, `capture`, `panic` or `other`.
pub fn classify_miss(reason: &str) -> (&'static str, Option<String>) {
    // `<OP> <escaped path>: ...` (an escaped path holds no space).
    let operation_path = |rest: &str| -> Option<String> {
        let path = &rest[..rest.find(": ").unwrap_or(rest.len())];
        (!path.is_empty() && !path.contains(' ')).then(|| path.to_owned())
    };
    let (first, rest) = reason.split_once(' ').unwrap_or((reason, ""));
    match first {
        "ENV" => ("env", None),
        "READ" => ("read", operation_path(rest)),
        "STAT" => ("stat", operation_path(rest)),
        "LSTAT" => ("lstat", operation_path(rest)),
        "CANON" => ("canon", operation_path(rest)),
        "READLINK" => ("readlink", operation_path(rest)),
        "INTEGRITY" => ("integrity", operation_path(rest)),
        _ if reason.starts_with("READDIR") => (
            "readdir",
            // `READDIR[<projection>] <path>: ...` (a projection may hold
            // spaces: the path follows its closing bracket).
            reason
                .find("] ")
                .and_then(|at| operation_path(&reason[at + 2..])),
        ),
        _ if reason.starts_with("no snapshot for this key") => ("absent", None),
        _ if reason.starts_with("unreadable") => ("unreadable", None),
        _ if reason.starts_with("repository ") => (
            "tree",
            reason
                .split_once("published repository tree ")
                .and_then(|(_, rest)| rest.split(" does not match").next())
                .map(|path| escape(OsStr::new(path))),
        ),
        _ if reason.starts_with("local repository ")
            || reason.contains(" local repositories replayed")
            || reason.starts_with("replay records") =>
        {
            ("replay", None)
        }
        _ if reason.starts_with("the workspace status command wrote") => ("status-stderr", None),
        _ if reason.starts_with("cannot capture stderr") => ("capture", None),
        _ if reason.contains("panicked") => ("panic", None),
        _ => ("other", None),
    }
}

/// The class of a full analysis's not-saved reason (crate::snapshot::save):
/// the first taint's name (`repository-rule-executed`, `network`, ...),
/// `failed` (the command failed), `capture`, `too-large`, `incomplete` or
/// `other`.
pub fn classify_not_saved(reason: &str) -> String {
    if let Some(rest) = reason.strip_prefix("taint ") {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if !name.is_empty() {
            return name;
        }
    }
    if reason.starts_with("the command failed") {
        "failed"
    } else if reason.contains("capture") {
        "capture"
    } else if reason.starts_with("planning printed more than")
        || reason.contains("larger than")
        || reason.contains("too large")
    {
        "too-large"
    } else if reason.starts_with("planning did not complete") {
        "incomplete"
    } else {
        "other"
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(1_792_800_000), "2026-10-24T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400 + 86_399), "2000-02-29T23:59:59Z");
    }

    /// A log rotates to `<name>.1` before it would pass its limit; the new
    /// line starts the next generation.
    #[test]
    fn logs_rotate_at_their_limit() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("snapshots").join(MISS_LOG);
        let line = |n: usize| format!("line {n} 0123456789\n");
        for n in 0..5 {
            append_bounded(&log, &line(n), 64).unwrap();
        }
        let current = std::fs::read_to_string(&log).unwrap();
        let rotated = std::fs::read_to_string(temp.path().join("snapshots/miss.log.1")).unwrap();
        assert_eq!(rotated, [line(0), line(1), line(2)].concat());
        assert_eq!(current, [line(3), line(4)].concat());
        // A line longer than the limit still lands (alone in its generation).
        let long = format!("{}\n", "y".repeat(100));
        append_bounded(&log, &long, 64).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), long);
    }

    #[test]
    fn miss_reasons_are_classified_with_their_path() {
        for (reason, class, path) in [
            ("no snapshot for this key", "absent", None),
            ("unreadable snapshot: bad header", "unreadable", None),
            ("unreadable plan: x", "unreadable", None),
            ("ENV W5_LOCAL: value changed", "env", None),
            (
                "READ /ws/BUILD.bazel: content changed",
                "read",
                Some("/ws/BUILD.bazel"),
            ),
            (
                "READDIR[glob:a b/*.txt@] /ws/data: listing changed",
                "readdir",
                Some("/ws/data"),
            ),
            (
                "STAT /ws/x\\x20y: recorded file, now missing",
                "stat",
                Some("/ws/x\\x20y"),
            ),
            (
                "INTEGRITY /c/repositories/direct/trees/ab/sub: lstat changed (repository x; recorded d:1, now d:2)",
                "integrity",
                Some("/c/repositories/direct/trees/ab/sub"),
            ),
            (
                "repository gen: published repository tree /c/repositories/direct/trees/a b does not match its manifest",
                "tree",
                Some("/c/repositories/direct/trees/a\\x20b"),
            ),
            (
                "local repository +local_rule+localrepo: the identity inputs differ",
                "replay",
                None,
            ),
            ("1 of 2 local repositories replayed", "replay", None),
            (
                "ENV X: value changed; and local repository +r+x: the identity inputs differ",
                "env",
                None,
            ),
            (
                "the workspace status command wrote 7 bytes to stderr (the snapshot is removed)",
                "status-stderr",
                None,
            ),
            ("cannot capture stderr for the replay: x", "capture", None),
            ("the validation panicked", "panic", None),
            ("something new", "other", None),
        ] {
            let (got, got_path) = classify_miss(reason);
            assert_eq!((got, got_path.as_deref()), (class, path), "{reason}");
        }
    }

    #[test]
    fn not_saved_reasons_are_classified() {
        for (reason, class) in [
            (
                "taint repository-rule-executed (1): +gen_rule+genrepo (0.4 ms)",
                "repository-rule-executed",
            ),
            (
                "taint lockfile-write-pending: MODULE.bazel.lock",
                "lockfile-write-pending",
            ),
            ("taint errors: 2 analysis error(s)", "errors"),
            ("the command failed", "failed"),
            ("the command failed (taint network (1): x)", "failed"),
            ("cannot capture stderr: x", "capture"),
            (
                "a process kept the captured stderr open after planning (a lingering child?)",
                "capture",
            ),
            ("planning printed more than 16777216 bytes", "too-large"),
            ("planning did not complete", "incomplete"),
            ("nothing was recorded", "other"),
        ] {
            assert_eq!(classify_not_saved(reason), class, "{reason}");
        }
    }

    /// Both logs: one binary-safe line per outcome, the targets last.
    #[test]
    fn lines_are_binary_safe() {
        let targets = vec!["//a:b".to_owned(), "//x y:\n\u{e9}".to_owned()];
        let line = outcome_line(
            "miss",
            "0011",
            "read",
            Some("/ws/BUILD"),
            "READ /ws/BUILD: content changed",
            "build",
            &targets,
        );
        assert!(
            line.ends_with(" kind=miss key=0011 command=build reason=read path=/ws/BUILD detail=READ\\x20/ws/BUILD:\\x20content\\x20changed targets=//a:b //x\\x20y:\\x0a\\xc3\\xa9\n"),
            "{line}"
        );
        assert_eq!(line.matches('\n').count(), 1);
        let long = "x".repeat(1000);
        let line = outcome_line("not-saved", "0011", "other", None, &long, "test", &targets);
        assert!(
            line.contains(&format!(" path=- detail={} targets=", "x".repeat(DETAIL))),
            "{line}"
        );
        let line = verify_line(
            "0022",
            "taint",
            2,
            Some("repository-rule-executed"),
            Some(OsStr::new("/c/repositories/direct/trees/a b")),
            "build",
            &targets,
        );
        assert!(
            line.ends_with(" key=0022 command=build first=taint differences=2 taint=repository-rule-executed path=/c/repositories/direct/trees/a\\x20b targets=//a:b //x\\x20y:\\x0a\\xc3\\xa9\n"),
            "{line}"
        );
        let line = verify_line("0022", "plan", 1, None, None, "build", &[]);
        assert!(
            line.ends_with(" first=plan differences=1 taint=- path=- targets=\n"),
            "{line}"
        );
    }
}
