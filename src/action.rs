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
//!
//! Tree artifacts (declared directories, docs/ACTIONS_PARAMS.md) are directory
//! outputs and inputs: their regular files and subdirectories are keyed,
//! staged and published; symlinks inside a tree are rejected explicitly.
//!
//! Staging and publication never copy file contents (docs/HARDLINK_STAGING.md).
//! Every staged or published regular file is an inode of the cache's
//! content-addressed store `cas/<sha256>-<mode>`, whose blobs are read-only
//! (0444 plus the keyed executable bits). Inputs are hardlinked from the
//! store into the execution root, like Bazel's hermetic linux-sandbox
//! (sandbox/HardlinkedSandboxedSpawn.java:80-102, a copy when a hardlink
//! fails); one content (and executable bits) is one inode, so a file reachable
//! at two input paths has one identity. Bytes from outside the cache are
//! copied into the store once. Output files are linked into the store from
//! the execution root (Bazel moves outputs by rename,
//! sandbox/SandboxHelpers.java:178-236). A process that modifies a staged
//! input in place fails like LinuxSandboxedSpawnRunner
//! .checkForConcurrentModifications (:478-521) and the damaged blob is
//! discarded. `actions.symlink` outputs are real symlinks to the target's
//! input path (analysis/actions/SymlinkAction.java:218-238).

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

// The key identity stays `native-action-v5`: actions without N23d features
// (no tree artifacts, parameter files, exact environment, execution
// requirements or transforms) keep their keys; those features are framed in
// an extra block. Hardlink staging (N36) changes no key. Manifests record
// symlink outputs since N36, so their format is v7.
const FORMAT: &str = "native-action-v5";
const MANIFEST_FORMAT: &str = "native-action-v7";
/// The content-addressed store below the cache root.
const CAS: &str = "cas";
/// Read and copy buffer for streamed digests.
const BUFFER_BYTES: usize = 1 << 20;
/// A digest of a file outside the cache is memoized only when its mtime and
/// ctime are this much older than the start of hashing ("racy git"): a later
/// in-place change then necessarily carries a different timestamp.
const RACY_MARGIN: Duration = Duration::from_secs(2);
const DIGEST_MEMO_LIMIT: usize = 1 << 20;
/// Published symlink outputs followed when they are inputs (a chain).
const MAX_LINK_DEPTH: usize = 32;
// Bazel 9.3 OutputPermissions.READONLY: all completed regular action outputs,
// including non-executable writes, normalize after execution, not during it.
const OUTPUT_MODE: u32 = 0o555;
// Completed tree artifacts: every entry below the root (files and
// subdirectories) is chmod'ed by
// ActionOutputMetadataStore.constructTreeArtifactValueFromFilesystem
// (skyframe/ActionOutputMetadataStore.java:245-281); genuine Bazel 9.3 also
// leaves the root read-only (0555, observed by the paired capture,
// tests/fixtures/actions_params/reference_receipt.json).
const TREE_ROOT_MODE: u32 = 0o555;
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
    /// Starlark-action features (docs/ACTIONS_PARAMS.md).
    pub extra: ActionExtra,
}

#[derive(Clone, Debug, Default)]
pub struct ActionExtra {
    /// Outputs (members of `Action::outputs`) that are tree artifacts.
    pub tree_outputs: BTreeSet<PathBuf>,
    /// Input destinations that are tree artifacts (directories); every other
    /// input must be a regular file.
    pub tree_inputs: BTreeSet<PathBuf>,
    /// Parameter files (CommandLines.ParamFileActionInput): written below
    /// the execution root before the process starts; content-keyed.
    pub param_files: Vec<(PathBuf, Vec<u8>)>,
    /// Starlark spawn environment: exactly `environment` plus TMPDIR
    /// (PosixLocalEnvProvider), without the PATH/LANG/LC_ALL defaults.
    pub exact_environment: bool,
    /// Execution requirements: keyed, no local effect.
    pub execution_info: BTreeMap<String, String>,
    /// A native, process-free transform of one input into the only output.
    pub transform: Option<Transform>,
}

#[derive(Clone, Debug)]
pub enum Transform {
    /// SymlinkAction: the output is a real symlink to the target's absolute
    /// input path, so it observes the target's bytes, permission bits and
    /// identity (SymlinkAction.java:218-238).
    Symlink {
        target: PathBuf,
        require_executable: bool,
        /// The expanded progress message that prefixes execution failures
        /// (`<progress message> failed: ...`); not keyed, like Bazel.
        progress: String,
    },
    /// TemplateExpansionAction: literal, sequential replacement of internal
    /// bytes (LocalTemplateExpansionStrategy.getExpandedTemplateUnsafe).
    Template {
        template: PathBuf,
        substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        executable: bool,
    },
    /// An action whose execution is an explicit native gap (its outputs are
    /// declared and registered, e.g. workspace-status transforms).
    Unsupported { message: String },
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
    /// Declaration-order output index of each tree output: its regular files,
    /// relative to the tree root, in byte order (TreeArtifactValue children).
    pub trees: BTreeMap<usize, Vec<PathBuf>>,
}

/// A regular file's content identity; its bytes stay on disk at `path` (the
/// resolved file they are read, linked or copied from).
#[derive(Clone)]
struct Input {
    path: PathBuf,
    digest: String,
    size: u64,
    executable: u32,
    mode: u32,
}

impl PartialEq for Input {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
            && self.executable == other.executable
            && self.mode == other.mode
    }
}

impl Eq for Input {}

#[derive(Clone, PartialEq, Eq)]
struct Tree {
    files: BTreeMap<PathBuf, Input>,
    directories: BTreeSet<PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
enum Staged {
    File(Input),
    Tree(Tree),
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
    mode: u32,
    tree: Option<TreeManifest>,
    /// A symlink output's hex-encoded target (Transform::Symlink).
    symlink: Option<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TreeManifest {
    files: Vec<(String, String)>,
    directories: Vec<String>,
}

struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = remove_directory(&self.0);
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

fn frame_count(hash: &mut Sha256, count: usize) {
    frame(hash, &(count as u64).to_le_bytes());
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
fn no_linked_ancestor(path: &Path) -> Result<fs::Metadata, String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&prefix).map_err(|e| io_error("cannot inspect", &prefix, e))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symlink is not allowed: '{}'", prefix.display()));
        }
    }
    fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))
}

fn regular_file(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = no_linked_ancestor(path)?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    Ok(metadata)
}

fn real_directory(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = no_linked_ancestor(path)?;
    if !metadata.is_dir() {
        return Err(format!("expected directory: '{}'", path.display()));
    }
    Ok(metadata)
}

/// Identity of a file state for the digest memo, after Bazel's
/// DigestUtils.CacheKey (vfs/DigestUtils.java:36-60: node id, change time,
/// modification time and size). Files owned by the cache (store blobs,
/// published outputs) leave out the change time: hardlinking a blob updates
/// it constantly, while its bytes are immutable and verified.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct FileKey {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn file_key(metadata: &fs::Metadata, owned: bool) -> FileKey {
    FileKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: if owned {
            (0, 0)
        } else {
            (metadata.ctime(), metadata.ctime_nsec())
        },
    }
}

fn digest_memo() -> &'static Mutex<HashMap<FileKey, String>> {
    static MEMO: OnceLock<Mutex<HashMap<FileKey, String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memoized_digest(key: &FileKey) -> Option<String> {
    digest_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

fn remember_digest(key: FileKey, digest: &str) {
    let mut memo = digest_memo().lock().unwrap_or_else(|e| e.into_inner());
    if memo.len() >= DIGEST_MEMO_LIMIT {
        memo.clear();
    }
    memo.insert(key, digest.to_owned());
}

fn timestamp(seconds: i64, nanoseconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanoseconds as u32))
}

/// Whether a file's timestamps are old enough that a change made after
/// `start` could not reproduce them. A file the cache owns (store blob,
/// published output) is judged by its mtime only, like its memo key: every
/// link() to it bumps its ctime, while its bytes are immutable and verified.
fn racy_safe(metadata: &fs::Metadata, start: SystemTime, owned: bool) -> bool {
    let mut times = vec![(metadata.mtime(), metadata.mtime_nsec())];
    if !owned {
        times.push((metadata.ctime(), metadata.ctime_nsec()));
    }
    times.into_iter().all(|(seconds, nanoseconds)| {
        timestamp(seconds, nanoseconds)
            .and_then(|time| time.checked_add(RACY_MARGIN))
            .is_some_and(|time| time <= start)
    })
}

/// Streamed SHA-256 of a regular file. The file must not change while it is
/// read. `trusted` memoizes regardless of timestamps: files this executor
/// has just written and made read-only.
fn hash_file(path: &Path, owned: bool, trusted: bool) -> Result<(String, fs::Metadata), String> {
    let before = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    let key = file_key(&before, owned);
    if let Some(digest) = memoized_digest(&key) {
        return Ok((digest, before));
    }
    let start = SystemTime::now();
    let mut file = File::open(path).map_err(|e| io_error("cannot read", path, e))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| io_error("cannot read", path, e))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .map_err(|e| io_error("cannot inspect", path, e))?;
    if file_key(&after, owned) != key {
        return Err(format!(
            "input '{}' changed while it was being read",
            path.display()
        ));
    }
    let digest = hex(&hash.finalize());
    if trusted || racy_safe(&after, start, owned) {
        remember_digest(key, &digest);
    }
    Ok((digest, before))
}

fn owned_by(root: Option<&Path>, path: &Path) -> bool {
    root.is_some_and(|root| path.starts_with(root))
}

/// Follow published symlink outputs (`<root>/actions/.../outputs/...`, see
/// Transform::Symlink) to the file or directory they name. Any other symlink
/// is left for the regular-file checks to reject.
fn resolve_published(path: &Path, root: Option<&Path>) -> Result<PathBuf, String> {
    let Some(root) = root else {
        return Ok(path.to_path_buf());
    };
    let actions = root.join("actions");
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_DEPTH {
        if !current.starts_with(&actions) {
            return Ok(current);
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if let Some(parent) = current.parent() {
                    no_linked_ancestor(parent)?;
                }
                let target = fs::read_link(&current)
                    .map_err(|e| io_error("cannot read link", &current, e))?;
                if !target.is_absolute() {
                    return Err(format!(
                        "published symlink '{}' has a relative target",
                        current.display()
                    ));
                }
                current = target;
            }
            _ => return Ok(current),
        }
    }
    Err(format!(
        "too many levels of published symlinks: '{}'",
        path.display()
    ))
}

/// Digest and mode of a regular file; `root` (the canonical cache root)
/// enables following published symlink outputs when `follow` is set.
fn read_input(path: &Path, root: Option<&Path>, follow: bool) -> Result<Input, String> {
    let path = if follow {
        resolve_published(path, root)?
    } else {
        path.to_path_buf()
    };
    regular_file(&path)?;
    read_regular(&path, owned_by(root, &path), false)
}

/// read_input for a path already checked to be a regular file.
fn read_regular(path: &Path, owned: bool, trusted: bool) -> Result<Input, String> {
    let (digest, metadata) = hash_file(path, owned, trusted)?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    Ok(Input {
        path: path.to_path_buf(),
        digest,
        size: metadata.size(),
        executable: metadata.permissions().mode() & 0o111,
        mode: metadata.permissions().mode() & 0o7777,
    })
}

/// Walk a directory without following symlinks. Entries are visited in byte
/// order; a symlink or special file below the root is an explicit error.
/// `normalize` makes every file OUTPUT_MODE before it is read (completed
/// tree outputs, ActionOutputMetadataStore.java:245-281); such files are the
/// executor's own and are memoized as trusted.
fn read_tree(root: &Path, cache: Option<&Path>, normalize: bool) -> Result<Tree, String> {
    real_directory(root)?;
    let owned = owned_by(cache, root);
    let mut tree = Tree {
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    };
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let mut entries = fs::read_dir(&directory)
            .map_err(|e| io_error("cannot list directory", &directory, e))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io_error("cannot list directory", &directory, e))?;
        entries.sort();
        for name in entries {
            let child = relative.join(&name);
            let path = root.join(&child);
            let metadata =
                fs::symlink_metadata(&path).map_err(|e| io_error("cannot inspect", &path, e))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "tree artifact entry '{}' is a symlink; symlinks inside tree artifacts are not supported by the native profile",
                    path.display()
                ));
            } else if metadata.is_dir() {
                tree.directories.insert(child.clone());
                pending.push(child);
            } else if metadata.is_file() {
                if normalize {
                    set_mode(&path, OUTPUT_MODE)?;
                }
                tree.files
                    .insert(child, read_regular(&path, owned, normalize)?);
            } else {
                return Err(format!(
                    "tree artifact entry '{}' is not a regular file or directory",
                    path.display()
                ));
            }
        }
    }
    Ok(tree)
}

fn read_staged(path: &Path, tree: bool, root: Option<&Path>) -> Result<Staged, String> {
    if tree {
        let resolved = resolve_published(path, root)?;
        Ok(Staged::Tree(read_tree(&resolved, root, false)?))
    } else {
        Ok(Staged::File(read_input(path, root, true)?))
    }
}

/// Byte-ordered relative paths of a tree's regular files.
fn tree_listing(tree: &Tree) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = tree.files.keys().cloned().collect();
    files.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    files
}

struct PreparedAction {
    inputs: BTreeMap<PathBuf, Staged>,
    /// Absolute input paths by destination (the first mapping of a
    /// duplicated destination): symlink outputs point at them.
    origins: BTreeMap<PathBuf, PathBuf>,
    outputs: Vec<PathBuf>,
    key: String,
}

fn check_one_output(action: &Action, kind: &str) -> Result<(), String> {
    if !action.command.is_empty()
        || action.program.is_some()
        || !action.arguments.is_empty()
        || !action.environment.is_empty()
        || !action.extra.param_files.is_empty()
        || action.outputs.len() != 1
    {
        return Err(format!(
            "{kind} action requires exactly one output, no program, and empty command, arguments, environment, and parameter files"
        ));
    }
    Ok(())
}

/// Validate commands and mapped input/output namespaces without filesystem I/O.
///
/// Writes require one output and no program, command, arguments, environment, or inputs.
/// Their content is arbitrary bytes, including NULs, with no argv size limit.
/// Direct runs require a nonempty, NUL-free program and an empty shell command.
/// Empty output lists are permitted for process actions. Duplicate input names
/// are permitted here; preparation checks that their contents and modes agree.
/// Transforms (symlink, template) require one output and their source among
/// the input destinations; parameter files must not overlap other paths.
/// Whether two namespace paths (distinct input destinations, outputs and
/// parameter files) are equal or one contains the other. In component order
/// every path sorts directly before the paths below it, so adjacent pairs
/// suffice: O(n log n) for the large input sets of toolchains and SDKs.
fn namespace_may_overlap(action: &Action) -> bool {
    let inputs: BTreeSet<&Path> = action
        .inputs
        .iter()
        .map(|(_, destination)| destination.as_path())
        .collect();
    let mut paths: Vec<&Path> = inputs.into_iter().collect();
    paths.extend(action.outputs.iter().map(PathBuf::as_path));
    paths.extend(
        action
            .extra
            .param_files
            .iter()
            .map(|(path, _)| path.as_path()),
    );
    paths.sort();
    paths.windows(2).any(|pair| pair[1].starts_with(pair[0]))
}

pub fn validate_structure(action: &Action) -> Result<(), String> {
    if action.write.is_some()
        && (!action.command.is_empty()
            || action.program.is_some()
            || !action.arguments.is_empty()
            || !action.environment.is_empty()
            // Only tree inputs, which written Args expand (ParameterFileWriteAction).
            || action
                .inputs
                .iter()
                .any(|(_, name)| !action.extra.tree_inputs.contains(name))
            || !action.extra.param_files.is_empty()
            || action.extra.transform.is_some()
            || action.outputs.len() != 1)
    {
        return Err("write action requires exactly one output, no program, and empty command, arguments, environment, and inputs".into());
    }
    if action.write.is_some() && !action.extra.tree_outputs.is_empty() {
        return Err("write action output cannot be a tree artifact".into());
    }
    match &action.extra.transform {
        Some(Transform::Symlink { target, .. }) => {
            check_one_output(action, "symlink")?;
            if !action.inputs.iter().any(|(_, name)| name == target) {
                return Err("symlink action target must be an input".into());
            }
        }
        Some(Transform::Template { template, .. }) => {
            check_one_output(action, "template")?;
            if !action.inputs.iter().any(|(_, name)| name == template) {
                return Err("template action template must be an input".into());
            }
            if !action.extra.tree_outputs.is_empty() {
                return Err("template action output cannot be a tree artifact".into());
            }
        }
        Some(Transform::Unsupported { .. }) => check_one_output(action, "unsupported")?,
        None => {}
    }
    for tree in &action.extra.tree_inputs {
        if !action.inputs.iter().any(|(_, name)| name == tree) {
            return Err(format!(
                "tree input '{}' is not an action input",
                tree.display()
            ));
        }
    }
    for tree in &action.extra.tree_outputs {
        if !action.outputs.contains(tree) {
            return Err(format!(
                "tree output '{}' is not an action output",
                tree.display()
            ));
        }
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
    for (name, value) in action
        .environment
        .iter()
        .chain(&action.extra.execution_info)
    {
        if name.is_empty() || name.as_bytes().contains(&b'=') || name.as_bytes().contains(&0) {
            return Err("environment key must be nonempty and contain neither '=' nor NUL".into());
        }
        if value.as_bytes().contains(&0) {
            return Err("environment value contains a NUL byte".into());
        }
    }
    // Validate the complete namespace before reading inputs or invoking processes.
    for path in action
        .inputs
        .iter()
        .map(|(_, destination)| destination)
        .chain(&action.outputs)
        .chain(action.extra.param_files.iter().map(|(path, _)| path))
    {
        validate_relative(path)?;
    }
    if !namespace_may_overlap(action) {
        return Ok(());
    }
    // Some paths overlap: find the first one in declaration order, quadratically.
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
    let mut params: Vec<&PathBuf> = Vec::new();
    for (path, _) in &action.extra.param_files {
        validate_relative(path)?;
        if input_names
            .iter()
            .chain(outputs.iter())
            .chain(params.iter().copied())
            .any(|p| overlaps(p, path))
        {
            return Err(format!(
                "parameter file '{}' overlaps another action path",
                path.display()
            ));
        }
        params.push(path);
    }
    Ok(())
}

fn frame_input(hash: &mut Sha256, input: &Input) {
    frame(hash, input.digest.as_bytes());
    frame(hash, &input.executable.to_le_bytes());
}

fn frame_staged(hash: &mut Sha256, staged: &Staged) {
    match staged {
        Staged::File(input) => frame_input(hash, input),
        Staged::Tree(tree) => {
            frame(hash, b"tree");
            frame_count(hash, tree.files.len());
            for (name, input) in &tree.files {
                frame(hash, name.as_os_str().as_bytes());
                frame_input(hash, input);
            }
            frame_count(hash, tree.directories.len());
            for name in &tree.directories {
                frame(hash, name.as_os_str().as_bytes());
            }
        }
    }
}

/// Validate, read and key an action. `root` is the canonical cache root when
/// the cache exists: published symlink outputs below it are followed.
fn prepare(action: &Action, root: Option<&Path>) -> Result<PreparedAction, String> {
    validate_structure(action)?;
    let mut outputs = action.outputs.clone();
    outputs.sort();
    let mut inputs: BTreeMap<PathBuf, Staged> = BTreeMap::new();
    let mut origins: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for (source, destination) in &action.inputs {
        let input = read_staged(source, action.extra.tree_inputs.contains(destination), root)?;
        if !origins.contains_key(destination) {
            let origin = std::path::absolute(source)
                .map_err(|e| io_error("cannot resolve input", source, e))?;
            origins.insert(destination.clone(), origin);
        }
        if let Some(previous) = inputs.get(destination) {
            let same = match (previous, &input) {
                (Staged::File(a), Staged::File(b)) => {
                    a.digest == b.digest && a.executable == b.executable
                }
                (a, b) => a == b,
            };
            if !same {
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
    match (&action.write, &action.program, &action.extra.transform) {
        (Some(write), _, _) => {
            frame(&mut hash, b"write");
            frame(&mut hash, &write.content);
            frame(&mut hash, &[u8::from(write.executable)]);
        }
        (
            None,
            _,
            Some(Transform::Symlink {
                target,
                require_executable,
                ..
            }),
        ) => {
            frame(&mut hash, b"symlink");
            frame(&mut hash, target.as_os_str().as_bytes());
            frame(&mut hash, &[u8::from(*require_executable)]);
            // The published copy carries the target's complete permission bits.
            if let Some(Staged::File(input)) = inputs.get(target) {
                frame(&mut hash, &input.mode.to_le_bytes());
            }
        }
        (
            None,
            _,
            Some(Transform::Template {
                template,
                substitutions,
                executable,
            }),
        ) => {
            frame(&mut hash, b"template");
            frame(&mut hash, template.as_os_str().as_bytes());
            frame_count(&mut hash, substitutions.len());
            for (key, value) in substitutions {
                frame(&mut hash, key);
                frame(&mut hash, value);
            }
            frame(&mut hash, &[u8::from(*executable)]);
        }
        (None, _, Some(Transform::Unsupported { message })) => {
            frame(&mut hash, b"unsupported");
            frame(&mut hash, message.as_bytes());
        }
        (None, Some(program), None) => {
            frame(&mut hash, b"direct");
            frame(&mut hash, program.as_bytes());
        }
        (None, None, None) => {
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
        frame_staged(&mut hash, input);
    }
    frame(&mut hash, &(outputs.len() as u64).to_le_bytes());
    for name in &outputs {
        frame(&mut hash, name.as_os_str().as_bytes());
    }
    let extra = &action.extra;
    if !extra.tree_outputs.is_empty()
        || !extra.tree_inputs.is_empty()
        || !extra.param_files.is_empty()
        || extra.exact_environment
        || !extra.execution_info.is_empty()
        || extra.transform.is_some()
    {
        frame(&mut hash, b"n23d-extra");
        frame_count(&mut hash, extra.tree_outputs.len());
        for name in &extra.tree_outputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
        frame_count(&mut hash, extra.tree_inputs.len());
        for name in &extra.tree_inputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
        frame(&mut hash, &[u8::from(extra.exact_environment)]);
        frame_count(&mut hash, extra.execution_info.len());
        for (name, value) in &extra.execution_info {
            frame(&mut hash, name.as_bytes());
            frame(&mut hash, value.as_bytes());
        }
        let mut params: Vec<&(PathBuf, Vec<u8>)> = extra.param_files.iter().collect();
        params.sort_by(|a, b| a.0.cmp(&b.0));
        frame_count(&mut hash, params.len());
        for (path, content) in params {
            frame(&mut hash, path.as_os_str().as_bytes());
            frame(&mut hash, content);
        }
    }
    Ok(PreparedAction {
        inputs,
        origins,
        outputs,
        key: hex(&hash.finalize()),
    })
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

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| io_error("cannot set permissions", path, e))
}

fn write_file(path: &Path, bytes: &[u8], executable: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent)?;
    }
    fs::write(path, bytes).map_err(|e| io_error("cannot write", path, e))?;
    set_mode(path, 0o644 | executable)
}

/// Remove a directory tree, first making read-only (0555) tree directories
/// writable again. Symlinks are never followed.
fn remove_directory(path: &Path) -> io::Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() {
            continue;
        }
        if metadata.permissions().mode() & 0o700 != 0o700 {
            fs::set_permissions(
                &directory,
                fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
            )?;
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(path)
}

/// Store blob permission bits: read-only, with the keyed executable bits.
fn blob_mode(input: &Input) -> u32 {
    0o444 | input.executable
}

fn blob_path(cas: &Path, input: &Input) -> PathBuf {
    cas.join(format!("{}-{:o}", input.digest, blob_mode(input)))
}

#[cfg(test)]
thread_local! {
    /// Test hook: every hardlink attempt fails with this errno (0: none),
    /// e.g. EXDEV (cross-device) or EMLINK (link count limit).
    static FORCE_LINK_ERRNO: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// Test hook: the next staged blob is removed after ensure_blob found it
    /// valid and before it is linked (a concurrent damaged-blob removal).
    static REMOVE_BLOB_BEFORE_LINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Test observation: (hardlinked, copied) staged and published files.
    static LINK_COUNTS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn count_link(linked: bool) {
    #[cfg(test)]
    LINK_COUNTS.with(|counts| {
        let (links, copies) = counts.get();
        counts.set(if linked {
            (links + 1, copies)
        } else {
            (links, copies + 1)
        });
    });
    #[cfg(not(test))]
    let _ = linked;
}

fn hard_link(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        let errno = FORCE_LINK_ERRNO.with(std::cell::Cell::get);
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
    fs::hard_link(source, target)
}

fn temporary(directory: &Path, stem: &str) -> PathBuf {
    directory.join(format!(
        ".tmp-{stem}-{}-{}",
        std::process::id(),
        NEXT_WORK.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Copy `source` to the new file `target` while hashing it; the copy must
/// have the expected digest.
fn copy_verified(source: &Path, target: &Path, expected: &str) -> Result<(), String> {
    let mut input = File::open(source).map_err(|e| io_error("cannot read", source, e))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| io_error("cannot write", target, e))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|e| io_error("cannot read", source, e))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|e| io_error("cannot write", target, e))?;
    }
    if hex(&hash.finalize()) != expected {
        let _ = fs::remove_file(target);
        return Err(format!(
            "input '{}' changed while it was being staged",
            source.display()
        ));
    }
    Ok(())
}

/// Whether the blob for `input` is present and intact: a read-only regular
/// file with the right size and digest. A damaged blob is removed.
fn blob_valid(blob: &Path, input: &Input) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(blob) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_error("cannot inspect", blob, e)),
    };
    let intact = metadata.is_file()
        && metadata.permissions().mode() & 0o7777 == blob_mode(input)
        && metadata.size() == input.size
        && hash_file(blob, true, false).is_ok_and(|(digest, _)| digest == input.digest);
    if !intact {
        remove_stale(blob)?;
    }
    Ok(intact)
}

/// The store blob of `input`, created on first use: by hardlinking a file
/// the cache owns that already has the blob's mode (a published output or
/// fresh execution-root output), otherwise by one verified copy. Concurrent
/// creators agree on the first linked inode.
fn ensure_blob(cas: &Path, input: &Input, root: &Path) -> Result<PathBuf, String> {
    let blob = blob_path(cas, input);
    if blob_valid(&blob, input)? {
        return Ok(blob);
    }
    // Workers of one process that need the same new blob create it once
    // (the others wait, then find it valid) instead of copying in parallel.
    static STRIPES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let stripe = u8::from_str_radix(&input.digest[..2], 16).unwrap_or(0) as usize % STRIPES.len();
    let _creating = STRIPES[stripe].lock().unwrap_or_else(|e| e.into_inner());
    if blob_valid(&blob, input)? {
        return Ok(blob);
    }
    let candidate = temporary(cas, &input.digest);
    let linked = input.path.starts_with(root)
        && input.mode == blob_mode(input)
        && hard_link(&input.path, &candidate).is_ok();
    if linked {
        let (digest, _) = hash_file(&candidate, true, false)?;
        if digest != input.digest {
            let _ = fs::remove_file(&candidate);
            return Err(format!(
                "input '{}' changed while it was being staged",
                input.path.display()
            ));
        }
    } else {
        copy_verified(&input.path, &candidate, &input.digest)?;
        set_mode(&candidate, blob_mode(input))?;
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|e| io_error("cannot inspect", &candidate, e))?;
        remember_digest(file_key(&metadata, true), &input.digest);
    }
    let published = match fs::hard_link(&candidate, &blob) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if blob_valid(&blob, input)? {
                Ok(())
            } else {
                fs::rename(&candidate, &blob).map_err(|e| io_error("cannot store", &blob, e))
            }
        }
        // A filesystem without hardlinks still allows a rename.
        Err(_) => fs::rename(&candidate, &blob).map_err(|e| io_error("cannot store", &blob, e)),
    };
    let _ = fs::remove_file(&candidate);
    published.map(|()| blob)
}

/// A failed [`link_or_copy_detail`]: the blob vanished (neither linkable
/// nor readable), or any other error.
enum LinkFailure {
    BlobMissing(String),
    Other(String),
}

/// Link `blob` to `target`, or copy it when the hardlink fails
/// (HardlinkedSandboxedSpawn.hardLinkRecursive, :85-102).
fn link_or_copy(blob: &Path, target: &Path, mode: u32) -> Result<(), String> {
    link_or_copy_detail(blob, target, mode).map_err(|failure| match failure {
        LinkFailure::BlobMissing(error) | LinkFailure::Other(error) => error,
    })
}

/// [`link_or_copy`] of the blob [`ensure_blob`] returned for `input`. The
/// blob can disappear between ensure_blob's validity check and the link
/// (another process removing it as damaged, a store cleanup): then the
/// blob is ensured once more and linked again; a second disappearance is
/// an error.
fn link_ensured_blob(
    cas: &Path,
    root: &Path,
    input: &Input,
    target: &Path,
    mode: u32,
) -> Result<PathBuf, String> {
    let blob = ensure_blob(cas, input, root)?;
    #[cfg(test)]
    if REMOVE_BLOB_BEFORE_LINK.with(|hook| hook.replace(false)) {
        let _ = fs::remove_file(&blob);
    }
    match link_or_copy_detail(&blob, target, mode) {
        Ok(()) => Ok(blob),
        Err(LinkFailure::BlobMissing(_)) => {
            let blob = ensure_blob(cas, input, root)?;
            link_or_copy(&blob, target, mode)?;
            Ok(blob)
        }
        Err(LinkFailure::Other(error)) => Err(error),
    }
}

fn link_or_copy_detail(blob: &Path, target: &Path, mode: u32) -> Result<(), LinkFailure> {
    let other = LinkFailure::Other;
    match hard_link(blob, target) {
        Ok(()) => {
            count_link(true);
            Ok(())
        }
        Err(_) => {
            let mut input = File::open(blob).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    LinkFailure::BlobMissing(io_error("cannot read", blob, e))
                } else {
                    LinkFailure::Other(io_error("cannot read", blob, e))
                }
            })?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(|e| other(io_error("cannot write", target, e)))?;
            io::copy(&mut input, &mut output)
                .map_err(|e| other(io_error("cannot copy", target, e)))?;
            drop(output);
            set_mode(target, mode).map_err(other)?;
            count_link(false);
            Ok(())
        }
    }
}

/// State of a staged inode that a process can change in place.
#[derive(Clone, Copy, PartialEq, Eq)]
struct InodeState {
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    mode: u32,
}

fn inode_state(metadata: &fs::Metadata) -> InodeState {
    InodeState {
        inode: metadata.ino(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        mode: metadata.permissions().mode() & 0o7777,
    }
}

/// A staged input file: its destination, store blob and inode state.
struct StagedFile {
    destination: PathBuf,
    blob: PathBuf,
    state: InodeState,
}

fn stage_file(
    cas: &Path,
    root: &Path,
    input: &Input,
    execroot: &Path,
    destination: &Path,
    staged: &mut Vec<StagedFile>,
) -> Result<(), String> {
    let target = execroot.join(destination);
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    let blob = link_ensured_blob(cas, root, input, &target, blob_mode(input))?;
    let metadata =
        fs::symlink_metadata(&target).map_err(|e| io_error("cannot inspect", &target, e))?;
    staged.push(StagedFile {
        destination: destination.to_path_buf(),
        blob,
        state: inode_state(&metadata),
    });
    Ok(())
}

/// Stage every input: files as store hardlinks, tree directories as real
/// (writable) directories holding hardlinked files.
fn stage_inputs(
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
    execroot: &Path,
) -> Result<Vec<StagedFile>, String> {
    let mut staged = Vec::new();
    for (name, input) in inputs {
        match input {
            Staged::File(input) => stage_file(cas, root, input, execroot, name, &mut staged)?,
            Staged::Tree(tree) => {
                ensure_directory(&execroot.join(name))?;
                for directory in &tree.directories {
                    ensure_directory(&execroot.join(name).join(directory))?;
                }
                for (file, input) in &tree.files {
                    stage_file(cas, root, input, execroot, &name.join(file), &mut staged)?;
                }
            }
        }
    }
    Ok(staged)
}

/// After a process ran: every staged inode (reached through its execution
/// root path, or the store when the process replaced that path) must be
/// unchanged. A damaged blob is removed so no later action stages it.
fn check_staged(staged: &[StagedFile], execroot: &Path) -> Result<(), String> {
    let mut modified = Vec::new();
    for file in staged {
        let state = |path: &Path| {
            fs::symlink_metadata(path)
                .ok()
                .map(|metadata| inode_state(&metadata))
                .filter(|state| state.inode == file.state.inode)
        };
        let current = state(&execroot.join(&file.destination)).or_else(|| state(&file.blob));
        if let Some(current) = current
            && current != file.state
        {
            if state(&file.blob).is_some() {
                let _ = fs::remove_file(&file.blob);
            }
            modified.push(file.destination.display().to_string());
        }
    }
    if modified.is_empty() {
        Ok(())
    } else {
        // LinuxSandboxedSpawnRunner.java:507-510.
        Err(modified
            .iter()
            .map(|path| format!("input dependency {path} was modified during execution."))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Publish an execution-root output file as `target`: normalized to
/// OUTPUT_MODE (OutputPermissions.READONLY) and linked to its store blob, so
/// identical outputs share one inode. A failed hardlink falls back to a
/// rename (SandboxHelpers.moveOutputs, :178-236), then a copy.
fn publish_file(cas: &Path, root: &Path, source: &Path, target: &Path) -> Result<Input, String> {
    regular_file(source)?;
    set_mode(source, OUTPUT_MODE)?;
    let input = read_regular(source, true, true)?;
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    let blob = ensure_blob(cas, &input, root)?;
    if hard_link(&blob, target).is_ok() || fs::rename(source, target).is_ok() {
        count_link(true);
    } else {
        link_ensured_blob(cas, root, &input, target, OUTPUT_MODE)?;
    }
    Ok(input)
}

/// Publish an execution-root tree output as `target` by renaming it, then
/// replace each file by its store blob's inode and make directories
/// read-only deepest first, the root TREE_ROOT_MODE.
fn publish_tree(cas: &Path, root: &Path, source: &Path, target: &Path) -> Result<Tree, String> {
    let tree = read_tree(source, Some(root), true)?;
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    fs::rename(source, target).map_err(|e| io_error("cannot publish tree", target, e))?;
    count_link(true);
    for (name, input) in &tree.files {
        let path = target.join(name);
        let input = Input {
            path: path.clone(),
            ..input.clone()
        };
        let blob = ensure_blob(cas, &input, root)?;
        let same = |a: &Path, b: &Path| -> bool {
            match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
                (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
                _ => false,
            }
        };
        if !same(&blob, &path) {
            let parent = path.parent().unwrap_or(target);
            let replacement = temporary(parent, "tree");
            if hard_link(&blob, &replacement).is_ok() {
                fs::rename(&replacement, &path)
                    .map_err(|e| io_error("cannot publish tree entry", &path, e))?;
            }
        }
    }
    let mut directories: Vec<&PathBuf> = tree.directories.iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        set_mode(&target.join(directory), OUTPUT_MODE)?;
    }
    set_mode(target, TREE_ROOT_MODE)?;
    Ok(tree)
}

fn tree_manifest(tree: &Tree) -> TreeManifest {
    TreeManifest {
        files: tree
            .files
            .iter()
            .map(|(name, input)| (hex(name.as_os_str().as_bytes()), input.digest.clone()))
            .collect(),
        directories: tree
            .directories
            .iter()
            .map(|name| hex(name.as_os_str().as_bytes()))
            .collect(),
    }
}

/// A published tree (or a symlink output's target tree) with completed-tree
/// permissions and the manifest's contents.
fn tree_valid(path: &Path, expected: &TreeManifest, root: &Path) -> Result<Option<Tree>, String> {
    if real_directory(path)?.permissions().mode() & 0o7777 != TREE_ROOT_MODE {
        return Ok(None);
    }
    let tree = read_tree(path, Some(root), false)?;
    if tree_manifest(&tree) != *expected
        || tree.files.values().any(|input| input.mode != OUTPUT_MODE)
    {
        return Ok(None);
    }
    for directory in &tree.directories {
        let mode = fs::symlink_metadata(path.join(directory))
            .map_err(|e| e.to_string())?
            .permissions()
            .mode();
        if mode & 0o7777 != OUTPUT_MODE {
            return Ok(None);
        }
    }
    Ok(Some(tree))
}

fn cache_valid(
    directory: &Path,
    key: &str,
    outputs: &[PathBuf],
    trees: &BTreeSet<PathBuf>,
    root: &Path,
) -> Option<BTreeMap<PathBuf, Vec<PathBuf>>> {
    let manifest_path = directory.join("manifest.json");
    let check = || -> Result<Option<BTreeMap<PathBuf, Vec<PathBuf>>>, String> {
        regular_file(&manifest_path)?;
        let bytes = fs::read(&manifest_path)
            .map_err(|e| io_error("cannot read manifest", &manifest_path, e))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if manifest.format != MANIFEST_FORMAT
            || manifest.key != key
            || manifest.outputs.len() != outputs.len()
        {
            return Ok(None);
        }
        let mut listings = BTreeMap::new();
        for (entry, name) in manifest.outputs.iter().zip(outputs) {
            if entry.name != hex(name.as_os_str().as_bytes()) {
                return Ok(None);
            }
            let mut path = directory.join("outputs").join(name);
            if let Some(target) = &entry.symlink {
                // A symlink output still names the recorded target, which
                // still has the recorded contents and mode.
                let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !metadata.file_type().is_symlink()
                    || hex(fs::read_link(&path)
                        .map_err(|e| e.to_string())?
                        .as_os_str()
                        .as_bytes())
                        != *target
                {
                    return Ok(None);
                }
                path = resolve_published(&path, Some(root))?;
            }
            match (&entry.tree, trees.contains(name)) {
                (Some(expected), true) => {
                    let Some(tree) = tree_valid(&path, expected, root)? else {
                        return Ok(None);
                    };
                    listings.insert(name.clone(), tree_listing(&tree));
                }
                (None, false) => {
                    let actual = read_input(&path, Some(root), false)?;
                    if actual.mode != entry.mode || actual.digest != entry.digest {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(listings))
    };
    check().unwrap_or(None)
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
                remove_directory(path)
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

/// StringUtilities.replaceAllLiteral (util/StringUtilities.java:70-88) on
/// internal bytes: non-overlapping, left to right; an empty key is a no-op.
fn replace_all_literal(input: &[u8], literal: &[u8], replacement: &[u8]) -> Vec<u8> {
    if literal.is_empty() {
        return input.to_vec();
    }
    let mut result = Vec::with_capacity(input.len());
    let mut start = 0;
    while let Some(offset) = input[start..]
        .windows(literal.len())
        .position(|window| window == literal)
    {
        result.extend_from_slice(&input[start..start + offset]);
        result.extend_from_slice(replacement);
        start += offset + literal.len();
    }
    result.extend_from_slice(&input[start..]);
    result
}

/// Run a native transform; the output is left in the execution root.
fn run_transform(
    action: &Action,
    transform: &Transform,
    prepared: &PreparedAction,
    execroot: &Path,
) -> Result<(), String> {
    let output = execroot.join(&action.outputs[0]);
    let inputs = &prepared.inputs;
    match transform {
        Transform::Unsupported { message } => Err(message.clone()),
        Transform::Template {
            template,
            substitutions,
            executable,
        } => {
            let Some(Staged::File(input)) = inputs.get(template) else {
                return Err(format!(
                    "failed to load template file '{}': not a regular file",
                    template.display()
                ));
            };
            let mut text =
                fs::read(&input.path).map_err(|e| io_error("cannot read", &input.path, e))?;
            if digest(&text) != input.digest {
                return Err(format!(
                    "input '{}' changed while it was being read",
                    input.path.display()
                ));
            }
            for (key, value) in substitutions {
                text = replace_all_literal(&text, key, value);
            }
            write_file(&output, &text, if *executable { 0o111 } else { 0 })
        }
        Transform::Symlink {
            target,
            require_executable,
            progress,
        } => {
            let tree_output = action.extra.tree_outputs.contains(&action.outputs[0]);
            match inputs.get(target) {
                Some(Staged::File(input)) if !tree_output => {
                    // SymlinkAction.maybeVerifyTargetIsExecutable
                    // (analysis/actions/SymlinkAction.java:296-330): S_IXUSR.
                    if *require_executable && input.mode & 0o100 == 0 {
                        return Err(format!(
                            "{progress} failed: failed to create symbolic link '{}': file '{}' is not executable",
                            action.outputs[0].display(),
                            target.display()
                        ));
                    }
                }
                Some(Staged::Tree(_)) if tree_output => {
                    // SymlinkAction.execute deletes the tree directory
                    // created before execution (:232-236).
                    remove_directory(&output)
                        .map_err(|e| io_error("cannot replace", &output, e))?;
                }
                _ => {
                    return Err(format!(
                        "symlink target '{}' and output '{}' must both be files or both be directories",
                        target.display(),
                        action.outputs[0].display()
                    ));
                }
            }
            // outputPath.createSymbolicLink(targetPath) with the target's
            // absolute input path (SymlinkAction.java:221-238).
            std::os::unix::fs::symlink(&prepared.origins[target], &output)
                .map_err(|e| io_error("cannot create symbolic link", &output, e))
        }
    }
}

/// Execute an action or return its verified cached outputs in declaration order.
///
/// Executable bits are preserved and keyed. Other permission bits, timestamps,
/// and source path spellings are not inputs. Only declared regular output files
/// and tree outputs are published; undeclared files are discarded. The cache uses per-key
/// cross-process locks, so unrelated actions may execute concurrently.
/// Shell commands are trusted and unsandboxed, run with Bash; `shell_strict` adds
/// -e -o pipefail for genrules, while custom run_shell actions use only -c.
/// All shell actions get an empty `$0`, then literal `$1`, `$2`, ... . Explicit environment
/// entries override PATH/LANG/LC_ALL/TMPDIR defaults; no caller variables leak in.
/// Starlark actions (`exact_environment`) get exactly their environment plus TMPDIR.
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
/// Input files are staged as read-only hardlinks to store blobs (0444 plus
/// their executable bits); a process that changes one in place fails.
pub fn execute(cache_root: &Path, action: &Action) -> Result<ActionResult, String> {
    let existing = fs::canonicalize(cache_root).ok();
    let prepared = prepare(action, existing.as_deref())?;
    let (inputs, outputs, key) = (&prepared.inputs, &prepared.outputs, &prepared.key);
    fs::create_dir_all(cache_root).map_err(|e| io_error("cannot create cache", cache_root, e))?;
    let root = fs::canonicalize(cache_root)
        .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
    let actions = root.join("actions");
    let locks = root.join("locks");
    let work_parent = root.join("work");
    let cas = root.join(CAS);
    for path in [&actions, &locks, &work_parent, &cas] {
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
    let cached = actions.join(key);
    let result = |listings: BTreeMap<PathBuf, Vec<PathBuf>>, cache_hit: bool| ActionResult {
        outputs: action
            .outputs
            .iter()
            .map(|p| cached.join("outputs").join(p))
            .collect(),
        cache_hit,
        trees: action
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(index, name)| Some((index, listings.get(name)?.clone())))
            .collect(),
    };
    if let Some(listings) = cache_valid(&cached, key, outputs, &action.extra.tree_outputs, &root) {
        return Ok(result(listings, true));
    }
    remove_stale(&cached)?;
    let work = fresh_work(&work_parent, key)?;
    let execroot = work.0.join("execroot");
    let tmp = work.0.join("tmp");
    ensure_directory(&execroot)?;
    ensure_directory(&tmp)?;
    let process = action.write.is_none() && action.extra.transform.is_none();
    // Only a process reads its inputs through the execution root; native
    // writes and transforms read the verified input files directly.
    let staged = if process {
        stage_inputs(&cas, &root, inputs, &execroot)?
    } else {
        Vec::new()
    };
    for (name, content) in &action.extra.param_files {
        write_file(&execroot.join(name), content, 0)?;
    }
    for name in outputs {
        if let Some(parent) = execroot.join(name).parent() {
            ensure_directory(parent)?;
        }
        // Bazel creates tree artifact directories before execution.
        if action.extra.tree_outputs.contains(name) {
            ensure_directory(&execroot.join(name))?;
        }
    }
    if let Some(write) = &action.write {
        write_file(
            &execroot.join(&outputs[0]),
            &write.content,
            if write.executable { 0o111 } else { 0 },
        )?;
    } else if let Some(transform) = &action.extra.transform {
        run_transform(action, transform, &prepared, &execroot)?;
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
        command.current_dir(&execroot).env_clear();
        if !action.extra.exact_environment {
            command.envs(ENV);
        }
        command
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
        check_staged(&staged, &execroot).map_err(|e| format!("action '{}': {e}", action.id))?;
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
    // Publish every output into the completed directory by link or rename,
    // never by copying contents; the manifest describes exactly what is
    // published.
    let symlink_target = match &action.extra.transform {
        Some(Transform::Symlink { target, .. }) => Some(target),
        _ => None,
    };
    let completed = work.0.join("completed");
    ensure_directory(&completed.join("outputs"))?;
    let mut manifest = Manifest {
        format: MANIFEST_FORMAT.into(),
        key: key.clone(),
        outputs: Vec::new(),
    };
    let mut listings = BTreeMap::new();
    for name in outputs {
        let source = execroot.join(name);
        let target = completed.join("outputs").join(name);
        let tree_output = action.extra.tree_outputs.contains(name);
        let mut publish = || -> Result<Output, String> {
            let hex_name = hex(name.as_os_str().as_bytes());
            if let Some(symlink_target) = symlink_target {
                let metadata = fs::symlink_metadata(&source)
                    .map_err(|e| io_error("cannot inspect", &source, e))?;
                if !metadata.file_type().is_symlink() {
                    return Err(format!("expected symlink: '{}'", source.display()));
                }
                let link =
                    fs::read_link(&source).map_err(|e| io_error("cannot read link", &source, e))?;
                if let Some(parent) = target.parent() {
                    ensure_directory(parent)?;
                }
                fs::rename(&source, &target).map_err(|e| io_error("cannot publish", &target, e))?;
                let symlink = Some(hex(link.as_os_str().as_bytes()));
                return Ok(match &inputs[symlink_target] {
                    Staged::File(input) => Output {
                        name: hex_name,
                        digest: input.digest.clone(),
                        mode: input.mode,
                        tree: None,
                        symlink,
                    },
                    Staged::Tree(tree) => {
                        listings.insert(name.clone(), tree_listing(tree));
                        Output {
                            name: hex_name,
                            digest: String::new(),
                            mode: TREE_ROOT_MODE,
                            tree: Some(tree_manifest(tree)),
                            symlink,
                        }
                    }
                });
            }
            if tree_output {
                let tree = publish_tree(&cas, &root, &source, &target)?;
                listings.insert(name.clone(), tree_listing(&tree));
                return Ok(Output {
                    name: hex_name,
                    digest: String::new(),
                    mode: TREE_ROOT_MODE,
                    tree: Some(tree_manifest(&tree)),
                    symlink: None,
                });
            }
            // Match metadata collection order: a successful action may leave a
            // regular output unreadable. Normalize before reading its bytes,
            // but never chmod a symlink or a path through a linked ancestor.
            let input = publish_file(&cas, &root, &source, &target)?;
            Ok(Output {
                name: hex_name,
                digest: input.digest,
                mode: OUTPUT_MODE,
                tree: None,
                symlink: None,
            })
        };
        let output = publish().map_err(|e| {
            format!(
                "action '{}' has missing or invalid output '{}': {e}",
                action.id,
                name.display()
            )
        })?;
        manifest.outputs.push(output);
    }
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("cannot encode manifest: {e}"))?;
    write_file(&completed.join("manifest.json"), &manifest_bytes, 0)?;
    // Same-filesystem rename publishes all declared files and the manifest at
    // once. Crash durability is not promised; damaged entries rebuild on use.
    fs::rename(&completed, &cached).map_err(|e| io_error("cannot publish action", &cached, e))?;
    Ok(result(listings, false))
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
                extra: ActionExtra::default(),
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
                extra: ActionExtra::default(),
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
                extra: ActionExtra::default(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Published tree artifacts are read-only (0555) directories.
            let _ = remove_directory(&self.0);
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
        let write_key = prepare(&empty, None).unwrap().key;
        empty.write = None;
        assert_ne!(write_key, prepare(&empty, None).unwrap().key);
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
        let direct_key = prepare(&action, None).unwrap().key;
        action.program = None;
        assert_ne!(direct_key, prepare(&action, None).unwrap().key);
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

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn tree_outputs_are_published_keyed_and_staged_as_tree_inputs() {
        let fixture = Fixture::new();
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into()];
        producer.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        producer.command =
            "test -d out/tree && mkdir -p out/tree/sub/empty && printf a > out/tree/a && printf b > out/tree/sub/b && chmod 600 out/tree/a".into();
        let first = execute(&fixture.cache(), &producer).unwrap();
        let tree = &first.outputs[0];
        assert_eq!(mode(tree), TREE_ROOT_MODE);
        for (name, expected) in [
            ("a", OUTPUT_MODE),
            ("sub", OUTPUT_MODE),
            ("sub/b", OUTPUT_MODE),
            ("sub/empty", OUTPUT_MODE),
        ] {
            assert_eq!(mode(&tree.join(name)), expected, "{name}");
        }
        assert_eq!(
            first.trees[&0],
            [PathBuf::from("a"), PathBuf::from("sub/b")]
        );
        let hit = execute(&fixture.cache(), &producer).unwrap();
        assert!(hit.cache_hit);
        assert_eq!(hit.trees, first.trees);
        // Tampering with a published tree entry rebuilds it.
        fs::set_permissions(tree.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(tree.join("sub/b"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(tree.join("sub/b"), "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(fs::read(tree.join("sub/b")).unwrap(), b"b");
        // A consumer stages the tree; its key covers the tree contents.
        let mut consumer = fixture.action();
        consumer.inputs = vec![(tree.clone(), "in/tree".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
        consumer.command =
            "cat in/tree/a in/tree/sub/b > out/result; test -d in/tree/sub/empty".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(fs::read(&consumed.outputs[0]).unwrap(), b"ab");
        producer.command = producer.command.replace("printf b", "printf c");
        let changed = execute(&fixture.cache(), &producer).unwrap();
        consumer.inputs = vec![(changed.outputs[0].clone(), "in/tree".into())];
        let reconsumed = execute(&fixture.cache(), &consumer).unwrap();
        assert!(!reconsumed.cache_hit);
        assert_eq!(fs::read(&reconsumed.outputs[0]).unwrap(), b"ac");
        // Without the tree flag a directory input is still rejected.
        consumer.extra.tree_inputs.clear();
        assert!(
            execute(&fixture.cache(), &consumer)
                .unwrap_err()
                .contains("expected regular file")
        );
        // Removing a cache entry with read-only directories works.
        drop(fixture);
    }

    #[test]
    fn tree_outputs_reject_symlinks_and_missing_directories() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.inputs.clear();
        action.outputs = vec!["out/tree".into()];
        action.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        action.command = "printf x > out/tree/real && ln -s real out/tree/link".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("symlinks inside tree artifacts are not supported by the native profile")
        );
        action.command = "rmdir out/tree && printf x > out/tree".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("expected directory")
        );
        fixture.assert_unpublished();
    }

    #[test]
    fn param_files_are_written_before_execution_and_keyed_by_content() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.extra.param_files = vec![("out/result-0.params".into(), b"'a b'\n".to_vec())];
        action.command = "cat out/result-0.params > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"'a b'\n");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.param_files[0].1 = b"changed\n".to_vec();
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed\n");
        action
            .extra
            .param_files
            .push(("src/input".into(), Vec::new()));
        assert!(
            validate_structure(&action)
                .unwrap_err()
                .contains("overlaps")
        );
    }

    #[test]
    fn exact_environment_and_execution_info_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.extra.exact_environment = true;
        action.environment.insert("X".into(), "1".into());
        action.command = "test -z \"${LANG+x}\" && test -z \"${LC_ALL+x}\" && [[ $TMPDIR = /* ]] && printf '%s' \"$X\" > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"1");
        action
            .extra
            .execution_info
            .insert("no-sandbox".into(), "1".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.exact_environment = false;
        assert!(execute(&fixture.cache(), &action).is_err());
    }

    #[test]
    fn symlink_transform_is_a_real_symlink_to_the_target_and_checks_executable() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command.clear();
        action.extra.transform = Some(Transform::Symlink {
            target: "src/input".into(),
            require_executable: false,
            progress: "Creating symlink out/result".into(),
        });
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"hello\n");
        // SymlinkAction.java:221-238: a symlink to the input's absolute path;
        // stat follows it to the target's mode and inode.
        assert!(
            fs::symlink_metadata(&first.outputs[0])
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&first.outputs[0]).unwrap(),
            action.inputs[0].0
        );
        assert_eq!(
            fs::metadata(&first.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
        assert_eq!(
            fs::metadata(&first.outputs[0]).unwrap().ino(),
            fs::metadata(&action.inputs[0].0).unwrap().ino()
        );
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.transform = Some(Transform::Symlink {
            target: "src/input".into(),
            require_executable: true,
            progress: "Creating symlink out/result".into(),
        });
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            error.contains(
                "failed to create symbolic link 'out/result': file 'src/input' is not executable"
            ),
            "{error}"
        );
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o755)).unwrap();
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::metadata(&executable.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        action.extra.transform = Some(Transform::Symlink {
            target: "missing".into(),
            require_executable: false,
            progress: "Creating symlink out/result".into(),
        });
        assert!(
            validate_structure(&action)
                .unwrap_err()
                .contains("target must be an input")
        );
    }

    #[test]
    fn template_transform_replaces_sequentially_and_literally() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        fs::write(&action.inputs[0].0, b"A B $x \xff\n").unwrap();
        action.command.clear();
        action.extra.transform = Some(Transform::Template {
            template: "src/input".into(),
            substitutions: vec![
                (b"A".to_vec(), b"AB".to_vec()),
                (b"B".to_vec(), b"C".to_vec()),
                (Vec::new(), b"never".to_vec()),
                (b"$x".to_vec(), b"$y".to_vec()),
            ],
            executable: true,
        });
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"AC C $y \xff\n");
        assert_eq!(mode(&first.outputs[0]), OUTPUT_MODE);
        assert_eq!(replace_all_literal(b"aaa", b"aa", b"b"), b"ba");
        if let Some(Transform::Template { executable, .. }) = action.extra.transform.as_mut() {
            *executable = false;
        }
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.transform = Some(Transform::Unsupported {
            message: "explicit gap".into(),
        });
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("explicit gap")
        );
    }
    fn ino(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }

    fn blob(fixture: &Fixture, content: &[u8], mode: u32) -> PathBuf {
        fixture
            .cache()
            .join(CAS)
            .join(format!("{}-{mode:o}", digest(content)))
    }

    fn link_counts() -> (u64, u64) {
        LINK_COUNTS.with(std::cell::Cell::get)
    }

    #[test]
    fn inputs_are_read_only_hardlinks_to_one_store_inode() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action
            .inputs
            .push((action.inputs[0].0.clone(), "other/path.h".into()));
        action.command =
            "stat -c '%i %a' src/input other/path.h > out/result; test ! -w src/input || test \"$(id -u)\" = 0".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        let store = blob(&fixture, b"hello\n", 0o444);
        let expected = format!("{0} 444\n{0} 444\n", ino(&store));
        assert_eq!(fs::read_to_string(&first.outputs[0]).unwrap(), expected);
        assert_eq!(mode(&store), 0o444);
        // Another action stages the same inode; the source is never linked.
        action.id = "second".into();
        let second = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read_to_string(&second.outputs[0]).unwrap(), expected);
        assert_ne!(ino(&store), ino(&action.inputs[0].0));
        assert_eq!(mode(&action.inputs[0].0), 0o644);
        // Executable bits select another blob.
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o755)).unwrap();
        action.command = "stat -c '%a' src/input > out/result".into();
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read_to_string(&executable.outputs[0]).unwrap(), "555\n");
        assert!(blob(&fixture, b"hello\n", 0o555).is_file());
    }

    #[test]
    fn in_place_input_modification_fails_and_discards_the_blob() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command =
            "chmod u+w src/input && printf x >> src/input && cat src/input > out/result".into();
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            error.contains(
                "action 'copy': input dependency src/input was modified during execution."
            ),
            "{error}"
        );
        assert!(!blob(&fixture, b"hello\n", 0o444).exists());
        assert_eq!(fs::read(&action.inputs[0].0).unwrap(), b"hello\n");
        fixture.assert_unpublished();
        // A writer without chmod is refused by the read-only blob.
        if unsafe_is_root() {
            return;
        }
        action.command = "printf x >> src/input; cat src/input > out/result".into();
        let refused = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            refused.contains("src/input: Permission denied"),
            "{refused}"
        );
        // The next action stages a fresh, intact blob.
        action.command = "cat src/input > out/result".into();
        let clean = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&clean.outputs[0]).unwrap(), b"hello\n");
        assert!(blob(&fixture, b"hello\n", 0o444).is_file());
    }

    fn unsafe_is_root() -> bool {
        fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
    }

    #[test]
    fn outputs_are_store_inodes_and_identical_outputs_share_one() {
        let fixture = Fixture::new();
        let mut first = fixture.action();
        first.command = "printf same > out/result".into();
        let mut second = first.clone();
        second.id = "other".into();
        second.outputs = vec!["elsewhere".into()];
        second.command = "printf same > elsewhere".into();
        let a = execute(&fixture.cache(), &first).unwrap();
        let b = execute(&fixture.cache(), &second).unwrap();
        let store = blob(&fixture, b"same", 0o555);
        assert_eq!(ino(&a.outputs[0]), ino(&store));
        assert_eq!(ino(&b.outputs[0]), ino(&store));
        assert_eq!(mode(&a.outputs[0]), OUTPUT_MODE);
        // A consumer stages the producer's inode.
        let mut consumer = fixture.action();
        consumer.id = "consumer".into();
        consumer.inputs = vec![(a.outputs[0].clone(), "generated".into())];
        consumer.command = "stat -c %i generated > out/result".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(
            fs::read_to_string(&consumed.outputs[0]).unwrap(),
            format!("{}\n", ino(&store))
        );
        // Tampering with a published output (and so its blob) is detected.
        fs::set_permissions(&a.outputs[0], fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&a.outputs[0], b"evil").unwrap();
        fs::set_permissions(&a.outputs[0], fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!execute(&fixture.cache(), &first).unwrap().cache_hit);
        assert_eq!(fs::read(&a.outputs[0]).unwrap(), b"same");
        assert_eq!(fs::read(blob(&fixture, b"same", 0o555)).unwrap(), b"same");
        assert!(!execute(&fixture.cache(), &second).unwrap().cache_hit);
    }

    #[test]
    fn failed_hardlinks_fall_back_to_copies() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "stat -c %i src/input > out/result; cat src/input >> out/result".into();
        FORCE_LINK_ERRNO.with(|force| force.set(18));
        LINK_COUNTS.with(|counts| counts.set((0, 0)));
        let result = execute(&fixture.cache(), &action);
        FORCE_LINK_ERRNO.with(|force| force.set(0));
        let result = result.unwrap();
        let text = fs::read_to_string(&result.outputs[0]).unwrap();
        let store = blob(&fixture, b"hello\n", 0o444);
        assert!(text.ends_with("\nhello\n"), "{text}");
        assert_ne!(text, format!("{}\nhello\n", ino(&store)));
        // One staged copy; the output is published by rename.
        assert_eq!(link_counts(), (1, 1));
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn symlink_outputs_and_their_targets_are_one_inode_for_consumers() {
        let fixture = Fixture::new();
        let header = fixture.0.join("hdr.h");
        fs::write(&header, b"#pragma once\n").unwrap();
        fs::set_permissions(&header, fs::Permissions::from_mode(0o644)).unwrap();
        let mut link = fixture.action();
        link.id = "virtual".into();
        link.command.clear();
        link.inputs = vec![(header.clone(), "pkg/hdr.h".into())];
        link.outputs = vec!["pkg/_virtual_includes/hdr.h".into()];
        link.extra.transform = Some(Transform::Symlink {
            target: "pkg/hdr.h".into(),
            require_executable: false,
            progress: "Creating symlink".into(),
        });
        let linked = execute(&fixture.cache(), &link).unwrap();
        assert_eq!(fs::read_link(&linked.outputs[0]).unwrap(), header);
        // Symlink of a symlink: a chain, like Bazel.
        let mut chain = link.clone();
        chain.id = "chain".into();
        chain.inputs = vec![(
            linked.outputs[0].clone(),
            "pkg/_virtual_includes/hdr.h".into(),
        )];
        chain.outputs = vec!["pkg/again.h".into()];
        chain.extra.transform = Some(Transform::Symlink {
            target: "pkg/_virtual_includes/hdr.h".into(),
            require_executable: false,
            progress: "Creating symlink".into(),
        });
        let chained = execute(&fixture.cache(), &chain).unwrap();
        assert_eq!(
            fs::read_link(&chained.outputs[0]).unwrap(),
            linked.outputs[0]
        );
        let mut consumer = fixture.action();
        consumer.id = "compile".into();
        consumer.inputs = vec![
            (header.clone(), "pkg/hdr.h".into()),
            (
                linked.outputs[0].clone(),
                "pkg/_virtual_includes/hdr.h".into(),
            ),
            (chained.outputs[0].clone(), "pkg/again.h".into()),
        ];
        consumer.command = "stat -L -c %i pkg/hdr.h pkg/_virtual_includes/hdr.h pkg/again.h | sort -u | wc -l > out/result".into();
        let compiled = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(
            fs::read_to_string(&compiled.outputs[0]).unwrap().trim(),
            "1"
        );
        // Retargeting a published symlink is detected on the next use.
        fs::remove_file(&linked.outputs[0]).unwrap();
        symlink(&consumer.inputs[2].0, &linked.outputs[0]).unwrap();
        let rebuilt = execute(&fixture.cache(), &link).unwrap();
        assert!(!rebuilt.cache_hit);
        assert_eq!(fs::read_link(&rebuilt.outputs[0]).unwrap(), header);
        // A changed target invalidates the old entry's validity.
        fs::write(&header, b"#pragma twice\n").unwrap();
        let mut stale = link.clone();
        stale.inputs = vec![(header.clone(), "pkg/hdr.h".into())];
        assert!(!execute(&fixture.cache(), &stale).unwrap().cache_hit);
        // Other symlinks stay rejected as inputs.
        let outside = fixture.0.join("outside-link");
        symlink(&header, &outside).unwrap();
        consumer.inputs = vec![(outside, "pkg/hdr.h".into())];
        assert!(
            execute(&fixture.cache(), &consumer)
                .unwrap_err()
                .contains("symlink is not allowed")
        );
    }

    #[test]
    fn tree_outputs_are_store_inodes_and_tree_symlinks_are_real_symlinks() {
        let fixture = Fixture::new();
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into()];
        producer.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        producer.command =
            "mkdir out/tree/sub && printf same > out/tree/a && printf same > out/tree/sub/b".into();
        let tree = execute(&fixture.cache(), &producer).unwrap();
        let store = blob(&fixture, b"same", 0o555);
        assert_eq!(ino(&tree.outputs[0].join("a")), ino(&store));
        assert_eq!(ino(&tree.outputs[0].join("sub/b")), ino(&store));
        let mut link = fixture.action();
        link.id = "tree-link".into();
        link.command.clear();
        link.inputs = vec![(tree.outputs[0].clone(), "in/tree".into())];
        link.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
        link.outputs = vec!["out/link".into()];
        link.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/link")]);
        link.extra.transform = Some(Transform::Symlink {
            target: "in/tree".into(),
            require_executable: false,
            progress: "Creating symlink".into(),
        });
        let linked = execute(&fixture.cache(), &link).unwrap();
        assert!(
            fs::symlink_metadata(&linked.outputs[0])
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&linked.outputs[0]).unwrap(), tree.outputs[0]);
        assert_eq!(
            linked.trees[&0],
            [PathBuf::from("a"), PathBuf::from("sub/b")]
        );
        let hit = execute(&fixture.cache(), &link).unwrap();
        assert!(hit.cache_hit);
        assert_eq!(hit.trees, linked.trees);
        let mut consumer = fixture.action();
        consumer.id = "tree-consumer".into();
        consumer.inputs = vec![(linked.outputs[0].clone(), "t".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("t")]);
        consumer.command = "cat t/a t/sub/b > out/result; stat -c %i t/a >> out/result".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(
            fs::read_to_string(&consumed.outputs[0]).unwrap(),
            format!("samesame{}\n", ino(&store))
        );
    }

    #[test]
    fn digest_memo_trusts_only_settled_or_owned_files() {
        let fixture = Fixture::new();
        let path = fixture.0.join("fresh");
        fs::write(&path, b"fresh").unwrap();
        let (value, metadata) = hash_file(&path, false, false).unwrap();
        assert_eq!(value, digest(b"fresh"));
        // Just written: a same-size rewrite could keep the timestamps.
        assert_eq!(memoized_digest(&file_key(&metadata, false)), None);
        let (_, metadata) = hash_file(&path, true, true).unwrap();
        assert_eq!(
            memoized_digest(&file_key(&metadata, true)),
            Some(digest(b"fresh"))
        );
        let old = SystemTime::now() - Duration::from_secs(60);
        assert!(!racy_safe(&fs::metadata(&path).unwrap(), old, false));
        assert!(racy_safe(
            &fs::metadata(&path).unwrap(),
            SystemTime::now() + RACY_MARGIN + Duration::from_secs(1),
            false
        ));
        // An old mtime with a fresh ctime (as every link() to a store blob
        // leaves): settled for a cache-owned file, racy for any other.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(racy_safe(&metadata, SystemTime::now(), true));
        assert!(!racy_safe(&metadata, SystemTime::now(), false));
        let blob = fixture.0.join("blob");
        fs::write(&blob, b"blob").unwrap();
        File::options()
            .write(true)
            .open(&blob)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        fs::hard_link(&blob, fixture.0.join("blob-link")).unwrap();
        let (_, metadata) = hash_file(&blob, true, false).unwrap();
        assert_eq!(
            memoized_digest(&file_key(&metadata, true)),
            Some(digest(b"blob"))
        );
    }

    #[test]
    fn emlink_hardlink_failures_fall_back_to_copies() {
        // EMLINK: the blob reached the filesystem's link-count limit.
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "stat -c %i src/input > out/result; cat src/input >> out/result".into();
        FORCE_LINK_ERRNO.with(|force| force.set(31));
        LINK_COUNTS.with(|counts| counts.set((0, 0)));
        let result = execute(&fixture.cache(), &action);
        FORCE_LINK_ERRNO.with(|force| force.set(0));
        let result = result.unwrap();
        let text = fs::read_to_string(&result.outputs[0]).unwrap();
        let store = blob(&fixture, b"hello\n", 0o444);
        assert!(text.ends_with("\nhello\n"), "{text}");
        assert_ne!(text, format!("{}\nhello\n", ino(&store)));
        assert_eq!(fs::read(&store).unwrap(), b"hello\n");
        assert_eq!(link_counts(), (1, 1));
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn a_blob_removed_before_linking_is_ensured_again() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "stat -c %i src/input > out/result; cat src/input >> out/result".into();
        REMOVE_BLOB_BEFORE_LINK.with(|hook| hook.set(true));
        let result = execute(&fixture.cache(), &action);
        REMOVE_BLOB_BEFORE_LINK.with(|hook| hook.set(false));
        let result = result.unwrap();
        let store = blob(&fixture, b"hello\n", 0o444);
        assert_eq!(
            fs::read_to_string(&result.outputs[0]).unwrap(),
            format!("{}\nhello\n", ino(&store))
        );
    }
}
