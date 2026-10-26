//! The spawn execution log (docs/PROFILE.md "Execution log"):
//! `--execution_log_json_file`, `--execution_log_binary_file` (the expanded
//! SpawnExec format of exec/ExpandedSpawnLogContext.java) and
//! `--execution_log_compact_file` (the zstd-compressed ExecLogEntry stream
//! of exec/CompactSpawnLogContext.java, Bazel 9's recommended format).
//! Schema: src/main/protobuf/spawn.proto.
//!
//! Every executed spawn is logged — a process the executor ran (its
//! failure included) or a remote cache hit; native writes, transforms and
//! action cache hits are not spawns (Bazel logs neither its internal
//! actions nor action cache hits). Off by default: the executor's hook is
//! one relaxed atomic load.

use crate::bep::proto::{Msg, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// The execution log options (ExecutionOptions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub json_file: Option<String>,
    pub binary_file: Option<String>,
    pub compact_file: Option<String>,
    /// `--[no]execution_log_sort` (default true): the expanded formats in
    /// a stable, dependency-respecting order.
    pub sort: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            json_file: None,
            binary_file: None,
            compact_file: None,
            sort: true,
        }
    }
}

impl Options {
    /// One option: `Some(Ok(used_next))` when it is an execution log option.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        if let Some(value) = crate::configuration::boolean_option(arg, "execution_log_sort") {
            return Some(value.map(|value| {
                self.sort = value;
                false
            }));
        }
        for name in [
            "execution_log_json_file",
            "execution_log_binary_file",
            "execution_log_compact_file",
            "experimental_execution_log_compact_file",
        ] {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                match next {
                    Some(value) => (value, true),
                    None => return Some(Err(format!("{flag} requires a value"))),
                }
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            let value = (!value.is_empty()).then(|| value.to_owned());
            match name {
                "execution_log_json_file" => self.json_file = value,
                "execution_log_binary_file" => self.binary_file = value,
                _ => self.compact_file = value,
            }
            return Some(Ok(used));
        }
        None
    }

    /// SpawnLogModule.initOutputs (:91-96): at most one log.
    pub fn validate(&self) -> Result<(), String> {
        let given = [&self.json_file, &self.binary_file, &self.compact_file]
            .iter()
            .filter(|file| file.is_some())
            .count();
        if given > 1 {
            return Err(
                "Must specify at most one of --execution_log_binary_file, --execution_log_json_file and --execution_log_compact_file"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// One file of a spawn (an input or an output).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFile {
    /// Exec path.
    pub path: String,
    /// SHA-256 hex and size; `None` for an unresolved symlink.
    pub digest: Option<(String, u64)>,
    pub symlink_target: Option<String>,
    /// Below a directory input or output: the directory's exec path.
    pub directory: Option<String>,
}

/// One executed spawn.
#[derive(Clone, Debug)]
pub struct Spawn {
    pub args: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub inputs: Vec<LogFile>,
    /// Declared outputs (exec paths), in declaration order.
    pub listed_outputs: Vec<String>,
    /// The outputs found after the spawn, in declaration order (`None`:
    /// missing).
    pub actual_outputs: Vec<(String, Option<Vec<LogFile>>)>,
    pub remotable: bool,
    pub cacheable: bool,
    pub remote_cacheable: bool,
    pub mnemonic: String,
    pub runner: String,
    pub cache_hit: bool,
    /// SpawnResult.Status (empty: SUCCESS).
    pub status: String,
    pub exit_code: i32,
    pub target_label: String,
    pub start: SystemTime,
    pub wall: Duration,
}

struct State {
    options: Options,
    cwd: PathBuf,
    spawns: Vec<Spawn>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Whether an execution log is being recorded.
#[inline]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Start recording (`cwd` resolves relative file names).
pub fn install(options: &Options, cwd: &Path) {
    if options.json_file.is_none()
        && options.binary_file.is_none()
        && options.compact_file.is_none()
    {
        return;
    }
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State {
        options: options.clone(),
        cwd: cwd.to_path_buf(),
        spawns: Vec::new(),
    });
    ACTIVE.store(true, Ordering::Relaxed);
}

/// The executor ran `spawn`.
pub fn log(spawn: Spawn) {
    if !active() {
        return;
    }
    if let Some(state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        state.spawns.push(spawn);
    }
}

/// A file's SHA-256 hex and size (for logged outputs).
pub fn digest_of(path: &Path) -> Option<(String, u64)> {
    // snapshot-unrecorded: digests of spawn outputs for the execution log.
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let (digest, size) = crate::digest_memo::sha256_reader(&mut file, size).ok()?;
    Some((
        digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        size,
    ))
}

/// The files of an output at `path` (exec path `exec`): a regular file, a
/// directory's files (sorted, recursively) or a symlink; `None` when
/// missing.
pub fn output_files(exec: &str, path: &Path) -> Option<Vec<LogFile>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        return Some(vec![LogFile {
            path: exec.to_owned(),
            digest: None,
            symlink_target: Some(target.display().to_string()),
            directory: None,
        }]);
    }
    if metadata.is_dir() {
        let mut files = Vec::new();
        collect_directory(exec, path, exec, &mut files);
        return Some(files);
    }
    Some(vec![LogFile {
        path: exec.to_owned(),
        digest: digest_of(path),
        symlink_target: None,
        directory: None,
    }])
}

fn collect_directory(display: &str, path: &Path, root: &str, files: &mut Vec<LogFile>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
    names.sort();
    for name in names {
        let child = path.join(&name);
        let child_display = format!("{display}/{}", name.to_string_lossy());
        if std::fs::metadata(&child).is_ok_and(|metadata| metadata.is_dir()) {
            collect_directory(&child_display, &child, root, files);
        } else {
            files.push(LogFile {
                path: child_display,
                digest: digest_of(&child),
                symlink_target: None,
                directory: Some(root.to_owned()),
            });
        }
    }
}

fn digest_message(digest: &(String, u64), with_function: bool) -> Msg {
    let mut message = Msg::new().raw(1, "hash", Value::Str(digest.0.clone())).raw(
        2,
        "size_bytes",
        Value::I64(digest.1 as i64),
    );
    if with_function {
        message = message.raw(3, "hash_function_name", Value::Str("SHA-256".to_owned()));
    }
    message
}

/// SpawnExec.File (with every field, as the JSON log prints them).
fn expanded_file(file: &LogFile) -> Msg {
    let mut message = Msg::new().raw(1, "path", Value::Str(file.path.clone()));
    if let Some(digest) = &file.digest {
        message = message.msg(2, "digest", digest_message(digest, true));
    }
    message.raw(3, "is_tool", Value::Bool(false)).raw(
        4,
        "symlink_target_path",
        Value::Str(file.symlink_target.clone().unwrap_or_default()),
    )
}

/// SpawnLogContext.getSpawnMetricsProto: the non-zero durations, every
/// count, the start time.
fn metrics(spawn: &Spawn) -> Msg {
    let millis = Duration::from_millis(spawn.wall.as_millis() as u64);
    let mut message = Msg::new();
    if !millis.is_zero() {
        message =
            message
                .duration(1, "total_time", millis)
                .duration(8, "execution_wall_time", millis);
    }
    for (number, name) in [
        (11, "input_bytes"),
        (12, "input_files"),
        (13, "memory_estimate_bytes"),
        (14, "input_bytes_limit"),
        (15, "input_files_limit"),
        (16, "output_bytes_limit"),
        (17, "output_files_limit"),
        (18, "memory_bytes_limit"),
    ] {
        message = message.raw(number, name, Value::I64(0));
    }
    let start = spawn
        .start
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    message
        .raw(
            20,
            "start_time",
            Value::Timestamp(
                start.as_secs() as i64,
                (start.subsec_millis() * 1_000_000) as i32,
            ),
        )
        .raw(21, "measured_memory_peak_bytes", Value::I64(0))
}

/// The SpawnExec of the expanded formats.
fn spawn_exec(spawn: &Spawn) -> Msg {
    let mut listed = spawn.listed_outputs.clone();
    listed.sort();
    let actual: Vec<Value> = spawn
        .actual_outputs
        .iter()
        .flat_map(|(_, files)| files.iter().flatten())
        .map(|file| Value::Msg(expanded_file(file)))
        .collect();
    Msg::new()
        .list(
            1,
            "command_args",
            spawn.args.iter().cloned().map(Value::Str).collect(),
        )
        .list(
            2,
            "environment_variables",
            spawn
                .environment
                .iter()
                .map(|(name, value)| {
                    Value::Msg(Msg::new().raw(1, "name", Value::Str(name.clone())).raw(
                        2,
                        "value",
                        Value::Str(value.clone()),
                    ))
                })
                .collect(),
        )
        .list(4, "inputs", {
            // The expanded log's input map is sorted by path.
            let mut inputs: Vec<&LogFile> = spawn.inputs.iter().collect();
            inputs.sort_by(|a, b| a.path.cmp(&b.path));
            inputs.dedup_by(|a, b| a.path == b.path);
            inputs
                .into_iter()
                .map(|file| Value::Msg(expanded_file(file)))
                .collect()
        })
        .list(
            5,
            "listed_outputs",
            listed.into_iter().map(Value::Str).collect(),
        )
        .raw(6, "remotable", Value::Bool(spawn.remotable))
        .raw(7, "cacheable", Value::Bool(spawn.cacheable))
        .raw(8, "timeout_millis", Value::I64(0))
        .raw(10, "mnemonic", Value::Str(spawn.mnemonic.clone()))
        .list(11, "actual_outputs", actual)
        .raw(12, "runner", Value::Str(spawn.runner.clone()))
        .raw(13, "cache_hit", Value::Bool(spawn.cache_hit))
        .raw(14, "status", Value::Str(spawn.status.clone()))
        .raw(15, "exit_code", Value::I32(spawn.exit_code))
        .raw(16, "remote_cacheable", Value::Bool(spawn.remote_cacheable))
        .raw(18, "target_label", Value::Str(spawn.target_label.clone()))
        .msg(20, "metrics", metrics(spawn))
}

/// StableSort (exec/StableSort.java): producers before consumers, ties
/// by the first listed output.
fn stable_order(spawns: &[Spawn]) -> Vec<usize> {
    let mut producer: HashMap<&str, usize> = HashMap::new();
    for (index, spawn) in spawns.iter().enumerate() {
        for output in &spawn.listed_outputs {
            producer.insert(output.as_str(), index);
        }
    }
    let key = |index: usize| {
        let mut outputs = spawns[index].listed_outputs.clone();
        outputs.sort();
        outputs.into_iter().next().unwrap_or_default()
    };
    let mut pending: Vec<BTreeSet<usize>> = spawns
        .iter()
        .enumerate()
        .map(|(index, spawn)| {
            spawn
                .inputs
                .iter()
                .filter_map(|input| {
                    producer
                        .get(input.directory.as_deref().unwrap_or(input.path.as_str()))
                        .copied()
                })
                .filter(|producer| *producer != index)
                .collect()
        })
        .collect();
    let mut ready: BTreeSet<(String, usize)> = pending
        .iter()
        .enumerate()
        .filter(|(_, dependencies)| dependencies.is_empty())
        .map(|(index, _)| (key(index), index))
        .collect();
    let mut order = Vec::new();
    let mut done = vec![false; spawns.len()];
    while let Some(first) = ready.iter().next().cloned() {
        ready.remove(&first);
        let index = first.1;
        done[index] = true;
        order.push(index);
        for (other, dependencies) in pending.iter_mut().enumerate() {
            if !done[other] && dependencies.remove(&index) && dependencies.is_empty() {
                ready.insert((key(other), other));
            }
        }
    }
    // A cycle cannot happen; keep anything left in arrival order.
    order.extend((0..spawns.len()).filter(|index| !done[*index]));
    order
}

/// The compact log's entry writer (CompactSpawnLogContext.logEntry).
struct Compact {
    out: Vec<u8>,
    next_id: u32,
    files: HashMap<String, u32>,
}

impl Compact {
    fn entry(&mut self, id: u32, field: (u32, &'static str, Msg)) {
        let message = Msg::new().u32(1, "id", id).msg(field.0, field.1, field.2);
        self.out
            .extend_from_slice(&crate::bep::proto::delimited(&message.encode()));
    }

    fn fresh(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// A file entry keyed by its path (logged once).
    fn file(&mut self, file: &LogFile) -> u32 {
        if let Some(id) = self.files.get(&file.path) {
            return *id;
        }
        let id = self.fresh();
        self.files.insert(file.path.clone(), id);
        let entry = match (&file.symlink_target, &file.digest) {
            (Some(target), _) => (
                5,
                "unresolved_symlink",
                Msg::new()
                    .str(1, "path", file.path.clone())
                    .str(2, "target_path", target.clone()),
            ),
            (None, digest) => (
                3,
                "file",
                Msg::new().str(1, "path", file.path.clone()).opt_msg(
                    2,
                    "digest",
                    digest.as_ref().map(|digest| digest_message(digest, false)),
                ),
            ),
        };
        self.entry(id, entry);
        id
    }

    /// A directory entry keyed by its path (logged once): its files,
    /// relative to it.
    fn directory(&mut self, path: &str, files: &[&LogFile]) -> u32 {
        if let Some(id) = self.files.get(path) {
            return *id;
        }
        let id = self.fresh();
        self.files.insert(path.to_owned(), id);
        let entries = files.iter().map(|file| {
            let relative = file
                .path
                .strip_prefix(path)
                .map(|rest| rest.trim_start_matches('/'))
                .unwrap_or(&file.path);
            Msg::new().str(1, "path", relative).opt_msg(
                2,
                "digest",
                file.digest
                    .as_ref()
                    .map(|digest| digest_message(digest, false)),
            )
        });
        self.entry(
            id,
            (
                4,
                "directory",
                Msg::new().str(1, "path", path).msgs(2, "files", entries),
            ),
        );
        id
    }

    /// The ids of `files`: directory members grouped into their directory.
    fn file_ids(&mut self, files: &[LogFile]) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut directories: BTreeMap<&str, Vec<&LogFile>> = BTreeMap::new();
        for file in files {
            match &file.directory {
                Some(directory) => directories.entry(directory).or_default().push(file),
                None => ids.push(self.file(file)),
            }
        }
        for (directory, members) in directories {
            ids.push(self.directory(directory, &members));
        }
        ids
    }

    fn spawn(&mut self, spawn: &Spawn) {
        let input_ids = self.file_ids(&spawn.inputs);
        let input_set = if input_ids.is_empty() {
            0
        } else {
            let id = self.fresh();
            self.entry(
                id,
                (
                    6,
                    "input_set",
                    Msg::new().list(
                        5,
                        "input_ids",
                        input_ids.into_iter().map(Value::U32).collect(),
                    ),
                ),
            );
            id
        };
        let mut outputs = Vec::new();
        for (path, files) in &spawn.actual_outputs {
            let output = match files {
                Some(files) if !files.is_empty() => {
                    let id = if files.iter().any(|file| file.directory.is_some()) {
                        let members: Vec<&LogFile> = files.iter().collect();
                        self.directory(path, &members)
                    } else {
                        self.file(&files[0])
                    };
                    Msg::new().u32(5, "output_id", id)
                }
                _ => Msg::new().str(4, "invalid_output_path", path.clone()),
            };
            outputs.push(output);
        }
        let millis = Duration::from_millis(spawn.wall.as_millis() as u64);
        let mut metrics = Msg::new();
        if !millis.is_zero() {
            metrics = metrics.duration(1, "total_time", millis).duration(
                8,
                "execution_wall_time",
                millis,
            );
        }
        let start = spawn
            .start
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        metrics = metrics.raw(
            20,
            "start_time",
            Value::Timestamp(
                start.as_secs() as i64,
                (start.subsec_millis() * 1_000_000) as i32,
            ),
        );
        let message = Msg::new()
            .strs(1, "args", spawn.args.iter().cloned())
            .msgs(
                2,
                "env_vars",
                spawn.environment.iter().map(|(name, value)| {
                    Msg::new()
                        .str(1, "name", name.clone())
                        .str(2, "value", value.clone())
                }),
            )
            .u32(4, "input_set_id", input_set)
            .msgs(6, "outputs", outputs)
            .str(7, "target_label", spawn.target_label.clone())
            .str(8, "mnemonic", spawn.mnemonic.clone())
            .i32(9, "exit_code", spawn.exit_code)
            .str(10, "status", spawn.status.clone())
            .str(11, "runner", spawn.runner.clone())
            .bool(12, "cache_hit", spawn.cache_hit)
            .bool(13, "remotable", spawn.remotable)
            .bool(14, "cacheable", spawn.cacheable)
            .bool(15, "remote_cacheable", spawn.remote_cacheable)
            .msg(18, "metrics", metrics);
        self.entry(0, (7, "spawn", message));
    }
}

/// The compact log's bytes before compression: the invocation entry, then
/// each spawn's file/input set entries and the spawn.
fn compact_stream(spawns: &[Spawn]) -> Vec<u8> {
    let mut compact = Compact {
        out: Vec::new(),
        next_id: 1,
        files: HashMap::new(),
    };
    compact.entry(
        0,
        (
            2,
            "invocation",
            Msg::new()
                .str(1, "hash_function_name", "SHA-256")
                .str(
                    2,
                    "workspace_runfiles_directory",
                    crate::output_tree::MAIN_DIRECTORY,
                )
                .str(4, "id", crate::bep::invocation_id()),
        ),
    );
    for spawn in spawns {
        compact.spawn(spawn);
    }
    compact.out
}

/// Write the log (when one is recorded). Idempotent.
pub fn finish() {
    if !active() {
        return;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    let Some(state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let options = &state.options;
    let order: Vec<usize> = if options.sort {
        stable_order(&state.spawns)
    } else {
        (0..state.spawns.len()).collect()
    };
    let result = if let Some(path) = &options.compact_file {
        let path = state.cwd.join(path);
        let bytes = compact_stream(&state.spawns);
        let compressed = ruzstd::encoding::compress_to_vec(
            bytes.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        write_file(&path, &compressed)
    } else if let Some(path) = &options.json_file {
        let path = state.cwd.join(path);
        let mut text = String::new();
        for index in &order {
            text.push_str(&spawn_exec(&state.spawns[*index]).to_json_pretty());
        }
        write_file(&path, text.as_bytes())
    } else if let Some(path) = &options.binary_file {
        let path = state.cwd.join(path);
        let mut bytes = Vec::new();
        for index in &order {
            bytes.extend_from_slice(&crate::bep::proto::delimited(
                &spawn_exec(&state.spawns[*index]).encode(),
            ));
        }
        write_file(&path, &bytes)
    } else {
        Ok(())
    };
    if let Err(error) = result {
        crate::note!("WARNING: Failed to write the execution log: {error}");
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // snapshot-unrecorded: the execution log file (a write).
    let mut file =
        std::fs::File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(output: &str, input: &str) -> Spawn {
        Spawn {
            args: vec!["/bin/true".to_owned()],
            environment: vec![("PATH".to_owned(), "/bin".to_owned())],
            inputs: vec![LogFile {
                path: input.to_owned(),
                digest: Some(("ab".to_owned(), 2)),
                symlink_target: None,
                directory: None,
            }],
            listed_outputs: vec![output.to_owned()],
            actual_outputs: vec![(
                output.to_owned(),
                Some(vec![LogFile {
                    path: output.to_owned(),
                    digest: Some(("cd".to_owned(), 3)),
                    symlink_target: None,
                    directory: None,
                }]),
            )],
            remotable: true,
            cacheable: true,
            remote_cacheable: true,
            mnemonic: "Genrule".to_owned(),
            runner: "linux-sandbox".to_owned(),
            cache_hit: false,
            status: String::new(),
            exit_code: 0,
            target_label: "//pkg:a".to_owned(),
            start: SystemTime::UNIX_EPOCH,
            wall: Duration::from_millis(6),
        }
    }

    #[test]
    fn options_allow_one_log() {
        let mut options = Options::default();
        assert_eq!(
            options.parse("--execution_log_json_file=/x", None),
            Some(Ok(false))
        );
        assert!(options.validate().is_ok());
        assert_eq!(
            options.parse("--execution_log_compact_file", Some("y")),
            Some(Ok(true))
        );
        assert!(
            options
                .validate()
                .unwrap_err()
                .starts_with("Must specify at most one")
        );
        assert_eq!(
            options.parse("--noexecution_log_sort", None),
            Some(Ok(false))
        );
        assert!(!options.sort);
    }

    #[test]
    fn producers_sort_first_and_the_compact_stream_references_earlier_entries() {
        let spawns = vec![spawn("out/b", "out/a"), spawn("out/a", "src/x")];
        assert_eq!(stable_order(&spawns), vec![1, 0]);
        let json = spawn_exec(&spawns[0]).to_json_pretty();
        assert!(
            json.starts_with("{\n  \"commandArgs\": [\"/bin/true\"],"),
            "{json}"
        );
        assert!(json.contains("\"isTool\": false,\n    \"symlinkTargetPath\": \"\""));
        assert!(json.contains("\"totalTime\": \"0.006s\""));
        let stream = compact_stream(&spawns);
        let compressed = ruzstd::encoding::compress_to_vec(
            stream.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let mut decoded = Vec::new();
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice()).unwrap();
        std::io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        assert_eq!(decoded, stream);
    }
}
