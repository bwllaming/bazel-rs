//! Base digests shared through the configured remote cache
//! (docs/AFFECTED_TARGETS.md §8.11): one Action Cache entry per target of a
//! revision, written through by a `readwrite` `affected` that planned it
//! (a base, or a head in a clean worktree), read by the next one before it
//! plans its base.
//!
//! **Entries.** A revision's key (computed by the orchestrator,
//! [`super::remote_base_key`]) covers its commit's tree, the
//! workspace's directory in it, this binary, the command, options and
//! patterns, the client state outside the tree that the planning reads
//! (OS and architecture, what the checkout's filters do, the options of rc files outside the
//! workspace) and the epoch — none of it by location (§8.11, "Location
//! independence"). An entry's Action Cache key is the
//! sha256 of the framed bytes `bazel-rs-affected-digest-v1`, the base key
//! and the label — no digest of any `Action`, so genuine Bazel never looks
//! it up. Its ActionResult has one output file, [`RECORD_PATH`], whose
//! blob (uploaded to the CAS first, and inlined in the entry) is the
//! record:
//!
//! ```text
//! bazel-rs-affected-digest-v1
//! entry<TAB><the entry's Action Cache key>
//! label<TAB><label>
//! value<TAB><64 hex digits | incompatible | absent>
//! check<TAB><sha256 of the lines above>
//! ```
//!
//! `absent`: the base selects no such target (a head target missing at
//! the base is new). A failed target's digest is never stored. A record is
//! used only when every field checks out (the output file's digest, the
//! header, the entry key, the label, the checksum, the value's form);
//! anything else is a miss, analysed and written again.
//!
//! **Processes.** The orchestrator has no remote cache of its own: the
//! lookups and uploads run in a *helper* — this binary, with the
//! orchestrator's startup options, command and options, in the working
//! copy — which sets up the remote cache exactly as a planning does
//! (rc files, credential helpers, TLS) and then, under [`REMOTE_OP_VAR`],
//! performs the request instead of the command ([`remote_op`]).

use super::{frame, hex};
use crate::reapi::{self, Digest};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Names the request file of a helper process (see the module docs).
pub const REMOTE_OP_VAR: &str = "BAZEL_RS_AFFECTED_REMOTE_OP";

/// The first line of a request or response file.
const EXCHANGE_HEADER: &str = "bazel-rs-affected-remote-v1";

/// The first line of a record, and the domain of an entry's key.
const RECORD_HEADER: &str = "bazel-rs-affected-digest-v1";

/// The output file of an entry's ActionResult.
pub const RECORD_PATH: &str = "bazel-rs-affected-digest";

/// The value of a target the base does not select.
pub const ABSENT: &str = "absent";

/// Parallel requests of a helper.
const PARALLEL: usize = 16;

/// The Action Cache key of `label`'s entry under `base_key`.
fn entry_key(base_key: &str, label: &str) -> Digest {
    let mut bytes = Vec::new();
    for part in [
        RECORD_HEADER.as_bytes(),
        base_key.as_bytes(),
        label.as_bytes(),
    ] {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    Digest {
        hash: hex(&Sha256::digest(&bytes).into()),
        size: bytes.len() as u64,
    }
}

fn valid_value(value: &str) -> bool {
    value == ABSENT
        || value == super::INCOMPATIBLE
        || (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}

/// The record of `label`'s `value` in entry `entry`.
fn record(entry: &Digest, label: &str, value: &str) -> Vec<u8> {
    let body = format!(
        "{RECORD_HEADER}\nentry\t{}\nlabel\t{label}\nvalue\t{value}\n",
        entry.hash
    );
    let check = hex(&Sha256::digest(body.as_bytes()).into());
    format!("{body}check\t{check}\n").into_bytes()
}

/// The value a record holds for `label` in entry `entry`, if it checks
/// out.
fn parse_record(bytes: &[u8], entry: &Digest, label: &str) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let at = text.rfind("check\t")?;
    let (body, check) = text.split_at(at);
    let check = check.strip_prefix("check\t")?.strip_suffix('\n')?;
    if check != hex(&Sha256::digest(body.as_bytes()).into()) {
        return None;
    }
    let mut lines = body.lines();
    if lines.next() != Some(RECORD_HEADER)
        || lines.next()?.strip_prefix("entry\t")? != entry.hash
        || lines.next()?.strip_prefix("label\t")? != label
    {
        return None;
    }
    let value = lines.next()?.strip_prefix("value\t")?;
    (lines.next().is_none() && valid_value(value)).then(|| value.to_owned())
}

/// The ActionResult of an entry holding `record`.
fn entry_result(record: &[u8]) -> Vec<u8> {
    reapi::ActionResult {
        output_files: vec![reapi::OutputFile {
            path: RECORD_PATH.to_owned(),
            digest: Digest::of(record),
            is_executable: false,
            contents: record.to_vec(),
        }],
        ..reapi::ActionResult::default()
    }
    .encode()
}

/// What one lookup found.
enum Found {
    Value(String),
    Miss,
    Malformed,
}

fn lookup_one(
    cache: &crate::remote_cache::RemoteCache,
    base_key: &str,
    label: &str,
) -> Result<Found, String> {
    let key = entry_key(base_key, label);
    let Some(bytes) = cache
        .remote_action_result(&key, RECORD_PATH)
        .map_err(|error| error.to_string())?
    else {
        return Ok(Found::Miss);
    };
    let Ok(result) = reapi::ActionResult::decode(&bytes) else {
        return Ok(Found::Malformed);
    };
    let Some(file) = result
        .output_files
        .iter()
        .find(|file| file.path == RECORD_PATH)
        .filter(|_| result.exit_code == 0)
    else {
        return Ok(Found::Malformed);
    };
    let contents = if file.contents.is_empty() {
        match cache
            .remote_blob(&file.digest, &key)
            .map_err(|error| error.to_string())?
        {
            Some(bytes) => bytes,
            // The record was evicted: a miss.
            None => return Ok(Found::Miss),
        }
    } else {
        file.contents.clone()
    };
    if Digest::of(&contents) != file.digest {
        return Ok(Found::Malformed);
    }
    Ok(match parse_record(&contents, &key, label) {
        Some(value) => Found::Value(value),
        None => Found::Malformed,
    })
}

fn store_one(
    cache: &crate::remote_cache::RemoteCache,
    base_key: &str,
    label: &str,
    value: &str,
) -> Result<(), String> {
    let key = entry_key(base_key, label);
    let record = record(&key, label, value);
    let digest = Digest::of(&record);
    cache
        .remote_store(&key, &[(digest, &record)], &entry_result(&record))
        .map_err(|error| error.to_string())
}

/// `f` over `items` on [`PARALLEL`] threads until one fails: the results
/// so far (`None` for items not done) and the first error.
fn until_failure<T: Sync, R: Send>(
    items: &[T],
    f: impl Fn(&T) -> Result<R, String> + Sync,
) -> (Vec<Option<R>>, Option<String>) {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let error = std::sync::Mutex::new(None);
    let results: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..PARALLEL.min(items.len()) {
            scope.spawn(|| {
                while !failed.load(Ordering::Relaxed) {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(at) else {
                        break;
                    };
                    match f(item) {
                        Ok(value) => *results[at].lock().expect("result") = Some(value),
                        Err(text) => {
                            failed.store(true, Ordering::Relaxed);
                            error.lock().expect("error").get_or_insert(text);
                        }
                    }
                }
            });
        }
    });
    (
        results
            .into_iter()
            .map(|cell| cell.into_inner().expect("result"))
            .collect(),
        error.into_inner().expect("error"),
    )
}

/// A helper request.
#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    op: String,
    key: String,
    labels: Vec<String>,
    puts: Vec<(String, String)>,
}

fn parse_request(text: &str) -> Result<Request, String> {
    let mut lines = text.lines();
    if lines.next() != Some(EXCHANGE_HEADER) {
        return Err("not an affected remote request".to_owned());
    }
    let mut request = Request::default();
    for line in lines {
        match line.split_once('\t') {
            Some(("op", op)) => request.op = op.to_owned(),
            Some(("key", key)) => request.key = key.to_owned(),
            Some(("label", label)) => request.labels.push(label.to_owned()),
            Some(("put", rest)) => match rest.split_once('\t') {
                Some((label, value)) => request.puts.push((label.to_owned(), value.to_owned())),
                None => return Err(format!("bad request line {line:?}")),
            },
            _ => return Err(format!("bad request line {line:?}")),
        }
    }
    Ok(request)
}

/// One line of text (an error message in a response).
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Performs the request file `request` of a helper process (its remote
/// cache set up) and writes the response next to it (`<request>.out`).
/// Returns the exit code.
pub fn remote_op(request: &Path) -> i32 {
    let mut out = format!("{EXCHANGE_HEADER}\n");
    let result = (|| -> Result<(), String> {
        let text = std::fs::read_to_string(request)
            .map_err(|error| format!("cannot read {}: {error}", request.display()))?;
        let request = parse_request(&text)?;
        let cache = crate::remote_cache::active().filter(|cache| cache.has_remote());
        let flag = |value: bool| if value { "1" } else { "0" };
        // `--remote_accept_cached` and `--remote_upload_local_results`
        // govern action results: the orchestrator's mode decides here.
        out.push_str(&format!("remote\t{}\n", flag(cache.is_some())));
        let Some(cache) = cache else {
            return Ok(());
        };
        match request.op.as_str() {
            "probe" => {}
            "lookup" => {
                let (found, error) = until_failure(&request.labels, |label| {
                    lookup_one(cache, &request.key, label)
                });
                let mut malformed = 0;
                for (label, found) in request.labels.iter().zip(found) {
                    match found {
                        Some(Found::Value(value)) => {
                            out.push_str(&format!("hit\t{label}\t{value}\n"))
                        }
                        Some(Found::Malformed) => malformed += 1,
                        Some(Found::Miss) | None => {}
                    }
                }
                out.push_str(&format!("malformed\t{malformed}\n"));
                if let Some(error) = error {
                    out.push_str(&format!("error\t{}\n", one_line(&error)));
                }
            }
            "upload" => {
                let (stored, error) = until_failure(&request.puts, |(label, value)| {
                    if !valid_value(value) {
                        return Err(format!("{label}: not a storable value {value:?}"));
                    }
                    store_one(cache, &request.key, label, value)
                });
                let stored = stored.iter().filter(|done| done.is_some()).count();
                out.push_str(&format!("stored\t{stored}\n"));
                if let Some(error) = error {
                    out.push_str(&format!("error\t{}\n", one_line(&error)));
                }
            }
            other => return Err(format!("unknown affected remote request {other:?}")),
        }
        Ok(())
    })();
    if let Err(error) = &result {
        crate::eprint_line!("ERROR: bazel-rs affected: {error}");
        return 2;
    }
    let mut path = request.as_os_str().to_owned();
    path.push(".out");
    match std::fs::write(&path, out) {
        Ok(()) => 0,
        Err(error) => {
            crate::eprint_line!(
                "ERROR: bazel-rs affected: cannot write {}: {error}",
                Path::new(&path).display()
            );
            2
        }
    }
}

/// A helper's response.
#[derive(Debug, Default)]
pub(super) struct Response {
    /// A remote cache is configured.
    pub remote: bool,
    pub hits: BTreeMap<String, String>,
    pub malformed: usize,
    pub stored: usize,
    /// The first failed request (the requests after it were not made).
    pub error: Option<String>,
}

fn parse_response(text: &str) -> Result<Response, String> {
    let mut lines = text.lines();
    if lines.next() != Some(EXCHANGE_HEADER) {
        return Err("not an affected remote response".to_owned());
    }
    let mut response = Response::default();
    let number = |value: &str| {
        value
            .parse::<usize>()
            .map_err(|_| format!("bad response number {value:?}"))
    };
    for line in lines {
        match line.split_once('\t') {
            Some(("remote", value)) => response.remote = value == "1",
            Some(("hit", rest)) => match rest.split_once('\t') {
                Some((label, value)) if valid_value(value) => {
                    response.hits.insert(label.to_owned(), value.to_owned());
                }
                _ => return Err(format!("bad response line {line:?}")),
            },
            Some(("malformed", value)) => response.malformed = number(value)?,
            Some(("stored", value)) => response.stored = number(value)?,
            Some(("error", value)) => response.error = Some(value.to_owned()),
            _ => return Err(format!("bad response line {line:?}")),
        }
    }
    Ok(response)
}

/// Runs helper processes for the orchestrator.
pub(super) struct Helper<'a> {
    pub binary: &'a Path,
    pub startup: &'a [String],
    pub command: &'a str,
    pub forwarded: &'a [String],
    /// The working copy (its rc files configure the remote cache).
    pub root: &'a Path,
    /// Where the request and response files go.
    pub dir: &'a Path,
}

impl Helper<'_> {
    /// Runs request `op` (named `name` on disk) with `lines`.
    pub fn run(
        &self,
        name: &str,
        op: &str,
        key: &str,
        lines: impl IntoIterator<Item = String>,
    ) -> Result<Response, String> {
        let path = self.dir.join(format!("remote-{name}.request"));
        let mut text = format!("{EXCHANGE_HEADER}\nop\t{op}\nkey\t{key}\n");
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(&path, text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        let mut out = path.clone().into_os_string();
        out.push(".out");
        let _ = std::fs::remove_file(&out);
        let mut command = std::process::Command::new(self.binary);
        command
            .args(self.startup)
            .arg(self.command)
            .args(self.forwarded)
            .arg("--")
            .current_dir(self.root)
            .env(REMOTE_OP_VAR, &path)
            .env(super::QUIET_EVENTS_VAR, "1")
            .env_remove(super::DIGESTS_VAR)
            .env_remove(super::SCOPE_VAR)
            .env_remove(super::SAVE_VAR)
            .env_remove(super::INCLUDE_MANUAL_VAR)
            .stdin(std::process::Stdio::null());
        let output = crate::spawn_lock::output(&mut command)
            .map_err(|error| format!("cannot run {}: {error}", self.binary.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
            return Err(format!(
                "the remote cache helper failed ({}): {}",
                output.status,
                one_line(&tail.into_iter().rev().collect::<Vec<_>>().join(" "))
            ));
        }
        let text = std::fs::read_to_string(&out).map_err(|error| {
            format!(
                "the remote cache helper wrote no response {}: {error}",
                Path::new(&out).display()
            )
        })?;
        parse_response(&text)
    }
}

/// The host's state a checkout and its planning depend on beyond the
/// tree: runners of other platforms, or whose checkouts smudge other
/// content, must not share (and overwrite) each other's entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HostState {
    pub os: String,
    pub arch: String,
    /// What the checkout's filters do to the commit's content
    /// ([`checkout_filters`]): normalized, printed by the key debug.
    pub checkout: String,
}

impl HostState {
    /// The state for a checkout of `commit` of the repository at `top`.
    pub fn of(top: &Path, commit: &str) -> Self {
        let git = |args: &[&str]| -> Option<String> {
            crate::spawn_lock::output(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(top)
                    .args(args),
            )
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        let config = git(&["config", "--get-regexp", "^filter\\."]).unwrap_or_default();
        // The filter drivers the checkout's attributes name: the commit's
        // `.gitattributes` files, `$GIT_DIR/info/attributes` and
        // `core.attributesFile`.
        let mut attributes = git(&[
            "grep",
            "-h",
            "-I",
            "-E",
            "-o",
            "filter=[^[:space:]]+",
            commit,
            "--",
            ":(glob)**/.gitattributes",
        ])
        .unwrap_or_default();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let file = |path: &str| -> Option<std::path::PathBuf> {
            let path = path.trim();
            if path.is_empty() {
                return None;
            }
            Some(match (path.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ if Path::new(path).is_absolute() => std::path::PathBuf::from(path),
                _ => top.join(path),
            })
        };
        for path in [
            git(&["rev-parse", "--git-path", "info/attributes"]),
            git(&["config", "core.attributesFile"]),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(path) = file(&path)
                && let Ok(text) = std::fs::read_to_string(path)
            {
                attributes.push('\n');
                attributes.push_str(&text);
            }
        }
        let on_path = |name: &str| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
            })
        };
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            checkout: checkout_filters(
                &config,
                &attribute_filters(&attributes),
                std::env::var("GIT_LFS_SKIP_SMUDGE").ok().as_deref(),
                &on_path,
            ),
        }
    }
}

/// The filter drivers attribute lines name (`filter=NAME`).
fn attribute_filters(text: &str) -> std::collections::BTreeSet<String> {
    text.split_whitespace()
        .filter_map(|token| token.strip_prefix("filter="))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// git-lfs's boolean of `GIT_LFS_SKIP_SMUDGE`.
fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "t" | "true" | "yes" | "on" | "y"
    )
}

/// A filter command without its executable's directory (`/usr/bin/git-lfs
/// smudge -- %f` is `git-lfs smudge -- %f`).
fn command_text(command: &str) -> String {
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    let program = program.rsplit('/').next().unwrap_or(program);
    std::iter::once(program)
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a filter command can run: an absolute executable that exists,
/// else one on `PATH` (`git lfs ...` needs `git-lfs`).
fn command_runs(command: &str, on_path: &dyn Fn(&str) -> bool) -> bool {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    if program == "git"
        && let Some(sub) = words.next()
    {
        return on_path(&format!("git-{sub}"));
    }
    on_path(program)
}

/// What the checkout's filters do to the content of a tree whose
/// attributes name the drivers `used`, from `git config --get-regexp
/// '^filter\.'` (`config`): only what can change the checked-out bytes.
/// LFS is one fact — smudged or not (a driver configured, by
/// `process` or `smudge`, not skipped by `GIT_LFS_SKIP_SMUDGE`, and its
/// command available) — whatever the configuration's spelling
/// (`git-lfs filter-process` or a `smudge`/`clean` pair, the executable's
/// path, `required`). Another driver is its `smudge` and `process`
/// commands (without the executable's directory), or `unconfigured`.
/// `clean` and `required` change no checkout; a driver no attribute names
/// does nothing.
fn checkout_filters(
    config: &str,
    used: &std::collections::BTreeSet<String>,
    skip: Option<&str>,
    on_path: &dyn Fn(&str) -> bool,
) -> String {
    let mut drivers: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for line in config.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let Some(rest) = key.strip_prefix("filter.") else {
            continue;
        };
        let Some((name, variable)) = rest.rsplit_once('.') else {
            continue;
        };
        drivers
            .entry(name.to_owned())
            .or_default()
            .insert(variable.to_ascii_lowercase(), value.trim().to_owned());
    }
    let mut parts = Vec::new();
    for name in used {
        let driver = drivers.get(name);
        let get = |variable: &str| {
            driver
                .and_then(|driver| driver.get(variable))
                .filter(|value| !value.is_empty())
        };
        if name == "lfs" {
            let runs = get("process")
                .or_else(|| get("smudge"))
                .is_some_and(|command| command_runs(command, on_path));
            let skipped = skip.is_some_and(truthy);
            parts.push(
                if runs && !skipped {
                    "lfs smudged"
                } else {
                    "lfs not smudged"
                }
                .to_owned(),
            );
            continue;
        }
        match (get("smudge"), get("process")) {
            (None, None) => parts.push(format!("filter {name} unconfigured")),
            (smudge, process) => parts.push(format!(
                "filter {name} smudge={} process={}",
                smudge
                    .map(|command| command_text(command))
                    .unwrap_or_default(),
                process
                    .map(|command| command_text(command))
                    .unwrap_or_default()
            )),
        }
    }
    if parts.is_empty() {
        "no filters".to_owned()
    } else {
        parts.join("; ")
    }
}

/// rc options that change no target digest, left out of
/// [`environment_key`] (an outer rc file names per-runner caches): the
/// caches' locations and policies, console and progress output, resource
/// limits, report files and the remote cache's transport. Names without
/// the leading `--` (and `no` for booleans).
const UNKEYED_RC_OPTIONS: &[&str] = &[
    "repository_cache",
    "repo_contents_cache",
    "experimental_repository_cache_hardlinks",
    "disk_cache",
    "experimental_disk_cache_gc_idle_delay",
    "experimental_disk_cache_gc_max_age",
    "experimental_disk_cache_gc_max_size",
    "announce_rc",
    "color",
    "curses",
    "show_progress",
    "show_progress_rate_limit",
    "progress_report_interval",
    "terminal_columns",
    "ui_event_filters",
    "ui_actions_shown",
    "show_timestamps",
    "verbose_failures",
    "jobs",
    "loading_phase_threads",
    "local_resources",
    "local_cpu_resources",
    "local_ram_resources",
    "keep_going",
    "build_event_json_file",
    "build_event_text_file",
    "build_event_binary_file",
    "bes_backend",
    "bes_results_url",
    "bes_header",
    "bes_timeout",
    "profile",
    "memory_profile",
    "execution_log_json_file",
    "execution_log_binary_file",
    "execution_log_compact_file",
    "remote_download_outputs",
    "remote_download_minimal",
    "remote_download_toplevel",
    "remote_download_all",
    "remote_download_regex",
];

/// Whether an rc option (`--name[=value]`, `--noname`) is
/// [`UNKEYED_RC_OPTIONS`] or one of the remote cache's transport options.
fn unkeyed_rc_option(option: &str) -> bool {
    let Some(name) = option.strip_prefix("--") else {
        return false;
    };
    let name = name.split_once('=').map_or(name, |(name, _)| name);
    let known = |name: &str| {
        UNKEYED_RC_OPTIONS.contains(&name)
            || super::TRANSPORT_VALUE_OPTIONS.contains(&name)
            || super::TRANSPORT_BOOLEAN_OPTIONS.contains(&name)
    };
    known(name) || name.strip_prefix("no").is_some_and(known)
}

/// [`environment_key`] and what it covered (`BAZEL_RS_AFFECTED_KEY_DEBUG`).
pub(super) struct EnvironmentKey {
    pub key: String,
    /// One line per component: what, and its digest (never a value: rc
    /// options and variables may hold secrets).
    pub parts: Vec<String>,
}

fn short(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes).into())[..16].to_owned()
}

/// The client state outside the tree that a planning reads, independent of
/// where the checkout, the cache and the rc files lie:
///
/// * the host: OS, architecture and what the checkout's filters do
///   ([`HostState`], [`checkout_filters`]; the workspace's location
///   normalized);
/// * the options of the rc files outside the workspace `root` (system,
///   home, `$BAZELRC`, `--bazelrc` and their imports), by command and
///   option text with the workspace's location normalized — not by path,
///   and without the options that change no digest ([`UNKEYED_RC_OPTIONS`],
///   the transport options: a runner's `~/.bazelrc` names its own
///   `--repository_cache`). The workspace's own rc files are the tree's;
///
/// Client variables are not keyed: the target digests key an inherited
/// variable (`--action_env=NAME`, `--test_env=NAME`) by name only, so a
/// reader computes the same head digests whatever its values, and ~/code's
/// CI holds per-runner paths in them (`PATH`, `KUBECONFIG` under
/// `test:replay --test_env=...`, exported by direnv). A variable a
/// repository rule reads (`--repo_env=NAME`) reaches the digests through
/// the repository's contents.
pub(super) fn environment_key(
    binary: &Path,
    startup: &[String],
    root: &Path,
    host: &HostState,
) -> EnvironmentKey {
    let root_bytes = root.as_os_str().as_encoded_bytes();
    let inside = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let inside_bytes = inside.as_os_str().as_encoded_bytes().to_vec();
    // The workspace's location, as given and canonical, is no input.
    let relative = |bytes: &[u8]| -> Vec<u8> {
        super::normalized(&super::normalized(bytes, root_bytes), &inside_bytes)
    };
    let mut hash = Sha256::new();
    let mut parts = Vec::new();
    frame(&mut hash, b"environment-v5");
    frame(&mut hash, host.os.as_bytes());
    frame(&mut hash, host.arch.as_bytes());
    let checkout = relative(host.checkout.as_bytes());
    frame(&mut hash, &checkout);
    parts.push(format!(
        "host {} {} checkout: {}",
        host.os,
        host.arch,
        String::from_utf8_lossy(&checkout)
    ));
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let env = crate::bazelrc::ClientEnv {
        cwd: root.to_string_lossy().into_owned(),
        workspace: root.to_string_lossy().into_owned(),
        home: var("HOME"),
        bazelrc_env: var("BAZELRC"),
        path_to_binary: binary.to_string_lossy().into_owned(),
        system_rc: crate::bazelrc::SYSTEM_BAZELRC.to_owned(),
        isatty: false,
        terminal_columns: 0,
        emacs: false,
    };
    let line = crate::bazelrc::CommandLine {
        startup_args: startup.to_vec(),
        ..Default::default()
    };
    if !crate::bazelrc::search_nullary(startup, "ignore_all_rc_files", false) {
        match crate::bazelrc::read_rc_files(&line, &env, &mut Vec::new()) {
            Ok(files) => {
                let mut kept = 0usize;
                let mut dropped = 0usize;
                for option in files.iter().flat_map(|file| &file.options) {
                    if Path::new(&option.source).starts_with(&inside) {
                        continue;
                    }
                    if unkeyed_rc_option(&option.option) {
                        dropped += 1;
                        continue;
                    }
                    kept += 1;
                    let text = relative(option.option.as_bytes());
                    frame(&mut hash, b"rc");
                    frame(&mut hash, option.command.as_bytes());
                    frame(&mut hash, &text);
                    let name = option
                        .option
                        .split_once('=')
                        .map_or(option.option.as_str(), |(name, _)| name);
                    parts.push(format!(
                        "rc {}:{} {} {name} {}",
                        option.source,
                        option.line,
                        option.command,
                        short(&text)
                    ));
                }
                parts.push(format!(
                    "rc outside the workspace: {kept} option(s) keyed, {dropped} unkeyed"
                ));
            }
            Err(error) => {
                let text = relative(error.as_bytes());
                frame(&mut hash, b"rc error");
                frame(&mut hash, &text);
                parts.push(format!("rc error {}", short(&text)));
            }
        }
    }
    EnvironmentKey {
        key: hex(&hash.finalize().into()),
        parts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_tampering() {
        let key = entry_key("k", "//a:b");
        let value = "ab".repeat(32);
        let bytes = record(&key, "//a:b", &value);
        assert_eq!(parse_record(&bytes, &key, "//a:b"), Some(value.clone()));
        // Another label, another entry, a changed value or checksum.
        assert_eq!(parse_record(&bytes, &key, "//a:c"), None);
        assert_eq!(
            parse_record(&bytes, &entry_key("k2", "//a:b"), "//a:b"),
            None
        );
        let text = String::from_utf8(bytes.clone()).unwrap();
        let tampered = text.replace(&value, &"cd".repeat(32));
        assert_eq!(parse_record(tampered.as_bytes(), &key, "//a:b"), None);
        assert_eq!(parse_record(&bytes[..bytes.len() - 2], &key, "//a:b"), None);
        // A record whose checksum is right but whose value is no digest.
        let bad = record(&key, "//a:b", "failed");
        assert_eq!(parse_record(&bad, &key, "//a:b"), None);
        for value in [ABSENT, super::super::INCOMPATIBLE] {
            let bytes = record(&key, "//a:b", value);
            assert_eq!(parse_record(&bytes, &key, "//a:b").as_deref(), Some(value));
        }
    }

    #[test]
    fn entry_keys_separate_bases_and_labels() {
        assert_eq!(entry_key("k", "//a:b"), entry_key("k", "//a:b"));
        assert_ne!(entry_key("k", "//a:b"), entry_key("k", "//a:c"));
        assert_ne!(entry_key("k", "//a:b"), entry_key("k2", "//a:b"));
        // Framed: no concatenation ambiguity.
        assert_ne!(entry_key("ka", "b"), entry_key("k", "ab"));
    }

    #[test]
    fn entries_decode_as_action_results() {
        let key = entry_key("k", "//a:b");
        let bytes = record(&key, "//a:b", ABSENT);
        let result = reapi::ActionResult::decode(&entry_result(&bytes)).unwrap();
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(result.output_files[0].path, RECORD_PATH);
        assert_eq!(result.output_files[0].digest, Digest::of(&bytes));
        assert_eq!(result.output_files[0].contents, bytes);
    }

    #[test]
    fn the_environment_key_covers_outer_rc_options_wherever_they_lie() {
        let dir = tempfile::tempdir().unwrap();
        let binary = Path::new("/bin/true");
        // Two checkouts, two homes (runners with per-runner data dirs).
        let key = |runner: &str, rc_text: &str| {
            let base = dir.path().join(runner);
            let root = base.join("work/code");
            std::fs::create_dir_all(&root).unwrap();
            let rc = base.join("home.bazelrc");
            std::fs::write(&rc, rc_text.replace("@ROOT@", &root.display().to_string())).unwrap();
            let startup = vec![
                "--nohome_rc".to_owned(),
                "--nosystem_rc".to_owned(),
                format!("--bazelrc={}", rc.display()),
            ];
            environment_key(binary, &startup, &root, &HostState::default()).key
        };
        let text = "build --define=x=1\n";
        // By options, not by the rc file's path.
        assert_eq!(key("data01", text), key("data02", text));
        assert_ne!(key("data01", text), key("data01", "build --define=x=2\n"));
        // The command a line is for counts.
        assert_ne!(key("data01", text), key("data01", "test --define=x=1\n"));
        // Per-runner caches and console options change no digest.
        let caches = |runner: &str| {
            format!(
                "{text}build --repository_cache=/runner/{runner}/cache/repos\nbuild --disk_cache=/runner/{runner}/disk\ncommon --color=yes\nbuild --remote_cache=grpc://{runner}\nbuild --noremote_upload_local_results\n"
            )
        };
        assert_eq!(
            key("data01", &caches("data01")),
            key("data02", &caches("data02"))
        );
        assert_eq!(key("data01", &caches("data01")), key("data01", text));
        // The workspace's location in an option is normalized.
        let local = "build --define=tool=@ROOT@/tools/x\n";
        assert_eq!(key("data01", local), key("data02", local));
        // An inheriting option is keyed by its text, never by the
        // variable's value (the digests key the name only).
        assert_ne!(
            key("data01", text),
            key("data01", &format!("{text}test:replay --test_env=PATH\n"))
        );
        // The workspace's own rc file is the tree's (keyed by the tree).
        let root = dir.path().join("data01/work/code");
        let inside = vec!["--nohome_rc".to_owned(), "--nosystem_rc".to_owned()];
        let before = environment_key(binary, &inside, &root, &HostState::default()).key;
        std::fs::write(root.join(".bazelrc"), "build --define=y=1\n").unwrap();
        assert_eq!(
            before,
            environment_key(binary, &inside, &root, &HostState::default()).key
        );
    }

    #[test]
    fn the_environment_key_covers_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let startup = ["--ignore_all_rc_files".to_owned()];
        let key = |host: &HostState| {
            environment_key(Path::new("/bin/true"), &startup, dir.path(), host).key
        };
        let base = HostState {
            os: "linux".into(),
            arch: "x86_64".into(),
            checkout: "lfs smudged".into(),
        };
        let plain = key(&base);
        assert_eq!(plain, key(&base.clone()));
        for other in [
            HostState {
                os: "macos".into(),
                ..base.clone()
            },
            HostState {
                arch: "aarch64".into(),
                ..base.clone()
            },
            HostState {
                checkout: "lfs not smudged".into(),
                ..base.clone()
            },
        ] {
            assert_ne!(plain, key(&other), "{other:?}");
        }
        // The host's own state is read.
        let host = HostState::of(dir.path(), "HEAD");
        assert_eq!(host.os, std::env::consts::OS);
        assert_eq!(host.arch, std::env::consts::ARCH);
    }

    #[test]
    fn checkout_filters_key_what_changes_the_checkout() {
        let used = |names: &[&str]| -> std::collections::BTreeSet<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        let lfs = used(&["lfs"]);
        let everywhere = |name: &str| name == "git-lfs" || name == "crypt-tool";
        let nowhere = |_: &str| false;
        // Cosmetic variants of an active LFS filter: one fact.
        let variants = [
            "filter.lfs.process git-lfs filter-process\nfilter.lfs.required true\nfilter.lfs.clean git-lfs clean -- %f\nfilter.lfs.smudge git-lfs smudge -- %f\n",
            "filter.lfs.smudge git-lfs smudge -- %f\nfilter.lfs.clean git-lfs clean -- %f\n",
            "filter.lfs.process git lfs filter-process\n",
            "filter.lfs.process /bin/sh filter-process\nfilter.lfs.required false\n",
        ];
        for config in variants {
            assert_eq!(
                checkout_filters(config, &lfs, None, &everywhere),
                "lfs smudged",
                "{config}"
            );
        }
        // Skipped, unavailable or unconfigured: not smudged.
        let config = variants[0];
        for skip in ["1", "true", "YES", "on"] {
            assert_eq!(
                checkout_filters(config, &lfs, Some(skip), &everywhere),
                "lfs not smudged"
            );
        }
        for skip in ["0", "false", ""] {
            assert_eq!(
                checkout_filters(config, &lfs, Some(skip), &everywhere),
                "lfs smudged"
            );
        }
        assert_eq!(
            checkout_filters(config, &lfs, None, &nowhere),
            "lfs not smudged"
        );
        assert_eq!(
            checkout_filters(
                "filter.lfs.process /nonexistent/git-lfs filter-process\n",
                &lfs,
                None,
                &everywhere
            ),
            "lfs not smudged"
        );
        assert_eq!(
            checkout_filters("", &lfs, None, &everywhere),
            "lfs not smudged"
        );
        // A tree naming no driver: nothing, whatever is configured.
        assert_eq!(
            checkout_filters(config, &used(&[]), None, &everywhere),
            "no filters"
        );
        // Another driver: its smudge and process commands, without the
        // executable's directory; `clean` and `required` change nothing.
        let crypt = used(&["crypt"]);
        let a = checkout_filters(
            "filter.crypt.smudge /opt/bin/crypt-tool smudge %f\nfilter.crypt.clean crypt-tool clean %f\nfilter.crypt.required true\n",
            &crypt,
            None,
            &everywhere,
        );
        let b = checkout_filters(
            "filter.crypt.smudge crypt-tool smudge %f\n",
            &crypt,
            None,
            &everywhere,
        );
        assert_eq!(a, b);
        assert_eq!(a, "filter crypt smudge=crypt-tool smudge %f process=");
        assert_ne!(
            a,
            checkout_filters(
                "filter.crypt.smudge crypt-tool smudge --other %f\n",
                &crypt,
                None,
                &everywhere
            )
        );
        assert_eq!(
            checkout_filters("", &crypt, None, &everywhere),
            "filter crypt unconfigured"
        );
        assert_eq!(
            attribute_filters(
                "*.bin filter=lfs diff=lfs merge=lfs -text\n*.x filter=crypt\n*.y -filter\n"
            ),
            used(&["crypt", "lfs"])
        );
    }

    /// Two repositories whose filter configuration differs only cosmetically
    /// key alike; LFS skipped or not does not.
    #[test]
    fn cosmetic_filter_configuration_differences_key_alike() {
        let dir = tempfile::tempdir().unwrap();
        let git = |repo: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", dir.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        let repo = |name: &str, config: &[(&str, &str)]| {
            let repo = dir.path().join(name);
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            std::fs::write(
                repo.join(".gitattributes"),
                "*.bin filter=lfs diff=lfs merge=lfs -text\n",
            )
            .unwrap();
            std::fs::create_dir_all(repo.join("sub")).unwrap();
            std::fs::write(repo.join("sub/.gitattributes"), "*.x filter=crypt\n").unwrap();
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "x"]);
            for (key, value) in config {
                git(&repo, &["config", key, value]);
            }
            repo
        };
        let one = repo(
            "one",
            &[
                ("filter.lfs.process", "git-lfs filter-process"),
                ("filter.lfs.required", "true"),
                ("filter.crypt.smudge", "/opt/bin/crypt-tool smudge %f"),
            ],
        );
        let two = repo(
            "two",
            &[
                ("filter.lfs.smudge", "git-lfs smudge -- %f"),
                ("filter.lfs.clean", "git-lfs clean -- %f"),
                ("filter.crypt.smudge", "crypt-tool smudge %f"),
                ("filter.unused.smudge", "anything"),
            ],
        );
        let state = |repo: &Path| HostState::of(repo, "HEAD");
        let (a, b) = (state(&one), state(&two));
        assert_eq!(a, b);
        assert!(
            a.checkout
                .contains("filter crypt smudge=crypt-tool smudge %f"),
            "{a:?}"
        );
        assert!(a.checkout.contains("; lfs "), "{a:?}");
        // Whether LFS smudges here depends on a git-lfs on PATH; either
        // way, skipping it is another key when it would smudge.
        let config = "filter.lfs.process git-lfs filter-process\n";
        let lfs: std::collections::BTreeSet<String> = ["lfs".to_owned()].into();
        let available = |name: &str| name == "git-lfs";
        assert_ne!(
            checkout_filters(config, &lfs, None, &available),
            checkout_filters(config, &lfs, Some("1"), &available)
        );
    }

    #[test]
    fn requests_and_responses_parse() {
        let request = parse_request(&format!(
            "{EXCHANGE_HEADER}\nop\tupload\nkey\tk\nput\t//a:b\tabsent\nlabel\t//c:d\n"
        ))
        .unwrap();
        assert_eq!(request.op, "upload");
        assert_eq!(request.puts, [("//a:b".to_owned(), ABSENT.to_owned())]);
        assert_eq!(request.labels, ["//c:d"]);
        assert!(parse_request("x\n").is_err());
        let response = parse_response(&format!(
            "{EXCHANGE_HEADER}\nremote\t1\nhit\t//a:b\tincompatible\nmalformed\t2\nerror\tdown\n"
        ))
        .unwrap();
        assert!(response.remote);
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.malformed, 2);
        assert_eq!(response.error.as_deref(), Some("down"));
        assert!(parse_response(&format!("{EXCHANGE_HEADER}\nhit\t//a:b\tfailed\n")).is_err());
    }
}
