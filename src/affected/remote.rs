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
//! (OS and architecture, LFS smudging, rc files outside the workspace, the
//! variables an rc or command-line option names for inheritance) and the
//! epoch. An entry's Action Cache key is the
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
        eprintln!("ERROR: bazel-rs affected: {error}");
        return 2;
    }
    let mut path = request.as_os_str().to_owned();
    path.push(".out");
    match std::fs::write(&path, out) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
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
/// tree: runners of other platforms, or with other LFS smudging, must not
/// share (and overwrite) each other's entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HostState {
    pub os: String,
    pub arch: String,
    /// `GIT_LFS_SKIP_SMUDGE`.
    pub lfs_skip_smudge: Option<String>,
    /// A `git-lfs` executable on `PATH`.
    pub lfs_installed: bool,
    /// `git config --get-regexp '^filter\.'` of the repository (the LFS
    /// filter's configuration, and any other smudge filter).
    pub filters: String,
}

impl HostState {
    pub fn of(top: &Path) -> Self {
        let lfs_installed = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("git-lfs").is_file())
        });
        let filters =
            crate::spawn_lock::output(std::process::Command::new("git").arg("-C").arg(top).args([
                "config",
                "--get-regexp",
                "^filter\\.",
            ]))
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_else(|error| format!("unknown: {error}"));
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            lfs_skip_smudge: std::env::var("GIT_LFS_SKIP_SMUDGE").ok(),
            lfs_installed,
            filters,
        }
    }
}

/// The client state outside the tree that a planning reads: the rc files
/// outside the workspace `root` (system, home, `$BAZELRC`, `--bazelrc` and
/// their imports, by path and contents), and the values of the client
/// variables that an rc or command-line `--action_env`, `--host_action_env`,
/// `--test_env` or `--repo_env` names without a value (inherited), the
/// workspace's location in them normalized. (The target digests key an
/// inherited variable by name only; the remote entries are shared between
/// machines, so its value is keyed here. The rest of the client
/// environment is not: `PATH` holds per-runner paths on ~/code's CI.)
pub(super) fn environment_key(
    binary: &Path,
    startup: &[String],
    root: &Path,
    forwarded: &[String],
    host: &HostState,
) -> String {
    let mut hash = Sha256::new();
    frame(&mut hash, b"environment-v2");
    frame(&mut hash, host.os.as_bytes());
    frame(&mut hash, host.arch.as_bytes());
    match &host.lfs_skip_smudge {
        Some(value) => frame(&mut hash, format!("lfs-skip-smudge={value}").as_bytes()),
        None => frame(&mut hash, b"lfs-skip-smudge unset"),
    }
    frame(&mut hash, &[u8::from(host.lfs_installed)]);
    frame(&mut hash, host.filters.as_bytes());
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
    let mut options: Vec<String> = Vec::new();
    if !crate::bazelrc::search_nullary(startup, "ignore_all_rc_files", false) {
        match crate::bazelrc::read_rc_files(&line, &env, &mut Vec::new()) {
            Ok(files) => {
                let inside = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                let mut paths: Vec<&String> = files
                    .iter()
                    .flat_map(|file| &file.canonical_source_paths)
                    .filter(|path| !Path::new(path).starts_with(&inside))
                    .collect();
                paths.sort();
                paths.dedup();
                for path in paths {
                    frame(&mut hash, path.as_bytes());
                    match std::fs::read(path) {
                        Ok(bytes) => frame(&mut hash, &bytes),
                        Err(error) => frame(&mut hash, error.to_string().as_bytes()),
                    }
                }
                options.extend(
                    files
                        .iter()
                        .flat_map(|file| &file.options)
                        .map(|option| option.option.clone()),
                );
            }
            Err(error) => frame(&mut hash, error.as_bytes()),
        }
    }
    options.extend(forwarded.iter().cloned());
    let mut names: Vec<String> = Vec::new();
    let mut at = 0;
    while at < options.len() {
        let option = &options[at];
        at += 1;
        for name in ["action_env", "host_action_env", "test_env", "repo_env"] {
            let flag = format!("--{name}");
            let value = if *option == flag {
                options.get(at).cloned()
            } else {
                option
                    .strip_prefix(&flag)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::to_owned)
            };
            if let Some(value) = value
                && !value.contains('=')
            {
                names.push(value);
            }
        }
    }
    names.sort();
    names.dedup();
    for name in names {
        frame(&mut hash, name.as_bytes());
        match std::env::var_os(&name) {
            Some(value) => {
                frame(&mut hash, b"=");
                // The workspace's location is no input (the digests
                // normalize it too).
                frame(
                    &mut hash,
                    &super::normalized(
                        value.as_encoded_bytes(),
                        root.as_os_str().as_encoded_bytes(),
                    ),
                );
            }
            None => frame(&mut hash, b"unset"),
        }
    }
    hex(&hash.finalize().into())
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
    fn the_environment_key_covers_inherited_variables_and_outer_rc_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let binary = Path::new("/bin/true");
        let ignore = ["--ignore_all_rc_files".to_owned()];
        let key = |startup: &[String], forwarded: &[&str]| {
            let forwarded: Vec<String> = forwarded.iter().map(|arg| (*arg).to_owned()).collect();
            environment_key(binary, startup, &root, &forwarded, &HostState::default())
        };
        let plain = key(&ignore, &[]);
        assert_eq!(plain, key(&ignore, &["--define=a=b"]));
        // An inherited variable (unset here) is keyed; a value given is not
        // (the options are in the digests key).
        assert_ne!(
            plain,
            key(&ignore, &["--repo_env=BAZEL_RS_TEST_UNSET_VARIABLE"])
        );
        assert_eq!(plain, key(&ignore, &["--action_env=A=1"]));
        // An rc file outside the workspace, by contents.
        let rc = dir.path().join("outer.rc");
        std::fs::write(&rc, "build --define=x=1\n").unwrap();
        let with_rc = vec![
            "--nohome_rc".to_owned(),
            "--nosystem_rc".to_owned(),
            format!("--bazelrc={}", rc.display()),
        ];
        let first = key(&with_rc, &[]);
        std::fs::write(&rc, "build --define=x=2\n").unwrap();
        assert_ne!(first, key(&with_rc, &[]));
        // ... and the variables it names for inheritance.
        std::fs::write(&rc, "build --test_env=BAZEL_RS_TEST_UNSET_VARIABLE\n").unwrap();
        let named = key(&with_rc, &[]);
        std::fs::write(&rc, "build --test_env=BAZEL_RS_TEST_UNSET_VARIABLE\n").unwrap();
        assert_eq!(named, key(&with_rc, &[]));
        // The workspace's own rc file is the tree's (keyed by the tree).
        let inside = vec!["--nohome_rc".to_owned(), "--nosystem_rc".to_owned()];
        let before = key(&inside, &[]);
        std::fs::write(root.join(".bazelrc"), "build --define=y=1\n").unwrap();
        assert_eq!(before, key(&inside, &[]));
    }

    #[test]
    fn the_environment_key_covers_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let startup = ["--ignore_all_rc_files".to_owned()];
        let key = |host: &HostState| {
            environment_key(Path::new("/bin/true"), &startup, dir.path(), &[], host)
        };
        let base = HostState {
            os: "linux".into(),
            arch: "x86_64".into(),
            ..HostState::default()
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
                lfs_skip_smudge: Some("1".into()),
                ..base.clone()
            },
            HostState {
                lfs_skip_smudge: Some(String::new()),
                ..base.clone()
            },
            HostState {
                lfs_installed: true,
                ..base.clone()
            },
            HostState {
                filters: "filter.lfs.smudge git-lfs smudge -- %f\n".into(),
                ..base.clone()
            },
        ] {
            assert_ne!(plain, key(&other), "{other:?}");
        }
        // The host's own state is read.
        let host = HostState::of(dir.path());
        assert_eq!(host.os, std::env::consts::OS);
        assert_eq!(host.arch, std::env::consts::ARCH);
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
