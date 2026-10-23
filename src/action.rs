//! Native action execution for an initial Bazel subset, not full Bazel.
//!
//! A fresh working directory is isolation for build state, NOT a security
//! sandbox. Arbitrary shell commands can access the host, network, clock, and
//! absolute paths; they are not hermetic. Sources and the cache must not be
//! modified by an adversary racing filesystem checks. Symlink checks prevent
//! accidental escapes, but are not a substitute for OS sandboxing.
//!
//! A process gets exactly its action environment (docs/ACTION_ENV.md): the
//! fixed variables, then the values the client environment has for its
//! inherited names (resolved when the action is planned, crate::action_env),
//! then TMPDIR. Nothing else from the caller's environment is inherited:
//! otherwise secrets could leak and an unkeyed environment variable could
//! silently change a cached result; the resolved inherited values are keyed.
//! The executed environment is [`resolved_environment`] (with TMPDIR replaced
//! last), the same map the remote cache key's `Command.environment_variables`
//! is built from (N41, docs/REMOTE_CACHE.md), so a remote entry describes
//! exactly the environment the action ran with.
//! TMPDIR is always a fresh absolute directory, replacing any TMPDIR of the
//! action environment (PosixLocalEnvProvider.rewriteLocalEnv,
//! exec/local/PosixLocalEnvProvider.java:41-59, always replaces it too); its
//! spelling (and the working directory's) is intentionally not stable, so
//! actions must not embed these paths in output.
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
//!
//! Per-process memos keep the execution phase from inspecting the same
//! cache-owned state once per action, like Bazel's per-build metadata:
//! - a published tree artifact (`<root>/actions/<key>/outputs/...`, read-only
//!   by contract and checked by `tree_valid`) is read once and shared as an
//!   `Arc<Tree>` by every consumer, like Bazel's in-memory TreeArtifactValue;
//!   its producer seeds the entry (cache check or publication) and replaces
//!   or drops it whenever it publishes the tree again (`read_staged`);
//! - a file of the cache's immutable stores (the content-addressed
//!   repository tree stores, `REPOSITORY_TREE_STORES`, and blobs) is read
//!   once (`file_memo`);
//! - cache-owned directories in `actions/`, `cas/` and the repository tree
//!   stores (never the execution roots under `work/`, nor the working areas
//!   of repository rules and module extensions) are lstat'ed once to
//!   establish that they are real directories (`no_linked_ancestor`).
//!
//! Workspace files and every other path outside the cache keep the full
//! checks on every use. Many inputs (or tree files) are read and hashed on
//! several threads (`try_map_ordered`, helpers from one process-wide budget
//! of as many threads as cores), reporting the error a sequential read
//! reports.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

// `native-action-v8` (integration of N41 with N42, docs/ACTION_ENV.md and
// docs/REMOTE_CACHE.md): Bazel's spawn argv (genrules source
// genrule-setup.sh, which is a keyed input; run_shell's `$0` padding and a
// File executable's argv[0] are framed) and the N42 action environment
// (the fixed map, the inherited names and their resolved client values,
// ActionEnvironment.addTo plus ActionCacheChecker.computeEffectiveEnvironment);
// output roots are k8-* (D1 closed). N23d features (tree artifacts, parameter
// files, execution requirements, transforms) are framed in an extra block.
// Hardlink staging (N36) changes no key. Manifests (symlink outputs since
// N36) move to v8 with the key, so v7 entries rebuild once.
const FORMAT: &str = "native-action-v8";
const MANIFEST_FORMAT: &str = "native-action-v8";
/// The content-addressed store below the cache root.
const CAS: &str = "cas";
/// Read and copy buffer for streamed digests.
const BUFFER_BYTES: usize = 1 << 20;
/// A digest of a file outside the cache is memoized only when its mtime and
/// ctime are this much older than the start of hashing ("racy git"): a later
/// in-place change then necessarily carries a different timestamp.
const RACY_MARGIN: Duration = Duration::from_secs(2);
// Caps of the per-process memos. A memo starts over when it would exceed
// its cap, which is always safe: a memo only saves work, and consumers keep
// what they took from it (shared trees are Arcs, inputs are copies). Their
// worst-case footprints at the caps (32-byte digests, absolute paths of
// 160-230 bytes as on ~/code, hash-table and B-tree slack included): digest
// memo ~45 MB, tree memo ~110 MB, file memo ~85 MB, directory memo ~60 MB.
// The warm `oauth2token/...` build (689 actions) fills about 3% of them:
// 8.9k digests, 958 tree entries, 7.4k store files, 7.3k directories.
/// File states with a known digest (88 bytes each, no allocation).
const DIGEST_MEMO_LIMIT: usize = 1 << 18;
/// Entries (files plus directories) of memoized published trees.
const TREE_MEMO_LIMIT: usize = 1 << 18;
/// Remembered cache-owned real directories (one path each).
const DIRECTORY_MEMO_LIMIT: usize = 1 << 18;
/// Remembered immutable-store files (a path and 48 bytes each).
const FILE_MEMO_LIMIT: usize = 1 << 18;
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
const SPAWN_RETRY_DELAYS_MS: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];
static NEXT_WORK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default)]
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
    /// The fixed part of the action environment (a TMPDIR entry is keyed
    /// but replaced by the fresh temporary directory).
    pub environment: BTreeMap<String, String>,
    /// Enable errexit and pipefail flags; ignored for writes/direct runs.
    /// Genrules get these from Bazel's genrule-setup.sh instead (N41).
    pub shell_strict: bool,
    /// Starlark-action features (docs/ACTIONS_PARAMS.md).
    pub extra: ActionExtra,
    /// Pass an empty `$0` before the shell's positional arguments
    /// (StarlarkActionFactory.java:627-631: run_shell with `arguments`).
    pub shell_pad: bool,
    /// `program` is a File's execution path: it runs from the execution
    /// root with that path as argv[0], as Bazel's spawn does.
    pub executable_file: bool,
    /// The rule tags that are execution requirements
    /// (TargetUtils.getExecutionInfo); with the keys of
    /// `extra.execution_info` they decide remote cacheability
    /// (actions/Spawns.java:29-56). Not part of the native key.
    pub execution_info: BTreeSet<String>,
    /// Compiled-in inputs (execution path, bytes), stored as executable
    /// content-addressed blobs and staged like every input
    /// (crate::builtin_tools::GENRULE_SETUP).
    pub embedded_inputs: Vec<(PathBuf, &'static [u8])>,
    /// Why this spawn's remote cache key cannot equal Bazel's (e.g. execution
    /// properties, which Bazel puts into a Platform message); such spawns
    /// never use the remote cache.
    pub remote_unsupported: Option<String>,
}

/// The process argv, exactly as Bazel's spawn has it
/// (BashCommandConstructor.asExecArgv, SpawnAction.Builder.setShellCommand):
/// the remote cache key's `Command.arguments` and the executed argv.
pub fn arguments(action: &Action) -> Vec<String> {
    let mut argv = Vec::new();
    match &action.program {
        Some(program) => argv.push(program.clone()),
        None => {
            argv.push(SHELL.to_owned());
            if action.shell_strict {
                argv.extend(SHELL_FLAGS.iter().map(|flag| (*flag).to_owned()));
            } else {
                argv.push("-c".to_owned());
            }
            argv.push(action.command.clone());
            if action.shell_pad {
                argv.push(String::new());
            }
        }
    }
    argv.extend(action.arguments.iter().cloned());
    argv
}

/// Spawns.mayBeCachedRemotely and mayBeExecutedRemotely
/// (actions/Spawns.java:34-56) over the rule tags and the action's
/// execution requirements.
pub fn remote_policy(action: &Action) -> (bool, bool) {
    let has = |key: &str| {
        action.execution_info.contains(key) || action.extra.execution_info.contains_key(key)
    };
    let cached = !has("no-cache") && !has("local") && !has("no-remote") && !has("no-remote-cache");
    let executed = !has("local") && !has("no-remote") && !has("no-remote-exec");
    (cached, executed)
}

/// The environment a process action runs with, before TMPDIR: the fixed
/// values, then the resolved client values of the inherited names, which
/// override fixed ones (ActionEnvironment.resolve, ActionEnvironment.java:
/// 113-122). A declared TMPDIR stays in this map (Bazel keys it in
/// `Command.environment_variables`); the executor replaces it last with the
/// fresh temporary directory (PosixLocalEnvProvider.rewriteLocalEnv). The
/// remote cache Command is built from this same map.
pub fn resolved_environment(action: &Action) -> BTreeMap<String, std::ffi::OsString> {
    let mut environment: BTreeMap<String, std::ffi::OsString> = action
        .environment
        .iter()
        .map(|(name, value)| (name.clone(), value.into()))
        .collect();
    for (name, value) in &action.extra.client_environment {
        environment.insert(name.clone(), value.clone());
    }
    environment
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
    /// The inherited part of the action environment: names whose values
    /// come from the client environment (ActionEnvironment.getInheritedEnv).
    pub inherited_environment: BTreeSet<String>,
    /// The client values of the inherited names that the client
    /// environment sets, resolved when the action is planned
    /// (crate::action_env::ActionEnvironment::resolve_inherited); keyed,
    /// and they override fixed values of the same name.
    pub client_environment: BTreeMap<String, std::ffi::OsString>,
    /// Execution requirements: keyed, no local effect.
    pub execution_info: BTreeMap<String, String>,
    /// The spawn mnemonic (Spawn.getMnemonic): "Genrule" for genrule actions
    /// (GenRuleAction.java:35), "TestRunner" for test spawns
    /// (TestRunnerAction.java:112), the Starlark `mnemonic` parameter or
    /// "Action" for ctx.actions spawns (StarlarkActionFactory.java:811).
    /// Execution metadata, never part of the action key: --strategy matches
    /// it (SpawnStrategyRegistry.getStrategies:142-143) exactly as Bazel's
    /// strategy selection never changes an action cache key.
    pub mnemonic: String,
    /// A native, process-free transform of one input into the only output.
    pub transform: Option<Transform>,
    /// Input destinations whose digests are excluded from the action key:
    /// constant-metadata artifacts (Artifact.isConstantMetadata), the
    /// volatile workspace status file and the volatile build-info files a
    /// transform_version_file action writes (BazelWorkspaceStatusAction
    /// marks them "constant metadata", BazelWorkspaceStatusAction.java
    /// :211-215; BuildInfoFileWriteAction's volatile output is created with
    /// getConstantMetadataArtifact, StarlarkActionFactory.java:514-519).
    /// Their bytes are staged like any input's, but a change does not
    /// rebuild the consumer (ActionKeyComputer skips constant metadata).
    pub constant_metadata: Vec<PathBuf>,
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
    /// An actions.transform_version_file/actions.transform_info_file
    /// registration (N63): a placeholder replaced during planning by a
    /// Template expansion of the evaluated translation callback
    /// (BuildInfoFileWriteAction.execute :121-176), or by Unsupported when
    /// the workspace status could not be determined.
    BuildInfo { volatile: bool },
    /// A resolved BuildInfoFileWriteAction (N63 follow-up): the template
    /// expansion whose key is Bazel's computeKey (:189-197) — GUID,
    /// isVolatile and the callback's bzl digest — plus the template input's
    /// digest (an ordinary keyed input), plus `keyed_substitutions`: the
    /// non-timestamp part of the callback result (the volatile callback's
    /// SCM revision/status; the stable callback's whole result, which the
    /// stable variant's regular keyed status input already keys, so it is
    /// equivalent there). The timestamp-bearing substitutions are never
    /// keyed. The volatile status input is constant metadata and the
    /// volatile status action re-executes every build, so a volatile
    /// change — a new timestamp, or a changed SCM key — produces fresh
    /// substitutions under a fresh (or, when nothing but the timestamp
    /// changed, the same) key and volatile_file.h is rewritten with the
    /// current values, like genuine (whose volatile translate likewise
    /// re-runs on timestamp or SCM change and cache-hits when neither
    /// changed, verified in the N63 paired capture). `volatile` selects
    /// that keying.
    TemplateKeyed {
        template: PathBuf,
        substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        keyed_substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        executable: bool,
        volatile: bool,
        callback_digest: String,
    },
    /// An action whose execution is an explicit native gap (its outputs are
    /// declared and registered, e.g. workspace-status transforms).
    Unsupported { message: String },
    /// SourceManifestAction (SOURCE_SYMLINKS, analysis/SourceManifestAction.java):
    /// the `<executable>.runfiles_manifest` lines `path target` of a runfiles
    /// tree, sorted by path. Each entry names the input destination it links
    /// to (the target is that input's absolute path) or `None` for an empty
    /// file. Inputs are not read: like Bazel, the manifest records paths only.
    RunfilesManifest {
        entries: Vec<(String, Option<PathBuf>)>,
    },
    /// SymlinkTreeAction + RunfilesTreeAction (analysis/actions/SymlinkTreeAction.java,
    /// exec/SymlinkTreeHelper.java): the only output is `<dir>/MANIFEST`, a
    /// symlink to the input manifest (SymlinkTreeHelper.linkManifest); the
    /// rest of `<dir>` (the entries' symlinks to absolute input paths, empty
    /// files, directories and the always-created `_main` directory) is
    /// published with it, plus a native sibling `executable` next to `<dir>`:
    /// a hardlink of the executable's content-addressed blob (one inode), so
    /// `$0.runfiles`, `realpath($0)` and `/proc/self/exe` all resolve to a
    /// path whose `.runfiles` sibling exists, like Bazel's output tree.
    /// `create == false` is RunfileSymlinksMode.SKIP
    /// (createMinimalRunfilesDirectory: MANIFEST and `_main` only).
    RunfilesTree {
        entries: Vec<(String, Option<PathBuf>)>,
        manifest: PathBuf,
        executable: Option<PathBuf>,
        create: bool,
    },
}

impl Transform {
    /// Runfiles transforms key and use their inputs' absolute paths, never
    /// their contents (only the tree's input manifest is read).
    fn reads_paths_only(&self) -> bool {
        matches!(
            self,
            Transform::RunfilesManifest { .. } | Transform::RunfilesTree { .. }
        )
    }
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
    /// Outputs came from the remote cache (crate::remote_cache).
    pub remote_hit: bool,
    /// Declaration-order output index of each tree output: its regular files,
    /// relative to the tree root, in byte order (TreeArtifactValue children).
    pub trees: BTreeMap<usize, Vec<PathBuf>>,
}

/// A regular file's content identity; its bytes stay on disk at `path` (the
/// resolved file they are read, linked or copied from).
#[derive(Clone)]
struct Input {
    path: PathBuf,
    digest: Sha256Digest,
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
    /// Shared: a published tree is read once per process and every consumer
    /// stages the same value ([`read_staged`]).
    Tree(Arc<Tree>),
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
    /// The runfiles tree published with a Transform::RunfilesTree MANIFEST.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runfiles: Option<RunfilesTreeManifest>,
}

/// A published runfiles tree: every entry below the tree root except
/// `MANIFEST` (hex path, `dir`, `empty` or `link:<hex target>`), and the
/// executable sibling, a hardlink of the executable's store blob (hex path
/// relative to `outputs`, digest, mode).
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct RunfilesTreeManifest {
    entries: Vec<(String, String)>,
    sibling_file: Option<(String, String, u32)>,
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

fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(Sha256::digest(bytes).into())
}

/// A file's SHA-256, kept as its 32 bytes (no allocation, half the size of
/// its hex spelling: inputs fill the per-process memos). Action keys,
/// manifests and blob names spell it in lowercase hex, exactly as before.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The lowercase hex spelling, without allocating.
    fn hex_bytes(&self) -> [u8; 64] {
        const DIGITS: &[u8] = b"0123456789abcdef";
        let mut text = [0; 64];
        for (index, byte) in self.0.iter().enumerate() {
            text[2 * index] = DIGITS[usize::from(byte >> 4)];
            text[2 * index + 1] = DIGITS[usize::from(byte & 15)];
        }
        text
    }

    fn hex(&self) -> String {
        hex(&self.0)
    }

    /// Whether `text` is exactly this digest's spelling (a manifest's).
    fn spelled(&self, text: &str) -> bool {
        self.hex_bytes().as_slice() == text.as_bytes()
    }
}

impl std::fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(std::str::from_utf8(&self.hex_bytes()).expect("ASCII hex"))
    }
}

impl std::fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
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

/// `Path::cmp` of two paths [`validate_relative`] admitted (relative, every
/// component normal, single separators), from the bytes: byte order with
/// the separator below every other byte. At the first differing byte a
/// separator ends the shorter of two components, which sorts first, as a
/// path that ends there does.
fn compare_relative(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let rank = |byte: u8| {
        if byte == b'/' { 0 } else { u16::from(byte) + 1 }
    };
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => rank(a[at]).cmp(&rank(b[at])),
        None => a.len().cmp(&b.len()),
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

// Check all ancestors, not just the leaf: dir/link/file must not escape via
// dir/link. This intentionally rejects source paths through symlink directories.
// `cache` is the canonical cache root when the caller has one: a cache-owned
// path then skips the directories this process already verified
// ([`owned_no_linked_ancestor`]). Any failure, and every other path, takes
// the full walk, so errors are reported exactly as without the memo.
fn no_linked_ancestor(path: &Path, cache: Option<&Path>) -> Result<fs::Metadata, String> {
    checked_path(path, cache, None)
}

/// [`no_linked_ancestor`], where `leaf` is the caller's lstat of `path`,
/// taken just before: the memoized check uses it instead of inspecting the
/// leaf again (the full walk ignores it).
fn checked_path(
    path: &Path,
    cache: Option<&Path>,
    leaf: Option<fs::Metadata>,
) -> Result<fs::Metadata, String> {
    if let Some(metadata) = cache.and_then(|root| owned_no_linked_ancestor(path, root, leaf)) {
        return Ok(metadata);
    }
    #[cfg(test)]
    FULL_ANCESTOR_WALKS.with(|walks| walks.set(walks.get() + 1));
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

/// Cache-owned directories verified by lstat to be real directories, each
/// together with every directory between it and the cache root. They stay
/// real directories while this process runs: the cache root is canonical,
/// and below `actions/`, `cas/` and the repository tree stores
/// ([`REPOSITORY_TREE_STORES`]) bazel-rs only creates real directories
/// (published outputs move into place by rename; repository trees are
/// content-addressed and only ever replaced as a whole by a tree of the same
/// content). The execution roots under `work/` and the rest of
/// `repositories/` (where repository rules and module extensions run
/// arbitrary code: `modextwd/`, `direct/stages/`) are never remembered.
/// Other writers racing these checks are outside the module's trust model.
fn verified_directories() -> &'static RwLock<HashSet<PathBuf>> {
    static MEMO: OnceLock<RwLock<HashSet<PathBuf>>> = OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(HashSet::new()))
}

fn directory_verified(path: &Path) -> bool {
    verified_directories()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

fn remember_directory(path: PathBuf) {
    let mut memo = verified_directories()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if memo.len() >= DIRECTORY_MEMO_LIMIT {
        memo.clear();
    }
    memo.insert(path);
}

/// Whether `bytes` spell a plain absolute path: a leading `/`, no empty
/// component (`//`), no `.` component and no trailing separator — exactly
/// the absolute paths that `Path::components` rebuilds byte for byte (`..`
/// stays a component). The canonical cache root and planned inputs are
/// spelled so; for two plain paths, `Path::strip_prefix` and the components
/// of its result follow from the bytes ([`plain_rest`]), without parsing
/// components or allocating (the per-input checks of a 6,000-input action).
fn plain(bytes: &[u8]) -> bool {
    bytes.first() == Some(&b'/')
        && !bytes.ends_with(b"/")
        && !bytes.ends_with(b"/.")
        && !bytes.windows(2).any(|pair| pair == b"//")
        && !bytes.windows(3).any(|triple| triple == b"/./")
}

/// For [`plain`] `root` and `path`: the bytes of `path.strip_prefix(root)`
/// when `path` lies strictly below `root` (its components are then the
/// `/`-separated names, none empty), else `None`.
fn plain_rest<'a>(root: &[u8], path: &'a [u8]) -> Option<&'a [u8]> {
    path.strip_prefix(root)?.strip_prefix(b"/")
}

/// Whether `/`-separated names include `..`.
fn has_parent_name(names: &[u8]) -> bool {
    names.split(|byte| *byte == b'/').any(|name| name == b"..")
}

/// The names of `path` below `root`, `/`-separated, when `path` lies
/// strictly below `root` and every component after it is a normal name.
fn relative_names(path: &Path, root: &Path) -> Option<Vec<u8>> {
    let relative = path.strip_prefix(root).ok()?;
    let mut names = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        if !names.is_empty() {
            names.push(b'/');
        }
        names.extend_from_slice(name.as_bytes());
    }
    (!names.is_empty()).then_some(names)
}

/// The content-addressed repository tree stores below the cache root:
/// `<store>/<identity>/...` holds a tree that is verified when it is
/// materialized and only ever replaced as a whole, by a tree of the same
/// content. They are the registry module trees (crate::materializer), the
/// results of repository rules (crate::repository_store), Git sources
/// (crate::git_source) and the embedded `bazel_tools` (crate::builtin_tools).
/// Nothing else below `repositories/` is such a store: module extensions and
/// repository rules run arbitrary code in `modextwd/` and `direct/stages/`,
/// and downloads, locks, manifests and specification pointers are no trees.
const REPOSITORY_TREE_STORES: [&[u8]; 4] = [
    b"repositories/trees",
    b"repositories/direct/trees",
    b"repositories/git/trees",
    b"repositories/builtins/trees",
];

/// For the `/`-separated names `rest` of a path below the cache root: the
/// names below `<identity>` when the path lies in a published tree of a
/// repository tree store ([`REPOSITORY_TREE_STORES`]; empty for the tree's
/// root itself; never for the stores' temporary entries `.stage-*`,
/// `.trash-*`), else `None`.
pub(crate) fn repository_tree_rest(rest: &[u8]) -> Option<&[u8]> {
    let below = REPOSITORY_TREE_STORES
        .iter()
        .find_map(|store| rest.strip_prefix(*store)?.strip_prefix(b"/"))?;
    let (identity, below) = match below.iter().position(|byte| *byte == b'/') {
        Some(at) => (&below[..at], &below[at + 1..]),
        None => (below, &below[below.len()..]),
    };
    (!identity.is_empty() && !identity.starts_with(b".")).then_some(below)
}

/// Whether the names `rest` (below the cache root, no `..`) lie where the
/// directory memo may remember directories: `actions/`, `cas/` and the
/// published repository trees.
fn owned_area_rest(rest: &[u8]) -> bool {
    matches!(
        rest.split(|byte| *byte == b'/').next(),
        Some(b"actions" | b"cas")
    ) || repository_tree_rest(rest).is_some()
}

/// [`owned_no_linked_ancestor`]'s qualification of `path` below the
/// canonical cache root `root`: in `actions/`, `cas/` or a published
/// repository tree ([`owned_area_rest`]), spelled exactly as its normal
/// components (no `.`, `..`, repeated or trailing separator).
fn owned_area(path: &Path, root: &Path) -> bool {
    let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    if !plain(root_bytes) {
        return owned_area_by_components(path, root);
    }
    // Spelled as its components: plain as well.
    plain(bytes)
        && plain_rest(root_bytes, bytes)
            .is_some_and(|rest| !has_parent_name(rest) && owned_area_rest(rest))
}

/// [`owned_area`] below a `root` that is not [`plain`], by components.
fn owned_area_by_components(path: &Path, root: &Path) -> bool {
    relative_names(path, root).is_some_and(|names| owned_area_rest(&names))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// The memoized part of [`no_linked_ancestor`] for `path` below the
/// canonical cache root `root` that qualifies ([`owned_area`]). The root and
/// its ancestors are real directories: the caller canonicalized the root
/// for this action. Directories below it are lstat'ed once per process; the
/// leaf is always inspected (or `leaf`, the caller's lstat of it, is used).
/// `None` when the path does not qualify or anything fails, so that the full
/// walk produces the result (and the exact error) it always did.
fn owned_no_linked_ancestor(
    path: &Path,
    root: &Path,
    leaf: Option<fs::Metadata>,
) -> Option<fs::Metadata> {
    if !owned_area(path, root) {
        return None;
    }
    let parent = path.parent()?;
    if parent != root && !directory_verified(parent) {
        let mut prefix = root.to_path_buf();
        for component in parent.strip_prefix(root).ok()?.components() {
            prefix.push(component);
            if directory_verified(&prefix) {
                continue;
            }
            // lstat: a symlink is never a directory here.
            if !fs::symlink_metadata(&prefix).ok()?.is_dir() {
                return None;
            }
            remember_directory(prefix.clone());
        }
    }
    let metadata = match leaf {
        Some(metadata) => metadata,
        None => fs::symlink_metadata(path).ok()?,
    };
    if metadata.file_type().is_symlink() {
        return None;
    }
    Some(metadata)
}

fn regular_file(path: &Path, cache: Option<&Path>) -> Result<fs::Metadata, String> {
    regular_file_known(path, cache, None)
}

/// [`regular_file`] with the caller's lstat of `path` ([`checked_path`]).
fn regular_file_known(
    path: &Path,
    cache: Option<&Path>,
    leaf: Option<fs::Metadata>,
) -> Result<fs::Metadata, String> {
    let metadata = checked_path(path, cache, leaf)?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    Ok(metadata)
}

fn real_directory(path: &Path, cache: Option<&Path>) -> Result<fs::Metadata, String> {
    let metadata = no_linked_ancestor(path, cache)?;
    if !metadata.is_dir() {
        return Err(format!("expected directory: '{}'", path.display()));
    }
    Ok(metadata)
}

/// Identity of a file state for the digest memo, after Bazel's
/// DigestUtils.CacheKey (vfs/DigestUtils.java:36-60: node id, change time,
/// modification time and size). Files owned by the cache (store blobs,
/// published outputs) leave out the change time: hardlinking a blob updates
/// it constantly, while its bytes are immutable and verified. They keep
/// their birth time in its place (`(0, 0)` where the filesystem reports
/// none): link() leaves it alone, and an inode number reused for a new file
/// gets a new one, so equal sizes and modification times (tools that fix
/// mtimes) cannot alias it with a deleted file (crate::digest_memo).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct FileKey {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    /// The change time, or a cache-owned file's birth time.
    ctime: (i64, i64),
}

fn file_key(metadata: &fs::Metadata, owned: bool) -> FileKey {
    FileKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: if owned {
            metadata
                .created()
                .ok()
                .and_then(|birth| birth.duration_since(SystemTime::UNIX_EPOCH).ok())
                .and_then(|birth| {
                    Some((
                        i64::try_from(birth.as_secs()).ok()?,
                        i64::from(birth.subsec_nanos()),
                    ))
                })
                .unwrap_or((0, 0))
        } else {
            (metadata.ctime(), metadata.ctime_nsec())
        },
    }
}

fn digest_memo() -> &'static Mutex<HashMap<FileKey, Sha256Digest>> {
    static MEMO: OnceLock<Mutex<HashMap<FileKey, Sha256Digest>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memoized_digest(key: &FileKey) -> Option<Sha256Digest> {
    digest_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .copied()
}

fn remember_digest(key: FileKey, digest: Sha256Digest) {
    let mut memo = digest_memo().lock().unwrap_or_else(|e| e.into_inner());
    if memo.len() >= DIGEST_MEMO_LIMIT {
        memo.clear();
    }
    memo.insert(key, digest);
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
fn hash_file(
    path: &Path,
    owned: bool,
    trusted: bool,
) -> Result<(Sha256Digest, fs::Metadata), String> {
    let before = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    hash_file_from(path, before, owned, trusted)
}

/// [`hash_file`] with `before`, the caller's lstat of `path` (taken just
/// before: the file must not change from then on). The digest comes from
/// the per-process memo (any path of the inode), else from the persistent
/// memo of this path in exactly this state (crate::digest_memo, when the
/// command installed one), else from reading the file; a settled or trusted
/// result enters both (the persistent one only for paths that recur across
/// processes: never an execution root).
fn hash_file_from(
    path: &Path,
    before: fs::Metadata,
    owned: bool,
    trusted: bool,
) -> Result<(Sha256Digest, fs::Metadata), String> {
    let key = file_key(&before, owned);
    let table = crate::digest_memo::installed()
        .and_then(|table| Some((table, crate::digest_memo::Key::of(&before, owned)?)));
    if let Some(digest) = memoized_digest(&key) {
        // An inode this process already knows, maybe through another path
        // (a published output and its blob): remember it under this one
        // (trusted: derived knowledge, which never causes a save by itself).
        if let Some((table, table_key)) = &table {
            table.remember(path, *table_key, digest.0, true);
        }
        return Ok((digest, before));
    }
    if let Some((table, table_key)) = &table
        && let Some(sha256) = table.lookup(path, table_key)
    {
        let digest = Sha256Digest(sha256);
        remember_digest(key, digest);
        return Ok((digest, before));
    }
    crate::digest_memo::count_hashed(crate::digest_memo::Use::Action, before.size());
    let start = SystemTime::now();
    let mut file = File::open(path).map_err(|e| io_error("cannot read", path, e))?;
    let mut hash = Sha256::new();
    // No larger than the file needs (plus one byte to see its end): a fresh
    // zeroed BUFFER_BYTES buffer per small file costs more than hashing it.
    let capacity = usize::try_from(before.size()).map_or(BUFFER_BYTES, |size| {
        size.saturating_add(1).min(BUFFER_BYTES)
    });
    let mut buffer = vec![0; capacity];
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
    let digest = Sha256Digest(hash.finalize().into());
    if trusted || racy_safe(&after, start, owned) {
        remember_digest(key, digest);
        if let Some((table, table_key)) = table {
            table.remember(path, table_key, digest.0, trusted);
        }
    }
    Ok((digest, before))
}

/// After publication: record `digest`, the content of the published output
/// `path`, in the persistent memo when this process hashed exactly its
/// inode (the execution-root file it was linked from, or its store blob),
/// so that the next command's cache check does not read it again.
fn remember_published(path: &Path, digest: Sha256Digest) {
    let Some(table) = crate::digest_memo::installed() else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_file() || memoized_digest(&file_key(&metadata, true)) != Some(digest) {
        return;
    }
    if let Some(key) = crate::digest_memo::Key::of(&metadata, true) {
        table.remember(path, key, digest.0, true);
    }
}

fn owned_by(root: Option<&Path>, path: &Path) -> bool {
    root.is_some_and(|root| {
        let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
        if plain(root_bytes) && plain(bytes) {
            bytes == root_bytes || plain_rest(root_bytes, bytes).is_some()
        } else {
            path.starts_with(root)
        }
    })
}

/// Whether `path` is `<root>/actions` or below it (`Path::starts_with`).
fn in_actions(root: &Path, path: &Path) -> bool {
    let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    if plain(root_bytes) && plain(bytes) {
        plain_rest(root_bytes, bytes)
            .is_some_and(|rest| rest == b"actions" || rest.starts_with(b"actions/"))
    } else {
        path.starts_with(root.join("actions"))
    }
}

/// Follow published symlink outputs (`<root>/actions/.../outputs/...`, see
/// Transform::Symlink) to the file or directory they name. Any other symlink
/// is left for the regular-file checks to reject. Also returns the lstat of
/// the resolved path when this took one (a published path that is no
/// symlink), so that the caller need not inspect it again.
fn resolve_published(
    path: &Path,
    root: Option<&Path>,
) -> Result<(PathBuf, Option<fs::Metadata>), String> {
    let Some(root) = root else {
        return Ok((path.to_path_buf(), None));
    };
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_DEPTH {
        if !in_actions(root, &current) {
            return Ok((current, None));
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if let Some(parent) = current.parent() {
                    no_linked_ancestor(parent, Some(root))?;
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
            Ok(metadata) => return Ok((current, Some(metadata))),
            Err(_) => return Ok((current, None)),
        }
    }
    Err(format!(
        "too many levels of published symlinks: '{}'",
        path.display()
    ))
}

/// Regular files of the cache's immutable stores ([`store_file`]) as the
/// first reader in this process read them, by path: later consumers stage
/// the same content identity without inspecting the file again, like Bazel's
/// per-build file metadata.
fn file_memo() -> &'static RwLock<HashMap<PathBuf, StoreFile>> {
    static MEMO: OnceLock<RwLock<HashMap<PathBuf, StoreFile>>> = OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(HashMap::new()))
}

/// What the file memo keeps of a store file's [`Input`]: all but its path,
/// which is the memo's key.
#[derive(Clone, Copy)]
struct StoreFile {
    digest: Sha256Digest,
    size: u64,
    executable: u32,
    mode: u32,
}

fn memoized_file(path: &Path) -> Option<Input> {
    let file = *file_memo()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)?;
    Some(Input {
        path: path.to_path_buf(),
        digest: file.digest,
        size: file.size,
        executable: file.executable,
        mode: file.mode,
    })
}

fn remember_file(input: &Input) {
    let mut memo = file_memo().write().unwrap_or_else(|e| e.into_inner());
    if memo.len() >= FILE_MEMO_LIMIT {
        memo.clear();
    }
    memo.insert(
        input.path.clone(),
        StoreFile {
            digest: input.digest,
            size: input.size,
            executable: input.executable,
            mode: input.mode,
        },
    );
}

/// Whether `path` is a file of a published repository tree below the
/// canonical cache root (`<store>/<identity>/...` of a repository tree store,
/// [`REPOSITORY_TREE_STORES`]: content-addressed, verified when materialized
/// before execution, and only ever replaced as a whole by a tree of the same
/// content) or a blob of the content-addressed store (`cas/<digest>-<mode>`,
/// read-only and immutable), spelled exactly as its normal components.
/// Published outputs are not: their producer can publish them again in this
/// process. Neither are the stores' temporary entries (`.stage-*`,
/// `.trash-*`, `.tmp-*`) nor the rest of `repositories/`.
fn store_file(root: Option<&Path>, path: &Path) -> bool {
    let Some(root) = root else {
        return false;
    };
    let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    if !plain(root_bytes) {
        return store_file_by_components(root, path);
    }
    // Spelled as its components: plain as well.
    plain(bytes)
        && plain_rest(root_bytes, bytes)
            .is_some_and(|rest| !has_parent_name(rest) && in_store(rest))
}

/// Whether the names below the cache root (`/`-separated, normal) are a
/// store file's ([`store_file`]).
fn in_store(rest: &[u8]) -> bool {
    match rest.strip_prefix(b"cas/") {
        Some(blob) => !blob.contains(&b'/') && !blob.starts_with(b"."),
        None => repository_tree_rest(rest).is_some_and(|below| !below.is_empty()),
    }
}

/// [`store_file`] below a `root` that is not [`plain`], by components.
fn store_file_by_components(root: &Path, path: &Path) -> bool {
    relative_names(path, root).is_some_and(|names| in_store(&names))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// Digest and mode of a regular file; `root` (the canonical cache root)
/// enables following published symlink outputs when `follow` is set. A file
/// of the cache's immutable stores is read once per process ([`file_memo`]).
fn read_input(path: &Path, root: Option<&Path>, follow: bool) -> Result<Input, String> {
    let (path, leaf) = if follow {
        resolve_published(path, root)?
    } else {
        (path.to_path_buf(), None)
    };
    let stored = store_file(root, &path);
    if stored && let Some(input) = memoized_file(&path) {
        return Ok(input);
    }
    #[cfg(test)]
    FILE_READS.with(|reads| reads.set(reads.get() + 1));
    let metadata = regular_file_known(&path, root, leaf)?;
    let input = read_regular_from(&path, metadata, owned_by(root, &path), false)?;
    if stored {
        remember_file(&input);
    }
    Ok(input)
}

/// read_input for a path already checked to be a regular file.
fn read_regular(path: &Path, owned: bool, trusted: bool) -> Result<Input, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    read_regular_from(path, metadata, owned, trusted)
}

/// [`read_regular`] with `metadata`, the caller's lstat of `path` (taken
/// just before).
fn read_regular_from(
    path: &Path,
    metadata: fs::Metadata,
    owned: bool,
    trusted: bool,
) -> Result<Input, String> {
    let (digest, metadata) = hash_file_from(path, metadata, owned, trusted)?;
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

/// Inputs (or tree files) per helper thread of [`try_map_ordered`]: fewer
/// items are mapped on the calling thread.
const PARALLEL_ITEMS_PER_THREAD: usize = 32;
/// At most this many threads map one set of items ([`try_map_ordered`]).
const PARALLEL_THREADS: usize = 16;

/// The helper threads that all [`try_map_ordered`] calls of the process
/// share. Concurrent actions (up to `--jobs`) and nested reads (a tree input
/// read on another call's helper) take helpers from this one budget instead
/// of starting up to PARALLEL_THREADS each, which could add up to hundreds of
/// threads: enough to exhaust a pids cgroup, and an action whose spawn then
/// fails with EAGAIN fails (`retry_spawn` retries only ETXTBSY). A call that
/// finds the budget exhausted maps on its calling thread; nothing waits for
/// a helper.
struct HelperBudget {
    available: std::sync::atomic::AtomicUsize,
}

impl HelperBudget {
    const fn new(capacity: usize) -> Self {
        Self {
            available: std::sync::atomic::AtomicUsize::new(capacity),
        }
    }

    /// Up to `wanted` helpers, as many as are free now (possibly none).
    fn take(&self, wanted: usize) -> usize {
        let mut available = self.available.load(Ordering::Relaxed);
        loop {
            let granted = available.min(wanted);
            if granted == 0 {
                return 0;
            }
            match self.available.compare_exchange_weak(
                available,
                available - granted,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return granted,
                Err(current) => available = current,
            }
        }
    }

    fn give_back(&self, count: usize) {
        self.available.fetch_add(count, Ordering::AcqRel);
    }
}

/// Helpers a call took from its budget and has not given back: each one is
/// returned once its thread was joined (or could not be started), the rest
/// when this is dropped, which [`try_map_ordered_in`] does only after its
/// thread scope joined every helper, also when unwinding. So the threads
/// alive at any moment never outnumber the budget.
struct TakenHelpers<'a> {
    budget: &'a HelperBudget,
    count: std::cell::Cell<usize>,
}

impl TakenHelpers<'_> {
    fn give_back(&self, count: usize) {
        let count = count.min(self.count.get());
        self.count.set(self.count.get() - count);
        self.budget.give_back(count);
    }
}

impl Drop for TakenHelpers<'_> {
    fn drop(&mut self) {
        self.budget.give_back(self.count.get());
    }
}

/// The process-wide helper budget: as many helpers as the process has
/// available cores.
fn helper_budget() -> &'static HelperBudget {
    static BUDGET: OnceLock<HelperBudget> = OnceLock::new();
    BUDGET.get_or_init(|| HelperBudget::new(available_cores()))
}

fn available_cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, usize::from))
}

/// `map` over `items`, results in `items` order. Callers consume them in
/// order and stop at the first error, which is therefore exactly the error a
/// sequential pass reports. Up to PARALLEL_ITEMS_PER_THREAD items run on the
/// calling thread, also stopping at the first error; more run on up to
/// PARALLEL_THREADS threads (the calling thread included, helpers taken from
/// the process-wide [`HelperBudget`]), where items after an error are mapped
/// too and their results discarded. Reading many inputs (an SDK, a large
/// tree) is then bounded by the I/O and hashing throughput of several cores
/// instead of one.
pub(crate) fn try_map_ordered<T, R>(
    items: &[T],
    map: impl Fn(&T) -> Result<R, String> + Sync,
) -> Vec<Result<R, String>>
where
    T: Sync,
    R: Send,
{
    try_map_ordered_in(helper_budget(), items, map)
}

/// [`try_map_ordered`] with helpers from `budget`.
fn try_map_ordered_in<T, R>(
    budget: &HelperBudget,
    items: &[T],
    map: impl Fn(&T) -> Result<R, String> + Sync,
) -> Vec<Result<R, String>>
where
    T: Sync,
    R: Send,
{
    let wanted = (items.len() / PARALLEL_ITEMS_PER_THREAD)
        .min(PARALLEL_THREADS)
        .min(available_cores())
        .saturating_sub(1);
    let taken = TakenHelpers {
        budget,
        count: std::cell::Cell::new(if wanted == 0 { 0 } else { budget.take(wanted) }),
    };
    if taken.count.get() == 0 {
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            let result = map(item);
            let failed = result.is_err();
            results.push(result);
            if failed {
                break;
            }
        }
        return results;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        let mut done = Vec::new();
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = items.get(index) else {
                return done;
            };
            done.push((index, map(item)));
        }
    };
    let mut slots: Vec<Option<Result<R, String>>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        // A helper that cannot be started leaves its share to the others.
        let helpers: Vec<_> = (0..taken.count.get())
            .filter_map(|_| std::thread::Builder::new().spawn_scoped(scope, work).ok())
            .collect();
        taken.give_back(taken.count.get() - helpers.len());
        let mut done = work();
        for helper in helpers {
            let joined = helper.join();
            taken.give_back(1);
            match joined {
                Ok(more) => done.extend(more),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        for (index, result) in done {
            slots[index] = Some(result);
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.expect("every item is mapped"))
        .collect()
}

/// Walk a directory without following symlinks. Entries are visited in byte
/// order; a symlink or special file below the root is an explicit error.
/// `normalize` makes every file OUTPUT_MODE before it is read (completed
/// tree outputs, ActionOutputMetadataStore.java:245-281); such files are the
/// executor's own and are memoized as trusted. The files are read after the
/// walk ([`try_map_ordered`]); the first failure in walk order is reported,
/// exactly as if each file were read when the walk reached it.
fn read_tree(root: &Path, cache: Option<&Path>, normalize: bool) -> Result<Tree, String> {
    #[cfg(test)]
    TREE_WALKS.with(|walks| walks.set(walks.get() + 1));
    real_directory(root, cache)?;
    let owned = owned_by(cache, root);
    let mut directories = BTreeSet::new();
    // Regular files in walk order with their lstat, unless normalized (the
    // chmod changes it, so the read inspects the file again).
    let mut files: Vec<(PathBuf, PathBuf, Option<fs::Metadata>)> = Vec::new();
    let mut walk = || -> Result<(), String> {
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
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|e| io_error("cannot inspect", &path, e))?;
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "tree artifact entry '{}' is a symlink; symlinks inside tree artifacts are not supported by the native profile",
                        path.display()
                    ));
                } else if metadata.is_dir() {
                    directories.insert(child.clone());
                    pending.push(child);
                } else if metadata.is_file() {
                    if normalize {
                        set_mode(&path, OUTPUT_MODE)?;
                    }
                    files.push((child, path, (!normalize).then_some(metadata)));
                } else {
                    return Err(format!(
                        "tree artifact entry '{}' is not a regular file or directory",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    };
    let walked = walk();
    let inputs = try_map_ordered(&files, |(_, path, metadata)| match metadata {
        Some(metadata) => read_regular_from(path, metadata.clone(), owned, normalize),
        None => read_regular(path, owned, normalize),
    });
    let mut tree = Tree {
        files: BTreeMap::new(),
        directories,
    };
    for ((child, _, _), input) in files.into_iter().zip(inputs) {
        tree.files.insert(child, input?);
    }
    walked?;
    Ok(tree)
}

/// Published tree artifacts by path ([`published_output`]), read once per
/// process and shared by every consumer, like Bazel's in-memory
/// TreeArtifactValue instead of a walk of the tree per consuming action. A
/// published tree is read-only by contract (TREE_ROOT_MODE and OUTPUT_MODE,
/// checked by [`tree_valid`]). Its producer seeds the entry when its cache
/// entry is valid or when it publishes the tree, and drops it before it
/// removes a stale entry, so a tree published again in this process
/// replaces the old value.
#[derive(Default)]
struct TreeMemo {
    trees: HashMap<PathBuf, Arc<Tree>>,
    /// Files plus directories of all memoized trees (TREE_MEMO_LIMIT).
    entries: usize,
}

fn tree_memo() -> &'static Mutex<TreeMemo> {
    static MEMO: OnceLock<Mutex<TreeMemo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(TreeMemo::default()))
}

fn tree_entries(tree: &Tree) -> usize {
    tree.files.len() + tree.directories.len()
}

fn memoized_tree(path: &Path) -> Option<Arc<Tree>> {
    tree_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .trees
        .get(path)
        .cloned()
}

fn remember_tree(path: PathBuf, tree: Arc<Tree>) {
    let mut memo = tree_memo().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = memo.trees.remove(&path) {
        memo.entries -= tree_entries(&previous);
    }
    let entries = tree_entries(&tree);
    if entries > TREE_MEMO_LIMIT {
        return;
    }
    if memo.entries + entries > TREE_MEMO_LIMIT {
        memo.trees.clear();
        memo.entries = 0;
    }
    memo.entries += entries;
    memo.trees.insert(path, tree);
}

/// Drop the memoized trees at or below `prefix`: an action's cache entry
/// that is about to be removed (and published again).
fn forget_trees(prefix: &Path) {
    let mut memo = tree_memo().lock().unwrap_or_else(|e| e.into_inner());
    let mut removed = 0;
    memo.trees.retain(|path, tree| {
        let keep = !path.starts_with(prefix);
        if !keep {
            removed += tree_entries(tree);
        }
        keep
    });
    memo.entries -= removed;
}

/// Whether `path` is a published action output below the canonical cache
/// root `root`: `<root>/actions/...`, normal components only.
fn published_output(root: Option<&Path>, path: &Path) -> bool {
    let Some(root) = root else {
        return false;
    };
    let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    if plain(root_bytes) && plain(bytes) {
        return plain_rest(root_bytes, bytes).is_some_and(|rest| {
            let mut names = rest.split(|byte| *byte == b'/');
            names.next() == Some(b"actions") && names.all(|name| name != b"..")
        });
    }
    published_output_by_components(root, path)
}

/// [`published_output`] for paths that are not [`plain`], by components.
fn published_output_by_components(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        let mut components = relative.components();
        components.next() == Some(Component::Normal(OsStr::new("actions")))
            && components.all(|component| matches!(component, Component::Normal(_)))
    })
}

/// `tree`, read below another directory (the execution root it was
/// published from), with its files' paths below `root`.
fn relocated(tree: Tree, root: &Path) -> Tree {
    Tree {
        files: tree
            .files
            .into_iter()
            .map(|(name, input)| {
                let path = root.join(&name);
                (name, Input { path, ..input })
            })
            .collect(),
        directories: tree.directories,
    }
}

/// What [`read_staged`] returns for `path` when a per-process memo holds it
/// and no filesystem access is needed: a file of the immutable stores (no
/// published symlink to follow there) or a memoized published tree (a real
/// directory when it was read, and published trees stay in place).
fn memoized_staged(path: &Path, tree: bool, root: Option<&Path>) -> Option<Staged> {
    if tree {
        published_output(root, path)
            .then(|| memoized_tree(path))
            .flatten()
            .map(Staged::Tree)
    } else {
        store_file(root, path)
            .then(|| memoized_file(path))
            .flatten()
            .map(Staged::File)
    }
}

/// [`std::path::absolute`], without rebuilding a path that is already
/// absolute and plain (no repeated or trailing separator, no `.`
/// component), which it would return unchanged: planned inputs are.
fn absolute_input(source: &Path) -> io::Result<PathBuf> {
    let bytes = source.as_os_str().as_bytes();
    let plain = bytes.first() == Some(&b'/')
        && !bytes.ends_with(b"/")
        && !bytes.ends_with(b"/.")
        && !bytes.windows(2).any(|pair| pair == b"//")
        && !bytes.windows(3).any(|triple| triple == b"/./");
    if plain {
        Ok(source.to_path_buf())
    } else {
        std::path::absolute(source)
    }
}

/// A tree input that resolves to a published tree takes its value from the
/// tree memo (read and remembered on a miss); any other tree is read.
fn read_staged(path: &Path, tree: bool, root: Option<&Path>) -> Result<Staged, String> {
    if !tree {
        return Ok(Staged::File(read_input(path, root, true)?));
    }
    let (resolved, _) = resolve_published(path, root)?;
    if !published_output(root, &resolved) {
        return Ok(Staged::Tree(Arc::new(read_tree(&resolved, root, false)?)));
    }
    if let Some(tree) = memoized_tree(&resolved) {
        return Ok(Staged::Tree(tree));
    }
    let tree = Arc::new(read_tree(&resolved, root, false)?);
    remember_tree(resolved, Arc::clone(&tree));
    Ok(Staged::Tree(tree))
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
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination.as_path()),
        )
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
            || !action.embedded_inputs.is_empty()
            || action.extra.transform.is_some()
            || action.outputs.len() != 1)
    {
        return Err("write action requires exactly one output, no program, and empty command, arguments, environment, and inputs".into());
    }
    if action.extra.transform.is_some() && !action.embedded_inputs.is_empty() {
        return Err("only process actions have embedded inputs".into());
    }
    if action.executable_file && action.program.is_none() {
        return Err("an executable File requires a direct program".into());
    }
    if let Some(name) = action
        .extra
        .client_environment
        .keys()
        .find(|name| !action.extra.inherited_environment.contains(*name))
    {
        return Err(format!(
            "client environment value for '{name}', which is not inherited"
        ));
    }
    if (action.write.is_some() || action.extra.transform.is_some())
        && !action.extra.inherited_environment.is_empty()
    {
        return Err("only process actions inherit environment variables".into());
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
        Some(Transform::Template { template, .. })
        | Some(Transform::TemplateKeyed { template, .. }) => {
            check_one_output(action, "template")?;
            if !action.inputs.iter().any(|(_, name)| name == template) {
                return Err("template action template must be an input".into());
            }
            if !action.extra.tree_outputs.is_empty() {
                return Err("template action output cannot be a tree artifact".into());
            }
        }
        Some(Transform::Unsupported { .. }) => check_one_output(action, "unsupported")?,
        // The registration-time shape; planning rewrites it into a Template
        // (crate::engine::resolve_build_info) before plan_action validates
        // the template input.
        Some(Transform::BuildInfo { .. }) => check_one_output(action, "build info")?,
        Some(Transform::RunfilesManifest { entries }) => {
            check_one_output(action, "runfiles manifest")?;
            check_runfiles_entries(action, entries)?;
        }
        Some(Transform::RunfilesTree {
            entries,
            manifest,
            executable,
            ..
        }) => {
            check_one_output(action, "runfiles tree")?;
            check_runfiles_entries(action, entries)?;
            if action.outputs[0].file_name() != Some(std::ffi::OsStr::new("MANIFEST"))
                || action.outputs[0]
                    .parent()
                    .is_none_or(|p| p.as_os_str().is_empty())
                || !action.extra.tree_outputs.is_empty()
            {
                return Err("runfiles tree action output must be a <dir>/MANIFEST file".into());
            }
            for path in std::iter::once(manifest).chain(executable) {
                if !action.inputs.iter().any(|(_, name)| name == path) {
                    return Err(format!(
                        "runfiles tree input '{}' is not an action input",
                        path.display()
                    ));
                }
            }
        }
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
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination),
        )
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
    for destination in action
        .inputs
        .iter()
        .map(|(_, destination)| destination)
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination),
        )
    {
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

/// Runfiles entries: relative, normalized tree paths whose linked
/// destinations are action inputs.
fn check_runfiles_entries(
    action: &Action,
    entries: &[(String, Option<PathBuf>)],
) -> Result<(), String> {
    for (path, destination) in entries {
        validate_relative(Path::new(path))
            .map_err(|e| format!("invalid runfiles path '{path}': {e}"))?;
        if let Some(destination) = destination
            && !action.inputs.iter().any(|(_, name)| name == destination)
        {
            return Err(format!(
                "runfiles entry '{path}' links to '{}', which is not an action input",
                destination.display()
            ));
        }
    }
    Ok(())
}

/// The entries of a runfiles transform with the absolute path each links to.
fn frame_runfiles_entries(
    hash: &mut Sha256,
    entries: &[(String, Option<PathBuf>)],
    origins: &BTreeMap<PathBuf, PathBuf>,
) {
    frame_count(hash, entries.len());
    for (path, destination) in entries {
        frame(hash, path.as_bytes());
        match destination {
            Some(destination) => {
                frame(hash, b"link");
                frame(hash, origins[destination].as_os_str().as_bytes());
            }
            None => frame(hash, b"empty"),
        }
    }
}

fn frame_input(hash: &mut Sha256, input: &Input) {
    frame(hash, &input.digest.hex_bytes());
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
    let paths_only = action
        .extra
        .transform
        .as_ref()
        .filter(|transform| transform.reads_paths_only());
    // Whether an input is read by content: runfiles transforms read only
    // their manifest and executable (the other inputs are keyed by path).
    let read = |destination: &PathBuf| match paths_only {
        None => true,
        Some(Transform::RunfilesTree {
            manifest,
            executable,
            ..
        }) => manifest == destination || executable.as_ref() == Some(destination),
        Some(_) => false,
    };
    let reads: Vec<(&Path, bool)> = action
        .inputs
        .iter()
        .filter(|(_, destination)| read(destination))
        .map(|(source, destination)| {
            (
                source.as_path(),
                action.extra.tree_inputs.contains(destination),
            )
        })
        .collect();
    // Inputs the per-process memos hold need no filesystem access; the
    // others are read up front (many of them, such as an SDK being hashed,
    // on several threads). The loop consumes the results in input order, so
    // it fails exactly where a sequential read would.
    let mut staged: Vec<Option<Result<Staged, String>>> = reads
        .iter()
        .map(|&(source, tree)| memoized_staged(source, tree, root).map(Ok))
        .collect();
    let misses: Vec<usize> = (0..staged.len())
        .filter(|&index| staged[index].is_none())
        .collect();
    let read_misses = try_map_ordered(&misses, |&index| {
        let (source, tree) = reads[index];
        read_staged(source, tree, root)
    });
    for (&index, result) in misses.iter().zip(read_misses) {
        staged[index] = Some(result);
    }
    // The first mapping of each destination (by bytes: validate_structure
    // admitted only relative normal components, so equal bytes are equal
    // paths), in input order, so that failures surface exactly where they
    // did; the ordered maps are then built from sorted runs instead of one
    // component-wise search per input (6,000-input actions).
    let mut staged = staged.into_iter();
    let mut first_origins: HashSet<&[u8]> = HashSet::with_capacity(action.inputs.len());
    let mut origin_entries: Vec<(&PathBuf, PathBuf)> = Vec::with_capacity(action.inputs.len());
    let mut input_slots: HashMap<&[u8], usize> = HashMap::with_capacity(reads.len());
    let mut input_entries: Vec<(&PathBuf, Staged)> = Vec::with_capacity(reads.len());
    for (source, destination) in &action.inputs {
        let name = destination.as_os_str().as_bytes();
        if first_origins.insert(name) {
            origin_entries.push((
                destination,
                absolute_input(source).map_err(|e| io_error("cannot resolve input", source, e))?,
            ));
        }
        if !read(destination) {
            continue;
        }
        // Every input before the first failure has a result.
        let input = staged
            .next()
            .flatten()
            .expect("one result per read input")?;
        match input_slots.entry(name) {
            hash_map::Entry::Occupied(slot) => {
                let same = match (&input_entries[*slot.get()].1, &input) {
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
            }
            hash_map::Entry::Vacant(slot) => {
                slot.insert(input_entries.len());
                input_entries.push((destination, input));
            }
        }
    }
    let by_destination = |a: &PathBuf, b: &PathBuf| {
        compare_relative(a.as_os_str().as_bytes(), b.as_os_str().as_bytes())
    };
    origin_entries.sort_unstable_by(|a, b| by_destination(a.0, b.0));
    input_entries.sort_unstable_by(|a, b| by_destination(a.0, b.0));
    // Sorted and unique: collecting checks the order in one pass.
    let origins: BTreeMap<PathBuf, PathBuf> = origin_entries
        .into_iter()
        .map(|(destination, origin)| (destination.clone(), origin))
        .collect();
    let inputs: BTreeMap<PathBuf, Staged> = input_entries
        .into_iter()
        .map(|(destination, input)| (destination.clone(), input))
        .collect();
    let mut hash = Sha256::new();
    for value in [FORMAT, std::env::consts::OS, std::env::consts::ARCH] {
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
        (
            None,
            _,
            Some(Transform::TemplateKeyed {
                template,
                keyed_substitutions,
                volatile,
                callback_digest,
                ..
            }),
        ) => {
            // BuildInfoFileWriteAction.computeKey (:189-197): GUID,
            // isVolatile, the callback's bzl digest. The timestamp-bearing
            // substitutions are deliberately excluded; the template
            // input's digest enters through the ordinary input framing
            // below (and the status input's through it too for the stable
            // variant, or as constant metadata for the volatile one).
            // `keyed_substitutions` carries only the non-timestamp part of
            // the callback result (volatile SCM keys; the stable result
            // in full, which the regular status input already keys).
            frame(&mut hash, b"build-info-keyed");
            frame(&mut hash, b"7e4657a6-dd09-435e-9423-51d4846aad4a");
            frame(&mut hash, &[u8::from(*volatile)]);
            frame(&mut hash, callback_digest.as_bytes());
            frame(&mut hash, template.as_os_str().as_bytes());
            frame_count(&mut hash, keyed_substitutions.len());
            for (key, value) in keyed_substitutions {
                frame(&mut hash, key);
                frame(&mut hash, value);
            }
        }
        (None, _, Some(Transform::Unsupported { message })) => {
            frame(&mut hash, b"unsupported");
            frame(&mut hash, message.as_bytes());
        }
        (None, _, Some(Transform::BuildInfo { volatile })) => {
            // Replaced during planning (crate::engine); keyed like the
            // resolved action would be, so a stray registration cannot
            // collide with either transform.
            frame(&mut hash, b"build-info");
            frame(&mut hash, &[u8::from(*volatile)]);
        }
        (None, _, Some(Transform::RunfilesManifest { entries })) => {
            frame(&mut hash, b"runfiles-manifest");
            frame_runfiles_entries(&mut hash, entries, &origins);
        }
        (
            None,
            _,
            Some(Transform::RunfilesTree {
                entries,
                manifest,
                executable,
                create,
            }),
        ) => {
            frame(&mut hash, b"runfiles-tree");
            frame(&mut hash, &[u8::from(*create)]);
            frame_runfiles_entries(&mut hash, entries, &origins);
            frame(&mut hash, origins[manifest].as_os_str().as_bytes());
            match executable {
                Some(executable) => {
                    frame(&mut hash, b"sibling");
                    frame(&mut hash, executable.as_os_str().as_bytes());
                    frame(&mut hash, origins[executable].as_os_str().as_bytes());
                }
                None => frame(&mut hash, b"no-sibling"),
            }
        }
        (None, Some(program), None) => {
            frame(&mut hash, b"direct");
            frame(&mut hash, program.as_bytes());
            frame(&mut hash, &[u8::from(action.executable_file)]);
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
            frame(&mut hash, &[u8::from(action.shell_pad)]);
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
    frame_count(&mut hash, action.extra.inherited_environment.len());
    for name in &action.extra.inherited_environment {
        frame(&mut hash, name.as_bytes());
    }
    frame_count(&mut hash, action.extra.client_environment.len());
    for (name, value) in &action.extra.client_environment {
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, value.as_bytes());
    }
    frame(&mut hash, &(inputs.len() as u64).to_le_bytes());
    for (name, input) in &inputs {
        frame(&mut hash, name.as_os_str().as_bytes());
        // Constant-metadata inputs (the volatile status and build-info
        // files) are staged but never keyed: a changed BUILD_TIMESTAMP must
        // not rebuild their consumers
        // (ActionKeyComputer/Artifact.isConstantMetadata).
        if action.extra.constant_metadata.contains(name) {
            frame(&mut hash, b"constant-metadata");
            continue;
        }
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

/// [`write_file`] for a file that is or will be executed: an action output
/// (published 0555, OUTPUT_MODE) or an executable store blob. Written under
/// the exclusive spawn lock so no concurrently started child inherits its
/// write descriptor (crate::spawn_lock).
fn write_executable_file(path: &Path, bytes: &[u8], executable: u32) -> Result<(), String> {
    crate::spawn_lock::exclusive(true, || write_file(path, bytes, executable))
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
    /// Test observation: full (unmemoized) no_linked_ancestor walks.
    static FULL_ANCESTOR_WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Test observation: read_tree walks.
    static TREE_WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Test observation: read_input reads (not served by the file memo).
    static FILE_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
fn copy_verified(source: &Path, target: &Path, expected: Sha256Digest) -> Result<(), String> {
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
    if Sha256Digest(hash.finalize().into()) != expected {
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
    let stripe = usize::from(input.digest.0[0]) % STRIPES.len();
    let _creating = STRIPES[stripe].lock().unwrap_or_else(|e| e.into_inner());
    if blob_valid(&blob, input)? {
        return Ok(blob);
    }
    let candidate = temporary(cas, &input.digest.hex());
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
        copy_verified(&input.path, &candidate, input.digest)?;
        if input.executable != 0 {
            // The verified copy hashes while it writes, too slow for the
            // exclusive spawn lock; re-create the executable blob as an inode
            // no child can hold open for writing (a kernel copy).
            crate::spawn_lock::seal(&candidate)
                .map_err(|e| io_error("cannot seal", &candidate, e))?;
        }
        set_mode(&candidate, blob_mode(input))?;
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|e| io_error("cannot inspect", &candidate, e))?;
        remember_digest(file_key(&metadata, true), input.digest);
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
            crate::spawn_lock::exclusive(mode & 0o111 != 0, || {
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target)
                    .map_err(|e| other(io_error("cannot write", target, e)))?;
                io::copy(&mut input, &mut output)
                    .map_err(|e| other(io_error("cannot copy", target, e)))?;
                Ok(())
            })?;
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
    // The execution root is the process's: always checked in full.
    regular_file(source, None)?;
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
            .map(|(name, input)| (hex(name.as_os_str().as_bytes()), input.digest.hex()))
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
    if real_directory(path, Some(root))?.permissions().mode() & 0o7777 != TREE_ROOT_MODE {
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

/// The tree listings of a valid cache entry. Every validated published tree
/// (also a symlink output's target tree) seeds the tree memo, so consumers
/// in this process never walk it again ([`read_staged`]).
fn cache_valid(
    directory: &Path,
    key: &str,
    outputs: &[PathBuf],
    trees: &BTreeSet<PathBuf>,
    root: &Path,
) -> Option<BTreeMap<PathBuf, Vec<PathBuf>>> {
    let manifest_path = directory.join("manifest.json");
    let mut validated = Vec::new();
    let mut check = || -> Result<Option<BTreeMap<PathBuf, Vec<PathBuf>>>, String> {
        regular_file(&manifest_path, Some(root))?;
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
                if let Some(tree) = &entry.runfiles
                    && !runfiles_tree_valid(&directory.join("outputs"), name, tree, root)
                {
                    return Ok(None);
                }
                path = resolve_published(&path, Some(root))?.0;
            }
            match (&entry.tree, trees.contains(name)) {
                (Some(expected), true) => {
                    let Some(tree) = tree_valid(&path, expected, root)? else {
                        return Ok(None);
                    };
                    listings.insert(name.clone(), tree_listing(&tree));
                    validated.push((path, tree));
                }
                (None, false) => {
                    let actual = read_input(&path, Some(root), false)?;
                    if actual.mode != entry.mode || !actual.digest.spelled(&entry.digest) {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(listings))
    };
    let listings = check().unwrap_or(None)?;
    for (path, tree) in validated {
        if published_output(Some(root), &path) {
            remember_tree(path, Arc::new(tree));
        }
    }
    Some(listings)
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
// concurrently spawned child, even after our own write has closed its fd.
// crate::spawn_lock removes that race for every executable this process writes
// through it and every start through crate::spawn_lock::spawn (Bazel instead
// serializes starts, JavaSubprocessFactory.java:124-157, and has no retry).
// This bounded retry stays as a last resort for writers outside that
// discipline (another process, or a file made executable after an unguarded
// write). Retry only an ETXTBSY spawn error: exec has not succeeded, so the
// action has not run. Never retry wait errors or exit statuses. The injected
// backoff keeps retry-count/error-classification tests deterministic and free
// of sleeps.
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
    store: (&Path, &Path),
) -> Result<(), String> {
    let output = execroot.join(&action.outputs[0]);
    let inputs = &prepared.inputs;
    let origin = |destination: &PathBuf| -> Result<&Path, String> {
        prepared
            .origins
            .get(destination)
            .map(PathBuf::as_path)
            .ok_or_else(|| format!("missing runfiles input '{}'", destination.display()))
    };
    match transform {
        Transform::BuildInfo { .. } => {
            Err("build info transform must be resolved during planning".to_owned())
        }
        Transform::Unsupported { message } => Err(message.clone()),
        Transform::RunfilesManifest { entries } => {
            let mut sorted: Vec<&(String, Option<PathBuf>)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let mut text = Vec::new();
            for (path, destination) in sorted {
                let target = match destination {
                    Some(destination) => Some(origin(destination)?.to_str().ok_or_else(|| {
                        format!(
                            "runfiles target '{}' is not valid UTF-8",
                            destination.display()
                        )
                    })?),
                    None => None,
                };
                crate::runfiles::manifest_line(path, target, &mut text);
            }
            write_file(&output, &text, 0)
        }
        Transform::RunfilesTree {
            entries,
            manifest,
            executable,
            create,
        } => {
            // The output is `<dir>/MANIFEST`; validate_structure checked it.
            let directory = output.parent().expect("validated MANIFEST parent");
            ensure_directory(directory)?;
            if *create {
                for (path, destination) in entries {
                    if path == "MANIFEST" {
                        // linkManifest replaces it (SymlinkTreeHelper.java:167-176).
                        continue;
                    }
                    if path.starts_with("MANIFEST/") {
                        return Err(format!(
                            "runfiles entry '{path}' is below the runfiles MANIFEST"
                        ));
                    }
                    let link = directory.join(path);
                    if let Some(parent) = link.parent() {
                        ensure_directory(parent)?;
                    }
                    match destination {
                        Some(destination) => {
                            std::os::unix::fs::symlink(origin(destination)?, &link)
                                .map_err(|e| io_error("cannot create symbolic link", &link, e))?
                        }
                        None => {
                            // FileSystemUtils.createEmptyFile: 0666 & ~umask.
                            write_file(&link, b"", 0)?;
                            set_mode(&link, 0o644)?;
                        }
                    }
                }
            }
            // createWorkspaceSubdirectory (SymlinkTreeHelper.java:178-188).
            ensure_directory(&directory.join(crate::runfiles::WORKSPACE_NAME))?;
            std::os::unix::fs::symlink(origin(manifest)?, &output)
                .map_err(|e| io_error("cannot create symbolic link", &output, e))?;
            if let Some(executable) = executable {
                let Some(Staged::File(input)) = inputs.get(executable) else {
                    return Err(format!(
                        "runfiles tree executable '{}' is not a regular file",
                        executable.display()
                    ));
                };
                let sibling = execroot.join(executable);
                if let Some(parent) = sibling.parent() {
                    ensure_directory(parent)?;
                }
                let (cas, root) = store;
                link_ensured_blob(cas, root, input, &sibling, blob_mode(input))?;
            }
            Ok(())
        }
        Transform::TemplateKeyed {
            template,
            substitutions,
            executable,
            ..
        }
        | Transform::Template {
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
            write_executable_file(&output, &text, if *executable { 0o111 } else { 0 })
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

/// The entries below a runfiles tree root other than its `MANIFEST`, in byte
/// order, without following symlinks. `cache`: the canonical cache root for
/// a published tree ([`no_linked_ancestor`]).
fn runfiles_listing(root: &Path, cache: Option<&Path>) -> Result<Vec<(String, String)>, String> {
    real_directory(root, cache)?;
    let mut listing = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let mut names = fs::read_dir(&directory)
            .map_err(|e| io_error("cannot list directory", &directory, e))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io_error("cannot list directory", &directory, e))?;
        names.sort();
        for name in names {
            let child = relative.join(&name);
            if child == Path::new("MANIFEST") {
                continue;
            }
            let path = root.join(&child);
            let metadata =
                fs::symlink_metadata(&path).map_err(|e| io_error("cannot inspect", &path, e))?;
            let kind = if metadata.file_type().is_symlink() {
                let target =
                    fs::read_link(&path).map_err(|e| io_error("cannot read link", &path, e))?;
                format!("link:{}", hex(target.as_os_str().as_bytes()))
            } else if metadata.is_dir() {
                pending.push(child.clone());
                "dir".to_owned()
            } else if metadata.is_file() && metadata.len() == 0 {
                "empty".to_owned()
            } else {
                return Err(format!(
                    "runfiles tree entry '{}' is not a symlink, directory or empty file",
                    path.display()
                ));
            };
            listing.push((hex(child.as_os_str().as_bytes()), kind));
        }
    }
    listing.sort();
    Ok(listing)
}

/// Move a created runfiles tree (all of `<dir>` but its already published
/// MANIFEST) and the executable sibling (its blob's inode) into the
/// completed outputs.
fn publish_runfiles_tree(
    execroot: &Path,
    outputs: &Path,
    manifest: &Path,
    sibling: Option<(&PathBuf, &Input)>,
) -> Result<RunfilesTreeManifest, String> {
    let relative = manifest.parent().expect("validated MANIFEST parent");
    let source = execroot.join(relative);
    let target = outputs.join(relative);
    let entries = runfiles_listing(&source, None)?;
    for entry in fs::read_dir(&source).map_err(|e| io_error("cannot list directory", &source, e))? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        fs::rename(source.join(&name), target.join(&name))
            .map_err(|e| io_error("cannot publish", &target.join(&name), e))?;
    }
    let sibling_file = match sibling {
        Some((sibling, input)) => {
            let from = execroot.join(sibling);
            let to = outputs.join(sibling);
            if let Some(parent) = to.parent() {
                ensure_directory(parent)?;
            }
            fs::rename(&from, &to).map_err(|e| io_error("cannot publish", &to, e))?;
            Some((
                hex(sibling.as_os_str().as_bytes()),
                input.digest.hex(),
                blob_mode(input),
            ))
        }
        None => None,
    };
    Ok(RunfilesTreeManifest {
        entries,
        sibling_file,
    })
}

/// A published runfiles tree still has exactly the recorded entries, and the
/// executable sibling is still a regular file with the recorded digest and
/// mode.
fn runfiles_tree_valid(
    outputs: &Path,
    manifest: &Path,
    expected: &RunfilesTreeManifest,
    root: &Path,
) -> bool {
    let relative = manifest.parent().unwrap_or(Path::new(""));
    if runfiles_listing(&outputs.join(relative), Some(root))
        .ok()
        .as_ref()
        != Some(&expected.entries)
    {
        return false;
    }
    match &expected.sibling_file {
        None => true,
        Some((name, digest, mode)) => {
            let Some(name) = unhex(name) else {
                return false;
            };
            let path = outputs.join(std::ffi::OsStr::from_bytes(&name));
            read_input(&path, Some(root), false)
                .is_ok_and(|actual| actual.digest.spelled(digest) && actual.mode == *mode)
        }
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

/// Execute an action or return its verified cached outputs in declaration order.
///
/// Executable bits are preserved and keyed. Other permission bits, timestamps,
/// and source path spellings are not inputs. Only declared regular output files
/// and tree outputs are published; undeclared files are discarded. The cache uses per-key
/// cross-process locks, so unrelated actions may execute concurrently.
/// Shell commands are trusted and unsandboxed, run with Bash; `shell_strict` adds
/// -e -o pipefail for genrules, while custom run_shell actions use only -c.
/// All shell actions get an empty `$0`, then literal `$1`, `$2`, ... . Processes get
/// exactly their fixed environment, the resolved inherited values over it, and
/// the fresh TMPDIR (which replaces any TMPDIR entry); no other caller variables leak in.
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
/// A regular file read by content for a process outside the action graph
/// (a test action, crate::test_runner, N23g): its digest and executable
/// bits, staged like any action input.
#[derive(Clone)]
pub struct ContentInput(Input);

impl ContentInput {
    /// The SHA-256 in lowercase hex.
    pub fn digest(&self) -> String {
        self.0.digest.hex()
    }

    pub fn executable(&self) -> bool {
        self.0.executable != 0
    }
}

/// The canonical cache root, created if needed, and its store directory.
fn cache_store(cache_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    fs::create_dir_all(cache_root).map_err(|e| io_error("cannot create cache", cache_root, e))?;
    let root = fs::canonicalize(cache_root)
        .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
    let cas = root.join(CAS);
    ensure_directory(&cas)?;
    Ok((root, cas))
}

/// Read `path` (a source file or a published output; published symlink
/// outputs are followed) by content, like an action input (prepare).
pub fn read_content_input(cache_root: &Path, path: &Path) -> Result<ContentInput, String> {
    let (root, _) = cache_store(cache_root)?;
    read_input(path, Some(&root), true).map(ContentInput)
}

/// Stage content inputs below `execroot` at their relative destinations:
/// read-only hardlinks of the cache's content-addressed store (N36), like
/// stage_inputs for an action.
pub fn stage_content_inputs(
    cache_root: &Path,
    inputs: &[(ContentInput, PathBuf)],
    execroot: &Path,
) -> Result<(), String> {
    let (root, cas) = cache_store(cache_root)?;
    let mut staged = Vec::new();
    for (input, destination) in inputs {
        validate_relative(destination)?;
        stage_file(&cas, &root, &input.0, execroot, destination, &mut staged)?;
    }
    Ok(())
}

pub fn execute(cache_root: &Path, action: &Action) -> Result<ActionResult, String> {
    execute_with_remote(cache_root, action, crate::remote_cache::active())
}

/// The store blob of compiled-in `bytes` (mode 0444 plus `executable`),
/// created on first use like [`ensure_blob`]'s verified copy.
fn store_bytes(cas: &Path, bytes: &[u8], executable: u32) -> Result<PathBuf, String> {
    let input = Input {
        path: PathBuf::new(),
        digest: digest(bytes),
        size: bytes.len() as u64,
        executable,
        mode: 0o444 | executable,
    };
    let blob = blob_path(cas, &input);
    if blob_valid(&blob, &input)? {
        return Ok(blob);
    }
    let candidate = temporary(cas, &input.digest.hex());
    crate::spawn_lock::exclusive(executable != 0, || write_file(&candidate, bytes, 0))?;
    set_mode(&candidate, blob_mode(&input))?;
    let published = match fs::hard_link(&candidate, &blob) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if blob_valid(&blob, &input)? {
                Ok(())
            } else {
                fs::rename(&candidate, &blob).map_err(|e| io_error("cannot store", &blob, e))
            }
        }
        Err(_) => fs::rename(&candidate, &blob).map_err(|e| io_error("cannot store", &blob, e)),
    };
    let _ = fs::remove_file(&candidate);
    published.map(|()| blob)
}

/// `action` with its embedded inputs stored in the content-addressed store
/// `cas` and turned into ordinary inputs of their blobs (N41's
/// genrule-setup.sh, staged through N36's store like every input).
fn with_embedded_inputs(action: &Action, cas: &Path) -> Result<Action, String> {
    let mut action = action.clone();
    for (destination, bytes) in std::mem::take(&mut action.embedded_inputs) {
        let blob = store_bytes(cas, bytes, 0o111)?;
        action.inputs.push((blob, destination));
    }
    Ok(action)
}

/// The REAPI action of a prepared process action
/// (RemoteExecutionService.buildRemoteAction, crate::remote_cache): the argv,
/// [`resolved_environment`], the input root (every regular input file at its
/// execution path; tree inputs as their files, without empty
/// subdirectories; parameter files at their paths, ParamFileActionInput)
/// and the declared outputs.
fn remote_action(
    action: &Action,
    prepared: &PreparedAction,
) -> Result<crate::remote_cache::RemoteAction, String> {
    use crate::remote_cache::path_string;
    let file_digest = |input: &Input| crate::reapi::Digest {
        hash: input.digest.hex(),
        size: input.size,
    };
    let mut files = BTreeMap::new();
    for (path, staged) in &prepared.inputs {
        match staged {
            Staged::File(input) => {
                files.insert(path_string(path)?, file_digest(input));
            }
            Staged::Tree(tree) => {
                for (name, input) in &tree.files {
                    files.insert(path_string(&path.join(name))?, file_digest(input));
                }
            }
        }
    }
    for (path, content) in &action.extra.param_files {
        files.insert(path_string(path)?, crate::reapi::Digest::of(content));
    }
    let outputs = action
        .outputs
        .iter()
        .map(|path| path_string(path))
        .collect::<Result<Vec<_>, _>>()?;
    let environment = resolved_environment(action)
        .into_iter()
        .map(|(name, value)| match value.into_string() {
            Ok(value) => Ok((name, value)),
            Err(_) => Err(format!(
                "non-UTF-8 value of {name} cannot enter a remote cache key"
            )),
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    crate::remote_cache::remote_action(
        arguments(action),
        &environment,
        &files,
        &outputs,
        remote_policy(action).1,
    )
}

fn replay_logs(id: &str, stdout: &str, stderr: &str) {
    if !stdout.is_empty() || !stderr.is_empty() {
        eprintln!("action '{id}' logs:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
}

/// [`execute`] with an explicit remote cache (RemoteSpawnCache.lookup/store,
/// remote/RemoteSpawnCache.java:85-305): after the native cache, process
/// actions consult the remote cache (`--remote_accept_cached`) and upload a
/// successful local execution (`--remote_upload_local_results`). Writes and
/// native transforms never use it (FileWriteAction is not a spawn). A hit's
/// outputs are written into the execution root and published through the
/// content-addressed store like executed outputs. Credential helper failures
/// fail the action; other cache failures are warnings and the action runs
/// locally.
pub fn execute_with_remote(
    cache_root: &Path,
    action: &Action,
    remote: Option<&crate::remote_cache::RemoteCache>,
) -> Result<ActionResult, String> {
    let embedded;
    let action = if action.embedded_inputs.is_empty() {
        action
    } else {
        // Compiled-in inputs live in the store: validate first, then create
        // the cache to store them.
        validate_structure(action)?;
        fs::create_dir_all(cache_root)
            .map_err(|e| io_error("cannot create cache", cache_root, e))?;
        let root = fs::canonicalize(cache_root)
            .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
        let cas = root.join(CAS);
        ensure_directory(&cas)?;
        embedded = with_embedded_inputs(action, &cas)?;
        &embedded
    };
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
    let result = |listings: BTreeMap<PathBuf, Vec<PathBuf>>, cache_hit: bool, remote_hit: bool| {
        ActionResult {
            outputs: action
                .outputs
                .iter()
                .map(|p| cached.join("outputs").join(p))
                .collect(),
            cache_hit,
            remote_hit,
            trees: action
                .outputs
                .iter()
                .enumerate()
                .filter_map(|(index, name)| Some((index, listings.get(name)?.clone())))
                .collect(),
        }
    };
    if let Some(listings) = cache_valid(&cached, key, outputs, &action.extra.tree_outputs, &root) {
        return Ok(result(listings, true, false));
    }
    // This entry's trees are no longer what the memo holds: they are removed
    // now and published again below.
    forget_trees(&cached);
    remove_stale(&cached)?;
    let work = fresh_work(&work_parent, key)?;
    let execroot = work.0.join("execroot");
    let tmp = work.0.join("tmp");
    ensure_directory(&execroot)?;
    ensure_directory(&tmp)?;
    let process = action.write.is_none() && action.extra.transform.is_none();
    // The remote cache applies to cacheable process actions only
    // (RemoteSpawnCache.java:85-92).
    let remote = remote
        .filter(|_| process && remote_policy(action).0)
        .filter(|remote| remote.accept_cached || remote.upload_local_results);
    // Tree artifact outputs need ActionResult.output_directories, which the
    // native remote cache does not model: such spawns stay local.
    let tree_reason;
    let unsupported = match (
        &action.remote_unsupported,
        action.extra.tree_outputs.first(),
    ) {
        (Some(reason), _) => Some(reason.as_str()),
        (None, Some(tree)) => {
            tree_reason = format!(
                "{} (tree artifact output {}; output_directories are not modelled)",
                action.id,
                tree.display()
            );
            Some(tree_reason.as_str())
        }
        (None, None) => None,
    };
    let remote = match (remote, unsupported) {
        (Some(_), Some(reason)) => {
            crate::remote_cache::warn(&crate::remote_cache::CacheError::Other(format!(
                "not used for {reason}"
            )));
            None
        }
        (remote, _) => remote,
    };
    let remote_spec = match remote {
        Some(_) => match remote_action(action, &prepared) {
            Ok(spec) => Some(spec),
            Err(reason) => {
                crate::remote_cache::warn(&crate::remote_cache::CacheError::Other(format!(
                    "not used for {}: {reason}",
                    action.id
                )));
                None
            }
        },
        None => None,
    };
    let remote = remote.filter(|_| remote_spec.is_some());
    let mut remote_hit = None;
    if let (Some(remote), Some(spec)) = (remote, &remote_spec)
        && remote.accept_cached
    {
        let declared = action
            .outputs
            .iter()
            .map(|path| crate::remote_cache::path_string(path))
            .collect::<Result<Vec<_>, _>>()?;
        match remote.lookup(spec, &declared) {
            Ok(hit) => remote_hit = hit,
            Err(crate::remote_cache::CacheError::Credentials(error)) => {
                // Utils.createExecExceptionForCredentialHelperException
                // (remote/util/Utils.java:637-648) and its cause.
                return Err(format!(
                    "Exec failed due to CredentialHelperException: {error}"
                ));
            }
            Err(error) => crate::remote_cache::warn(&error),
        }
    }
    // Only a process reads its inputs through the execution root; native
    // writes and transforms read the verified input files directly, and a
    // remote hit runs nothing.
    let staged = if process && remote_hit.is_none() {
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
    let started = SystemTime::now();
    let clock = std::time::Instant::now();
    let mut logs = (Vec::new(), Vec::new());
    // Whether the executed spawn ran under the sandbox strategy (N59); a
    // native write, transform or remote hit runs no process at all.
    let mut sandboxed = false;
    if let Some(write) = &action.write {
        write_executable_file(
            &execroot.join(&outputs[0]),
            &write.content,
            if write.executable { 0o111 } else { 0 },
        )?;
    } else if let Some(transform) = &action.extra.transform {
        run_transform(action, transform, &prepared, &execroot, (&cas, &root))?;
    } else if let Some(hit) = &remote_hit {
        // The downloaded (verified) outputs take the place of the process's;
        // they are published below like executed outputs.
        for (name, bytes) in action.outputs.iter().zip(&hit.outputs) {
            write_executable_file(&execroot.join(name), bytes, 0o111)?;
        }
        replay_logs(
            &action.id,
            &String::from_utf8_lossy(&hit.stdout),
            &String::from_utf8_lossy(&hit.stderr),
        );
    } else {
        let mut stdout = create_log(&work.0.join("stdout.log"))?;
        let mut stderr = create_log(&work.0.join("stderr.log"))?;
        let argv = arguments(action);
        // The spawn strategy (N59): the highest-priority strategy that can
        // execute this spawn (SpawnStrategyResolver.resolveOne), resolved per
        // action from its mnemonic. The strategy never changes the action key:
        // a cache entry produced sandboxed satisfies a standalone request and
        // vice versa, like Bazel's action cache. The execution info merges
        // the rule tags with the spawn's execution_requirements map, exactly
        // like remote_policy (crate::action::remote_policy).
        let execution_info = {
            let mut merged = action.execution_info.clone();
            merged.extend(action.extra.execution_info.keys().cloned());
            merged
        };
        let strategy = crate::sandbox::resolve(&action.extra.mnemonic, &execution_info)
            .map_err(|error| format!("action '{}': {error}", action.id))?;
        sandboxed = strategy == crate::sandbox::Strategy::Sandboxed;
        let environment = resolved_environment(action);
        let mut command = if sandboxed {
            // The executor starts its own binary (/proc/self/exe) as the
            // sandbox worker (the role linux-sandbox fills as the spawn's
            // argv[0], SandboxedSpawnStrategy.exec ->
            // LinuxSandboxCommandLineBuilder.buildForCommand) with the
            // action's cwd and environment; the worker applies the sandbox
            // and execs the action. The wrapped argv carries the action's own
            // arguments after --.
            let mut command = Command::new("/proc/self/exe");
            command.args(crate::sandbox::wrap(
                &execroot,
                &tmp,
                &environment,
                crate::sandbox::denies_network(&execution_info),
                &argv,
                &work.0,
                None,
            ));
            command
        } else if action.executable_file && !argv[0].starts_with('/') {
            // Bazel runs a File executable by its execution path, relative
            // to the execution root; argv[0] keeps that spelling.
            use std::os::unix::process::CommandExt;
            let mut command = Command::new(execroot.join(&argv[0]));
            command.arg0(&argv[0]);
            command.args(&argv[1..]);
            command
        } else {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        };
        command.current_dir(&execroot).env_clear();
        {
            // Its own process group: the memory watchdog kills live
            // groups on a ceiling hit (crate::memory_limit), and a
            // group also keeps an action's descendants addressable.
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        // Exactly resolved_environment (ActionEnvironment.resolve,
        // ActionEnvironment.java:113-122), the remote key's environment;
        // then PosixLocalEnvProvider's TMPDIR, last.
        command.envs(&environment);
        command
            .env("TMPDIR", &tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?));
        let mut child = retry_spawn(
            || crate::spawn_lock::spawn(&mut command),
            std::thread::sleep,
        )
        .map_err(|e| format!("cannot execute action '{}': {e}", action.id))?;
        let status = child
            .wait()
            .map_err(|e| format!("cannot wait for action '{}': {e}", action.id))?;
        crate::sandbox::count(strategy);
        if remote_spec.is_some() {
            logs = (
                fs::read(work.0.join("stdout.log"))
                    .map_err(|e| format!("cannot read action stdout: {e}"))?,
                fs::read(work.0.join("stderr.log"))
                    .map_err(|e| format!("cannot read action stderr: {e}"))?,
            );
        }
        let stdout =
            log_tail(&mut stdout).map_err(|e| format!("cannot read action stdout: {e}"))?;
        let stderr =
            log_tail(&mut stderr).map_err(|e| format!("cannot read action stderr: {e}"))?;
        // A sandbox setup failure is an infrastructure failure of the
        // strategy (AbstractSandboxSpawnRunner.runSpawn:
        // sandbox.createFileSystem -> "Could not copy inputs into sandbox"),
        // not the action's: the worker reports it through the setup-error
        // file, so no action exit code is mistaken for one.
        if sandboxed
            && let Ok(error) = fs::read_to_string(work.0.join("sandbox-setup-error"))
            && !error.is_empty()
        {
            crate::sandbox::note_setup_failure();
            let message = format!(
                "action '{}' failed: could not set up the sandbox: {}",
                action.id,
                error.trim_end()
            );
            if crate::sandbox::debug() {
                let sandbox_root = work.0.display().to_string();
                std::mem::forget(work);
                return Err(format!(
                    "{message}\n\n(sandbox build root retained at '{sandbox_root}')"
                ));
            }
            return Err(message);
        }
        check_staged(&staged, &execroot).map_err(|e| format!("action '{}': {e}", action.id))?;
        if !status.success() {
            // The sandbox strategy's failure message appends Bazel's
            // suggestion unless --sandbox_debug itself is on (makeFailureMessage,
            // AbstractSandboxSpawnRunner.java:186-195). The sandbox contents —
            // this action's execroot, with the failed action's outputs in it —
            // are retained below exactly when --sandbox_debug is on
            // (runSpawn's finally: sandbox.delete() unless sandboxDebug).
            if sandboxed {
                if crate::sandbox::debug() {
                    let sandbox_root = work.0.display().to_string();
                    std::mem::forget(work);
                    return Err(format!(
                        "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}\n\n\
                         (sandbox build root retained at '{sandbox_root}')",
                        action.id, status, stdout, stderr
                    ));
                }
                return Err(format!(
                    "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}{}",
                    action.id,
                    status,
                    stdout,
                    stderr,
                    crate::sandbox::SANDBOX_DEBUG_SUGGESTION
                ));
            }
            return Err(format!(
                "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}",
                action.id, status, stdout, stderr
            ));
        }
        replay_logs(&action.id, &stdout, &stderr);
    }
    let wall = clock.elapsed();
    // Publish every output into the completed directory by link or rename,
    // never by copying contents; the manifest describes exactly what is
    // published.
    let symlink_target = match &action.extra.transform {
        Some(Transform::Symlink { target, .. }) => Some(target),
        Some(Transform::RunfilesTree { manifest, .. }) => Some(manifest),
        _ => None,
    };
    let runfiles_sibling = match &action.extra.transform {
        Some(Transform::RunfilesTree { executable, .. }) => Some(executable.as_ref()),
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
    // Published tree outputs, for the tree memo once the entry is in place.
    let mut published_trees = Vec::new();
    // Published regular files and their digests, for the persistent memo.
    let mut published_files = Vec::new();
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
                let runfiles = match runfiles_sibling {
                    Some(sibling) => Some(publish_runfiles_tree(
                        &execroot,
                        &completed.join("outputs"),
                        name,
                        sibling.map(|sibling| match &inputs[sibling] {
                            Staged::File(input) => (sibling, input),
                            Staged::Tree(_) => unreachable!("checked by run_transform"),
                        }),
                    )?),
                    None => None,
                };
                return Ok(match &inputs[symlink_target] {
                    Staged::File(input) => Output {
                        name: hex_name,
                        digest: input.digest.hex(),
                        mode: input.mode,
                        tree: None,
                        symlink,
                        runfiles,
                    },
                    Staged::Tree(tree) => {
                        listings.insert(name.clone(), tree_listing(tree));
                        Output {
                            name: hex_name,
                            digest: String::new(),
                            mode: TREE_ROOT_MODE,
                            tree: Some(tree_manifest(tree)),
                            symlink,
                            runfiles: None,
                        }
                    }
                });
            }
            if tree_output {
                let tree = publish_tree(&cas, &root, &source, &target)?;
                listings.insert(name.clone(), tree_listing(&tree));
                let output = Output {
                    name: hex_name,
                    digest: String::new(),
                    mode: TREE_ROOT_MODE,
                    tree: Some(tree_manifest(&tree)),
                    symlink: None,
                    runfiles: None,
                };
                published_trees.push((name, tree));
                return Ok(output);
            }
            // Match metadata collection order: a successful action may leave a
            // regular output unreadable. Normalize before reading its bytes,
            // but never chmod a symlink or a path through a linked ancestor.
            let input = publish_file(&cas, &root, &source, &target)?;
            published_files.push((name, input.digest));
            Ok(Output {
                name: hex_name,
                digest: input.digest.hex(),
                mode: OUTPUT_MODE,
                tree: None,
                symlink: None,
                runfiles: None,
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
    // What a consumer would read from the published entry: the same files,
    // digests and (normalized) modes at their published paths.
    for (name, digest) in &published_files {
        remember_published(&cached.join("outputs").join(name), *digest);
    }
    for (name, tree) in published_trees {
        let path = cached.join("outputs").join(name);
        if published_output(Some(&root), &path) {
            let tree = relocated(tree, &path);
            for input in tree.files.values() {
                remember_published(&input.path, input.digest);
            }
            remember_tree(path, Arc::new(tree));
        }
    }
    if remote_hit.is_some() {
        return Ok(result(listings, false, true));
    }
    if let (Some(remote), Some(spec)) = (remote, &remote_spec)
        && remote.upload_local_results
    {
        let uploads = action
            .outputs
            .iter()
            .map(|name| {
                let path = cached.join("outputs").join(name);
                Ok((
                    crate::remote_cache::path_string(name)?,
                    fs::read(&path).map_err(|e| io_error("cannot read output", &path, e))?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        // RemoteExecutionService.reportUploadError: every upload failure,
        // credential helper failures included, is a warning.
        if let Err(error) = remote.upload(spec, &uploads, &logs.0, &logs.1, started, wall) {
            crate::remote_cache::warn(&error);
        }
    }
    // --sandbox_debug retains the sandbox build root after the build too
    // (SandboxOptions.sandbox_debug: "the sandbox root contents are left
    // untouched after a build").
    if sandboxed && crate::sandbox::debug() {
        std::mem::forget(work);
    }
    Ok(result(listings, false, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};

    include!("../tests/support/executable_script.rs");

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
                ..Default::default()
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
                ..Default::default()
            }
        }

        fn direct_action(&self, script: &str) -> Action {
            let source = self.0.join("executable-source");
            // Not fs::write: see write_executable_script (ETXTBSY).
            write_executable_script(&source, script, 0o755);
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
                ..Default::default()
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
    fn runfiles_tree_publishes_links_and_validates_cached_entries() {
        let fixture = Fixture::new();
        let data = fixture.0.join("data.txt");
        fs::write(&data, b"data\n").unwrap();
        let manifest = fixture.0.join("b.runfiles_manifest");
        fs::write(&manifest, b"_main/d data\n").unwrap();
        let exe = fixture.0.join("b");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let action = Action {
            id: "tree".into(),
            command: String::new(),
            inputs: vec![
                (data.clone(), "pkg/data.txt".into()),
                (manifest.clone(), "bin/pkg/b.runfiles_manifest".into()),
                (exe.clone(), "bin/pkg/b".into()),
            ],
            outputs: vec!["bin/pkg/b.runfiles/MANIFEST".into()],
            write: None,
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            shell_strict: false,
            extra: ActionExtra {
                transform: Some(Transform::RunfilesTree {
                    entries: vec![
                        ("_main/pkg/d".into(), Some("pkg/data.txt".into())),
                        ("_main/pkg/__init__.py".into(), None),
                    ],
                    manifest: "bin/pkg/b.runfiles_manifest".into(),
                    executable: Some("bin/pkg/b".into()),
                    create: true,
                }),
                ..ActionExtra::default()
            },
            ..Action::default()
        };
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        let manifest_link = &first.outputs[0];
        assert_eq!(fs::read_link(manifest_link).unwrap(), manifest);
        let tree = manifest_link.parent().unwrap();
        assert_eq!(fs::read_link(tree.join("_main/pkg/d")).unwrap(), data);
        assert_eq!(
            fs::metadata(tree.join("_main/pkg/__init__.py"))
                .unwrap()
                .len(),
            0
        );
        // The sibling is the executable's store blob (one inode), not a link.
        let sibling = tree.parent().unwrap().join("b");
        let metadata = fs::symlink_metadata(&sibling).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o555);
        let blob = fixture
            .cache()
            .join("cas")
            .join(format!("{}-555", digest(b"#!/bin/sh\n")));
        assert_eq!(metadata.ino(), fs::metadata(&blob).unwrap().ino());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // A damaged sibling or tree entry is rebuilt.
        fs::remove_file(&sibling).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        fs::write(tree.join("_main/extra"), b"").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        // Content changes of a linked source do not change the tree.
        fs::write(&data, b"changed\n").unwrap();
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // SKIP mode: only MANIFEST and `_main`.
        let mut skip = action.clone();
        if let Some(Transform::RunfilesTree { create, .. }) = &mut skip.extra.transform {
            *create = false;
        }
        let skipped = execute(&fixture.cache(), &skip).unwrap();
        let tree = skipped.outputs[0].parent().unwrap();
        let mut names: Vec<_> = fs::read_dir(tree)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["MANIFEST", "_main"]);
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
            "#!/bin/bash\nfalse\nfalse | true\ntest \"$#\" = 4 || exit 12\ncase \"$(export -p)\" in *' PATH='*) exit 13;; esac\ntest -z \"${HOME+x}\" || exit 14\n[[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" || exit 15\nprintf '%s\\0' \"$0\" \"$1\" \"$2\" \"$3\" \"$4\" \"$VALUE\" > out/result\n",
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
    fn padded_shell_has_empty_zero_and_literal_first_argument() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.shell_strict = false;
        // Without padding `$0` is the shell, as in Bazel's `bash -c CMD` (N41).
        action.command =
            "test \"$0\" = /bin/bash && test \"$#\" = 0 && printf ok > out/result".into();
        execute(&fixture.cache(), &action).unwrap();
        action.shell_pad = true;
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
        action.shell_pad = true;
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
    fn shell_environment_is_exact_keyed_and_tmpdir_always_fresh() {
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
        // PosixLocalEnvProvider.rewriteLocalEnv (:41-59): an action TMPDIR
        // never reaches the process.
        action.command = "test \"$PATH\" = /explicit/path && test \"$LANG\" = POSIX && test \"$LC_ALL\" = POSIX && [[ $TMPDIR = /* ]] && test \"$TMPDIR\" != /explicit/tmp && test -d \"$TMPDIR\" && test \"${EMPTY+x}\" = x && test -z \"$EMPTY\" && test -z \"${HOME+x}\" && printf '%s' \"$VALUE\" > out/result".into();
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
    fn empty_environment_is_only_tmpdir() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        // No native PATH/LANG/LC_ALL defaults (docs/ACTION_ENV.md); bash
        // itself exports PWD, SHLVL and OLDPWD and defines (without
        // exporting) a default PATH.
        action.command = "case \"$(export -p)\" in *' PATH='*) exit 1;; esac && test -z \"${LANG+x}\" && test -z \"${LC_ALL+x}\" && test -z \"${HOME+x}\" && test -z \"${CARGO_HOME+x}\" && [[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" && printf ok > out/result".into();
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

    /// A separate process holding `path` open for writing until dropped.
    struct WriterProcess(std::process::Child);

    impl WriterProcess {
        fn open(path: &Path) -> Self {
            use std::io::BufRead;
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(r#"exec 3>>"$1" && echo open && exec sleep 600"#)
                .arg("writer")
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "open\n");
            Self(child)
        }
    }

    impl Drop for WriterProcess {
        fn drop(&mut self) {
            // Its descriptors are closed once it can be reaped.
            let _ = self.0.kill();
            let _ = self.0.wait();
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
        // The writer lives in a helper process, never in this one: a write
        // descriptor opened here would be inherited by every child another
        // test thread spawns meanwhile and would keep exec busy until those
        // children exec, which on a loaded host outlasted the whole 127 ms
        // retry budget. Killing and reaping the helper removes the only
        // writer, so the retry after the first back-off must succeed.
        let mut writer = Some(WriterProcess::open(executable));
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
        assert_eq!((attempts, waits), (2, 1));
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

    fn tree_walks() -> u64 {
        TREE_WALKS.with(std::cell::Cell::get)
    }

    fn full_ancestor_walks() -> u64 {
        FULL_ANCESTOR_WALKS.with(std::cell::Cell::get)
    }

    /// File paths of a tree, which Input equality ignores.
    fn tree_paths(tree: &Tree) -> Vec<&Path> {
        tree.files
            .values()
            .map(|input| input.path.as_path())
            .collect()
    }

    #[test]
    fn published_trees_are_read_once_per_process_and_replaced_when_published_again() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("fail");
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into()];
        producer.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        // The marker is not an input: it fails a re-execution with the same key.
        producer.command = format!(
            "test ! -e '{}' && mkdir -p out/tree/sub/empty && printf a > out/tree/a && printf b > out/tree/sub/b",
            marker.display()
        );
        let tree = execute(&fixture.cache(), &producer).unwrap().outputs[0].clone();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        // The publication seeds the memo with exactly what a consumer reads.
        let seeded = memoized_tree(&tree).expect("published tree is memoized");
        let fresh = read_tree(&tree, Some(&root), false).unwrap();
        assert!(*seeded == fresh);
        assert_eq!(tree_paths(&seeded), tree_paths(&fresh));
        let consumer = |id: &str| {
            let mut consumer = fixture.action();
            consumer.id = id.into();
            consumer.inputs = vec![(tree.clone(), "in/tree".into())];
            consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
            consumer.command =
                "cat in/tree/a in/tree/sub/b > out/result; test -d in/tree/sub/empty".into();
            consumer
        };
        // Consumers stage the memoized tree; nobody walks it again.
        let walks = tree_walks();
        for id in ["first", "second"] {
            let consumed = execute(&fixture.cache(), &consumer(id)).unwrap();
            assert!(!consumed.cache_hit);
            assert_eq!(fs::read(&consumed.outputs[0]).unwrap(), b"ab");
            assert!(execute(&fixture.cache(), &consumer(id)).unwrap().cache_hit);
        }
        assert_eq!(tree_walks(), walks);
        // The memoized value keys a consumer exactly like a fresh read.
        let memo_key = prepare(&consumer("first"), Some(&root)).unwrap().key;
        forget_trees(&tree);
        assert_eq!(
            prepare(&consumer("first"), Some(&root)).unwrap().key,
            memo_key
        );
        assert_eq!(tree_walks(), walks + 1);
        // The producer's cache check always validates the published tree, and
        // the validated value replaces the entry.
        let before = memoized_tree(&tree).unwrap();
        assert!(execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(tree_walks(), walks + 2);
        assert!(!Arc::ptr_eq(&before, &memoized_tree(&tree).unwrap()));
        // A damaged entry whose re-execution fails leaves no memoized tree.
        fs::set_permissions(tree.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(tree.join("sub/b")).unwrap();
        fs::write(&marker, b"").unwrap();
        assert!(execute(&fixture.cache(), &producer).is_err());
        assert!(memoized_tree(&tree).is_none());
        let Err(error) = prepare(&consumer("first"), Some(&root)) else {
            panic!("a removed tree is not served from the memo");
        };
        assert!(error.contains("cannot inspect"), "{error}");
        // Published again, the new tree replaces it for every consumer.
        fs::remove_file(&marker).unwrap();
        assert!(!execute(&fixture.cache(), &producer).unwrap().cache_hit);
        let republished = memoized_tree(&tree).expect("republished tree is memoized");
        assert!(*republished == read_tree(&tree, Some(&root), false).unwrap());
        assert!(
            execute(&fixture.cache(), &consumer("first"))
                .unwrap()
                .cache_hit
        );
        let third = execute(&fixture.cache(), &consumer("third")).unwrap();
        assert_eq!(fs::read(&third.outputs[0]).unwrap(), b"ab");
    }

    #[test]
    fn ordered_maps_report_the_first_error_in_item_order() {
        let fail = |&item: &usize| {
            if item % 400 == 399 {
                Err(format!("item {item}"))
            } else {
                Ok(item * 2)
            }
        };
        // Few items: mapped on this thread, up to the first error.
        let small: Vec<usize> = (390..410).collect();
        let results = try_map_ordered(&small, fail);
        assert_eq!(results.len(), 10);
        assert_eq!(results[9], Err("item 399".to_owned()));
        assert!(results[..9].iter().all(Result::is_ok));
        // Many: every item mapped (on several threads), in order.
        let large: Vec<usize> = (0..1000).collect();
        let results = try_map_ordered(&large, fail);
        assert_eq!(results.len(), 1000);
        assert_eq!(results.iter().position(Result::is_err), Some(399));
        assert_eq!(results[998], Ok(1996));
        assert_eq!(results[799], Err("item 799".to_owned()));
        let panicked = std::panic::catch_unwind(|| {
            try_map_ordered(&large, |&item| {
                assert_ne!(item, 700, "intentional panic");
                Ok(item)
            })
        });
        assert!(panicked.is_err());
    }

    /// Concurrent and nested calls take their helpers from one budget: the
    /// threads mapping at any moment never outnumber the calling threads plus
    /// the budget, every helper is given back (also after a panic), and an
    /// exhausted budget maps on the calling thread, sequentially.
    #[test]
    fn ordered_maps_share_one_helper_budget() {
        use std::sync::atomic::AtomicUsize;
        const CAPACITY: usize = 3;
        const CALLERS: usize = 6;
        let budget = HelperBudget::new(CAPACITY);
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let mappers = Mutex::new(HashSet::new());
        let leaf = |&value: &usize| -> Result<usize, String> {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            mappers.lock().unwrap().insert(std::thread::current().id());
            std::thread::sleep(Duration::from_micros(20));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(value)
        };
        // Each outer item maps 96 leaves itself (nested, like a tree input
        // read by prepare); unbudgeted, 6 callers would map on up to
        // 6 × 2 × 3 threads at once.
        let inner: Vec<usize> = (0..96).collect();
        let outer = |&chunk: &usize| -> Result<usize, String> {
            let leaves = try_map_ordered_in(&budget, &inner, leaf);
            Ok(chunk + leaves.into_iter().map(Result::unwrap).sum::<usize>())
        };
        let chunks: Vec<usize> = (0..64).collect();
        let callers: HashSet<std::thread::ThreadId> = std::thread::scope(|scope| {
            let calls: Vec<_> = (0..CALLERS)
                .map(|_| {
                    scope.spawn(|| {
                        let results = try_map_ordered_in(&budget, &chunks, outer);
                        let expected: Vec<Result<usize, String>> =
                            chunks.iter().map(|chunk| Ok(chunk + 4560)).collect();
                        assert_eq!(results, expected);
                        std::thread::current().id()
                    })
                })
                .collect();
            calls.into_iter().map(|call| call.join().unwrap()).collect()
        });
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= CALLERS + CAPACITY, "{peak} threads mapped at once");
        if available_cores() > 2 {
            assert!(
                mappers
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|id| !callers.contains(id)),
                "no helper ever ran"
            );
        }
        assert_eq!(budget.available.load(Ordering::SeqCst), CAPACITY);
        // A panicking item gives its call's helpers back.
        let large: Vec<usize> = (0..1000).collect();
        let panicked = std::panic::catch_unwind(|| {
            try_map_ordered_in(&budget, &large, |&item| {
                assert_ne!(item, 700, "intentional panic");
                Ok(item)
            })
        });
        assert!(panicked.is_err());
        assert_eq!(budget.available.load(Ordering::SeqCst), CAPACITY);
        // Exhausted: everything on the calling thread, up to the first error.
        let none = HelperBudget::new(0);
        let caller = std::thread::current().id();
        let results = try_map_ordered_in(&none, &large, |&item| {
            assert_eq!(std::thread::current().id(), caller);
            if item == 399 {
                Err(format!("item {item}"))
            } else {
                Ok(item)
            }
        });
        assert_eq!(results.len(), 400);
        assert_eq!(results[399], Err("item 399".to_owned()));
        assert_eq!(none.available.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn large_trees_and_input_sets_fail_like_sequential_reads() {
        let fixture = Fixture::new();
        let tree = fixture.0.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        for i in 0..300 {
            fs::write(tree.join(format!("f{i:03}")), format!("{i}")).unwrap();
        }
        fs::write(tree.join("sub/x"), b"x").unwrap();
        let read = read_tree(&tree, None, false).unwrap();
        assert_eq!(read.files.len(), 301);
        assert_eq!(read.directories, BTreeSet::from([PathBuf::from("sub")]));
        for (name, input) in &read.files {
            let expected = read_regular(&tree.join(name), false, false).unwrap();
            assert!(*input == expected);
            assert_eq!((&input.path, input.size), (&expected.path, expected.size));
        }
        // Inputs: the first failure in input order, wherever it is read.
        let mut action = fixture.action();
        action.inputs = (0..300)
            .map(|i| {
                (
                    tree.join(format!("f{i:03}")),
                    PathBuf::from(format!("in/{i}")),
                )
            })
            .collect();
        action.inputs.push((tree.join("f001"), "in/0".into()));
        action.inputs[100].0 = fixture.0.join("missing");
        let Err(error) = prepare(&action, None) else {
            panic!("missing input");
        };
        assert!(error.starts_with("cannot inspect") && error.contains("missing"));
        action.inputs[100].0 = tree.join("f100");
        let Err(error) = prepare(&action, None) else {
            panic!("conflicting duplicate");
        };
        assert_eq!(error, "conflicting duplicate input destination 'in/0'");
        action.inputs.pop();
        assert!(prepare(&action, None).is_ok());
        // Trees: a file that cannot be read before a later walk failure.
        symlink("x", tree.join("sub/link")).unwrap();
        if !unsafe_is_root() {
            fs::set_permissions(tree.join("f150"), fs::Permissions::from_mode(0o000)).unwrap();
            let Err(error) = read_tree(&tree, None, false) else {
                panic!("unreadable file");
            };
            assert!(
                error.starts_with(&format!("cannot read '{}'", tree.join("f150").display())),
                "{error}"
            );
            fs::set_permissions(tree.join("f150"), fs::Permissions::from_mode(0o644)).unwrap();
        }
        let Err(error) = read_tree(&tree, None, false) else {
            panic!("symlink in tree");
        };
        assert!(
            error.starts_with(&format!(
                "tree artifact entry '{}' is a symlink",
                tree.join("sub/link").display()
            )),
            "{error}"
        );
    }

    #[test]
    fn immutable_store_files_are_read_once_per_process() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let read = |path: &Path| {
            let before = FILE_READS.with(std::cell::Cell::get);
            let input = read_input(path, Some(&root), true).unwrap();
            (input, FILE_READS.with(std::cell::Cell::get) - before)
        };
        let create = |path: &Path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"package x\n").unwrap();
        };
        // Exactly the repository tree stores (registry modules, repository
        // rules, Git sources, the embedded bazel_tools) and blobs.
        for path in [
            root.join("repositories/trees/abc/pkg/file.go"),
            root.join("repositories/direct/trees/def/file.go"),
            root.join("repositories/git/trees/ghi/file.go"),
            root.join("repositories/builtins/trees/jkl/tools/file.sh"),
            root.join("cas/0123-444"),
        ] {
            create(&path);
            let (first, reads) = read(&path);
            assert_eq!(reads, 1, "{}", path.display());
            let (again, reads) = read(&path);
            assert_eq!(reads, 0, "{}", path.display());
            assert!(again == first);
            assert_eq!((&again.path, again.size), (&path, first.size));
        }
        // Read every time: published outputs, temporary store entries, the
        // working areas of repository rules and module extensions (even a
        // `trees` directory there, e.g. an extension named "trees"), other
        // parts of the repository cache, other spellings and files outside
        // the cache.
        for path in [
            root.join("actions/key/outputs/file"),
            root.join("repositories/trees/.stage-x/file"),
            root.join("repositories/direct/trees/.trash-x/tree/file"),
            root.join("repositories/modextwd/x/file"),
            root.join("repositories/modextwd/trees/x/file"),
            root.join("repositories/direct/stages/slug/file"),
            root.join("repositories/downloads/trees/x/file"),
            root.join("repositories/direct/manifests/x/file"),
            root.join("repositories/trees/abc/pkg/./file.go"),
            root.join("cas/.tmp-0123-1-2"),
            fixture.0.join("source.go"),
        ] {
            create(&path);
            for _ in 0..2 {
                assert_eq!(read(&path).1, 1, "{}", path.display());
            }
        }
        assert!(!store_file(
            Some(&root),
            &root.join("repositories/trees/abc")
        ));
        assert!(!store_file(None, &root.join("cas/0123-444")));
    }

    /// The byte-level checks of plain paths agree with the component-wise
    /// definitions on every spelling (repeated, trailing and `.`/`..`
    /// components, siblings sharing a prefix, relative paths).
    #[test]
    fn plain_path_checks_agree_with_component_checks() {
        let roots = ["/cache", "/c/root", "/c/ro"];
        let relatives = [
            "",
            "/",
            "/actions",
            "/actions/",
            "/actions/k/outputs/file",
            "/actions/k/outputs/../file",
            "/actions/./k",
            "/actions//k",
            "/actionsx/k",
            "/cas",
            "/cas/0123-444",
            "/cas/.tmp-0123-1-2",
            "/cas/0123-444/",
            "/cas/0123-444/.",
            "/cas/a/b",
            "/repositories/trees/abc/pkg/file.go",
            "/repositories/trees/abc/pkg/./file.go",
            "/repositories/trees/abc//file.go",
            "/repositories/trees/abc/../x/file.go",
            "/repositories/trees/abc",
            "/repositories/trees",
            "/repositories/trees/.stage-x/file",
            "/repositories/direct/trees/def/file.go",
            "/repositories/direct/trees/def",
            "/repositories/direct/trees/.trash-x/tree/file",
            "/repositories/direct/stages/slug/file",
            "/repositories/direct/stages/trees/x/file",
            "/repositories/git/trees/ghi/sub/file",
            "/repositories/builtins/trees/jkl/tools/file.sh",
            "/repositories/builtins/treesx/jkl/file",
            "/repositories/modextwd/x/file",
            "/repositories/modextwd/trees/x/file",
            "/repositories/downloads/trees/x/file",
            "/repositories/treesx/abc/file",
            "/repositories",
            "/work/k/execroot/file",
            "/..",
            "/../cache/cas/x",
            "x/cas/0123-444",
        ];
        for root in roots {
            let root = Path::new(root);
            for relative in relatives {
                let joined = format!("{}{relative}", root.display());
                let others = [
                    joined.clone(),
                    format!("/c{relative}"),
                    relative.trim_start_matches('/').to_owned(),
                ];
                for spelled in others {
                    let path = Path::new(&spelled);
                    let by_components = path.components().collect::<PathBuf>();
                    assert_eq!(
                        plain(path.as_os_str().as_bytes()),
                        path.is_absolute() && by_components.as_os_str() == path.as_os_str(),
                        "plain {spelled}"
                    );
                    assert_eq!(
                        store_file(Some(root), path),
                        store_file_by_components(root, path),
                        "store_file {spelled} below {}",
                        root.display()
                    );
                    assert_eq!(
                        published_output(Some(root), path),
                        published_output_by_components(root, path),
                        "published_output {spelled}"
                    );
                    assert_eq!(
                        owned_by(Some(root), path),
                        path.starts_with(root),
                        "owned_by {spelled}"
                    );
                    assert_eq!(
                        in_actions(root, path),
                        path.starts_with(root.join("actions")),
                        "in_actions {spelled}"
                    );
                    assert_eq!(
                        owned_area(path, root),
                        owned_area_by_components(path, root),
                        "owned area {spelled}"
                    );
                }
            }
        }
    }

    /// The byte comparison of validated relative paths is `Path::cmp`.
    #[test]
    fn relative_destinations_compare_like_paths() {
        let names = [
            "a", "a-b", "a.b", "a/b", "a/b/c", "a/bc", "a0", "ab", "a/b-c", "a/b/c-d", "b", "a/b0",
            "A", "a/B", "a b", "a/\u{e9}", "z/y", "a/b/c/d", "\u{e9}", "a~", "a/~",
        ];
        for a in names {
            assert!(validate_relative(Path::new(a)).is_ok(), "{a}");
            for b in names {
                assert_eq!(
                    compare_relative(a.as_bytes(), b.as_bytes()),
                    Path::new(a).cmp(Path::new(b)),
                    "{a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn cache_owned_directories_are_verified_once_and_links_stay_rejected() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let tree = root.join("repositories/trees/x");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/file"), b"x").unwrap();
        let file = tree.join("a/b/file");
        let walks = full_ancestor_walks();
        for _ in 0..2 {
            assert!(no_linked_ancestor(&file, Some(&root)).unwrap().is_file());
        }
        assert_eq!(full_ancestor_walks(), walks);
        for directory in ["repositories", "repositories/trees/x/a/b"] {
            assert!(directory_verified(&root.join(directory)), "{directory}");
        }
        // Everything else takes the full walk: paths outside the cache, the
        // execution roots, other spellings, and calls without a cache root.
        let work = root.join("work/x");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("file"), b"x").unwrap();
        let source = fixture.0.join("source");
        fs::write(&source, b"x").unwrap();
        for (path, cache) in [
            (source, Some(root.as_path())),
            (work.join("file"), Some(root.as_path())),
            (tree.join("a/./b/file"), Some(root.as_path())),
            (tree.join("a/b/../b/file"), Some(root.as_path())),
            (file.clone(), None),
        ] {
            let before = full_ancestor_walks();
            assert!(no_linked_ancestor(&path, cache).unwrap().is_file());
            assert_eq!(full_ancestor_walks(), before + 1, "{}", path.display());
        }
        assert!(!directory_verified(&work));
        // Links and missing or misspelled entries fail exactly as before.
        symlink(tree.join("a"), tree.join("link")).unwrap();
        symlink(&file, tree.join("a/b/leaf")).unwrap();
        for path in [
            tree.join("link/b/file"),
            tree.join("a/b/leaf"),
            tree.join("a/missing/file"),
            tree.join("a/b/missing"),
            PathBuf::from(format!("{}/", file.display())),
        ] {
            let error = no_linked_ancestor(&path, Some(&root)).unwrap_err();
            assert_eq!(error, no_linked_ancestor(&path, None).unwrap_err());
        }
        assert!(
            no_linked_ancestor(&tree.join("link/b/file"), Some(&root))
                .unwrap_err()
                .contains("symlink is not allowed")
        );
        assert!(!directory_verified(&tree.join("link")));
        // The working areas of repository rules and module extensions are
        // never remembered (not even a `trees` directory there, e.g. of an
        // extension named "trees"): a directory there that becomes a link
        // between two checks is rejected by the second.
        for area in [
            "repositories/modextwd/trees/x",
            "repositories/modextwd/ext",
            "repositories/direct/stages/slug",
        ] {
            let directory = root.join(area);
            fs::create_dir_all(directory.join("d")).unwrap();
            fs::write(directory.join("d/file"), b"x").unwrap();
            let before = full_ancestor_walks();
            assert!(
                no_linked_ancestor(&directory.join("d/file"), Some(&root))
                    .unwrap()
                    .is_file()
            );
            assert_eq!(full_ancestor_walks(), before + 1, "{area}");
            assert!(!directory_verified(&directory.join("d")), "{area}");
            fs::rename(directory.join("d"), directory.join("moved")).unwrap();
            symlink(directory.join("moved"), directory.join("d")).unwrap();
            let error = no_linked_ancestor(&directory.join("d/file"), Some(&root)).unwrap_err();
            assert!(error.contains("symlink is not allowed"), "{area}: {error}");
        }
        // Every repository tree store is remembered like the registry's: an
        // edit there is not seen again by this process (its trees are
        // replaced only as a whole, by bazel-rs), only by the next one,
        // whose memo starts empty.
        for store in [
            "repositories/direct/trees/y",
            "repositories/git/trees/y",
            "repositories/builtins/trees/y",
        ] {
            let directory = root.join(store);
            fs::create_dir_all(directory.join("d")).unwrap();
            fs::write(directory.join("d/file"), b"x").unwrap();
            let file = directory.join("d/file");
            let before = full_ancestor_walks();
            assert!(no_linked_ancestor(&file, Some(&root)).unwrap().is_file());
            assert_eq!(full_ancestor_walks(), before, "{store}");
            assert!(directory_verified(&directory.join("d")), "{store}");
            fs::rename(directory.join("d"), directory.join("moved")).unwrap();
            symlink(directory.join("moved"), directory.join("d")).unwrap();
            assert!(no_linked_ancestor(&file, Some(&root)).is_ok(), "{store}");
            verified_directories().write().unwrap().clear();
            assert!(
                no_linked_ancestor(&file, Some(&root))
                    .unwrap_err()
                    .contains("symlink is not allowed"),
                "{store}"
            );
        }
    }

    /// The trusted areas of the cache, exactly: published outputs and blobs
    /// (directories), blobs and the repository tree stores (files).
    #[test]
    fn trusted_cache_areas_are_exactly_the_stores() {
        let root = Path::new("/cache");
        for (relative, owned, stored) in [
            ("actions/k/outputs/file", true, false),
            ("cas/0123-444", true, true),
            ("cas/.tmp-0123-1-2", true, false),
            ("repositories/trees/abc/file", true, true),
            ("repositories/trees/abc", true, false),
            ("repositories/direct/trees/def/sub/file", true, true),
            ("repositories/git/trees/ghi/file", true, true),
            ("repositories/builtins/trees/jkl/tools/file.sh", true, true),
            ("repositories/trees/.stage-x/file", false, false),
            ("repositories/git/trees/.stage-x/file", false, false),
            ("repositories/direct/trees/.trash-x/tree/file", false, false),
            ("repositories/direct/stages/slug/file", false, false),
            ("repositories/direct/stages/trees/x/file", false, false),
            ("repositories/modextwd/trees/x/file", false, false),
            ("repositories/modextwd/ext/file", false, false),
            ("repositories/downloads/trees/x/file", false, false),
            ("repositories/download_ids/sha256/x", false, false),
            ("repositories/direct/manifests/x.json", false, false),
            ("repositories/locks/x", false, false),
            // A tree's manifest, next to it: its directories are the store's.
            ("repositories/trees/abc.json", true, false),
            ("repositories/trees", false, false),
            ("repositories", false, false),
            ("work/k/execroot/file", false, false),
            ("digests/v1.dmemo", false, false),
        ] {
            let path = root.join(relative);
            assert_eq!(owned_area(&path, root), owned, "owned area {relative}");
            assert_eq!(
                store_file(Some(root), &path),
                stored,
                "store file {relative}"
            );
            let tree = repository_tree_rest(relative.as_bytes());
            assert_eq!(
                tree.is_some(),
                owned && relative.starts_with("repositories/"),
                "{relative}"
            );
        }
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
    fn inherited_environment_and_execution_info_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.environment.insert("X".into(), "1".into());
        action.environment.insert("H".into(), "fixed".into());
        action.command =
            "test -z \"${LANG+x}\" && [[ $TMPDIR = /* ]] && printf '%s:%s' \"$X\" \"$H\" > out/result"
                .into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"1:fixed");
        action
            .extra
            .execution_info
            .insert("no-sandbox".into(), "1".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        // An inherited name the client does not set keeps the fixed value
        // but is keyed (ActionEnvironment.addTo).
        action.extra.inherited_environment.insert("H".into());
        let unset = execute(&fixture.cache(), &action).unwrap();
        assert!(!unset.cache_hit);
        assert_eq!(fs::read(&unset.outputs[0]).unwrap(), b"1:fixed");
        // A resolved client value overrides the fixed one and is keyed
        // (ActionEnvironment.resolve; ActionCacheChecker.computeEffectiveEnvironment).
        action
            .extra
            .client_environment
            .insert("H".into(), "client".into());
        let client = execute(&fixture.cache(), &action).unwrap();
        assert!(!client.cache_hit);
        assert_eq!(fs::read(&client.outputs[0]).unwrap(), b"1:client");
        action
            .extra
            .client_environment
            .insert("H".into(), "other".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action
            .extra
            .client_environment
            .insert("NOT_INHERITED".into(), "x".into());
        assert_eq!(
            execute(&fixture.cache(), &action).unwrap_err(),
            "client environment value for 'NOT_INHERITED', which is not inherited"
        );
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
