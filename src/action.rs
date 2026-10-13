//! Native action execution for an initial Bazel subset, not full Bazel.
//!
//! A fresh working directory is isolation for build state, NOT a security
//! sandbox. Arbitrary shell commands can access the host, network, clock, and
//! absolute paths; they are not hermetic. Sources and the cache must not be
//! modified by an adversary racing filesystem checks. Symlink checks prevent
//! accidental escapes, but are not a substitute for OS sandboxing.
//!
//! No caller environment is inherited: otherwise secrets could leak and an
//! unkeyed environment variable could silently change a cached result. TMPDIR
//! defaults to a fresh absolute directory; its spelling (and the working directory's)
//! is intentionally not stable, so actions must not embed these paths in output.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const FORMAT: &str = "native-action-v5";
// Bazel 9.3 OutputPermissions.READONLY: all completed regular action outputs,
// including non-executable writes, normalize after execution, not during it.
const OUTPUT_MODE: u32 = 0o555;
const SHELL: &str = "/bin/bash";
const SHELL_FLAGS: [&str; 4] = ["-e", "-o", "pipefail", "-c"];
const LOG_TAIL_BYTES: usize = 64 * 1024;
const ENV: [(&str, &str); 3] = [("PATH", "/usr/bin:/bin"), ("LANG", "C"), ("LC_ALL", "C")];
const SPAWN_RETRY_DELAYS_MS: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];
static NEXT_WORK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct Action {
    pub id: String,
    pub command: String,
    pub inputs: Vec<(PathBuf, PathBuf)>,
    pub outputs: Vec<PathBuf>,
    /// Native write, mutually exclusive with `program` and shell commands.
    pub write: Option<WriteAction>,
    /// Direct executable; `None` selects Bash unless this is a native write.
    pub program: Option<String>,
    /// Literal process arguments, or shell positional parameters starting at `$1`.
    pub arguments: Vec<String>,
    /// Explicit overrides applied after controlled defaults, including TMPDIR.
    pub environment: BTreeMap<String, String>,
    /// Enable genrule-style errexit and pipefail; ignored for writes/direct runs.
    pub shell_strict: bool,
}

#[derive(Clone, Debug)]
pub struct WriteAction {
    pub content: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug)]
pub struct ActionResult {
    pub outputs: Vec<PathBuf>,
    pub cache_hit: bool,
}

struct Input {
    bytes: Vec<u8>,
    digest: String,
    executable: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    key: String,
    outputs: Vec<Output>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    // Hex encoding supports even non-UTF-8 Unix filenames.
    name: String,
    digest: String,
    executable: u32,
}

struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn io_error(context: &str, path: &Path, error: io::Error) -> String {
    format!("{context} '{}': {error}", path.display())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// Length-prefix every field, including list lengths, to avoid concatenation
// collisions and make the key independent of input/output declaration order.
fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn validate_relative(path: &Path) -> Result<(), String> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes
            .split(|&b| b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b"..")
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!(
            "unsafe mapped path '{}' (expected relative normal components)",
            path.display()
        ));
    }
    Ok(())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

// Check all ancestors, not just the leaf: dir/link/file must not escape via
// dir/link. This intentionally rejects source paths through symlink directories.
fn regular_file(path: &Path) -> Result<fs::Metadata, String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&prefix).map_err(|e| io_error("cannot inspect", &prefix, e))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symlink is not allowed: '{}'", prefix.display()));
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    Ok(metadata)
}

fn read_input(path: &Path) -> Result<Input, String> {
    let metadata = regular_file(path)?;
    let bytes = fs::read(path).map_err(|e| io_error("cannot read", path, e))?;
    Ok(Input {
        digest: digest(&bytes),
        bytes,
        executable: metadata.permissions().mode() & 0o111,
    })
}

type PreparedAction = (BTreeMap<PathBuf, Input>, Vec<PathBuf>, String);

/// Validate commands and mapped input/output namespaces without filesystem I/O.
///
/// Writes require one output and no program, command, arguments, environment, or inputs.
/// Their content is arbitrary bytes, including NULs, with no argv size limit.
/// Direct runs require a nonempty, NUL-free program and an empty shell command.
/// Empty output lists are permitted for process actions. Duplicate input names
/// are permitted here; preparation checks that their contents and modes agree.
pub fn validate_structure(action: &Action) -> Result<(), String> {
    if action.write.is_some()
        && (!action.command.is_empty()
            || action.program.is_some()
            || !action.arguments.is_empty()
            || !action.environment.is_empty()
            || !action.inputs.is_empty()
            || action.outputs.len() != 1)
    {
        return Err("write action requires exactly one output, no program, and empty command, arguments, environment, and inputs".into());
    }
    if let Some(program) = &action.program {
        if program.is_empty() || program.as_bytes().contains(&0) {
            return Err("direct program must be nonempty and contain no NUL byte".into());
        }
        if !action.command.is_empty() {
            return Err("direct action requires an empty shell command".into());
        }
    }
    if action.command.as_bytes().contains(&0) {
        return Err("command contains a NUL byte".into());
    }
    for argument in &action.arguments {
        if argument.as_bytes().contains(&0) {
            return Err("argument contains a NUL byte".into());
        }
    }
    for (name, value) in &action.environment {
        if name.is_empty() || name.as_bytes().contains(&b'=') || name.as_bytes().contains(&0) {
            return Err("environment key must be nonempty and contain neither '=' nor NUL".into());
        }
        if value.as_bytes().contains(&0) {
            return Err("environment value contains a NUL byte".into());
        }
    }
    // Validate the complete namespace before reading inputs or invoking processes.
    let mut input_names: Vec<PathBuf> = Vec::new();
    for (_, destination) in &action.inputs {
        validate_relative(destination)?;
        if input_names
            .iter()
            .any(|p| p != destination && overlaps(p, destination))
        {
            return Err(format!(
                "overlapping input destination '{}'",
                destination.display()
            ));
        }
        input_names.push(destination.clone());
    }
    let mut outputs: Vec<PathBuf> = Vec::new();
    for output in &action.outputs {
        validate_relative(output)?;
        if input_names
            .iter()
            .chain(outputs.iter())
            .any(|p| overlaps(p, output))
        {
            return Err(format!(
                "overlapping input/output path '{}'",
                output.display()
            ));
        }
        outputs.push(output.clone());
    }
    Ok(())
}

fn prepare(action: &Action) -> Result<PreparedAction, String> {
    validate_structure(action)?;
    let mut outputs = action.outputs.clone();
    outputs.sort();
    let mut inputs: BTreeMap<PathBuf, Input> = BTreeMap::new();
    for (source, destination) in &action.inputs {
        let input = read_input(source)?;
        if let Some(previous) = inputs.get(destination) {
            if previous.digest != input.digest || previous.executable != input.executable {
                return Err(format!(
                    "conflicting duplicate input destination '{}'",
                    destination.display()
                ));
            }
        } else {
            inputs.insert(destination.clone(), input);
        }
    }
    let mut hash = Sha256::new();
    for value in [FORMAT, std::env::consts::OS, std::env::consts::ARCH] {
        frame(&mut hash, value.as_bytes());
    }
    for (name, value) in ENV {
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, value.as_bytes());
    }
    frame(&mut hash, b"TMPDIR=fresh-absolute-work-tmp");
    frame(&mut hash, action.id.as_bytes());
    match (&action.write, &action.program) {
        (Some(write), _) => {
            frame(&mut hash, b"write");
            frame(&mut hash, &write.content);
            frame(&mut hash, &[u8::from(write.executable)]);
        }
        (None, Some(program)) => {
            frame(&mut hash, b"direct");
            frame(&mut hash, program.as_bytes());
        }
        (None, None) => {
            frame(&mut hash, b"shell");
            frame(&mut hash, SHELL.as_bytes());
            frame(&mut hash, &[u8::from(action.shell_strict)]);
            let flags: &[&str] = if action.shell_strict {
                &SHELL_FLAGS
            } else {
                &["-c"]
            };
            frame(&mut hash, &(flags.len() as u64).to_le_bytes());
            for flag in flags {
                frame(&mut hash, flag.as_bytes());
            }
        }
    }
    frame(&mut hash, action.command.as_bytes());
    frame(&mut hash, &(action.arguments.len() as u64).to_le_bytes());
    for argument in &action.arguments {
        frame(&mut hash, argument.as_bytes());
    }
    frame(&mut hash, &(action.environment.len() as u64).to_le_bytes());
    for (name, value) in &action.environment {
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, value.as_bytes());
    }
    frame(&mut hash, &(inputs.len() as u64).to_le_bytes());
    for (name, input) in &inputs {
        frame(&mut hash, name.as_os_str().as_bytes());
        frame(&mut hash, input.digest.as_bytes());
        frame(&mut hash, &input.executable.to_le_bytes());
    }
    frame(&mut hash, &(outputs.len() as u64).to_le_bytes());
    for name in &outputs {
        frame(&mut hash, name.as_os_str().as_bytes());
    }
    Ok((inputs, outputs, hex(&hash.finalize())))
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| io_error("cannot create directory", path, e))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect directory", path, e))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("expected real directory: '{}'", path.display()));
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8], executable: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent)?;
    }
    fs::write(path, bytes).map_err(|e| io_error("cannot write", path, e))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644 | executable))
        .map_err(|e| io_error("cannot set permissions", path, e))
}

fn cache_valid(directory: &Path, key: &str, outputs: &[PathBuf]) -> bool {
    let manifest_path = directory.join("manifest.json");
    let check = || -> Result<bool, String> {
        regular_file(&manifest_path)?;
        let bytes = fs::read(&manifest_path)
            .map_err(|e| io_error("cannot read manifest", &manifest_path, e))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if manifest.format != FORMAT
            || manifest.key != key
            || manifest.outputs.len() != outputs.len()
        {
            return Ok(false);
        }
        for (entry, name) in manifest.outputs.iter().zip(outputs) {
            if entry.name != hex(name.as_os_str().as_bytes()) || entry.executable != 0o111 {
                return Ok(false);
            }
            let path = directory.join("outputs").join(name);
            if regular_file(&path)?.permissions().mode() & 0o7777 != OUTPUT_MODE {
                return Ok(false);
            }
            let actual = read_input(&path)?;
            if actual.digest != entry.digest || actual.executable != entry.executable {
                return Ok(false);
            }
        }
        Ok(true)
    };
    check().unwrap_or(false)
}

fn fresh_work(parent: &Path, key: &str) -> Result<WorkDir, String> {
    loop {
        let n = NEXT_WORK.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{key}-{}-{n}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(WorkDir(path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("cannot create work directory", &path, e)),
        }
    }
}

fn remove_stale(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            result.map_err(|e| io_error("cannot remove invalid cache entry", path, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("cannot inspect cache entry", path, e)),
    }
}

// Read through the original file handle, not an action-controlled path. Limit
// reads even if a background child continues appending after the shell exits.
fn log_tail(file: &mut File) -> io::Result<String> {
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LOG_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(LOG_TAIL_BYTES as u64).read_to_end(&mut bytes)?;
    let mut result = String::new();
    if start != 0 {
        result.push_str("[truncated: showing last 65536 bytes]\n");
    }
    result.push_str(&String::from_utf8_lossy(&bytes));
    Ok(result)
}

fn create_log(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io_error("cannot create action log", path, e))
}

// On Linux an executable can transiently remain open for writing in another
// concurrently spawned child, even after our own fs::write has closed its fd.
// Retry only an ETXTBSY spawn error: exec has not succeeded, so the action has
// not run. Never retry wait errors or exit statuses. The injected backoff keeps
// retry-count/error-classification tests deterministic and free of sleeps.
fn retry_spawn<T>(
    mut spawn: impl FnMut() -> io::Result<T>,
    mut backoff: impl FnMut(Duration),
) -> io::Result<T> {
    let mut delays = SPAWN_RETRY_DELAYS_MS.into_iter();
    loop {
        match spawn() {
            Err(error) if cfg!(target_os = "linux") && error.raw_os_error() == Some(26) => {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                backoff(Duration::from_millis(delay));
            }
            result => return result,
        }
    }
}

/// Execute an action or return its verified cached outputs in declaration order.
///
/// Executable bits are preserved and keyed. Other permission bits, timestamps,
/// and source path spellings are not inputs. Only declared regular output files
/// are published; undeclared files are discarded. The cache uses per-key
/// cross-process locks, so unrelated actions may execute concurrently.
/// Shell commands are trusted and unsandboxed, run with Bash; `shell_strict` adds
/// -e -o pipefail for genrules, while custom run_shell actions use only -c.
/// All shell actions get an empty `$0`, then literal `$1`, `$2`, ... . Explicit environment
/// entries override PATH/LANG/LC_ALL/TMPDIR defaults; no caller variables leak in.
/// Direct programs receive arguments verbatim, without a shell or strictness flags.
/// Relative programs containing a slash resolve in the action working directory;
/// bare names search PATH, and absolute paths are accepted. Host executables are
/// trusted, unsandboxed, and not content-keyed unless declared as action inputs.
/// Native writes spawn no subprocess and set output permissions to 0644 or 0755.
/// Logs are spooled to temporary files; at most the last 64 KiB per stream is
/// decoded (lossily) for diagnostics. Successful log tails are replayed to stderr,
/// keeping stdout artifact-only. Cache hits do not replay logs. Disk usage is not
/// capped; logs are removed with the working directory.
/// Linux ETXTBSY spawn failures get at most seven retries (127 ms total requested
/// backoff). Once a child starts, its wait result and exit status are never retried.
pub fn execute(cache_root: &Path, action: &Action) -> Result<ActionResult, String> {
    let (inputs, outputs, key) = prepare(action)?;
    fs::create_dir_all(cache_root).map_err(|e| io_error("cannot create cache", cache_root, e))?;
    let root = fs::canonicalize(cache_root)
        .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
    let actions = root.join("actions");
    let locks = root.join("locks");
    let work_parent = root.join("work");
    for path in [&actions, &locks, &work_parent] {
        ensure_directory(path)?;
    }
    let lock_path = locks.join(format!("{key}.lock"));
    // Locks are never unlinked: removing a held lock would permit two distinct
    // inodes (and thus two owners) for the same action key.
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(format!(
            "expected regular lock file: '{}'",
            lock_path.display()
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_error("cannot open lock", &lock_path, e))?;
    FileExt::lock_exclusive(&lock).map_err(|e| io_error("cannot lock", &lock_path, e))?;
    // File drop releases the advisory lock on every success/error path.
    let cached = actions.join(&key);
    let result_paths = || {
        action
            .outputs
            .iter()
            .map(|p| cached.join("outputs").join(p))
            .collect()
    };
    if cache_valid(&cached, &key, &outputs) {
        return Ok(ActionResult {
            outputs: result_paths(),
            cache_hit: true,
        });
    }
    remove_stale(&cached)?;
    let work = fresh_work(&work_parent, &key)?;
    let execroot = work.0.join("execroot");
    let tmp = work.0.join("tmp");
    ensure_directory(&execroot)?;
    ensure_directory(&tmp)?;
    for (name, input) in &inputs {
        write_file(&execroot.join(name), &input.bytes, input.executable)?;
    }
    for name in &outputs {
        if let Some(parent) = execroot.join(name).parent() {
            ensure_directory(parent)?;
        }
    }
    if let Some(write) = &action.write {
        write_file(
            &execroot.join(&outputs[0]),
            &write.content,
            if write.executable { 0o111 } else { 0 },
        )?;
    } else {
        let mut stdout = create_log(&work.0.join("stdout.log"))?;
        let mut stderr = create_log(&work.0.join("stderr.log"))?;
        let mut command = if let Some(program) = &action.program {
            let mut command = Command::new(program);
            command.args(&action.arguments);
            command
        } else {
            let mut command = Command::new(SHELL);
            if action.shell_strict {
                command.args(SHELL_FLAGS);
            } else {
                command.arg("-c");
            }
            command.arg(&action.command).arg("").args(&action.arguments);
            command
        };
        command
            .current_dir(&execroot)
            .env_clear()
            .envs(ENV)
            .env("TMPDIR", &tmp)
            .envs(&action.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?));
        let mut child = retry_spawn(|| command.spawn(), std::thread::sleep)
            .map_err(|e| format!("cannot execute action '{}': {e}", action.id))?;
        let status = child
            .wait()
            .map_err(|e| format!("cannot wait for action '{}': {e}", action.id))?;
        let stdout =
            log_tail(&mut stdout).map_err(|e| format!("cannot read action stdout: {e}"))?;
        let stderr =
            log_tail(&mut stderr).map_err(|e| format!("cannot read action stderr: {e}"))?;
        if !status.success() {
            return Err(format!(
                "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}",
                action.id, status, stdout, stderr
            ));
        }
        if !stdout.is_empty() || !stderr.is_empty() {
            eprintln!(
                "action '{}' logs:\nstdout:\n{}\nstderr:\n{}",
                action.id, stdout, stderr
            );
        }
    }
    // Validate every output before creating the completed directory. Snapshot
    // output contents too, so the manifest describes exactly what is published.
    let mut collected = Vec::new();
    for name in &outputs {
        let path = execroot.join(name);
        let snapshot = || -> Result<Input, String> {
            // Match metadata collection order: a successful action may leave a
            // regular output unreadable. Normalize before reading its bytes,
            // but never chmod a symlink or a path through a linked ancestor.
            regular_file(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(OUTPUT_MODE))
                .map_err(|e| io_error("cannot normalize action output permissions", &path, e))?;
            read_input(&path)
        };
        let input = snapshot().map_err(|e| {
            format!(
                "action '{}' has missing or invalid output '{}': {e}",
                action.id,
                name.display()
            )
        })?;
        collected.push((name, input));
    }
    let completed = work.0.join("completed");
    ensure_directory(&completed.join("outputs"))?;
    let mut manifest = Manifest {
        format: FORMAT.into(),
        key: key.clone(),
        outputs: Vec::new(),
    };
    for (name, input) in collected {
        let path = completed.join("outputs").join(name);
        write_file(&path, &input.bytes, 0o111)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(OUTPUT_MODE))
            .map_err(|e| io_error("cannot normalize action output permissions", &path, e))?;
        manifest.outputs.push(Output {
            name: hex(name.as_os_str().as_bytes()),
            digest: input.digest,
            executable: 0o111,
        });
    }
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("cannot encode manifest: {e}"))?;
    write_file(&completed.join("manifest.json"), &manifest_bytes, 0)?;
    // Same-filesystem rename publishes all declared files and the manifest at
    // once. Crash durability is not promised; damaged entries rebuild on use.
    fs::rename(&completed, &cached).map_err(|e| io_error("cannot publish action", &cached, e))?;
    Ok(ActionResult {
        outputs: result_paths(),
        cache_hit: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir();
            loop {
                let path = root.join(format!(
                    "native-action-test-{}-{}",
                    std::process::id(),
                    NEXT_WORK.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }

        fn action(&self) -> Action {
            let source = self.0.join("source");
            fs::write(&source, b"hello\n").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
            Action {
                id: "copy".into(),
                command: "cat src/input > out/result".into(),
                inputs: vec![(source, PathBuf::from("src/input"))],
                outputs: vec![PathBuf::from("out/result")],
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
            }
        }

        fn assert_unpublished(&self) {
            for name in ["actions", "work"] {
                let path = self.cache().join(name);
                if path.exists() {
                    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
                }
            }
        }

        fn write_action(&self, content: Vec<u8>) -> Action {
            Action {
                id: "write".into(),
                command: String::new(),
                inputs: Vec::new(),
                outputs: vec!["out/result".into()],
                write: Some(WriteAction {
                    content,
                    executable: false,
                }),
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
            }
        }

        fn direct_action(&self, script: &str) -> Action {
            let source = self.0.join("executable-source");
            fs::write(&source, script).unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
            Action {
                id: "direct".into(),
                command: String::new(),
                inputs: vec![(source, "tools/quoted '$ tool".into())],
                outputs: vec!["out/result".into()],
                write: None,
                program: Some("./tools/quoted '$ tool".into()),
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn native_write_large_binary_and_shell_text_are_not_executed() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("subprocess-ran");
        let mut content = format!(
            "#!/bin/bash\ntouch '{}'\n$(touch '{}')\n",
            marker.display(),
            marker.display()
        )
        .into_bytes();
        content.extend((0..300 * 1024).map(|i| (i % 256) as u8));
        let action = fixture.write_action(content.clone());
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), content);
        assert_eq!(
            fs::metadata(&first.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::read_dir(fixture.cache().join("work")).unwrap().count(),
            0
        );
    }

    #[test]
    fn native_write_content_mode_and_variant_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.write_action(b"original\0".to_vec());
        let first = execute(&fixture.cache(), &action).unwrap();
        action.write.as_mut().unwrap().executable = true;
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert!(!executable.cache_hit);
        assert_ne!(first.outputs, executable.outputs);
        assert_eq!(
            fs::metadata(&executable.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        action.write.as_mut().unwrap().content.push(0xff);
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(executable.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"original\0\xff");
        let mut empty = fixture.write_action(Vec::new());
        let write_key = prepare(&empty).unwrap().2;
        empty.write = None;
        assert_ne!(write_key, prepare(&empty).unwrap().2);
    }

    #[test]
    fn native_write_tampered_bytes_and_mode_rebuild() {
        let fixture = Fixture::new();
        let action = fixture.write_action(b"restored\0".to_vec());
        let first = execute(&fixture.cache(), &action).unwrap();
        let path = &first.outputs[0];
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path, b"tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(path).unwrap(), b"restored\0");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_write_shapes_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.write_action(vec![0]);
        for case in 0..6 {
            let mut action = original.clone();
            match case {
                0 => action.command = "true".into(),
                1 => action.arguments.push(String::new()),
                2 => {
                    action.environment.insert("KEY".into(), "value".into());
                }
                3 => action
                    .inputs
                    .push((fixture.0.join("missing"), "input".into())),
                4 => action.outputs.clear(),
                5 => action.outputs.push("other".into()),
                _ => unreachable!(),
            }
            assert!(
                validate_structure(&action)
                    .unwrap_err()
                    .contains("write action")
            );
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("write action")
            );
            assert!(!fixture.cache().exists());
        }
    }

    #[test]
    fn direct_executable_input_preserves_literal_arguments_and_environment() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action(
            "#!/bin/bash\nfalse\nfalse | true\ntest \"$#\" = 4 || exit 12\ntest \"$PATH\" = /usr/bin:/bin || exit 13\ntest -z \"${HOME+x}\" || exit 14\n[[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" || exit 15\nprintf '%s\\0' \"$0\" \"$1\" \"$2\" \"$3\" \"$4\" \"$VALUE\" > out/result\n",
        );
        let marker = fixture.0.join("expanded");
        action.arguments = vec![
            String::new(),
            "雪 'quotes'".into(),
            "$HOME *".into(),
            format!("$(touch '{}')", marker.display()),
        ];
        action
            .environment
            .insert("VALUE".into(), "$literal 雪".into());
        let first = execute(&fixture.cache(), &action).unwrap();
        let expected: Vec<u8> = std::iter::once(action.program.as_ref().unwrap())
            .chain(action.arguments.iter())
            .chain(std::iter::once(&action.environment["VALUE"]))
            .flat_map(|value| value.bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), expected);
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.shell_strict = false;
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.arguments.swap(0, 1);
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.environment.insert("VALUE".into(), "changed".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn direct_program_content_mode_path_and_variant_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action("#!/bin/sh\nprintf first > out/result\n");
        let first = execute(&fixture.cache(), &action).unwrap();
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o744)).unwrap();
        let mode = execute(&fixture.cache(), &action).unwrap();
        assert!(!mode.cache_hit);
        assert_ne!(first.outputs, mode.outputs);
        fs::write(
            &action.inputs[0].0,
            "#!/bin/sh\nprintf changed > out/result\n",
        )
        .unwrap();
        let content = execute(&fixture.cache(), &action).unwrap();
        assert!(!content.cache_hit);
        assert_eq!(fs::read(&content.outputs[0]).unwrap(), b"changed");
        action.program = Some("tools/quoted '$ tool".into());
        let path = execute(&fixture.cache(), &action).unwrap();
        assert!(!path.cache_hit);
        assert_ne!(content.outputs, path.outputs);
        let direct_key = prepare(&action).unwrap().2;
        action.program = None;
        assert_ne!(direct_key, prepare(&action).unwrap().2);
    }

    #[test]
    fn direct_program_searches_explicit_path_and_accepts_absolute_paths() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command.clear();
        action.arguments = vec!["src/input".into(), "out/result".into()];
        action.program = Some("cp".into());
        action.environment.insert("PATH".into(), "/bin".into());
        let searched = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&searched.outputs[0]).unwrap(), b"hello\n");
        action.program = Some("/bin/cp".into());
        let absolute = execute(&fixture.cache(), &action).unwrap();
        assert!(!absolute.cache_hit);
        assert_eq!(fs::read(&absolute.outputs[0]).unwrap(), b"hello\n");
        action.program = Some("cp".into());
        action
            .environment
            .insert("PATH".into(), "/no/such/directory".into());
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("cannot execute action")
        );
    }

    #[test]
    fn direct_failed_process_has_bounded_logs_and_no_publication() {
        let fixture = Fixture::new();
        let action = fixture.direct_action(
            "#!/bin/sh\nprintf partial > out/result\nhead -c 100000 /dev/zero\nprintf stdout-end\nhead -c 100000 /dev/zero >&2\nprintf stderr-end >&2\nexit 7\n",
        );
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(error.contains("failed (exit status: 7)"));
        assert!(error.contains("stdout-end") && error.contains("stderr-end"));
        assert_eq!(error.matches("[truncated:").count(), 2);
        assert!(error.len() < 2 * LOG_TAIL_BYTES + 1024);
        fixture.assert_unpublished();
        fs::write(
            &action.inputs[0].0,
            "#!/bin/sh\nprintf recovered > out/result\n",
        )
        .unwrap();
        let recovered = execute(&fixture.cache(), &action).unwrap();
        fs::set_permissions(&recovered.outputs[0], fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&recovered.outputs[0], "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(&recovered.outputs[0]).unwrap(), b"recovered");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_direct_shapes_are_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.direct_action("#!/bin/sh\nexit 0\n");
        fs::remove_file(&original.inputs[0].0).unwrap();
        for case in 0..6 {
            let mut action = original.clone();
            match case {
                0 => action.program = Some(String::new()),
                1 => action.program = Some("bad\0program".into()),
                2 => action.command = "true".into(),
                3 => action.arguments.push("bad\0argument".into()),
                4 => {
                    action.environment.insert("bad=key".into(), "value".into());
                }
                5 => {
                    action.inputs.clear();
                    action.write = Some(WriteAction {
                        content: vec![],
                        executable: false,
                    });
                }
                _ => unreachable!(),
            }
            let structural = validate_structure(&action).unwrap_err();
            assert_eq!(execute(&fixture.cache(), &action).unwrap_err(), structural);
            assert!(!fixture.cache().exists());
        }
    }

    #[test]
    fn shell_strictness_controls_errexit_and_pipeline_status() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        for command in [
            "false; printf ok > out/result",
            "false | printf ok > out/result",
        ] {
            action.command = command.into();
            action.shell_strict = false;
            let lax = execute(&fixture.cache(), &action).unwrap();
            assert!(!lax.cache_hit);
            assert_eq!(fs::read(&lax.outputs[0]).unwrap(), b"ok");
            action.shell_strict = true;
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("failed (exit status: 1)")
            );
        }
        action.shell_strict = false;
        action.command = "printf partial > out/result; true | false".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("failed (exit status: 1)")
        );
    }

    #[test]
    fn lax_shell_always_has_empty_zero_and_literal_first_argument() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.shell_strict = false;
        action.command = "test \"$0\" = '' && test \"$#\" = 0 && printf ok > out/result".into();
        execute(&fixture.cache(), &action).unwrap();
        action.arguments = vec!["雪 $HOME 'literal'".into()];
        action.command =
            "test \"$0\" = '' && test \"$#\" = 1 && printf '%s' \"$1\" > out/result".into();
        let result = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::read(&result.outputs[0]).unwrap(),
            action.arguments[0].as_bytes()
        );
    }

    #[test]
    fn shell_strictness_is_keyed_only_for_shell_actions() {
        let fixture = Fixture::new();
        let mut shell = fixture.action();
        let strict = execute(&fixture.cache(), &shell).unwrap();
        shell.shell_strict = false;
        let lax = execute(&fixture.cache(), &shell).unwrap();
        assert!(!lax.cache_hit);
        assert_ne!(strict.outputs, lax.outputs);
        let mut write = fixture.write_action(b"native".to_vec());
        let first = execute(&fixture.cache(), &write).unwrap();
        write.shell_strict = false;
        let second = execute(&fixture.cache(), &write).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
    }

    #[test]
    fn shell_arguments_are_literal_ordered_and_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let marker = fixture.0.join("argument-ran");
        action.arguments = vec![
            String::new(),
            "雪 🦀".into(),
            "$HOME 'quoted' \\\n".into(),
            format!("$(touch '{}')", marker.display()),
        ];
        action.command = "test \"$0\" = '' && test \"$#\" = 4 && printf '%s\\0' \"$1\" \"$2\" \"$3\" \"$4\" > out/result".into();
        let expected: Vec<u8> = action
            .arguments
            .iter()
            .flat_map(|arg| arg.bytes().chain(std::iter::once(0)))
            .collect();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), expected);
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.arguments.swap(0, 1);
        let reordered = execute(&fixture.cache(), &action).unwrap();
        assert!(!reordered.cache_hit);
        assert_ne!(first.outputs, reordered.outputs);
        action.arguments.clear();
        action.command = "test \"$0\" = '' && test \"$#\" = 0 && printf ok > out/result".into();
        execute(&fixture.cache(), &action).unwrap();
    }

    #[test]
    fn shell_environment_overlays_defaults_and_is_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.environment = BTreeMap::from([
            ("VALUE".into(), "literal $HOME 雪".into()),
            ("PATH".into(), "/explicit/path".into()),
            ("LANG".into(), "POSIX".into()),
            ("LC_ALL".into(), "POSIX".into()),
            ("TMPDIR".into(), "/explicit/tmp".into()),
            ("EMPTY".into(), String::new()),
        ]);
        action.command = "test \"$PATH\" = /explicit/path && test \"$LANG\" = POSIX && test \"$LC_ALL\" = POSIX && test \"$TMPDIR\" = /explicit/tmp && test \"${EMPTY+x}\" = x && test -z \"$EMPTY\" && test -z \"${HOME+x}\" && printf '%s' \"$VALUE\" > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::read(&first.outputs[0]).unwrap(),
            "literal $HOME 雪".as_bytes()
        );
        let reordered: BTreeMap<_, _> = action
            .environment
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        action.environment = reordered;
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.environment.insert("VALUE".into(), "changed".into());
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(first.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed");
        action
            .environment
            .insert("UNUSED".into(), "also-keyed".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_shell_arguments_and_environment_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.action();
        for (name, value) in [
            ("", "value"),
            ("a=b", "value"),
            ("a\0b", "value"),
            ("key", "a\0b"),
        ] {
            let mut action = original.clone();
            action.environment.insert(name.into(), value.into());
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("environment")
            );
            assert!(!fixture.cache().exists());
        }
        let mut action = original;
        action.arguments.push("a\0b".into());
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("argument contains a NUL")
        );
        assert!(!fixture.cache().exists());
    }

    #[test]
    fn successful_hit_and_output_order() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command += "; printf second > other; chmod 751 out/result";
        action.outputs.push("other".into());
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"hello\n");
        assert_eq!(fs::read(&first.outputs[1]).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&first.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        let second = execute(&fixture.cache(), &action).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
        action.outputs.reverse();
        let reordered = execute(&fixture.cache(), &action).unwrap();
        assert!(reordered.cache_hit);
        assert_eq!(reordered.outputs[0], first.outputs[1]);
    }

    #[test]
    fn content_command_id_and_mode_invalidation() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let first = execute(&fixture.cache(), &action).unwrap();
        fs::write(&action.inputs[0].0, b"changed").unwrap();
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(first.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed");
        action.command += "; printf ! >> out/result";
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o755)).unwrap();
        action.command += "; test -x src/input";
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert!(!executable.cache_hit);
        // Change only mode, not command or contents.
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o744)).unwrap();
        let mode_changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!mode_changed.cache_hit);
        assert_ne!(executable.outputs, mode_changed.outputs);
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.id += "-new";
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn tampered_missing_mode_and_manifest_rebuild() {
        let fixture = Fixture::new();
        let action = fixture.action();
        let result = execute(&fixture.cache(), &action).unwrap();
        let path = &result.outputs[0];
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path, "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(path).unwrap(), b"hello\n");
        for mode in [0o444, 0o644, 0o755, 0o4755] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                OUTPUT_MODE
            );
        }
        fs::remove_file(path).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        let entry = path.parent().unwrap().parent().unwrap().parent().unwrap();
        fs::write(entry.join("manifest.json"), b"{}").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        fs::remove_file(path).unwrap();
        symlink(&action.inputs[0].0, path).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert!(!fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn failures_do_not_publish_and_can_retry() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command =
            "echo stdout-message; echo stderr-message >&2; echo partial > out/result; exit 7"
                .into();
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            error.contains("stdout-message")
                && error.contains("stderr-message")
                && error.contains('7')
        );
        fixture.assert_unpublished();
        action.command = "true".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("missing or invalid output")
        );
        fixture.assert_unpublished();
        // The same key retries cleanly when an external prerequisite changes.
        let prerequisite = fixture.0.join("ready");
        action.command = format!(
            "test -f '{}' && cat src/input > out/result",
            prerequisite.display()
        );
        assert!(execute(&fixture.cache(), &action).is_err());
        fixture.assert_unpublished();
        fs::write(prerequisite, b"").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn shell_failures_and_pipeline_failures_do_not_publish() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let marker = fixture.0.join("continued");
        for command in ["false; echo ok > out/result", "false | cat > out/result"] {
            action.command = format!("{command}; touch '{}'", marker.display());
            let error = execute(&fixture.cache(), &action).unwrap_err();
            assert!(error.contains("failed (exit status: 1)"), "{error}");
            assert!(!marker.exists());
            fixture.assert_unpublished();
        }
    }

    #[test]
    fn substantial_logs_succeed_or_fail_with_bounded_tails() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let logs = "printf stdout-start; head -c 1048576 /dev/zero | tr '\\0' x; printf stdout-end; printf stderr-start >&2; head -c 1048576 /dev/zero | tr '\\0' y >&2; printf stderr-end >&2";
        action.command = format!("{logs}; printf artifact > out/result");
        let result = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&result.outputs[0]).unwrap(), b"artifact");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::read_dir(fixture.cache().join("work")).unwrap().count(),
            0
        );

        let failed_fixture = Fixture::new();
        action.command = format!("{logs}; printf partial > out/result; false");
        let error = execute(&failed_fixture.cache(), &action).unwrap_err();
        assert!(error.len() < 2 * LOG_TAIL_BYTES + 1024);
        assert_eq!(error.matches("[truncated:").count(), 2);
        assert!(error.contains("stdout-end") && error.contains("stderr-end"));
        assert!(!error.contains("stdout-start") && !error.contains("stderr-start"));
        failed_fixture.assert_unpublished();
    }

    #[test]
    fn log_tails_are_bounded_and_mark_only_truncation() {
        let fixture = Fixture::new();
        let path = fixture.0.join("log");
        for length in [0, 10, LOG_TAIL_BYTES, LOG_TAIL_BYTES + 1] {
            fs::write(&path, vec![b'x'; length]).unwrap();
            let tail = log_tail(&mut File::open(&path).unwrap()).unwrap();
            assert_eq!(tail.contains("[truncated:"), length > LOG_TAIL_BYTES);
            assert_eq!(tail.matches('x').count(), length.min(LOG_TAIL_BYTES));
            assert!(tail.len() <= LOG_TAIL_BYTES + 100);
        }
        fs::write(&path, vec![0xff; LOG_TAIL_BYTES + 1]).unwrap();
        let tail = log_tail(&mut File::open(&path).unwrap()).unwrap();
        assert!(tail.starts_with("[truncated:"));
        assert!(tail.len() <= 3 * LOG_TAIL_BYTES + 100);
    }

    #[test]
    fn structure_validation_does_not_require_sources_or_create_cache() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        fs::remove_file(&action.inputs[0].0).unwrap();
        validate_structure(&action).unwrap();
        action
            .inputs
            .push((fixture.0.join("also-missing"), "src/input".into()));
        validate_structure(&action).unwrap();
        action.outputs.clear();
        validate_structure(&action).unwrap();

        action.command = "true\0; echo unreachable".into();
        assert!(validate_structure(&action).unwrap_err().contains("NUL"));
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("NUL")
        );
        assert!(!fixture.cache().exists());
    }

    #[test]
    fn unsafe_paths_rejected_before_shell() {
        let fixture = Fixture::new();
        let original = fixture.action();
        fs::remove_file(&original.inputs[0].0).unwrap();
        let marker = fixture.0.join("ran");
        for name in [
            "",
            "/absolute",
            "../escape",
            "a/../b",
            "./a",
            "a/./b",
            "a//b",
            "a/",
            "a\0b",
        ] {
            for input in [false, true] {
                let mut action = original.clone();
                action.command = format!("touch '{}'", marker.display());
                if input {
                    action.inputs[0].1 = name.into();
                } else {
                    action.outputs[0] = name.into();
                }
                assert!(validate_structure(&action).is_err(), "accepted {name:?}");
                assert!(
                    execute(&fixture.cache(), &action).is_err(),
                    "accepted {name:?}"
                );
                assert!(!marker.exists());
            }
        }
        for name in ["src", "src/input", "src/input/child"] {
            let mut action = original.clone();
            action.outputs = vec![name.into()];
            assert!(validate_structure(&action).is_err());
            assert!(execute(&fixture.cache(), &action).is_err());
        }
        let mut action = original.clone();
        action.outputs = vec!["a".into(), "a/b".into()];
        assert!(validate_structure(&action).is_err());
        assert!(execute(&fixture.cache(), &action).is_err());
        action.outputs = vec!["a".into(), "a".into()];
        assert!(validate_structure(&action).is_err());
        action = original.clone();
        action
            .inputs
            .push((action.inputs[0].0.clone(), "src".into()));
        assert!(validate_structure(&action).is_err());
        assert!(execute(&fixture.cache(), &action).is_err());
        assert!(!fixture.cache().exists());
        fixture.assert_unpublished();
    }

    #[test]
    fn duplicate_mappings_and_input_order() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action
            .inputs
            .push((action.inputs[0].0.clone(), "second".into()));
        let first = execute(&fixture.cache(), &action).unwrap();
        action.inputs.reverse();
        action.inputs.push(action.inputs[0].clone());
        let second = execute(&fixture.cache(), &action).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
        let identical = fixture.0.join("identical");
        fs::write(&identical, b"hello\n").unwrap();
        fs::set_permissions(&identical, fs::Permissions::from_mode(0o644)).unwrap();
        action.inputs.push((identical, "src/input".into()));
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        let conflicting = fixture.0.join("different");
        fs::write(&conflicting, "different").unwrap();
        action.inputs.push((conflicting, "src/input".into()));
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("conflicting duplicate")
        );
    }

    #[test]
    fn reject_nonregular_and_symlink_inputs_outputs() {
        let fixture = Fixture::new();
        let original = fixture.action();
        let link = fixture.0.join("link");
        symlink(&original.inputs[0].0, &link).unwrap();
        let mut action = original.clone();
        action.inputs[0].0 = link;
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("symlink")
        );
        let dirlink = fixture.0.join("dirlink");
        symlink(&fixture.0, &dirlink).unwrap();
        action.inputs[0].0 = dirlink.join("source");
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.clone();
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.join("missing");
        assert!(execute(&fixture.cache(), &action).is_err());
        for command in [
            format!("ln -s '{}' out/result", original.inputs[0].0.display()),
            format!("rmdir out; ln -s '{}' out", fixture.0.display()),
            "mkdir out/result".into(),
            "mkfifo out/result".into(),
        ] {
            action = original.clone();
            action.command = command;
            assert!(execute(&fixture.cache(), &action).is_err());
            fixture.assert_unpublished();
        }
    }

    #[test]
    fn controlled_environment_and_absolute_tmpdir() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "test \"$PATH\" = /usr/bin:/bin && test \"$LANG\" = C && test \"$LC_ALL\" = C && test -z \"${HOME+x}\" && test -z \"${CARGO_HOME+x}\" && [[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" && printf ok > out/result".into();
        assert!(execute(&fixture.cache(), &action).is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_retry_is_bounded_and_only_handles_etxtbsy() {
        let mut calls = 0;
        let mut delays = Vec::new();
        let error = retry_spawn::<()>(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(26))
            },
            |delay| delays.push(delay.as_millis()),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(26));
        assert_eq!(calls, 8);
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 64]);
        assert_eq!(delays.iter().sum::<u128>(), 127);

        calls = 0;
        delays.clear();
        let result = retry_spawn(
            || {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::from_raw_os_error(26))
                } else {
                    Ok(42)
                }
            },
            |delay| delays.push(delay.as_millis()),
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls, 3);
        assert_eq!(delays, vec![1, 2]);

        for errno in [1, 2, 4, 11, 13, 24] {
            calls = 0;
            let error = retry_spawn::<()>(
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(errno))
                },
                |_| panic!("must not back off for errno {errno}"),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(calls, 1);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_retry_recovers_from_real_writable_executable_without_duplicate_execution() {
        let fixture = Fixture::new();
        let action = fixture.direct_action(
            "#!/bin/sh\nprintf 'run\\n' >> \"$1\"\nprintf stdout-message\nprintf stderr-message >&2\n",
        );
        let executable = &action.inputs[0].0;
        let marker = fixture.0.join("ran");
        let mut writer = Some(OpenOptions::new().write(true).open(executable).unwrap());
        let mut stdout = create_log(&fixture.0.join("stdout")).unwrap();
        let mut stderr = create_log(&fixture.0.join("stderr")).unwrap();
        let mut command = Command::new(executable);
        command
            .arg(&marker)
            .stdout(Stdio::from(stdout.try_clone().unwrap()))
            .stderr(Stdio::from(stderr.try_clone().unwrap()));
        let mut attempts = 0;
        let mut waits = 0;
        let mut child = retry_spawn(
            || {
                attempts += 1;
                command.spawn()
            },
            |delay| {
                waits += 1;
                assert!(!marker.exists());
                drop(writer.take());
                std::thread::sleep(delay);
            },
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        assert!((2..=8).contains(&attempts));
        assert_eq!(waits, attempts - 1);
        assert_eq!(fs::read_to_string(marker).unwrap(), "run\n");
        assert_eq!(log_tail(&mut stdout).unwrap(), "stdout-message");
        assert_eq!(log_tail(&mut stderr).unwrap(), "stderr-message");
    }

    #[test]
    fn direct_nonzero_exit_is_not_a_spawn_error_and_is_never_retried() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action("#!/bin/sh\nprintf run >> \"$1\"\nexit 26\n");
        let marker = fixture.0.join("ran");
        action.arguments = vec![marker.to_str().unwrap().into()];
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(error.contains("failed (exit status: 26)"), "{error}");
        assert_eq!(fs::read_to_string(marker).unwrap(), "run");
        fixture.assert_unpublished();
    }

    #[test]
    fn concurrent_direct_actions_in_distinct_workdirs_run_once() {
        const WORKERS: usize = 16;
        const ROUNDS: usize = 64;
        let barrier = Arc::new(Barrier::new(WORKERS));
        let mut workers = Vec::new();
        for _ in 0..WORKERS {
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let fixture = Fixture::new();
                let marker = fixture.0.join("executions");
                let mut action = fixture.direct_action(
                    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$2\"\nprintf '%s' \"$1\" > out/result\n",
                );
                barrier.wait();
                for round in 0..ROUNDS {
                    // Every round gets a fresh cache key and copied executable.
                    action.arguments = vec![round.to_string(), marker.to_str().unwrap().into()];
                    let first = execute(&fixture.cache(), &action).unwrap();
                    assert!(!first.cache_hit);
                    assert_eq!(
                        fs::read_to_string(&first.outputs[0]).unwrap(),
                        round.to_string()
                    );
                    assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
                }
                let expected: String = (0..ROUNDS).map(|round| format!("{round}\n")).collect();
                assert_eq!(fs::read_to_string(marker).unwrap(), expected);
            }));
        }
        // Join every worker even if one panics; no background test work leaks.
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
        assert!(results.into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn concurrent_same_key_runs_once() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let count = fixture.0.join("count");
        action.command = format!(
            "echo run >> '{}'; sleep 0.1; cat src/input > out/result",
            count.display()
        );
        let barrier = Arc::new(Barrier::new(4));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let action = action.clone();
            let cache = fixture.cache();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                execute(&cache, &action).unwrap()
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| !r.cache_hit).count(), 1);
        assert_eq!(fs::read_to_string(count).unwrap(), "run\n");
        assert!(results.iter().all(|r| r.outputs == results[0].outputs));
    }
}
