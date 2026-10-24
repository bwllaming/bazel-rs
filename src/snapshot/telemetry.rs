//! The soak's logs below the snapshot store root (`<cache>/snapshots/`,
//! L2-W5g, L2-W5h): `verify.log` — one line per verify-mode mismatch
//! (crate::snapshot::verify), the details of each — and `events.log` — one
//! line per snapshot event of every command with the snapshot on, so the
//! soak has a denominator (L2-W5h; `miss.log` before, which held misses,
//! unsaved runs and heals only):
//!
//! | kind | when | `reason` | `ms` |
//! |---|---|---|---|
//! | `hit` | a lookup hit (crate::snapshot::check; in verify mode before the shadow analysis) | - | the attempt's wall, key included |
//! | `miss` | a lookup missed | its class ([`classify_miss`]) | the attempt's wall |
//! | `saved` | a full analysis was saved (crate::snapshot::save) | - | check point to rename |
//! | `not-saved` | a full analysis was not saved | its class ([`classify_not_saved`]) | - |
//! | `verified` | verify mode: the shadow analysis agreed (crate::snapshot::verify) | - | analysis and comparison |
//! | `mismatch` | verify mode: it disagreed (verify.log has the details) | the first difference's class | - |
//! | `healed` | a failed hit removed its snapshot (crate::snapshot::hit) | `<rule>:<entry>` | the trees' check |
//! | `evicted` | garbage collection removed a snapshot (crate::snapshot::store) | `count` or `bytes` | - |
//! | `recorded` | `BAZEL_RS_SNAPSHOT=record` reported (crate::snapshot::record) | eligible: `yes`, `no`, `unless-pruned` | - |
//!
//! A heal's rule is `store-path` (its error named a path in the repository
//! store) or `tree` (a tree of the snapshot no longer verified); its entry
//! is the class of the first differing entry
//! (crate::repository_store::TreeMismatch: `extra`, `missing`, `kind`,
//! `mode`, `content`, `link`, `manifest`, `unreadable`), whose path is the
//! line's `path`. An eviction names the evicted snapshot's key (its file
//! name) and, in `detail`, the key of the save that collected it, so a later
//! `absent` miss of that key is told apart from one never saved.
//!
//! Every line is binary-safe and space-separated, each field one token
//! (crate::observed_fs::escape; `-` when absent), the target patterns last:
//!
//! ```text
//! verify.log: <UTC time> <unix seconds> key=<short G> command=<command> first=<class> differences=<n> taint=<name> path=<path> targets=<pattern>...
//! events.log: <UTC time> <unix seconds> kind=<kind> mode=<on|verify|record> key=<short G> command=<command> reason=<class> path=<path> ms=<wall ms> detail=<text> targets=<pattern>...
//! ```
//!
//! scripts/snapshot_soak_report.py summarizes both. Each append is one
//! `write` (O_APPEND: concurrent commands never interleave within a line).
//! A log that would grow past [`LIMIT`] bytes is renamed to `<name>.1` first
//! (replacing the previous generation): at most about twice the limit per
//! log. Best effort: a failed write or rotation loses telemetry, never
//! fails the command; two commands rotating at once may lose a generation.

use crate::observed_fs::escape;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The verify-mode mismatch log.
pub const VERIFY_LOG: &str = "verify.log";
/// The event log (every hit, miss, save, unsaved run, verification, heal
/// and eviction).
pub const EVENTS_LOG: &str = "events.log";
/// A log's size before it rotates.
pub const LIMIT: u64 = 1 << 20;
/// Bytes of a reason kept in a `detail=` field (before escaping).
const DETAIL: usize = 240;

/// The `mode=` of `BAZEL_RS_SNAPSHOT=on`.
pub const MODE_ON: &str = "on";
/// The `mode=` of `BAZEL_RS_SNAPSHOT=verify`.
pub const MODE_VERIFY: &str = "verify";
/// The `mode=` of `BAZEL_RS_SNAPSHOT=record`.
pub const MODE_RECORD: &str = "record";

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

/// One `events.log` event (see the module docs): its kind, the snapshot's
/// key and what the kind names.
#[derive(Clone, Copy, Debug, Default)]
pub struct Event<'a> {
    pub kind: &'a str,
    pub key: &'a str,
    pub reason: Option<&'a str>,
    /// Already escaped (validation names its paths escaped).
    pub path: Option<&'a str>,
    pub ms: Option<f64>,
    /// Free text: its first [`DETAIL`] bytes, escaped.
    pub detail: Option<&'a str>,
}

/// An `events.log` line of `event` in `mode` for `command` and its target
/// patterns.
pub fn event_line(mode: &str, command: &str, targets: &[String], event: &Event<'_>) -> String {
    let field = |value: Option<&str>| {
        value
            .filter(|value| !value.is_empty())
            .unwrap_or("-")
            .to_owned()
    };
    format!(
        "{} kind={} mode={} key={} command={} reason={} path={} ms={} detail={} {}\n",
        now_fields(),
        event.kind,
        field(Some(mode)),
        field(Some(event.key)),
        token(Some(OsStr::new(command))),
        token(event.reason.map(OsStr::new)),
        field(event.path),
        event
            .ms
            .map_or_else(|| "-".to_owned(), |ms| format!("{ms:.1}")),
        event.detail.map_or_else(|| "-".to_owned(), detail),
        targets_field(targets),
    )
}

/// What an `events.log` line names besides its event: the cache whose store
/// holds the log, the snapshot mode, the command and its target patterns. A
/// context without a cache (the default) logs nothing.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub cache: PathBuf,
    /// [`MODE_ON`], [`MODE_VERIFY`] or [`MODE_RECORD`].
    pub mode: &'static str,
    pub command: String,
    pub targets: Vec<String>,
}

impl Context {
    /// Append `event` to this cache's event log (best effort).
    pub fn log(&self, event: &Event<'_>) {
        if !self.cache.as_os_str().is_empty() {
            let line = event_line(self.mode, &self.command, &self.targets, event);
            let _ = append(&log_path(&self.cache, EVENTS_LOG), &line);
        }
    }

    /// A lookup of the snapshot `key` hit after `ms` (the attempt's wall);
    /// `detail`: where the time went.
    pub fn log_hit(&self, key: &str, ms: f64, detail: &str) {
        self.log(&Event {
            kind: "hit",
            key,
            ms: Some(ms),
            detail: Some(detail),
            ..Event::default()
        });
    }

    /// A lookup of the snapshot `key` missed for `reason` after `ms`.
    pub fn log_miss(&self, key: &str, reason: &str, ms: f64) {
        let (class, path) = classify_miss(reason);
        self.log(&Event {
            kind: "miss",
            key,
            reason: Some(class),
            path: path.as_deref(),
            ms: Some(ms),
            detail: Some(reason),
        });
    }

    /// The full analysis of `key` was saved, `ms` after the check point.
    pub fn log_saved(&self, key: &str, ms: f64, detail: &str) {
        self.log(&Event {
            kind: "saved",
            key,
            ms: Some(ms),
            detail: Some(detail),
            ..Event::default()
        });
    }

    /// The full analysis of `key` was not saved, for `reason`.
    pub fn log_not_saved(&self, key: &str, reason: &str) {
        let class = classify_not_saved(reason);
        self.log(&Event {
            kind: "not-saved",
            key,
            reason: Some(&class),
            detail: Some(reason),
            ..Event::default()
        });
    }

    /// Verify mode: the shadow full analysis of the hit of `key` agreed,
    /// after `ms` (analysis and comparison).
    pub fn log_verified(&self, key: &str, ms: f64, detail: &str) {
        self.log(&Event {
            kind: "verified",
            key,
            ms: Some(ms),
            detail: Some(detail),
            ..Event::default()
        });
    }

    /// Verify mode: the shadow full analysis of the hit of `key` disagreed;
    /// `class`: the first difference's, `path`: the first differing path.
    pub fn log_mismatch(&self, key: &str, class: &str, path: Option<&OsStr>, detail: &str) {
        let path = path.map(escape);
        self.log(&Event {
            kind: "mismatch",
            key,
            reason: Some(class),
            path: path.as_deref(),
            detail: Some(detail),
            ..Event::default()
        });
    }

    /// A failed hit removed the snapshot of `key`: `rule` is `store-path`
    /// (its error named a path in the repository store) or `tree`; `entry`
    /// the class of the first differing entry, at `path`; `ms` what the
    /// check cost.
    pub fn log_healed(
        &self,
        key: &str,
        rule: &str,
        entry: &str,
        path: &Path,
        ms: f64,
        detail: &str,
    ) {
        let reason = format!("{rule}:{entry}");
        let path = escape(path.as_os_str());
        self.log(&Event {
            kind: "healed",
            key,
            reason: Some(&reason),
            path: Some(&path),
            ms: Some(ms),
            detail: Some(detail),
        });
    }

    /// Garbage collection removed the snapshot of `key` (at `path`): over
    /// the store's count (`count`) or byte budget (`bytes`).
    pub fn log_evicted(&self, key: &str, reason: &str, path: &Path, detail: &str) {
        let path = escape(path.as_os_str());
        self.log(&Event {
            kind: "evicted",
            key,
            reason: Some(reason),
            path: Some(&path),
            detail: Some(detail),
            ..Event::default()
        });
    }

    /// `BAZEL_RS_SNAPSHOT=record` reported: whether the run would be
    /// eligible to save (`yes`, `no`, `unless-pruned`) and why not.
    pub fn log_recorded(&self, eligible: &str, detail: &str) {
        self.log(&Event {
            kind: "recorded",
            reason: Some(eligible),
            detail: Some(detail),
            ..Event::default()
        });
    }
}

/// The class of a hit attempt's miss reason (crate::snapshot::hit) and the
/// path it names (escaped, as validation spells it), if any: `absent`,
/// `unreadable`, `env`, a validation operation in lower case (`read`,
/// `readdir`, `stat`, `lstat`, `canon`, `readlink`, `integrity`), `tree`
/// (a tree verified against its manifest did not match), `replay`,
/// `local-tree` (a local repository's published tree differs, L2-W5h; the
/// path of its first differing entry), `status-stderr`, `capture`, `panic`
/// or `other`.
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
        // `local repository <name>: its published tree differs from its
        // manifest: <entry class> <escaped path>` (L2-W5h).
        _ if reason.starts_with("local repository ")
            && reason
                .split("; and ")
                .next()
                .is_some_and(|first| first.contains(crate::snapshot::hit::LOCAL_TREE_DIFFERS)) =>
        {
            let first = reason.split("; and ").next().unwrap_or_default();
            let entry = first
                .split_once(crate::snapshot::hit::LOCAL_TREE_DIFFERS)
                .map(|(_, entry)| entry)
                .unwrap_or_default();
            (
                "local-tree",
                entry
                    .split(' ')
                    .nth(1)
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned),
            )
        }
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
        let log = temp.path().join("snapshots").join(EVENTS_LOG);
        let line = |n: usize| format!("line {n} 0123456789\n");
        for n in 0..5 {
            append_bounded(&log, &line(n), 64).unwrap();
        }
        let current = std::fs::read_to_string(&log).unwrap();
        let rotated = std::fs::read_to_string(temp.path().join("snapshots/events.log.1")).unwrap();
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
                "local repository +local_rule+tools: its published tree differs from its manifest: mode /c/repositories/direct/trees/ab/tool\\x20x.sh",
                "local-tree",
                Some("/c/repositories/direct/trees/ab/tool\\x20x.sh"),
            ),
            (
                "local repository +r+a: its published tree differs from its manifest: content /c/a (and 1 more); and local repository +r+b: the identity inputs differ",
                "local-tree",
                Some("/c/a"),
            ),
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
                "the stderr capture ended early (a caught panic?)",
                "capture",
            ),
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

    /// Both logs: one binary-safe line per event, the targets last.
    #[test]
    fn lines_are_binary_safe() {
        let targets = vec!["//a:b".to_owned(), "//x y:\n\u{e9}".to_owned()];
        let line = event_line(
            MODE_ON,
            "build",
            &targets,
            &Event {
                kind: "miss",
                key: "0011",
                reason: Some("read"),
                path: Some("/ws/BUILD"),
                ms: Some(12.345),
                detail: Some("READ /ws/BUILD: content changed"),
            },
        );
        assert!(
            line.ends_with(" kind=miss mode=on key=0011 command=build reason=read path=/ws/BUILD ms=12.3 detail=READ\\x20/ws/BUILD:\\x20content\\x20changed targets=//a:b //x\\x20y:\\x0a\\xc3\\xa9\n"),
            "{line}"
        );
        assert_eq!(line.matches('\n').count(), 1);
        let long = "x".repeat(1000);
        let line = event_line(
            MODE_VERIFY,
            "test",
            &targets,
            &Event {
                kind: "not-saved",
                key: "0011",
                reason: Some("other"),
                detail: Some(&long),
                ..Event::default()
            },
        );
        assert!(
            line.contains(&format!(
                " kind=not-saved mode=verify key=0011 command=test reason=other path=- ms=- detail={} targets=",
                "x".repeat(DETAIL)
            )),
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

    /// L2-W5h: every kind of event lands in `<cache>/snapshots/events.log`
    /// with its mode, key, reason, path and wall time; a context without a
    /// cache logs nothing.
    #[test]
    fn every_event_kind_is_logged() {
        let temp = tempfile::tempdir().unwrap();
        let log = Context {
            cache: temp.path().to_path_buf(),
            mode: MODE_ON,
            command: "build".to_owned(),
            targets: vec!["//:t".to_owned()],
        };
        log.log_hit("k1", 120.04, "validate:1.0");
        log.log_miss("k1", "READ /ws/BUILD: content changed", 40.0);
        log.log_saved("k1", 900.0, "bytes:4096");
        log.log_not_saved("k1", "taint network (1): x");
        log.log_healed(
            "k1",
            "tree",
            "mode",
            Path::new("/c/repositories/direct/trees/ab/tool.sh"),
            150.0,
            "mode of tool.sh",
        );
        log.log_evicted("k0", "count", Path::new("/c/snapshots/v1/k0.snap"), "by:k1");
        let verify = Context {
            mode: MODE_VERIFY,
            ..log.clone()
        };
        verify.log_verified("k1", 7000.0, "analysis:6900.0,compare:100.0");
        verify.log_mismatch(
            "k1",
            "taint",
            Some(OsStr::new("/c/x y")),
            "repository-rule-executed",
        );
        Context {
            mode: MODE_RECORD,
            ..log.clone()
        }
        .log_recorded("no", "taint network");
        Context::default().log_hit("ignored", 1.0, "");
        let text = std::fs::read_to_string(temp.path().join("snapshots").join(EVENTS_LOG)).unwrap();
        let fields: Vec<String> = text
            .lines()
            .map(|line| {
                line.split(' ')
                    .skip(2)
                    .filter(|field| {
                        ["kind=", "mode=", "key=", "reason=", "path=", "ms="]
                            .iter()
                            .any(|name| field.starts_with(name))
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(
            fields,
            [
                "kind=hit mode=on key=k1 reason=- path=- ms=120.0",
                "kind=miss mode=on key=k1 reason=read path=/ws/BUILD ms=40.0",
                "kind=saved mode=on key=k1 reason=- path=- ms=900.0",
                "kind=not-saved mode=on key=k1 reason=network path=- ms=-",
                "kind=healed mode=on key=k1 reason=tree:mode path=/c/repositories/direct/trees/ab/tool.sh ms=150.0",
                "kind=evicted mode=on key=k0 reason=count path=/c/snapshots/v1/k0.snap ms=-",
                "kind=verified mode=verify key=k1 reason=- path=- ms=7000.0",
                "kind=mismatch mode=verify key=k1 reason=taint path=/c/x\\x20y ms=-",
                "kind=recorded mode=record key=- reason=no path=- ms=-",
            ],
            "{text}"
        );
        assert!(
            text.lines().all(|line| line.ends_with(" targets=//:t")),
            "{text}"
        );
    }
}
